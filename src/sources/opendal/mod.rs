mod filter;
pub(crate) mod parsers;
mod resource;

use self::filter::{FilePatternMatcher, Filter};
use self::parsers::{Parser, ParserEnum};
use self::resource::Resource;
use super::EventSourcePipe;
use crate::errors::{IntoDiagnostic, Result};
use futures::TryStreamExt;
use opendal::Operator;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Config {
    #[serde(with = "humantime_serde")]
    pub(crate) polling_interval: Duration,
    pub(crate) kind: String,
    pub(crate) parameters: HashMap<String, String>,
    pub(crate) recursive: bool,
    pub(crate) path_patterns: Vec<String>,
    /// Optional upper cap. Once `ts_after` reaches this value the source stops.
    /// Accepts an RFC3339 timestamp, or `"$now"` which is substituted with the
    /// current timestamp once, at config-load time (see `ConfigBuilder::build_figment`).
    #[serde(default)]
    pub(crate) ts_before_limit: Option<jiff::Timestamp>,
    pub(crate) parser: parsers::Config,
    #[serde(default)]
    pub(crate) try_read_headers_json: bool,
    /// Base metadata to include in all `EventSource` instances created by this extractor.
    /// The `context.source` field will be automatically populated if not set.
    #[serde(default)]
    pub(crate) metadata: serde_json::Value,
}

pub(crate) struct OpendalExtractor {
    op: Operator,
    polling_interval: Duration,
    recursive: bool,
    filter: Filter,
    parser: ParserEnum,
    try_read_headers_json: bool,
    state_op: Option<Operator>,
    source_name: String,
}

impl OpendalExtractor {
    pub(crate) fn try_from(
        config: &Config,
        next: EventSourcePipe,
        state_op: Option<Operator>,
        source_name: String,
    ) -> Result<Self> {
        opendal::install_default();
        let op: Operator =
            Operator::via_iter(&config.kind, config.parameters.clone()).into_diagnostic()?;
        let filter = Filter::from_patterns(
            FilePatternMatcher::from(&config.path_patterns)?,
            config.ts_before_limit,
        );
        let parser = config.parser.make_parser(config.metadata.clone(), next)?;
        let try_read_headers_json = config.try_read_headers_json;
        Ok(Self {
            op,
            polling_interval: config.polling_interval,
            recursive: config.recursive,
            filter,
            parser,
            try_read_headers_json,
            state_op,
            source_name,
        })
    }

    // Not instrumented: this scan runs every `polling_interval` and is usually idle, so a
    // per-scan span would create empty spans. The trace originates only when a message is
    // emitted, via the source `SpanPipe` reached through `parser.parse(...) -> next.send`.
    pub(crate) async fn run_once(&mut self) -> Result<usize> {
        let op = &self.op;
        let filter = &self.filter;
        let recursive = self.recursive;
        let try_read_headers_json = self.try_read_headers_json;
        let parser = &mut self.parser;
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
        if let Some(state_op) = &self.state_op
            && let Some(ts) = crate::state::load_ts_after(state_op, &self.source_name).await
        {
            self.filter.set_ts_after(ts);
        }
        while !cancel_token.is_cancelled() {
            if self.filter.is_at_limit() {
                tracing::info!(source = %self.source_name, "reached ts_before_limit, source stopping");
                break;
            }
            match self.run_once().await {
                Err(err) => {
                    tracing::warn!(?err, scheme =? self.op.info().scheme(), root =? self.op.info().root(), "fail during scanning");
                }
                Ok(count) => {
                    tracing::debug!(count, scheme =? self.op.info().scheme(), root =? self.op.info().root(), "scanning accepted counted resources");
                }
            }
            tokio::select! {
                () = sleep(self.polling_interval) => {},
                () = cancel_token.cancelled() => {},
            }
            self.filter.jump_to_next_ts_window();
            if let Some(state_op) = &self.state_op
                && let Err(err) =
                    crate::state::save_ts_after(state_op, &self.source_name, self.filter.ts_after())
                        .await
            {
                tracing::warn!(?err, source = %self.source_name, "failed to save checkpoint");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::EventSource;
    use crate::transformers::collect_to_vec::Collector;
    use tokio::time::timeout;

    fn make_config(root: &str, ts_before_limit: Option<jiff::Timestamp>) -> Config {
        Config {
            polling_interval: Duration::from_millis(10),
            kind: "fs".to_string(),
            parameters: HashMap::from([("root".to_string(), root.to_string())]),
            recursive: false,
            path_patterns: Vec::new(),
            ts_before_limit,
            parser: parsers::Config::Metadata,
            try_read_headers_json: false,
            metadata: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn test_ts_before_limit_stops_source() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = make_config(&dir.path().to_string_lossy(), Some(jiff::Timestamp::MIN));

        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let mut extractor =
            OpendalExtractor::try_from(&config, pipe, None, "test".to_string()).unwrap();

        let cancel_token = CancellationToken::new();
        let result = timeout(Duration::from_secs(2), extractor.run(cancel_token)).await;
        assert!(result.is_ok(), "run() should complete within timeout");
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn test_no_ts_before_limit_keeps_running() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = make_config(&dir.path().to_string_lossy(), None);

        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let mut extractor =
            OpendalExtractor::try_from(&config, pipe, None, "test".to_string()).unwrap();

        let cancel_token = CancellationToken::new();
        cancel_token.cancel();
        let result = timeout(Duration::from_secs(2), extractor.run(cancel_token)).await;
        assert!(result.is_ok(), "run() should complete within timeout once cancelled");
        assert!(result.unwrap().is_ok());
    }
}
