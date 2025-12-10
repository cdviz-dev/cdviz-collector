//! Kafka event sink implementation.
//!
//! This module provides a Kafka producer sink that publishes `CDEvents` to Kafka topics.
//! It supports:
//!
//! - `CDEvent` serialization to JSON with configurable content-type headers
//! - Header generation and propagation from source messages
//! - Configurable message keys (unset or `CDEvent` ID-based)
//! - Producer configuration with sensible defaults for reliability
//! - Timeout handling and error reporting with detailed logging
//!
//! The Kafka sink uses the rdkafka library with async/await support and integrates
//! with the cdviz-collector's header security system for outgoing message headers.
//!
//! ## Configuration Example
//!
//! ```toml
//! [sinks.kafka]
//! type = "kafka"
//! enabled = true
//! brokers = "localhost:9092"
//! topic = "cdevents"
//! timeout = "30s"
//! key_policy = "cdevent_id"  # or "unset"
//!
//! # Optional: Custom rdkafka producer configuration
//! [sinks.kafka.rdkafka_config]
//! "acks" = "all"
//! "retries" = "5"
//! "retry.backoff.ms" = "100"
//! "compression.type" = "snappy"
//!
//! # Optional: Header generation configuration
//! [sinks.kafka.headers.hmac]
//! key = "your-secret-key"
//! algorithm = "sha256"
//! header_name = "x-signature"
//! ```

use super::Sink;
use crate::Message;
use crate::errors::{IntoDiagnostic, Report, Result};
use crate::security::header::{
    OutgoingHeaderMap, generate_headers, outgoing_header_map_to_configs,
};
use rdkafka::config::ClientConfig;
use rdkafka::message::Headers;
use rdkafka::producer::{FutureProducer, FutureRecord};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Clone, Debug, Deserialize, Default)]
pub(crate) struct Config {
    /// Is the sink enabled?
    pub(crate) enabled: bool,
    /// Kafka broker addresses (comma-separated)
    brokers: String,
    /// Target topic for messages
    topic: String,
    /// Producer configuration options
    /// see <https://docs.confluent.io/platform/current/clients/librdkafka/html/md_CONFIGURATION.html>
    #[serde(default)]
    #[allow(clippy::struct_field_names)]
    rdkafka_config: HashMap<String, String>,
    /// Header generation for outgoing Kafka messages
    #[serde(default)]
    pub(crate) headers: OutgoingHeaderMap,
    /// Timeout for message production (default 30s)
    #[serde(with = "humantime_serde", default = "default_timeout")]
    timeout: Duration,
    /// Policy to set key of kafka's message (default: "unset")
    #[serde(default)]
    key_policy: KeyPolicy,
}

