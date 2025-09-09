//TODO add persistance for state (time window to not reprocess same file after restart)

mod filter;
pub(crate) mod parsers;
mod resource;

use self::filter::{FilePatternMatcher, Filter};
use self::parsers::{Parser, ParserEnum};
use self::resource::Resource;
use super::EventSourcePipe;
use crate::errors::{IntoDiagnostic, Result};
use futures::TryStreamExt;
use opendal::{Operator, Scheme};
use serde::Deserialize;
use serde::Serialize;
use serde_with::{DisplayFromStr, serde_as};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::instrument;

#[serde_as]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Config {
    #[serde(with = "humantime_serde")]
    pub(crate) polling_interval: Duration,
    #[serde_as(as = "DisplayFromStr")]
    pub(crate) kind: Scheme,
    pub(crate) parameters: HashMap<String, String>,
    pub(crate) recursive: bool,
    pub(crate) path_patterns: Vec<String>,
    pub(crate) parser: parsers::Config,
    #[serde(default)]
    pub(crate) try_read_headers_json: bool,
}

pub(crate) struct OpendalExtractor {
    op: Operator,
    polling_interval: Duration,
    recursive: bool,
    filter: Filter,
    parser: ParserEnum,
    try_read_headers_json: bool,
}

impl OpendalExtractor {
    pub(crate) fn try_from(value: &Config, next: EventSourcePipe) -> Result<Self> {
        let op: Operator =
            Operator::via_iter(value.kind, value.parameters.clone()).into_diagnostic()?;
        let filter = Filter::from_patterns(FilePatternMatcher::from(&value.path_patterns)?);
        let parser = value.parser.make_parser(next)?;
        let try_read_headers_json = value.try_read_headers_json;
        Ok(Self {
            op,
            polling_interval: value.polling_interval,
            recursive: value.recursive,
            filter,
            parser,
            try_read_headers_json,
        })
    }

    #[instrument(skip(self))]
    pub(crate) async fn run_once(&mut self) -> Result<usize> {
        let op = &self.op;
        let filter = &self.filter;
        let recursive = self.recursive;
        let try_read_headers_json = self.try_read_headers_json;
        let parser = &mut self.parser;
        // TODO convert into arg of instrument
        tracing::debug!(filter=? filter, scheme =? op.info().scheme(), root =? op.info().root(), "scanning");
        let mut lister = op
        .lister_with("")
        .recursive(recursive)
        // Make sure content-length and last-modified been fetched.
        .await.into_diagnostic()?;
        let mut count = 0;
        while let Some(entry) = lister.try_next().await.into_diagnostic()? {
            let resource = Resource::from_entry(op, entry, try_read_headers_json).await;
            if filter.accept(&resource) {
                count += 1;
                if let Err(err) = parser.parse(op, &resource).await {
                    tracing::warn!(?err, path = resource.path(), "fail to process, skip");
                }
            }
        }
        Ok(count)
    }

    pub(crate) async fn run(&mut self, cancel_token: CancellationToken) -> Result<()> {
        while !cancel_token.is_cancelled() {
            if let Err(err) = self.run_once().await {
                tracing::warn!(?err, scheme =? self.op.info().scheme(), root =? self.op.info().root(), "fail during scanning");
            }
            tokio::select! {
                () = sleep(self.polling_interval) => {},
                () = cancel_token.cancelled() => {},
            }
            self.filter.jump_to_next_ts_window();
        }
        Ok(())
    }
}
