#[cfg(feature = "source_http_polling")]
use super::http_polling;
#[cfg(feature = "source_kafka")]
use super::kafka;
#[cfg(feature = "source_nats")]
use super::nats;
#[cfg(feature = "source_sse")]
use super::sse;
use super::{EventSourcePipe, cli, opendal, subprocess, webhook};
use crate::errors::Result;
use axum::Router;
use serde::Deserialize;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(tag = "type")]
pub(crate) enum Config {
    #[serde(alias = "noop")]
    #[default]
    Sleep,
    #[serde(alias = "cli")]
    Cli(cli::Config),
    #[serde(alias = "webhook")]
    Webhook(webhook::Config),
    #[cfg(feature = "source_kafka")]
    #[serde(alias = "kafka")]
    Kafka(kafka::config::Config),
    #[cfg(feature = "source_nats")]
    #[serde(alias = "nats")]
    Nats(nats::config::Config),
    #[cfg(feature = "source_opendal")]
    #[serde(alias = "opendal")]
    Opendal(opendal::Config),
    #[cfg(feature = "source_sse")]
    #[serde(alias = "sse")]
    Sse(sse::Config),
    #[serde(alias = "subprocess")]
    Subprocess(subprocess::Config),
    #[cfg(feature = "source_http_polling")]
    #[serde(alias = "http_polling")]
    HttpPolling(http_polling::Config),
}

pub enum Extractor {
    Task(JoinHandle<Result<()>>),
    Webhook(Router),
}

