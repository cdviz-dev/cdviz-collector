use crate::{
    config,
    errors::{Error, IntoDiagnostic, Result},
    sinks, sources,
};
use cdevents_sdk::CDEvent;
use clap::Args;
use futures::future::TryJoinAll;
use std::{path::PathBuf, sync::LazyLock};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Args)]
#[command(args_conflicts_with_subcommands = true,flatten_help = true, about, long_about = None)]
pub(crate) struct ConnectArgs {
    /// The configuration file to use.
    #[clap(long = "config", env("CDVIZ_COLLECTOR_CONFIG"))]
    config: Option<PathBuf>,

    /// The directory to use as the working directory.
    #[clap(short = 'C', long = "directory")]
    directory: Option<PathBuf>,
}

pub(crate) type Sender<T> = tokio::sync::broadcast::Sender<T>;
pub(crate) type Receiver<T> = tokio::sync::broadcast::Receiver<T>;

#[derive(Clone, Debug)]
pub(crate) struct Message {
    // received_at: OffsetDateTime,
    pub(crate) cdevent: CDEvent,
    //raw: serde_json::Value,
}

impl From<CDEvent> for Message {
    fn from(value: CDEvent) -> Self {
        Self {
            // received_at: OffsetDateTime::now_utc(),
            cdevent: value,
        }
    }
}

static SHUTDOWN_TOKEN: LazyLock<CancellationToken> = LazyLock::new(CancellationToken::new);

//TODO add transformers ( eg file/event info, into cdevents) for sources
//TODO integrations with cloudevents (sources & sink)
//TODO integrations with kafka / redpanda, nats,
/// retuns true if the connection service ran successfully
pub(crate) async fn connect(args: ConnectArgs) -> Result<bool> {
    if let Some(dir) = &args.directory {
        std::env::set_current_dir(dir).into_diagnostic()?;
    }

    let config = config::Config::from_file(args.config)?;

    let (tx, _) = broadcast::channel::<Message>(100);

    let sinks = config
        .sinks
        .into_iter()
        .filter(|(_name, config)| config.is_enabled())
        .inspect(|(name, _config)| tracing::info!(kind = "sink", name, "starting"))
        .map(|(name, config)| sinks::start(name, config, tx.subscribe()))
        .collect::<Vec<_>>();

    if sinks.is_empty() {
        tracing::error!("no sink configured or started");
        return Err(Error::NoSink).into_diagnostic();
    }

    let sources = config
        .sources
        .into_iter()
        .filter(|(_name, config)| config.is_enabled())
        .inspect(|(name, _config)| tracing::info!(kind = "source", name, "starting"))
        .map(|(name, config)| sources::make(&name, &config, tx.clone(), SHUTDOWN_TOKEN.clone()))
        .collect::<Result<Vec<_>>>()?;

    if sources.is_empty() {
        tracing::error!("no source configured or started");
        return Err(Error::NoSource).into_diagnostic();
    }
    let mut join_handles = vec![];
    let mut routes = vec![];
    for source in sources {
        match source {
            sources::extractors::Extractor::Task(task) => join_handles.push(task),
            sources::extractors::Extractor::Webhook(route) => routes.push(route),
        }
    }

    let servers = vec![crate::http::launch(&config.http, routes, &SHUTDOWN_TOKEN)];

    // the channel is closed when all (sender / tx) are dropped) and then in cascade the receiver and the sinks
    // so we can drop the no more useful sender to avoid a leak
    drop(tx);

    // setup the handler to (try to) shutdown gracefully
    tokio::spawn(handle_shutdown_signal());

    //TODO use tokio JoinSet or TaskTracker?
    sinks
        .into_iter()
        .chain(join_handles)
        .chain(servers)
        .collect::<TryJoinAll<_>>()
        .await
        .into_diagnostic()?;
    // handlers.append(&mut sinks);
    // handlers.append(&mut sources);
    //tokio::try_join!(handlers).await?;
    //futures::try_join!(handlers);
    tracing::info!("connect exited (gracefully)");
    Ok(true)
}

#[allow(clippy::expect_used)]
async fn handle_shutdown_signal() {
    use tokio::signal;
    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    SHUTDOWN_TOKEN.cancel();
}

#[cfg(test)]
mod tests {
    use super::*;

    impl proptest::arbitrary::Arbitrary for Message {
        type Parameters = ();
        type Strategy = proptest::strategy::BoxedStrategy<Self>;

        fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
            use proptest::prelude::*;
            (any::<CDEvent>()).prop_map(Message::from).boxed()
        }
    }
}
