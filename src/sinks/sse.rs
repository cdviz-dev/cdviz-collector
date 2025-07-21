use super::Sink;
use crate::Message;
use crate::errors::Result;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::response::sse::{Event, Sse};
use axum::routing::{Router, get};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Config {
    /// Is the sink enabled?
    pub(crate) enabled: bool,
    /// ID of the SSE endpoint, used to define the path of the SSE URL (`/sse/{id}`)
    pub(crate) id: String,
}

impl TryFrom<Config> for SseSink {
    type Error = crate::errors::Report;

    fn try_from(value: Config) -> Result<Self> {
        Ok(SseSink::new(value.id))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SseSink {
    id: String,
    tx: broadcast::Sender<Message>,
}

impl SseSink {
    pub(crate) fn new(id: String) -> Self {
        let (tx, _) = broadcast::channel(1000); // Buffer up to 1000 messages
        Self { id, tx }
    }

    pub(crate) fn make_route(&self) -> Router {
        let state = SseState { tx: self.tx.clone() };
        Router::new().route(&format!("/sse/{}", self.id), get(sse_handler)).with_state(state)
    }
}

#[derive(Clone)]
struct SseState {
    tx: broadcast::Sender<Message>,
}

impl Sink for SseSink {
    async fn send(&self, msg: &Message) -> Result<()> {
        // Send message to all connected SSE clients
        if let Err(err) = self.tx.send(msg.clone()) {
            // If no receivers are connected, that's ok
            if !matches!(err, broadcast::error::SendError(_)) {
                tracing::warn!(
                    sse_id = self.id,
                    event_id = msg.cdevent.id().as_str(),
                    error = ?err,
                    "Failed to broadcast SSE message"
                );
            }
        }
        Ok(())
    }

    fn get_routes(&self) -> Option<axum::Router> {
        Some(self.make_route())
    }
}

#[tracing::instrument(skip(state))]
async fn sse_handler(State(state): State<SseState>) -> impl IntoResponse {
    tracing::debug!("New SSE client connected");

    let rx = state.tx.subscribe();
    let stream = BroadcastStream::new(rx).map(|result| {
        use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
        match result {
            Ok(msg) => {
                // Convert Message to SSE Event
                match serde_json::to_string(&msg.cdevent) {
                    Ok(data) => Result::<Event, std::convert::Infallible>::Ok(
                        Event::default().event("cdevent").id(msg.cdevent.id().as_str()).data(data),
                    ),
                    Err(err) => {
                        tracing::warn!(error = ?err, "Failed to serialize CDEvent for SSE");
                        Result::<Event, std::convert::Infallible>::Ok(
                            Event::default()
                                .event("error")
                                .data(format!("serialization error: {err}")),
                        )
                    }
                }
            }
            Err(BroadcastStreamRecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, "SSE client lagged, skipped messages");
                Result::<Event, std::convert::Infallible>::Ok(
                    Event::default()
                        .event("error")
                        .data(format!("lagged: {skipped} messages skipped")),
                )
            }
        }
    });

    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(std::time::Duration::from_secs(30))
            .text("keep-alive"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode, header::ACCEPT},
    };
    use test_strategy::proptest;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_sse_endpoint_exists() {
        let config = Config { enabled: true, id: "test".to_string() };
        let sink = SseSink::try_from(config).unwrap();
        let router = sink.make_route();

        let request = Request::builder()
            .uri("/sse/test")
            .method("GET")
            .header(ACCEPT, "text/event-stream")
            .body(Body::empty())
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get("content-type").unwrap(), "text/event-stream");
    }

    #[tokio::test]
    async fn test_sse_wrong_path() {
        let config = Config { enabled: true, id: "test".to_string() };
        let sink = SseSink::try_from(config).unwrap();
        let router = sink.make_route();

        let request = Request::builder()
            .uri("/sse/wrong")
            .method("GET")
            .header(ACCEPT, "text/event-stream")
            .body(Body::empty())
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_sse_wrong_method() {
        let config = Config { enabled: true, id: "test".to_string() };
        let sink = SseSink::try_from(config).unwrap();
        let router = sink.make_route();

        let request = Request::builder()
            .uri("/sse/test")
            .method("POST")
            .header(ACCEPT, "text/event-stream")
            .body(Body::empty())
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[proptest(async = "tokio")]
    async fn test_sse_sink_send(msg: Message) {
        let config = Config { enabled: true, id: "test".to_string() };
        let sink = SseSink::try_from(config).unwrap();

        // Should not fail even if no clients are connected
        let result = sink.send(&msg).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_sse_sink_creation() {
        let config = Config { enabled: true, id: "my-sse-endpoint".to_string() };

        let sink = SseSink::try_from(config.clone()).unwrap();
        assert_eq!(sink.id, config.id);

        // Verify route is created with correct path
        let router = sink.make_route();
        let request = Request::builder()
            .uri("/sse/my-sse-endpoint")
            .method("GET")
            .header(ACCEPT, "text/event-stream")
            .body(Body::empty())
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use proptest::arbitrary::any;
    use proptest::strategy::Strategy;
    use proptest::test_runner::TestRunner;
    use std::net::SocketAddr;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn test_sse_message_broadcast() {
        let config = Config { enabled: true, id: "broadcast-test".to_string() };
        let sink = SseSink::try_from(config).unwrap();

        // Create two test messages
        let mut runner = TestRunner::default();
        let msgs = vec![
            any::<Message>().new_tree(&mut runner).unwrap().current(),
            any::<Message>().new_tree(&mut runner).unwrap().current(),
            any::<Message>().new_tree(&mut runner).unwrap().current(),
        ];
        let msgs_count = msgs.len();

        // Start a test HTTP server
        let router = sink.make_route();
        let addr = SocketAddr::from(([127, 0, 0, 1], 0)); // Use port 0 for dynamic allocation
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let server_addr = listener.local_addr().unwrap();

        // Spawn the server
        let server_handle =
            tokio::spawn(async move { axum::serve(listener, router.into_make_service()).await });

        // Wait a bit for server to start
        tokio::time::sleep(Duration::from_millis(10)).await;

        let sse_url = format!("http://{server_addr}/sse/broadcast-test");

        // Start streaming in background first
        let stream_handle = tokio::spawn(async move {
            use reqwest_eventsource::{Event, EventSource};

            // Connect to SSE endpoint
            let mut es = EventSource::get(sse_url);
            let mut received_events = Vec::new();

            // Read SSE events from the stream with timeout
            while received_events.len() < msgs_count {
                match timeout(Duration::from_secs(5), es.next()).await {
                    Ok(Some(Ok(Event::Open))) => {
                        // Connection opened - no action needed
                    }
                    Ok(Some(Ok(Event::Message(message)))) => {
                        // Only process 'cdevent' type messages, ignore keep-alive
                        if message.event == "cdevent" {
                            if let Ok(event_data) =
                                serde_json::from_str::<serde_json::Value>(&message.data)
                            {
                                received_events.push((
                                    message.event.clone(),
                                    message.id.clone(),
                                    event_data,
                                ));
                            }
                        }
                    }
                    Ok(Some(Err(err))) => {
                        tracing::warn!(error = ?err, "SSE Error during test");
                        break;
                    }
                    Ok(None) | Err(_) => break, // Stream ended or timeout
                }
            }

            received_events
        });

        // Wait longer for client to connect before sending messages
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Send messages with delays to ensure they are processed separately
        for msg in &msgs {
            tokio::time::sleep(Duration::from_millis(50)).await;
            sink.send(msg).await.unwrap();
        }

        // Wait for the streaming to complete and get results
        let received_events = stream_handle.await.unwrap();

        // Cleanup server
        server_handle.abort();

        // Verify we received exactly 2 SSE events
        assert_eq!(
            received_events.len(),
            msgs_count,
            "Should receive same number of SSE events as sent messages"
        );

        // Verify the events have correct format and order
        for i in 0..msgs_count {
            let (event_type, event_id, event_data) = &received_events[i];
            let expected_event = msgs[i].cdevent.clone();
            assert_eq!(event_type, "cdevent", "Event should have 'cdevent' type");
            // Verify the event IDs match the CDEvent IDs in the correct order
            assert_eq!(event_id, expected_event.id().as_str());
            // Verify the event data matches the expected CDEvent JSON in the correct order
            let expected_event_json = serde_json::to_value(&expected_event).unwrap();
            assert_eq!(event_data, &expected_event_json,);
        }
    }
}
