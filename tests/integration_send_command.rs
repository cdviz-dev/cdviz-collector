#![allow(clippy::unwrap_used)]

use indoc::indoc;
use serde_json::json;
use std::time::Duration;
use tokio::time::sleep;
use wiremock::matchers::{header, header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Integration test for send command -> HTTP sink pipeline
/// Tests direct `CDEvent` sending via CLI with HTTP sink
#[tokio::test]
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
#[tokio::test]
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
#[tokio::test]
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
#[tokio::test]
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
#[tokio::test]
async fn test_send_command_http_error_handling() {
    // Setup mock HTTP sink server that returns errors
    let mock_server = MockServer::start().await;
    let sink_url = mock_server.uri();

    // Setup mock to return 500 error
    let mock = Mock::given(method("POST"))
        .and(path("/events"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .expect(1);

    mock_server.register(mock).await;

    // Create a test CDEvent
    let test_event = create_test_cdevent();
    let event_json = serde_json::to_string(&test_event).unwrap();

    // Run send command - should handle error gracefully
    let result = cdviz_collector::run_with_args(vec![
        "--disable-otel",
        "send",
        "-d",
        &event_json,
        "-u",
        &format!("{sink_url}/events"),
    ])
    .await;

    // Command should still complete (error is logged but not fatal)
    match result {
        Ok(success) => assert!(success, "Command completed but returned false"),
        Err(e) => panic!("Command failed with error: {e:?}"),
    }

    // Give a moment for the request to be processed
    sleep(Duration::from_millis(100)).await;

    // Verify mock expectations were met
    mock_server.verify().await;
}

/// Integration test using indoc for inline JSON
#[tokio::test]
async fn test_send_command_with_inline_json() {
    // Setup mock HTTP sink server
    let mock_server = MockServer::start().await;
    let sink_url = mock_server.uri();

    // Setup mock expectation - expect CloudEvents format (no custom headers)
    let mock = Mock::given(method("POST"))
        .and(path("/webhook"))
        .and(header("ce-specversion", "1.0"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1);

    mock_server.register(mock).await;

    // Use indoc for clean inline JSON
    let event_json = indoc! {r#"
        {
            "context": {
                "version": "0.4.1",
                "id": "inline-test-123",
                "source": "cli-test",
                "type": "dev.cdevents.service.deployed.0.1.1",
                "timestamp": "2023-03-20T14:27:05.315384Z"
            },
            "subject": {
                "id": "test-service",
                "type": "service",
                "content": {
                    "environment": {
                        "id": "test-env"
                    },
                    "artifactId": "test-artifact-inline"
                }
            }
        }
    "#};

    // Run send command with inline JSON
    let result = cdviz_collector::run_with_args(vec![
        "--disable-otel",
        "send",
        "-d",
        event_json.trim(),
        "-u",
        &format!("{sink_url}/webhook"),
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
#[tokio::test]
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
