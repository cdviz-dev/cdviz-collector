//! OpenTelemetry sink: exports `CDEvents` as OTLP log records (events).
//!
//! Each `CDEvent` becomes one OpenTelemetry log record, sent in batches to an OTLP endpoint
//! (gRPC or HTTP/protobuf). The sink uses its own exporter, independent of the collector's
//! own telemetry (`--disable-otel` does not affect it).
//!
//! | Log record field | Value |
//! |---|---|
//! | `event_name` | `CDEvent` `context.type` (e.g. `dev.cdevents.pipelinerun.finished.0.3.0`); custom types use `dev.cdeventsx.custom` (see `cdevents.type` attribute) |
//! | `timestamp` | `CDEvent` `context.timestamp` (`observed_timestamp` = reception time) |
//! | `severity` | `WARN` when the outcome is a failure or error, else `INFO` |
//! | `body` | the full `CDEvent` as a structured map |
//! | attributes | `cdevents.*` for every event, plus OpenTelemetry semantic conventions (CI/CD, VCS, test, deployment) for known subjects |
//!
//! Semantic-convention attributes by subject:
//!
//! | `CDEvent` subject | Attributes |
//! |---|---|
//! | `pipelinerun` | `cicd.pipeline.name`, `cicd.pipeline.run.id`, `cicd.pipeline.run.url.full`, `cicd.pipeline.run.state`, `cicd.pipeline.result` |
//! | `taskrun` | `cicd.pipeline.task.name`, `cicd.pipeline.task.run.id`, `cicd.pipeline.task.run.url.full`, `cicd.pipeline.run.id`, `cicd.pipeline.task.run.result` |
//! | `testsuiterun` | `test.suite.name`, `test.suite.run.status` |
//! | `testcaserun` | `test.case.name`, `test.case.result.status` |
//! | `change` | `vcs.change.id`, `vcs.change.state` |
//! | `branch` | `vcs.ref.head.name`, `vcs.ref.head.type` |
//! | `repository` | `vcs.repository.name`, `vcs.repository.url.full`, `vcs.owner.name` |
//! | `service` | `deployment.environment.name`, `deployment.status` |
//! | `environment` | `deployment.environment.name` |
//!
//! Missing fields are omitted. Other subjects only carry the `cdevents.*` attributes.
//!
//! ## Configuration Example
//!
//! ```toml
//! [sinks.otel]
//! enabled = true
//! type = "otel"
//! # optional, default from `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` / `OTEL_EXPORTER_OTLP_ENDPOINT`,
//! # else `http://localhost:4317` (grpc) or `http://localhost:4318/v1/logs` (http_protobuf)
//! endpoint = "http://localhost:4317"
//! protocol = "grpc" # or "http_protobuf"
//! timeout = "10s"
//!
//! # Optional: headers sent with each export (only `static` and `secret` are supported)
//! [sinks.otel.headers.x-api-key]
//! type = "secret"
//! value = "your-api-key"
//! ```

use super::Sink;
use crate::Message;
use crate::errors::{IntoDiagnostic, Report, Result};
use crate::security::header::{
    HeaderSource, OutgoingHeaderMap, generate_headers, outgoing_header_map_to_configs,
};
use cdevents_sdk::{CDEvent, Content};
use opentelemetry::Key;
use opentelemetry::logs::{AnyValue, LogRecord, Logger, LoggerProvider, Severity};
use opentelemetry_otlp::{LogExporter, WithExportConfig, WithHttpConfig, WithTonicConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::{SdkLogger, SdkLoggerProvider};
use opentelemetry_semantic_conventions::attribute as semconv;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime};

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    /// Is the sink enabled?
    pub(crate) enabled: bool,
    /// OTLP endpoint (default from `OTEL_EXPORTER_OTLP_*` env vars, else the protocol default)
    #[serde(default)]
    endpoint: Option<String>,
    /// OTLP protocol: "grpc" (default) or "`http_protobuf`"
    #[serde(default)]
    protocol: Protocol,
    /// Export timeout (default 10s)
    #[serde(with = "humantime_serde", default = "default_timeout")]
    timeout: Duration,
    /// Headers sent with each export (e.g. authentication for a `SaaS` backend)
    #[serde(default)]
    headers: OutgoingHeaderMap,
}

