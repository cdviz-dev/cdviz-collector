#![allow(clippy::unwrap_used)]

use cdviz_collector_testkit as testkit;

use indoc::formatdoc;
use std::time::Duration;
use testkit::{generate_random_cdevents, get_free_port, test_connector_chain};

use crate::testkit::connector_chain::ConnectorSetup;

#[tokio::test(flavor = "multi_thread")]
async fn test_chain_simple() {
    // Get free port for connector A's HTTP server (webhook endpoint)
    let webhook_port = get_free_port().await;

    let input_events = generate_random_cdevents(2).unwrap();
    test_connector_chain(
        // Connector A config
        ConnectorSetup {
            config: &formatdoc!(
                r#"
                [sinks.link]
                enabled = true
                type = "http"
                destination = "http://127.0.0.1:{}/webhook/link"
                "#,
                webhook_port
            ),
            ..Default::default()
        },
        // Connector B config
        ConnectorSetup {
            http_port: webhook_port,
            config: &formatdoc!(
                r#"
                [sources.link]
                enabled = true

                [sources.link.extractor]
                type = "webhook"
                id = "link"
                "#
            ),
            //..Default::default()
        },
        &input_events,
        Duration::from_secs(15), // Allow max time for both connectors to process
    )
    .await
    .unwrap();
}
