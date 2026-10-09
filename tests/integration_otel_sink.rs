#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![cfg(feature = "sink_otel")]
use cdviz_collector_testkit as testkit;

use indoc::formatdoc;
use serde_json::json;
use std::time::Duration;
use testkit::testcontainers_otelcol::*;

/// `send` command -> otel sink -> real OpenTelemetry Collector (debug exporter).
#[rstest::rstest]
#[case::grpc("grpc", OTLP_GRPC_PORT, "")]
#[case::http_protobuf("http_protobuf", OTLP_HTTP_PORT, "/v1/logs")]
#[tokio::test(flavor = "multi_thread")]
async fn test_send_to_otel_collector(
    #[case] protocol: &str,
    #[case] port: u16,
    #[case] path: &str,
) {
    let collector = OtelCollector::default().start().await.expect("start otel collector");
    let endpoint = format!("{}{path}", find_url(&collector, port).await);

    let config = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        config.path(),
        formatdoc!(
            r#"
            [sinks.otel]
            enabled = true
            type = "otel"
            endpoint = "{endpoint}"
            protocol = "{protocol}"
            "#
        ),
    )
    .unwrap();
    let event = json!({
        "context": {
            "specversion": "0.5.0",
            "id": "otel_it_1",
            "source": "/otel-it",
            "type": "dev.cdevents.pipelinerun.finished.0.3.0",
            "timestamp": "2026-10-09T10:00:00Z"
        },
        "subject": {
            "id": "run-42",
            "content": {
                "pipelineName": "release",
                "uri": "https://ci.example.com/run/42",
                "outcome": "failure"
            }
        }
    })
    .to_string();

    let result = cdviz_collector::run_with_args(vec![
        "--disable-otel",
        "send",
        "--config",
        config.path().to_str().unwrap(),
        "--data",
        &event,
    ])
    .await;
    assert!(result.expect("send command must succeed"), "send command returned false");

    // `send` flushes the sink on exit; the collector prints the record on reception.
    let expected = [
        "EventName: dev.cdevents.pipelinerun.finished.0.3.0",
        "Timestamp: 2026-10-09 10:00:00 +0000 UTC",
        "SeverityText: WARN",
        "-> service.name: Str(cdviz-collector)",
        "-> cdevents.id: Str(otel_it_1)",
        "-> cicd.pipeline.name: Str(release)",
        "-> cicd.pipeline.run.id: Str(run-42)",
        "-> cicd.pipeline.result: Str(failure)",
    ];
    let output = wait_for_output(&collector, &expected, Duration::from_secs(10)).await;
    for e in expected {
        assert!(output.contains(e), "collector output misses `{e}`:\n{output}");
    }
}
