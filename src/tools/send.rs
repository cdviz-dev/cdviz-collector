//! Send command implementation for the cdviz-collector CLI.
//!
//! This module provides the `send` subcommand that allows sending JSON data directly
//! to configured sinks, useful for testing and scripting scenarios.
//!
//! # Features
//!
//! - **CLI Source**: Reads data from command line arguments, files, or stdin
//! - **Multi-Format Support**: Parses JSON, XML, YAML, and TAP formats with auto-detection
//! - **HTTP Sink**: Default sink for sending data to HTTP endpoints with `CloudEvents` format
//! - **curl-like Interface**: Uses `--data`/`-d` flag similar to curl for data input
//! - **Flexible Input**: Supports direct strings, `@filename`, or `@-` for stdin
//! - **Configuration Override**: Command line arguments can override default sink configuration
//! - **Custom Headers**: Support for additional HTTP headers via `--header`/`-H` flags
//!
//! # Usage Examples
//!
//! ```bash
//! # Send direct JSON data to debug sink (default)
//! cdviz-collector send --data '{"test": "value"}'
//!
//! # Send to HTTP endpoint with URL override
//! cdviz-collector send --data '{"test": "value"}' --url http://example.com/webhook
//!
//! # Read from file
//! cdviz-collector send --data @data.json --url http://example.com/webhook
//!
//! # Read from stdin
//! echo '{"test": "value"}' | cdviz-collector send --data @- --url http://example.com/webhook
//!
//! # With custom headers
//! cdviz-collector send --data '{"test": "value"}' --url http://example.com/webhook \
//!   --header "X-API-Key: secret" --header "X-Custom: value"
//!
//! # Send with HMAC signature for webhook security
//! # Create a config file with signature header generation:
//! # cat > send-config.toml << EOF
//! # [sinks.http.headers.x-signature-256]
//! # type = "signature"
//! # token = "your-webhook-secret"
//! # algorithm = "sha256"
//! # prefix = "sha256="
//! # EOF
//! cdviz-collector send --data '{"test": "value"}' --url https://api.example.com/webhook \
//!   --config send-config.toml
//!
//! # Send XML file with auto-detection
//! cdviz-collector send --data @test-results.xml --url http://example.com/webhook \
//!   --input-parser auto
//!
//! # Send YAML file with explicit parser
//! cdviz-collector send --data @config.yaml --url http://example.com/webhook \
//!   --input-parser yaml
//!
//! # Send TAP test results
//! cdviz-collector send --data @results.tap --url http://example.com/webhook \
//!   --input-parser tap
//! ```
//!
//! # Architecture
//!
//! The send command follows the same pipeline architecture as the connect command:
//! 1. **CLI Source**: Reads and parses JSON data from various input sources
//! 2. **Event Pipeline**: Converts raw JSON to `CDEvents` format via broadcast channel
//! 3. **Sinks**: Processes and dispatches events to configured destinations
//!
//! # Configuration
//!
//! The command uses a layered configuration approach:
//! 1. **Base Configuration**: Embedded TOML with debug sink enabled by default
//! 2. **User Configuration**: Optional config file via `--config` flag
//! 3. **CLI Overrides**: Command line arguments override previous layers
//!
//! When `--url` is specified, the HTTP sink is automatically enabled and the debug sink
//! is disabled to avoid duplicate output.

use crate::{
    config::{Config, ConfigSource, resolve_config_source},
    errors::{IntoDiagnostic, Result},
    pipeline::PipelineBuilder,
    sources::cli::parsers,
};
use clap::{Args, ValueEnum};
use reqwest::Url;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

/// Arguments for send command
#[derive(Debug, Clone, Args)]
#[command(args_conflicts_with_subcommands = true, flatten_help = true)]
pub(crate) struct SendArgs {
    /// Data to send to the sink.
    ///
    /// Can be specified as:
    /// - Direct string: '{"test": "value"}' or raw text/XML/YAML content
    /// - File path: @data.json (format auto-detected from extension)
    /// - Stdin: @-
    ///
    /// The data will be parsed according to `--input-parser` and processed
    /// through the configured pipeline before being sent to the specified sink.
    /// Can be repeated to send multiple items.
    #[clap(short = 'd', long = "data")]
    data: Vec<String>,

