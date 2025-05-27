pub(crate) mod extractors;
#[cfg(feature = "source_opendal")]
pub(crate) mod opendal;
mod send_cdevents;
pub(crate) mod transformers;
pub(crate) mod webhook;

use crate::errors::{IntoDiagnostic, Result};
use crate::pipes::Pipe;
use crate::{Message, Sender};
use cdevents_sdk::CDEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

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
        set_id_zero_to_cid(&mut body)?;
        // TODO if source is empty, set a default value based on configuration TBD
        let cdevent: CDEvent = serde_json::from_value(body).into_diagnostic()?;
        Ok(cdevent)
    }
}

#[allow(clippy::indexing_slicing)]
fn set_id_zero_to_cid(body: &mut serde_json::Value) -> Result<()> {
    use cid::Cid;
    use multihash::Multihash;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    const RAW: u64 = 0x55;
    const SHA2_256: u64 = 0x12;

    if body["context"]["id"] == json!("0") {
        // Do not use multihash-codetable because one of it's transitive dependency raise
        // an alert "unmaintained advisory detected" about `proc-macro-error`
        // https://rustsec.org/advisories/RUSTSEC-2024-0370
        // let hash = Code::Sha2_256.digest(serde_json::to_string(&input.body)?.as_bytes());
        let mut hasher = Sha256::new();
        hasher.update(serde_json::to_string(&body).into_diagnostic()?.as_bytes());
        let hash = hasher.finalize();
        let mhash = Multihash::<64>::wrap(SHA2_256, hash.as_slice()).into_diagnostic()?;
        let cid = Cid::new_v1(RAW, mhash);
        body["context"]["id"] = json!(cid.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

        set_id_zero_to_cid(&mut body).unwrap();

        assert_eq!(
            body["context"]["id"],
            json!("bafkreid4ehbvqs3ae6l3htd35xhxbhbfehfkrq3gyf242s6nfcsnz2ueve")
        );
    }
}
