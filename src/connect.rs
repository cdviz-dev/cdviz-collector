use crate::{
    config,
    errors::{Error, IntoDiagnostic, Result},
    sinks, sources,
};
use cdevents_sdk::CDEvent;
use clap::Args;
use futures::future::TryJoinAll;
use opentelemetry::trace::{SpanId, TraceContextExt, TraceId};
use std::{path::PathBuf, sync::LazyLock};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing_opentelemetry::OpenTelemetrySpanExt;

static SHUTDOWN_TOKEN: LazyLock<CancellationToken> = LazyLock::new(CancellationToken::new);

/// Simplified trace context for message correlation
#[derive(Debug, Clone)]
pub(crate) struct TraceContext {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    #[allow(dead_code)] // Reserved for future W3C trace context propagation
    pub trace_flags: u8,
}

#[derive(Debug, Clone, Args)]
#[command(args_conflicts_with_subcommands = true,flatten_help = true, about, long_about = None)]
pub(crate) struct ConnectArgs {
    /// The configuration file to use.
    #[clap(long = "config", env("CDVIZ_COLLECTOR_CONFIG"))]
    config: Option<PathBuf>,
}

pub(crate) type Sender<T> = tokio::sync::broadcast::Sender<T>;
pub(crate) type Receiver<T> = tokio::sync::broadcast::Receiver<T>;

#[derive(Clone, Debug)]
pub(crate) struct Message {
    // received_at: OffsetDateTime,
    pub(crate) cdevent: CDEvent,
    #[allow(dead_code)] // Headers will be used by sinks that need them
    pub(crate) headers: std::collections::HashMap<String, String>,
    /// Trace context for distributed tracing across the message queue
    /// Contains the trace ID and span context for correlation
    pub(crate) trace_context: Option<TraceContext>,
    //raw: serde_json::Value,
}

impl Message {
    /// Creates a new Message with the current trace context
    pub(crate) fn with_trace_context(
        cdevent: CDEvent,
        headers: std::collections::HashMap<String, String>,
    ) -> Self {
        Self { cdevent, headers, trace_context: Self::capture_current_context() }
    }

    /// Captures the current trace context from the active span
    pub(crate) fn capture_current_context() -> Option<TraceContext> {
        let current_span = tracing::Span::current();
        if current_span.is_none() {
            return None;
        }

        // Extract the OpenTelemetry context from the current span
        let otel_context = current_span.context();
        let span = otel_context.span();
        let span_context = span.span_context();

        if !span_context.is_valid() {
            return None;
        }

        Some(TraceContext {
            trace_id: span_context.trace_id(),
            span_id: span_context.span_id(),
            trace_flags: span_context.trace_flags().to_u8(),
        })
    }

    /// Creates a span linked to this message's trace context
    #[allow(dead_code)] // Currently unused but kept for future use
    pub(crate) fn create_processing_span(&self) -> tracing::Span {
        if let Some(ref trace_ctx) = self.trace_context {
            tracing::info_span!(
                "message_processing",
                cdevent_id = %self.cdevent.id(),
                trace_id = %trace_ctx.trace_id,
                parent_span_id = %trace_ctx.span_id
            )
        } else {
            tracing::info_span!(
                "message_processing",
                cdevent_id = %self.cdevent.id()
            )
        }
    }
}

impl From<CDEvent> for Message {
    fn from(value: CDEvent) -> Self {
        Self {
            // received_at: OffsetDateTime::now_utc(),
            cdevent: value,
            headers: std::collections::HashMap::new(),
            trace_context: None, // CDEvent-only messages don't carry trace context
        }
    }
}

//TODO add transformers ( eg file/event info, into cdevents) for sources
//TODO integrations with cloudevents (sources & sink)
//TODO integrations with kafka / redpanda, nats,
/// retuns true if the connection service ran successfully
pub(crate) async fn connect(args: ConnectArgs) -> Result<bool> {
    let config = config::Config::from_file(args.config)?;

    let (tx, _) = broadcast::channel::<Message>(100);

    let (sinks, sink_routes) = sinks::create_sinks_and_routes(config.sinks, &tx)?;

    if sinks.is_empty() && sink_routes.is_empty() {
        tracing::error!("no sink configured or started");
        return Err(Error::NoSink).into_diagnostic();
    }

    let (sources, source_routes) =
        sources::create_sources_and_routes(config.sources, &tx, &SHUTDOWN_TOKEN)?;

    if sources.is_empty() && source_routes.is_empty() {
        tracing::error!("no source configured or started");
        return Err(Error::NoSource).into_diagnostic();
    }

    let mut routes = vec![];
    routes.extend(source_routes);
    routes.extend(sink_routes);

    let servers = vec![crate::http::launch(&config.http, routes, &SHUTDOWN_TOKEN)];

    // the channel is closed when all (sender / tx) are dropped) and then in cascade the receiver and the sinks
    // so we drop the no more useful sender to avoid a leak
    // TODO Alternative (since tokio 1.44): look at WeakSender & Sender.closed()
    drop(tx);

    // setup the handler to (try to) shutdown gracefully
    tokio::spawn(handle_shutdown_signal());

    //TODO use tokio JoinSet or TaskTracker?
    sinks
        .into_iter()
        .chain(sources)
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
