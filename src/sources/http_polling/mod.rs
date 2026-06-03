#![doc = include_str!("README.md")]

mod filter;
mod request;
mod retry_after_middleware;

use crate::errors::{IntoDiagnostic, Result};
use crate::security::header::{
    OutgoingHeaderMap, generate_headers, outgoing_header_map_to_configs,
};
use crate::sources::{EventSource, EventSourcePipe};
use filter::TimeWindowFilter;
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{RetryTransientMiddleware, policies::ExponentialBackoff};
use reqwest_tracing::TracingMiddleware;
use retry_after_middleware::RetryAfterMiddleware;
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

    /// Follow `Link: <url>; rel="next"` headers to consume all pages within a
    /// single time window before advancing. Default: `false`.
    #[serde(default)]
    pub(crate) follow_link_header: bool,

    /// Minimum delay between consecutive HTTP requests, including pagination
    /// fetches. Useful for APIs without rate-limit headers (e.g. Jenkins).
    /// Default: no delay.
    #[serde(default, with = "humantime_serde::option")]
    pub(crate) min_request_interval: Option<Duration>,
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

/// Outcome of a single page fetch.
enum PageOutcome {
    /// Page fetched successfully. Inner value is the next page URL (if any).
    Done(Option<String>),
    /// Recoverable failure; already logged.
    Failed,
}

/// Maximum number of pages to follow in a single `run_once` poll cycle.
const MAX_PAGES: u32 = 1000;

