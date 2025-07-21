use super::{EventSourcePipe, opendal, webhook};
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
    #[serde(alias = "webhook")]
    Webhook(webhook::Config),
    #[cfg(feature = "source_opendal")]
    #[serde(alias = "opendal")]
    Opendal(opendal::Config),
}

pub enum Extractor {
    Task(JoinHandle<Result<()>>),
    Webhook(Router),
}

impl Config {
    //TODO include some metadata into the extractor like the source name
    pub(crate) fn make_extractor(
        &self,
        name: &str,
        next: EventSourcePipe,
        cancel_token: CancellationToken,
    ) -> Result<Extractor> {
        let name = name.to_string();
        let out = match self {
            Config::Sleep => Extractor::Task(tokio::spawn(async move {
                cancel_token.cancelled().await;
                tracing::info!(name, kind = "source", "exiting");
                drop(next); // to drop in cascade channel's sender
                Ok(())
            })),
            Config::Webhook(config) => Extractor::Webhook(webhook::make_route(config, next)),
            #[cfg(feature = "source_opendal")]
            Config::Opendal(config) => {
                let mut extractor = opendal::OpendalExtractor::try_from(config, next)?;
                Extractor::Task(tokio::spawn(async move {
                    extractor.run(cancel_token).await?;
                    tracing::info!(name, kind = "source", "exiting");
                    drop(extractor); // to drop in cascade channel's sender
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
    use crate::pipes::collect_to_vec::Collector;
    use crate::sources::EventSource;
    use assert2::let_assert;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn test_extractor_config_sleep_cancellation() {
        let config = Config::Sleep;
        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let cancel_token = CancellationToken::new();

        let_assert!(
            Ok(Extractor::Task(handle)) = config.make_extractor("test", pipe, cancel_token.clone())
        );

        // Cancel the token and verify task completes
        cancel_token.cancel();
        let_assert!(Ok(Ok(_)) = timeout(Duration::from_millis(100), handle).await);
    }

    #[tokio::test]
    async fn test_extractor_config_webhook_creates_router() {
        let config = Config::Webhook(webhook::Config {
            id: "test-webhook".to_string(),
            headers_to_keep: vec!["Content-Type".to_string()],
            signature: None,
        });
        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let cancel_token = CancellationToken::new();

        let_assert!(
            Ok(Extractor::Webhook(_router)) = config.make_extractor("test", pipe, cancel_token)
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
        });
        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let cancel_token = CancellationToken::new();

        // Should fail with invalid configuration
        let_assert!(Err(_) = config.make_extractor("test", pipe, cancel_token));
    }

    #[test]
    fn test_extractor_config_deserialization() {
        // Test default config
        let toml_str = r#"type = "noop""#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let_assert!(Config::Sleep = config);

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
    use crate::pipes::collect_to_vec::Collector;
    use crate::security::signature::{Encoding, SignatureConfig, SignatureOn};
    use crate::sources::EventSource;

    #[test]
    fn test_webhook_config_with_signature_validation() {
        let config = Config::Webhook(webhook::Config {
            id: "secure-webhook".to_string(),
            headers_to_keep: vec!["Content-Type".to_string()],
            signature: Some(SignatureConfig {
                header: "X-Hub-Signature-256".to_string(),
                token: "secret-token".to_string().into(),
                token_encoding: None,
                signature_prefix: Some("sha256=".to_string()),
                signature_on: SignatureOn::Body,
                signature_encoding: Encoding::Hex,
            }),
        });

        if let Config::Webhook(webhook_config) = config {
            assert!(webhook_config.signature.is_some());
            let sig_config = webhook_config.signature.unwrap();
            assert_eq!(sig_config.header, "X-Hub-Signature-256");
            assert_eq!(sig_config.signature_prefix, Some("sha256=".to_string()));
        } else {
            panic!("Expected webhook config with signature");
        }
    }

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
            let result = config.make_extractor(name, pipe, cancel_token);
            assert!(result.is_ok(), "Failed for name: {name}");
        }
    }
}
