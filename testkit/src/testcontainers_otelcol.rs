//! OpenTelemetry Collector testcontainer helper for integration tests.
//!
//! Provides an [`OtelCollector`] image (core distribution) that receives OTLP logs on gRPC and
//! HTTP, and prints them with the `debug` exporter (`verbosity: detailed`) on stderr.
//! Follows the same pattern as [`super::testcontainers_nats`].

use std::time::Duration;
use testcontainers::{
    ContainerAsync, Image,
    core::{ContainerPort, WaitFor, wait::LogWaitStrategy},
};

pub use testcontainers;
pub use testcontainers::runners::AsyncRunner;

/// OTLP gRPC port
pub const OTLP_GRPC_PORT: u16 = 4317;
/// OTLP HTTP port
pub const OTLP_HTTP_PORT: u16 = 4318;

const IMAGE: &str = "otel/opentelemetry-collector";
// >= 0.118 is required for the debug exporter to print the log record `EventName`
const TAG: &str = "0.130.0";

/// Inline collector config (replaces the image default config).
const CONFIG: &str = "yaml:{receivers: {otlp: {protocols: {grpc: {endpoint: '0.0.0.0:4317'}, http: {endpoint: '0.0.0.0:4318'}}}}, exporters: {debug: {verbosity: detailed}}, service: {pipelines: {logs: {receivers: [otlp], exporters: [debug]}}}}";

/// OpenTelemetry Collector test container, logs pipeline: OTLP receiver -> debug exporter.
#[derive(Debug, Default)]
pub struct OtelCollector {}

impl Image for OtelCollector {
    fn name(&self) -> &str {
        IMAGE
    }

    fn tag(&self) -> &str {
        TAG
    }

    fn cmd(&self) -> impl IntoIterator<Item = impl Into<std::borrow::Cow<'_, str>>> {
        vec![format!("--config={CONFIG}")]
    }

    fn ready_conditions(&self) -> Vec<WaitFor> {
        vec![WaitFor::Log(LogWaitStrategy::stderr("Everything is ready"))]
    }

    fn expose_ports(&self) -> &[ContainerPort] {
        &[ContainerPort::Tcp(OTLP_GRPC_PORT), ContainerPort::Tcp(OTLP_HTTP_PORT)]
    }
}

/// Returns the host URL of a collector port (e.g. [`OTLP_GRPC_PORT`]).
///
/// # Panics
///
/// Panics if the port is not mapped (intended for use in tests only).
#[allow(clippy::expect_used)]
pub async fn find_url(container: &ContainerAsync<OtelCollector>, port: u16) -> String {
    let port = container.get_host_port_ipv4(port).await.expect("OTLP port not available");
    format!("http://127.0.0.1:{port}")
}

/// Polls the collector stderr (debug exporter output) until it contains every `expected`
/// string, or `timeout` elapses. Returns the last stderr read.
///
/// # Panics
///
/// Panics if the container logs cannot be read (intended for use in tests only).
#[allow(clippy::expect_used)]
pub async fn wait_for_output(
    container: &ContainerAsync<OtelCollector>,
    expected: &[&str],
    timeout: Duration,
) -> String {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let stderr = container.stderr_to_vec().await.expect("read collector stderr");
        let stderr = String::from_utf8_lossy(&stderr).into_owned();
        if expected.iter().all(|e| stderr.contains(e)) || tokio::time::Instant::now() >= deadline {
            return stderr;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
