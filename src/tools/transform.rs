use crate::{
    config,
    errors::{IntoDiagnostic, Result, miette},
    pipes::Pipe,
    sources::{EventSource, EventSourcePipe, opendal as source_opendal, transformers},
    utils::PathExt,
};
use cdevents_sdk::CDEvent;
use clap::{Args, ValueEnum};
use opendal::Scheme;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
};

#[derive(Debug, Clone, Args)]
#[command(args_conflicts_with_subcommands = true,flatten_help = true, about, long_about = None)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct TransformArgs {
    /// The configuration file to use.
    #[clap(long = "config", env("CDVIZ_COLLECTOR_CONFIG"))]
    config: Option<PathBuf>,

    /// The directory to use as the working directory.
    #[clap(short = 'C', long = "directory")]
    directory: Option<PathBuf>,

    /// Names of transformers to chain (comma separated or multiple args)
    #[clap(short = 't', long = "transformer-refs", default_value = "passthrough", value_delimiter= ',', num_args = 1..)]
    transformer_refs: Vec<String>,

    /// The input directory with json files.
    #[clap(short = 'i', long = "input")]
    input: PathBuf,

    /// The output directory with json files.
    #[clap(short = 'o', long = "output")]
    output: PathBuf,

    /// How to handle new vs existing output files
    #[clap(short = 'm', long = "mode", value_enum, default_value_t = TransformMode::Review)]
    mode: TransformMode,

    /// Do not check if output's body is a cdevent
    #[clap(long)]
    no_check_cdevent: bool,

    /// Export headers to .headers.json files
    #[clap(long)]
    export_headers: bool,

    /// Export metadata to .metadata.json files
    #[clap(long)]
    export_metadata: bool,

    /// Keep the .json.new files after transformation (temporary generated files)
    #[clap(long)]
    keep_new_files: bool,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Default)]
enum TransformMode {
    /// interactive review generated against existing output
    #[default]
    Review,
    /// overwrite existing output files without checking
    Overwrite,
    /// check generated against existing output and failed on difference
    Check,
}

pub(crate) async fn transform(args: TransformArgs) -> Result<bool> {
    if let Some(dir) = &args.directory {
        std::env::set_current_dir(dir).into_diagnostic()?;
    }

    let config = config::Config::from_file(args.config)?;
    cliclack::intro("Transforming files...").into_diagnostic()?;

    if !args.output.exists() {
        std::fs::create_dir_all(&args.output).into_diagnostic()?;
    }
    let check_cdevent_failures_counter = Arc::new(AtomicU16::new(0));
    let mut pipe: EventSourcePipe = Box::new(OutputToJsonFile {
        directory: args.output.clone(),
        check_cdevent: !args.no_check_cdevent,
        check_cdevent_failures_counter: Arc::clone(&check_cdevent_failures_counter),
        export_headers: args.export_headers,
        export_metadata: args.export_metadata,
    });
    let mut tconfigs =
        transformers::resolve_transformer_refs(&args.transformer_refs, &config.transformers)?;
    tconfigs.reverse();
    for tconfig in tconfigs {
        pipe = tconfig.make_transformer(pipe)?;
    }
    let config_extractor = source_opendal::Config {
        polling_interval: std::time::Duration::ZERO,
        kind: Scheme::Fs,
        parameters: HashMap::from([("root".to_string(), args.input.to_string_lossy().to_string())]),
        recursive: true,
        path_patterns: vec![
            "**/*.json".to_string(),
            "!**/*.headers.json".to_string(),
            "!**/*.metadata.json".to_string(),
            "!**/*.json.new".to_string(),
        ],
        parser: source_opendal::parsers::Config::Json,
        try_read_headers_json: true,
    };
    let processed =
        source_opendal::OpendalExtractor::try_from(&config_extractor, pipe)?.run_once().await?;
    cliclack::log::info(format!("Processed {processed} input files.")).into_diagnostic()?;
    // TODO process .json.new vs .json using the self.mode strategy
    let output_files = list_output_files(&args.output).await?;
    let res = match args.mode {
        TransformMode::Review => review(&output_files),
        TransformMode::Check => check(&output_files),
        TransformMode::Overwrite => overwrite(&output_files),
    };
    if !args.keep_new_files {
        remove_new_files(&output_files)?;
    }

    let check_cdevent_failures_count = check_cdevent_failures_counter.load(Ordering::Acquire);
    if check_cdevent_failures_count > 0 {
        cliclack::log::warning(format!(
            "could not parse body as a cdevent {check_cdevent_failures_count} times"
        ))
        .into_diagnostic()?;
    }

    cliclack::outro("Transformation done.").into_diagnostic()?;
    res.map(|ok| ok && (check_cdevent_failures_count == 0))
}

