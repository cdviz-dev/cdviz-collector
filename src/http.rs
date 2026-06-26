use crate::errors::{Error, IntoDiagnostic, ReportWrapper, Result};

/// Default maximum request body size (1 MB).
///
/// Applied globally to all HTTP routes including webhooks. Override per-route with
/// [`axum::extract::DefaultBodyLimit`] if a source needs a different limit.
const DEFAULT_BODY_LIMIT_BYTES: usize = 1024 * 1024;
use axum::Extension;
use axum::middleware::{self, Next};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http,
    response::IntoResponse,
    routing::get,
};
use axum_tracing_opentelemetry::middleware::{OtelAxumLayer, OtelInResponseLayer};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tower_http::{
    compression::CompressionLayer,
    cors::{Any, CorsLayer},
    decompression::RequestDecompressionLayer,
    sensitive_headers::SetSensitiveRequestHeadersLayer,
    timeout::TimeoutLayer,
    validate_request::ValidateRequestHeaderLayer,
};
use tracing_opentelemetry_instrumentation_sdk::find_current_trace_id;

/// Status filter patterns for access logging.
/// Each entry is an exact status code (`"404"`) or a class (`"4xx"`, `"5xx"`).
/// An empty list disables logging.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct AccessLogConfig {
    #[serde(default = "default_access_log_filter")]
    pub(crate) filter: Vec<String>,
}

fn default_access_log_filter() -> Vec<String> {
    vec!["4xx".into(), "5xx".into()]
}

impl Default for AccessLogConfig {
    fn default() -> Self {
        Self { filter: default_access_log_filter() }
    }
}

fn status_matches_filter(status: u16, filter: &[String]) -> bool {
    let class = format!("{}xx", status / 100);
    let exact = status.to_string();
    filter.iter().any(|p| p == &exact || p == &class)
}

async fn access_log_middleware(
    State(filter): State<Vec<String>>,
    req: Request,
    next: Next,
) -> axum::response::Response {
    let uri = req.uri().to_string();
    let method = req.method().to_string();
    let resp = next.run(req).await;
    let status = resp.status().as_u16();
    if status_matches_filter(status, &filter) {
        let trace_id = find_current_trace_id();
        tracing::warn!(http_status = status, uri, method, ?trace_id, "access");
    }
    resp
}

/// The http server config
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Config {
    /// Listening host of http server
    pub(crate) host: IpAddr,

    /// Listening port of http server
    pub(crate) port: u16,

    /// Root URL of the service (used for generating URLs in responses, webhooks, etc.)
    /// Must be a valid absolute URL. Will be validated at deserialization time.
    #[serde(default = "default_root_url")]
    pub(crate) root_url: url::Url,

    /// Access log configuration: which HTTP responses to log.
    #[serde(default)]
    pub(crate) access_log: AccessLogConfig,

    /// Per-request timeout (e.g. "30s"); requests exceeding it return 408.
    /// Keeps a bound so requests don't hang forever during graceful shutdown.
    #[serde(default = "default_request_timeout", with = "humantime_serde")]
    pub(crate) request_timeout: Duration,
}

#[allow(clippy::expect_used)]
fn default_root_url() -> url::Url {
    url::Url::parse("http://cdviz-collector.example.com")
        .expect("default root_url should be a valid URL")
}

fn default_request_timeout() -> Duration {
    Duration::from_secs(30)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            port: 8080,
            root_url: default_root_url(),
            access_log: AccessLogConfig::default(),
            request_timeout: default_request_timeout(),
        }
    }
}

pub(crate) fn launch(
    config: &Config,
    routes: Vec<Router>,
    shutdown_token: CancellationToken,
) -> JoinHandle<Result<()>> {
    let addr = SocketAddr::new(config.host, config.port);
    let access_log = config.access_log.clone();
    let request_timeout = config.request_timeout;
    tokio::spawn(async move {
        let app = app(access_log, request_timeout, routes, shutdown_token.clone());
        // run it
        tracing::warn!("listening on {}", addr);
        let listener = tokio::net::TcpListener::bind(addr).await.into_diagnostic()?;
        axum::serve(listener, app.into_make_service())
            // see [axum/examples/graceful-shutdown/src/main.rs at main · tokio-rs/axum](https://github.com/tokio-rs/axum/blob/main/examples/graceful-shutdown/src/main.rs)
            .with_graceful_shutdown(async move {
                shutdown_token.cancelled().await;
                tracing::info!("request graceful shutdown");
            })
            .await.into_diagnostic()?;
        tracing::info!(kind = "source", "exiting: http server");
        Ok(())
    })
}

