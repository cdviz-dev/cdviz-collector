#![allow(clippy::unwrap_used)]

use indoc::indoc;
use serde_json::json;
use std::time::Duration;
use tokio::time::sleep;
use wiremock::matchers::{header, header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Integration test for send command -> HTTP sink pipeline
/// Tests direct `CDEvent` sending via CLI with HTTP sink
#[tokio::test(flavor = "multi_thread")]
async fn test_send_command_to_http_sink() {
    // Setup mock HTTP sink server
    let mock_server = MockServer::start().await;
    let sink_url = mock_server.uri();

    // Setup mock expectation for HTTP sink - expect CloudEvents format (no custom headers)
    let mock = Mock::given(method("POST"))
        .and(path("/events"))
        .and(header("ce-specversion", "1.0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "received",
            "message": "CDEvent processed successfully"
        })))
        .expect(1);

    mock_server.register(mock).await;

    // Create a test CDEvent
    let test_event = create_test_cdevent();
    let event_json = serde_json::to_string(&test_event).unwrap();

    // Run send command with HTTP sink
    let result = cdviz_collector::run_with_args(vec![
        "--disable-otel",
        "send",
        "-d",
        &event_json,
        "-u",
        &format!("{sink_url}/events"),
    ])
    .await;

    // Should complete successfully
    match result {
        Ok(success) => assert!(success, "Command completed but returned false"),
        Err(e) => panic!("Command failed with error: {e:?}"),
    }

    // Give a moment for the request to be processed
    sleep(Duration::from_millis(100)).await;

    // Verify mock expectations were met
    mock_server.verify().await;
}