fn default_timeout() -> Duration {
    Duration::from_secs(10)
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Protocol {
    #[default]
    Grpc,
    HttpProtobuf,
}

pub(crate) struct OtelSink {
    provider: SdkLoggerProvider,
    logger: SdkLogger,
}

impl std::fmt::Debug for OtelSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OtelSink").finish_non_exhaustive()
    }
}

impl TryFrom<Config> for OtelSink {
    type Error = Report;

    fn try_from(value: Config) -> Result<Self> {
        if value.headers.values().any(|h| matches!(h, HeaderSource::Signature { .. })) {
            return Err(miette::miette!(
                "otel sink: `signature` headers are not supported (only `static` and `secret`)"
            ));
        }
        let headers = generate_headers(&outgoing_header_map_to_configs(&value.headers), None)
            .into_diagnostic()?;
        let exporter = match value.protocol {
            Protocol::Grpc => {
                // TLS (with trusted roots) is only used for `https` endpoints, including ones
                // from `OTEL_EXPORTER_OTLP_*` env vars.
                let mut builder = LogExporter::builder()
                    .with_tonic()
                    .with_timeout(value.timeout)
                    .with_metadata(
                        opentelemetry_otlp::tonic_types::metadata::MetadataMap::from_headers(
                            headers,
                        ),
                    )
                    .with_tls_config(
                        opentelemetry_otlp::tonic_types::transport::ClientTlsConfig::new()
                            .with_enabled_roots(),
                    );
                if let Some(endpoint) = value.endpoint {
                    builder = builder.with_endpoint(endpoint);
                }
                builder.build()
            }
            Protocol::HttpProtobuf => {
                let headers = headers
                    .iter()
                    .map(|(k, v)| v.to_str().map(|v| (k.to_string(), v.to_string())))
                    .collect::<std::result::Result<_, _>>()
                    .into_diagnostic()?;
                let mut builder = LogExporter::builder()
                    .with_http()
                    .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
                    .with_timeout(value.timeout)
                    .with_headers(headers);
                if let Some(endpoint) = value.endpoint {
                    builder = builder.with_endpoint(endpoint);
                }
                builder.build()
            }
        }
        .into_diagnostic()?;
        let provider = SdkLoggerProvider::builder()
            .with_resource(Resource::builder().with_service_name("cdviz-collector").build())
            .with_batch_exporter(exporter)
            .build();
        Ok(Self::from_provider(provider))
    }
}

impl OtelSink {
    fn from_provider(provider: SdkLoggerProvider) -> Self {
        let logger = provider.logger("cdviz-collector");
        Self { provider, logger }
    }
}

impl Sink for OtelSink {
    // `emit` only queues the record (batch processor): nothing to await, but the trait is async.
    #[allow(clippy::unused_async_trait_impl)]
    async fn send(&self, msg: &Message) -> Result<()> {
        let cdevent = &msg.cdevent;
        let json = serde_json::to_value(cdevent).into_diagnostic()?;
        let mut record = self.logger.create_log_record();
        record.set_event_name(event_name(cdevent));
        record.set_timestamp(SystemTime::from(*cdevent.timestamp()));
        record.set_observed_timestamp(SystemTime::now());
        let attributes = to_attributes(cdevent.ty(), &json);
        if is_failure(&json) {
            record.set_severity_number(Severity::Warn);
            record.set_severity_text("WARN");
        } else {
            record.set_severity_number(Severity::Info);
            record.set_severity_text("INFO");
        }
        record.add_attributes(attributes);
        if let Some(body) = to_any_value(json) {
            record.set_body(body);
        }
        self.logger.emit(record);
        Ok(())
    }