    /// HTTP URL to send the data to.
    ///
    /// When specified, automatically enables the HTTP sink and disables
    /// the debug sink. The data will be sent as `CloudEvents` format to
    /// the specified endpoint.
    ///
    /// Example: `--url <https://api.example.com/webhook>`
    #[clap(short = 'u', long = "url")]
    url: Option<Url>,

    /// Total duration of retries on failed http request. (default 30s)
    ///
    /// Example: `--total-duration-of-retries 1m`
    #[clap(long)]
    total_duration_of_retries: Option<String>,

    /// Configuration file path or HTTP/HTTPS URL for advanced sink settings.
    ///
    /// Optional TOML configuration for advanced sink configuration
    /// such as authentication, headers generation, or custom sink types.
    /// Command line arguments will override configuration file settings.
    #[clap(long = "config", env("CDVIZ_COLLECTOR_CONFIG"))]
    config: Option<ConfigSource>,

    /// HTTP headers to use when fetching config from a URL.
    ///
    /// Format: `"Header-Name: value"`. Can be repeated.
    /// Example: `--config-header "Authorization: Bearer token"`
    #[clap(long = "config-header")]
    config_headers: Vec<String>,

    /// Working directory for relative paths.
    ///
    /// Changes the working directory before processing. This affects
    /// relative paths in configuration files and data file references.
    #[clap(short = 'C', long = "directory")]
    directory: Option<PathBuf>,

    /// Additional HTTP headers for the request.
    ///
    /// Specify custom headers for HTTP sink requests. Can be used multiple
    /// times to add several headers. Format: "Header-Name: value"
    ///
    /// Example: `--header "X-API-Key: secret" --header "Content-Type: application/json"`
    #[clap(short = 'H', long = "header")]
    headers: Vec<String>,

    /// Input data format parser selection.
    ///
    /// Determines how input data is parsed. Use 'auto' for automatic format
    /// detection based on file extension (when using @file), or specify an
    /// explicit format.
    ///
    /// Supported formats:
    /// - auto: Auto-detect format based on file extension (default)
    /// - json: Parse as JSON
    /// - xml: Parse as XML (requires `parser_xml` feature)
    /// - yaml: Parse as YAML (requires `parser_yaml` feature)
    /// - tap: Parse as TAP format (requires `parser_tap` feature)
    /// - text: Entire input as a single event with body `{"text": "..."}`
    /// - text-line: Each non-empty line as a separate event with body `{"text": "..."}`
    #[clap(long = "input-parser", value_enum, default_value = "auto")]
    parser: ParserSelection,

    /// Wrap a child process and emit `CDEvents` around it.
    #[clap(long = "run")]
    run: Option<String>,

    /// Disable result file parsing, use exit code only (--run mode).
    #[clap(long = "no-data")]
    no_data: bool,

    /// Override metadata fields: --metadata key=value (repeatable, --run mode).
    #[clap(long = "metadata")]
    metadata: Vec<String>,

    /// Fail if collector sink is unreachable (default: warn and continue).
    #[clap(long = "fail-on-collector-error")]
    fail_on_collector_error: bool,

    /// Child command and args (specified after --)
    #[clap(last = true)]
    command: Vec<String>,
}

/// Input data format selection for the send command
#[derive(Debug, Clone, PartialEq, Eq, ValueEnum, Default)]
enum ParserSelection {
    /// Auto-detect format based on file extension
    #[default]
    Auto,
    /// Parse as JSON
    Json,
    /// Parse as XML
    #[cfg(feature = "parser_xml")]
    Xml,
    /// Parse as YAML
    #[cfg(feature = "parser_yaml")]
    Yaml,
    /// Parse as TAP (Test Anything Protocol)
    #[cfg(feature = "parser_tap")]
    Tap,
    /// Entire input as raw text
    Text,
    /// Each line as a separate raw text event
    TextLine,
}

impl From<ParserSelection> for parsers::Config {
    fn from(selection: ParserSelection) -> Self {
        match selection {
            ParserSelection::Auto => parsers::Config::Auto,
            ParserSelection::Json => parsers::Config::Json,
            #[cfg(feature = "parser_xml")]
            ParserSelection::Xml => parsers::Config::Xml,
            #[cfg(feature = "parser_yaml")]
            ParserSelection::Yaml => parsers::Config::Yaml,
            #[cfg(feature = "parser_tap")]
            ParserSelection::Tap => parsers::Config::Tap,
            ParserSelection::Text => parsers::Config::Text,
            ParserSelection::TextLine => parsers::Config::TextLine,
        }
    }
}

