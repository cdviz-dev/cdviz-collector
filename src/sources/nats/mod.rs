//! NATS event source implementation.
//!
//! This module provides a NATS subscriber source that receives messages from NATS
//! and processes them through the cdviz-collector pipeline. It supports:
//!
//! - Core NATS pub/sub subscription
//! - `JetStream` durable consumer for persistent streaming
//! - Header validation and selective header forwarding
//! - JSON payload parsing with graceful fallback to string
//! - Message metadata preservation (subject, sequence for `JetStream`)
//!
//! ## Configuration Example
//!
//! ```toml
//! # Core NATS
//! [sources.nats]
//! enabled = true
//! [sources.nats.extractor]
//! type = "nats"
//! servers = "nats://localhost:4222"
//! subject = "cdevents.>"
//! mode = "core"
//!
//! # JetStream
//! [sources.nats]
//! enabled = true
//! [sources.nats.extractor]
//! type = "nats"
//! servers = "nats://localhost:4222"
//! subject = "cdevents.>"
//! mode = "jetstream"
//! stream = "EVENTS"
//! consumer = "cdviz-collector"
//! ```

pub(crate) mod config;

use super::{EventSource, EventSourcePipe};
use crate::errors::{IntoDiagnostic, Result};
use crate::security::rule::{header_rule_map_to_configs, validate_headers};
use async_nats::HeaderMap as NatsHeaderMap;
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use config::{Config, NatsSourceMode};
use futures::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use std::str::FromStr;
use tokio_util::sync::CancellationToken;

pub(crate) struct NatsExtractor {
    client: async_nats::Client,
    config: Config,
    next: EventSourcePipe,
    base_metadata: serde_json::Value,
    header_rules: Vec<crate::security::rule::HeaderRuleConfig>,
}

