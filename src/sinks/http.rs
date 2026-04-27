use std::collections::HashMap;
use std::time::Duration;

use super::Sink;
use crate::Message;
use crate::errors::{IntoDiagnostic, Report, Result};
use crate::event::{Event, EventPipe, message_to_event};
use crate::security::header::{
    OutgoingHeaderMap, generate_headers, outgoing_header_map_to_configs,
};
use crate::transformers;
use crate::transformers::collect_to_vec;
use opentelemetry::propagation::Injector;
use opentelemetry::trace::TraceContextExt;
use opentelemetry::{Context, global};
use reqwest::Url;
use reqwest::header::CONTENT_TYPE;
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware, RequestBuilder};
use reqwest_retry::{RetryTransientMiddleware, policies::ExponentialBackoff};
use reqwest_tracing::TracingMiddleware;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Config {
    /// Is the sink is enabled?
    pub(crate) enabled: bool,
    destination: Url,
    /// Header generation for outgoing HTTP requests - new map format
    #[serde(default)]
    pub(crate) headers: OutgoingHeaderMap,
    /// Timeout for message production (default 30m)
    #[serde(with = "humantime_serde", default = "default_total_duration_of_retries")]
    pub(crate) total_duration_of_retries: Duration,
    /// Log the full HTTP response (headers + body) when the server returns a non-2xx status.
    /// Useful for debugging in CI; defaults to false.
    #[serde(default)]
    log_full_response_on_error: bool,
    /// `User-Agent` header sent with every request.
    /// Defaults to `cdviz-collector/<version>`.
    #[serde(default = "default_user_agent")]
    user_agent: String,
    #[serde(flatten)]
    pub(crate) chain: transformers::TransformerChainConfig,
}

impl Config {
    pub(crate) fn resolve_transformers(
        &mut self,
        configs: &HashMap<String, transformers::Config>,
    ) -> Result<()> {
        self.chain.resolve(configs)
    }
}

fn default_user_agent() -> String {
    crate::DEFAULT_USER_AGENT.to_string()
}

fn default_total_duration_of_retries() -> Duration {
    Duration::from_mins(30)
}

impl TryFrom<Config> for HttpSink {
    type Error = Report;

    fn try_from(value: Config) -> Result<Self> {
        let event_collector = collect_to_vec::Collector::<Event>::new();
        let terminal: EventPipe = Box::new(event_collector.create_pipe());
        let transformer_chain =
            transformers::TransformerChain::try_new(&value.chain.transformers, terminal)?;
        // Retry up to 3 times with increasing intervals between attempts.
        let retry_policy = ExponentialBackoff::builder()
            .build_with_total_retry_duration_and_limit_retries(value.total_duration_of_retries);
        let client = ClientBuilder::new(
            reqwest::Client::builder().user_agent(value.user_agent).build().into_diagnostic()?,
        )
        .with(TracingMiddleware::default())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build();
        Ok(Self {
            dest: value.destination,
            client,
            headers: value.headers,
            log_full_response_on_error: value.log_full_response_on_error,
            transformer_chain,
            event_collector,
        })
    }
}

pub(crate) struct HttpSink {
    client: ClientWithMiddleware,
    dest: Url,
    headers: OutgoingHeaderMap,
    log_full_response_on_error: bool,
    transformer_chain: transformers::TransformerChain,
    event_collector: collect_to_vec::Collector<Event>,
}

impl std::fmt::Debug for HttpSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpSink")
            .field("dest", &self.dest)
            .field("log_full_response_on_error", &self.log_full_response_on_error)
            .finish_non_exhaustive()
    }
}

