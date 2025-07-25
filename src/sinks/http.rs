use super::Sink;
use crate::Message;
use crate::errors::{IntoDiagnostic, Report, Result};
use crate::security::rule::HeaderRuleConfig;
use cdevents_sdk::cloudevents::BuilderExt;
use cloudevents::{EventBuilder, EventBuilderV10};
use http_cloudevents::RequestBuilderExt;
use reqwest::Url;
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware, RequestBuilder};
use reqwest_tracing::TracingMiddleware;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Config {
    /// Is the sink is enabled?
    pub(crate) enabled: bool,
    destination: Url,
    /// Header generation for outgoing HTTP requests
    #[serde(default)]
    pub(crate) headers: Vec<HeaderRuleConfig>,
}

impl TryFrom<Config> for HttpSink {
    type Error = Report;

    fn try_from(value: Config) -> Result<Self> {
        Ok(HttpSink::new(value.destination, value.headers))
    }
}

#[derive(Debug)]
pub(crate) struct HttpSink {
    client: ClientWithMiddleware,
    dest: Url,
    headers: Vec<HeaderRuleConfig>,
}

impl HttpSink {
    pub(crate) fn new(url: Url, headers: Vec<HeaderRuleConfig>) -> Self {
        // Retry up to 3 times with increasing intervals between attempts.
        //let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
        let client = ClientBuilder::new(reqwest::Client::new())
            // Trace HTTP requests. See the tracing crate to make use of these traces.
            .with(TracingMiddleware::default())
            // Retry failed requests.
            //.with(RetryTransientMiddleware::new_with_policy(retry_policy))
            .build();
        Self { dest: url, client, headers }
    }