fn app(
    access_log: AccessLogConfig,
    request_timeout: Duration,
    routes: Vec<Router>,
    shutdown_token: CancellationToken,
) -> Router {
    // build our application with a route
    let mut app = Router::new();
    for route in routes {
        app = app.merge(route);
    }

    let cors = CorsLayer::new()
        // allow `GET` and `POST` when accessing the resource
        .allow_methods([http::Method::GET, http::Method::OPTIONS, http::Method::PATCH, http::Method::PUT, http::Method::POST])
        .allow_headers(Any)
        // allow requests from any origin
        .allow_origin(Any);

    // TimeoutLayer lives inside the OTel span so access_log (which is also inside the span)
    // receives the 408 response and can log it with a valid trace_id.
    let inner = app
        // include trace context as header into the response
        .layer(OtelInResponseLayer)
        // Graceful shutdown will wait for outstanding requests to complete. Add a timeout so
        // requests don't hang forever.
        .layer(TimeoutLayer::with_status_code(http::StatusCode::REQUEST_TIMEOUT, request_timeout));

    let inner = if access_log.filter.is_empty() {
        inner
    } else {
        inner.layer(middleware::from_fn_with_state(access_log.filter, access_log_middleware))
    };

    inner
        //start OpenTelemetry trace on incoming request
        .layer(OtelAxumLayer::default())
        // request processed without span / trace
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .fallback(fallback)
        .layer(Extension(shutdown_token))
        .layer((
            cors,
            SetSensitiveRequestHeadersLayer::new(std::iter::once(http::header::AUTHORIZATION)),
            ValidateRequestHeaderLayer::accept("application/json"),
            RequestDecompressionLayer::new(),
            CompressionLayer::new(),
            DefaultBodyLimit::max(DEFAULT_BODY_LIMIT_BYTES),
        ))
}

async fn ready(shutdown_token: Extension<CancellationToken>) -> impl IntoResponse {
    if shutdown_token.is_cancelled() {
        http::StatusCode::PRECONDITION_FAILED
    } else {
        http::StatusCode::OK
    }
}

async fn health() -> impl IntoResponse {
    http::StatusCode::OK
}

async fn fallback() -> impl IntoResponse {
    http::StatusCode::NOT_FOUND
}

// try to follow [RFC 9457: Problem Details for HTTP APIs](https://www.rfc-editor.org/rfc/rfc9457.html)
impl IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        let trace_id = find_current_trace_id();
        tracing::warn!(error = ?self);
        let status = self.status_code();
        // Detail is only surfaced for client-caused (4xx) rejections; 5xx stays opaque.
        let body = if status.is_client_error() {
            Json(json!({
                "title": status.as_str(),
                "status": status.as_u16(),
                "detail": self.to_string(),
                "trace_id": trace_id,
            }))
        } else {
            Json(json!({
                "title": status.as_str(),
                "status": status.as_u16(),
                "trace_id": trace_id,
            }))
        };
        (status, body).into_response()
    }
}

// try to follow [RFC 9457: Problem Details for HTTP APIs](https://www.rfc-editor.org/rfc/rfc9457.html)
impl IntoResponse for ReportWrapper {
    fn into_response(self) -> axum::response::Response {
        let trace_id = find_current_trace_id();
        tracing::warn!(error = ?self);
        match self.0.downcast_ref::<Error>() {
            Some(err) if err.status_code().is_client_error() => {
                let status = err.status_code();
                let body = Json(json!({
                    "title": status.as_str(),
                    "status": status.as_u16(),
                    "detail": err.to_string(),
                    "trace_id": trace_id,
                }));
                (status, body).into_response()
            }
            _ => {
                let body = Json(json!({
                    "title": http::StatusCode::INTERNAL_SERVER_ERROR.as_str(),
                    "status": http::StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    "trace_id": trace_id,
                }));
                (http::StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{self, Request, StatusCode},
    };
    use rstest::*;
    use serde_json::json;
    use tower::ServiceExt; // for `oneshot`

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    // test health endpoint
    async fn test_readyz() {
        let shutdown_token = CancellationToken::new();
        let app = app(
            AccessLogConfig::default(),
            default_request_timeout(),
            vec![],
            shutdown_token.clone(),
        );
        let response = app
            .clone()
            .oneshot(Request::builder().uri("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(), "*",);
        shutdown_token.cancel();
        let response = app
            .oneshot(Request::builder().uri("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    // test health endpoint
    async fn test_call_option_for_cors() {
        let app = app(
            AccessLogConfig::default(),
            default_request_timeout(),
            vec![],
            CancellationToken::new(),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/webhook/000-cdevents")
                    .method(http::Method::OPTIONS)
                    .header(http::header::ORIGIN, "http://example.com")
                    .header(http::header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                    .header(http::header::ACCESS_CONTROL_REQUEST_HEADERS, "Content-Type")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(), "*");
        assert_eq!(
            response.headers().get(http::header::ACCESS_CONTROL_ALLOW_METHODS).unwrap(),
            "GET,OPTIONS,PATCH,PUT,POST"
        );
        assert_eq!(
            response.headers().get(http::header::ACCESS_CONTROL_ALLOW_HEADERS).unwrap(),
            "*"
        );
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_post_webhook_not_found() {
        let app = app(
            AccessLogConfig::default(),
            default_request_timeout(),
            vec![],
            CancellationToken::new(),
        );

        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/webhook/test")
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&json!([1, 2, 3, 4])).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_request_timeout_returns_408() {
        use axum::routing::get;
        // a route slower than the configured timeout must yield 408
        let slow = Router::new().route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                StatusCode::OK
            }),
        );
        let app = app(
            AccessLogConfig::default(),
            Duration::from_millis(1),
            vec![slow],
            CancellationToken::new(),
        );
        let response = app
            .oneshot(Request::builder().uri("/slow").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    }
}
