#![allow(clippy::unwrap_used)]

use rstest::rstest;
use std::time::Duration;
use tokio::time::sleep;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ── --run taskrun integration tests ──────────────────────────────────────

/// Test that `--run taskrun -- false` emits two `CDEvents` and returns failure (exit 1).
#[tokio::test(flavor = "multi_thread")]
async fn test_send_run_taskrun_exit_1_returns_false() {
    let mock_server = MockServer::start().await;
    let sink_url = mock_server.uri();

    let mock = Mock::given(method("POST"))
        .and(path("/events"))
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

// ── --run testsuiterun integration tests ─────────────────────────────────

/// Parametrized integration test for `send --run testsuiterun_tap` and
/// `send --run testsuiterun_junit`.
///
/// # Parameters
/// - `run_type`: the `--run` argument (`testsuiterun_tap` or `testsuiterun_junit`)
/// - `report_subpath`: path relative to `tests/assets/reports/` for the result file.
///   The file is copied into an isolated temp directory so the subprocess extractor's
///   default glob (`**/*.tap` or `**/*.xml`) finds exactly one report.
/// - `results_url`: if `Some`, passed as `--metadata results_url=<url>` which triggers
///   an extra `testoutput.published` `CDEvent`
/// - `expected_outcome`: `"success"` or `"failure"` expected in the finished event
///
/// To add a new case: add a `#[case]` line with the appropriate parameters.
/// The expected event count is derived automatically (2 without `results_url`, 3 with).
#[rstest]
// TAP: all tests pass, no results URL
#[case("testsuiterun_tap", "tap/passing.tap", None, "success")]
// TAP: one test fails → outcome "failure"
#[case("testsuiterun_tap", "tap/failing.tap", None, "failure")]
// TAP: passing with results_url → extra testoutput.published event
#[case(
    "testsuiterun_tap",
    "tap/passing.tap",
    Some("https://ci.example.com/results/tap"),
    "success"
)]
// JUnit: all tests pass
#[case("testsuiterun_junit", "junit/passing.xml", None, "success")]
// JUnit: one test fails → outcome "failure"
#[case("testsuiterun_junit", "junit/failing.xml", None, "failure")]
// JUnit: failing with results_url → extra testoutput.published event
#[case(
    "testsuiterun_junit",
    "junit/failing.xml",
    Some("https://ci.example.com/results/junit"),
    "failure"
)]
#[tokio::test(flavor = "multi_thread")]
async fn test_send_run_testsuiterun(
    #[case] run_type: &str,
    #[case] report_subpath: &str,
    #[case] results_url: Option<&str>,
    #[case] expected_outcome: &str,
) {
    use std::sync::{Arc, Mutex};

    let mock_server = MockServer::start().await;
    let sink_url = mock_server.uri();

    // Collect (event_type, body) for each received request
    let received: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::new(Mutex::new(vec![]));
    let received_clone = Arc::clone(&received);

    // With a results_url the testoutput_from_testsuiterun transformer emits an extra
    // testoutput.published event alongside the finished event → 3 events total.
    let expected_event_count: u64 = if results_url.is_some() { 3 } else { 2 };

    let mock = Mock::given(method("POST"))
        .and(path("/events"))
        .respond_with(move |req: &wiremock::Request| {
            let ce_type =
                req.headers.get("ce-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            let body = serde_json::from_slice::<serde_json::Value>(&req.body)
                .unwrap_or(serde_json::Value::Null);
            if let Ok(mut entries) = received_clone.lock() {
                entries.push((ce_type, body));
            }
            ResponseTemplate::new(200)
        })
        .expect(expected_event_count);
    mock_server.register(mock).await;

    // Pass the absolute path of the fixture as --data; in --run mode this overrides
    // the source's data_globs so the subprocess extractor processes exactly one file.
    let report_abs = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/assets/reports")
        .join(report_subpath);
    let report_str = report_abs.to_string_lossy().to_string();

    let sink_url_events = format!("{sink_url}/events");
    let results_url_metadata = results_url.map(|u| format!("results_url={u}"));

    let mut args: Vec<&str> = vec![
        "--disable-otel",
        "send",
        "--run",
        run_type,
        "--data",
        &report_str,
        "-u",
        &sink_url_events,
    ];

    if let Some(ref meta) = results_url_metadata {
        args.push("--metadata");
        args.push(meta);
    }

    args.extend_from_slice(&["--", "true"]);

    let result = cdviz_collector::run_with_args(args).await;

    sleep(Duration::from_millis(200)).await;
    mock_server.verify().await;

    assert!(matches!(result, Ok(true)), "Expected Ok(true), got: {result:?}");

    let entries = received.lock().unwrap();
    let event_types: Vec<&str> = entries.iter().map(|(t, _)| t.as_str()).collect();

    // A "started" event must have been emitted
    assert!(
        event_types.iter().any(|t| t.contains("testsuiterun.started")),
        "Expected testsuiterun.started event; received types: {event_types:?}"
    );

    // A "finished" event must have been emitted with the expected outcome
    let finished_body =
        entries.iter().find(|(t, _)| t.contains("testsuiterun.finished")).map_or_else(
            || panic!("Expected testsuiterun.finished event; received types: {event_types:?}"),
            |(_, b)| b,
        );
    assert_eq!(
        finished_body["subject"]["content"]["outcome"], expected_outcome,
        "outcome mismatch in finished event body: {finished_body}"
    );

    // When results_url is provided, a testoutput.published event must also appear
    if results_url.is_some() {
        assert!(
            event_types.iter().any(|t| t.contains("testoutput.published")),
            "Expected testoutput.published event when results_url is set; received types: {event_types:?}"
        );
    }
}