/// Integration test for send command with custom headers
#[tokio::test(flavor = "multi_thread")]
async fn test_send_command_with_custom_headers() {
    // Setup mock HTTP sink server
    let mock_server = MockServer::start().await;
    let sink_url = mock_server.uri();

    // Setup mock expectation with custom headers - focus on essential headers only
    let mock = Mock::given(method("POST"))
        .and(path("/webhook"))
        .and(header("x-api-key", "test-secret-123"))
        .and(header("authorization", "Bearer test-token"))
        .and(header("ce-specversion", "1.0"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1);

    mock_server.register(mock).await;

    // Create a test CDEvent
    let test_event = create_test_cdevent();
    let event_json = serde_json::to_string(&test_event).unwrap();

    // Run send command with custom headers
    let result = cdviz_collector::run_with_args(vec![
        "--disable-otel",
        "send",
        "-d",
        &event_json,
        "-u",
        &format!("{sink_url}/webhook"),
        "-H",
        "X-API-Key: test-secret-123",
        "-H",
        "Authorization: Bearer test-token",
    ])
    .await;

    // Should complete successfully
    match result {
        Ok(success) => assert!(success, "Command completed but returned false"),
        Err(e) => panic!("Command failed with error: {e:?}"),
    }

    // Give a moment for the request to be processed
    sleep(Duration::from_millis(100)).await;

    // Verify mock expectations were met
    mock_server.verify().await;
}

/// Integration test for send command with file input
#[tokio::test(flavor = "multi_thread")]
async fn test_send_command_with_file_input() {
    use std::fs;
    use tempfile::NamedTempFile;

    // Setup mock HTTP sink server
    let mock_server = MockServer::start().await;
    let sink_url = mock_server.uri();

    // Setup mock expectation - expect CloudEvents format (no custom headers)
    let mock = Mock::given(method("POST"))
        .and(path("/events"))
        .and(header("ce-specversion", "1.0"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1);

    mock_server.register(mock).await;

    // Create temporary file with test CDEvent
    let test_event = create_test_cdevent();
    let temp_file = NamedTempFile::new().unwrap();
    fs::write(temp_file.path(), serde_json::to_string_pretty(&test_event).unwrap()).unwrap();

    // Run send command with file input (using @ syntax)
    let file_arg = format!("@{}", temp_file.path().to_string_lossy());
    let result = cdviz_collector::run_with_args(vec![
        "--disable-otel",
        "send",
        "-d",
        &file_arg,
        "-u",
        &format!("{sink_url}/events"),
    ])
    .await;

    // Should complete successfully
    match result {
        Ok(success) => assert!(success, "Command completed but returned false"),
        Err(e) => panic!("Command failed with error: {e:?}"),
    }

    // Give a moment for the request to be processed
    sleep(Duration::from_millis(100)).await;

    // Verify mock expectations were met
    mock_server.verify().await;
}

/// Integration test for send command with array of events
#[tokio::test(flavor = "multi_thread")]
async fn test_send_command_with_array_of_events() {
    // Setup mock HTTP sink server
    let mock_server = MockServer::start().await;
    let sink_url = mock_server.uri();

    // Setup mock expectation - should receive 2 separate events in CloudEvents format
    let mock = Mock::given(method("POST"))
        .and(path("/events"))
        .and(header("ce-specversion", "1.0"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2); // Expecting 2 separate events

    mock_server.register(mock).await;

    // Create array of test CDEvents
    let event1 = create_test_cdevent_with_id("test-event-001");
    let event2 = create_test_cdevent_with_id("test-event-002");
    let events_array = json!([event1, event2]);
    let events_json = serde_json::to_string(&events_array).unwrap();

    // Run send command with array of events
    let result = cdviz_collector::run_with_args(vec![
        "--disable-otel",
        "send",
        "-d",
        &events_json,
        "-u",
        &format!("{sink_url}/events"),
    ])
    .await;

    // Should complete successfully
    match result {
        Ok(success) => assert!(success, "Command completed but returned false"),
        Err(e) => panic!("Command failed with error: {e:?}"),
    }

    // Give a moment for the requests to be processed
    sleep(Duration::from_millis(200)).await;

    // Verify mock expectations were met (2 events sent)
    mock_server.verify().await;
}

/// Integration test for send command error handling
#[tokio::test(flavor = "multi_thread")]
async fn test_send_command_http_error_handling() {
    // Setup mock HTTP sink server that returns errors
    let mock_server = MockServer::start().await;
    let sink_url = mock_server.uri();

    // Setup mock to return 500 error; at least one request must arrive (with retries there may be
    // more, but the exact count is timing-dependent so we assert at-least-1).
    let mock = Mock::given(method("POST"))
        .and(path("/events"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .expect(1_u64..);

    mock_server.register(mock).await;

    // Create a test CDEvent
    let test_event = create_test_cdevent();
    let event_json = serde_json::to_string(&test_event).unwrap();

    // Run send command - HTTP sink failures are logged but the command still returns Ok(true)
    // (retries are bounded by --total-duration-of-retries, so the await blocks until done)
    let result = cdviz_collector::run_with_args(vec![
        "--disable-otel",
        "send",
        "-d",
        &event_json,
        "-u",
        &format!("{sink_url}/events"),
        "--total-duration-of-retries",
        "2s",
    ])
    .await;

    match result {
        Ok(success) => assert!(success, "Command completed but returned false"),
        Err(e) => panic!("Command failed with error: {e:?}"),
    }

    // Verify at least one request was made (retries may have produced more)
    mock_server.verify().await;
}

/// Create a test `CDEvent` for integration testing
fn create_test_cdevent() -> serde_json::Value {
    json!({
        "context": {
            "version": "0.4.1",
            "id": "integration-test-123",
            "source": "send-command-test",
            "type": "dev.cdevents.service.deployed.0.1.1",
            "timestamp": "2023-03-20T14:27:05.315384Z",
        },
        "subject": {
            "id": "test-service",
            "source": "send-command-test",
            "type": "service",
            "content": {
                "environment": {
                    "id": "test-environment",
                    "source": "send-command-test/env"
                },
                "artifactId": "pkg:oci/test-app@sha256:test123"
            }
        }
    })
}

// ── --run integration tests ──────────────────────────────────────────────

/// Test that `--run taskrun -- false` emits two `CDEvents` and returns failure (exit 1).
#[tokio::test(flavor = "multi_thread")]
async fn test_send_run_taskrun_exit_1_returns_false() {
    let mock_server = MockServer::start().await;
    let sink_url = mock_server.uri();

    let mock = Mock::given(method("POST"))
        .and(path("/events"))
        .and(header("ce-specversion", "1.0"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2);
    mock_server.register(mock).await;

    let result = cdviz_collector::run_with_args(vec![
        "--disable-otel",
        "send",
        "--run",
        "taskrun",
        "-u",
        &format!("{sink_url}/events"),
        "--",
        "false",
    ])
    .await;

    sleep(Duration::from_millis(200)).await;
    mock_server.verify().await;

    // exit 1 → Ok(false)
    assert!(matches!(result, Ok(false)), "Expected Ok(false), got: {result:?}");
}

/// Test that `--run taskrun` `CDEvents` contain correct event types.
#[tokio::test(flavor = "multi_thread")]
async fn test_send_run_taskrun_event_types() {
    use std::sync::{Arc, Mutex};

    let mock_server = MockServer::start().await;
    let sink_url = mock_server.uri();
    let received_types: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let received_types_clone = Arc::clone(&received_types);

    let mock = Mock::given(method("POST"))
        .and(path("/events"))
        .and(header("ce-specversion", "1.0"))
        .respond_with(move |req: &wiremock::Request| {
            // Extract the ce-type header which contains the CDEvent type
            if let Some(ce_type) = req.headers.get("ce-type")
                && let Ok(mut types) = received_types_clone.lock()
            {
                types.push(ce_type.to_str().unwrap_or("").to_string());
            }
            ResponseTemplate::new(200)
        })
        .expect(2);
    mock_server.register(mock).await;

    cdviz_collector::run_with_args(vec![
        "--disable-otel",
        "send",
        "--run",
        "taskrun",
        "-u",
        &format!("{sink_url}/events"),
        "--",
        "true",
    ])
    .await
    .unwrap();

    sleep(Duration::from_millis(200)).await;
    mock_server.verify().await;

    let types = received_types.lock().unwrap();
    assert!(
        types.iter().any(|t| t.contains("taskrun.started")),
        "Expected taskrun.started event, got: {types:?}"
    );
    assert!(
        types.iter().any(|t| t.contains("taskrun.finished")),
        "Expected taskrun.finished event, got: {types:?}"
    );
}

/// Test that `--run taskrun --metadata suite_name=…` overrides the `taskName` field in the
/// emitted `CDEvents`.  The `to_taskrun` VRL transformer reads `.metadata.run.suite_name` and
/// writes it into `subject.content.taskName`, so we inspect the request body to confirm.
#[tokio::test(flavor = "multi_thread")]
async fn test_send_run_taskrun_metadata_override() {
    use std::sync::{Arc, Mutex};

    let mock_server = MockServer::start().await;
    let sink_url = mock_server.uri();
    let received_bodies: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(vec![]));
    let received_bodies_clone = Arc::clone(&received_bodies);

    let mock = Mock::given(method("POST"))
        .and(path("/events"))
        .and(header("ce-specversion", "1.0"))
        .respond_with(move |req: &wiremock::Request| {
            if let Ok(body) = serde_json::from_slice::<serde_json::Value>(&req.body)
                && let Ok(mut bodies) = received_bodies_clone.lock()
            {
                bodies.push(body);
            }
            ResponseTemplate::new(200)
        })
        .expect(2);
    mock_server.register(mock).await;

    let result = cdviz_collector::run_with_args(vec![
        "--disable-otel",
        "send",
        "--run",
        "taskrun",
        "--metadata",
        "task_name=my-custom-task",
        "-u",
        &format!("{sink_url}/events"),
        "--",
        "true",
    ])
    .await;

    sleep(Duration::from_millis(200)).await;
    mock_server.verify().await;

    assert!(matches!(result, Ok(true)), "Expected Ok(true), got: {result:?}");

    // Verify the metadata override was actually applied: `suite_name` maps to
    // `subject.content.taskName` in the `to_taskrun` VRL transformer.
    let bodies = received_bodies.lock().unwrap();
    assert!(
        bodies.iter().any(|b| b["subject"]["content"]["taskName"] == "my-custom-task"),
        "Expected taskName=my-custom-task in at least one event body, got: {bodies:?}"
    );
}

/// Create a test `CDEvent` with custom ID
fn create_test_cdevent_with_id(id: &str) -> serde_json::Value {
    json!({
        "context": {
            "version": "0.4.1",
            "id": id,
            "source": "send-command-test",
            "type": "dev.cdevents.service.deployed.0.1.1",
            "timestamp": "2023-03-20T14:27:05.315384Z",
        },
        "subject": {
            "id": format!("service-{}", id),
            "source": "send-command-test",
            "type": "service",
            "content": {
                "environment": {
                    "id": "test-environment"
                },
                "artifactId": format!("pkg:oci/test-app-{}@sha256:test123", id)
            }
        }
    })
}

/// Integration test for send command with HMAC signature validation
#[tokio::test(flavor = "multi_thread")]
async fn test_send_command_with_signature_validation() {
    // Setup mock HTTP sink server
    let mock_server = MockServer::start().await;
    let sink_url = mock_server.uri();

    let webhook_secret = "test-webhook-secret";
    let test_event = create_test_cdevent();
    let event_json = serde_json::to_string(&test_event).unwrap();

    // We'll validate that the signature header exists - exact payload verification would require
    // knowing the CloudEvents serialization format which is created during the pipeline

    // Setup mock expectation - should receive signature header
    let mock = Mock::given(method("POST"))
        .and(path("/webhook"))
        .and(header("ce-specversion", "1.0"))
        .and(header_exists("x-signature-256"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "received",
            "message": "Signed webhook processed successfully"
        })))
        .expect(1);

    mock_server.register(mock).await;

    // Create temporary config file with signature configuration
    let config_content = format!(
        indoc! {r#"
        [sinks.http.headers.x-signature-256]
        type = "signature"
        token = "{}"
        algorithm = "sha256"
        prefix = "sha256="
    "#},
        webhook_secret
    );

    let temp_config = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(temp_config.path(), config_content).unwrap();

    // Run send command with signature config
    let result = cdviz_collector::run_with_args(vec![
        "--disable-otel",
        "send",
        "-d",
        &event_json,
        "-u",
        &format!("{sink_url}/webhook"),
        "--config",
        temp_config.path().to_str().unwrap(),
    ])
    .await;

    // Command should succeed
    match result {
        Ok(success) => assert!(success, "Command completed but returned false"),
        Err(e) => panic!("Command failed with error: {e:?}"),
    }

    // Give a moment for the request to be processed
    sleep(Duration::from_millis(100)).await;

    // Verify mock expectations were met (including signature header validation)
    mock_server.verify().await;
}
