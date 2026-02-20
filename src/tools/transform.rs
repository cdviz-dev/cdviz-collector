//! Transform command for cdviz-collector.
//!
//! The `transform` command processes files from an input directory through
//! configured transformers and writes the results to an output directory. Supports
//! multiple input formats (JSON, XML, YAML, TAP, CSV) with automatic format detection
//! based on file extension. This is primarily used for testing transformations,
//! validating transformer configurations, and batch processing of event files.
//!
//! # Features
//!
//! - **Multi-Format Support**: Process JSON, XML, YAML, TAP, and CSV files with auto-detection
//! - **Batch Processing**: Recursively processes all matching files in input directory
//! - **Transformer Chaining**: Apply multiple transformers in sequence
//! - **`CDEvent` Validation**: Validates output against `CDEvents` specification
//! - **Interactive Review**: Review differences before accepting changes
//! - **Flexible Output Modes**: Review, overwrite, or check modes for managing output
//! - **Metadata Export**: Optional export of headers and metadata to separate files
//!
//! # Usage Examples
//!
//! ```bash
//! # Basic transformation with passthrough
//! cdviz-collector transform --input ./events --output ./transformed
//!
//! # Apply specific transformers
//! cdviz-collector transform -i ./events -o ./transformed \
//!   --transformer-refs github_events,add_metadata
//!
//! # Review mode (default) - interactive review of changes
//! cdviz-collector transform -i ./events -o ./transformed --mode review
//!
//! # Overwrite mode - replace existing files without prompting
//! cdviz-collector transform -i ./events -o ./transformed --mode overwrite
//!
//! # Check mode - fail if differences found (useful for CI)
//! cdviz-collector transform -i ./events -o ./transformed --mode check
//!
//! # Export headers and metadata alongside transformed files
//! cdviz-collector transform -i ./events -o ./transformed \
//!   --export-headers --export-metadata
//!
//! # Skip CDEvent validation for non-CDEvent JSON
//! cdviz-collector transform -i ./events -o ./transformed --no-check-cdevent
//! ```
//!
//! # File Processing
//!
//! The transform command processes files as follows:
//! 1. **Input Discovery**: Recursively finds files matching the selected parser format(s)
//! 2. **Format Detection**: Auto-detects file format by extension (when using `--input-parser auto`)
//! 3. **File Filtering**: Excludes `*.headers.json`, `*.metadata.json`, and `*.json.new` files
//! 4. **Transformation**: Applies configured transformers in sequence
//! 5. **Temporary Output**: Creates `*.json.new` files with transformed content
//! 6. **Validation**: Validates output against `CDEvents` specification (if enabled)
//! 7. **Mode Processing**: Handles conflicts according to selected mode
//! 8. **Cleanup**: Removes temporary files (unless `--keep-new-files` specified)
//!
//! # Modes
//!
//! - **Review Mode**: Interactive review showing differences between existing and new files
//! - **Overwrite Mode**: Automatically replaces existing files with new versions
//! - **Check Mode**: Compares files and fails if differences are found (useful for CI/testing)
//!
//! # Configuration
//!
//! The command requires a configuration file that defines available transformers:
//!
//! ```toml
//! [transformers.github_events]
//! type = "vrl"
//! script = '''
//! .type = "dev.cdevents.repository.created.0.1.1"
//! .subject.id = .repository.full_name
//! '''
//!
//! [transformers.add_metadata]
//! type = "vrl"
//! script = '''
//! .customData.processed_at = now()
//! '''
//! ```

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
#[command(args_conflicts_with_subcommands = true, flatten_help = true)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct TransformArgs {
    /// Configuration file defining transformers and their settings.
    ///
    /// TOML configuration file that defines available transformers. The file
    /// should contain transformer definitions that can be referenced by name
    /// using the --transformer-refs option.
    #[clap(long = "config", env("CDVIZ_COLLECTOR_CONFIG"))]
    config: Option<PathBuf>,

    /// Names of transformers to chain together.
    ///
    /// Comma-separated list or multiple arguments specifying which transformers
    /// to apply in sequence. Transformers are applied in the order specified.
    ///
    /// Example: `--transformer-refs github_events,add_metadata`
    /// Example: `-t github_events -t add_metadata`
    #[clap(short = 't', long = "transformer-refs", default_value = "passthrough", value_delimiter= ',', num_args = 1..)]
    transformer_refs: Vec<String>,

    /// Input directory containing files to transform.
    ///
    /// Directory path containing the files to be processed. The tool will
    /// recursively search for files matching the selected `--input-parser` format
    /// (json, jsonl, csv, xml, yaml, tap), excluding *.headers.json,
    /// *.metadata.json, and *.json.new files.
    #[clap(short = 'i', long = "input")]
    input: PathBuf,

    /// Output directory for transformed files (always written as JSON).
    ///
    /// Directory where transformed files will be written. The directory structure
    /// from the input will be preserved. Regardless of input format, output files
    /// are always written as JSON. Files will initially be created with
    /// .json.new extension before being processed according to the selected mode.
    #[clap(short = 'o', long = "output")]
    output: PathBuf,

    /// How to handle conflicts between new and existing output files.
    ///
    /// Controls the behavior when output files already exist:
    /// - review: Interactive review of differences (default)
    /// - overwrite: Replace existing files without prompting
    /// - check: Fail if differences are found
    #[clap(short = 'm', long = "mode", value_enum, default_value_t = TransformMode::Review)]
    mode: TransformMode,

    /// Skip validation that output body is a valid `CDEvent`.
    ///
    /// By default, the tool validates that transformation results produce
    /// valid `CDEvent` objects. Use this flag to disable validation if you're
    /// working with non-CDEvent JSON data.
    #[clap(long)]
    no_check_cdevent: bool,

    /// Export headers to separate .headers.json files.
    ///
    /// When enabled, HTTP headers from the original request will be exported
    /// to .headers.json files alongside the main JSON output files.
    #[clap(long)]
    export_headers: bool,

    /// Export metadata to separate .metadata.json files.
    ///
    /// When enabled, event metadata (timestamps, source info, etc.) will be
    /// exported to .metadata.json files alongside the main JSON output files.
    #[clap(long)]
    export_metadata: bool,

    /// Keep temporary .json.new files after processing.
    ///
    /// Normally, temporary .json.new files created during transformation are
    /// cleaned up after processing. Use this flag to preserve them for debugging.
    #[clap(long)]
    keep_new_files: bool,

    /// Input file format parser selection.
    ///
    /// Determines how input files are parsed. Use 'auto' for automatic format
    /// detection based on file extension, or specify an explicit format.
    ///
    /// Supported formats:
    /// - auto: Auto-detect (json, xml, yaml, yml, tap, csv)
    /// - json: JSON files
    /// - jsonl: JSON Lines (one object per line)
    /// - csv: CSV files (one event per row)
    /// - xml: XML files (requires `parser_xml` feature)
    /// - yaml: YAML files (requires `parser_yaml` feature)
    /// - tap: TAP format (requires `parser_tap` feature)
    #[clap(long = "input-parser", value_enum, default_value = "auto")]
    parser: ParserSelection,
}

