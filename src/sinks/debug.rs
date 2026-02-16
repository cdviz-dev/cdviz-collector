//! Debug sink for logging `CDEvents` to stdout/stderr for development and debugging purposes.
//!
//! The debug sink provides a simple way to observe `CDEvents` as they flow through the pipeline
//! without persisting them to storage or sending them to external services. It's primarily
//! intended for development, testing, and troubleshooting scenarios.
//!
//! ## Configuration
//!
//! The debug sink supports two display formats for `CDEvents`:
//!
//! ### JSON Format (`json`)
//! The default format that displays events as pretty-printed JSON with 2-space indentation.
//! This format is useful when you need to see the exact JSON structure or copy
//! events for testing purposes.
//!
//! ```toml
//! [sinks.debug]
//! enabled = true
//! format = "json"  # Default, can be omitted
//! ```
//!
//! ### Rust Debug Format (`rust_debug`)
//! Alternative format that uses Rust's `Debug` trait to display the `CDEvent` structure.
//! This format is compact and shows the internal structure of the event object.
//!
//! ```toml
//! [sinks.debug]
//! enabled = true
//! format = "rust_debug"
//! ```
//!
//! ## Destination Options
//!
//! The debug sink supports two output destinations:
//!
//! ### Standard Output (`stdout`)
//! The default destination that prints events directly to stdout with pretty formatting.
//! This is ideal for development and troubleshooting as it provides clean, readable output.
//!
//! ```toml
//! [sinks.debug]
//! enabled = true
//! destination = "stdout"  # Default, can be omitted
//! ```
//!
//! ### Structured Logging (`log_info`)
//! Alternative destination that sends events through the structured logging system.
//! Uses compact formatting to work well with log aggregation systems.
//!
//! ```toml
//! [sinks.debug]
//! enabled = true
//! destination = "log_info"
//! ```
//!
//! ## Environment Variables
//!
//! Configuration can also be set via environment variables:
//! - `CDVIZ_COLLECTOR__SINKS__DEBUG__ENABLED=true`
//! - `CDVIZ_COLLECTOR__SINKS__DEBUG__FORMAT=json`
//! - `CDVIZ_COLLECTOR__SINKS__DEBUG__DESTINATION=stdout`
//!
//! ## Usage Examples
//!
//! ### Basic Usage
//! ```bash
//! # Enable debug sink with default format
//! CDVIZ_COLLECTOR__SINKS__DEBUG__ENABLED=true cargo run -- connect --config config.toml
//! ```
//!
//! ### Compare Formats
//! ```bash
//! # Run the demonstration task to see both formats
//! mise run examples:debug-formats
//! ```
//!
//! ## Output
//!
//! The debug sink logs events using the `tracing` crate at the `INFO` level with the target
//! `cdviz_collector::sinks::debug`. Each log entry includes:
//! - `CDEvent` ID for correlation
//! - The event content in the specified format
//! - "mock sending" message to indicate this is a debug sink
//!
//! ## Use Cases
//!
//! - **Development**: Quick visibility into event flow during development
//! - **Debugging**: Troubleshooting transformation pipelines and event processing
//! - **Testing**: Validating event structure and content before production deployment
//! - **Monitoring**: Temporary event inspection without affecting production sinks

use super::Sink;
use crate::Message;
use crate::errors::{IntoDiagnostic, Report, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Default)]
pub(crate) enum Format {
    #[serde(rename = "rust_debug")]
    RustDebug,
    #[default]
    #[serde(rename = "json")]
    Json,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Default)]
pub(crate) enum Destination {
    #[serde(rename = "log_info")]
    LogInfo,
    #[default]
    #[serde(rename = "stdout")]
    Stdout,
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub(crate) struct Config {
    /// Is the sink is enabled?
    pub(crate) enabled: bool,
    /// Display format for `CDEvents`
    #[serde(default)]
    pub(crate) format: Format,
    /// Output destination for debug messages
    #[serde(default)]
    pub(crate) destination: Destination,
}

impl TryFrom<Config> for DebugSink {
    type Error = Report;

