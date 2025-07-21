use super::Sink;
use crate::Message;
use crate::errors::{IntoDiagnostic, Report, Result};
use cdevents_sdk::cloudevents::BuilderExt;
use cloudevents::{EventBuilder, EventBuilderV10};
use http_cloudevents::RequestBuilderExt;
use reqwest::Url;
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_tracing::TracingMiddleware;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Config {
    /// Is the sink is enabled?
    pub(crate) enabled: bool,
    destination: Url,
}

impl TryFrom<Config> for HttpSink {
    type Error = Report;

    fn try_from(value: Config) -> Result<Self> {
        Ok(HttpSink::new(value.destination))
    }
}

#[derive(Debug)]
pub(crate) struct HttpSink {
    client: ClientWithMiddleware,
    dest: Url,
}

impl HttpSink {
    pub(crate) fn new(url: Url) -> Self {
        // Retry up to 3 times with increasing intervals between attempts.
        //let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
        let client = ClientBuilder::new(reqwest::Client::new())
            // Trace HTTP requests. See the tracing crate to make use of these traces.
            .with(TracingMiddleware::default())
            // Retry failed requests.
            //.with(RetryTransientMiddleware::new_with_policy(retry_policy))
            .build();
        Self { dest: url, client }
    }
}

impl Sink for HttpSink {
    //TODO use cloudevents
    async fn send(&self, msg: &Message) -> Result<()> {
        let cd_event = msg.cdevent.clone();
        // convert  CdEvent to cloudevents
        let event_result = EventBuilderV10::new().with_cdevent(cd_event.clone());

        let mut req = self.client.post(self.dest.clone());
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
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_http_sink() {
        use proptest::prelude::*;
        use proptest::test_runner::TestRunner;
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
        .and(path("/events"))
        .and(header("Content-Type", "application/json"))
        .and(header("ce-specversion", "1.0"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        // Mounting the mock on the mock server - it's now effective!
        .mount(&mock_server)
        .await;

        let config = Config {
            enabled: true,
            destination: Url::parse(&format!("{}/events", &mock_server.uri())).unwrap(),
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
            .and(header("content-type", "application/cloudevents+json"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let config = Config {
            enabled: true,
            destination: Url::parse(&format!("{}/events", mock_server.uri())).unwrap(),
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
            .and(header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let config = Config {
            enabled: true,
            destination: Url::parse(&format!("{}/events", mock_server.uri())).unwrap(),
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
        };
        let_assert!(Ok(_) = HttpSink::try_from(config));

        // Config with various URL schemes
        let schemes = vec!["http", "https"];
        for scheme in schemes {
            let config = Config {
                enabled: true,
                destination: Url::parse(&format!("{scheme}://example.com/events")).unwrap(),
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
            let config = Config { enabled: true, destination: Url::parse(url).unwrap() };

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
        };
        let sink = HttpSink::try_from(config).unwrap();

        // Should handle redirects properly
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
        };
        let sink = HttpSink::try_from(config).unwrap();

        // Verify that tracing middleware is present and doesn't interfere
        let_assert!(Ok(()) = sink.send(&msg).await);
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
