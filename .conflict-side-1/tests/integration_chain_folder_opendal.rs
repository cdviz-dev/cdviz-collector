#![allow(clippy::unwrap_used)]

mod testkit;

use indoc::formatdoc;
use std::time::Duration;
use tempfile::TempDir;
use testkit::{generate_random_cdevents, test_connector_chain};

use testkit::connector_chain::ConnectorSetup;

#[tokio::test(flavor = "multi_thread")]
async fn test_chain_simple() {
    let link_dir = TempDir::new()
        .map_err(|e| miette::miette!("Failed to create temp link dir: {}", e))
        .unwrap();
    let link_dir_path = link_dir.path().to_string_lossy().replace('\\', "\\\\");

    let input_events = generate_random_cdevents(2).unwrap();
    test_connector_chain(
        // Connector A config
        ConnectorSetup {
            config: &formatdoc!(
                r#"
                [sinks.link]
                enabled = true
                type = "folder"
                kind = "fs"
                parameters = {{ root = "{}" }}
                "#,
                link_dir_path
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
                type = "opendal"
                kind = "fs"
                polling_interval = "1s"
                parameters = {{ root = "{}" }}
                recursive = false
                path_patterns = ["**/*.json"]
                parser = "json"
                "#,
                link_dir_path
            ),
            ..Default::default()
        },
        &input_events,
        Duration::from_secs(15), // Allow max time for both connectors to process
    )
    .await
    .unwrap();
}
