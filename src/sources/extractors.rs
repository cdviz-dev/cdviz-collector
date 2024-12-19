use std::sync::Arc;

use super::{opendal, webhook, EventSourcePipe, Extractor};
use crate::errors::Result;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
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

impl Config {
    //TODO include some metadata into the extractor like the source name
    pub(crate) fn make_extractor(
        &self,
        next: EventSourcePipe,
        webhooks: &Arc<DashMap<String, EventSourcePipe>>,
    ) -> Result<Box<dyn Extractor>> {
        let out: Box<dyn Extractor> = match self {
            Config::Sleep => Box::new(SleepExtractor {}),
            Config::Webhook(config) => {
                webhooks.insert(config.id.clone(), next);
                Box::new(NoopExtractor {})
            }
            #[cfg(feature = "source_opendal")]
            Config::Opendal(config) => Box::new(opendal::OpendalExtractor::try_from(config, next)?),
        };
        Ok(out)
    }
}

struct SleepExtractor {}

#[async_trait::async_trait]
impl Extractor for SleepExtractor {
    async fn run(&mut self) -> Result<()> {
        use std::future;

        let future = future::pending();
        let () = future.await;
        unreachable!()
    }
}

struct NoopExtractor {}

#[async_trait::async_trait]
impl Extractor for NoopExtractor {
    async fn run(&mut self) -> Result<()> {
        Ok(())
    }
}
