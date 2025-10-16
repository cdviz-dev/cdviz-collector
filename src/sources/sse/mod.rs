pub mod config;

pub use config::Config;

use crate::errors::Result;
#[cfg(test)]
use crate::security::rule::HeaderRuleMap;
use crate::sources::{EventSource, EventSourcePipe};
use futures::StreamExt;
use reqwest_eventsource::{Event, RequestBuilderExt};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

// SSE extractor that follows the same pattern as `OpendalExtractor`
pub(crate) struct SseExtractor {
    config: Config,
    next: EventSourcePipe,
}

impl SseExtractor {
    /// Create a new SSE extractor from configuration
    pub(crate) fn from(config: &Config, next: EventSourcePipe) -> Self {
        Self { config: config.clone(), next }
    }

    /// Run the SSE extractor, handling the SSE connection and event forwarding
    #[allow(clippy::ignored_unit_patterns)]
    pub(crate) async fn run(self, cancel_token: CancellationToken) -> Result<()> {
        let sse_task = create_sse_source(self.config, self.next);

        tokio::select! {
            _ = cancel_token.cancelled() => {
                // Clean cancellation - the task will be dropped
            }
            result = sse_task => {
                if let Err(e) = result {
                    error!("SSE task failed: {}", e);
                }
            }
        }

        Ok(())
    }
}

pub struct SseSourceState {
    pub config: Config,
    pub next: EventSourcePipe,
}

impl SseSourceState {
    pub fn new(config: Config, next: EventSourcePipe) -> Self {
        Self { config, next }
    }

