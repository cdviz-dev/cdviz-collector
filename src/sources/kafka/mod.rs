pub(crate) mod config;

use super::{EventSource, EventSourcePipe};
use crate::errors::{IntoDiagnostic, Result};
use crate::security::rule::{header_rule_map_to_configs, validate_headers};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use config::Config;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::message::{Headers, Message};
use serde_json::Value;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub(crate) struct KafkaExtractor {
    consumer: StreamConsumer,
    next: EventSourcePipe,
    headers_to_keep: Vec<String>,
    header_rules: Vec<crate::security::rule::HeaderRuleConfig>,
    poll_timeout: Duration,
}

impl KafkaExtractor {
    /// must be called from the context of a Tokio 1.x runtime
    pub(crate) fn try_from(config: &Config, next: EventSourcePipe) -> Result<Self> {
        let mut client_config = ClientConfig::new();
        client_config.set("bootstrap.servers", &config.brokers);
        client_config.set("group.id", &config.group_id);

        // Apply additional consumer configuration
        for (key, val) in &config.rdkafka_config {
            client_config.set(key, val);
        }

        // Set some sensible defaults if not overridden
        if !config.rdkafka_config.contains_key("enable.auto.commit") {
            client_config
                .set("enable.auto.commit", if config.auto_commit { "true" } else { "false" });
        }
        if !config.rdkafka_config.contains_key("auto.offset.reset") {
            client_config.set("auto.offset.reset", "earliest");
        }
        if !config.rdkafka_config.contains_key("session.timeout.ms") {
            client_config.set("session.timeout.ms", "30000");
        }

        let consumer: StreamConsumer = client_config.create().into_diagnostic()?;

        // Subscribe to topics
        let topic_refs: Vec<&str> = config.topics.iter().map(String::as_str).collect();
        consumer.subscribe(&topic_refs).into_diagnostic()?;

        let header_rules = header_rule_map_to_configs(&config.headers);

        Ok(KafkaExtractor {
            consumer,
            next,
            headers_to_keep: config.headers_to_keep.clone(),
            header_rules,
            poll_timeout: config.poll_timeout,
        })
    }

