//! Configuration structures for the NATS event source.
//!
//! This module defines the configuration options for the NATS consumer source,
//! including server connection settings, subject subscriptions, `JetStream` consumer
//! configuration, and message processing behavior.

use crate::security::rule::HeaderRuleMap;
use serde::Deserialize;

/// Configuration for NATS source
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Config {
    /// NATS server URLs (comma-separated), e.g. `<nats://localhost:4222>`
    pub(crate) servers: String,
    /// Subject to subscribe to (Core) or filter on (`JetStream`)
    pub(crate) subject: String,
    /// Source mode: "core" (default) or "jetstream"
    #[serde(default)]
    pub(crate) mode: NatsSourceMode,
    /// `JetStream` stream name (required when mode = "jetstream")
    pub(crate) stream: Option<String>,
    /// Durable consumer name (required when mode = "jetstream")
    pub(crate) consumer: Option<String>,
    /// Header validation rules for incoming messages
    #[serde(default)]
    pub(crate) headers: HeaderRuleMap,
    /// Headers to forward from NATS message to pipeline
    #[serde(default)]
    pub(crate) headers_to_keep: Vec<String>,
    /// Base metadata to include in all `EventSource` instances created by this extractor.
    /// The `context.source` field will be automatically populated if not set.
    #[serde(default)]
    pub(crate) metadata: serde_json::Value,
}

/// NATS source operation mode
#[derive(Clone, Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NatsSourceMode {
    /// Core NATS pub/sub (default)
    #[default]
    Core,
    /// `JetStream` persistent streaming
    Jetstream,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_mode_is_core() {
        let mode = NatsSourceMode::default();
        assert_eq!(mode, NatsSourceMode::Core);
    }

    #[test]
    fn test_mode_deserialization() {
        // Use JSON since serde_json supports top-level string values for enum variants
        let core: NatsSourceMode = serde_json::from_str(r#""core""#).unwrap();
        assert_eq!(core, NatsSourceMode::Core);

        let js: NatsSourceMode = serde_json::from_str(r#""jetstream""#).unwrap();
        assert_eq!(js, NatsSourceMode::Jetstream);
    }
}
