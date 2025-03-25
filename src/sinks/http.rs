use super::Sink;
use crate::Message;
use crate::errors::{IntoDiagnostic, Report, Result};
use crate::security::signature;
use cdevents_sdk::cloudevents::BuilderExt;
use cloudevents::{EventBuilder, EventBuilderV10};
use http_cloudevents::RequestBuilderExt;
use reqwest::Url;
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_tracing::TracingMiddleware;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Config {
    /// Is the sink is enabled?
    pub(crate) enabled: bool,
    destination: Url,
    /// Add header with signature to the outgoing request
    #[serde(default)]
    pub(crate) signature: Option<signature::SignatureConfig>,
}

impl TryFrom<Config> for HttpSink {
    type Error = Report;

    fn try_from(value: Config) -> Result<Self> {
        Ok(HttpSink::new(value.destination, value.signature))
    }
}

#[derive(Debug)]
pub(crate) struct HttpSink {
    client: ClientWithMiddleware,
    dest: Url,
}

impl HttpSink {
    pub(crate) fn new(url: Url, signature_config: Option<signature::SignatureConfig>) -> Self {
        // Retry up to 3 times with increasing intervals between attempts.
        //let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
        let mut client_builder = ClientBuilder::new(reqwest::Client::new())
            // Trace HTTP requests. See the tracing crate to make use of these traces.
            .with(TracingMiddleware::default())
            // Retry failed requests.
            //.with(RetryTransientMiddleware::new_with_policy(retry_policy))
            ;
        if let Some(config) = signature_config {
            client_builder = client_builder.with(http_signature::SignatureMiddleware::new(config));
        }
        Self { dest: url, client: client_builder.build() }
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

mod http_signature {
    use axum::http::{Extensions, HeaderName}; // axum::http is a reexport of http crate
    use reqwest::{Request, Response};
    use reqwest_middleware::{Error, Middleware, Next, Result};
    use std::str::FromStr;

    use crate::security::signature;

    pub(super) struct SignatureMiddleware {
        config: signature::SignatureConfig,
    }

    impl SignatureMiddleware {
        pub fn new(config: signature::SignatureConfig) -> Self {
            Self { config }
        }

        fn sign_request(&self, req: Request) -> Result<Request> {
            let body_bytes = req.body().and_then(|body| body.as_bytes()).unwrap_or(&[]);
            let signature = signature::build_signature(&self.config, req.headers(), body_bytes)
                .map_err(Error::middleware)?;
            let mut req = req;
            {
                let header_name =
                    HeaderName::from_str(self.config.header()).map_err(Error::middleware)?;
                let val = signature.parse().map_err(Error::middleware)?;
                req.headers_mut().insert(header_name, val);
            };
            Ok(req)
        }
    }

    #[async_trait::async_trait]
    impl Middleware for SignatureMiddleware {
        async fn handle(
            &self,
            req: Request,
            extensions: &mut Extensions,
            next: Next<'_>,
        ) -> Result<Response> {
            let req = self.sign_request(req)?;
            next.run(req, extensions).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::signature;
    use proptest::prelude::*;
    use proptest::test_runner::TestRunner;
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_http_sink() {
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
            signature: None,
        };

        let sink = HttpSink::try_from(config).unwrap();

        let mut runner = TestRunner::default();
        for _ in 0..1 {
            let val = any::<Message>().new_tree(&mut runner).unwrap();
            assert!(sink.send(&val.current()).await.is_ok());
        }
    }

    #[tokio::test]
    async fn test_http_sink_with_signature() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/events"))
            .and(header("Content-Type", "application/json"))
            .and(header_exists("X-Signature"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock_server)
            .await;

        let signature_config = signature::SignatureConfig {
            header: "X-Signature".to_string(),
            token: "mySecretToken".to_string().into(),
            ..Default::default()
        };

        let config = Config {
            enabled: true,
            destination: Url::parse(&format!("{}/events", &mock_server.uri())).unwrap(),
            signature: Some(signature_config.clone()),
        };

        let sink = HttpSink::try_from(config).unwrap();

        let mut runner = TestRunner::default();
        for _ in 0..1 {
            let val = any::<Message>().new_tree(&mut runner).unwrap();
            assert!(sink.send(&val.current()).await.is_ok());
        }
    }
}
