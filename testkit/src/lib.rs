pub mod cdevent_utils;
pub mod connector_chain;
pub mod testcontainers_redpanda;

pub use cdevent_utils::generate_random_cdevents;
pub use connector_chain::test_connector_chain;

/// Get a free port for testing
#[allow(dead_code)]
pub async fn get_free_port() -> u16 {
    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    addr.port()
}