    async fn flush(&self) -> Result<()> {
        // `shutdown` flushes pending records and blocks until the exporter is done.
        let provider = self.provider.clone();
        tokio::task::spawn_blocking(move || provider.shutdown())
            .await
            .into_diagnostic()?
            .into_diagnostic()
    }
}

/// Name for custom (`dev.cdeventsx.*`) types: their set is unbounded, so they are not interned.
const CUSTOM_EVENT_NAME: &str = "dev.cdeventsx.custom";

/// `LogRecord::set_event_name` requires a `&'static str`: intern the `CDEvent` type.
/// Only types known by `cdevents-sdk` are interned, so the leaked set stays bounded.
fn event_name(cdevent: &CDEvent) -> &'static str {
    static NAMES: LazyLock<Mutex<HashSet<&'static str>>> = LazyLock::new(Default::default);
    if matches!(cdevent.subject().content(), Content::Custom { .. }) {
        return CUSTOM_EVENT_NAME;
    }
    let ty = cdevent.ty();
    let Ok(mut names) = NAMES.lock() else {
        return CUSTOM_EVENT_NAME;
    };
    if let Some(name) = names.get(ty) {
        return name;
    }
    let name: &'static str = Box::leak(ty.to_string().into_boxed_str());
    names.insert(name);
    name
}

fn str_at<'a>(json: &'a Value, pointer: &str) -> Option<&'a str> {
    json.pointer(pointer).and_then(Value::as_str)
}

/// `outcome` of `*.finished` events, as defined by the `CDEvents` spec (all versions).
fn outcome(json: &Value) -> Option<&str> {
    str_at(json, "/subject/content/outcome")
}

fn is_failure(json: &Value) -> bool {
    matches!(outcome(json), Some("failure" | "fail" | "error"))
}

/// Build the log record attributes for a `CDEvent` (`ty` = `context.type`, `json` = the
/// serialized `CDEvent`).
fn to_attributes(ty: &str, json: &Value) -> Vec<(&'static str, String)> {
    // `dev.cdevents.<subject>.<predicate>.<version>`
    let mut parts = ty.split('.').skip(2);
    let subject = parts.next().unwrap_or_default();
    let predicate = parts.next().unwrap_or_default();
    let subject_id = str_at(json, "/subject/id");
    let mut attrs: Vec<(&'static str, Option<&str>)> = vec![
        ("cdevents.id", str_at(json, "/context/id")),
        ("cdevents.type", Some(ty)),
        ("cdevents.source", str_at(json, "/context/source")),
        ("cdevents.subject.id", subject_id),
        ("cdevents.subject.source", str_at(json, "/subject/source")),
        ("cdevents.subject.type", Some(subject)),
        ("cdevents.predicate", Some(predicate)),
    ];
    attrs.extend(semconv_attributes(subject, predicate, json));
    attrs
        .into_iter()
        .filter_map(|(k, v)| v.filter(|v| !v.is_empty()).map(|v| (k, v.to_string())))
        .collect()
}