/// Embedded base configuration for send command
const SEND_BASE_CONFIG: &str = include_str!("../assets/send.base.toml");

pub(crate) async fn send(args: SendArgs, shutdown_token: CancellationToken) -> Result<bool> {
    if let Some(run_type) = args.run.clone() {
        send_run(args, &run_type, shutdown_token).await
    } else {
        let config = load_config(&args).await?;
        PipelineBuilder::new(config).run(false, shutdown_token).await
    }
}

/// Handle `--run <type>` mode: wrap a subprocess and emit `CDEvents`.
async fn send_run(
    args: SendArgs,
    run_type: &str,
    shutdown_token: CancellationToken,
) -> Result<bool> {
    let exit_code_out = Arc::new(AtomicI32::new(0));
    let config = load_run_config(&args, run_type, Arc::clone(&exit_code_out)).await?;
    PipelineBuilder::new(config).run(false, shutdown_token).await?;
    Ok(exit_code_out.load(Ordering::SeqCst) == 0)
}

/// Load configuration for `--run` mode.
async fn load_run_config(
    args: &SendArgs,
    run_type: &str,
    exit_code_out: Arc<AtomicI32>,
) -> Result<crate::config::Config> {
    let resolved = resolve_config_source(args.config.clone(), &args.config_headers).await?;
    let cli_toml = convert_run_args_into_toml(args, run_type)?;
    let mut config = Config::builder()
        .with_base_config(SEND_BASE_CONFIG)
        .with_resolved_source(resolved)
        .with_cli_overrides(if cli_toml.is_empty() { None } else { Some(cli_toml) })
        .with_env_vars(true)
        .build()?;
    if let Some(source_config) = config.sources.get_mut(run_type) {
        source_config.inject_subprocess_exit_code_out(exit_code_out);
    }
    Ok(config)
}

/// Build CLI override TOML for `--run` mode.
fn convert_run_args_into_toml(args: &SendArgs, run_type: &str) -> Result<String> {
    use std::fmt::Write as _;
    let mut out = String::new();

    writeln!(&mut out, "sources.{run_type}.enabled = true").into_diagnostic()?;
    writeln!(&mut out, "sources.cli.enabled = false").into_diagnostic()?;

    if !args.command.is_empty() {
        let cmd_json = serde_json::to_string(&args.command).into_diagnostic()?;
        writeln!(&mut out, "sources.{run_type}.extractor.command = {cmd_json}")
            .into_diagnostic()?;
    }
    if !args.data.is_empty() {
        let globs_json = serde_json::to_string(&args.data).into_diagnostic()?;
        writeln!(&mut out, "sources.{run_type}.extractor.data_globs = {globs_json}")
            .into_diagnostic()?;
    }
    if args.no_data {
        writeln!(&mut out, "sources.{run_type}.extractor.no_data = true").into_diagnostic()?;
    }
    if args.fail_on_collector_error {
        writeln!(&mut out, "sources.{run_type}.extractor.fail_on_collector_error = true")
            .into_diagnostic()?;
    }
    for kv in &args.metadata {
        if let Some((key, value)) = kv.split_once('=') {
            let escaped = escape_toml_string(value);
            writeln!(
                &mut out,
                "sources.{run_type}.extractor.metadata.run.overrides.{} = \"{escaped}\"",
                key.trim()
            )
            .into_diagnostic()?;
        }
    }
    append_http_sink_toml(&mut out, args)?;
    Ok(out)
}

/// Load configuration with CLI argument overrides (used by send command).
async fn load_config(args: &SendArgs) -> Result<Config> {
    let resolved = resolve_config_source(args.config.clone(), &args.config_headers).await?;
    let cli_toml = convert_args_into_toml(args)?;
    Config::builder()
        .with_base_config(SEND_BASE_CONFIG)
        .with_resolved_source(resolved)
        .with_cli_overrides(if cli_toml.is_empty() { None } else { Some(cli_toml) })
        .with_env_vars(true)
        .build()
}

