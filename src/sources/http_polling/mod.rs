#![doc = include_str!("README.md")]

mod filter;
mod request;
mod retry_after_middleware;

use crate::errors::{IntoDiagnostic, Result};
use crate::security::header::OutgoingHeaderConfig;
use crate::security::header::{
    OutgoingHeaderMap, filter_http_headers, generate_headers, outgoing_header_map_to_configs,
};
use crate::sources::{EventSource, EventSourcePipe};
use filter::TimeWindowFilter;
use futures::stream::{FuturesUnordered, StreamExt};
use reqwest::header::{HeaderMap, HeaderName};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{RetryTransientMiddleware, policies::ExponentialBackoff};
use retry_after_middleware::RetryAfterMiddleware;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

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

    /// Response headers to forward into the pipeline (transformers + sinks).
    /// Case-insensitive whitelist, mirroring the webhook source's `headers_to_keep`.
    /// Default: empty — no response headers are forwarded downstream. The VRL
    /// driver always sees the full set of response headers regardless of this.
    #[serde(default)]
    pub(crate) headers_to_keep: Vec<String>,

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

    /// Per-status handling policy for non-2xx responses. See [`StatusPolicy`].
    #[serde(default)]
    pub(crate) on_status: StatusPolicy,

    /// Log a warning when a non-2xx response is received. Defaults to true.
    #[serde(default = "default_true")]
    pub(crate) log_errors: bool,
}

/// What to do with a non-2xx HTTP response (see [`StatusPolicy`]).
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Behavior {
    /// Ignore the response; advance the time window (counts as progress).
    Skip,
    /// Hold the time window; retry the whole query sequence next poll.
    Hold,
    /// Retry this single request via the middleware (exponential backoff +
    /// `total_duration_of_retries`); if still failing, degrade to [`Behavior::Hold`].
    Retry,
    /// Stop the source entirely (loud misconfiguration signal).
    Abort,
}

/// Maps an HTTP status — an exact code (`"404"`) or a class (`"4xx"`) — to a
/// [`Behavior`] for non-2xx responses. Also serves as the
/// [`reqwest_retry::RetryableStrategy`], so `retry` statuses are retried by the
/// middleware with the configured backoff/budget (the type owns both the policy
/// data and how to apply it).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct StatusPolicy(HashMap<String, Behavior>);

impl StatusPolicy {
    /// Resolve the [`Behavior`] for a status: exact code → class → built-in default.
    /// Built-in defaults are a function of the status (so they can't be a single
    /// `Default` value, and live here rather than as seed entries that user config
    /// would replace wholesale): `401`/`403` → `abort` (auth/permission misconfig
    /// that won't self-heal), `5xx` → `hold` (transient), everything else → `skip`.
    fn resolve(&self, status: u16) -> Behavior {
        if let Some(b) = self.0.get(&status.to_string()) {
            return *b;
        }
        if let Some(b) = self.0.get(&format!("{}xx", status / 100)) {
            return *b;
        }
        match status {
            401 | 403 => Behavior::Abort,
            s if s >= 500 => Behavior::Hold,
            _ => Behavior::Skip,
        }
    }
}

impl reqwest_retry::RetryableStrategy for StatusPolicy {
    fn handle(
        &self,
        res: &std::result::Result<reqwest::Response, reqwest_middleware::Error>,
    ) -> Option<reqwest_retry::Retryable> {
        if let Ok(resp) = res
            && self.resolve(resp.status().as_u16()) == Behavior::Retry
        {
            return Some(reqwest_retry::Retryable::Transient);
        }
        reqwest_retry::DefaultRetryableStrategy.handle(res)
    }
}

/// Outcome of a single poll, deciding what `run()` does with the time window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PollOutcome {
    /// Advance the time window (and persist the checkpoint).
    Advance,
    /// Keep the window; retry the same window on the next poll.
    Hold,
    /// Stop the source entirely.
    Abort,
}

impl PollOutcome {
    #[cfg(test)]
    fn advanced(self) -> bool {
        matches!(self, Self::Advance)
    }
}

fn default_user_agent() -> String {
    crate::DEFAULT_USER_AGENT.to_string()
}