/// OpenTelemetry semantic-convention attributes for the known `CDEvent` subjects.
fn semconv_attributes<'a>(
    subject: &str,
    predicate: &str,
    json: &'a Value,
) -> Vec<(&'static str, Option<&'a str>)> {
    let subject_id = str_at(json, "/subject/id");
    let content = |path: &str| str_at(json, &format!("/subject/content{path}"));
    // `url` up to spec 0.4, `uri` since spec 0.5
    let run_url = || content("/uri").or_else(|| content("/url"));
    match subject {
        "pipelinerun" => vec![
            (semconv::CICD_PIPELINE_NAME, content("/pipelineName")),
            (semconv::CICD_PIPELINE_RUN_ID, subject_id),
            (semconv::CICD_PIPELINE_RUN_URL_FULL, run_url()),
            (
                semconv::CICD_PIPELINE_RUN_STATE,
                match predicate {
                    "queued" => Some("pending"),
                    "started" => Some("executing"),
                    _ => None,
                },
            ),
            (semconv::CICD_PIPELINE_RESULT, outcome(json).and_then(cicd_result)),
        ],
        "taskrun" => vec![
            (semconv::CICD_PIPELINE_TASK_NAME, content("/taskName")),
            (semconv::CICD_PIPELINE_TASK_RUN_ID, subject_id),
            (semconv::CICD_PIPELINE_TASK_RUN_URL_FULL, run_url()),
            (semconv::CICD_PIPELINE_RUN_ID, content("/pipelineRun/id")),
            (semconv::CICD_PIPELINE_TASK_RUN_RESULT, outcome(json).and_then(cicd_result)),
        ],
        "testsuiterun" => vec![
            (semconv::TEST_SUITE_NAME, content("/testSuite/name")),
            (
                semconv::TEST_SUITE_RUN_STATUS,
                match predicate {
                    "queued" | "started" => Some("in_progress"),
                    _ => match outcome(json) {
                        Some("success" | "pass") => Some("success"),
                        Some("failure" | "fail" | "error") => Some("failure"),
                        Some("cancel") => Some("aborted"),
                        _ => None,
                    },
                },
            ),
        ],
        "testcaserun" => vec![
            (semconv::TEST_CASE_NAME, content("/testCase/name")),
            (
                semconv::TEST_CASE_RESULT_STATUS,
                match outcome(json) {
                    Some("success" | "pass") => Some("pass"),
                    Some("failure" | "fail" | "error") => Some("fail"),
                    _ => None,
                },
            ),
        ],
        "change" => vec![
            (semconv::VCS_CHANGE_ID, subject_id),
            (
                semconv::VCS_CHANGE_STATE,
                match predicate {
                    "created" | "updated" | "reviewed" => Some("open"),
                    "merged" => Some("merged"),
                    "abandoned" => Some("closed"),
                    _ => None,
                },
            ),
        ],
        "branch" => vec![
            (semconv::VCS_REF_HEAD_NAME, subject_id),
            (semconv::VCS_REF_HEAD_TYPE, Some("branch")),
        ],
        "repository" => vec![
            (semconv::VCS_REPOSITORY_NAME, content("/name")),
            (semconv::VCS_REPOSITORY_URL_FULL, content("/viewUrl").or_else(|| content("/url"))),
            (semconv::VCS_OWNER_NAME, content("/owner")),
        ],
        "service" => vec![
            (semconv::DEPLOYMENT_ENVIRONMENT_NAME, content("/environment/id")),
            (
                semconv::DEPLOYMENT_STATUS,
                match predicate {
                    "deployed" | "upgraded" | "rolledback" => Some("succeeded"),
                    _ => None,
                },
            ),
        ],
        "environment" => {
            vec![(semconv::DEPLOYMENT_ENVIRONMENT_NAME, content("/name").or(subject_id))]
        }
        _ => vec![],
    }
}

/// Map a `CDEvents` `outcome` to `cicd.pipeline.result` / `cicd.pipeline.task.run.result`.
fn cicd_result(outcome: &str) -> Option<&'static str> {
    match outcome {
        "success" => Some("success"),
        "failure" => Some("failure"),
        "error" => Some("error"),
        "cancel" => Some("cancellation"),
        _ => None,
    }
}