/// Input file format selection for the transform command
#[derive(Debug, Clone, PartialEq, Eq, ValueEnum, Default)]
enum ParserSelection {
    /// Auto-detect format based on file extension
    #[default]
    Auto,
    /// Parse as JSON
    Json,
    /// Parse as JSON Lines (one JSON object per line)
    Jsonl,
    /// Parse as CSV (one event per row)
    #[value(name = "csv")]
    CsvRow,
    /// Parse as XML
    #[cfg(feature = "parser_xml")]
    Xml,
    /// Parse as YAML
    #[cfg(feature = "parser_yaml")]
    Yaml,
    /// Parse as TAP (Test Anything Protocol)
    #[cfg(feature = "parser_tap")]
    Tap,
}

impl ParserSelection {
    /// Get file extensions for this parser
    fn extensions(&self) -> Vec<&'static str> {
        match self {
            Self::Auto => {
                let mut exts = vec!["json", "jsonl", "csv"];
                #[cfg(feature = "parser_xml")]
                exts.push("xml");
                #[cfg(feature = "parser_yaml")]
                exts.extend(&["yaml", "yml"]);
                #[cfg(feature = "parser_tap")]
                exts.push("tap");
                exts
            }
            Self::Json => vec!["json"],
            Self::Jsonl => vec!["jsonl"],
            Self::CsvRow => vec!["csv"],
            #[cfg(feature = "parser_xml")]
            Self::Xml => vec!["xml"],
            #[cfg(feature = "parser_yaml")]
            Self::Yaml => vec!["yaml", "yml"],
            #[cfg(feature = "parser_tap")]
            Self::Tap => vec!["tap"],
        }
    }

    /// Build glob patterns for file matching
    fn path_patterns(&self) -> Vec<String> {
        let mut patterns = Vec::new();

        for ext in self.extensions() {
            patterns.push(format!("**/*.{ext}"));
        }

        // Exclude special files
        patterns.extend([
            "!**/*.headers.json".to_string(),
            "!**/*.metadata.json".to_string(),
            "!**/*.json.new".to_string(),
        ]);

        patterns
    }

    /// Convert to `OpenDAL` parser config
    fn to_parser_config(&self) -> source_opendal::parsers::Config {
        match self {
            Self::Auto => source_opendal::parsers::Config::Auto,
            Self::Json => source_opendal::parsers::Config::Json,
            Self::Jsonl => source_opendal::parsers::Config::Jsonl,
            Self::CsvRow => source_opendal::parsers::Config::CsvRow,
            #[cfg(feature = "parser_xml")]
            Self::Xml => source_opendal::parsers::Config::Xml,
            #[cfg(feature = "parser_yaml")]
            Self::Yaml => source_opendal::parsers::Config::Yaml,
            #[cfg(feature = "parser_tap")]
            Self::Tap => source_opendal::parsers::Config::Tap,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Default)]