/// Create CLI override TOML from command line arguments.
fn convert_args_into_toml(args: &SendArgs) -> Result<String> {
    use std::fmt::Write as _;
    let mut out = String::new();

    // Use triple-quoted string to avoid escaping issues with JSON body
    let data_str = args.data.join("\n");
    writeln!(&mut out, "sources.cli.extractor.data = \"\"\"{data_str}\"\"\"").into_diagnostic()?;

    let parser_str = match args.parser.clone().into() {
        parsers::Config::Auto => "auto",
        parsers::Config::Json => "json",
        #[cfg(feature = "parser_xml")]
        parsers::Config::Xml => "xml",
        #[cfg(feature = "parser_yaml")]
        parsers::Config::Yaml => "yaml",
        #[cfg(feature = "parser_tap")]
        parsers::Config::Tap => "tap",
        parsers::Config::Text => "text",
        parsers::Config::TextLine => "text_line",
    };
    writeln!(&mut out, "sources.cli.extractor.parser = \"{parser_str}\"").into_diagnostic()?;

    for kv in &args.metadata {
        if let Some((key, value)) = kv.split_once('=') {
            let escaped = escape_toml_string(value);
            writeln!(&mut out, "sources.cli.extractor.metadata.{} = \"{escaped}\"", key.trim())
                .into_diagnostic()?;
        }
    }
    append_http_sink_toml(&mut out, args)?;
    Ok(out)
}

/// Append HTTP sink overrides to a TOML string when `--url` is provided.
fn append_http_sink_toml(out: &mut String, args: &SendArgs) -> Result<()> {
    use std::fmt::Write as _;
    if let Some(url) = &args.url {
        writeln!(out, "sinks.http.enabled = true").into_diagnostic()?;
        writeln!(out, "sinks.http.destination = \"{url}\"").into_diagnostic()?;
        writeln!(out, "sinks.debug.enabled = false").into_diagnostic()?;
        if let Some(duration) = &args.total_duration_of_retries {
            writeln!(out, "sinks.http.total_duration_of_retries = \"{duration}\"")
                .into_diagnostic()?;
        }
        for header in &args.headers {
            if let Some((key, value)) = header.split_once(':') {
                let key = key.trim();
                writeln!(out, "sinks.http.headers.{key}.type = \"static\"").into_diagnostic()?;
                writeln!(out, "sinks.http.headers.{key}.value = \"{}\"", value.trim())
                    .into_diagnostic()?;
            }
        }
    }
    Ok(())
}

/// Escape a string value for use inside a TOML double-quoted string.
fn escape_toml_string(s: &str) -> String {
    s.trim()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_args() -> SendArgs {
        SendArgs {
            data: vec!["{}".to_string()],
            url: None,
            config: None,
            config_headers: vec![],
            directory: None,
            headers: vec![],
            total_duration_of_retries: None,
            parser: ParserSelection::Auto,
            run: None,
            no_data: false,
            metadata: vec![],
            fail_on_collector_error: false,
            command: vec![],
        }
    }

    #[tokio::test]
    async fn test_load_config_with_base() {
        let args = default_args();

        let config = load_config(&args).await.unwrap();

        // Should have HTTP sink (disabled by default)
        assert!(config.sinks.contains_key("http"));
        assert!(!config.sinks["http"].is_enabled());

        // Should have debug sink enabled by default
        assert!(config.sinks.contains_key("debug"));
        assert!(config.sinks["debug"].is_enabled());
    }

    #[tokio::test]
    async fn test_load_config_with_url_override() {
        let args = SendArgs {
            data: vec!["{}".to_string()],
            url: Some("https://example.com/webhook".parse().unwrap()),
            headers: vec!["X-API-Key: secret".to_string()],
            ..default_args()
        };

        let config = load_config(&args).await.unwrap();

        // Should have enabled HTTP sink and disabled debug sink
        assert!(config.sinks.contains_key("http"));
        assert!(config.sinks["http"].is_enabled());
        assert!(config.sinks.contains_key("debug"));
        assert!(!config.sinks["debug"].is_enabled());
    }

    #[test]
    fn test_convert_args_with_metadata_generates_dotted_keys() {
        let args = SendArgs {
            metadata: vec!["my_key=my_value".to_string(), "other=42".to_string()],
            ..default_args()
        };

        let toml = convert_args_into_toml(&args).unwrap();

        assert!(
            toml.contains(r#"sources.cli.extractor.metadata.my_key = "my_value""#),
            "Expected metadata key in TOML, got:\n{toml}"
        );
        assert!(
            toml.contains(r#"sources.cli.extractor.metadata.other = "42""#),
            "Expected metadata key in TOML, got:\n{toml}"
        );
    }
}
