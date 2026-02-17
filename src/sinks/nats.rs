//! NATS event sink implementation.
//!
//! This module provides a NATS publisher sink that publishes `CDEvents` to NATS subjects.
//! It supports:
//!
//! - Core NATS pub/sub publishing
//! - `JetStream` persistent publishing with ack confirmation
//! - `CDEvent` serialization to JSON with configurable content-type headers
//! - Header generation and propagation from source messages
//! - Timeout handling and error reporting with detailed logging
//!
//! ## Configuration Example
//!
//! ```toml
//! # Core NATS sink
//! [sinks.nats]
//! enabled = true
//! type = "nats"
//! servers = "nats://localhost:4222"
//! subject = "cdevents.out"
//!
//! # JetStream sink
//! [sinks.nats]
//! enabled = true
//! type = "nats"
//! servers = "nats://localhost:4222"
//! subject = "cdevents.out"
//! mode = "jetstream"
//! timeout = "10s"
//! ```

use super::Sink;
use crate::Message;
use crate::errors::{IntoDiagnostic, Report, Result};
use crate::security::header::{
    OutgoingHeaderMap, generate_headers, outgoing_header_map_to_configs,
};
use async_nats::header::HeaderValue as NatsHeaderValue;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Debug, Deserialize, Default)]
pub(crate) struct Config {
    /// Is the sink enabled?
    pub(crate) enabled: bool,
    /// NATS server URLs (comma-separated), e.g. `<nats://localhost:4222>`
    servers: String,
    /// Target subject for messages
    subject: String,
    /// Sink mode: "core" (default) or "jetstream"
    #[serde(default)]
    mode: NatsSinkMode,
    /// Timeout for `JetStream` publish ack (default 30s)
    #[serde(with = "humantime_serde", default = "default_timeout")]
    timeout: Duration,
    /// Header generation for outgoing NATS messages
    #[serde(default)]
    pub(crate) headers: OutgoingHeaderMap,
}

fn default_timeout() -> Duration {
    Duration::from_secs(30)
}

/// NATS sink operation mode
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NatsSinkMode {
    /// Core NATS pub/sub (default)
    #[default]
    Core,
    /// `JetStream` persistent streaming with ack
    Jetstream,
}

pub(crate) struct NatsSink {
    client: async_nats::Client,
    subject: String,
    mode: NatsSinkMode,
    timeout: Duration,
    headers: OutgoingHeaderMap,
}

impl std::fmt::Debug for NatsSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NatsSink")
            .field("subject", &self.subject)
            .field("mode", &self.mode)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl TryFrom<Config> for NatsSink {
    type Error = Report;

    fn try_from(value: Config) -> Result<Self> {
        let servers: Vec<String> = value.servers.split(',').map(|s| s.trim().to_string()).collect();
        let client = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async_nats::connect(servers))
                .into_diagnostic()
        })?;
        Ok(NatsSink {
            client,
            subject: value.subject,
            mode: value.mode,
            timeout: value.timeout,
            headers: value.headers,
        })
    }
}

impl NatsSink {
    /// Build a NATS `HeaderMap` from the outgoing header config and source message headers.
    fn build_nats_headers(
        &self,
        body: &[u8],
        msg_headers: &std::collections::HashMap<String, String>,
    ) -> Result<async_nats::HeaderMap> {
        let mut nats_headers = async_nats::HeaderMap::new();

        // Forward headers from the source message
        for (name, value) in msg_headers {
            nats_headers.insert(name.as_str(), NatsHeaderValue::from(value.as_str()));
        }

        // Add content-type
        nats_headers.insert("content-type", NatsHeaderValue::from("application/json"));

        // Generate and add configured outgoing headers (they take precedence)
        let header_configs = outgoing_header_map_to_configs(&self.headers);
        if !header_configs.is_empty() {
            let http_headers = generate_headers(&header_configs, Some(body)).into_diagnostic()?;
            for (name, value) in &http_headers {
                if let Ok(value_str) = value.to_str() {
                    nats_headers.insert(name.as_str(), NatsHeaderValue::from(value_str));
                }
            }
        }

        Ok(nats_headers)
    }
}

