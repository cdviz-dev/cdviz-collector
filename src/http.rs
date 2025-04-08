use crate::errors::{Error, IntoDiagnostic, ReportWrapper, Result};
use axum::{Json, Router, extract::DefaultBodyLimit, http, response::IntoResponse, routing::get};
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

/// The http server config
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Config {
    /// Listening host of http server
    pub(crate) host: IpAddr,

    /// Listening port of http server
    pub(crate) port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self { host: IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)), port: 8080 }
    }
}

pub(crate) fn launch(
    config: &Config,
    routes: Vec<Router>,
    cancel_token: &'static CancellationToken,
) -> JoinHandle<Result<()>> {
    let addr = SocketAddr::new(config.host, config.port);
    tokio::spawn(async move {
        let app = app(routes);
        // run it
        tracing::warn!("listening on {}", addr);
        let listener = tokio::net::TcpListener::bind(addr).await.into_diagnostic()?;
        axum::serve(listener, app.into_make_service())
            // see [axum/examples/graceful-shutdown/src/main.rs at main · tokio-rs/axum](https://github.com/tokio-rs/axum/blob/main/examples/graceful-shutdown/src/main.rs)
            // TODO check graceful shutdown with spawned task & integration with main
            .with_graceful_shutdown(cancel_token.cancelled())
            .await.into_diagnostic()?;
        tracing::info!(kind = "source", "exiting: http server");
        Ok(())
    })
}

fn app(routes: Vec<Router>) -> Router {
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

    app
        // include trace context as header into the response
        .layer(OtelInResponseLayer)
        //start OpenTelemetry trace on incoming request
        .layer(OtelAxumLayer::default())
        .route("/healthz", get(health)) // request processed without span / trace
        .route("/readyz", get(health)) // request processed without span / trace
        .layer((
            cors,
            SetSensitiveRequestHeadersLayer::new(std::iter::once(http::header::AUTHORIZATION)),
            ValidateRequestHeaderLayer::accept("application/json"),
            RequestDecompressionLayer::new(),
            CompressionLayer::new(),
            // Graceful shutdown will wait for outstanding requests to complete. Add a timeout so
            // requests don't hang forever.
            TimeoutLayer::new(Duration::from_secs(3)),
            // Replace the default of 2MB with 1MB.
            DefaultBodyLimit::max(1024*1024),
        ))
}

async fn health() -> impl IntoResponse {
    http::StatusCode::OK
}

// try to follow [RFC 9457: Problem Details for HTTP APIs](https://www.rfc-editor.org/rfc/rfc9457.html)
impl IntoResponse for Error {
    //TODO report the trace_id into the message to help to debug
    fn into_response(self) -> axum::response::Response {
        // let (status, error_message) = match self {
        //     Error::Db(e) => (http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        //     _ => (http::StatusCode::INTERNAL_SERVER_ERROR, "".to_string()),
        // };
        let trace_id = find_current_trace_id();
        tracing::warn!(error = ?self);
        let body = Json(json!({
            "title": http::StatusCode::INTERNAL_SERVER_ERROR.as_str(),
            "status": http::StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
            "trace_id": trace_id,
        }));
        (http::StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
    }
}

// try to follow [RFC 9457: Problem Details for HTTP APIs](https://www.rfc-editor.org/rfc/rfc9457.html)
impl IntoResponse for ReportWrapper {
    //TODO report the trace_id into the message to help to debug
    fn into_response(self) -> axum::response::Response {
        let trace_id = find_current_trace_id();
        tracing::warn!(error = ?self);
        let body = Json(json!({
            "title": http::StatusCode::INTERNAL_SERVER_ERROR.as_str(),
            "status": http::StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
            "trace_id": trace_id,
        }));

        (http::StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
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
        let app = app(vec![]);
        let response = app
            .oneshot(Request::builder().uri("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(), "*",);
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    // test health endpoint
    async fn test_call_option_for_cors() {
        let app = app(vec![]);
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
        let app = app(vec![]);

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
}
