#![doc = include_str!("README.md")]

mod filter;
mod request;

use crate::errors::{IntoDiagnostic, Result};
use crate::security::header::{
    OutgoingHeaderMap, generate_headers, outgoing_header_map_to_configs,
};
use crate::sources::{EventSource, EventSourcePipe};
use filter::TimeWindowFilter;
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{RetryTransientMiddleware, policies::ExponentialBackoff};
use reqwest_tracing::TracingMiddleware;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::instrument;

fn default_retry_duration() -> Duration {
    Duration::from_secs(30)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Config {
    /// How often to poll the endpoint.
    #[serde(with = "humantime_serde")]
    pub(crate) polling_interval: Duration,

    /// Initial lower bound for the time window (bootstrap start).
    /// If state is persisted from a previous run, the persisted value wins.
    /// Defaults to `Timestamp::MIN` if absent.
    #[serde(default)]
    pub(crate) ts_after: Option<jiff::Timestamp>,

    /// Optional upper cap. Once `ts_after` reaches this value the source stops.
    #[serde(default)]
    pub(crate) ts_before_limit: Option<jiff::Timestamp>,

    /// VRL script that receives `{ metadata: { ts_after, ts_before, ... } }` and
    /// must set `.url` (required), `.method`, `.headers`, `.body`, `.query`.
    pub(crate) request_vrl: String,

    /// Static/secret/signature headers merged into each request.
    #[serde(default)]
    pub(crate) headers: OutgoingHeaderMap,

    /// Retry duration for transient HTTP failures.
    #[serde(default = "default_retry_duration", with = "humantime_serde")]
    pub(crate) total_duration_of_retries: Duration,

    /// How to parse the response body. Controls the `Accept` request header.
    #[serde(default)]
    pub(crate) parser: ParserConfig,

    /// Base metadata included in every `EventSource`. `context.source` is
    /// auto-populated during config loading if not already set.
    #[serde(default)]
    pub(crate) metadata: serde_json::Value,
    /// `User-Agent` header sent with every request.
    /// Defaults to `cdviz-collector/<version>`.
    #[serde(default = "default_user_agent")]
    pub(crate) user_agent: String,
}

fn default_user_agent() -> String {
    crate::DEFAULT_USER_AGENT.to_string()
}

/// Controls how the HTTP response body is split into [`EventSource`] instances.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum ParserConfig {
    /// Detect from the response `Content-Type` header.
    #[default]
    #[serde(alias = "auto")]
    Auto,
    /// Whole body parsed as a single JSON value.
    #[serde(alias = "json")]
    Json,
    /// One `EventSource` per newline-delimited JSON line.
    #[serde(alias = "jsonl")]
    Jsonl,
    /// Whole body as a JSON string (raw text).
    #[serde(alias = "text")]
    RawText,
}