struct OutputToJsonFile {
    directory: PathBuf,
    check_cdevent: bool,
    check_cdevent_failures_counter: Arc<AtomicU16>,
    export_headers: bool,
    export_metadata: bool,
}

impl Pipe for OutputToJsonFile {
    type Input = EventSource;
    fn send(&mut self, input: Self::Input) -> Result<()> {
        // remove fields that can change between runs on different machines
        let mut input = input;
        if let Some(metadata) = input.metadata.as_object_mut() {
            metadata.remove("last_modified");
            metadata.remove("root");
        }

        let filename = input.metadata["path"]
            .as_str()
            .ok_or(miette!("could not extract 'name' field from metadata"))?
            .to_string();
        let path = self.directory.join(&filename);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).into_diagnostic()?;
        }

        std::fs::write(
            path.with_extension("json.new"),
            serde_json::to_string_pretty(&input.body).into_diagnostic()?,
        )
        .into_diagnostic()?;
        if self.export_headers {
            std::fs::write(
                path.with_extension("headers.json.new"),
                serde_json::to_string_pretty(&input.headers).into_diagnostic()?,
            )
            .into_diagnostic()?;
        }
        if self.export_metadata {
            std::fs::write(
                path.with_extension("metadata.json.new"),
                serde_json::to_string_pretty(&input.metadata).into_diagnostic()?,
            )
            .into_diagnostic()?;
        }
        if self.check_cdevent {
            let maybe_cdevents = CDEvent::try_from(input);
            if let Err(err) = maybe_cdevents {
                self.check_cdevent_failures_counter.fetch_add(1, Ordering::SeqCst);
                cliclack::log::warning(format!("{filename}: could not parse as a cdevent: {err}"))
                    .into_diagnostic()?;
            }
        }
        Ok(())
    }
}

// recursive iterator over all files in a directory
#[allow(clippy::case_sensitive_file_extension_comparisons)]
async fn list_output_files(path: &Path) -> Result<Vec<PathBuf>> {
    use opendal::{Operator, services::Fs};
    let builder = Fs::default()
        // Set the root for fs, all operations will happen under this root.
        //
        // NOTE: the root must be absolute path.
        .root(&path.to_string_lossy());

    // `Accessor` provides the low level APIs, we will use `Operator` normally.
    let op: Operator = Operator::new(builder).into_diagnostic()?.finish();
    let result = op
        .list_with("")
        .recursive(true)
        .await
        .into_diagnostic()?
        .iter()
        .filter(|entry| {
            entry.metadata().mode().is_file()
                && (entry.path().ends_with(".json") || entry.path().ends_with(".json.new"))
        })
        .map(|entry| path.join(entry.path()))
        .collect::<Vec<PathBuf>>();
    Ok(result)
}

fn overwrite(output_files: &Vec<PathBuf>) -> Result<bool> {
    let mut count = 0;
    for path in output_files {
        let filename = path.extract_filename()?;
        if filename.ends_with(".json.new") {
            let out_filename = filename.replace(".json.new", ".json");
            let out_path = path.with_file_name(out_filename);
            std::fs::rename(path, out_path).into_diagnostic()?;
            count += 1;
        }
    }
    cliclack::log::success(format!("Overwritten {count} files.")).into_diagnostic()?;
    Ok(true)
}

fn check(output_files: &Vec<PathBuf>) -> Result<bool> {
    let differences = crate::tools::difference::find_differences(output_files)?;
    if differences.is_empty() {
        cliclack::log::success("0 differences found.").into_diagnostic()?;
        Ok(true)
    } else {
        cliclack::log::warning(format!("{} differences found.", differences.len()))
            .into_diagnostic()?;
        for (comparison, diff) in differences {
            diff.show(&comparison)?;
        }
        Ok(false)
    }
}

fn review(output_files: &Vec<PathBuf>) -> Result<bool> {
    let differences = crate::tools::difference::find_differences(output_files)?;
    if differences.is_empty() {
        cliclack::log::success("0 differences found.").into_diagnostic()?;
        Ok(true)
    } else {
        cliclack::log::warning(format!("{} differences found.", differences.len()))
            .into_diagnostic()?;
        let mut no_differences = true;
        for (comparison, diff) in differences {
            no_differences = diff.review(&comparison)? && no_differences;
        }
        Ok(no_differences)
    }
}

fn remove_new_files(output_files: &Vec<PathBuf>) -> Result<()> {
    for path in output_files {
        let filename = path.extract_filename()?;
        if filename.ends_with(".json.new") && std::fs::exists(path).into_diagnostic()? {
            std::fs::remove_file(path).into_diagnostic()?;
            // cliclack::log::remark(format!("remove {:?}", path))
            //     .into_diagnostic()?;
        }
    }
    Ok(())
}