fn default_timeout() -> Duration {
    Duration::from_secs(30)
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum KeyPolicy {
    // key are not set (or set to null)
    #[default]
    Unset,
    // key are set with the `CDEvent`'s id (aka `context.id` field)
    CdeventId,
}

impl TryFrom<Config> for KafkaSink {
    type Error = Report;

    fn try_from(value: Config) -> Result<Self> {
        let mut client_config = ClientConfig::new();
        client_config.set("bootstrap.servers", &value.brokers);

        // Apply additional producer configuration
        for (key, val) in &value.rdkafka_config {
            client_config.set(key, val);
        }

        // Set some sensible defaults if not overridden
        if !value.rdkafka_config.contains_key("acks") {
            client_config.set("acks", "1");
        }
        if !value.rdkafka_config.contains_key("retries") {
            client_config.set("retries", "3");
        }
        if !value.rdkafka_config.contains_key("retry.backoff.ms") {
            client_config.set("retry.backoff.ms", "100");
        }
        let producer: FutureProducer = client_config.create().into_diagnostic()?;

        Ok(KafkaSink {
            producer,
            topic: value.topic,
            headers: value.headers,
            timeout: value.timeout,
            key_policy: value.key_policy,
        })
    }
}

pub(crate) struct KafkaSink {
    producer: FutureProducer,
    topic: String,
    headers: OutgoingHeaderMap,
    timeout: Duration,
    key_policy: KeyPolicy,
}

impl std::fmt::Debug for KafkaSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KafkaSink")
            .field("topic", &self.topic)
            .field("headers", &self.headers)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl KafkaSink {
    /// Generate configured headers for the Kafka message
    fn generate_kafka_headers(
        &self,
        body: &[u8],
    ) -> Result<Option<rdkafka::message::OwnedHeaders>> {
        let header_configs = outgoing_header_map_to_configs(&self.headers);
        if header_configs.is_empty() {
            return Ok(None);
        }

        let http_headers = match generate_headers(&header_configs, Some(body)) {
            Ok(headers) => headers,
            Err(e) => {
                tracing::error!("Failed to generate headers: {}", e);
                return Err(e).into_diagnostic();
            }
        };
        let mut kafka_headers = rdkafka::message::OwnedHeaders::new();

        // Convert HTTP headers to Kafka headers
        for (name, value) in &http_headers {
            if let Ok(value_str) = value.to_str() {
                kafka_headers = kafka_headers.insert(rdkafka::message::Header {
                    key: name.as_str(),
                    value: Some(value_str),
                });
            }
        }

        Ok(Some(kafka_headers))
    }
}

impl Sink for KafkaSink {
    // TODO support opentelemetry (propagation of trace)
    #[tracing::instrument(skip(self, msg), fields(
        cdevent_id = %msg.cdevent.id(),
        kafka_topic = %self.topic
    ))]
    async fn send(&self, msg: &Message) -> Result<()> {
        let cdevent = msg.cdevent.clone();

        //TODO Convert CDEvent to CloudEvent format and inject data as body and attributes as headers
        // let event_result = EventBuilderV10::new().with_cdevent(cdevent.clone());

        // Prepare the message body
        let (body_bytes, content_type) = {
            let body = serde_json::to_vec(&cdevent).into_diagnostic()?;
            (body, "application/json")
        };

        // Create base record
        let mut record = FutureRecord::to(&self.topic).payload(&body_bytes);
        let key = if self.key_policy == KeyPolicy::CdeventId { cdevent.id().as_str() } else { "" };
        if !key.is_empty() {
            record = record.key(key);
        }

        // Add source headers from the original message
        let mut kafka_headers = rdkafka::message::OwnedHeaders::new();
        for (name, value) in &msg.headers {
            kafka_headers = kafka_headers
                .insert(rdkafka::message::Header { key: name, value: Some(value.as_str()) });
        }

        // Add content type header
        kafka_headers = kafka_headers
            .insert(rdkafka::message::Header { key: "content-type", value: Some(content_type) });

        // Generate and add configured headers
        if let Some(additional_headers) = self.generate_kafka_headers(&body_bytes)? {
            // Merge configured headers (they take precedence)
            for header in additional_headers.iter() {
                kafka_headers = kafka_headers
                    .insert(rdkafka::message::Header { key: header.key, value: header.value });
            }
        }

        record = record.headers(kafka_headers);

        // Send the message
        match self.producer.send(record, self.timeout).await {
            Ok(delivery) => {
                tracing::debug!(
                    cdevent_id = msg.cdevent.id().as_str(),
                    topic = %self.topic,
                    partition = delivery.partition,
                    offset = delivery.offset,
                    "Successfully sent message to Kafka"
                );
                Ok(())
            }
            Err((kafka_error, _original_message)) => {
                tracing::warn!(
                    cdevent_id = msg.cdevent.id().as_str(),
                    topic = %self.topic,
                    error = ?kafka_error,
                    "Failed to send message to Kafka"
                );
                Err(kafka_error).into_diagnostic()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::let_assert;
    use std::collections::HashMap;

    #[test]
    fn test_kafka_sink_config_validation() {
        // Valid config
        let config = Config {
            enabled: true,
            brokers: "localhost:9092".to_string(),
            topic: "test-topic".to_string(),
            ..Default::default()
        };

        // Should not panic during creation (actual connection happens at send time)
        let_assert!(Ok(_) = KafkaSink::try_from(config));
    }

    #[test]
    fn test_kafka_sink_config_with_custom_producer_settings() {
        let mut producer_config = HashMap::new();
        producer_config.insert("acks".to_string(), "all".to_string());
        producer_config.insert("retries".to_string(), "5".to_string());

        let config = Config {
            enabled: true,
            brokers: "localhost:9092,localhost:9093".to_string(),
            topic: "events".to_string(),
            rdkafka_config: producer_config,
            timeout: Duration::from_secs(60),
            ..Default::default()
        };

        let_assert!(Ok(_) = KafkaSink::try_from(config));
    }

    #[test]
    fn test_default_timeout() {
        assert_eq!(default_timeout(), Duration::from_secs(30));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_kafka_sink_sends_message_to_topic() {
        use crate::Message;
        use cdviz_collector_testkit::testcontainers_redpanda::Redpanda;
        use proptest::prelude::*;
        use proptest::test_runner::TestRunner;
        use rdkafka::Message as KafkaMessage;
        use rdkafka::config::ClientConfig;
        use rdkafka::consumer::{Consumer, StreamConsumer};
        use testcontainers::core::ContainerAsync;
        use testcontainers::runners::AsyncRunner;

        // Start Redpanda container
        let kafka_container: ContainerAsync<Redpanda> =
            Redpanda::default().start().await.expect("Failed to start Redpanda container");

        // Get broker address
        let mapped_port = kafka_container.get_host_port_ipv4(9092).await.unwrap();
        let brokers = format!("127.0.0.1:{mapped_port}");

        let topic = "test-sink-topic";

        // Create sink
        let config = Config {
            enabled: true,
            brokers: brokers.clone(),
            topic: topic.to_string(),
            key_policy: KeyPolicy::CdeventId,
            ..Default::default()
        };

        let sink = KafkaSink::try_from(config).expect("Failed to create Kafka sink");

        // Generate a random test message using proptest
        let mut runner = TestRunner::default();
        let test_message = any::<Message>().new_tree(&mut runner).unwrap().current();

        // Send message to Kafka
        sink.send(&test_message).await.expect("Failed to send message");

        // Create consumer to verify message was sent
        let mut consumer_config = ClientConfig::new();
        consumer_config
            .set("bootstrap.servers", &brokers)
            .set("group.id", "test-consumer-group")
            .set("auto.offset.reset", "earliest")
            .set("session.timeout.ms", "10000");

        let consumer: StreamConsumer = consumer_config.create().expect("Failed to create consumer");
        consumer.subscribe(&[topic]).expect("Failed to subscribe to topic");

        // Wait for and verify the message
        let message = tokio::time::timeout(Duration::from_secs(10), consumer.recv())
            .await
            .expect("Timeout waiting for message")
            .expect("Failed to receive message");

        // Verify message content
        let payload = message.payload().expect("Message should have payload");
        let _: serde_json::Value =
            serde_json::from_slice(payload).expect("Message payload should be valid JSON");

        // Verify message key is the event ID
        let message_key = message.key().expect("Message should have key");
        let key_str = std::str::from_utf8(message_key).expect("Key should be valid UTF-8");
        assert_eq!(key_str, test_message.cdevent.id().to_string());

        // Verify topic
        assert_eq!(message.topic(), topic);
    }
}
