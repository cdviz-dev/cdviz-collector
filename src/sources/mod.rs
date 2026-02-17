pub(crate) mod cli;
pub(crate) mod extractors;
pub(crate) mod format_converters;
#[cfg(feature = "source_kafka")]
pub(crate) mod kafka;
#[cfg(feature = "source_nats")]
pub(crate) mod nats;
#[cfg(feature = "source_opendal")]
pub(crate) mod opendal;
pub(crate) mod send_cdevents;
#[cfg(feature = "source_sse")]
pub(crate) mod sse;
pub(crate) mod transformers;
pub(crate) mod webhook;

use crate::cdevent_utils::sanitize_id;
use crate::errors::{Error, IntoDiagnostic, Result};
use crate::pipes::Pipe;
use crate::{Message, Sender};
use axum::Router;
use cdevents_sdk::CDEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

type SourceHandlesAndRoutes = (Vec<JoinHandle<Result<()>>>, Vec<Router>);

// TODO support name/reference for extractor / transformer
#[derive(Clone, Debug, Deserialize, Default)]
pub(crate) struct Config {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    extractor: extractors::Config,
    #[serde(default)]
    transformer_refs: Vec<String>,
    #[serde(default)]
    transformers: Vec<transformers::Config>,
}

impl Config {
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn resolve_transformers(
        &mut self,
        configs: &HashMap<String, transformers::Config>,
    ) -> Result<()> {
        let mut tconfigs = transformers::resolve_transformer_refs(&self.transformer_refs, configs)?;
        self.transformers.append(&mut tconfigs);
        Ok(())
    }

    /// Inject `context.source` into extractor metadata if not already set.
    /// This is called during config loading to populate the source URL.
    pub(crate) fn inject_context_source(&mut self, source_name: &str, root_url: &url::Url) {
        // Get mutable reference to the extractor's metadata
        let metadata = match &mut self.extractor {
            extractors::Config::Cli(config) => &mut config.metadata,
            extractors::Config::Webhook(config) => &mut config.metadata,
            #[cfg(feature = "source_opendal")]
            extractors::Config::Opendal(config) => &mut config.metadata,
            #[cfg(feature = "source_kafka")]
            extractors::Config::Kafka(config) => &mut config.metadata,
            #[cfg(feature = "source_nats")]
            extractors::Config::Nats(config) => &mut config.metadata,
            #[cfg(feature = "source_sse")]
            extractors::Config::Sse(config) => &mut config.metadata,
            extractors::Config::Sleep => return, // No metadata for Sleep
        };

        // Check if context.source already exists
        if metadata.get("context").and_then(|c| c.get("source")).is_some() {
            // Already set, don't override
            return;
        }

        // Build URL with proper encoding using url crate
        let mut source_url = root_url.clone();
        source_url.query_pairs_mut().append_pair("source", source_name);

        // Ensure metadata is an object
        if !metadata.is_object() {
            *metadata = serde_json::json!({});
        }

        // Set context.source
        metadata["context"]["source"] = serde_json::json!(source_url.as_str());
    }
}

