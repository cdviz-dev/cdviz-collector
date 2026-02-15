#[cfg(feature = "sink_clickhouse")]
pub(crate) mod clickhouse;
#[cfg(feature = "sink_db")]
pub(crate) mod db;
pub(crate) mod debug;
#[cfg(feature = "sink_folder")]
pub(crate) mod folder;
#[cfg(feature = "sink_http")]
pub(crate) mod http;
#[cfg(feature = "sink_kafka")]
pub(crate) mod kafka;
#[cfg(feature = "sink_sse")]
pub(crate) mod sse;

use crate::errors::{Report, Result};
use crate::{Message, Receiver};
use axum::Router;
#[cfg(feature = "sink_clickhouse")]
use clickhouse::ClickHouseSink;
#[cfg(feature = "sink_db")]
use db::DbSink;
use debug::DebugSink;
use enum_dispatch::enum_dispatch;
#[cfg(feature = "sink_folder")]
use folder::FolderSink;
#[cfg(feature = "sink_http")]
use http::HttpSink;
#[cfg(feature = "sink_kafka")]
use kafka::KafkaSink;
use serde::Deserialize;
#[cfg(feature = "sink_sse")]
use sse::SseSink;
use tokio::task::JoinHandle;

type SinkHandlesAndRoutes = (Vec<JoinHandle<Result<()>>>, Vec<Router>);

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum Config {
    #[cfg(feature = "sink_clickhouse")]
    #[serde(alias = "clickhouse")]
    Clickhouse(clickhouse::Config),
    #[cfg(feature = "sink_db")]
    #[serde(alias = "db")]
    Db(db::Config),
    #[serde(alias = "debug")]
    Debug(debug::Config),
    #[cfg(feature = "sink_http")]
    #[serde(alias = "http")]
    Http(http::Config),
    #[cfg(feature = "sink_kafka")]
    #[serde(alias = "kafka")]
    Kafka(kafka::Config),
    #[cfg(feature = "sink_folder")]
    #[serde(alias = "folder")]
    Folder(folder::Config),
    #[cfg(feature = "sink_sse")]
    #[serde(alias = "sse")]
    Sse(sse::Config),
}

impl Default for Config {
    fn default() -> Self {
        Self::Debug(debug::Config { enabled: true, ..debug::Config::default() })
    }
}

impl Config {
    pub(crate) fn is_enabled(&self) -> bool {
        match self {
            #[cfg(feature = "sink_clickhouse")]
            Self::Clickhouse(clickhouse::Config { enabled, .. }) => *enabled,
            #[cfg(feature = "sink_db")]
            Self::Db(db::Config { enabled, .. }) => *enabled,
            Self::Debug(debug::Config { enabled, .. }) => *enabled,
            #[cfg(feature = "sink_folder")]
            Self::Folder(folder::Config { enabled, .. }) => *enabled,
            #[cfg(feature = "sink_http")]
            Self::Http(http::Config { enabled, .. }) => *enabled,
            #[cfg(feature = "sink_kafka")]
            Self::Kafka(kafka::Config { enabled, .. }) => *enabled,
            #[cfg(feature = "sink_sse")]
            Self::Sse(sse::Config { enabled, .. }) => *enabled,
        }
    }
}

impl TryFrom<Config> for SinkEnum {
    type Error = Report;

    fn try_from(value: Config) -> Result<Self> {
        let out = match value {
            #[cfg(feature = "sink_clickhouse")]
            Config::Clickhouse(config) => ClickHouseSink::try_from(config)?.into(),
            #[cfg(feature = "sink_db")]
            Config::Db(config) => DbSink::try_from(config)?.into(),
            Config::Debug(config) => DebugSink::try_from(config)?.into(),
            #[cfg(feature = "sink_folder")]
            Config::Folder(config) => FolderSink::try_from(config)?.into(),
            #[cfg(feature = "sink_http")]
            Config::Http(config) => HttpSink::try_from(config)?.into(),
            #[cfg(feature = "sink_kafka")]
            Config::Kafka(config) => KafkaSink::try_from(config)?.into(),
            #[cfg(feature = "sink_sse")]
            Config::Sse(config) => SseSink::try_from(config)?.into(),
        };
        Ok(out)
    }
}

#[enum_dispatch]
#[allow(clippy::enum_variant_names)]
enum SinkEnum {
    #[cfg(feature = "sink_clickhouse")]
    ClickHouseSink,
    #[cfg(feature = "sink_db")]
    DbSink,
    DebugSink,
    #[cfg(feature = "sink_folder")]
    FolderSink,
    #[cfg(feature = "sink_http")]
    HttpSink,
    #[cfg(feature = "sink_kafka")]
    KafkaSink,
    #[cfg(feature = "sink_sse")]
    SseSink,
}

#[enum_dispatch(SinkEnum)]
trait Sink {
    async fn send(&self, msg: &Message) -> Result<()>;

    /// Get the routes that this sink needs to register with the HTTP server
    /// Most sinks don't need routes, so this returns None by default
    fn get_routes(&self) -> Option<axum::Router> {
        None
    }
}

pub(crate) fn start(name: String, config: Config, rx: Receiver<Message>) -> JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let sink = SinkEnum::try_from(config)?;
        let mut rx = rx;
        while let Ok(msg) = rx.recv().await {
            tracing::debug!(name, event_id = ?msg.cdevent.id(), "sending");
            if let Err(err) = sink.send(&msg).await {
                tracing::warn!(name, ?err, "fail during sending of event");
            }
        }
        tracing::info!(name, kind = "sink", "exiting");
        Ok(())
    })
}

/// Create sinks and return both the task handles and any routes they need to register
pub(crate) fn create_sinks_and_routes(
    sink_configs: impl IntoIterator<Item = (String, Config)>,
    tx: &tokio::sync::broadcast::Sender<Message>,
) -> Result<SinkHandlesAndRoutes> {
    let mut handles = Vec::new();
    let mut routes = Vec::new();

    for (name, config) in sink_configs {
        if !config.is_enabled() {
            continue;
        }

        tracing::info!(kind = "sink", name, "starting");

        // Create the sink first to extract any routes
        let sink = SinkEnum::try_from(config.clone())?;

        // Extract routes if the sink provides them
        if let Some(route) = sink.get_routes() {
            routes.push(route);
        }

        // Start the sink task
        let handle = start(name, config, tx.subscribe());
        handles.push(handle);
    }

    Ok((handles, routes))
}
