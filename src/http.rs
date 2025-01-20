use crate::errors::{Error, IntoDiagnostic, ReportWrapper, Result};
use axum::{extract::DefaultBodyLimit, http, response::IntoResponse, routing::get, Json, Router};
use axum_tracing_opentelemetry::middleware::{OtelAxumLayer, OtelInResponseLayer};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tokio::task::JoinHandle;
use tower_http::{
    compression::CompressionLayer, decompression::RequestDecompressionLayer,
    sensitive_headers::SetSensitiveRequestHeadersLayer, timeout::TimeoutLayer,
    validate_request::ValidateRequestHeaderLayer,
};

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

pub(crate) fn launch(config: &Config, routes: Vec<Router>) -> JoinHandle<Result<()>> {
    let addr = SocketAddr::new(config.host, config.port);
    tokio::spawn(async move {
        let app = app(routes);
        // run it
        tracing::warn!("listening on {}", addr);
        let listener = tokio::net::TcpListener::bind(addr).await.into_diagnostic()?;
        axum::serve(listener, app.into_make_service())
            // see [axum/examples/graceful-shutdown/src/main.rs at main · tokio-rs/axum](https://github.com/tokio-rs/axum/blob/main/examples/graceful-shutdown/src/main.rs)
            // TODO check graceful shutdown with spawned task & integration with main
            .with_graceful_shutdown(shutdown_signal())
            .await.into_diagnostic()?;
        Ok(())
    })
}

#[allow(clippy::expect_used)]
#[allow(clippy::ignored_unit_patterns)]
async fn shutdown_signal() {
    use tokio::signal;
    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

//TODO make route per extractor/sources
fn app(routes: Vec<Router>) -> Router {
    // build our application with a route
    let mut app = Router::new();
    for route in routes {
        app = app.merge(route);
    }
    app
        // include trace context as header into the response
        .layer(OtelInResponseLayer)
        //start OpenTelemetry trace on incoming request
        .layer(OtelAxumLayer::default())
        .route("/healthz", get(health)) // request processed without span / trace
        .route("/readyz", get(health)) // request processed without span / trace
        .layer((
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

impl IntoResponse for Error {
    //TODO report the trace_id into the message to help to debug
    fn into_response(self) -> axum::response::Response {
        // let (status, error_message) = match self {
        //     Error::Db(e) => (http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        //     _ => (http::StatusCode::INTERNAL_SERVER_ERROR, "".to_string()),
        // };
        tracing::warn!(error = ?self);
        let body = Json(json!({
            "error": "",
        }));
        (http::StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
    }
}

impl IntoResponse for ReportWrapper {
    //TODO report the trace_id into the message to help to debug
    fn into_response(self) -> axum::response::Response {
        tracing::warn!(error = ?self);
        let body = Json(json!({
            "error": "",
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
