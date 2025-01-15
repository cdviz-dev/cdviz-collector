#[cfg(feature = "sink_db")]
pub(crate) mod db;
pub(crate) mod debug;
#[cfg(feature = "sink_folder")]
pub(crate) mod folder;
#[cfg(feature = "sink_http")]
pub(crate) mod http;

use crate::errors::Result;
use crate::{Message, Receiver};
use enum_dispatch::enum_dispatch;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

#[cfg(feature = "sink_db")]
use db::DbSink;
use debug::DebugSink;
#[cfg(feature = "sink_folder")]
use folder::FolderSink;
#[cfg(feature = "sink_http")]
use http::HttpSink;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
pub(crate) enum Config {
    #[cfg(feature = "sink_db")]
    #[serde(alias = "db")]
    Db(db::Config),
    #[serde(alias = "debug")]
    Debug(debug::Config),
    #[cfg(feature = "sink_http")]
    #[serde(alias = "http")]
    Http(http::Config),
    #[cfg(feature = "sink_folder")]
    #[serde(alias = "folder")]
    Folder(folder::Config),
}

impl Default for Config {
    fn default() -> Self {
        Self::Debug(debug::Config { enabled: true })
    }
}

impl Config {
    pub(crate) fn is_enabled(&self) -> bool {
        match self {
            #[cfg(feature = "sink_db")]
            Self::Db(db::Config { enabled, .. }) => *enabled,
            Self::Debug(debug::Config { enabled, .. }) => *enabled,
            #[cfg(feature = "sink_folder")]
            Self::Folder(folder::Config { enabled, .. }) => *enabled,
            #[cfg(feature = "sink_http")]
            Self::Http(http::Config { enabled, .. }) => *enabled,
        }
    }
}

impl TryFrom<Config> for SinkEnum {
    type Error = Report;

    fn try_from(value: Config) -> Result<Self> {
        let out = match value {
            #[cfg(feature = "sink_db")]
            Config::Db(config) => DbSink::try_from(config)?.into(),
            Config::Debug(config) => DebugSink::try_from(config)?.into(),
            #[cfg(feature = "sink_folder")]
            Config::Folder(config) => FolderSink::try_from(config)?.into(),
            #[cfg(feature = "sink_http")]
            Config::Http(config) => HttpSink::try_from(config)?.into(),
        };
        Ok(out)
    }
}

#[enum_dispatch]
#[allow(clippy::enum_variant_names)]
enum SinkEnum {
    #[cfg(feature = "sink_db")]
    DbSink,
    DebugSink,
    #[cfg(feature = "sink_folder")]
    FolderSink,
    #[cfg(feature = "sink_http")]
    HttpSink,
}

#[enum_dispatch(SinkEnum)]
trait Sink {
    async fn send(&self, msg: &Message) -> Result<()>;
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
        Ok(())
    })
}