impl NatsExtractor {
    /// Must be called from the context of a Tokio 1.x runtime.
    pub(crate) fn try_from(config: &Config, next: EventSourcePipe) -> Result<Self> {
        let servers: Vec<String> =
            config.servers.split(',').map(|s| s.trim().to_string()).collect();
        let client = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async_nats::connect(servers))
                .into_diagnostic()
        })?;
        let header_rules = header_rule_map_to_configs(&config.headers);
        Ok(NatsExtractor {
            client,
            config: config.clone(),
            next,
            base_metadata: config.metadata.clone(),
            header_rules,
        })
    }

    #[tracing::instrument(skip(self, cancel_token))]
    pub(crate) async fn run(mut self, cancel_token: CancellationToken) -> Result<()> {
        tracing::info!(mode = ?self.config.mode, "NATS source extractor starting");
        match self.config.mode {
            NatsSourceMode::Core => self.run_core(cancel_token).await,
            NatsSourceMode::Jetstream => self.run_jetstream(cancel_token).await,
        }
    }

    async fn run_core(&mut self, cancel_token: CancellationToken) -> Result<()> {
        let subject = self.config.subject.clone();
        let mut subscriber = self.client.subscribe(subject.clone()).await.into_diagnostic()?;

        loop {
            tokio::select! {
                () = cancel_token.cancelled() => {
                    break;
                }
                msg = subscriber.next() => {
                    if let Some(message) = msg {
                        let subject_str = message.subject.as_str().to_string();
                        self.process_message(
                            &subject_str,
                            &message.payload,
                            message.headers.as_ref(),
                            None,
                        );
                    } else {
                        tracing::info!("NATS subscription stream ended");
                        break;
                    }
                }
            }
        }

        subscriber.unsubscribe().await.into_diagnostic()?;
        tracing::info!("NATS core source extractor shutting down");
        Ok(())
    }

    async fn run_jetstream(&mut self, cancel_token: CancellationToken) -> Result<()> {
        let stream_name = self
            .config
            .stream
            .clone()
            .ok_or_else(|| miette::miette!("JetStream mode requires 'stream' to be set"))?;
        let consumer_name = self
            .config
            .consumer
            .clone()
            .ok_or_else(|| miette::miette!("JetStream mode requires 'consumer' to be set"))?;

        let jetstream = async_nats::jetstream::new(self.client.clone());

        // Get or create the stream
        let stream = jetstream
            .get_or_create_stream(async_nats::jetstream::stream::Config {
                name: stream_name.clone(),
                subjects: vec![self.config.subject.clone()],
                ..Default::default()
            })
            .await
            .into_diagnostic()?;

        // Get or create a pull consumer
        let consumer: async_nats::jetstream::consumer::PullConsumer = stream
            .get_or_create_consumer(
                &consumer_name,
                async_nats::jetstream::consumer::pull::Config {
                    durable_name: Some(consumer_name.clone()),
                    filter_subject: self.config.subject.clone(),
                    ..Default::default()
                },
            )
            .await
            .into_diagnostic()?;

        let mut messages = consumer.messages().await.into_diagnostic()?;

        loop {
            tokio::select! {
                () = cancel_token.cancelled() => {
                    break;
                }
                msg = messages.next() => {
                    match msg {
                        Some(Ok(message)) => {
                            let subject_str = message.subject.as_str().to_string();
                            let sequence = message.info().ok().map(|i| i.stream_sequence);
                            self.process_message(
                                &subject_str,
                                &message.payload,
                                message.headers.as_ref(),
                                sequence,
                            );
                            if let Err(e) = message.ack().await {
                                tracing::warn!(error = ?e, "Failed to ack JetStream message");
                            }
                        }
                        Some(Err(e)) => {
                            tracing::warn!(error = ?e, "Error receiving JetStream message");
                        }
                        None => {
                            tracing::info!("JetStream message stream ended");
                            break;
                        }
                    }
                }
            }
        }

        tracing::info!("NATS JetStream source extractor shutting down");
        Ok(())
    }

    fn process_message(
        &mut self,
        subject: &str,
        payload: &bytes::Bytes,
        nats_headers: Option<&NatsHeaderMap>,
        stream_sequence: Option<u64>,
    ) {
        // Parse JSON payload
        let body: Value = match serde_json::from_slice(payload) {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to parse NATS message as JSON, treating as string");
                Value::String(String::from_utf8_lossy(payload).into_owned())
            }
        };

        // Extract headers and convert for validation
        let mut headers = HashMap::new();
        let mut http_header_map = HeaderMap::new();

        if let Some(nats_hdrs) = nats_headers {
            // iter() yields (HeaderName, &Vec<HeaderValue>) — one entry per unique key
            for (name, values) in nats_hdrs.iter() {
                // HeaderName::as_str() is private; use Display instead
                let name_str = name.to_string();
                for value in values {
                    let value_str = value.as_str().to_string();

                    if self.config.headers_to_keep.contains(&name_str) {
                        headers.insert(name_str.clone(), value_str.clone());
                    }

                    if let (Ok(http_name), Ok(http_value)) =
                        (HeaderName::from_str(&name_str), HeaderValue::from_str(&value_str))
                    {
                        http_header_map.insert(http_name, http_value);
                    }
                }
            }
        }

        // Validate headers if rules are configured
        if !self.header_rules.is_empty()
            && let Err(validation_error) =
                validate_headers(&http_header_map, &self.header_rules, Some(payload))
        {
            tracing::warn!(
                subject,
                error = ?validation_error,
                "NATS message rejected due to header rule validation failure"
            );
            return;
        }

        // Build metadata merging base with NATS-specific fields
        let mut metadata = self.base_metadata.clone();
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert("nats_subject".to_string(), serde_json::json!(subject));
            if let Some(seq) = stream_sequence {
                obj.insert("nats_sequence".to_string(), serde_json::json!(seq));
            }
        }

        let event = EventSource { metadata, headers, body };

        if let Err(e) = self.next.send(event) {
            tracing::warn!(subject, error = ?e, "Failed to send NATS event to pipeline");
        }

        tracing::debug!(subject, "Successfully processed NATS message");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipes::collect_to_vec;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_nats_source_config_validation() {
        let config = Config {
            servers: "nats://localhost:4222".to_string(),
            subject: "test.>".to_string(),
            mode: config::NatsSourceMode::Core,
            stream: None,
            consumer: None,
            headers: crate::security::rule::HeaderRuleMap::new(),
            headers_to_keep: vec![],
            metadata: serde_json::json!({}),
        };

        let collector = collect_to_vec::Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());

        // Connection to non-existent server will still construct (async-nats connects lazily)
        // try_from uses block_in_place so we need a multi-thread runtime
        let result = NatsExtractor::try_from(&config, pipe);
        // May succeed or fail depending on async-nats lazy connect behaviour
        drop(result);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_nats_source_receives_message() {
        use cdviz_collector_testkit::testcontainers_nats::Nats;
        use testcontainers::runners::AsyncRunner;
        use tokio_util::sync::CancellationToken;

        let nats_container = Nats::default().start().await.expect("start NATS container");
        let server_url =
            cdviz_collector_testkit::testcontainers_nats::find_server_url(&nats_container).await;

        let subject = "test.events";

        // Create collector to capture received events
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());

        let config = Config {
            servers: server_url.clone(),
            subject: subject.to_string(),
            mode: config::NatsSourceMode::Core,
            stream: None,
            consumer: None,
            headers: crate::security::rule::HeaderRuleMap::new(),
            headers_to_keep: vec![],
            metadata: serde_json::json!({}),
        };

        let extractor = NatsExtractor::try_from(&config, pipe).expect("Failed to create extractor");

        let cancel_token = CancellationToken::new();
        let cancel_clone = cancel_token.clone();

        // Connect a publisher and send a message
        let server_url_clone = server_url.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let client = async_nats::connect(&server_url_clone).await.expect("connect");
            let test_event = serde_json::json!({
                "context": {
                    "id": "test-id",
                    "source": "/test",
                    "type": "dev.cdevents.service.deployed.0.1.1",
                    "timestamp": "2024-01-01T00:00:00Z",
                    "version": "0.4.0"
                },
                "subject": {
                    "id": "subj",
                    "source": "/test",
                    "type": "service",
                    "content": {}
                }
            });
            let payload = serde_json::to_vec(&test_event).unwrap();
            client.publish(subject, bytes::Bytes::from(payload)).await.expect("publish");
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            cancel_clone.cancel();
        });

        let result = extractor.run(cancel_token).await;
        assert!(result.is_ok(), "Extractor should run successfully");

        let events: Vec<EventSource> = collector.try_into_iter().unwrap().collect();
        assert!(!events.is_empty(), "Should have received at least one event");

        let received = &events[0];
        let metadata = &received.metadata;
        assert_eq!(metadata.get("nats_subject").unwrap().as_str().unwrap(), subject);
    }
}