    #[tracing::instrument(skip(self, cancel_token))]
    #[allow(clippy::ignored_unit_patterns)]
    pub(crate) async fn run(mut self, cancel_token: CancellationToken) -> Result<()> {
        tracing::info!("Kafka source extractor starting");

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    tracing::info!("Kafka source extractor received cancellation signal");
                    break;
                }
                msg_result = tokio::time::timeout(self.poll_timeout, self.consumer.recv()) => {
                    match msg_result {
                        Ok(Ok(message)) => {
                            let owned_message = message.detach();
                            if let Err(e) = self.process_message(owned_message).await {
                                tracing::warn!(error = ?e, "Failed to process Kafka message");
                            }
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(error = ?e, "Error receiving Kafka message");
                            // Continue on error - don't break the loop
                        }
                        Err(_) => {
                            // Timeout occurred, continue polling
                            tracing::trace!("Kafka poll timeout, continuing");
                        }
                    }
                }
            }
        }

        tracing::info!("Kafka source extractor shutting down");
        Ok(())
    }

    // TODO support opentelemetry (propagation of trace or start of new trace)
    #[tracing::instrument(skip(self, message), fields(
        kafka_topic = %message.topic(),
        kafka_partition = %message.partition(),
        kafka_offset = %message.offset()
    ))]
    async fn process_message(&mut self, message: rdkafka::message::OwnedMessage) -> Result<()> {
        // Extract payload
        let Some(payload) = message.payload() else {
            tracing::debug!("Received Kafka message with no payload, skipping");
            return Ok(());
        };

        // Parse JSON payload
        let body: Value = match serde_json::from_slice(payload) {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to parse Kafka message as JSON, treating as string");
                Value::String(String::from_utf8_lossy(payload).into_owned())
            }
        };

        // Extract Kafka headers and convert to HTTP-style headers for validation
        let mut headers = HashMap::new();
        let mut http_header_map = HeaderMap::new();

        if let Some(kafka_headers) = message.headers() {
            for header in kafka_headers.iter() {
                if let Some(value) = header.value {
                    let value_str = String::from_utf8_lossy(value);

                    // Add to our headers map if it's in headers_to_keep
                    if self.headers_to_keep.contains(&header.key.to_string()) {
                        headers.insert(header.key.to_string(), value_str.to_string());
                    }

                    // Also add to HTTP header map for validation
                    if let (Ok(name), Ok(value)) =
                        (HeaderName::from_str(header.key), HeaderValue::from_str(&value_str))
                    {
                        http_header_map.insert(name, value);
                    }
                }
            }
        }

        // // Add Kafka-specific metadata as headers
        // headers.insert("kafka-topic".to_string(), message.topic().to_string());
        // headers.insert("kafka-partition".to_string(), message.partition().to_string());
        // headers.insert("kafka-offset".to_string(), message.offset().to_string());

        // // Add timestamp if available
        // if let Some(timestamp) = message.timestamp().to_millis() {
        //     headers.insert("kafka-timestamp".to_string(), timestamp.to_string());
        // }

        // Validate headers if rules are configured
        if !self.header_rules.is_empty()
            && let Err(validation_error) =
                validate_headers(&http_header_map, &self.header_rules, Some(payload))
        {
            tracing::warn!(
                topic = message.topic(),
                partition = message.partition(),
                offset = message.offset(),
                error = ?validation_error,
                "Kafka message rejected due to header rule validation failure"
            );
            return Ok(());
        }

        // Create EventSource and send to pipeline
        let event = EventSource {
            metadata: serde_json::json!({
                "kafka_topic": message.topic(),
                "kafka_partition": message.partition(),
                "kafka_offset": message.offset(),
                "kafka_timestamp": message.timestamp().to_millis()
            }),
            headers,
            body,
        };

        if let Err(e) = self.next.send(event) {
            tracing::warn!(
                topic = message.topic(),
                partition = message.partition(),
                offset = message.offset(),
                error = ?e,
                "Failed to send event to pipeline"
            );
        }

        tracing::debug!(
            topic = message.topic(),
            partition = message.partition(),
            offset = message.offset(),
            "Successfully processed Kafka message"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipes::collect_to_vec;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_kafka_config_validation() {
        let config = Config {
            brokers: "localhost:9092".to_string(),
            topics: vec!["test-topic".to_string()],
            group_id: "test-group".to_string(),
            rdkafka_config: HashMap::new(),
            headers: crate::security::rule::HeaderRuleMap::new(),
            headers_to_keep: vec!["content-type".to_string()],
            poll_timeout: Duration::from_secs(1),
            auto_commit: true,
        };

        let collector = collect_to_vec::Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());

        // Should not panic during creation (actual connection happens at run time)
        // must be called from the context of a Tokio 1.x runtime
        assert!(KafkaExtractor::try_from(&config, pipe).is_ok());
    }

    #[tokio::test]
    async fn test_kafka_config_with_custom_consumer_settings() {
        let mut rdkafka_config = HashMap::new();
        rdkafka_config.insert("auto.offset.reset".to_string(), "latest".to_string());
        rdkafka_config.insert("session.timeout.ms".to_string(), "10000".to_string());

        let config = Config {
            brokers: "localhost:9092,localhost:9093".to_string(),
            topics: vec!["events".to_string(), "alerts".to_string()],
            group_id: "cdviz-collector".to_string(),
            rdkafka_config,
            headers: crate::security::rule::HeaderRuleMap::new(),
            headers_to_keep: vec!["X-Event-Type".to_string(), "Authorization".to_string()],
            poll_timeout: Duration::from_secs(5),
            auto_commit: false,
        };

        let collector = collect_to_vec::Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());

        // must be called from the context of a Tokio 1.x runtime
        assert!(KafkaExtractor::try_from(&config, pipe).is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_kafka_source_receives_message_from_topic() {
        use rdkafka::config::ClientConfig;
        use rdkafka::producer::{FutureProducer, FutureRecord};
        use testcontainers::core::ContainerAsync;
        use testcontainers::runners::AsyncRunner;
        use testcontainers_redpanda_rs::Redpanda;
        use tokio_util::sync::CancellationToken;

        // Start Redpanda container
        let kafka_container: ContainerAsync<Redpanda> =
            Redpanda::default().start().await.expect("Failed to start Redpanda container");

        // Get broker address
        let mapped_port = kafka_container.get_host_port_ipv4(9092).await.unwrap();
        let brokers = format!("127.0.0.1:{mapped_port}");

        let topic = "test-source-topic";

        // Create producer to send test message
        let mut producer_config = ClientConfig::new();
        producer_config.set("bootstrap.servers", &brokers);
        let producer: FutureProducer = producer_config.create().expect("Failed to create producer");

        // Send a test message to Kafka
        let test_message = serde_json::json!({
            "test": "data",
            "timestamp": "2023-01-01T00:00:00Z",
            "source": "test-producer"
        });
        let message_body = serde_json::to_string(&test_message).unwrap();

        let record = FutureRecord::to(topic).payload(&message_body).key("test-key");

        producer.send(record, Duration::from_secs(5)).await.expect("Failed to send test message");

        // Create collector to capture received events
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());

        // Create Kafka source/extractor
        let config = Config {
            brokers: brokers.clone(),
            topics: vec![topic.to_string()],
            group_id: "test-consumer-group".to_string(),
            rdkafka_config: HashMap::new(),
            headers: crate::security::rule::HeaderRuleMap::new(),
            headers_to_keep: vec!["content-type".to_string()],
            poll_timeout: Duration::from_secs(1),
            auto_commit: true,
        };

        let extractor =
            KafkaExtractor::try_from(&config, pipe).expect("Failed to create extractor");

        // Run extractor for a short time
        let cancel_token = CancellationToken::new();
        let cancel_token_clone = cancel_token.clone();

        // Cancel after 3 seconds to allow message processing
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            cancel_token_clone.cancel();
        });

        // Run the extractor
        let result = extractor.run(cancel_token).await;
        assert!(result.is_ok(), "Extractor should run successfully");

        // Verify the message was received
        let events: Vec<EventSource> = collector.try_into_iter().unwrap().collect();
        assert!(!events.is_empty(), "Should have received at least one event");

        let received_event = &events[0];

        // Verify the message content
        assert_eq!(received_event.body, test_message);

        // // Verify Kafka metadata headers are present
        // assert!(received_event.headers.contains_key("kafka-topic"));
        // assert!(received_event.headers.contains_key("kafka-partition"));
        // assert!(received_event.headers.contains_key("kafka-offset"));
        // assert_eq!(received_event.headers.get("kafka-topic").unwrap(), topic);

        // Verify metadata is populated
        let metadata = &received_event.metadata;
        assert_eq!(metadata.get("kafka_topic").unwrap().as_str().unwrap(), topic);
        assert!(metadata.get("kafka_partition").is_some());
        assert!(metadata.get("kafka_offset").is_some());
    }
}
