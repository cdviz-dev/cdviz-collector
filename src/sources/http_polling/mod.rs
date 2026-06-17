#![doc = include_str!("README.md")]

mod filter;
mod request;
mod retry_after_middleware;

use crate::errors::{IntoDiagnostic, Result};
use crate::security::header::OutgoingHeaderConfig;
use crate::security::header::{
    OutgoingHeaderMap, generate_headers, outgoing_header_map_to_configs,
};
use crate::sources::{EventSource, EventSourcePipe};
use filter::TimeWindowFilter;
use futures::stream::{FuturesUnordered, StreamExt};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{RetryTransientMiddleware, policies::ExponentialBackoff};
use reqwest_tracing::TracingMiddleware;
use retry_after_middleware::RetryAfterMiddleware;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, sleep};
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

    /// VRL script driving the (multi-pass) request worklist. Invoked once at
    /// bootstrap (with `.response = null`) to seed requests, then again for every
    /// response whose `route` feeds back. It receives
    /// `{ metadata, state, request, response }` and must set `.requests` to an
    /// array of request objects; it may set `.state` (an immutable snapshot
    /// carried into each request). See the module README for the full contract.
    pub(crate) driver_vrl: String,

    /// Static/secret/signature headers merged into each request.
    #[serde(default)]
    pub(crate) headers: OutgoingHeaderMap,

    /// Retry duration for transient HTTP failures.
    #[serde(default = "default_retry_duration", with = "humantime_serde")]
    pub(crate) total_duration_of_retries: Duration,

    /// Default parser for response bodies routed to the pipeline. Overridable
    /// per-request via the driver's `requests[].parser`. Controls the `Accept`
    /// request header.
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

    /// Minimum delay between consecutive HTTP requests (the *start* of each
    /// request is spaced by at least this much, across the whole worklist).
    /// Useful for APIs without rate-limit headers (e.g. Jenkins). Default: none.
    #[serde(default, with = "humantime_serde::option")]
    pub(crate) min_request_interval: Option<Duration>,

    /// Maximum number of requests fetched concurrently within one poll.
    /// Default: 4.
    #[serde(default = "default_max_concurrency")]
    pub(crate) max_concurrency: usize,

    /// Hard budget on the total number of requests issued in a single poll
    /// (guards against runaway driver loops). Default: 1000.
    #[serde(default = "default_max_requests")]
    pub(crate) max_requests: u32,

    /// Maximum feedback-chain depth (bootstrap requests are depth 0). Guards
    /// against unbounded recursion. Default: 50.
    #[serde(default = "default_max_depth")]
    pub(crate) max_depth: u32,
}

fn default_user_agent() -> String {
    crate::DEFAULT_USER_AGENT.to_string()
}

fn default_max_concurrency() -> usize {
    4
}

fn default_max_requests() -> u32 {
    1000
}

