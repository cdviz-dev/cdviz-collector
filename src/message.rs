//! Core message types for cdviz-collector pipeline.
//!
//! This module provides the core messaging infrastructure used throughout the pipeline,
//! including message structure, trace context, and channel types.

use cdevents_sdk::CDEvent;
use init_tracing_opentelemetry::opentelemetry::trace::TraceContextExt;
use init_tracing_opentelemetry::tracing_opentelemetry::OpenTelemetrySpanExt;

pub(crate) type Sender<T> = tokio::sync::broadcast::Sender<T>;
pub(crate) type Receiver<T> = tokio::sync::broadcast::Receiver<T>;

#[derive(Clone, Debug)]
pub(crate) struct Message {
    // received_at: OffsetDateTime,
    pub(crate) cdevent: CDEvent,
    /// Message headers; also carries the W3C `traceparent` injected at the queue boundary
    /// (see `crate::otel`) so sinks can both link spans and forward trace context downstream.
    pub(crate) headers: std::collections::HashMap<String, String>,
    //raw: serde_json::Value,
}

impl Message {
    pub(crate) fn new(
        cdevent: CDEvent,
        headers: std::collections::HashMap<String, String>,
    ) -> Self {
        Self { cdevent, headers }
    }

    /// Builds a sink-processing span parented to the source trace context carried in the
    /// message headers (W3C `traceparent`), so the sink span shares the source's `trace_id`
    /// across the broadcast queue boundary.
    pub(crate) fn processing_span(&self, sink_name: &str) -> tracing::Span {
        let span = tracing::info_span!("sink", name = %sink_name, cdevent_id = %self.cdevent.id());
        let parent_cx = crate::otel::extract_context(&self.headers);
        if parent_cx.span().span_context().is_valid()
            && let Err(err) = span.set_parent(parent_cx)
        {
            tracing::debug!(?err, "failed to link sink span to source trace context");
        }
        span
    }
}

impl From<CDEvent> for Message {
    fn from(value: CDEvent) -> Self {
        Self {
            // received_at: OffsetDateTime::now_utc(),
            cdevent: value,
            headers: std::collections::HashMap::new(),
        }
    }
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