impl Config {
    /// ignore the 'enabled' field, create the extractor like if it was enabled.
    pub(crate) fn make_extractor(
        &self,
        name: &str,
        next: EventSourcePipe,
        cancel_token: CancellationToken,
        state_config: Option<&crate::state::Config>,
    ) -> Result<Extractor> {
        let name = name.to_string();
        let out = match self {
            Config::Sleep => Extractor::Task(tokio::spawn(async move {
                cancel_token.cancelled().await;
                tracing::info!(name, kind = "source", "exiting");
                drop(next); // to drop in cascade channel's sender
                Ok(())
            })),
            Config::Cli(config) => {
                let extractor = cli::CliExtractor::from_config(config, next)?;
                Extractor::Task(tokio::spawn(async move {
                    extractor.run().await?;
                    tracing::info!(name, kind = "source", "exiting");
                    Ok(())
                }))
            }
            Config::Webhook(config) => Extractor::Webhook(webhook::make_route(config, next)),
            #[cfg(feature = "source_kafka")]
            Config::Kafka(config) => {
                let extractor = kafka::KafkaExtractor::try_from(config, next)?;
                Extractor::Task(tokio::spawn(async move {
                    extractor.run(cancel_token).await?;
                    tracing::info!(name, kind = "source", "exiting");
                    Ok(())
                }))
            }
            #[cfg(feature = "source_nats")]
            Config::Nats(config) => {
                let extractor = nats::NatsExtractor::try_from(config, next)?;
                Extractor::Task(tokio::spawn(async move {
                    extractor.run(cancel_token).await?;
                    tracing::info!(name, kind = "source", "exiting");
                    Ok(())
                }))
            }
            #[cfg(feature = "source_opendal")]
            Config::Opendal(config) => {
                let state_op = state_config.and_then(|sc| {
                    sc.make_operator()
                        .map_err(|err| tracing::warn!(?err, "failed to create state operator"))
                        .ok()
                });
                let mut extractor =
                    opendal::OpendalExtractor::try_from(config, next, state_op, name.clone())?;
                Extractor::Task(tokio::spawn(async move {
                    extractor.run(cancel_token).await?;
                    tracing::info!(name, kind = "source", "exiting");
                    drop(extractor); // to drop in cascade channel's sender
                    Ok(())
                }))
            }
            #[cfg(feature = "source_sse")]
            Config::Sse(config) => {
                let extractor = sse::SseExtractor::from(config, next);
                Extractor::Task(tokio::spawn(async move {
                    extractor.run(cancel_token).await?;
                    tracing::info!(name, kind = "source", "exiting");
                    // extractor is moved into run(), so no need to drop explicitly
                    Ok(())
                }))
            }
            Config::Subprocess(config) => {
                let extractor = subprocess::SubprocessExtractor::new(config.clone(), next);
                Extractor::Task(tokio::spawn(async move {
                    extractor.run(cancel_token).await?;
                    tracing::info!(name, kind = "source", "exiting");
                    Ok(())
                }))
            }
            #[cfg(feature = "source_http_polling")]
            Config::HttpPolling(config) => {
                let mut extractor = http_polling::HttpPollingExtractor::try_from(
                    config,
                    next,
                    state_config,
                    name.clone(),
                )?;
                Extractor::Task(tokio::spawn(async move {
                    extractor.run(cancel_token).await?;
                    tracing::info!(name, kind = "source", "exiting");
                    drop(extractor);
                    Ok(())
                }))
            }
        };
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::EventSource;
    use crate::transformers::collect_to_vec::Collector;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn test_extractor_config_sleep_cancellation() {
        let config = Config::Sleep;
        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let cancel_token = CancellationToken::new();

        assert2::assert!(let
            Ok(Extractor::Task(handle)) = config.make_extractor("test", pipe, cancel_token.clone(), None)
        );

        // Cancel the token and verify task completes
        cancel_token.cancel();
        assert2::assert!(let Ok(Ok(_)) = timeout(Duration::from_millis(100), handle).await);
    }

    #[tokio::test]
    async fn test_extractor_config_webhook_creates_router() {
        let config = Config::Webhook(webhook::Config {
            id: "test-webhook".to_string(),
            headers_to_keep: vec!["Content-Type".to_string()],
            ..Default::default()
        });
        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let cancel_token = CancellationToken::new();

        assert2::assert!(let
            Ok(Extractor::Webhook(_router)) = config.make_extractor("test", pipe, cancel_token, None)
        );
    }

    #[cfg(feature = "source_opendal")]
    #[tokio::test]
    async fn test_extractor_config_opendal_invalid_config() {
        use crate::sources::opendal;

        let config = Config::Opendal(opendal::Config {
            polling_interval: std::time::Duration::from_secs(10),
            kind: ::opendal::Scheme::Fs,
            parameters: std::collections::HashMap::new(), // Invalid - missing required params
            recursive: false,
            path_patterns: Vec::new(),
            parser: opendal::parsers::Config::Metadata,
            try_read_headers_json: false,
            metadata: serde_json::json!({}),
        });
        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let cancel_token = CancellationToken::new();

        // Should fail with invalid configuration
        assert2::assert!(let Err(_) = config.make_extractor("test", pipe, cancel_token, None));
    }

    #[test]
    fn test_extractor_config_deserialization() {
        // Test default config
        let toml_str = r#"type = "noop""#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert2::assert!(let Config::Sleep = config);

        // Test webhook config
        let toml_str = r#"
            type = "webhook"
            id = "github"
            headers_to_keep = ["X-GitHub-Event"]
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        if let Config::Webhook(webhook_config) = config {
            assert_eq!(webhook_config.id, "github");
            assert_eq!(webhook_config.headers_to_keep, vec!["X-GitHub-Event"]);
        } else {
            panic!("Expected webhook config");
        }
    }
}

#[cfg(test)]
mod security_tests {
    use super::*;
    use crate::sources::EventSource;
    use crate::transformers::collect_to_vec::Collector;

    #[tokio::test]
    async fn test_extractor_name_injection_safety() {
        // Test that source names don't cause issues when used in logging/tracing
        let very_long = "very-long-name-".repeat(100);
        let problematic_names = vec![
            "normal-name",
            "name with spaces",
            "name/with/slashes",
            "name\"with\"quotes",
            "name\nwith\nnewlines",
            "name\0with\0nulls",
            very_long.as_str(),
        ];

        for name in problematic_names {
            let config = Config::Sleep;
            let collector = Collector::<EventSource>::new();
            let pipe = Box::new(collector.create_pipe());
            let cancel_token = CancellationToken::new();

            // Should not panic or fail due to name content
            let result = config.make_extractor(name, pipe, cancel_token, None);
            assert!(result.is_ok(), "Failed for name: {name}");
        }
    }
}
