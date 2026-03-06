#[cfg(feature = "transformer_vrl")]
mod vrl;
#[cfg(feature = "transformer_vrl")]
pub(crate) mod vrl_purl;

use crate::{
    errors::{Error, IntoDiagnostic, Result},
    pipes::{discard_all, log, passthrough},
    sources::EventSourcePipe,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(tag = "type")]
pub(crate) enum Config {
    #[serde(alias = "passthrough")]
    #[default]
    Passthrough,
    #[serde(alias = "log")]
    Log(log::Config),
    #[serde(alias = "discard_all")]
    DiscardAll,
    #[cfg(feature = "transformer_vrl")]
    #[serde(alias = "vrl")]
    Vrl { template: String },
}

impl Config {
    pub(crate) fn make_transformer(&self, next: EventSourcePipe) -> Result<EventSourcePipe> {
        let out: EventSourcePipe = match &self {
            Config::Passthrough => Box::new(passthrough::Processor::new(next)),
            Config::Log(config) => Box::new(log::Processor::try_from(config, next)?),
            Config::DiscardAll => Box::new(discard_all::Processor::new()),
            #[cfg(feature = "transformer_vrl")]
            Config::Vrl { template } => Box::new(vrl::Processor::new(template, next)?),
        };
        Ok(out)
    }
}

pub fn resolve_transformer_refs(
    transformer_refs: &[String],
    configs: &HashMap<String, Config>,
) -> Result<Vec<Config>> {
    let transformers = transformer_refs
        .iter()
        .map(|name| {
            configs
                .get(name)
                .cloned()
                .ok_or_else(|| Error::ConfigTransformerNotFound(name.clone()))
                .into_diagnostic()
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(transformers)
}
