#![allow(clippy::unwrap_used)]

use miette::Result;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, header_exists, header_regex, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn lauch_collector(
    temp_dir: &TempDir,
    collector_port: u16,
    sink_url: &str,
    //) -> tokio::process::Child {
) -> (JoinHandle<Result<bool>>, CancellationToken) {
    let config_path = temp_dir.path().join("test-config.toml");

    // Configuration for cdviz-collector with webhook source and HTTP sink
    let config_content = format!(
        r#"
[http]
host = "0.0.0.0"
port = {collector_port}

[sources.test_webhook]
enabled = true

[sources.test_webhook.extractor]
type = "webhook"
id = "test"
headers_to_keep = ["x-myheader-to-keep"]

[sinks.http_sink]
enabled = true
type = "http"
destination = "{sink_url}/events"
"#
    );

    std::fs::write(&config_path, config_content).unwrap();

    // // blocking: build the application (could take a while ~ 20s-60s)
    // // use test profile, like the current running test, to reduce (re) building
    // let _ = std::process::Command::new("cargo")
    //     .args(&["build", "--profile", "test"])
    //     .current_dir(env!("CARGO_MANIFEST_DIR"))
    //     .spawn()
    //     .expect("Failed to build cdviz-collector")
    //     .wait()
    //     .expect("Failed to build cdviz-collector");

    // // Start cdviz-collector in the background
    // let mut cmd = tokio::process::Command::new(format!("{}/target/debug/cdviz-collector", env!("CARGO_MANIFEST_DIR")))
    //     .args(&["connect", "--config"])
    //     .arg(&config_path)
    //     //.stdout(Stdio::piped())
    //     //.stderr(Stdio::piped())
    //     .spawn()
    //     .expect("Failed to start cdviz-collector");

    // launch collector as library to avoid (re) building it
    let shutdown_cancel = CancellationToken::new();
    let shutdown_cancel_signal = shutdown_cancel.clone();
    let task = tokio::spawn(async move {
        cdviz_collector::run_with_args_and_log(
            vec!["connect", "--config", &config_path.to_string_lossy()],
            true,
            shutdown_cancel,
        )
        .await
    });

    // Wait for collector to start (server takes time to initialize)
    sleep(Duration::from_secs(1)).await;

    // // Verify the process is still running
    // match cmd.try_wait() {
    //     Ok(Some(status)) => {
    //         panic!("cdviz-collector exited unexpectedly with status: {}", status);
    //     }
    //     Ok(None) => {
    //         // Process is still running, which is expected for server mode
    //     }
    //     Err(e) => {
    //         panic!("Error checking process status: {}", e);
    //     }
    // }

    // Wait for the HTTP server to be ready by trying the webhook endpoint
    let health_client = reqwest::Client::new();
    let mut attempts = 0;
    while attempts < 10 {
        // Try a simple GET to see if server is responding (will likely 404 but shows server is up)
        if health_client
            .get(format!("http://127.0.0.1:{collector_port}/healthz"))
            .send()
            .await
            .is_ok()
        {
            break;
        }
        sleep(Duration::from_millis(500)).await;
        attempts += 1;
    }

    // cmd
    (task, shutdown_cancel_signal)
}

