#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use cdviz_collector_testkit as testkit;

use indoc::formatdoc;
use std::time::Duration;
use testcontainers::runners::AsyncRunner;
use testkit::testcontainers_nats::*;
use testkit::{generate_random_cdevents, test_connector_chain};

use crate::testkit::connector_chain::ConnectorSetup;

#[tokio::test(flavor = "multi_thread")]
async fn test_chain_core() {
    let subject = "cdevents.chain.core";
    let nats_container = Nats::default().start().await.expect("start NATS container");
    let server_url = find_server_url(&nats_container).await;
    let input_events = generate_random_cdevents(2).unwrap();

    test_connector_chain(
        // Connector A: folder source -> NATS core sink
        ConnectorSetup {
            config: &formatdoc!(
                r#"
                [sinks.link]
                enabled = true
                type = "nats"
                servers = "{server_url}"
                subject = "{subject}"
                "#,
            ),
            ..Default::default()
        },
        // Connector B: NATS core source -> folder sink
        ConnectorSetup {
            config: &formatdoc!(
                r#"
                [sources.link]
                enabled = true

                [sources.link.extractor]
                type = "nats"
                servers = "{server_url}"
                subject = "{subject}"
                mode = "core"
                "#,
            ),
            ..Default::default()
        },
        &input_events,
        Duration::from_secs(15),
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_chain_jetstream() {
    let subject = "cdevents.chain.jetstream";
    let stream_name = "CHAIN_EVENTS";
    let consumer_name = "link-0";

    let nats_container = Nats::default().start().await.expect("start NATS container");
    let server_url = find_server_url(&nats_container).await;

    // Pre-create the stream so sink and source are both ready immediately
    create_jetstream_stream(&server_url, stream_name, &[subject]).await;

    let input_events = generate_random_cdevents(2).unwrap();

    test_connector_chain(
        // Connector A: folder source -> NATS JetStream sink
        ConnectorSetup {
            config: &formatdoc!(
                r#"
                [sinks.link]
                enabled = true
                type = "nats"
                servers = "{server_url}"
                subject = "{subject}"
                mode = "jetstream"
                timeout = "10s"
                "#,
            ),
            ..Default::default()
        },
        // Connector B: NATS JetStream source -> folder sink
        ConnectorSetup {
            config: &formatdoc!(
                r#"
                [sources.link]
                enabled = true

                [sources.link.extractor]
                type = "nats"
                servers = "{server_url}"
                subject = "{subject}"
                mode = "jetstream"
                stream = "{stream_name}"
                consumer = "{consumer_name}"
                "#,
            ),
            ..Default::default()
        },
        &input_events,
        Duration::from_secs(15),
    )
    .await
    .unwrap();
}
