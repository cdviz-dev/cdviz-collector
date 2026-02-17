//! NATS testcontainer helper for integration tests.
//!
//! Provides a [`Nats`] image that starts a NATS server with JetStream enabled.
//! Follows the same pattern as [`super::testcontainers_redpanda`].

use testcontainers::{
    Image,
    core::{ContainerPort, WaitFor},
};

pub use testcontainers;
pub use testcontainers::runners::AsyncRunner;

/// NATS client connections port
pub const NATS_PORT: u16 = 4222;
/// NATS HTTP monitoring port
pub const NATS_MONITOR_PORT: u16 = 8222;

const IMAGE: &str = "nats";
const TAG: &str = "2.10";

/// NATS test container with JetStream enabled.
#[derive(Debug, Default)]
pub struct Nats {}

impl Image for Nats {
    fn name(&self) -> &str {
        IMAGE
    }

    fn tag(&self) -> &str {
        TAG
    }

    fn cmd(&self) -> impl IntoIterator<Item = impl Into<std::borrow::Cow<'_, str>>> {
        // Enable JetStream via --js flag
        vec!["--js"]
    }

    fn ready_conditions(&self) -> Vec<WaitFor> {
        // NATS starts in milliseconds; a short fixed delay is more reliable
        // than log-based waiting with testcontainers' stdout log stream.
        vec![WaitFor::Duration { length: std::time::Duration::from_millis(500) }]
    }

    fn expose_ports(&self) -> &[ContainerPort] {
        &[]
    }
}

/// Returns the NATS server URL for the running container.
pub async fn find_server_url(container: &testcontainers::core::ContainerAsync<Nats>) -> String {
    let port = container.get_host_port_ipv4(NATS_PORT).await.expect("NATS port not available");
    format!("nats://127.0.0.1:{port}")
}

/// Pre-creates a JetStream stream so both sink and source can use it without a race.
///
/// # Panics
///
/// Panics if the connection or stream creation fails (intended for use in tests only).
#[allow(clippy::expect_used)]
pub async fn create_jetstream_stream(server_url: &str, stream_name: &str, subjects: &[&str]) {
    let client = async_nats::connect(server_url).await.expect("connect to NATS");
    let js = async_nats::jetstream::new(client);
    js.get_or_create_stream(async_nats::jetstream::stream::Config {
        name: stream_name.to_string(),
        subjects: subjects.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    })
    .await
    .expect("create JetStream stream");
}
