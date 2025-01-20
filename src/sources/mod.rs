pub(crate) mod extractors;
#[cfg(feature = "source_opendal")]
pub(crate) mod opendal;
mod send_cdevents;
pub(crate) mod transformers;
pub(crate) mod webhook;

use crate::errors::Result;
use crate::pipes::Pipe;
use crate::{Message, Sender};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

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
    _name: &str,
    config: &Config,
    tx: Sender<Message>,
) -> Result<extractors::Extractor> {
    let mut pipe: EventSourcePipe = Box::new(send_cdevents::Processor::new(tx));
    let mut tconfigs = config.transformers.clone();
    tconfigs.reverse();
    for tconfig in tconfigs {
        pipe = tconfig.make_transformer(pipe)?;
    }
    config.extractor.make_extractor(pipe)
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct EventSource {
    pub metadata: Value,
    pub header: HashMap<String, String>,
    pub body: Value,
}

// TODO explore to use enum_dispatch instead of Box(dyn) on EventSourcePipe (a recursive structure)
pub type EventSourcePipe = Box<dyn Pipe<Input = EventSource> + Send + Sync>;