/// Extract the `rel="next"` URL from a `Link` header, if present.
///
/// Handles the RFC 5988 format: `Link: <https://api.example.com/page2>; rel="next"`
/// Multiple link entries separated by commas are supported.
fn parse_link_next(headers: &reqwest::header::HeaderMap) -> Option<&str> {
    let value = headers.get(reqwest::header::LINK)?.to_str().ok()?;
    for part in value.split(',') {
        let part = part.trim();
        // Split into URL part and parameter segment(s)
        let mut segments = part.splitn(2, ';');
        let Some(url_segment) = segments.next() else { continue };
        let url_part = url_segment.trim().trim_start_matches('<').trim_end_matches('>');
        // Each ';'-separated param token is checked for exact "rel=next" match
        // (RFC 5988 §5.3 — token must be exactly "next", not "nextpage" etc.)
        let params = segments.next().unwrap_or("");
        let is_next = params.split(';').map(str::trim).any(|p| {
            let p = p.to_ascii_lowercase();
            p == "rel=\"next\"" || p == "rel=next"
        });
        if is_next {
            return Some(url_part);
        }
    }
    None
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
        // Disable automatic redirect following so RetryAfterMiddleware can handle
        // 303/301/302/307 responses with optional Retry-After delays.
        let client = ClientBuilder::new(
            reqwest::Client::builder()
                .user_agent(&config.user_agent)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .into_diagnostic()?,
        )
        .with(TracingMiddleware::default())
        .with(RetryAfterMiddleware)
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

    /// Send one HTTP request and forward the parsed events downstream.
    ///
    /// Returns `Ok(PageOutcome::Done(next_url))` on success — `next_url` is `Some`
    /// when a `Link: rel="next"` header indicates more pages, `None` for the last
    /// page. Returns `Ok(PageOutcome::Failed)` on a recoverable failure (already
    /// logged).
    async fn fetch_page(
        &mut self,
        url: &str,
        method: &str,
        params_headers: &HashMap<String, String>,
        params_body: Option<Vec<u8>>,
    ) -> Result<PageOutcome> {
        let method_val = reqwest::Method::from_bytes(method.to_uppercase().as_bytes())
            .map_err(|_| miette::MietteDiagnostic::new(format!("invalid HTTP method: {method}")))?;

        let mut req = self.client.request(method_val, url);
        req = req.header(reqwest::header::ACCEPT, self.config.parser.accept_header());
        for (name, value) in params_headers {
            req = req.header(name.as_str(), value.as_str());
        }

        let header_configs = outgoing_header_map_to_configs(&self.config.headers);
        let body_bytes = params_body.as_deref().unwrap_or(&[]);
        match generate_headers(&header_configs, Some(body_bytes)) {
            Ok(headers) => req = req.headers(headers),
            Err(e) => tracing::warn!(err = ?e, "failed to generate static headers, skipping"),
        }
        if let Some(body) = params_body {
            req = req.body(body);
        }

        let response = match req.send().await {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(?err, url, "HTTP polling request error");
                return Ok(PageOutcome::Failed);
            }
        };

        let status = response.status();
        if !status.is_success() {
            tracing::warn!(status = status.as_u16(), url, "HTTP polling non-success status");
            return Ok(PageOutcome::Failed);
        }

        let next_url = self
            .config
            .follow_link_header
            .then(|| parse_link_next(response.headers()))
            .flatten()
            .map(str::to_string);

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
                return Ok(PageOutcome::Failed);
            }
        };

        let resolved = self.config.parser.resolve(content_type.as_deref());
        let body_values = match resolved.parse(&body_text) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(?err, url, "failed to parse response body");
                return Ok(PageOutcome::Failed);
            }
        };

        let metadata = self.build_event_metadata(url, method, status.as_u16());
        for body_value in body_values {
            let event = EventSource {
                body: body_value,
                metadata: metadata.clone(),
                headers: response_headers.clone(),
            };
            if let Err(err) = self.next.send(event) {
                tracing::warn!(?err, "failed to send event downstream");
                return Ok(PageOutcome::Failed);
            }
        }

        Ok(PageOutcome::Done(next_url))
    }

    /// Perform a single poll: build request via VRL, fetch all pages, forward events.
    ///
    /// Returns `Ok(true)` on success (caller should advance the time window),
    /// `Ok(false)` on recoverable failure (already logged), or `Err` on hard failure.
    ///
    /// **Partial pagination failure:** if page N > 0 fails after pages 0..N-1 succeeded,
    /// this still returns `Ok(true)` so the window advances past the already-emitted
    /// events. The failed page's events are silently dropped. Sinks should be idempotent
    /// if full consistency is required.
    #[instrument(skip(self), fields(source = %self.source_name))]
    async fn run_once(&mut self) -> Result<bool> {
        // 1. Generate initial request params via VRL
        let vrl_input = self.build_vrl_input();
        let params = match self.request_program.execute(&vrl_input) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(?err, "VRL request generation failed");
                return Ok(false);
            }
        };

        // 2. Build URL with query params
        let mut initial_url = url::Url::parse(&params.url)
            .map_err(|e| miette::MietteDiagnostic::new(format!("invalid URL: {e}")))?;
        if !params.query.is_empty() {
            let mut pairs = initial_url.query_pairs_mut();
            for (k, v) in &params.query {
                pairs.append_pair(k, v);
            }
        }

        // 3. Fetch first page (and follow pagination links if configured)
        let mut current_url = initial_url.to_string();
        let mut page_index: u32 = 0;
        loop {
            if page_index > 0
                && let Some(interval) = self.config.min_request_interval
            {
                sleep(interval).await;
            }

            if page_index >= MAX_PAGES {
                tracing::warn!(
                    MAX_PAGES,
                    source = %self.source_name,
                    "page limit reached; remaining pages skipped"
                );
                break;
            }

            match self
                .fetch_page(&current_url, &params.method, &params.headers, params.body.clone())
                .await?
            {
                PageOutcome::Done(Some(next)) => {
                    current_url = next;
                    page_index += 1;
                }
                PageOutcome::Done(None) => break,
                PageOutcome::Failed if page_index == 0 => {
                    // First page failed; do not advance window
                    return Ok(false);
                }
                PageOutcome::Failed => {
                    // Pages 0..page_index-1 succeeded; advancing the window prevents
                    // re-emitting those events. Events from this page are dropped.
                    tracing::warn!(
                        url = current_url,
                        source = %self.source_name,
                        "pagination fetch failed; partial window processed"
                    );
                    break;
                }
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
    use crate::transformers::collect_to_vec::Collector;
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
            follow_link_header: false,
            min_request_interval: None,
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
            follow_link_header: false,
            min_request_interval: None,
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
            polling_interval: Duration::from_mins(1), // long interval
            ts_after: None,
            ts_before_limit: None,
            request_vrl: format!(r#".url = "{}/data""#, server.uri()),
            headers: OutgoingHeaderMap::new(),
            total_duration_of_retries: Duration::from_millis(100),
            parser: ParserConfig::Json,
            metadata: serde_json::json!({}),
            user_agent: default_user_agent(),
            follow_link_header: false,
            min_request_interval: None,
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

    #[tokio::test]
    async fn test_pagination_follows_link_header() {
        let server = MockServer::start().await;

        // Page 1 — returns Link: rel="next" pointing to /page2
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        "link",
                        format!("<{}/page2>; rel=\"next\"", server.uri()).as_str(),
                    )
                    .set_body_raw(r#"{"page":1}"#, "application/json"),
            )
            .mount(&server)
            .await;

        // Page 2 — no Link header
        Mock::given(method("GET"))
            .and(path("/page2"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(r#"{"page":2}"#, "application/json"),
            )
            .mount(&server)
            .await;

        let mut config = make_config(&server.uri(), "");
        config.follow_link_header = true;
        let (mut extractor, collector) = make_extractor(&config);

        let result = extractor.run_once().await;
        assert!(result.is_ok());
        assert!(result.unwrap()); // should advance

        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 2, "expected events from both pages");
        assert_eq!(events[0].body["page"], 1);
        assert_eq!(events[1].body["page"], 2);
    }

    #[tokio::test]
    async fn test_pagination_disabled_stops_at_first_page() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        "link",
                        format!("<{}/page2>; rel=\"next\"", server.uri()).as_str(),
                    )
                    .set_body_raw(r#"{"page":1}"#, "application/json"),
            )
            .mount(&server)
            .await;

        // follow_link_header is false (default) — page2 should never be requested
        let config = make_config(&server.uri(), "");
        let (mut extractor, collector) = make_extractor(&config);

        let result = extractor.run_once().await;
        assert!(result.unwrap());

        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 1, "should only fetch first page");
    }

    #[test]
    fn test_parse_link_next_basic() {
        use reqwest::header::{HeaderMap, HeaderValue, LINK};
        let mut h = HeaderMap::new();
        h.insert(LINK, HeaderValue::from_static("<https://api.example.com/page2>; rel=\"next\""));
        assert_eq!(parse_link_next(&h), Some("https://api.example.com/page2"));
    }

    #[test]
    fn test_parse_link_next_multiple_rels() {
        use reqwest::header::{HeaderMap, HeaderValue, LINK};
        let mut h = HeaderMap::new();
        h.insert(
            LINK,
            HeaderValue::from_static(
                "<https://api.example.com/page1>; rel=\"prev\", <https://api.example.com/page3>; rel=\"next\"",
            ),
        );
        assert_eq!(parse_link_next(&h), Some("https://api.example.com/page3"));
    }

    #[test]
    fn test_parse_link_next_absent() {
        use reqwest::header::{HeaderMap, HeaderValue, LINK};
        let mut h = HeaderMap::new();
        h.insert(LINK, HeaderValue::from_static("<https://api.example.com/page1>; rel=\"prev\""));
        assert_eq!(parse_link_next(&h), None);
    }

    #[test]
    fn test_parse_link_next_no_header() {
        use reqwest::header::HeaderMap;
        assert_eq!(parse_link_next(&HeaderMap::new()), None);
    }

    #[test]
    fn test_parse_link_next_does_not_match_partial_rel() {
        // "rel=nextpage" must NOT match "rel=next"
        use reqwest::header::{HeaderMap, HeaderValue, LINK};
        let mut h = HeaderMap::new();
        h.insert(LINK, HeaderValue::from_static("<https://api.example.com/page2>; rel=nextpage"));
        assert_eq!(parse_link_next(&h), None);
    }

    #[test]
    fn test_parse_link_next_unquoted_rel() {
        // rel=next (unquoted) is valid per RFC 5988
        use reqwest::header::{HeaderMap, HeaderValue, LINK};
        let mut h = HeaderMap::new();
        h.insert(LINK, HeaderValue::from_static("<https://api.example.com/page2>; rel=next"));
        assert_eq!(parse_link_next(&h), Some("https://api.example.com/page2"));
    }

    #[test]
    fn test_parse_link_next_extra_params() {
        // rel="next" with additional params like title="foo"
        use reqwest::header::{HeaderMap, HeaderValue, LINK};
        let mut h = HeaderMap::new();
        h.insert(
            LINK,
            HeaderValue::from_static(
                "<https://api.example.com/page2>; rel=\"next\"; title=\"Page 2\"",
            ),
        );
        assert_eq!(parse_link_next(&h), Some("https://api.example.com/page2"));
    }
}