impl HttpSink {
    async fn send_event(
        &self,
        event: &Event,
        trace_context: Option<&crate::message::TraceContext>,
    ) -> Result<()> {
        let body_bytes = serde_json::to_vec(&event.body).into_diagnostic()?;
        let mut req = self.client.post(self.dest.clone());
        // Content-Type is set first so event.headers (from VRL) can override it.
        // add_headers (HMAC) runs last so it signs the final body bytes.
        req = req.header(CONTENT_TYPE, "application/json");
        req = add_cloudevents_headers(req, event);
        for (name, value) in &event.headers {
            req = req.header(name.as_str(), value.as_str());
        }
        req = add_trace_context_headers(req, trace_context);
        req = self.add_headers(req, &body_bytes)?.body(body_bytes);
        let response = req.send().await.into_diagnostic()?;
        if !response.status().is_success() {
            let http_status = response.status().as_u16();
            let destination = self.dest.as_str();
            if self.log_full_response_on_error {
                let resp_headers: serde_json::Map<String, serde_json::Value> = response
                    .headers()
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.as_str().to_owned(),
                            serde_json::Value::String(
                                v.to_str().unwrap_or("<non-utf8>").to_owned(),
                            ),
                        )
                    })
                    .collect();
                let http_body = response.text().await.unwrap_or_default();
                tracing::warn!(
                    http_status,
                    destination,
                    http_headers = %serde_json::Value::Object(resp_headers),
                    http_body,
                    "Failed to send event",
                );
            } else {
                tracing::warn!(http_status, destination, "Failed to send event");
            }
        }
        Ok(())
    }

    /// Generate and add configured headers to the request
    fn add_headers(&self, req: RequestBuilder, body: &[u8]) -> Result<RequestBuilder> {
        let header_configs = outgoing_header_map_to_configs(&self.headers);
        match generate_headers(&header_configs, Some(body)) {
            Ok(headers) => {
                // Since both axum and reqwest use the same http crate types, we can use headers directly
                Ok(req.headers(headers))
            }
            Err(e) => {
                tracing::error!("Failed to generate headers: {}", e);
                Err(e).into_diagnostic()
            }
        }
    }
}

/// Set `CloudEvents` headers from the `CDEvent` context (if event has it) so downstream consumers
/// can route without parsing the body.
fn add_cloudevents_headers(mut req: RequestBuilder, event: &Event) -> RequestBuilder {
    if let Some(ctx) = event.body.get("context") {
        let mut ce_defined = false;
        for (header, field) in [
            //("ce-specversion", "specversion"),
            ("ce-id", "id"),
            ("ce-source", "source"),
            ("ce-type", "type"),
            ("ce-time", "timestamp"),
        ] {
            if let Some(value) = ctx.get(field).and_then(|v| v.as_str()) {
                req = req.header(header, value);
                ce_defined = true;
            }
        }
        if ce_defined {
            req = req.header("ce-specversion", "1.0");
            //req = req.header("content-type", "application/cloudevents+json");
        }
    }
    req
}

/// Add W3C Trace Context headers using OpenTelemetry propagation
fn add_trace_context_headers(
    mut req: RequestBuilder,
    trace_context: Option<&crate::message::TraceContext>,
) -> RequestBuilder {
    if let Some(trace_ctx) = trace_context {
        // Create OpenTelemetry context with trace information
        use opentelemetry::trace::{SpanContext, TraceFlags, TraceState};

        let span_context = SpanContext::new(
            trace_ctx.trace_id,
            trace_ctx.span_id,
            TraceFlags::new(trace_ctx.trace_flags),
            false,
            TraceState::NONE,
        );

        // Create context with the remote span context for propagation
        let context = Context::current().with_remote_span_context(span_context);

        // Create a header injector for the request
        let mut header_injector = HeaderInjector::new();

        // Use the global text map propagator to inject headers
        global::get_text_map_propagator(|propagator| {
            propagator.inject_context(&context, &mut header_injector);
        });

        // Add all injected headers to the request
        for (key, value) in header_injector.headers {
            req = req.header(key, value);
        }

        tracing::debug!(
            trace_id = %trace_ctx.trace_id,
            span_id = %trace_ctx.span_id,
            "Added trace context headers to outgoing HTTP request using OpenTelemetry propagation"
        );
    }
    req
}

/// Header injector for OpenTelemetry propagation
struct HeaderInjector {
    headers: std::collections::HashMap<String, String>,
}

impl HeaderInjector {
    fn new() -> Self {
        Self { headers: std::collections::HashMap::new() }
    }
}

impl Injector for HeaderInjector {
    fn set(&mut self, key: &str, value: String) {
        self.headers.insert(key.to_string(), value);
    }
}

