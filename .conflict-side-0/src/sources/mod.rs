pub(crate) mod cli;
pub(crate) mod extractors;
#[cfg(feature = "source_kafka")]
pub(crate) mod kafka;
#[cfg(feature = "source_opendal")]
pub(crate) mod opendal;
pub(crate) mod send_cdevents;
#[cfg(feature = "source_sse")]
pub(crate) mod sse;
pub(crate) mod transformers;
pub(crate) mod webhook;

use crate::cdevent_utils::sanitize_id;
use crate::errors::{IntoDiagnostic, Result};
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
        let mut cdevent: CDEvent = serde_json::from_value(body).into_diagnostic()?;
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
        .is_none_or(|v| v.as_str() == Some("0"));
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
        let mhash = Multihash::<64>::wrap(SHA2_256, hash.as_slice()).into_diagnostic()?;
        let cid = Cid::new_v1(RAW, mhash);
        body["context"]["id"] = json!(cid.to_string());
    }
    Ok(())
}

fn set_timestamp_if_missing(body: &mut serde_json::Value) {
    use serde_json::json;
    let compute_timestamp =
        body.get("context").is_none_or(|context| context.get("timestamp").is_none());
    if compute_timestamp {
        let timestamp = chrono::Utc::now().to_rfc3339();
        body["context"]["timestamp"] = json!(timestamp);
    }
}

pub(crate) fn create_sources_and_routes(
    source_configs: impl IntoIterator<Item = (String, Config)>,
    tx: &tokio::sync::broadcast::Sender<Message>,
    cancel_token: &'static CancellationToken,
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
    use assert2::{assert, let_assert};
    use chrono::{DateTime, Utc};
    use serde_json::json;

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
        let_assert!(Some(datetime) = body["context"]["timestamp"].as_str());
        let_assert!(Ok(_) = datetime.parse::<DateTime<Utc>>());
    }
}