/// Integration test for webhook source -> HTTP sink pipeline
/// Tests `CDEvent` passthrough and `trace_id` propagation behavior
#[tokio::test(flavor = "multi_thread")]
async fn test_webhook_to_http_sink_with_trace_propagation() {
    // Setup mock HTTP sink server
    let mock_server = MockServer::start().await;
    let sink_url = mock_server.uri();

    // Test case 1: Request with W3C trace context
    let trace_id = "4bf92f3577b34da6a3ce929d0e0e4736";
    let span_id = "00f067aa0ba902b7";
    // https://www.w3.org/TR/trace-context/#traceparent-header-field-values
    let traceparent = format!("00-{trace_id}-{span_id}-01");
    // the last part is the trace flags, it could be 01 or 00 (sampled or not)
    let traceparent_pattern = format!("00-{trace_id}-([0-9a-z]+)-([0-9a-z]+)");

    // Setup mock expectation for HTTP sink - expect trace propagation
    let mock_with_trace = Mock::given(method("POST"))
        .and(path("/events"))
        .and(header("content-type", "application/json"))
        .and(header_regex("traceparent", traceparent_pattern.as_str()))
        .and(header("x-myheader-to-keep", "push"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1);
    mock_server.register(mock_with_trace).await;

    // Get a free port for the collector server
    let collector_port = get_free_port().await;

    // Create temporary directory for config
    let temp_dir = TempDir::new().unwrap();

    let (_cmd, signal) = lauch_collector(&temp_dir, collector_port, &sink_url).await;

    // Send test CDEvent to webhook with trace context
    let test_event = create_test_cdevent();
    let client = reqwest::Client::new();
    let webhook_response = client
        .post(format!("http://127.0.0.1:{collector_port}/webhook/test"))
        .header("content-type", "application/json")
        .header("x-myheader-to-keep", "push")
        .header("traceparent", &traceparent)
        .json(&test_event)
        .send()
        .await
        .expect("Failed to send webhook request");
    assert_eq!(webhook_response.status(), 201);
    // Give time for processing
    sleep(Duration::from_millis(200)).await;
    // Clean up
    // cmd.kill().await.ok();
    signal.cancel();
    // Verify mock expectations were met
    mock_server.verify().await;
}

/// Test `CDEvent` passthrough without incoming `trace_id` context
#[tokio::test(flavor = "multi_thread")]
async fn test_webhook_to_http_sink_without_trace_context() {
    // Setup mock HTTP sink server
    let mock_server = MockServer::start().await;
    let sink_url = mock_server.uri();

    // Setup mock expectation for HTTP sink - should receive request without specific trace
    let mock_without_trace = Mock::given(method("POST"))
        .and(path("/events"))
        .and(header("content-type", "application/json"))
        .and(header_exists("traceparent"))
        .and(header("x-myheader-to-keep", "push"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1);
    mock_server.register(mock_without_trace).await;

    // Get a free port for the collector server
    let collector_port = get_free_port().await;

    // Create temporary directory for config
    let temp_dir = TempDir::new().unwrap();

    let (_cmd, signal) = lauch_collector(&temp_dir, collector_port, &sink_url).await;

    // Send test CDEvent to webhook WITHOUT trace context
    let test_event = create_test_cdevent();
    let client = reqwest::Client::new();
    let webhook_response = client
        .post(format!("http://127.0.0.1:{collector_port}/webhook/test"))
        .header("content-type", "application/json")
        .header("x-myheader-to-keep", "push")
        .json(&test_event)
        .send()
        .await
        .expect("Failed to send webhook request");

    assert_eq!(webhook_response.status(), 201);

    // Give time for processing
    sleep(Duration::from_millis(200)).await;

    // Clean up
    // cmd.kill().await.ok();
    signal.cancel();

    // Verify mock expectations were met
    mock_server.verify().await;
}

/// Create a test `CDEvent` for sending to the webhook
fn create_test_cdevent() -> serde_json::Value {
    json!({
        "context": {
            "version": "0.4.1",
            "id": "271069a8-fc18-44f1-b38f-9d70a1695819",
            "chainId": "4c8cb7dd-3448-41de-8768-eec704e2829b",
            "source": "/event/source/123",
            "type": "dev.cdevents.artifact.published.0.2.0",
            "timestamp": "2023-03-20T14:27:05.315384Z",
        },
        "subject": {
            "id": "pkg:golang/mygit.com/myorg/myapp@234fd47e07d1004f0aed9c",
            "source": "/event/source/123",
            "type": "artifact",
            "content": {
                "sbom": {
                "uri": "https://sbom.repo/myorg/234fd47e07d1004f0aed9c.sbom"
                },
                "user": "mybot-myapp"
            }
        }
    })
}

/// Get a free port for testing
async fn get_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    addr.port()
}
