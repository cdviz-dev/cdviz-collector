//! Configuration structures for the Kafka event source.
//!
//! This module defines the configuration options for the Kafka consumer source,
//! including broker connection settings, topic subscriptions, consumer group
//! configuration, and message processing behavior.

use crate::security::rule::HeaderRuleMap;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

/// Configuration for Kafka source
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Config {
    /// Kafka broker addresses (comma-separated)
    pub(crate) brokers: String,
    /// Topics to consume from
    pub(crate) topics: Vec<String>,
    /// Consumer group ID
    pub(crate) group_id: String,
    /// Consumer configuration options
    /// see <https://docs.confluent.io/platform/current/clients/librdkafka/html/md_CONFIGURATION.html>
    #[allow(clippy::struct_field_names)]
    #[serde(default)]
    pub(crate) rdkafka_config: HashMap<String, String>,
    /// Header validation rules for incoming messages
    #[serde(default)]
    pub(crate) headers: HeaderRuleMap,
    /// Headers to forward from Kafka message to pipeline
    #[serde(default)]
    pub(crate) headers_to_keep: Vec<String>,
    /// Polling timeout for consumer (default: 1s)
    #[serde(with = "humantime_serde", default = "default_poll_timeout")]
    pub(crate) poll_timeout: Duration,
    /// Whether to commit offsets automatically (default: true)
    #[serde(default = "default_auto_commit")]
    pub(crate) auto_commit: bool,
    /// Base metadata to include in all `EventSource` instances created by this extractor.
    /// The `context.source` field will be automatically populated if not set.
    #[serde(default)]
    pub(crate) metadata: serde_json::Value,
}

pub(super) fn default_poll_timeout() -> Duration {
    Duration::from_secs(1)
}

pub(super) fn default_auto_commit() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_default_values() {
        assert_eq!(default_poll_timeout(), Duration::from_secs(1));
        assert!(default_auto_commit());
    }
}