impl ParserConfig {
    fn accept_header(&self) -> &'static str {
        match self {
            Self::Auto => "application/json, application/x-ndjson, text/plain",
            Self::Json => "application/json",
            Self::Jsonl => "application/x-ndjson",
            Self::RawText => "text/plain",
        }
    }

    /// Resolve `Auto` to a concrete parser based on the response `Content-Type`.
    fn resolve(&self, content_type: Option<&str>) -> ResolvedParser {
        match self {
            Self::Json => ResolvedParser::Json,
            Self::Jsonl => ResolvedParser::Jsonl,
            Self::RawText => ResolvedParser::RawText,
            Self::Auto => {
                let ct = content_type.unwrap_or("");
                if ct.contains("application/x-ndjson") || ct.contains("application/jsonl") {
                    ResolvedParser::Jsonl
                } else if ct.contains("text/") {
                    ResolvedParser::RawText
                } else {
                    // Default to JSON for application/json and unknown types
                    ResolvedParser::Json
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedParser {
    Json,
    Jsonl,
    RawText,
}

impl ResolvedParser {
    /// Parse the response body into a list of JSON values.
    ///
    /// Returns `Err` on parse failure so the caller can skip timestamp advancement.
    fn parse(self, body: &str) -> Result<Vec<serde_json::Value>> {
        match self {
            Self::Json => {
                let v = serde_json::from_str(body).into_diagnostic()?;
                Ok(vec![v])
            }
            Self::Jsonl => {
                let mut values = Vec::new();
                for line in body.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let v: serde_json::Value = serde_json::from_str(line).into_diagnostic()?;
                    values.push(v);
                }
                Ok(values)
            }
            Self::RawText => Ok(vec![serde_json::Value::String(body.to_string())]),
        }
    }
}

pub(crate) struct HttpPollingExtractor {
    config: Config,
    filter: TimeWindowFilter,
    request_program: request::RequestProgram,
    client: ClientWithMiddleware,
    next: EventSourcePipe,
    source_name: String,
    #[cfg(feature = "state")]
    state_op: Option<opendal::Operator>,
}

impl HttpPollingExtractor {
    pub(crate) fn try_from(
        config: &Config,
        next: EventSourcePipe,
        state_config: Option<&crate::state::Config>,
        source_name: String,
    ) -> Result<Self> {
        let request_program = request::RequestProgram::compile(&config.request_vrl)?;

        let ts_after = config.ts_after.unwrap_or(jiff::Timestamp::MIN);
        let filter = TimeWindowFilter::new(ts_after, config.ts_before_limit);

        let retry_policy = ExponentialBackoff::builder()
            .build_with_total_retry_duration_and_limit_retries(config.total_duration_of_retries);
        let client = ClientBuilder::new(
            reqwest::Client::builder().user_agent(&config.user_agent).build().into_diagnostic()?,
        )
        .with(TracingMiddleware::default())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build();

        #[cfg(feature = "state")]
        let state_op = state_config.and_then(|sc| {
            sc.make_operator()
                .map_err(|err| tracing::warn!(?err, "failed to create state operator"))
                .ok()
        });

        #[cfg(not(feature = "state"))]
        let _ = state_config; // suppress unused variable warning

        Ok(Self {
            config: config.clone(),
            filter,
            request_program,
            client,
            next,
            source_name,
            #[cfg(feature = "state")]
            state_op,
        })
    }

    /// Build the VRL input object for request generation.
    fn build_vrl_input(&self) -> serde_json::Value {
        let mut metadata = self.config.metadata.clone();
        if !metadata.is_object() {
            metadata = serde_json::json!({});
        }
        let ts_meta = self.filter.as_metadata();
        if let (serde_json::Value::Object(obj), serde_json::Value::Object(ts_obj)) =
            (&mut metadata, ts_meta)
        {
            for (k, v) in ts_obj {
                obj.insert(k, v);
            }
        }
        serde_json::json!({ "metadata": metadata })
    }

    /// Build the `EventSource` metadata for events from this poll.
    fn build_event_metadata(&self, url: &str, method: &str, status: u16) -> serde_json::Value {
        let mut metadata = self.config.metadata.clone();
        if !metadata.is_object() {
            metadata = serde_json::json!({});
        }
        let ts_meta = self.filter.as_metadata();
        if let (serde_json::Value::Object(obj), serde_json::Value::Object(ts_obj)) =
            (&mut metadata, ts_meta)
        {
            for (k, v) in ts_obj {
                obj.insert(k, v);
            }
            obj.insert(
                "http_polling".to_string(),
                serde_json::json!({ "url": url, "method": method, "status": status }),
            );
        }
        metadata
    }

    /// Perform a single poll: build request, send, parse response, forward events.
    ///
    /// Returns `Ok(true)` on success (caller should advance the time window),
    /// `Ok(false)` on recoverable failure (already logged), or `Err` on hard failure.
    #[instrument(skip(self), fields(source = %self.source_name))]
    async fn run_once(&mut self) -> Result<bool> {
        // 1. Generate request params via VRL
        let vrl_input = self.build_vrl_input();
        let params = match self.request_program.execute(&vrl_input) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(?err, "VRL request generation failed");
                return Ok(false);
            }
        };

        // 2. Build URL with query params
        let mut url = url::Url::parse(&params.url)
            .map_err(|e| miette::MietteDiagnostic::new(format!("invalid URL: {e}")))?;
        if !params.query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in &params.query {
                pairs.append_pair(k, v);
            }
        }

        // 3. Build reqwest method
        let method =
            reqwest::Method::from_bytes(params.method.to_uppercase().as_bytes()).map_err(|_| {
                miette::MietteDiagnostic::new(format!("invalid HTTP method: {}", params.method))
            })?;

        let mut req = self.client.request(method, url.as_str());

        // 4. Accept header from parser config
        req = req.header(reqwest::header::ACCEPT, self.config.parser.accept_header());

        // 5. VRL-computed headers (added first; static config may override below)
        for (name, value) in &params.headers {
            req = req.header(name.as_str(), value.as_str());
        }

        // 6. Static/secret/signature headers from config
        let header_configs = outgoing_header_map_to_configs(&self.config.headers);
        let body_bytes = params.body.as_deref().unwrap_or(&[]);
        match generate_headers(&header_configs, Some(body_bytes)) {
            Ok(headers) => req = req.headers(headers),
            Err(e) => tracing::warn!(err = ?e, "failed to generate static headers, skipping"),
        }

        // 7. Request body
        if let Some(body) = params.body {
            req = req.body(body);
        }

        // 8. Send
        let response = match req.send().await {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(?err, url = url.as_str(), "HTTP polling request error");
                return Ok(false);
            }
        };

        let status = response.status();
        if !status.is_success() {
            tracing::warn!(
                status = status.as_u16(),
                url = url.as_str(),
                "HTTP polling returned non-success status, not advancing"
            );
            return Ok(false);
        }

        // 9. Extract content-type before consuming the response
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let response_headers: HashMap<String, String> = response
            .headers()
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.to_string(), s.to_string())))
            .collect();

        let body_text = match response.text().await {
            Ok(t) => t,
            Err(err) => {
                tracing::warn!(?err, "failed to read response body");
                return Ok(false);
            }
        };

        // 10. Parse response body
        let resolved = self.config.parser.resolve(content_type.as_deref());
        let body_values = match resolved.parse(&body_text) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(?err, url = url.as_str(), "failed to parse response body");
                return Ok(false);
            }
        };

        // 11. Build metadata for all events from this poll
        let metadata = self.build_event_metadata(url.as_str(), &params.method, status.as_u16());

        // 12. Send each event downstream
        for body_value in body_values {
            let event = EventSource {
                body: body_value,
                metadata: metadata.clone(),
                headers: response_headers.clone(),
            };
            if let Err(err) = self.next.send(event) {
                tracing::warn!(?err, "failed to send event downstream");
                return Ok(false);
            }
        }

        Ok(true)
    }

    pub(crate) async fn run(&mut self, cancel_token: CancellationToken) -> Result<()> {
        // Load persisted checkpoint when opendal state is available
        #[cfg(feature = "state")]
        if let Some(state_op) = &self.state_op
            && let Some(ts) = crate::state::load_ts_after(state_op, &self.source_name).await
        {
            self.filter.set_ts_after(ts);
        }

        while !cancel_token.is_cancelled() {
            if self.filter.is_at_limit() {
                tracing::info!(
                    source = %self.source_name,
                    "reached ts_before_limit, source stopping"
                );
                break;
            }

            match self.run_once().await {
                Ok(true) => {
                    self.filter.advance();
                    #[cfg(feature = "state")]
                    if let Some(state_op) = &self.state_op
                        && let Err(err) = crate::state::save_ts_after(
                            state_op,
                            &self.source_name,
                            self.filter.ts_after(),
                        )
                        .await
                    {
                        tracing::warn!(
                            ?err,
                            source = %self.source_name,
                            "failed to save checkpoint"
                        );
                    }
                }
                Ok(false) => {
                    // Soft failure already logged in run_once(); don't advance.
                }
                Err(err) => {
                    tracing::warn!(?err, source = %self.source_name, "polling error");
                }
            }

            tokio::select! {
                () = sleep(self.config.polling_interval) => {},
                () = cancel_token.cancelled() => {},
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipes::collect_to_vec::Collector;
    use std::time::Duration;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_config(server_uri: &str, request_vrl: &str) -> Config {
        Config {
            polling_interval: Duration::from_millis(50),
            ts_after: None,
            ts_before_limit: None,
            request_vrl: format!(r#".url = "{server_uri}/data"; {request_vrl}"#),
            headers: OutgoingHeaderMap::new(),
            total_duration_of_retries: Duration::from_millis(100),
            parser: ParserConfig::Json,
            metadata: serde_json::json!({}),
            user_agent: default_user_agent(),
        }
    }

    fn make_extractor(config: &Config) -> (HttpPollingExtractor, Collector<EventSource>) {
        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let extractor =
            HttpPollingExtractor::try_from(config, pipe, None, "test".to_string()).unwrap();
        (extractor, collector)
    }

    #[tokio::test]
    async fn test_happy_path_json_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_raw(r#"{"key":"value"}"#, "application/json"),
            )
            .mount(&server)
            .await;

        let config = make_config(&server.uri(), "");
        let (mut extractor, collector) = make_extractor(&config);

        let result = extractor.run_once().await;
        assert!(result.is_ok());
        assert!(result.unwrap()); // should advance

        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].body, serde_json::json!({"key": "value"}));
    }

    #[tokio::test]
    async fn test_http_error_does_not_advance() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let config = make_config(&server.uri(), "");
        let (mut extractor, _collector) = make_extractor(&config);

        let result = extractor.run_once().await;
        assert!(result.is_ok());
        assert!(!result.unwrap()); // should NOT advance
    }

    #[tokio::test]
    async fn test_parse_failure_does_not_advance() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw("this is not json", "application/json"),
            )
            .mount(&server)
            .await;

        let config = make_config(&server.uri(), "");
        let (mut extractor, _collector) = make_extractor(&config);

        let result = extractor.run_once().await;
        assert!(result.is_ok());
        assert!(!result.unwrap()); // should NOT advance
    }

    #[tokio::test]
    async fn test_jsonl_parser_emits_multiple_events() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("{\"a\":1}\n{\"b\":2}\n", "application/x-ndjson"),
            )
            .mount(&server)
            .await;

        let mut config = make_config(&server.uri(), "");
        config.parser = ParserConfig::Jsonl;
        let (mut extractor, collector) = make_extractor(&config);

        let result = extractor.run_once().await;
        assert!(result.is_ok());
        assert!(result.unwrap()); // should advance

        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn test_ts_before_limit_stops_source() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
            .mount(&server)
            .await;

        let now = jiff::Timestamp::now();
        // Set limit to now: ts_after starts at MIN which is < limit, but after first advance
        // ts_after = ts_before which should be around now, reaching the limit.
        let mut config = make_config(&server.uri(), "");
        config.ts_before_limit = Some(now);
        // Use MIN so ts_after starts before the limit
        config.ts_after = Some(jiff::Timestamp::MIN);

        let cancel_token = CancellationToken::new();
        let (mut extractor, _collector) = make_extractor(&config);

        // Should finish quickly due to ts_before_limit
        let result = timeout(Duration::from_secs(2), extractor.run(cancel_token)).await;
        assert!(result.is_ok(), "run() should complete within timeout");
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn test_vrl_request_generation_includes_timestamps() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
            .mount(&server)
            .await;

        let config = Config {
            polling_interval: Duration::from_millis(50),
            ts_after: None,
            ts_before_limit: None,
            request_vrl: format!(
                r#".url = "{}/data"; .query.from = to_string!(.metadata.ts_after)"#,
                server.uri()
            ),
            headers: OutgoingHeaderMap::new(),
            total_duration_of_retries: Duration::from_millis(100),
            parser: ParserConfig::Json,
            metadata: serde_json::json!({}),
            user_agent: default_user_agent(),
        };
        let (mut extractor, _collector) = make_extractor(&config);

        // run_once should succeed (server returns 200, but query params aren't
        // validated by the mock - we're testing VRL doesn't panic)
        let result = extractor.run_once().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_static_headers_sent_in_request() {
        use crate::security::header::HeaderSource;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .and(header("x-api-key", "test-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
            .expect(1)
            .mount(&server)
            .await;

        let mut config = make_config(&server.uri(), "");
        config.headers.insert(
            "x-api-key".to_string(),
            HeaderSource::Static { value: "test-secret".to_string() },
        );

        let (mut extractor, _collector) = make_extractor(&config);
        let result = extractor.run_once().await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_raw_text_parser() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("hello world", "text/plain"))
            .mount(&server)
            .await;

        let mut config = make_config(&server.uri(), "");
        config.parser = ParserConfig::RawText;
        let (mut extractor, collector) = make_extractor(&config);

        let result = extractor.run_once().await;
        assert!(result.is_ok());
        assert!(result.unwrap());

        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].body, serde_json::Value::String("hello world".to_string()));
    }

    #[tokio::test]
    async fn test_cancellation_stops_run() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
            .mount(&server)
            .await;

        let config = Config {
            polling_interval: Duration::from_secs(60), // long interval
            ts_after: None,
            ts_before_limit: None,
            request_vrl: format!(r#".url = "{}/data""#, server.uri()),
            headers: OutgoingHeaderMap::new(),
            total_duration_of_retries: Duration::from_millis(100),
            parser: ParserConfig::Json,
            metadata: serde_json::json!({}),
            user_agent: default_user_agent(),
        };
        let cancel_token = CancellationToken::new();
        let (mut extractor, _collector) = make_extractor(&config);

        // Cancel after a short delay
        let ct = cancel_token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            ct.cancel();
        });

        let result = timeout(Duration::from_secs(2), extractor.run(cancel_token)).await;
        assert!(result.is_ok(), "run() should stop after cancellation");
        assert!(result.unwrap().is_ok());
    }
}
