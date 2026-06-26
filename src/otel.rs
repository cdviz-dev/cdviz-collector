//! W3C trace-context propagation over `HashMap<String, String>` "headers".
//!
//! Used to carry the trace context across boundaries that the implicit `tracing` parent
//! cannot cross — notably the in-memory broadcast queue (source task → sink task), the
//! same way Kafka/HTTP boundaries propagate via a `traceparent` header. Injection writes
//! the current span's context into the message headers; extraction rebuilds it on the
//! other side so the sink span can be parented to the source trace.

use init_tracing_opentelemetry::opentelemetry::Context;
use init_tracing_opentelemetry::opentelemetry::global;
use init_tracing_opentelemetry::opentelemetry::propagation::{Extractor, Injector};
use init_tracing_opentelemetry::tracing_opentelemetry::OpenTelemetrySpanExt;
use std::collections::HashMap;

struct HeaderInjector<'a>(&'a mut HashMap<String, String>);

impl Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        self.0.insert(key.to_string(), value);
    }
}

struct HeaderExtractor<'a>(&'a HashMap<String, String>);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
}

/// Inject the current span's trace context into `headers` (writes `traceparent`, etc).
pub(crate) fn inject_current_context(headers: &mut HashMap<String, String>) {
    let cx = tracing::Span::current().context();
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&cx, &mut HeaderInjector(headers));
    });
}

/// Rebuild a trace context from `headers` (reads `traceparent`, etc).
pub(crate) fn extract_context(headers: &HashMap<String, String>) -> Context {
    global::get_text_map_propagator(|propagator| propagator.extract(&HeaderExtractor(headers)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use init_tracing_opentelemetry::opentelemetry::trace::{
        SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState,
    };
    use init_tracing_opentelemetry::opentelemetry_sdk::propagation::TraceContextPropagator;

    #[test]
    fn inject_extract_roundtrip_preserves_trace_id() {
        global::set_text_map_propagator(TraceContextPropagator::new());

        let sc = SpanContext::new(
            TraceId::from_bytes(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef_u128.to_be_bytes()),
            SpanId::from_bytes(0x0011_2233_4455_6677_u64.to_be_bytes()),
            TraceFlags::SAMPLED,
            true,
            TraceState::NONE,
        );
        let cx = Context::new().with_remote_span_context(sc.clone());

        let mut headers = HashMap::new();
        global::get_text_map_propagator(|p| {
            p.inject_context(&cx, &mut HeaderInjector(&mut headers));
        });
        assert!(headers.contains_key("traceparent"), "traceparent must be written: {headers:?}");

        let extracted = extract_context(&headers);
        assert_eq!(extracted.span().span_context().trace_id(), sc.trace_id());
        assert!(extracted.span().span_context().is_sampled());
    }
}
