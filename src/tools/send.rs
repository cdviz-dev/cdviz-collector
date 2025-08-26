//! Send command implementation for the cdviz-collector CLI.
//!
//! This module provides the `send` subcommand that allows sending JSON data directly
//! to configured sinks, useful for testing and scripting scenarios.
//!
//! # Features
//!
//! - **CLI Source**: Reads JSON data from command line arguments, files, or stdin
//! - **HTTP Sink**: Default sink for sending data to HTTP endpoints with CloudEvents format
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
//! ```
//!
//! # Architecture
//!
//! The send command follows the same pipeline architecture as the connect command:
//! 1. **CLI Source**: Reads and parses JSON data from various input sources
//! 2. **Event Pipeline**: Converts raw JSON to CDEvents format via broadcast channel
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
    config,
    errors::{IntoDiagnostic, Result},
    sinks,
    sources::{EventSourcePipe, cli, send_cdevents},
};
use clap::Args;
use reqwest::Url;
use std::{
    fs::File,
    io::{Cursor, Read},
    path::PathBuf,
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Args)]
#[command(args_conflicts_with_subcommands = true, flatten_help = true, about, long_about = None)]
pub(crate) struct SendArgs {
    /// JSON data to send. Can be a JSON string, @filename, or @- for stdin
    #[clap(short = 'd', long = "data")]
    data: String,

    /// HTTP URL to send the data to (default sink)
    #[clap(short = 'u', long = "url")]
    url: Option<Url>,

    /// The configuration file to use for advanced sink configuration
    #[clap(long = "config", env("CDVIZ_COLLECTOR_CONFIG"))]
    config: Option<PathBuf>,

    /// The directory to use as the working directory
    #[clap(short = 'C', long = "directory")]
    directory: Option<PathBuf>,

    /// Additional headers for HTTP sink (can be specified multiple times)
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
    if let Some(dir) = &args.directory {
        std::env::set_current_dir(dir).into_diagnostic()?;
    }

    // Parse data input
    let reader = create_reader_from_data(&args.data)?;

    // Load configuration
    let config = load_config(&args)?;

    // Create broadcast channel for message passing
    let (tx, _rx) = broadcast::channel(1024);

    // Create pipeline components
    let pipe: EventSourcePipe = Box::new(send_cdevents::Processor::new(tx.clone()));

    // Create CLI extractor with the reader
    let cli_extractor = cli::CliExtractor::new(reader, pipe);

    // Create sinks
    let sink_handles = create_sinks(&config, &tx)?;

    // Run the CLI extractor
    let cancel_token = CancellationToken::new();

    tracing::info!("Starting to send data");

    // Run the CLI extractor
    cli_extractor.run(cancel_token.clone()).await?;

    // Give sinks a moment to process messages
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Cancel sinks
    cancel_token.cancel();

    // Wait for sinks to finish with a reasonable timeout
    let shutdown_timeout = std::time::Duration::from_millis(500);
    for handle in sink_handles {
        match tokio::time::timeout(shutdown_timeout, handle).await {
            Ok(Ok(_)) => tracing::debug!("Sink shutdown successfully"),
            Ok(Err(e)) => tracing::warn!("Sink error during shutdown: {}", e),
            Err(_) => tracing::warn!("Sink shutdown timed out"),
        }
    }

    tracing::info!("Send operation completed");
    Ok(true)
}

fn create_reader_from_data(data: &str) -> Result<Box<dyn Read + Send>> {
    if let Some(path) = data.strip_prefix('@') {
        if path == "-" {
            // Read from stdin
            Ok(Box::new(std::io::stdin()))
        } else {
            // Read from file
            let file = File::open(path).into_diagnostic()?;
            Ok(Box::new(file))
        }
    } else {
        // Direct JSON string
        Ok(Box::new(Cursor::new(data.to_string())))
    }
}

fn load_config(args: &SendArgs) -> Result<config::Config> {
    use figment::{
        Figment,
        providers::{Format, Toml},
    };

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

// Create CLI override configuration from command line arguments
// return an empty string if nothing to overrides
fn convert_args_into_toml(args: &SendArgs) -> Result<String> {
    use std::fmt::Write as _;

    let mut cli_overrides = std::collections::HashMap::<String, String>::new();

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
        } else {
            writeln!(&mut cli_toml, "{key} = \"{value}\"").into_diagnostic()?;
        }
    }
    Ok(cli_toml)
}

fn create_sinks(
    config: &config::Config,
    tx: &broadcast::Sender<crate::Message>,
) -> Result<Vec<tokio::task::JoinHandle<Result<()>>>> {
    let mut handles = Vec::new();

    for (name, sink_config) in &config.sinks {
        if sink_config.is_enabled() {
            tracing::info!(sink = name, "starting sink");

            let rx = tx.subscribe();
            let sink_config = sink_config.clone();
            let name = name.clone();

            let handle = sinks::start(name, sink_config, rx);
            handles.push(handle);
        }
    }

    Ok(handles)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_reader_from_data_direct_json() {
        let data = r#"{"test": "value"}"#;
        let reader = create_reader_from_data(data).unwrap();

        // Read the data back
        let mut buf = String::new();
        let mut reader = reader;
        reader.read_to_string(&mut buf).unwrap();

        assert_eq!(buf, data);
    }

    #[test]
    fn test_create_reader_from_data_stdin() {
        let data = "@-";
        let reader = create_reader_from_data(data);

        // Should succeed (we can't easily test stdin in unit tests)
        assert!(reader.is_ok());
    }

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
