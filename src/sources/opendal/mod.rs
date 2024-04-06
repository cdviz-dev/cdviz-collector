mod filter;

use crate::errors::{Error, Result};
use crate::{Message, Sender};
use cdevents_sdk::CDEvent;
use filter::{globset_from, Filter};
use futures::TryStreamExt;
use opendal::Entry;
use opendal::Metakey;
use opendal::Operator;
use opendal::Scheme;
use serde::Deserialize;
use serde::Serialize;
use serde_with::{serde_as, DisplayFromStr};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::sleep;
use tracing::instrument;

use super::Source;

//TODO add persistance for state (time window to not reprocess same file after restart)
//TODO add transformer: identity, csv+template -> bunch, jsonl

#[serde_as]
#[derive(Debug, Deserialize, Serialize, Default)]
pub(crate) struct Config {
    #[serde(with = "humantime_serde")]
    polling_interval: Duration,
    #[serde_as(as = "DisplayFromStr")]
    kind: Scheme,
    parameters: HashMap<String, String>,
    recursive: bool,
    path_patterns: Vec<String>,
}

impl TryFrom<Config> for OpendalSource {
    type Error = crate::errors::Error;

    fn try_from(value: Config) -> Result<Self> {
        let op: Operator = Operator::via_map(value.kind, value.parameters)?;
        let filter = Filter::from_patterns(globset_from(&value.path_patterns)?);
        Ok(Self {
            op,
            polling_interval: value.polling_interval,
            recursive: value.recursive,
            filter,
        })
    }
}

pub(crate) struct OpendalSource {
    op: Operator,
    polling_interval: Duration,
    recursive: bool,
    filter: Filter,
}

impl Source for OpendalSource {
    async fn run(&mut self, tx: Sender<Message>) -> Result<()> {
        loop {
            if let Err(err) = run_once(&tx, &self.op, &self.filter, self.recursive).await {
                tracing::warn!(?err, filter = ?self.filter, scheme =? self.op.info().scheme(), root =? self.op.info().root(), "fail during scanning");
            }
            sleep(self.polling_interval).await;
            self.filter.jump_to_next_ts_window();
        }
    }
}

#[instrument]
pub(crate) async fn run_once(
    tx: &Sender<Message>,
    op: &Operator,
    filter: &Filter,
    recursive: bool,
) -> Result<()> {
    // TODO convert into arg of instrument
    tracing::debug!(filter=? filter, scheme =? op.info().scheme(), root =? op.info().root(), "scanning");
    let mut lister = op
        .lister_with("")
        .recursive(recursive)
        // Make sure content-length and last-modified been fetched.
        .metakey(Metakey::ContentLength | Metakey::LastModified)
        .await?;
    while let Some(entry) = lister.try_next().await? {
        if filter.accept(&entry) {
            if let Err(err) = process_entry(tx, op, &entry).await {
                tracing::warn!(?err, path = entry.path(), "fail to process, skip")
            }
        }
    }
    Ok(())
}

async fn process_entry(tx: &Sender<Message>, op: &Operator, entry: &Entry) -> Result<usize> {
    let read = op.read(entry.path()).await?;
    let cdevent: CDEvent = serde_json::from_slice::<CDEvent>(&read)?;
    tx.send(cdevent.into()).map_err(Error::from)
}