impl Sink for NatsSink {
    #[tracing::instrument(skip(self, msg), fields(
        cdevent_id = %msg.cdevent.id(),
        nats_subject = %self.subject
    ))]
    async fn send(&self, msg: &Message) -> Result<()> {
        let body_bytes = serde_json::to_vec(&msg.cdevent).into_diagnostic()?;
        let nats_headers = self.build_nats_headers(&body_bytes, &msg.headers)?;
        let subject = self.subject.clone();
        let payload = bytes::Bytes::from(body_bytes);

        match self.mode {
            NatsSinkMode::Core => {
                self.client
                    .publish_with_headers(subject, nats_headers, payload)
                    .await
                    .into_diagnostic()?;
                tracing::debug!(
                    cdevent_id = msg.cdevent.id().as_str(),
                    subject = %self.subject,
                    "Successfully sent message to NATS (core)"
                );
            }
            NatsSinkMode::Jetstream => {
                let jetstream = async_nats::jetstream::new(self.client.clone());
                let ack_future = jetstream
                    .publish_with_headers(subject, nats_headers, payload)
                    .await
                    .into_diagnostic()?;
                tokio::time::timeout(self.timeout, ack_future)
                    .await
                    .map_err(|_| {
                        miette::miette!(
                            "Timed out waiting for JetStream ack on subject '{}'",
                            self.subject
                        )
                    })?
                    .into_diagnostic()?;
                tracing::debug!(
                    cdevent_id = msg.cdevent.id().as_str(),
                    subject = %self.subject,
                    "Successfully sent message to NATS JetStream with ack"
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_timeout() {
        assert_eq!(default_timeout(), Duration::from_secs(30));
    }

    #[test]
    fn test_nats_sink_mode_default() {
        assert_eq!(NatsSinkMode::default(), NatsSinkMode::Core);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_nats_sink_sends_core_message() {
        use crate::Message;
        use cdviz_collector_testkit::testcontainers_nats::Nats;
        use futures::StreamExt;
        use proptest::prelude::*;
        use proptest::test_runner::TestRunner;
        use testcontainers::runners::AsyncRunner;

        let nats_container = Nats::default().start().await.expect("start NATS container");
        let server_url =
            cdviz_collector_testkit::testcontainers_nats::find_server_url(&nats_container).await;

        let subject = "test.sink.core";

        // Subscribe before publishing so we don't miss the message
        let subscriber_client = async_nats::connect(&server_url).await.expect("connect subscriber");
        let mut subscriber = subscriber_client.subscribe(subject).await.expect("subscribe");

        let config = Config {
            enabled: true,
            servers: server_url,
            subject: subject.to_string(),
            mode: NatsSinkMode::Core,
            ..Default::default()
        };
        let sink = NatsSink::try_from(config).expect("Failed to create NATS sink");

        let mut runner = TestRunner::default();
        let test_message = any::<Message>().new_tree(&mut runner).unwrap().current();

        sink.send(&test_message).await.expect("Failed to send message");

        let received = tokio::time::timeout(Duration::from_secs(5), subscriber.next())
            .await
            .expect("Timeout waiting for message")
            .expect("No message received");

        // Verify payload is valid JSON
        let _: serde_json::Value =
            serde_json::from_slice(&received.payload).expect("Payload should be valid JSON");

        assert_eq!(received.subject.as_str(), subject);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_nats_sink_sends_jetstream_message() {
        use crate::Message;
        use cdviz_collector_testkit::testcontainers_nats::Nats;
        use proptest::prelude::*;
        use proptest::test_runner::TestRunner;
        use testcontainers::runners::AsyncRunner;

        let nats_container = Nats::default().start().await.expect("start NATS container");
        let server_url =
            cdviz_collector_testkit::testcontainers_nats::find_server_url(&nats_container).await;

        let subject = "js.sink.events";
        let stream_name = "SINK_TEST";

        // Create the stream before publishing
        let admin_client = async_nats::connect(&server_url).await.expect("connect admin");
        let js_admin = async_nats::jetstream::new(admin_client);
        js_admin
            .get_or_create_stream(async_nats::jetstream::stream::Config {
                name: stream_name.to_string(),
                subjects: vec![subject.to_string()],
                ..Default::default()
            })
            .await
            .expect("create stream");

        let config = Config {
            enabled: true,
            servers: server_url,
            subject: subject.to_string(),
            mode: NatsSinkMode::Jetstream,
            timeout: Duration::from_secs(5),
            ..Default::default()
        };
        let sink = NatsSink::try_from(config).expect("Failed to create NATS JetStream sink");

        let mut runner = TestRunner::default();
        let test_message = any::<Message>().new_tree(&mut runner).unwrap().current();

        sink.send(&test_message).await.expect("Failed to send JetStream message");
    }
}
