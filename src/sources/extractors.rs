use super::{opendal, webhook, EventSourcePipe};
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
