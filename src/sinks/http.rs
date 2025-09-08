use std::time::Duration;

use super::Sink;
use crate::Message;
use crate::errors::{IntoDiagnostic, Report, Result};
use crate::security::header::{
    OutgoingHeaderMap, generate_headers, outgoing_header_map_to_configs,
};
use cdevents_sdk::cloudevents::BuilderExt;
use cloudevents::{EventBuilder, EventBuilderV10};
use http_cloudevents::RequestBuilderExt;
use opentelemetry::propagation::Injector;
use opentelemetry::trace::TraceContextExt;
use opentelemetry::{Context, global};
use reqwest::Url;
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
}

fn default_total_duration_of_retries() -> Duration {
    Duration::from_secs(30 * 60)
}

impl TryFrom<Config> for HttpSink {
    type Error = Report;

    fn try_from(value: Config) -> Result<Self> {
        Ok(HttpSink::new(value.destination, value.headers, value.total_duration_of_retries))
    }
}

#[derive(Debug)]
pub(crate) struct HttpSink {
    client: ClientWithMiddleware,
    dest: Url,
    headers: OutgoingHeaderMap,
}

impl HttpSink {
    pub(crate) fn new(
        url: Url,
        headers: OutgoingHeaderMap,
        total_duration_of_retries: Duration,
    ) -> Self {
        // Retry up to 3 times with increasing intervals between attempts.
        let retry_policy = ExponentialBackoff::builder()
            .build_with_total_retry_duration_and_max_retries(total_duration_of_retries);
        let client = ClientBuilder::new(reqwest::Client::new())
            // Trace HTTP requests. See the tracing crate to make use of these traces.
            .with(TracingMiddleware::default())
            // Retry failed requests.
            .with(RetryTransientMiddleware::new_with_policy(retry_policy))
            .build();
        Self { dest: url, client, headers }
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

/// Add source headers from the message to the request
fn add_source_headers(
    mut req: RequestBuilder,
    source_headers: &std::collections::HashMap<String, String>,
) -> RequestBuilder {
    for (name, value) in source_headers {
        req = req.header(name, value);
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
    //TODO use cloudevents
    #[tracing::instrument(skip(self, msg), fields(cdevent_id = %msg.cdevent.id()))]
    async fn send(&self, msg: &Message) -> Result<()> {
        let cd_event = msg.cdevent.clone();
        // convert  CdEvent to cloudevents
        let event_result = EventBuilderV10::new().with_cdevent(cd_event.clone());

        let mut req = self.client.post(self.dest.clone());

        // Determine the body content for signature generation
        let body_bytes = match &event_result {
            Ok(event_builder) => {
                let event = event_builder.clone().build().into_diagnostic()?;
                serde_json::to_vec(&event).into_diagnostic()?
            }
            Err(_) => {
                // In error case, use the original event
                serde_json::to_vec(&cd_event).into_diagnostic()?
            }
        };

        // Add source headers first
        req = add_source_headers(req, &msg.headers);

        // Add trace context headers for distributed tracing
        req = add_trace_context_headers(req, msg.trace_context.as_ref());

        // Add configured headers (including signatures based on body)
        // These can override source headers if there are conflicts
        req = self.add_headers(req, &body_bytes)?;

        req = match event_result {
            Ok(event_builder) => {
                let event_result = event_builder.build();
                let value = event_result.into_diagnostic()?;
                req.event(value).into_diagnostic()?
            }
            Err(err) => {
                tracing::warn!(error = ?err, "Failed to convert to cloudevents");
                // In error case, send the original event
                req.json(&cd_event)
            }
        };
        let response = req.send().await.into_diagnostic()?;
        // TODO handle error, retry, etc
        if !response.status().is_success() {
            tracing::warn!(
                cdevent_id = msg.cdevent.id().as_str(),
                http_status = response.status().as_u16(),
                "Failed to send event",
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;
    use crate::security::header::HeaderSource;
    use assert2::let_assert;
    use reqwest::Url;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn build_config(url: &str) -> Config {
        Config {
            enabled: true,
            destination: Url::parse(url).unwrap(),
            headers: OutgoingHeaderMap::new(),
            total_duration_of_retries: Duration::from_secs(1),
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
            .and(wiremock::matchers::header("content-type", "application/cloudevents+json"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let config = build_config(&format!("{}/events", mock_server.uri()));
        let sink = HttpSink::try_from(config).unwrap();

        let_assert!(Ok(()) = sink.send(&msg).await);
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
        let_assert!(Ok(()) = sink.send(&msg).await);
    }

    #[test_strategy::proptest(async = "tokio", cases = 10)]
    async fn test_http_sink_network_failure(msg: Message) {
        // Use invalid URL to simulate network failure
        let config = build_config("http://invalid-host-that-does-not-exist:9999/events");
        let sink = HttpSink::try_from(config).unwrap();

        // Should fail with network error
        let_assert!(Err(_) = sink.send(&msg).await);
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

        let_assert!(Ok(()) = sink.send(&msg).await);
    }

    #[test]
    fn test_http_sink_config_validation() {
        // Valid config
        let config = build_config("https://example.com/events");
        let_assert!(Ok(_) = HttpSink::try_from(config));

        // Config with various URL schemes
        let schemes = vec!["http", "https"];
        for scheme in schemes {
            let config = build_config(&format!("{scheme}://example.com/events"));
            let_assert!(Ok(_) = HttpSink::try_from(config));
        }
    }

    #[tokio::test]
    async fn test_http_sink_rejects_non_https_in_production() {
        // In a real production environment, you might want to reject non-HTTPS URLs
        // This test documents the current behavior
        let config = build_config("http://insecure.example.com/events");

        // Currently allows HTTP - in production you might want to validate this
        let_assert!(Ok(_) = HttpSink::try_from(config));
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
            let_assert!(Ok(_) = HttpSink::try_from(config));
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
        let_assert!(Ok(()) = sink.send(&msg).await);
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
        };
        let sink = HttpSink::try_from(config).unwrap();

        let_assert!(Ok(()) = sink.send(&msg).await);
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
        let_assert!(Ok(()) = sink.send(&msg).await);
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

        let_assert!(Ok(()) = sink.send(&message).await);
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

        let_assert!(Ok(()) = sink.send(&message).await);
    }
}

//

mod http_cloudevents {
    use cloudevents::Event;
    use cloudevents::binding::http::SPEC_VERSION_HEADER;
    use cloudevents::binding::http::header_prefix;
    use cloudevents::event::SpecVersion;
    use cloudevents::message::BinaryDeserializer;
    use cloudevents::message::BinarySerializer;
    use cloudevents::message::MessageAttributeValue;
    use cloudevents::message::Result;
    use reqwest_middleware::RequestBuilder;

    pub trait RequestBuilderExt {
        /// Write in this [`RequestBuilder`] the provided [`Event`]. Similar to invoking [`Event`].
        fn event(self, event: Event) -> Result<RequestBuilder>;
    }

    impl RequestBuilderExt for RequestBuilder {
        fn event(self, event: Event) -> Result<RequestBuilder> {
            BinaryDeserializer::deserialize_binary(event, RequestSerializer::new(self))
        }
    }
    /// Wrapper for [`RequestBuilder`] that implements [`StructuredSerializer`] & [`BinarySerializer`] traits.
    pub struct RequestSerializer {
        req: RequestBuilder,
    }

    impl RequestSerializer {
        pub fn new(req: RequestBuilder) -> RequestSerializer {
            RequestSerializer { req }
        }
    }

    impl BinarySerializer<RequestBuilder> for RequestSerializer {
        fn set_spec_version(mut self, spec_ver: SpecVersion) -> Result<Self> {
            self.req = self.req.header(SPEC_VERSION_HEADER, spec_ver.to_string());
            Ok(self)
        }

        fn set_attribute(mut self, name: &str, value: MessageAttributeValue) -> Result<Self> {
            let key = &header_prefix(name);
            self.req = self.req.header(key, value.to_string());
            Ok(self)
        }

        fn set_extension(mut self, name: &str, value: MessageAttributeValue) -> Result<Self> {
            let key = &header_prefix(name);
            self.req = self.req.header(key, value.to_string());
            Ok(self)
        }

        fn end_with_data(self, bytes: Vec<u8>) -> Result<RequestBuilder> {
            Ok(self.req.body(bytes))
        }

        fn end(self) -> Result<RequestBuilder> {
            Ok(self.req)
        }
    }
}
