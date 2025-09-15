#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod testkit;

use indoc::formatdoc;
use std::time::Duration;
use testcontainers::{core::ContainerAsync, runners::AsyncRunner};
use testcontainers_redpanda_rs::*;
use testkit::{generate_random_cdevents, test_connector_chain};

use crate::testkit::connector_chain::ConnectorSetup;

#[tokio::test(flavor = "multi_thread")]
async fn test_chain_simple() {
    let topic = "event";
    let kafka_container = launch_redpanda(&[topic]).await;
    let brokers = find_brokers(&kafka_container).await;
    let input_events = generate_random_cdevents(2).unwrap();
    test_connector_chain(
        // Connector A config
        ConnectorSetup {
            config: &formatdoc!(
                r#"
                [sinks.link]
                enabled = true
                type = "kafka"
                brokers = "{}"
                topic = "{}"
                key_policy = "cdevent_id"
                "#,
                brokers,
                topic,
            ),
            ..Default::default()
        },
        // Connector B config
        ConnectorSetup {
            config: &formatdoc!(
                r#"
                [sources.link]
                enabled = true

                [sources.link.extractor]
                type = "kafka"
                brokers = "{}"
                topics = ["{}"]
                group_id = "link-0"
                poll_timeout = "1s"
                # to read already published message
                rdkafka_config = {{ "auto.offset.reset" = "earliest" }}
                "#,
                brokers,
                topic,
            ),
            ..Default::default()
        },
        &input_events,
        Duration::from_secs(15), // Allow max time for both connectors to process
    )
    .await
    .unwrap();
}

/// !! Do not use rdkafka admin tools with redpanda (but OK for producer & consumer)
async fn launch_redpanda(topics: &[&str]) -> ContainerAsync<Redpanda> {
    // Start Redpanda container using testcontainers GenericImage
    // let container = GenericImage::new("redpandadata/redpanda", "v25.2.3")
    //     .with_exposed_port(9092.tcp())
    //     //.with_wait_for(WaitFor::message_on_either_std("Successfully started Redpanda!"))
    //     .with_network("bridge")
    //     .start()
    //     .await
    //     .expect("start container");
    let container = Redpanda::default().start().await.expect("start container");

    // Get broker address from the actual mapped port (testcontainers automatically maps to random host port)
    //let brokers = find_brokers(&container).await;

    // if topic has only one partition this part is optional
    // it will be automatically created when producer send a message
    // but consumer can connect before producer first'sent
    for topic in topics {
        container.exec(Redpanda::cmd_create_topic(topic, 1)).await.unwrap();
    }

    container
}

async fn find_brokers(container: &ContainerAsync<Redpanda>) -> String {
    // Get broker address from the actual mapped port (testcontainers automatically maps to random host port)
    let mapped_port = container.get_host_port_ipv4(9092).await.unwrap();
    format!("127.0.0.1:{mapped_port}")
}