    pub async fn run(mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut retry_count = 0;
        let max_retries = self.config.max_retries.unwrap_or(10);

        loop {
            match self.connect_and_stream().await {
                Ok(()) => {
                    info!("SSE source disconnected gracefully");
                    break;
                }
                Err(e) => {
                    error!("SSE source error: {}", e);
                    retry_count += 1;

                    if retry_count >= max_retries {
                        error!("Max retries ({}) exceeded, stopping SSE source", max_retries);
                        break;
                    }

                    let delay = Duration::from_secs(2_u64.pow(retry_count.min(6)));
                    warn!(
                        "Retrying SSE connection in {:?} (attempt {}/{})",
                        delay, retry_count, max_retries
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }

        Ok(())
    }

    async fn connect_and_stream(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("Connecting to SSE endpoint: {}", self.config.url);

        let mut request_builder = reqwest::Client::new().get(&self.config.url);

        // Generate and add configured headers
        match crate::security::header::generate_headers(&self.config.headers_as_configs(), None) {
            Ok(headers) => {
                request_builder = request_builder.headers(headers);
            }
            Err(e) => {
                error!("Failed to generate headers: {}", e);
                return Err(e.into());
            }
        }

        let mut event_source = request_builder.eventsource()?;

        while let Some(event) = event_source.next().await {
            match event {
                Ok(Event::Open) => {
                    info!("SSE connection opened");
                }
                Ok(Event::Message(message)) => {
                    debug!(
                        "Received SSE message: id={:?}, event_type={:?}",
                        message.id, message.event
                    );

                    // Parse the SSE data as JSON for the body
                    let body = match serde_json::from_str(&message.data) {
                        Ok(json_value) => json_value,
                        Err(_) => {
                            // If parsing fails, wrap the raw data as a string value
                            serde_json::Value::String(message.data)
                        }
                    };

                    // Merge base_metadata with SSE-specific metadata
                    let mut metadata = self.config.metadata.clone();
                    if let Some(obj) = metadata.as_object_mut() {
                        let message_id = if message.id.is_empty() {
                            uuid::Uuid::new_v4().to_string()
                        } else {
                            message.id
                        };
                        let message_event = if message.event.is_empty() {
                            "message".to_string()
                        } else {
                            message.event
                        };

                        obj.insert("sse_id".to_string(), serde_json::Value::String(message_id));
                        obj.insert(
                            "sse_event".to_string(),
                            serde_json::Value::String(message_event),
                        );
                        obj.insert(
                            "sse_url".to_string(),
                            serde_json::Value::String(self.config.url.clone()),
                        );
                    }

                    let event_source =
                        EventSource { metadata, headers: std::collections::HashMap::new(), body };

                    if let Err(e) = self.next.send(event_source) {
                        error!("Failed to send event to pipeline: {}", e);
                        break;
                    }
                }
                Err(e) => {
                    error!("SSE error: {}", e);
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }
}

pub fn create_sse_source(
    config: Config,
    next: EventSourcePipe,
) -> tokio::task::JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>> {
    let state = SseSourceState::new(config, next);
    tokio::spawn(async move { state.run().await })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert_eq!(config.url, "http://localhost:8080/sse/001");
        assert!(config.headers.is_empty());
        assert_eq!(config.max_retries, Some(10));
        assert!(config.enabled);
    }

    #[test]
    fn test_config_serialization() {
        use crate::security::header::{HeaderSource, OutgoingHeaderMap};

        let config = Config {
            url: "https://example.com/events".to_string(),
            headers: {
                let mut map = OutgoingHeaderMap::new();
                map.insert(
                    "Authorization".to_string(),
                    HeaderSource::Static { value: "Bearer token".to_string() },
                );
                map
            },
            max_retries: Some(5),
            enabled: true,
            metadata: serde_json::json!({}),
        };

        let serialized = toml::to_string(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();

        assert_eq!(config.url, deserialized.url);
        assert_eq!(config.headers.len(), deserialized.headers.len());
        assert_eq!(config.max_retries, deserialized.max_retries);
        assert_eq!(config.enabled, deserialized.enabled);
    }

    #[test]
    fn test_sse_source_state_creation() {
        use crate::pipes::collect_to_vec::Collector;

        let config = Config::default();
        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());

        let state = SseSourceState::new(config.clone(), pipe);
        assert_eq!(state.config.url, config.url);
        assert_eq!(state.config.max_retries, config.max_retries);
    }

    #[tokio::test]
    async fn test_sse_source_connection_failure() {
        use crate::pipes::collect_to_vec::Collector;

        // Test that SSE source handles connection failures gracefully
        let config = Config {
            url: "http://localhost:99999/nonexistent".to_string(), // Use a port that's unlikely to be in use
            max_retries: Some(1),                                  // Limit retries for test speed
            ..Default::default()
        };

        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let state = SseSourceState::new(config, pipe);

        // The connection should fail quickly and the task should complete
        let handle = tokio::spawn(async move { state.run().await });

        // Give it a brief moment to attempt connection and fail
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

        assert!(result.is_ok(), "Task should complete within timeout");

        // Check that no events were collected
        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert!(events.is_empty(), "Should not receive any events due to connection failure");
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::pipes::collect_to_vec::Collector;
    use crate::security::header::OutgoingHeaderMap;
    use crate::sinks::sse as sse_sink;
    use axum::Router;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio::time::timeout;

    async fn setup_test_sse_sink() -> (String, tokio::task::JoinHandle<()>) {
        // Find an available port
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();

        // Create SSE sink
        let sse_sink = sse_sink::SseSink::new("test-sse".to_string(), HeaderRuleMap::new());
        let routes = sse_sink.make_route();

        // Create HTTP server with SSE routes
        let app = Router::new().merge(routes);

        // Start the server
        let server_handle = tokio::spawn(async move {
            let server = axum::serve(listener, app);
            let _ = server.await;
        });

        // Wait a bit for server to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        let server_url = format!("http://127.0.0.1:{port}/sse/test-sse");

        (server_url, server_handle)
    }

    #[tokio::test]
    async fn test_sse_source_sink_integration() {
        // Setup SSE sink as test server
        let (server_url, _server_handle) = setup_test_sse_sink().await;

        // Create SSE source config pointing to the sink
        let source_config = Config {
            url: server_url,
            headers: OutgoingHeaderMap::new(),
            max_retries: Some(3),
            enabled: true,
            metadata: serde_json::json!({}),
        };

        // Setup SSE source
        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let source_handle = create_sse_source(source_config, pipe);

        // Test basic connection - source should connect to sink successfully
        // Give it time to establish connection
        tokio::time::sleep(Duration::from_millis(500)).await;

        // The source should be running (not immediately fail)
        assert!(!source_handle.is_finished());

        // Clean up
        source_handle.abort();
    }

    #[tokio::test]
    async fn test_sse_source_connection_error() {
        // Test with a URL that will return an error quickly
        let source_config = Config {
            url: "http://localhost:99999/nonexistent".to_string(),
            headers: OutgoingHeaderMap::new(),
            max_retries: Some(1),
            enabled: true,
            metadata: serde_json::json!({}),
        };

        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let source_handle = create_sse_source(source_config, pipe);

        // Should complete quickly with connection error
        let result = timeout(Duration::from_secs(5), source_handle).await;

        // Should complete (with error, but complete nonetheless)
        assert!(result.is_ok());

        // Should not have received any events
        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert!(events.is_empty(), "Should not receive any events due to connection failure");
    }

    #[tokio::test]
    async fn test_toml_integration_complete_headers_config() {
        use axum::http::{HeaderMap, HeaderName, HeaderValue};
        use indoc::indoc;
        use secrecy::ExposeSecret;

        #[derive(serde::Deserialize)]
        struct MixedHeaderConfig {
            outgoing_headers: Vec<crate::security::header::OutgoingHeaderConfig>,
            validation_headers: Vec<crate::security::rule::HeaderRuleConfig>,
        }

        // Integration test showing both incoming validation and outgoing generation
        // header configs as they would appear in a realistic setup
        let toml_str = indoc! {r#"
            # Outgoing headers configuration for SSE source
            [[outgoing_headers]]
            header = "Authorization"

            [outgoing_headers.rule]
            type = "static"
            value = "Bearer sse-token"

            [[outgoing_headers]]
            header = "X-API-Key"

            [outgoing_headers.rule]
            type = "secret"
            value = "secret-api-key"

            # Validation headers configuration (would be used for incoming webhooks)
            [[validation_headers]]
            header = "X-Hub-Signature-256"

            [validation_headers.rule]
            type = "signature"
            token = "webhook-secret"
            signature_prefix = "sha256="

            [[validation_headers]]
            header = "Content-Type"

            [validation_headers.rule]
            type = "equals"
            value = "application/json"
            case_sensitive = false
        "#};

        let config: MixedHeaderConfig = toml::from_str(toml_str).unwrap();

        // Validate outgoing headers
        assert_eq!(config.outgoing_headers.len(), 2);

        assert_eq!(config.outgoing_headers[0].header, "Authorization");
        match &config.outgoing_headers[0].rule {
            crate::security::header::HeaderSource::Static { value } => {
                assert_eq!(value, "Bearer sse-token");
            }
            _ => panic!("Expected static header"),
        }

        assert_eq!(config.outgoing_headers[1].header, "X-API-Key");
        match &config.outgoing_headers[1].rule {
            crate::security::header::HeaderSource::Secret { value } => {
                assert_eq!(value.expose_secret(), "secret-api-key");
            }
            _ => panic!("Expected secret header"),
        }

        // Validate validation headers
        assert_eq!(config.validation_headers.len(), 2);

        assert_eq!(config.validation_headers[0].header, "X-Hub-Signature-256");
        match &config.validation_headers[0].rule {
            crate::security::rule::Rule::Signature { signature_prefix, .. } => {
                assert_eq!(signature_prefix, &Some("sha256=".to_string()));
            }
            _ => panic!("Expected signature rule"),
        }

        assert_eq!(config.validation_headers[1].header, "Content-Type");
        match &config.validation_headers[1].rule {
            crate::security::rule::Rule::Equals { value, case_sensitive } => {
                assert_eq!(value, "application/json");
                assert!(!case_sensitive);
            }
            _ => panic!("Expected equals rule"),
        }

        // Test header generation functionality
        let generated_headers =
            crate::security::header::generate_headers(&config.outgoing_headers, None).unwrap();
        assert_eq!(generated_headers.len(), 2);
        assert_eq!(generated_headers.get("Authorization").unwrap(), "Bearer sse-token");
        assert_eq!(generated_headers.get("X-API-Key").unwrap(), "secret-api-key");

        // Test header validation functionality (would be used on incoming requests)
        let mut test_headers = HeaderMap::new();
        test_headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );

        let result = crate::security::rule::validate_header(
            &test_headers,
            &config.validation_headers[1], // Content-Type rule
            None,
        );
        assert!(result.is_ok(), "Content-Type validation should pass");
    }
}

#[cfg(test)]
mod unit_tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn test_header_config() {
        use crate::security::header::HeaderSource;

        let header_source = HeaderSource::Static { value: "Bearer test-token".to_string() };

        match header_source {
            HeaderSource::Static { value } => assert_eq!(value, "Bearer test-token"),
            _ => panic!("Expected static header source"),
        }
    }

    #[test]
    fn test_toml_integration_sse_source_with_headers() {
        use indoc::indoc;
        use secrecy::ExposeSecret;

        // Comprehensive test showing a realistic SSE source configuration
        let toml_str = indoc! {r#"
            url = "https://events.example.com/stream"
            enabled = true
            max_retries = 5

            # Headers to include in outgoing SSE requests - new map format
            [headers]
            "Authorization" = { type = "static", value = "Bearer api-token-12345" }
            "X-API-Key" = { type = "secret", value = "my-secret-api-key" }
            "X-Request-Signature" = { type = "signature", token = "webhook-signing-secret", signature_prefix = "sha256=", signature_on = "body", signature_encoding = "hex" }
            "User-Agent" = { type = "static", value = "cdviz-collector/1.0" }
        "#};

        let config: Config = toml::from_str(toml_str).unwrap();

        assert_eq!(config.url, "https://events.example.com/stream");
        assert!(config.enabled);
        assert_eq!(config.max_retries, Some(5));
        assert_eq!(config.headers.len(), 4);

        // Validate Authorization header (static)
        match config.headers.get("Authorization").unwrap() {
            crate::security::header::HeaderSource::Static { value } => {
                assert_eq!(value, "Bearer api-token-12345");
            }
            _ => panic!("Expected static header for Authorization"),
        }

        // Validate X-API-Key header (secret)
        match config.headers.get("X-API-Key").unwrap() {
            crate::security::header::HeaderSource::Secret { value } => {
                assert_eq!(value.expose_secret(), "my-secret-api-key");
            }
            _ => panic!("Expected secret header for X-API-Key"),
        }

        // Validate X-Request-Signature header (signature)
        match config.headers.get("X-Request-Signature").unwrap() {
            crate::security::header::HeaderSource::Signature {
                token,
                signature_prefix,
                signature_on,
                signature_encoding,
                ..
            } => {
                assert_eq!(token.expose_secret(), "webhook-signing-secret");
                assert_eq!(signature_prefix, &Some("sha256=".to_string()));
                assert_eq!(*signature_on, crate::security::signature::SignatureOn::Body);
                assert_eq!(*signature_encoding, crate::security::signature::Encoding::Hex);
            }
            _ => panic!("Expected signature header for X-Request-Signature"),
        }

        // Validate User-Agent header (static)
        match config.headers.get("User-Agent").unwrap() {
            crate::security::header::HeaderSource::Static { value } => {
                assert_eq!(value, "cdviz-collector/1.0");
            }
            _ => panic!("Expected static header for User-Agent"),
        }
    }

    #[tokio::test]
    async fn test_sse_extractor_cancellation() {
        use crate::pipes::collect_to_vec::Collector;
        use crate::sources::EventSource;
        use tokio::time::{Duration, timeout};
        use tokio_util::sync::CancellationToken;

        // Use an unreachable URL to ensure connection fails quickly
        let config = Config {
            url: "http://localhost:99999/nonexistent".to_string(),
            max_retries: Some(1),
            ..Default::default()
        };

        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let cancel_token = CancellationToken::new();

        let extractor = SseExtractor::from(&config, pipe);

        // Cancel immediately to test cancellation works
        cancel_token.cancel();

        // The extractor should complete quickly due to cancellation
        let result = timeout(Duration::from_millis(500), extractor.run(cancel_token)).await;
        assert!(result.is_ok(), "Extractor should complete within timeout when cancelled");
    }
}
