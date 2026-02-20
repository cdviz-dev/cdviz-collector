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
    config::Config,
    errors::{IntoDiagnostic, Result},
    pipeline::PipelineBuilder,
    sources::cli::parsers,
};
use clap::{Args, ValueEnum};
use reqwest::Url;
use std::path::PathBuf;
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
    #[clap(short = 'd', long = "data")]
    data: String,

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

    /// Configuration file for advanced sink settings.
    ///
    /// Optional TOML configuration file for advanced sink configuration
    /// such as authentication, headers generation, or custom sink types.
    /// Command line arguments will override configuration file settings.
    #[clap(long = "config", env("CDVIZ_COLLECTOR_CONFIG"))]
    config: Option<PathBuf>,

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
const SEND_BASE_CONFIG: &str = r#"
[sinks.debug]
enabled = true
type = "debug"

[sinks.http]
enabled = false
type = "http"
destination = "http://localhost:8080/webhook/000"
total_duration_of_retries = "30s"

[sources.cli]
enabled = true
[sources.cli.extractor]
type = "cli"
"#;

pub(crate) async fn send(args: SendArgs, shutdown_token: CancellationToken) -> Result<bool> {
    // Load configuration with CLI overrides using ConfigBuilder
    let config = load_config(&args)?;

    // Create and run pipeline
    let pipeline = PipelineBuilder::new(config);

    pipeline.run(false, shutdown_token).await
}

/// Load configuration with CLI argument overrides (used by send command).
fn load_config(args: &SendArgs) -> Result<Config> {
    // Create CLI override configuration from command line arguments
    let cli_toml = convert_args_into_toml(args)?;

    // Use ConfigBuilder with send-specific base config and CLI overrides
    Config::builder()
        .with_base_config(SEND_BASE_CONFIG)
        .with_config_file(args.config.clone())
        .with_cli_overrides(if cli_toml.is_empty() { None } else { Some(cli_toml) })
        .with_env_vars(true) // Now environment variables are supported!
        .build()
}

/// Create CLI override configuration from command line arguments.
/// Returns an empty string if nothing to override.
fn convert_args_into_toml(args: &SendArgs) -> Result<String> {
    use std::fmt::Write as _;

    let mut cli_overrides = std::collections::HashMap::<String, String>::new();

    // Always inject the CLI data
    // Use triple-quoted string to avoid escaping issues with JSON
    let data_toml = format!("\"\"\"{}\"\"\"", args.data);
    cli_overrides.insert("sources.cli.extractor.data".to_string(), data_toml);

    // Set the parser configuration
    let parser_config: parsers::Config = args.parser.clone().into();
    let parser_toml = match parser_config {
        parsers::Config::Auto => "\"auto\"".to_string(),
        parsers::Config::Json => "\"json\"".to_string(),
        #[cfg(feature = "parser_xml")]
        parsers::Config::Xml => "\"xml\"".to_string(),
        #[cfg(feature = "parser_yaml")]
        parsers::Config::Yaml => "\"yaml\"".to_string(),
        #[cfg(feature = "parser_tap")]
        parsers::Config::Tap => "\"tap\"".to_string(),
        parsers::Config::Text => "\"text\"".to_string(),
        parsers::Config::TextLine => "\"text_line\"".to_string(),
    };
    cli_overrides.insert("sources.cli.extractor.parser".to_string(), parser_toml);

    // Override the HTTP sink configuration
    if let Some(url) = &args.url {
        cli_overrides.insert("sinks.http.enabled".to_string(), "true".to_string());
        cli_overrides.insert("sinks.http.destination".to_string(), url.to_string());
        cli_overrides.insert("sinks.debug.enabled".to_string(), "false".to_string());
        if let Some(duration) = &args.total_duration_of_retries {
            cli_overrides
                .insert("sinks.http.total_duration_of_retries".to_string(), duration.to_owned());
        }

        // Add custom headers if provided
        if !args.headers.is_empty() {
            for header in &args.headers {
                if let Some((key, value)) = header.split_once(':') {
                    cli_overrides.insert(
                        format!("sinks.http.headers.{}.type", key.trim()),
                        "static".to_string(),
                    );
                    cli_overrides.insert(
                        format!("sinks.http.headers.{}.value", key.trim()),
                        value.trim().to_string(),
                    );
                }
            }
        }
    }

    // Create a TOML string from CLI overrides
    let mut cli_toml = String::new();
    for (key, value) in &cli_overrides {
        // Handle boolean values without quotes
        if value == "true" || value == "false" {
            writeln!(&mut cli_toml, "{key} = {value}").into_diagnostic()?;
        } else if key == "sources.cli.extractor.data" || key == "sources.cli.extractor.parser" {
            // Data and parser are already formatted as TOML values (quoted strings)
            writeln!(&mut cli_toml, "{key} = {value}").into_diagnostic()?;
        } else {
            writeln!(&mut cli_toml, "{key} = \"{value}\"").into_diagnostic()?;
        }
    }
    Ok(cli_toml)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_load_config_with_base() {
        let args = SendArgs {
            data: "{}".to_string(),
            url: None,
            config: None,
            directory: None,
            headers: vec![],
            total_duration_of_retries: None,
            parser: ParserSelection::Auto,
        };

        let config = load_config(&args).unwrap();

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
            data: "{}".to_string(),
            url: Some("https://example.com/webhook".parse().unwrap()),
            config: None,
            directory: None,
            headers: vec!["X-API-Key: secret".to_string()],
            total_duration_of_retries: None,
            parser: ParserSelection::Auto,
        };

        let config = load_config(&args).unwrap();

        // Should have enabled HTTP sink and disabled debug sink
        assert!(config.sinks.contains_key("http"));
        assert!(config.sinks["http"].is_enabled());
        assert!(config.sinks.contains_key("debug"));
        assert!(!config.sinks["debug"].is_enabled());
    }
}