    fn try_from(value: Config) -> Result<Self> {
        Ok(DebugSink { format: value.format, destination: value.destination })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DebugSink {
    format: Format,
    destination: Destination,
}

impl Sink for DebugSink {
    #[tracing::instrument(skip(self, msg), fields(cdevent_id = %msg.cdevent.id()))]
    #[allow(clippy::print_stdout, clippy::disallowed_macros)]
    async fn send(&self, msg: &Message) -> Result<()> {
        match self.destination {
            Destination::LogInfo => {
                // Use compact format for logging to avoid issues with structured logging
                match self.format {
                    Format::RustDebug => {
                        tracing::info!(cdevent=?msg.cdevent, "mock sending");
                    }
                    Format::Json => {
                        // Use compact JSON for logging
                        let json = serde_json::to_string(&msg.cdevent).into_diagnostic()?;
                        tracing::info!(cdevent=%json, "mock sending");
                    }
                }
            }
            Destination::Stdout => {
                // Print directly to stdout with human-readable formatting, no prefix message
                match self.format {
                    Format::RustDebug => {
                        println!("{:#?}", msg.cdevent);
                    }
                    Format::Json => {
                        // Use pretty JSON for direct stdout output
                        let json = serde_json::to_string_pretty(&msg.cdevent).into_diagnostic()?;
                        println!("{json}");
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_strategy::proptest(
        async = "tokio",
        proptest::prelude::ProptestConfig::default(),
        cases = 100
    )]
    async fn test_debug_sink_successful_send(msg: Message) {
        let config = Config { enabled: true, ..Config::default() };
        let sink = DebugSink::try_from(config).unwrap();

        assert2::assert!(let Ok(()) = sink.send(&msg).await);
    }

    #[test_strategy::proptest(
        async = "tokio",
        proptest::prelude::ProptestConfig::default(),
        cases = 10
    )]
    async fn test_debug_sink_concurrent_sends(msg: Message) {
        let config = Config { enabled: true, ..Config::default() };
        let sink = DebugSink::try_from(config).unwrap();

        let tasks = (0..10).map(|_| {
            let sink = sink.clone();
            let msg = msg.clone();
            tokio::spawn(async move { sink.send(&msg).await })
        });

        let results = futures::future::join_all(tasks).await;
        for result in results {
            assert2::assert!(let Ok(Ok(())) = result);
        }
    }

    #[test]
    fn test_debug_sink_config_creation() {
        let config = Config { enabled: true, ..Config::default() };
        assert2::assert!(let Ok(_) = DebugSink::try_from(config));

        let config = Config { enabled: false, ..Config::default() };
        assert2::assert!(let Ok(_) = DebugSink::try_from(config));

        let config = Config { enabled: true, format: Format::Json, ..Config::default() };
        assert2::assert!(let Ok(_) = DebugSink::try_from(config));

        let config = Config { enabled: false, format: Format::RustDebug, ..Config::default() };
        assert2::assert!(let Ok(_) = DebugSink::try_from(config));
    }

    #[test]
    fn test_debug_sink_serialization() {
        let config = Config { enabled: true, ..Config::default() };
        let serialized = serde_json::to_string(&config).unwrap();
        assert!(serialized.contains("true"));
        assert!(serialized.contains("json"));
        assert!(serialized.contains("stdout"));

        let config = Config { enabled: false, format: Format::RustDebug, ..Config::default() };
        let serialized = serde_json::to_string(&config).unwrap();
        assert!(serialized.contains("false"));
        assert!(serialized.contains("rust_debug"));
    }

    #[test]
    fn test_debug_sink_deserialization() {
        let json = r#"{"enabled": true}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(config.enabled);
        assert_eq!(config.format, Format::Json);

        let json = r#"{"enabled": false}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(!config.enabled);
        assert_eq!(config.format, Format::Json);

        let json = r#"{"enabled": true, "format": "json"}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(config.enabled);
        assert_eq!(config.format, Format::Json);

        let json = r#"{"enabled": false, "format": "rust_debug"}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(!config.enabled);
        assert_eq!(config.format, Format::RustDebug);
    }

    #[test_strategy::proptest(
        async = "tokio",
        proptest::prelude::ProptestConfig::default(),
        cases = 10
    )]
    async fn test_debug_sink_json_format(msg: Message) {
        let config = Config { enabled: true, format: Format::Json, ..Config::default() };
        let sink = DebugSink::try_from(config).unwrap();

        // This should not fail even with JSON serialization
        assert2::assert!(let Ok(()) = sink.send(&msg).await);
    }
}