fn default_max_depth() -> u32 {
    50
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

/// A pending request in the worklist, carrying its immutable state snapshot.
struct RequestTask {
    spec: request::RequestSpec,
    /// Snapshot of the driver state at the time this request was emitted.
    state_snapshot: serde_json::Value,
    /// Feedback-chain depth (bootstrap requests are depth 0).
    depth: u32,
}

/// A fetched HTTP response, decoupled from parsing/routing.
struct FetchedResponse {
    status: u16,
    headers: HashMap<String, String>,
    content_type: Option<String>,
    body_text: String,
}

/// Send one HTTP request and read its response. Free function (borrows nothing
/// from the extractor) so it can run inside a `FuturesUnordered` while the
/// extractor is later borrowed mutably to emit events. Returns `Err(())` on any
/// recoverable failure (already logged).
async fn fetch(
    client: &ClientWithMiddleware,
    spec: &request::RequestSpec,
    accept: &'static str,
    header_configs: &[OutgoingHeaderConfig],
) -> std::result::Result<FetchedResponse, ()> {
    let Ok(method_val) = reqwest::Method::from_bytes(spec.method.to_uppercase().as_bytes()) else {
        tracing::warn!(method = spec.method, "invalid HTTP method");
        return Err(());
    };

    let mut url = match url::Url::parse(&spec.url) {
        Ok(u) => u,
        Err(err) => {
            tracing::warn!(?err, url = spec.url, "invalid URL");
            return Err(());
        }
    };
    if !spec.query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (k, v) in &spec.query {
            pairs.append_pair(k, v);
        }
    }
    let url_str = url.to_string();

    let mut req = client.request(method_val, url);
    req = req.header(reqwest::header::ACCEPT, accept);
    for (name, value) in &spec.headers {
        req = req.header(name.as_str(), value.as_str());
    }
    let body_bytes = spec.body.as_deref().unwrap_or(&[]);
    match generate_headers(header_configs, Some(body_bytes)) {
        Ok(headers) => req = req.headers(headers),
        Err(err) => tracing::warn!(?err, "failed to generate static headers, skipping"),
    }
    if let Some(body) = &spec.body {
        req = req.body(body.clone());
    }

    let response = match req.send().await {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(?err, url = url_str, "HTTP polling request error");
            return Err(());
        }
    };

    let status = response.status();
    if !status.is_success() {
        tracing::warn!(status = status.as_u16(), url = url_str, "HTTP polling non-success status");
        return Err(());
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let headers: HashMap<String, String> = response
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.to_string(), s.to_string())))
        .collect();
    let body_text = match response.text().await {
        Ok(t) => t,
        Err(err) => {
            tracing::warn!(?err, url = url_str, "failed to read response body");
            return Err(());
        }
    };

    Ok(FetchedResponse { status: status.as_u16(), headers, content_type, body_text })
}

