//! Core message types for cdviz-collector pipeline.
//!
//! This module provides the core messaging infrastructure used throughout the pipeline,
//! including message structure, trace context, and channel types.

use cdevents_sdk::CDEvent;
use opentelemetry::trace::{SpanId, TraceContextExt, TraceId};
use tracing_opentelemetry::OpenTelemetrySpanExt;

pub(crate) type Sender<T> = tokio::sync::broadcast::Sender<T>;
pub(crate) type Receiver<T> = tokio::sync::broadcast::Receiver<T>;

/// Simplified trace context for message correlation
#[derive(Debug, Clone)]
pub(crate) struct TraceContext {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    #[allow(dead_code)] // Reserved for future W3C trace context propagation
    pub trace_flags: u8,
}

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
