use super::EventSourcePipe;
use crate::errors::ReportWrapper;
use crate::security::signature;
use crate::sources::EventSource;
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName};
use axum::response::IntoResponse;
use axum::routing::{Router, post};
use futures::lock::Mutex;
use serde::Deserialize;
use std::str::FromStr;
use std::sync::Arc;

/// The webhook config
#[derive(Clone, Debug, Deserialize, Default)]
pub(crate) struct Config {
    /// id of the webhook, used to define the path of the webhook's url (`/webhooks/{id}`)
    pub(crate) id: String,
    /// HTTP headers to forward into the pipeline
    #[serde(default)]
    pub(crate) headers_to_keep: Vec<String>,
    /// Verify the incoming request and the signature
    #[serde(default)]
    pub(crate) signature: Option<signature::SignatureConfig>,
}

#[derive(Clone)]
struct WebhookState {
    next: Arc<Mutex<EventSourcePipe>>,
    headers_to_keep: Vec<HeaderName>,
    signature: Option<signature::SignatureConfig>,
}

pub(crate) fn make_route(config: &Config, next: EventSourcePipe) -> Router {
    let state = WebhookState {
        next: Arc::new(Mutex::new(next)),
        headers_to_keep: config
            .headers_to_keep
            .iter()
            .filter_map(|name| HeaderName::from_str(name.as_str()).ok())
            .collect(),
        signature: config.signature.clone(),
    };
    Router::new().route(&format!("/webhook/{}", config.id), post(webhook)).with_state(state)
}

//TODO support events in cloudevents format (extract info from headers)
//TODO try [deser](https://crates.io/crates/deserr) to return good error
//TODO use cloudevents
//TODO add metadata & headers info into SourceEvent
//TODO log & convert error
#[tracing::instrument(skip(state, headers, body))]
async fn webhook(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    //tracing::trace!(?body, "received");
    //TODO replace by a middleware
    if let Some(signature) = state.signature.as_ref() {
        if let Err(err) = signature::check_signature(signature, &headers, &body) {
            return err.into_response();
        }
    }
    let maybe_json = Json::from_bytes(&body);
    if let Err(err) = maybe_json {
        return err.into_response();
    }
    let Json(body): Json<serde_json::Value> = maybe_json.unwrap_or_default();
    let header = header_to_map(&headers, &state.headers_to_keep);
    let event = EventSource { header, body, ..Default::default() };
    if let Err(err) = state.next.lock().await.send(event) {
        return ReportWrapper::from(err).into_response();
    }
    (axum::http::StatusCode::CREATED).into_response()
}

/// Convert a header map to a map of header name to header value
/// The output map will only contain headers that are in the `headers_to_keep` list
/// and that are not sensitive.
#[allow(clippy::min_ident_chars)]
fn header_to_map(
    headers: &HeaderMap,
    headers_to_keep: &[HeaderName],
) -> std::collections::HashMap<String, String> {
    headers
        .iter()
        .filter(|(_, v)| !v.is_sensitive())
        .filter(|(k, _)| headers_to_keep.contains(k))
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), v.to_string())))
        .collect()
}

#[cfg(test)]
mod tests_handler {
    use crate::pipes::collect_to_vec;

    use super::*;
    use assert2::let_assert;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_webhook_success() {
        let config = Config {
            id: "test".to_string(),
            headers_to_keep: vec!["Content-Type".to_string()],
            ..Default::default()
        };
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let router = make_route(&config, Box::new(collector.create_pipe()));

        let payload = json!({"key": "value"});
        let request = Request::builder()
            .uri("/webhook/test")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let_assert!(Some(output) = collector.try_into_iter().unwrap().next());
        assert_eq!(output.body, payload);
        assert_eq!(output.header.get("content-type").unwrap(), "application/json");
    }

    #[tokio::test]
    async fn test_webhook_invalid_path() {
        let config = Config { id: "test".to_string(), ..Default::default() };
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let router = make_route(&config, Box::new(collector.create_pipe()));

        let request = Request::builder()
            .uri("/webhook/invalid")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(json!({"key": "value"}).to_string()))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let_assert!(None = collector.try_into_iter().unwrap().next());
    }

    #[tokio::test]
    async fn test_webhook_invalid_method() {
        let config = Config { id: "test".to_string(), ..Default::default() };
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let router = make_route(&config, Box::new(collector.create_pipe()));

        let request = Request::builder()
            .uri("/webhook/test")
            .method("GET")
            .header("content-type", "application/json")
            .body(Body::from(json!({"key": "value"}).to_string()))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        let_assert!(None = collector.try_into_iter().unwrap().next());
    }
}

#[cfg(test)]
mod tests_headers_conversion {
    use std::collections::HashMap;

    use axum::http::{HeaderMap, HeaderValue};
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_header_to_map_with_valid_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));
        headers.insert("X-Custom-Header", HeaderValue::from_static("custom_value"));

        let headers_to_keep = vec![
            HeaderName::from_str("Content-Type").unwrap(),
            HeaderName::from_str("X-Custom-Header").unwrap(),
        ];

        let result = header_to_map(&headers, &headers_to_keep);
        let mut expected = HashMap::new();
        expected.insert("Content-Type".to_lowercase(), "application/json".to_string());
        expected.insert("X-Custom-Header".to_lowercase(), "custom_value".to_string());

        assert_eq!(result, expected);
    }

    #[test]
    fn test_header_to_map_with_sensitive_headers() {
        let mut headers = HeaderMap::new();
        let mut authorization = HeaderValue::from_static("Bearer token");
        authorization.set_sensitive(true);
        headers.insert("Authorization", authorization);
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));

        let headers_to_keep = vec![
            HeaderName::from_str("Authorization").unwrap(),
            HeaderName::from_str("Content-Type").unwrap(),
        ];

        let result = header_to_map(&headers, &headers_to_keep);
        let mut expected = HashMap::new();
        expected.insert("Content-Type".to_lowercase(), "application/json".to_string());

        assert_eq!(result, expected);
    }

    #[test]
    fn test_header_to_map_with_empty_headers() {
        let headers = HeaderMap::new();
        let headers_to_keep = vec![
            HeaderName::from_str("Content-Type").unwrap(),
            HeaderName::from_str("X-Custom-Header").unwrap(),
        ];

        let result = header_to_map(&headers, &headers_to_keep);
        let expected: HashMap<String, String> = HashMap::new();

        assert_eq!(result, expected);
    }

    #[test]
    fn test_header_to_map_with_non_matching_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));

        let headers_to_keep = vec![HeaderName::from_str("X-Custom-Header").unwrap()];

        let result = header_to_map(&headers, &headers_to_keep);
        let expected: HashMap<String, String> = HashMap::new();

        assert_eq!(result, expected);
    }
}
