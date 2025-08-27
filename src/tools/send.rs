//! Send command implementation for the cdviz-collector CLI.
//!
//! This module provides the `send` subcommand that allows sending JSON data directly
//! to configured sinks, useful for testing and scripting scenarios.
//!
//! # Features
//!
//! - **CLI Source**: Reads JSON data from command line arguments, files, or stdin
//! - **HTTP Sink**: Default sink for sending data to HTTP endpoints with `CloudEvents` format
//! - **curl-like Interface**: Uses `--data`/`-d` flag similar to curl for JSON input
//! - **Flexible Input**: Supports direct JSON strings, `@filename`, or `@-` for stdin
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
};
use clap::Args;
use figment::{
    Figment,
    providers::{Format, Toml},
};
use reqwest::Url;
use std::path::PathBuf;

/// Arguments for send command
#[derive(Debug, Clone, Args)]
#[command(args_conflicts_with_subcommands = true, flatten_help = true)]
pub(crate) struct SendArgs {
    /// JSON data to send to the sink.
    ///
    /// Can be specified as:
    /// - Direct JSON string: '{"test": "value"}'
    /// - File path: @data.json
    /// - Stdin: @-
    ///
    /// The JSON data will be processed through the configured pipeline
    /// and sent to the specified sink.
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

[sources.cli]
enabled = true
[sources.cli.extractor]
type = "cli"
"#;

pub(crate) async fn send(args: SendArgs) -> Result<bool> {
    // Load configuration with CLI overrides
    let config = load_config(&args)?;

    // Create and run pipeline
    let pipeline = PipelineBuilder::new(config);

    pipeline.run(false).await
}

/// Load configuration with CLI argument overrides (used by send command).
fn load_config(args: &SendArgs) -> Result<Config> {
    // Start with base configuration
    let mut figment = Figment::new().merge(Toml::string(SEND_BASE_CONFIG));

    // Merge user config file if provided
    if let Some(config_file) = &args.config {
        figment = figment.merge(Toml::file(config_file));
    }

    // Create CLI override configuration from command line arguments
    let cli_toml = convert_args_into_toml(args)?;
    if !cli_toml.is_empty() {
        figment = figment.merge(Toml::string(&cli_toml));
    }

    // Extract final configuration
    figment.extract().into_diagnostic()
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

    // Override the HTTP sink configuration
    if let Some(url) = &args.url {
        cli_overrides.insert("sinks.http.enabled".to_string(), "true".to_string());
        cli_overrides.insert("sinks.http.destination".to_string(), url.to_string());
        cli_overrides.insert("sinks.debug.enabled".to_string(), "false".to_string());

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
        } else if key == "sources.cli.extractor.data" {
            // Data is already formatted as a TOML value (triple-quoted string)
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
        };

        let config = load_config(&args).unwrap();

        // Should have enabled HTTP sink and disabled debug sink
        assert!(config.sinks.contains_key("http"));
        assert!(config.sinks["http"].is_enabled());
        assert!(config.sinks.contains_key("debug"));
        assert!(!config.sinks["debug"].is_enabled());
    }
}