/// Convert JSON into an OpenTelemetry `AnyValue` (`null` has no equivalent and is dropped).
fn to_any_value(json: Value) -> Option<AnyValue> {
    match json {
        Value::Null => None,
        Value::Bool(b) => Some(AnyValue::Boolean(b)),
        Value::Number(n) => {
            n.as_i64().map(AnyValue::Int).or_else(|| n.as_f64().map(AnyValue::Double))
        }
        Value::String(s) => Some(AnyValue::String(s.into())),
        Value::Array(a) => {
            Some(AnyValue::ListAny(Box::new(a.into_iter().filter_map(to_any_value).collect())))
        }
        Value::Object(o) => Some(AnyValue::Map(Box::new(
            o.into_iter()
                .filter_map(|(k, v)| to_any_value(v).map(|v| (Key::from(k), v)))
                .collect::<HashMap<_, _>>(),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::logs::{InMemoryLogExporter, SimpleLogProcessor};
    use serde_json::json;

    fn cdevent(ty: &str, subject_id: &str, content: &Value) -> CDEvent {
        serde_json::from_value(json!({
            "context": {
                "specversion": "0.5.0",
                "id": "271069a8-fc18-44f1-b38f-9d70a1695819",
                "source": "/event/source/123",
                "type": ty,
                "timestamp": "2026-10-09T10:00:00Z"
            },
            "subject": {
                "id": subject_id,
                "source": "/event/source/123",
                "content": content
            }
        }))
        .unwrap()
    }

    fn attributes(cdevent: &CDEvent) -> Vec<(&'static str, String)> {
        let json = serde_json::to_value(cdevent).unwrap();
        to_attributes(cdevent.ty(), &json)
    }

    #[rstest::rstest]
    #[case::pipelinerun_queued("dev.cdevents.pipelinerun.queued.0.3.0", "run-1", json!({"pipelineName": "build", "uri": "https://ci.example.com/run/1"}))]
    #[case::pipelinerun_finished("dev.cdevents.pipelinerun.finished.0.3.0", "run-1", json!({"pipelineName": "build", "outcome": "failure"}))]
    #[case::pipelinerun_finished_0_1_1("dev.cdevents.pipelinerun.finished.0.1.1", "run-1", json!({"pipelineName": "build", "url": "https://ci.example.com/run/1", "outcome": "success"}))]
    #[case::taskrun_finished("dev.cdevents.taskrun.finished.0.3.0", "task-1", json!({"taskName": "test", "pipelineRun": {"id": "run-1"}, "outcome": "cancel"}))]
    #[case::testsuiterun_started("dev.cdevents.testsuiterun.started.0.3.0", "suite-1", json!({"environment": {"id": "dev"}, "testSuite": {"id": "s", "name": "unit"}}))]
    #[case::testsuiterun_finished("dev.cdevents.testsuiterun.finished.0.3.0", "suite-1", json!({"environment": {"id": "dev"}, "outcome": "error", "testSuite": {"id": "s", "name": "unit"}}))]
    #[case::testcaserun_finished("dev.cdevents.testcaserun.finished.0.3.0", "case-1", json!({"environment": {"id": "dev"}, "outcome": "success", "testCase": {"id": "c", "name": "it_works"}}))]
    #[case::change_merged("dev.cdevents.change.merged.0.3.0", "42", json!({"repository": {"id": "cdviz-dev/cdviz-collector"}}))]
    #[case::branch_created("dev.cdevents.branch.created.0.3.0", "feat/otel", json!({}))]
    #[case::repository_created("dev.cdevents.repository.created.0.3.0", "repo-1", json!({"name": "cdviz-collector", "owner": "cdviz-dev", "uri": "https://github.com/cdviz-dev/cdviz-collector.git", "viewUrl": "https://github.com/cdviz-dev/cdviz-collector"}))]
    #[case::service_deployed("dev.cdevents.service.deployed.0.3.0", "svc-1", json!({"artifactId": "pkg:oci/app@sha256:0", "environment": {"id": "prod"}}))]
    #[case::environment_created("dev.cdevents.environment.created.0.3.0", "env-1", json!({"name": "staging"}))]
    #[case::build_started_unmapped("dev.cdevents.build.started.0.3.0", "build-1", json!({}))]
    #[case::taskrun_missing_fields("dev.cdevents.taskrun.started.0.3.0", "task-1", json!({}))]
    fn mapping(#[case] ty: &str, #[case] subject_id: &str, #[case] content: Value) {
        let cdevent = cdevent(ty, subject_id, &content);
        insta::with_settings!({ snapshot_suffix => ty.replace('.', "_") }, {
            insta::assert_yaml_snapshot!(attributes(&cdevent));
        });
    }

    #[test]
    fn custom_types_are_not_interned() {
        let custom = cdevent("dev.cdeventsx.mytool-foo.bar.0.1.0", "x", &json!({}));
        assert_eq!(event_name(&custom), CUSTOM_EVENT_NAME);
        let known = cdevent("dev.cdevents.build.started.0.3.0", "x", &json!({}));
        assert_eq!(event_name(&known), "dev.cdevents.build.started.0.3.0");
    }

    #[tokio::test]
    async fn send_emits_log_records() {
        let exporter = InMemoryLogExporter::default();
        let provider = SdkLoggerProvider::builder()
            .with_log_processor(SimpleLogProcessor::new(exporter.clone()))
            .build();
        let sink = OtelSink::from_provider(provider);
        let finished = cdevent(
            "dev.cdevents.pipelinerun.finished.0.3.0",
            "run-1",
            &json!({"pipelineName": "build", "outcome": "failure"}),
        );
        let started = cdevent(
            "dev.cdevents.pipelinerun.started.0.3.0",
            "run-1",
            &json!({"pipelineName": "build", "uri": "https://ci.example.com/run/1"}),
        );
        sink.send(&Message::new(started, HashMap::new())).await.unwrap();
        sink.send(&Message::new(finished.clone(), HashMap::new())).await.unwrap();

        let logs = exporter.get_emitted_logs().unwrap();
        assert_eq!(logs.len(), 2);
        let record = &logs[1].record;
        assert_eq!(record.event_name(), Some("dev.cdevents.pipelinerun.finished.0.3.0"));
        assert_eq!(record.timestamp(), Some(SystemTime::from(*finished.timestamp())));
        assert_eq!(record.severity_number(), Some(Severity::Warn));
        assert!(
            record.attributes_iter().any(|(k, v)| k.as_str() == semconv::CICD_PIPELINE_RESULT
                && *v == AnyValue::from("failure"))
        );
        let Some(AnyValue::Map(body)) = record.body() else { panic!("body must be a map") };
        assert!(body.contains_key(&Key::from("subject")));
        sink.flush().await.unwrap();
    }

    #[test]
    fn config_from_toml() {
        let config: Config = toml::from_str(indoc::indoc! {r#"
            enabled = true
            endpoint = "http://localhost:4318/v1/logs"
            protocol = "http_protobuf"
            timeout = "5s"
            [headers.x-api-key]
            type = "static"
            value = "abc"
        "#})
        .unwrap();
        assert_eq!(config.protocol, Protocol::HttpProtobuf);
        assert_eq!(config.timeout, Duration::from_secs(5));
        assert!(toml::from_str::<Config>("enabled = true\nfoo = 1").is_err());
    }

    #[tokio::test]
    async fn signature_headers_are_rejected() {
        let config: Config = toml::from_str(indoc::indoc! {r#"
            enabled = true
            [headers.x-signature]
            type = "signature"
            token = "secret"
        "#})
        .unwrap();
        assert!(OtelSink::try_from(config).is_err());
    }

    #[tokio::test]
    async fn non_ascii_http_headers_are_rejected() {
        let config: Config = toml::from_str(indoc::indoc! {r#"
            enabled = true
            protocol = "http_protobuf"
            [headers.authorization]
            type = "static"
            value = "Bearer é"
        "#})
        .unwrap();
        assert!(OtelSink::try_from(config).is_err());
    }
}