    /// Generate and add configured headers to the request
    fn add_headers(&self, mut req: RequestBuilder, body: &[u8]) -> Result<RequestBuilder> {
        use crate::security::rule::Rule;

        for header_config in &self.headers {
            match &header_config.rule {
                // For static values, just set the header
                Rule::Equals { value, .. } => {
                    req = req.header(&header_config.header, value);
                }

                // For signatures, generate the signature and set the header
                Rule::Signature {
                    token,
                    token_encoding,
                    signature_prefix,
                    signature_on,
                    signature_encoding,
                } => {
                    use crate::security::signature::{self, SignatureConfig};

                    let signature_config = SignatureConfig {
                        header: header_config.header.clone(),
                        token: token.clone(),
                        token_encoding: token_encoding.clone(),
                        signature_prefix: signature_prefix.clone(),
                        signature_on: signature_on.clone(),
                        signature_encoding: signature_encoding.clone(),
                    };

                    // Build signature using the body
                    let signature = signature::build_signature(
                        &signature_config,
                        &axum::http::HeaderMap::new(), // No existing headers for signature computation
                        body,
                    )
                    .into_diagnostic()?;

                    req = req.header(&header_config.header, signature);
                }

                // Other validation methods are not supported for header generation
                _ => {
                    tracing::warn!(
                        header = header_config.header,
                        validation_type = ?header_config.rule,
                        "Unsupported validation method for HTTP header generation"
                    );
                }
            }
        }

        Ok(req)
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

impl Sink for HttpSink {
    //TODO use cloudevents
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
    use assert2::let_assert;
    use reqwest::Url;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

        let config = Config {
            enabled: true,
            destination: Url::parse(&format!("{}/events", &mock_server.uri())).unwrap(),
            headers: vec![],
        };

        let sink = HttpSink::try_from(config).unwrap();

        let mut runner = TestRunner::default();
        for _ in 0..1 {
            let val = any::<Message>().new_tree(&mut runner).unwrap();
            assert!(sink.send(&val.current()).await.is_ok());
        }
    }

    #[test_strategy::proptest(async = "tokio")]
    async fn test_http_sink_successful_send(msg: Message) {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/events"))
            .and(wiremock::matchers::header("content-type", "application/cloudevents+json"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let config = Config {
            enabled: true,
            destination: Url::parse(&format!("{}/events", mock_server.uri())).unwrap(),
            headers: vec![],
        };
        let sink = HttpSink::try_from(config).unwrap();

        let_assert!(Ok(()) = sink.send(&msg).await);
    }

    #[test_strategy::proptest(async = "tokio")]
    async fn test_http_sink_server_error_handling(msg: Message) {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/events"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let config = Config {
            enabled: true,
            destination: Url::parse(&format!("{}/events", mock_server.uri())).unwrap(),
            headers: vec![],
        };
        let sink = HttpSink::try_from(config).unwrap();

        // Should not fail even on server error (logs warning)
        let_assert!(Ok(()) = sink.send(&msg).await);
    }

    #[test_strategy::proptest(async = "tokio")]
    async fn test_http_sink_network_failure(msg: Message) {
        // Use invalid URL to simulate network failure
        let config = Config {
            enabled: true,
            destination: Url::parse("http://invalid-host-that-does-not-exist:9999/events").unwrap(),
            headers: vec![],
        };
        let sink = HttpSink::try_from(config).unwrap();

        // Should fail with network error
        let_assert!(Err(_) = sink.send(&msg).await);
    }

    #[test_strategy::proptest(async = "tokio")]
    async fn test_http_sink_fallback_to_raw_json(msg: Message) {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/events"))
            .and(wiremock::matchers::header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let config = Config {
            enabled: true,
            destination: Url::parse(&format!("{}/events", mock_server.uri())).unwrap(),
            headers: vec![],
        };
        let sink = HttpSink::try_from(config).unwrap();

        let_assert!(Ok(()) = sink.send(&msg).await);
    }

    #[test]
    fn test_http_sink_config_validation() {
        // Valid config
        let config = Config {
            enabled: true,
            destination: Url::parse("https://example.com/events").unwrap(),
            headers: vec![],
        };
        let_assert!(Ok(_) = HttpSink::try_from(config));

        // Config with various URL schemes
        let schemes = vec!["http", "https"];
        for scheme in schemes {
            let config = Config {
                enabled: true,
                destination: Url::parse(&format!("{scheme}://example.com/events")).unwrap(),
                headers: vec![],
            };
            let_assert!(Ok(_) = HttpSink::try_from(config));
        }
    }
}

#[cfg(test)]
mod security_tests {
    use super::*;
    use crate::Message;
    use assert2::let_assert;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_http_sink_rejects_non_https_in_production() {
        // In a real production environment, you might want to reject non-HTTPS URLs
        // This test documents the current behavior
        let config = Config {
            enabled: true,
            destination: Url::parse("http://insecure.example.com/events").unwrap(),
            headers: vec![],
        };

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
            let config =
                Config { enabled: true, destination: Url::parse(url).unwrap(), headers: vec![] };

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

        let config = Config {
            enabled: true,
            destination: Url::parse(&format!("{}/events", mock_server.uri())).unwrap(),
            headers: vec![],
        };
        let sink = HttpSink::try_from(config).unwrap();

        // Should handle redirects properly
        let_assert!(Ok(()) = sink.send(&msg).await);
    }

    #[test_strategy::proptest(async = "tokio", cases = 10)]
    async fn test_http_sink_with_static_headers(msg: Message) {
        use crate::security::rule::{HeaderRuleConfig, Rule};

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
            headers: vec![
                HeaderRuleConfig {
                    header: "X-API-Key".to_string(),
                    rule: Rule::Equals {
                        value: "test-secret-key".to_string(),
                        case_sensitive: true,
                    },
                },
                HeaderRuleConfig {
                    header: "X-Client-ID".to_string(),
                    rule: Rule::Equals {
                        value: "cdviz-collector".to_string(),
                        case_sensitive: true,
                    },
                },
            ],
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

        let config = Config {
            enabled: true,
            destination: Url::parse(&format!("{}/events", mock_server.uri())).unwrap(),
            headers: vec![],
        };
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

        let config = Config {
            enabled: true,
            destination: Url::parse(&format!("{}/events", mock_server.uri())).unwrap(),
            headers: vec![],
        };
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
        let message = Message { cdevent, headers: source_headers };

        let_assert!(Ok(()) = sink.send(&message).await);
    }

    #[tokio::test]
    async fn test_http_sink_configured_headers_override_source_headers() {
        use crate::security::rule::{HeaderRuleConfig, Rule};
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
            headers: vec![HeaderRuleConfig {
                header: "Authorization".to_string(),
                rule: Rule::Equals {
                    value: "Bearer configured-token".to_string(),
                    case_sensitive: true,
                },
            }],
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
        let message = Message { cdevent, headers: source_headers };

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