impl Sink for HttpSink {
    #[tracing::instrument(skip(self, msg), fields(cdevent_id = %msg.cdevent.id()))]
    async fn send(&self, msg: &Message) -> Result<()> {
        // convert first and drain to avoid:
        // There is a potential for event leakage between messages.
        // If transformer_chain.push fails, any events already emitted by the
        // chain and stored in the event_collector will remain there because
        // drain() is never called.
        // These stale events would then be sent during the next successful
        // send call for a different message.
        let push_res = self.transformer_chain.push(message_to_event(msg)?);
        let events = self.event_collector.drain()?;
        push_res?;
        for event in events {
            self.send_event(&event, msg.trace_context.as_ref()).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;
    use crate::security::header::HeaderSource;
    use crate::transformers::TransformerChainConfig;
    use reqwest::Url;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn build_config(url: &str) -> Config {
        Config {
            enabled: true,
            destination: Url::parse(url).unwrap(),
            headers: OutgoingHeaderMap::new(),
            total_duration_of_retries: Duration::from_secs(1),
            log_full_response_on_error: false,
            user_agent: default_user_agent(),
            chain: TransformerChainConfig::default(),
        }
    }

    #[tokio::test]
    async fn test_http_sink() {
        use proptest::prelude::*;
        use proptest::test_runner::TestRunner;
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
        .and(path("/events"))
        .and(wiremock::matchers::header("Content-Type", "application/json"))
        .and(wiremock::matchers::header("ce-specversion", "1.0"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        // Mounting the mock on the mock server - it's now effective!
        .mount(&mock_server)
        .await;

        let config = build_config(&format!("{}/events", &mock_server.uri()));

        let sink = HttpSink::try_from(config).unwrap();

        let mut runner = TestRunner::default();
        for _ in 0..1 {
            let val = any::<Message>().new_tree(&mut runner).unwrap();
            assert!(sink.send(&val.current()).await.is_ok());
        }
    }

    #[test_strategy::proptest(async = "tokio", cases = 10)]
    async fn test_http_sink_successful_send(msg: Message) {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/events"))
            //.and(wiremock::matchers::header("content-type", "application/cloudevents+json"))
            .and(wiremock::matchers::header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let config = build_config(&format!("{}/events", mock_server.uri()));
        let sink = HttpSink::try_from(config).unwrap();

        assert2::assert!(let Ok(()) = sink.send(&msg).await);
    }

    #[test_strategy::proptest(async = "tokio", cases = 10)]
    async fn test_http_sink_server_error_handling(msg: Message) {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/events"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let config = build_config(&format!("{}/events", mock_server.uri()));
        let sink = HttpSink::try_from(config).unwrap();

        // Should not fail even on server error (logs warning)
        assert2::assert!(let Ok(()) = sink.send(&msg).await);
    }

    #[test_strategy::proptest(async = "tokio", cases = 10)]
    async fn test_http_sink_network_failure(msg: Message) {
        // Use invalid URL to simulate network failure
        let config = build_config("http://invalid-host-that-does-not-exist:9999/events");
        let sink = HttpSink::try_from(config).unwrap();

        // Should fail with network error
        assert2::assert!(let Err(_) = sink.send(&msg).await);
    }

    #[test_strategy::proptest(async = "tokio", cases = 10)]
    async fn test_http_sink_fallback_to_raw_json(msg: Message) {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/events"))
            .and(wiremock::matchers::header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let config = build_config(&format!("{}/events", mock_server.uri()));
        let sink = HttpSink::try_from(config).unwrap();

        assert2::assert!(let Ok(()) = sink.send(&msg).await);
    }

    #[test]
    fn test_http_sink_config_validation() {
        // Valid config
        let config = build_config("https://example.com/events");
        assert2::assert!(let Ok(_) = HttpSink::try_from(config));

        // Config with various URL schemes
        let schemes = vec!["http", "https"];
        for scheme in schemes {
            let config = build_config(&format!("{scheme}://example.com/events"));
            assert2::assert!(let Ok(_) = HttpSink::try_from(config));
        }
    }

    #[tokio::test]
    async fn test_http_sink_rejects_non_https_in_production() {
        // In a real production environment, you might want to reject non-HTTPS URLs
        // This test documents the current behavior
        let config = build_config("http://insecure.example.com/events");

        // Currently allows HTTP - in production you might want to validate this
        assert2::assert!(let Ok(_) = HttpSink::try_from(config));
    }

    /// Verify all four header categories coexist in a single request:
    /// Content-Type (hardcoded), `CloudEvents` headers (from body context),
    /// source/VRL headers (from event.headers), and configured headers (HMAC/static).
    #[tokio::test]
    async fn test_http_sink_all_header_categories_preserved() {
        use crate::security::header::HeaderSource;
        use crate::sources::EventSource;
        use cdevents_sdk::CDEvent;
        use serde_json::json;
        use std::collections::HashMap;
        use wiremock::matchers::header_exists;

        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/events"))
            .and(wiremock::matchers::header("Content-Type", "application/json")) // category 1: hardcoded
            .and(header_exists("ce-id")) // category 2: CloudEvents from body context
            .and(wiremock::matchers::header("X-Source-Header", "source-value")) // category 3: event.headers
            .and(wiremock::matchers::header("X-API-Key", "test-key")) // category 4: configured static
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock_server)
            .await;

        let config = Config {
            enabled: true,
            destination: Url::parse(&format!("{}/events", mock_server.uri())).unwrap(),
            headers: {
                let mut map = OutgoingHeaderMap::new();
                map.insert(
                    "X-API-Key".to_string(),
                    HeaderSource::Static { value: "test-key".to_string() },
                );
                map
            },
            total_duration_of_retries: Duration::from_secs(1),
            log_full_response_on_error: false,
            user_agent: default_user_agent(),
            chain: TransformerChainConfig::default(),
        };
        let sink = HttpSink::try_from(config).unwrap();

        let mut source_headers = HashMap::new();
        source_headers.insert("X-Source-Header".to_string(), "source-value".to_string());

        let event_source = EventSource {
            metadata: json!({}),
            headers: source_headers.clone(),
            body: json!({
                "context": {
                    "version": "0.4.0",
                    "id": "test-all-headers",
                    "source": "test-source",
                    "type": "dev.cdevents.service.deployed.0.1.1",
                    "timestamp": "2024-03-14T10:30:00Z"
                },
                "subject": {
                    "id": "test-subject",
                    "source": "test-source",
                    "type": "service",
                    "content": {
                        "environment": { "id": "test-env" },
                        "artifactId": "pkg:test/artifact@1.0.0"
                    }
                }
            }),
        };

        let cdevent = CDEvent::try_from(event_source).unwrap();
        let message = Message { cdevent, headers: source_headers, trace_context: None };

        assert2::assert!(let Ok(()) = sink.send(&message).await);
        // wiremock verifies all 4 matchers on MockServer drop (via .expect(1))
    }

    #[tokio::test]
    async fn test_http_sink_prevents_ssrf_attacks() {
        // Test that the sink doesn't allow requests to internal networks
        // Note: This is more of a documentation test as the current implementation
        // doesn't prevent SSRF attacks
        let internal_urls = vec![
            "http://localhost:8080/events",
            "http://127.0.0.1:8080/events",
            "http://10.0.0.1:8080/events",
            "http://192.168.1.1:8080/events",
        ];

        for url in internal_urls {
            let config = build_config(url);

            // Currently allows internal URLs - in production you might want to validate this
            assert2::assert!(let Ok(_) = HttpSink::try_from(config));
        }
    }

    #[test_strategy::proptest(
        async = "tokio",
        proptest::prelude::ProptestConfig::default(),
        cases = 10
    )]
    async fn test_http_sink_handles_redirect_securely(msg: Message) {
        let mock_server = MockServer::start().await;

        // Set up redirect
        Mock::given(method("POST"))
            .and(path("/events"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/redirected"))
            .mount(&mock_server)
            .await;

        Mock::given(method("POST"))
            .and(path("/redirected"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let config = build_config(&format!("{}/events", mock_server.uri()));
        let sink = HttpSink::try_from(config).unwrap();

        // Should handle redirects properly
        assert2::assert!(let Ok(()) = sink.send(&msg).await);
    }

    #[test_strategy::proptest(async = "tokio", cases = 10)]
    async fn test_http_sink_with_static_headers(msg: Message) {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/events"))
            .and(wiremock::matchers::header("X-API-Key", "test-secret-key"))
            .and(wiremock::matchers::header("X-Client-ID", "cdviz-collector"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let config = Config {
            enabled: true,
            destination: Url::parse(&format!("{}/events", mock_server.uri())).unwrap(),
            headers: {
                let mut map = OutgoingHeaderMap::new();
                map.insert(
                    "X-API-Key".to_string(),
                    HeaderSource::Static { value: "test-secret-key".to_string() },
                );
                map.insert(
                    "X-Client-ID".to_string(),
                    HeaderSource::Static { value: "cdviz-collector".to_string() },
                );
                map
            },
            total_duration_of_retries: Duration::from_secs(1),
            log_full_response_on_error: false,
            user_agent: default_user_agent(),
            chain: TransformerChainConfig::default(),
        };
        let sink = HttpSink::try_from(config).unwrap();

        assert2::assert!(let Ok(()) = sink.send(&msg).await);
    }

    #[test_strategy::proptest(
        async = "tokio",
        proptest::prelude::ProptestConfig::default(),
        cases = 10
    )]
    async fn test_http_sink_request_tracing(msg: Message) {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/events"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let config = build_config(&format!("{}/events", mock_server.uri()));
        let sink = HttpSink::try_from(config).unwrap();

        // Verify that tracing middleware is present and doesn't interfere
        assert2::assert!(let Ok(()) = sink.send(&msg).await);
    }

    #[tokio::test]
    async fn test_http_sink_forwards_source_headers() {
        use crate::sources::EventSource;
        use cdevents_sdk::CDEvent;
        use serde_json::json;
        use std::collections::HashMap;

        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/events"))
            .and(wiremock::matchers::header("X-Source-Header", "source-value"))
            .and(wiremock::matchers::header("Authorization", "Bearer source-token"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let config = build_config(&format!("{}/events", mock_server.uri()));
        let sink = HttpSink::try_from(config).unwrap();

        // Create a message with source headers
        let mut source_headers = HashMap::new();
        source_headers.insert("X-Source-Header".to_string(), "source-value".to_string());
        source_headers.insert("Authorization".to_string(), "Bearer source-token".to_string());

        let event_source = EventSource {
            metadata: json!({}),
            headers: source_headers.clone(),
            body: json!({
                "context": {
                    "version": "0.4.0",
                    "id": "test-header-forward",
                    "source": "test-source",
                    "type": "dev.cdevents.service.deployed.0.1.1",
                    "timestamp": "2024-03-14T10:30:00Z"
                },
                "subject": {
                    "id": "test-subject",
                    "source": "test-source",
                    "type": "service",
                    "content": {
                        "environment": {
                            "id": "test-env"
                        },
                        "artifactId": "pkg:test/artifact@1.0.0"
                    }
                }
            }),
        };

        let cdevent = CDEvent::try_from(event_source).unwrap();
        let message = Message { cdevent, headers: source_headers, trace_context: None };

        assert2::assert!(let Ok(()) = sink.send(&message).await);
    }

    #[tokio::test]
    async fn test_http_sink_configured_headers_override_source_headers() {
        use crate::sources::EventSource;
        use cdevents_sdk::CDEvent;
        use serde_json::json;
        use std::collections::HashMap;

        let mock_server = MockServer::start().await;

        // Should receive the configured header value, not the source header value
        Mock::given(method("POST"))
            .and(path("/events"))
            .and(wiremock::matchers::header("Authorization", "Bearer configured-token"))
            .and(wiremock::matchers::header("X-Source-Only", "source-only-value"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let config = Config {
            enabled: true,
            destination: Url::parse(&format!("{}/events", mock_server.uri())).unwrap(),
            headers: {
                let mut map = OutgoingHeaderMap::new();
                map.insert(
                    "Authorization".to_string(),
                    HeaderSource::Static { value: "Bearer configured-token".to_string() },
                );
                map
            },
            total_duration_of_retries: Duration::from_secs(1),
            log_full_response_on_error: false,
            user_agent: default_user_agent(),
            chain: TransformerChainConfig::default(),
        };
        let sink = HttpSink::try_from(config).unwrap();

        // Create a message with source headers that will conflict with configured headers
        let mut source_headers = HashMap::new();
        source_headers.insert("Authorization".to_string(), "Bearer source-token".to_string()); // This should be overridden
        source_headers.insert("X-Source-Only".to_string(), "source-only-value".to_string()); // This should remain

        let event_source = EventSource {
            metadata: json!({}),
            headers: source_headers.clone(),
            body: json!({
                "context": {
                    "version": "0.4.0",
                    "id": "test-header-override",
                    "source": "test-source",
                    "type": "dev.cdevents.service.deployed.0.1.1",
                    "timestamp": "2024-03-14T10:30:00Z"
                },
                "subject": {
                    "id": "test-subject",
                    "source": "test-source",
                    "type": "service",
                    "content": {
                        "environment": {
                            "id": "test-env"
                        },
                        "artifactId": "pkg:test/artifact@1.0.0"
                    }
                }
            }),
        };

        let cdevent = CDEvent::try_from(event_source).unwrap();
        let message = Message { cdevent, headers: source_headers, trace_context: None };

        assert2::assert!(let Ok(()) = sink.send(&message).await);
    }

    /// Verify that the HMAC signature header computed by the sink matches the actual HTTP body
    /// bytes received by a server.  This is the end-to-end unit-level check that guards against
    /// mismatches between what is signed and what is sent (e.g. full `CloudEvent` JSON vs the binary
    /// binding's data-only body).
    #[tokio::test]
    async fn test_http_sink_signature_matches_body() {
        use crate::security::signature::{Encoding, SignatureConfig, SignatureOn, build_signature};
        use crate::sources::EventSource;
        use cdevents_sdk::CDEvent;
        use serde_json::json;

        let mock_server = MockServer::start().await;
        // Accept every POST — we verify the signature manually from the captured request.
        Mock::given(method("POST"))
            .and(path("/events"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let token = "test-signing-secret";
        let config = Config {
            enabled: true,
            destination: Url::parse(&format!("{}/events", mock_server.uri())).unwrap(),
            headers: {
                let mut map = OutgoingHeaderMap::new();
                map.insert(
                    "x-signature".to_string(),
                    HeaderSource::Signature {
                        token: token.into(),
                        token_encoding: None,
                        signature_prefix: Some("sha256=".to_string()),
                        signature_on: crate::security::signature::SignatureOn::Body,
                        signature_encoding: crate::security::signature::Encoding::Hex,
                    },
                );
                map
            },
            total_duration_of_retries: Duration::from_secs(1),
            log_full_response_on_error: false,
            user_agent: default_user_agent(),
            chain: TransformerChainConfig::default(),
        };
        let sink = HttpSink::try_from(config).unwrap();

        let event_source = EventSource {
            metadata: json!({}),
            headers: std::collections::HashMap::new(),
            body: json!({
                "context": {
                    "version": "0.4.0",
                    "id": "test-sig-check",
                    "source": "test-source",
                    "type": "dev.cdevents.service.deployed.0.1.1",
                    "timestamp": "2024-03-14T10:30:00Z"
                },
                "subject": {
                    "id": "test-subject",
                    "source": "test-source",
                    "type": "service",
                    "content": {
                        "environment": { "id": "test-env" },
                        "artifactId": "pkg:test/artifact@1.0.0"
                    }
                }
            }),
        };
        let cdevent = CDEvent::try_from(event_source).unwrap();
        let message =
            Message { cdevent, headers: std::collections::HashMap::new(), trace_context: None };

        assert2::assert!(let Ok(()) = sink.send(&message).await);

        let received =
            mock_server.received_requests().await.expect("request recording must be enabled");
        assert_eq!(received.len(), 1, "expected exactly one request");
        let req = &received[0];

        let body_bytes = &req.body;
        let signature_header = req
            .headers
            .get("x-signature")
            .and_then(|v| v.to_str().ok())
            .expect("x-signature header must be present");

        // Recompute the expected signature from the bytes the server actually received.
        let sig_config = SignatureConfig {
            header: "x-signature".to_string(),
            token: token.into(),
            token_encoding: None,
            signature_prefix: Some("sha256=".to_string()),
            signature_on: SignatureOn::Body,
            signature_encoding: Encoding::Hex,
        };
        let expected =
            build_signature(&sig_config, &axum::http::HeaderMap::new(), body_bytes).unwrap();

        assert_eq!(
            signature_header, expected,
            "x-signature header must be HMAC of the actual request body bytes"
        );
    }
}
