use crate::errors::{self, Error};
use axum::{http, response::IntoResponse, routing::get, Json, Router};
use axum_tracing_opentelemetry::middleware::{OtelAxumLayer, OtelInResponseLayer};
use errors::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::{IpAddr, SocketAddr};
use tokio::task::JoinHandle;

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
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app.into_make_service())
            //FIXME gracefull shutdown is in wip for axum 0.7
            // see [axum/examples/graceful-shutdown/src/main.rs at main · tokio-rs/axum](https://github.com/tokio-rs/axum/blob/main/examples/graceful-shutdown/src/main.rs)
            // .with_graceful_shutdown(shutdown_signal())
            .await?;
        Ok(())
    })
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
        let (status, error_message) = (http::StatusCode::INTERNAL_SERVER_ERROR, String::new());
        tracing::warn!(?error_message);
        let body = Json(json!({
            "error": error_message,
        }));

        (status, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum_test::{TestServer, TestServerBuilder};
    use rstest::*;
    use serde_json::json;

    struct TestContext {
        http: TestServer,
        // keep tracing subscriber
        _tracing_guard: tracing::subscriber::DefaultGuard,
    }

    // #[fixture]
    // //#[once] // only work with non-async, non generic fixtures
    // // workaround explained at [Async once fixtures · Issue #141 · la10736/rstest](https://github.com/la10736/rstest/issues/141)
    // // no drop call on the fixture like on static
    // fn pg() -> (PgPool, Container<Postgres>) {
    //     futures::executor::block_on(async { async_pg().await })
    // }

    // servers() is called once per test, so db could only started several times.
    // We could not used `static` (or the once on fixtures) because statis are not dropped at end of the test
    #[fixture]
    async fn testcontext() -> TestContext {
        let subscriber = tracing_subscriber::FmtSubscriber::builder()
            .with_max_level(tracing::Level::WARN)
            .finish();
        let tracing_guard = tracing::subscriber::set_default(subscriber);

        let app = app(vec![]);

        let config = TestServerBuilder::new()
            // Preserve cookies across requests
            // for the session cookie to work.
            .save_cookies()
            .default_content_type("application/json")
            .mock_transport()
            .into_config();

        TestContext {
            http: TestServer::new_with_config(app, config).unwrap(),
            _tracing_guard: tracing_guard,
        }
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    // test health endpoint
    async fn test_readyz(#[future] testcontext: TestContext) {
        let resp = testcontext.await.http.get("/readyz").await;
        resp.assert_status_ok();
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_post_webhook_not_found(#[future] testcontext: TestContext) {
        let resp = testcontext
            .await
            .http
            .post("/webhook/test")
            .json(&json!({
                "bar": "foo",
            }))
            .await;
        resp.assert_text("");
        resp.assert_status(http::StatusCode::NOT_FOUND);
    }
}