pub(crate) fn make(
    name: &str,
    config: &Config,
    tx: Sender<Message>,
    cancel_token: CancellationToken,
) -> Result<extractors::Extractor> {
    let mut pipe: EventSourcePipe = Box::new(send_cdevents::Processor::new(tx));
    let mut tconfigs = config.transformers.clone();
    tconfigs.reverse();
    for tconfig in tconfigs {
        pipe = tconfig.make_transformer(pipe)?;
    }
    config.extractor.make_extractor(name, pipe, cancel_token)
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct EventSource {
    pub metadata: Value,
    pub headers: HashMap<String, String>,
    pub body: Value,
}

// TODO explore to use enum_dispatch instead of Box(dyn) on EventSourcePipe (a recursive structure)
pub type EventSourcePipe = Box<dyn Pipe<Input = EventSource> + Send + Sync>;

impl TryFrom<EventSource> for CDEvent {
    type Error = miette::Error;

    fn try_from(value: EventSource) -> std::result::Result<Self, Self::Error> {
        let mut body = value.body;
        set_timestamp_if_missing(&mut body);
        set_id_zero_or_missing_to_cid(&mut body)?;
        // TODO if source is empty, set a default value based on configuration TBD
        let mut cdevent: CDEvent = serde_json::from_value(body.clone()).map_err(|cause0| {
            // to provide located / contextualized error, body is converted into string, to be then parsed with error
            // TODO find an alternatives that avoid string conversion and reparse
            if let Ok(body_str) = serde_json::to_string_pretty(&body)
                && let Err(cause) = serde_json::from_str::<CDEvent>(&body_str)
            {
                Error::from_serde_error(&body_str, cause)
            } else {
                Error::from_serde_error("", cause0)
            }
        })?;
        let sanitized_id = sanitize_id(cdevent.id())?;
        cdevent = cdevent.with_id(sanitized_id);
        Ok(cdevent)
    }
}

//#[allow(clippy::indexing_slicing)]
fn set_id_zero_or_missing_to_cid(body: &mut serde_json::Value) -> Result<()> {
    use cid::Cid;
    use multihash::Multihash;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    const RAW: u64 = 0x55;
    const SHA2_256: u64 = 0x12;

    let compute_id = body
        .get("context")
        .and_then(|context| context.get("id"))
        .is_none_or(|v| v.is_null() || (v.as_str() == Some("0")));
    if compute_id {
        // Do not use multihash-codetable because one of it's transitive dependency raise
        // an alert "unmaintained advisory detected" about `proc-macro-error`
        // https://rustsec.org/advisories/RUSTSEC-2024-0370
        // let hash = Code::Sha2_256.digest(serde_json::to_string(&input.body)?.as_bytes());
        let mut hasher = Sha256::new();
        // remove the field before computation to create the same hash when set to "0" or missing
        body["context"].as_object_mut().map(|obj| obj.remove("id"));
        hasher.update(serde_json::to_string(&body).into_diagnostic()?.as_bytes());
        let hash = hasher.finalize();
        let mhash = Multihash::<64>::wrap(SHA2_256, &hash).into_diagnostic()?;
        let cid = Cid::new_v1(RAW, mhash);
        body["context"]["id"] = json!(cid.to_string());
    }
    Ok(())
}

fn set_timestamp_if_missing(body: &mut serde_json::Value) {
    use serde_json::json;
    let compute_timestamp = body
        .get("context")
        .is_none_or(|context| context.get("timestamp").is_none_or(serde_json::Value::is_null));
    if compute_timestamp {
        let timestamp = jiff::Timestamp::now().to_string(); //rfc3339
        body["context"]["timestamp"] = json!(timestamp);
    }
}

pub(crate) fn create_sources_and_routes(
    source_configs: impl IntoIterator<Item = (String, Config)>,
    tx: &tokio::sync::broadcast::Sender<Message>,
    cancel_token: &CancellationToken,
) -> Result<SourceHandlesAndRoutes> {
    let sources = source_configs
        .into_iter()
        .filter(|(_name, config)| config.is_enabled())
        .inspect(|(name, _config)| tracing::info!(kind = "source", name, "starting"))
        .map(|(name, config)| make(&name, &config, tx.clone(), cancel_token.clone()))
        .collect::<Result<Vec<_>>>()?;

    let mut join_handles = vec![];
    let mut routes = vec![];
    for source in sources {
        match source {
            extractors::Extractor::Task(task) => join_handles.push(task),
            extractors::Extractor::Webhook(route) => routes.push(route),
        }
    }
    Ok((join_handles, routes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::assert;
    use serde_json::json;

    #[test]
    fn test_inject_context_source_with_valid_url() {
        let mut config = Config {
            enabled: true,
            extractor: extractors::Config::Webhook(webhook::Config {
                id: "test".to_string(),
                metadata: json!({}),
                ..Default::default()
            }),
            ..Default::default()
        };

        let root_url = url::Url::parse("http://example.com").unwrap();
        config.inject_context_source("my-webhook", &root_url);

        if let extractors::Config::Webhook(webhook_config) = &config.extractor {
            assert_eq!(
                webhook_config.metadata["context"]["source"],
                json!("http://example.com/?source=my-webhook")
            );
        } else {
            panic!("Expected Webhook config");
        }
    }

    #[test]
    fn test_inject_context_source_preserves_existing() {
        let mut config = Config {
            enabled: true,
            extractor: extractors::Config::Webhook(webhook::Config {
                id: "test".to_string(),
                metadata: json!({"context": {"source": "http://custom.com/hook"}}),
                ..Default::default()
            }),
            ..Default::default()
        };

        let root_url = url::Url::parse("http://example.com").unwrap();
        config.inject_context_source("my-webhook", &root_url);

        if let extractors::Config::Webhook(webhook_config) = &config.extractor {
            assert_eq!(
                webhook_config.metadata["context"]["source"],
                json!("http://custom.com/hook")
            );
        } else {
            panic!("Expected Webhook config");
        }
    }

    #[test]
    fn test_inject_context_source_encodes_special_chars() {
        let mut config = Config {
            enabled: true,
            extractor: extractors::Config::Webhook(webhook::Config {
                id: "test".to_string(),
                metadata: json!({}),
                ..Default::default()
            }),
            ..Default::default()
        };

        let root_url = url::Url::parse("http://example.com").unwrap();
        config.inject_context_source("my webhook/source", &root_url);

        if let extractors::Config::Webhook(webhook_config) = &config.extractor {
            assert_eq!(
                webhook_config.metadata["context"]["source"],
                json!("http://example.com/?source=my+webhook%2Fsource")
            );
        } else {
            panic!("Expected Webhook config");
        }
    }

    #[test]
    fn test_inject_context_source_with_existing_query_params() {
        let mut config = Config {
            enabled: true,
            extractor: extractors::Config::Webhook(webhook::Config {
                id: "test".to_string(),
                metadata: json!({}),
                ..Default::default()
            }),
            ..Default::default()
        };

        let root_url = url::Url::parse("http://example.com/path?existing=param").unwrap();
        config.inject_context_source("my-webhook", &root_url);

        if let extractors::Config::Webhook(webhook_config) = &config.extractor {
            let source_url = webhook_config.metadata["context"]["source"].as_str().unwrap();
            // URL crate should append to existing query params
            assert!(source_url.contains("existing=param"));
            assert!(source_url.contains("source=my-webhook"));
        } else {
            panic!("Expected Webhook config");
        }
    }

    #[test]
    fn test_set_id_preserve_non_zero_id() {
        let mut body = json!({
            "context": {
                "id": "my-id",
                "source": "/event/source/123",
                "type": "dev.cdevents.service.deployed.0.1.1",
                "timestamp": "2023-03-20T14:27:05.315384Z"
            },
            "subject": {
                "id": "mySubject123",
                "source": "/event/source/123",
                "type": "service",
                "content": {
                    "environment": {
                        "id": "test123"
                    },
                    "artifactId": "pkg:oci/myapp@sha256%3A0b31b1c02ff458ad9b7b81cbdf8f028bd54699fa151f221d1e8de6817db93427"
                }
            }
        });

        set_id_zero_or_missing_to_cid(&mut body).unwrap();

        assert_eq!(body["context"]["id"], json!("my-id"));
    }

    #[test]
    fn test_set_id_zero_to_cid() {
        let mut body = json!({
            "context": {
                "id": "0",
                "source": "/event/source/123",
                "type": "dev.cdevents.service.deployed.0.1.1",
                "timestamp": "2023-03-20T14:27:05.315384Z"
            },
            "subject": {
                "id": "mySubject123",
                "source": "/event/source/123",
                "type": "service",
                "content": {
                    "environment": {
                        "id": "test123"
                    },
                    "artifactId": "pkg:oci/myapp@sha256%3A0b31b1c02ff458ad9b7b81cbdf8f028bd54699fa151f221d1e8de6817db93427"
                }
            }
        });

        set_id_zero_or_missing_to_cid(&mut body).unwrap();

        assert_eq!(
            body["context"]["id"],
            json!("bafkreicuuogf3pc3lkdewdnngcnxgfrnontlfvtiihmxvcxqqxphxson3m")
        );
    }

    #[test]
    fn test_set_id_missing_to_cid() {
        let mut body = json!({
            "context": {
                "source": "/event/source/123",
                "type": "dev.cdevents.service.deployed.0.1.1",
                "timestamp": "2023-03-20T14:27:05.315384Z"
            },
            "subject": {
                "id": "mySubject123",
                "source": "/event/source/123",
                "type": "service",
                "content": {
                    "environment": {
                        "id": "test123"
                    },
                    "artifactId": "pkg:oci/myapp@sha256%3A0b31b1c02ff458ad9b7b81cbdf8f028bd54699fa151f221d1e8de6817db93427"
                }
            }
        });

        set_id_zero_or_missing_to_cid(&mut body).unwrap();

        assert_eq!(
            body["context"]["id"],
            json!("bafkreicuuogf3pc3lkdewdnngcnxgfrnontlfvtiihmxvcxqqxphxson3m")
        );
    }

    #[test]
    fn test_set_timestamp_if_missing() {
        let mut body = json!({
            "context": {
                "source": "/event/source/123",
                "type": "dev.cdevents.service.deployed.0.1.1",
            },
            "subject": {
                "id": "mySubject123",
                "source": "/event/source/123",
                "type": "service",
                "content": {
                    "environment": {
                        "id": "test123"
                    },
                    "artifactId": "pkg:oci/myapp@sha256%3A0b31b1c02ff458ad9b7b81cbdf8f028bd54699fa151f221d1e8de6817db93427"
                }
            }
        });

        assert!(!body["context"]["timestamp"].is_string());
        set_timestamp_if_missing(&mut body);
        assert2::assert!(let Some(datetime) = body["context"]["timestamp"].as_str());
        assert2::assert!(let Ok(_) = datetime.parse::<jiff::Timestamp>());
    }
}
