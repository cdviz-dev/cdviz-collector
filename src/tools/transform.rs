use crate::{
    config,
    errors::{miette, IntoDiagnostic, Result},
    pipes::Pipe,
    sources::{opendal as source_opendal, transformers, EventSource, EventSourcePipe},
    utils::PathExt,
};
use clap::{Args, ValueEnum};
use opendal::Scheme;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Args)]
#[command(args_conflicts_with_subcommands = true,flatten_help = true, about, long_about = None)]
pub(crate) struct TransformArgs {
    /// The configuration file to use.
    #[clap(long = "config", env("CDVIZ_COLLECTOR_CONFIG"))]
    config: Option<PathBuf>,

    /// The directory to use as the working directory.
    #[clap(short = 'C', long = "directory")]
    directory: Option<PathBuf>,

    /// Names of transformers to chain (comma separated)
    #[clap(short = 't', long = "transformer-refs", default_value = "passthrough")]
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
    if let Some(dir) = args.directory {
        std::env::set_current_dir(dir).into_diagnostic()?;
    }

    cliclack::intro("Transforming files...").into_diagnostic()?;
    let config = config::Config::from_file(args.config)?;

    if !args.output.exists() {
        std::fs::create_dir_all(&args.output).into_diagnostic()?;
    }

    let mut pipe: EventSourcePipe = Box::new(OutputToJsonFile { directory: args.output.clone() });
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
            "!**/*.out.json".to_string(),
            "!**/*.new.json".to_string(),
        ],
        parser: source_opendal::parsers::Config::Json,
    };
    let processed =
        source_opendal::OpendalExtractor::try_from(&config_extractor, pipe)?.run_once().await?;
    cliclack::log::info(format!("Processed {processed} input files.")).into_diagnostic()?;
    // TODO process .new.json vs .out.json using the self.mode strategy
    let output_files = list_output_files(&args.output).await?;
    let res = match args.mode {
        TransformMode::Review => review(&output_files),
        TransformMode::Check => check(&output_files),
        TransformMode::Overwrite => overwrite(&output_files),
    };
    remove_new_files(&output_files)?;
    cliclack::outro("Transformation done.").into_diagnostic()?;
    res
}

struct OutputToJsonFile {
    directory: PathBuf,
}

impl Pipe for OutputToJsonFile {
    type Input = EventSource;
    fn send(&mut self, input: Self::Input) -> Result<()> {
        let filename = input.metadata["path"]
            .as_str()
            .ok_or(miette!("could not extract 'name' field from metadata"))?;
        let filename = filename.replace(".json", ".new.json");
        let path = self.directory.join(filename);
        // remove fields that can change between runs on different machines
        let mut input = input;
        if let Some(metadata) = input.metadata.as_object_mut() {
            metadata.remove("last_modified");
            metadata.remove("root");
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).into_diagnostic()?;
        }
        std::fs::write(path, serde_json::to_string_pretty(&input).into_diagnostic()?)
            .into_diagnostic()?;
        Ok(())
    }
}

// recursive iterator over all files in a directory
async fn list_output_files(path: &Path) -> Result<Vec<PathBuf>> {
    use opendal::{services::Fs, Operator};
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
                && (entry.path().ends_with(".out.json") || entry.path().ends_with(".new.json"))
        })
        .map(|entry| path.join(entry.path()))
        .collect::<Vec<PathBuf>>();
    Ok(result)
}

fn overwrite(output_files: &Vec<PathBuf>) -> Result<bool> {
    let mut count = 0;
    for path in output_files {
        let filename = path.extract_filename()?;
        if filename.ends_with(".new.json") {
            let out_filename = filename.replace(".new.json", ".out.json");
            let out_path = path.with_file_name(out_filename);
            std::fs::rename(path, out_path).into_diagnostic()?;
            count += 1;
        }
    }
    cliclack::log::success(format!("Overwritten {count} files.")).into_diagnostic()?;
    Ok(true)
}

fn check(output_files: &Vec<PathBuf>) -> Result<bool> {
    let differences = crate::tools::difference::search_new_vs_out(output_files)?;
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
    let differences = crate::tools::difference::search_new_vs_out(output_files)?;
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
        if filename.ends_with(".new.json") && std::fs::exists(path).into_diagnostic()? {
            std::fs::remove_file(path).into_diagnostic()?;
        }
    }
    Ok(())
}