enum TransformMode {
    /// Interactive review of differences between new and existing output files
    #[default]
    Review,
    /// Overwrite existing output files without prompting
    Overwrite,
    /// Check for differences and fail if any are found
    Check,
}

pub(crate) async fn transform(args: TransformArgs) -> Result<bool> {
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
        seen_paths: HashMap::new(),
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
        path_patterns: args.parser.path_patterns(),
        parser: args.parser.to_parser_config(),
        try_read_headers_json: true,
        metadata: serde_json::json!({
            "context": {
                "source": "http://cdviz-collector.example.com?source=cli-transform"
            }
        }),
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
    /// Track how many outputs we've seen for each input file path.
    /// Used to generate unique filenames when transformers produce multiple outputs (1:n).
    seen_paths: HashMap<String, u16>,
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

        let base_filename = input.metadata["path"]
            .as_str()
            .ok_or(miette!("could not extract 'name' field from metadata"))?
            .to_string();

        // Track how many times we've seen this input file to generate unique output filenames
        // For 1:n transformations (1 input → n outputs), we append an index to duplicates
        let counter = self.seen_paths.entry(base_filename.clone()).or_insert(0);
        let current_index = *counter;
        *counter += 1;

        // Generate unique filename: first occurrence uses original name, subsequent ones append index
        // Example: 001.json → 001.json, 001.json (again) → 001.1.json, 001.json (again) → 001.2.json
        let filename = if current_index == 0 {
            base_filename.clone()
        } else {
            // Insert index before the .json extension
            let path = Path::new(&base_filename);
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(&base_filename);
            let parent = path.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
            let unique_name = format!("{stem}.{current_index}.json");
            if parent.is_empty() { unique_name } else { format!("{parent}/{unique_name}") }
        };

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