fn default_true() -> bool {
    true
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
    headers: HeaderMap,
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
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let headers = response.headers().clone();
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
    /// Whitelist of response header names forwarded into the pipe.
    headers_to_keep: Vec<HeaderName>,
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
        // No TracingMiddleware here: polls run every interval and are usually idle, so a span
        // per HTTP request would be noise. The trace originates only when a message is emitted
        // (the source span in `emit_response`). Retry middleware is independent of tracing.
        .with(RetryAfterMiddleware)
        .with(RetryTransientMiddleware::new_with_policy_and_strategy(
            retry_policy,
            config.on_status.clone(),
        ))
        .build();

        #[cfg(feature = "state")]
        let state_op = state_config.and_then(|sc| {
            sc.make_operator()
                .map_err(|err| tracing::warn!(?err, "failed to create state operator"))
                .ok()
        });

        #[cfg(not(feature = "state"))]
        let _ = state_config; // suppress unused variable warning

        let headers_to_keep = config
            .headers_to_keep
            .iter()
            .filter_map(|name| HeaderName::from_str(name).ok())
            .collect();

        Ok(Self {
            config: config.clone(),
            filter,
            driver_program,
            client,
            next,
            source_name,
            headers_to_keep,
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
            // The driver sees ALL response headers (e.g. for Link/cursor pagination),
            // independent of the `headers_to_keep` whitelist applied to the pipe.
            let headers: HashMap<String, String> = r
                .headers
                .iter()
                .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.to_string(), s.to_string())))
                .collect();
            serde_json::json!({ "status": r.status, "headers": headers, "body": body })
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

    /// Resolve and log the [`Behavior`] for a non-2xx response per `on_status`.
    fn classify_non_success(&self, status: u16, url: &str) -> Behavior {
        let behavior = self.config.on_status.resolve(status);
        if self.config.log_errors {
            match behavior {
                Behavior::Skip => {
                    tracing::warn!(status, url, source = %self.source_name, "non-success status, skipped");
                }
                Behavior::Hold | Behavior::Retry => {
                    tracing::warn!(status, url, source = %self.source_name, "non-success status, holding window");
                }
                Behavior::Abort => {
                    tracing::error!(status, url, source = %self.source_name, "non-success status, aborting source");
                }
            }
        }
        behavior
    }

    /// Perform a single poll: drive the request worklist to exhaustion, emitting
    /// events for `pipeline`/`both` responses and feeding `feedback`/`both`
    /// responses back into the driver for further requests.
    ///
    /// Returns [`PollOutcome::Advance`] when the worklist drained and the caller
    /// should advance the time window. Returns [`PollOutcome::Hold`] (window stays
    /// put, retried next poll) when the bootstrap driver call failed, or when
    /// requests were issued but **none** made progress (a transport/HTTP outage, or
    /// every response resolved to `hold`/exhausted `retry`). Returns
    /// [`PollOutcome::Abort`] when a response resolved to [`Behavior::Abort`].
    ///
    /// A 2xx response whose body fails to parse still counts as progress and
    /// advances the window — re-polling the same window would not change a malformed
    /// body. A non-2xx response routed to `skip` also counts as progress. Sinks
    /// should be idempotent if strict consistency is required.
    // Deliberately NOT instrumented: a poll runs every interval and is usually idle, so a
    // per-poll span would create a "bunch of empty spans". The trace originates instead at the
    // source `SpanPipe` (in `emit_response → self.next.send`), i.e. only when a message is
    // actually emitted. Inner logs carry `source` explicitly to keep diagnostics.
    async fn run_once(&mut self) -> Result<PollOutcome> {
        // Bootstrap: seed the worklist (response = null).
        let bootstrap = self.build_driver_input(&serde_json::Value::Null, None, None);
        let mut queue = match self.driver_tasks(&bootstrap, 0) {
            Ok(q) => q,
            Err(err) => {
                tracing::warn!(?err, source = %self.source_name, "driver bootstrap execution failed");
                return Ok(PollOutcome::Hold);
            }
        };

        let header_configs = Arc::new(outgoing_header_map_to_configs(&self.config.headers));
        let accept = self.config.parser.accept_header();
        let concurrency = self.config.max_concurrency.max(1);
        let mut inflight = FuturesUnordered::new();
        let mut issued: u32 = 0;
        let mut progressed: u32 = 0;
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
                // Transport failure already logged in `fetch`; not progress.
                continue;
            };

            // Non-2xx responses are handled by the status policy, not the driver.
            if !(200..300).contains(&resp.status) {
                match self.classify_non_success(resp.status, &task.spec.url) {
                    Behavior::Skip => progressed += 1,
                    // `Retry` reaching here means the middleware exhausted its retry
                    // budget, so it degrades to `hold`.
                    Behavior::Hold | Behavior::Retry => {}
                    Behavior::Abort => return Ok(PollOutcome::Abort),
                }
                continue;
            }
            progressed += 1;

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
                        Err(err) => {
                            tracing::warn!(?err, source = %self.source_name, "driver feedback execution failed");
                        }
                    }
                }
            }
        }

        // Hold the window if we issued requests but none made progress.
        if issued > 0 && progressed == 0 {
            return Ok(PollOutcome::Hold);
        }
        Ok(PollOutcome::Advance)
    }

    /// Parse a response routed to the pipeline and forward its events downstream.
    fn emit_response(&mut self, task: &RequestTask, resp: &FetchedResponse) {
        let parser = task.spec.parser.clone().unwrap_or_else(|| self.config.parser.clone());
        let resolved = parser.resolve(resp.content_type.as_deref());
        let body_values = match resolved.parse(&resp.body_text) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(?err, source = %self.source_name, url = task.spec.url, "failed to parse response body");
                return;
            }
        };
        let metadata = self.build_event_metadata(
            &task.spec.url,
            &task.spec.method,
            resp.status,
            &task.state_snapshot,
        );
        let headers = filter_http_headers(&resp.headers, &self.headers_to_keep);
        for body_value in body_values {
            let event = EventSource {
                body: body_value,
                metadata: metadata.clone(),
                headers: headers.clone(),
            };
            if let Err(err) = self.next.send(event) {
                tracing::warn!(?err, source = %self.source_name, "failed to send event downstream");
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
                Ok(PollOutcome::Advance) => {
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
                Ok(PollOutcome::Hold) => {
                    // Soft failure already logged in run_once(); don't advance.
                }
                Ok(PollOutcome::Abort) => {
                    // Abort already logged in run_once(); stop the source.
                    tracing::error!(source = %self.source_name, "source aborting after status policy");
                    break;
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
            headers_to_keep: Vec::new(),
            total_duration_of_retries: Duration::from_millis(100),
            parser: ParserConfig::Json,
            metadata: serde_json::json!({}),
            user_agent: default_user_agent(),
            min_request_interval: None,
            max_concurrency: default_max_concurrency(),
            max_requests: default_max_requests(),
            max_depth: default_max_depth(),
            on_status: StatusPolicy::default(),
            log_errors: true,
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

        assert!(extractor.run_once().await.unwrap().advanced()); // should advance

        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].body, serde_json::json!({"key": "value"}));
    }

    #[tokio::test]
    async fn test_response_headers_whitelist_to_pipe() {
        // The response carries two headers; only the whitelisted one (case-insensitive)
        // reaches the pipe, while the driver still sees all of them.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .insert_header("x-secret", "leak-me-not")
                    .set_body_raw(r#"{"key":"value"}"#, "application/json"),
            )
            .mount(&server)
            .await;

        let mut config = make_config(&server.uri());
        config.headers_to_keep = vec!["Content-Type".to_string()]; // mixed case on purpose
        let (mut extractor, collector) = make_extractor(&config);

        assert!(extractor.run_once().await.unwrap().advanced());

        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        assert!(
            !events[0].headers.contains_key("x-secret"),
            "non-whitelisted header must not reach the pipe"
        );
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

        // 5xx → default `hold` behavior → no progress → window must NOT advance.
        assert!(!extractor.run_once().await.unwrap().advanced());
    }

    #[tokio::test]
    async fn test_4xx_skips_and_advances_by_default() {
        // A definitive 4xx (404) is not a transient outage: with no `on_status`
        // override it resolves to `skip`, so the window advances (no infinite loop)
        // and nothing is emitted.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let config = make_config(&server.uri());
        let (mut extractor, collector) = make_extractor(&config);

        assert!(extractor.run_once().await.unwrap().advanced());
        assert_eq!(collector.try_into_iter().unwrap().count(), 0);
    }

    #[tokio::test]
    async fn test_4xx_hold_override_does_not_advance() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let mut config = make_config(&server.uri());
        config.on_status.0.insert("404".to_string(), Behavior::Hold);
        let (mut extractor, _collector) = make_extractor(&config);

        assert!(!extractor.run_once().await.unwrap().advanced());
    }

    #[tokio::test]
    async fn test_abort_on_403_by_default() {
        // 403 aborts the source by default (no on_status entry) — a loud signal for
        // an auth/permission misconfiguration.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let config = make_config(&server.uri());
        let (mut extractor, _collector) = make_extractor(&config);

        assert_eq!(extractor.run_once().await.unwrap(), PollOutcome::Abort);
    }

    #[tokio::test]
    async fn test_retry_then_hold() {
        // `retry` lifts the status into the middleware's transient set, so the
        // single query is re-attempted with backoff. The mock always 404s, so once
        // the retry budget is exhausted the poll degrades to `hold` (no advance).
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(404))
            .expect(2..) // initial attempt + at least one middleware retry
            .mount(&server)
            .await;

        let mut config = make_config(&server.uri());
        config.on_status.0.insert("404".to_string(), Behavior::Retry);
        // Budget must exceed the backoff's ~1s minimum retry interval so at least
        // one retry actually fires.
        config.total_duration_of_retries = Duration::from_secs(3);
        let (mut extractor, _collector) = make_extractor(&config);

        assert!(!extractor.run_once().await.unwrap().advanced());
        server.verify().await;
    }

    #[test]
    fn test_status_policy_resolve_precedence() {
        let mut map = HashMap::new();
        map.insert("404".to_string(), Behavior::Abort);
        map.insert("4xx".to_string(), Behavior::Skip);
        map.insert("5xx".to_string(), Behavior::Retry);
        let policy = StatusPolicy(map);

        // exact code wins over class
        assert_eq!(policy.resolve(404), Behavior::Abort);
        // class match
        assert_eq!(policy.resolve(403), Behavior::Skip);
        assert_eq!(policy.resolve(500), Behavior::Retry);
        // built-in defaults (no entry): 401/403 → abort, 5xx → hold, else skip
        let empty = StatusPolicy::default();
        assert_eq!(empty.resolve(401), Behavior::Abort);
        assert_eq!(empty.resolve(403), Behavior::Abort);
        assert_eq!(empty.resolve(404), Behavior::Skip);
        assert_eq!(empty.resolve(503), Behavior::Hold);
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

        assert!(extractor.run_once().await.unwrap().advanced()); // advances despite parse error
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

        assert!(extractor.run_once().await.unwrap().advanced());
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

        assert!(extractor.run_once().await.unwrap().advanced());
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
        assert!(extractor.run_once().await.unwrap().advanced());
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
            HeaderSource::Static {
                value: "test-secret".to_string(),
                prefix: String::new(),
                suffix: String::new(),
            },
        );

        let (mut extractor, _collector) = make_extractor(&config);
        assert!(extractor.run_once().await.unwrap().advanced());
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

        assert!(extractor.run_once().await.unwrap().advanced());
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

        assert!(extractor.run_once().await.unwrap().advanced());
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

        assert!(extractor.run_once().await.unwrap().advanced());
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

        assert!(extractor.run_once().await.unwrap().advanced());
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

        assert!(extractor.run_once().await.unwrap().advanced());
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

        assert!(extractor.run_once().await.unwrap().advanced());
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

        assert!(extractor.run_once().await.unwrap().advanced());
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
        assert!(extractor.run_once().await.unwrap().advanced());
        // 3 requests spaced by ≥100ms → at least 200ms total.
        assert!(start.elapsed() >= Duration::from_millis(200), "requests were not spaced");
    }
}