pub(crate) struct HttpPollingExtractor {
    config: Config,
    filter: TimeWindowFilter,
    driver_program: request::DriverProgram,
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
        let driver_program = request::DriverProgram::compile(&config.driver_vrl)?;

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
            driver_program,
            client,
            next,
            source_name,
            #[cfg(feature = "state")]
            state_op,
        })
    }

    /// Build the time-window-aware base metadata object (config metadata merged
    /// with `ts_after`/`ts_before`/`context`).
    fn base_metadata(&self) -> serde_json::Map<String, serde_json::Value> {
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
        match metadata {
            serde_json::Value::Object(obj) => obj,
            _ => serde_json::Map::new(),
        }
    }

    /// Build the driver VRL input: `{ metadata, state, request, response }`.
    /// `request`/`response` are `null` on the bootstrap call.
    fn build_driver_input(
        &self,
        state: &serde_json::Value,
        request: Option<&request::RequestSpec>,
        response: Option<&FetchedResponse>,
    ) -> serde_json::Value {
        let request_json = request
            .map(|s| serde_json::json!({ "url": s.url, "method": s.method, "headers": s.headers }));
        let response_json = response.map(|r| {
            // Best-effort: expose the body as parsed JSON so the driver can read
            // cursors/ids; fall back to a raw string when it is not JSON.
            let body = serde_json::from_str::<serde_json::Value>(&r.body_text)
                .unwrap_or_else(|_| serde_json::Value::String(r.body_text.clone()));
            serde_json::json!({ "status": r.status, "headers": r.headers, "body": body })
        });
        serde_json::json!({
            "metadata": self.base_metadata(),
            "state": state,
            "request": request_json,
            "response": response_json,
        })
    }

    /// Build the `EventSource` metadata for an emitted event, including the
    /// request's immutable state snapshot under `http_polling.state`.
    fn build_event_metadata(
        &self,
        url: &str,
        method: &str,
        status: u16,
        state: &serde_json::Value,
    ) -> serde_json::Value {
        let mut obj = self.base_metadata();
        let mut http_polling =
            serde_json::json!({ "url": url, "method": method, "status": status });
        if !state.is_null()
            && let serde_json::Value::Object(hp) = &mut http_polling
        {
            hp.insert("state".to_string(), state.clone());
        }
        obj.insert("http_polling".to_string(), http_polling);
        serde_json::Value::Object(obj)
    }

    /// Invoke the driver and turn its output into worklist tasks at `depth`.
    fn driver_tasks(&self, input: &serde_json::Value, depth: u32) -> Result<VecDeque<RequestTask>> {
        let out = self.driver_program.execute(input)?;
        let state = out.state;
        Ok(out
            .requests
            .into_iter()
            .map(|spec| RequestTask { spec, state_snapshot: state.clone(), depth })
            .collect())
    }

    /// Perform a single poll: drive the request worklist to exhaustion, emitting
    /// events for `pipeline`/`both` responses and feeding `feedback`/`both`
    /// responses back into the driver for further requests.
    ///
    /// Returns `Ok(true)` when the worklist drained and the caller should advance
    /// the time window. Returns `Ok(false)` (window stays put, retried next poll)
    /// when the bootstrap driver call failed, or when requests were issued but
    /// **none** fetched successfully (a transport/HTTP outage). A successful fetch
    /// whose body fails to parse still counts as progress and advances the window
    /// — re-polling the same window would not change a malformed 2xx body. Sinks
    /// should be idempotent if strict consistency is required.
    #[instrument(skip(self), fields(source = %self.source_name))]
    async fn run_once(&mut self) -> Result<bool> {
        // Bootstrap: seed the worklist (response = null).
        let bootstrap = self.build_driver_input(&serde_json::Value::Null, None, None);
        let mut queue = match self.driver_tasks(&bootstrap, 0) {
            Ok(q) => q,
            Err(err) => {
                tracing::warn!(?err, "driver bootstrap execution failed");
                return Ok(false);
            }
        };

        let header_configs = Arc::new(outgoing_header_map_to_configs(&self.config.headers));
        let accept = self.config.parser.accept_header();
        let concurrency = self.config.max_concurrency.max(1);
        let mut inflight = FuturesUnordered::new();
        let mut issued: u32 = 0;
        let mut fetch_successes: u32 = 0;
        let mut next_allowed: Option<Instant> = None;

        loop {
            // Fill in-flight slots from the queue, honoring budget + rate limit.
            while inflight.len() < concurrency {
                let Some(task) = queue.pop_front() else { break };
                if issued >= self.config.max_requests {
                    tracing::warn!(
                        max_requests = self.config.max_requests,
                        source = %self.source_name,
                        "request budget reached; remaining worklist dropped"
                    );
                    queue.clear();
                    break;
                }
                issued += 1;

                if let Some(interval) = self.config.min_request_interval {
                    let now = Instant::now();
                    let at = next_allowed.unwrap_or(now);
                    if at > now {
                        sleep(at - now).await;
                    }
                    next_allowed = Some(Instant::now() + interval);
                }

                let client = self.client.clone();
                let configs = Arc::clone(&header_configs);
                inflight.push(async move {
                    let resp = fetch(&client, &task.spec, accept, &configs).await;
                    (task, resp)
                });
            }

            let Some((task, resp)) = inflight.next().await else {
                if queue.is_empty() {
                    break;
                }
                continue;
            };

            let Ok(resp) = resp else {
                // Failure already logged in `fetch`.
                continue;
            };
            fetch_successes += 1;

            if task.spec.route.emits() {
                self.emit_response(&task, &resp);
            }

            if task.spec.route.feeds_back() {
                if task.depth >= self.config.max_depth {
                    tracing::warn!(
                        max_depth = self.config.max_depth,
                        source = %self.source_name,
                        "max feedback depth reached; not recursing"
                    );
                } else {
                    let input = self.build_driver_input(
                        &task.state_snapshot,
                        Some(&task.spec),
                        Some(&resp),
                    );
                    match self.driver_tasks(&input, task.depth + 1) {
                        Ok(mut tasks) => queue.append(&mut tasks),
                        Err(err) => tracing::warn!(?err, "driver feedback execution failed"),
                    }
                }
            }
        }

        // Hold the window if we issued requests but none reached the server.
        if issued > 0 && fetch_successes == 0 {
            return Ok(false);
        }
        Ok(true)
    }

    /// Parse a response routed to the pipeline and forward its events downstream.
    fn emit_response(&mut self, task: &RequestTask, resp: &FetchedResponse) {
        let parser = task.spec.parser.clone().unwrap_or_else(|| self.config.parser.clone());
        let resolved = parser.resolve(resp.content_type.as_deref());
        let body_values = match resolved.parse(&resp.body_text) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(?err, url = task.spec.url, "failed to parse response body");
                return;
            }
        };
        let metadata = self.build_event_metadata(
            &task.spec.url,
            &task.spec.method,
            resp.status,
            &task.state_snapshot,
        );
        for body_value in body_values {
            let event = EventSource {
                body: body_value,
                metadata: metadata.clone(),
                headers: resp.headers.clone(),
            };
            if let Err(err) = self.next.send(event) {
                tracing::warn!(?err, "failed to send event downstream");
            }
        }
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

    /// Build a config from a full driver VRL script.
    fn config_with_driver(driver_vrl: String) -> Config {
        Config {
            polling_interval: Duration::from_millis(50),
            ts_after: None,
            ts_before_limit: None,
            driver_vrl,
            headers: OutgoingHeaderMap::new(),
            total_duration_of_retries: Duration::from_millis(100),
            parser: ParserConfig::Json,
            metadata: serde_json::json!({}),
            user_agent: default_user_agent(),
            min_request_interval: None,
            max_concurrency: default_max_concurrency(),
            max_requests: default_max_requests(),
            max_depth: default_max_depth(),
        }
    }

    /// Config whose driver issues a single GET to `{uri}/data` (pipeline route).
    fn make_config(server_uri: &str) -> Config {
        config_with_driver(format!(r#".requests = [{{ "url": "{server_uri}/data" }}]"#))
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

        let config = make_config(&server.uri());
        let (mut extractor, collector) = make_extractor(&config);

        assert!(extractor.run_once().await.unwrap()); // should advance

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

        let config = make_config(&server.uri());
        let (mut extractor, _collector) = make_extractor(&config);

        // The only issued request failed to fetch → window must NOT advance.
        assert!(!extractor.run_once().await.unwrap());
    }

    #[tokio::test]
    async fn test_parse_failure_still_advances() {
        // A 2xx with an unparseable body is "progress": re-polling the same
        // window won't fix the body, so the window advances (0 events emitted).
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw("this is not json", "application/json"),
            )
            .mount(&server)
            .await;

        let config = make_config(&server.uri());
        let (mut extractor, collector) = make_extractor(&config);

        assert!(extractor.run_once().await.unwrap()); // advances despite parse error
        assert_eq!(collector.try_into_iter().unwrap().count(), 0);
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

        let mut config = make_config(&server.uri());
        config.parser = ParserConfig::Jsonl;
        let (mut extractor, collector) = make_extractor(&config);

        assert!(extractor.run_once().await.unwrap());
        assert_eq!(collector.try_into_iter().unwrap().count(), 2);
    }

    #[tokio::test]
    async fn test_per_request_parser_override() {
        // Source default is Json, but the driver requests jsonl for this request.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("{\"a\":1}\n{\"b\":2}\n", "application/octet-stream"),
            )
            .mount(&server)
            .await;

        let config = config_with_driver(format!(
            r#".requests = [{{ "url": "{}/data", "parser": "jsonl" }}]"#,
            server.uri()
        ));
        let (mut extractor, collector) = make_extractor(&config);

        assert!(extractor.run_once().await.unwrap());
        assert_eq!(collector.try_into_iter().unwrap().count(), 2);
    }

    #[tokio::test]
    async fn test_ts_before_limit_stops_source() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
            .mount(&server)
            .await;

        let mut config = make_config(&server.uri());
        config.ts_before_limit = Some(jiff::Timestamp::now());
        config.ts_after = Some(jiff::Timestamp::MIN);

        let cancel_token = CancellationToken::new();
        let (mut extractor, _collector) = make_extractor(&config);

        let result = timeout(Duration::from_secs(2), extractor.run(cancel_token)).await;
        assert!(result.is_ok(), "run() should complete within timeout");
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn test_driver_reads_window_timestamps() {
        let server = MockServer::start().await;
        // Match only when the `from` query param (from ts_after) is present.
        Mock::given(method("GET"))
            .and(path("/data"))
            .and(wiremock::matchers::query_param("from", "2024-01-01T00:00:00Z"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
            .expect(1)
            .mount(&server)
            .await;

        let mut config = config_with_driver(format!(
            r#".requests = [{{ "url": "{}/data", "query": {{ "from": to_string!(.metadata.ts_after) }} }}]"#,
            server.uri()
        ));
        config.ts_after = Some("2024-01-01T00:00:00Z".parse().unwrap());
        let (mut extractor, _collector) = make_extractor(&config);
        assert!(extractor.run_once().await.unwrap());
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

        let mut config = make_config(&server.uri());
        config.headers.insert(
            "x-api-key".to_string(),
            HeaderSource::Static { value: "test-secret".to_string() },
        );

        let (mut extractor, _collector) = make_extractor(&config);
        assert!(extractor.run_once().await.unwrap());
    }

    #[tokio::test]
    async fn test_raw_text_parser() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("hello world", "text/plain"))
            .mount(&server)
            .await;

        let mut config = make_config(&server.uri());
        config.parser = ParserConfig::RawText;
        let (mut extractor, collector) = make_extractor(&config);

        assert!(extractor.run_once().await.unwrap());
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

        let mut config = make_config(&server.uri());
        config.polling_interval = Duration::from_mins(1); // long interval
        let cancel_token = CancellationToken::new();
        let (mut extractor, _collector) = make_extractor(&config);

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
    async fn test_bootstrap_emits_multiple_requests() {
        // One bootstrap call can seed several pipeline requests.
        let server = MockServer::start().await;
        for p in ["/a", "/b", "/c"] {
            Mock::given(method("GET"))
                .and(path(p))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_raw(format!(r#"{{"p":"{p}"}}"#), "application/json"),
                )
                .mount(&server)
                .await;
        }

        let uri = server.uri();
        let config = config_with_driver(format!(
            r#".requests = [{{ "url": "{uri}/a" }}, {{ "url": "{uri}/b" }}, {{ "url": "{uri}/c" }}]"#
        ));
        let (mut extractor, collector) = make_extractor(&config);

        assert!(extractor.run_once().await.unwrap());
        assert_eq!(collector.try_into_iter().unwrap().count(), 3);
    }

    #[tokio::test]
    async fn test_two_pass_discovery_then_detail() {
        // Pass 1: discovery list (feedback only, not emitted).
        // Pass 2: per-item detail fetch (pipeline), carrying discovery id in state.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/list"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(r#"["1","2"]"#, "application/json"),
            )
            .mount(&server)
            .await;
        for id in ["1", "2"] {
            Mock::given(method("GET"))
                .and(path(format!("/item/{id}")))
                .respond_with(
                    ResponseTemplate::new(200).set_body_raw(
                        format!(r#"{{"id":"{id}","detail":true}}"#),
                        "application/json",
                    ),
                )
                .mount(&server)
                .await;
        }

        let uri = server.uri();
        let driver = format!(
            r#"
            if .response == null {{
                # bootstrap: discovery request, feed result back
                .requests = [{{ "url": "{uri}/list", "route": "feedback" }}]
            }} else {{
                # feedback: one detail request per discovered id, carry id in state
                reqs = []
                for_each(array!(.response.body)) -> |_i, id| {{
                    reqs = push(reqs, {{
                        "url": "{uri}/item/" + string!(id),
                        "route": "pipeline",
                    }})
                }}
                .requests = reqs
            }}
            "#
        );
        let config = config_with_driver(driver);
        let (mut extractor, collector) = make_extractor(&config);

        assert!(extractor.run_once().await.unwrap());
        let mut ids: Vec<String> = collector
            .try_into_iter()
            .unwrap()
            .map(|e| e.body["id"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["1".to_string(), "2".to_string()]);
    }

    #[tokio::test]
    async fn test_state_snapshot_in_event_metadata() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
            .mount(&server)
            .await;

        let config = config_with_driver(format!(
            r#"
            .state = {{ "discovery_id": "abc" }}
            .requests = [{{ "url": "{}/data" }}]
            "#,
            server.uri()
        ));
        let (mut extractor, collector) = make_extractor(&config);

        assert!(extractor.run_once().await.unwrap());
        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events[0].metadata["http_polling"]["state"]["discovery_id"], "abc");
    }

    #[tokio::test]
    async fn test_graphql_both_cursor_loop() {
        // `both` route: emit each page's node AND feed pageInfo back until the
        // cursor is null. Three pages → three events, then the loop terminates.
        let server = MockServer::start().await;
        let pages = [
            (r#"{"node":1,"next":"c2"}"#),
            (r#"{"node":2,"next":"c3"}"#),
            (r#"{"node":3,"next":null}"#),
        ];
        // page 1 has no cursor param; pages 2/3 are matched by cursor query.
        Mock::given(method("GET"))
            .and(path("/gql"))
            .and(wiremock::matchers::query_param("cursor", "c2"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(pages[1], "application/json"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/gql"))
            .and(wiremock::matchers::query_param("cursor", "c3"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(pages[2], "application/json"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/gql"))
            .and(wiremock::matchers::query_param_is_missing("cursor"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(pages[0], "application/json"))
            .mount(&server)
            .await;

        let uri = server.uri();
        let driver = format!(
            r#"
            if .response == null {{
                .requests = [{{ "url": "{uri}/gql", "route": "both" }}]
            }} else {{
                next = .response.body.next
                if next != null {{
                    .requests = [{{
                        "url": "{uri}/gql",
                        "query": {{ "cursor": next }},
                        "route": "both",
                    }}]
                }} else {{
                    .requests = []
                }}
            }}
            "#
        );
        let config = config_with_driver(driver);
        let (mut extractor, collector) = make_extractor(&config);

        assert!(extractor.run_once().await.unwrap());
        let mut nodes: Vec<i64> =
            collector.try_into_iter().unwrap().map(|e| e.body["node"].as_i64().unwrap()).collect();
        nodes.sort_unstable();
        assert_eq!(nodes, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn test_max_depth_guard_stops_recursion() {
        // Driver always asks to feed back; max_depth must stop the chain.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/loop"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
            .expect(3) // depth 0 (bootstrap) + 2 feedback hops, then stop at depth 2
            .mount(&server)
            .await;

        let mut config = config_with_driver(format!(
            r#".requests = [{{ "url": "{}/loop", "route": "feedback" }}]"#,
            server.uri()
        ));
        config.max_depth = 2;
        let (mut extractor, _collector) = make_extractor(&config);

        assert!(extractor.run_once().await.unwrap());
        server.verify().await;
    }

    #[tokio::test]
    async fn test_max_requests_budget_caps_fetches() {
        let server = MockServer::start().await;
        // Bootstrap emits 10 requests but budget is 3.
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
            .expect(3)
            .mount(&server)
            .await;

        let uri = server.uri();
        let reqs = format!(r#"{{ "url": "{uri}/data" }},"#).repeat(10);
        let mut config = config_with_driver(format!(".requests = [{reqs}]"));
        config.max_requests = 3;
        config.max_concurrency = 1;
        let (mut extractor, _collector) = make_extractor(&config);

        assert!(extractor.run_once().await.unwrap());
        server.verify().await;
    }

    #[tokio::test]
    async fn test_min_request_interval_spaces_requests() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
            .mount(&server)
            .await;

        let uri = server.uri();
        let mut config = config_with_driver(format!(
            r#".requests = [{{ "url": "{uri}/data" }}, {{ "url": "{uri}/data" }}, {{ "url": "{uri}/data" }}]"#
        ));
        config.min_request_interval = Some(Duration::from_millis(100));
        config.max_concurrency = 4; // even with concurrency, starts are spaced
        let (mut extractor, _collector) = make_extractor(&config);

        let start = std::time::Instant::now();
        assert!(extractor.run_once().await.unwrap());
        // 3 requests spaced by ≥100ms → at least 200ms total.
        assert!(start.elapsed() >= Duration::from_millis(200), "requests were not spaced");
    }
}
