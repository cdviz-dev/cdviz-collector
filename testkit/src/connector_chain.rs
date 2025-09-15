#![allow(clippy::unwrap_used)]

use super::cdevent_utils::{cdevent_to_json, compare_cdevents};
use cdevents_sdk::CDEvent;
use cdviz_collector::{cdevent_utils::sanitize_id, run_with_args_and_log};
use indoc::formatdoc;
use miette::{IntoDiagnostic, Result, miette};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::time::sleep;

#[derive(Default)]
pub struct ConnectorSetup<'a> {
    pub http_port: u16,
    pub config: &'a str,
}

/// Test framework for chaining two connectors to validate end-to-end event processing
///
/// This framework:
/// 1. Sets up a temporary directory structure with input/output folders
/// 2. Writes input events as JSON files
/// 3. Generates configurations for both connectors
/// 4. Launches connector A (folder source -> provided sink)
/// 5. Launches connector B (provided source -> folder sink)
/// 6. Waits for processing to complete
/// 7. Validates output matches input using existing difference tools
#[allow(clippy::similar_names)]
pub async fn test_connector_chain<'a>(
    connector_a: ConnectorSetup<'a>,
    connector_b: ConnectorSetup<'a>,
    input_events: &[CDEvent],
    timeout_duration: Duration,
) -> Result<()> {
    // Setup temporary environment
    let env = setup_temp_environment(input_events)?;

    // Generate connector configurations
    let connector_a_config_path =
        generate_connector_a_config(&env, connector_a.http_port, connector_a.config)?;
    let connector_b_config_path =
        generate_connector_b_config(&env, connector_b.http_port, connector_b.config)?;

    // Launch both connectors
    // multiple log/tracing configuration can generate confusion and random error
    // like `tried to drop a ref to Id(...), but no such span exists!`
    let connector_a_task = launch_connector(&connector_a_config_path, true);
    let connector_b_task = launch_connector(&connector_b_config_path, false);

    // Wait for startup
    sleep(Duration::from_millis(200)).await;

    if connector_a_task.is_finished() {
        connector_b_task.abort();
        return Err(miette!("connector a early stop"));
    }
    if connector_b_task.is_finished() {
        connector_a_task.abort();
        return Err(miette!("connector b early stop"));
    }

    // Wait for processing
    let wait_duration = Duration::from_millis(500);
    let t0 = Instant::now();
    let expected_count = count_files_in_directory(&env.input_dir)?;
    while t0.elapsed() < timeout_duration
        && expected_count > count_files_in_directory(&env.output_dir)?
    {
        sleep(wait_duration).await;
    }

    // Shutdown connectors
    connector_a_task.abort();
    connector_b_task.abort();

    // Give time for graceful shutdown
    sleep(Duration::from_millis(100)).await;

    // Validate output matches input
    validate_output(&env, input_events)?;

    Ok(())
}

/// compute the number of files in a directory
fn count_files_in_directory(dir_path: &Path) -> Result<usize> {
    let entries = std::fs::read_dir(dir_path)
        .map_err(|e| miette::miette!("Failed to read directory {:?}: {}", dir_path, e))?;

    let mut count = 0;
    for entry in entries {
        let entry = entry.map_err(|e| miette::miette!("Failed to read directory entry: {}", e))?;
        if entry.path().is_file() {
            count += 1;
        }
    }

    Ok(count)
}

/// Test environment containing temporary directories and paths
#[allow(clippy::struct_field_names)]
pub struct TestEnvironment {
    #[allow(dead_code)] // Needs to be kept alive for automatic cleanup
    pub temp_dir: TempDir,
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
    pub config_dir: PathBuf,
}

/// Sets up the temporary directory structure and writes input events
fn setup_temp_environment(input_events: &[CDEvent]) -> Result<TestEnvironment> {
    let temp_dir =
        TempDir::new().map_err(|e| miette::miette!("Failed to create temp dir: {}", e))?;

    let input_dir = temp_dir.path().join("input");
    let output_dir = temp_dir.path().join("output");
    let config_dir = temp_dir.path().join("config");

    // Create directories
    std::fs::create_dir_all(&input_dir)
        .map_err(|e| miette::miette!("Failed to create input dir: {}", e))?;
    std::fs::create_dir_all(&output_dir)
        .map_err(|e| miette::miette!("Failed to create output dir: {}", e))?;
    std::fs::create_dir_all(&config_dir)
        .map_err(|e| miette::miette!("Failed to create config dir: {}", e))?;

    // Write input events as JSON files using <context.id>.json naming
    // Sanitize the event ID for use as filename
    for event in input_events {
        let sanitized_id = sanitize_id(event.id())?;
        let filename = format!("{sanitized_id}.json");
        let filepath = input_dir.join(&filename);
        let event_json = cdevent_to_json(event)?;
        std::fs::write(&filepath, event_json)
            .map_err(|e| miette::miette!("Failed to write input event {}: {}", filename, e))?;
    }

    Ok(TestEnvironment { temp_dir, input_dir, output_dir, config_dir })
}

/// Generates configuration for connector A (folder source -> provided sink)
fn generate_connector_a_config(
    env: &TestEnvironment,
    http_port: u16,
    additional_config: &str,
) -> Result<PathBuf> {
    let config_content = formatdoc!(
        r#"
        # Connector A: Folder source -> Configurable sink
        [http]
        host = "0.0.0.0"
        port = {http_port}

        {additional_config}

        [sources.folder_source]
        enabled = true

        [sources.folder_source.extractor]
        type = "opendal"
        kind = "fs"
        polling_interval = "1s"
        parameters = {{ root = "{}" }}
        recursive = false
        path_patterns = ["**/*.json"]
        parser = "json"
        "#,
        env.input_dir.to_string_lossy().replace('\\', "\\\\"),
    );

    let config_path = env.config_dir.join("connector_a.toml");
    std::fs::write(&config_path, config_content)
        .map_err(|e| miette::miette!("Failed to write connector A config: {}", e))?;

    Ok(config_path)
}

/// Generates configuration for connector B (provided source -> folder sink)
fn generate_connector_b_config(
    env: &TestEnvironment,
    http_port: u16,
    additional_config: &str,
) -> Result<PathBuf> {
    let config_content = formatdoc!(
        r#"
        # Connector B: Configurable source -> Folder sink
        [http]
        host = "0.0.0.0"
        port = {http_port}

        {additional_config}

        [sinks.folder_sink]
        enabled = true
        type = "folder"
        kind = "fs"
        parameters = {{ root = "{}" }}
        "#,
        env.output_dir.to_string_lossy().replace('\\', "\\\\")
    );

    let config_path = env.config_dir.join("connector_b.toml");
    std::fs::write(&config_path, config_content)
        .map_err(|e| miette::miette!("Failed to write connector B config: {}", e))?;

    Ok(config_path)
}

/// Launches a connector process using the cdviz-collector library
#[allow(clippy::print_stderr)]
#[allow(clippy::disallowed_macros)]
fn launch_connector(config_path: &Path, with_init_log: bool) -> tokio::task::JoinHandle<bool> {
    let config_path = config_path.to_string_lossy().to_string();
    tokio::spawn(async move {
        let result =
            run_with_args_and_log(vec!["connect", "-vvv", "--config", &config_path], with_init_log)
                .await;
        // tracing::warn!("connector stopped with {:?}", result);
        eprintln!(">>> connector stopped with {result:?}");
        result.unwrap_or(false)
    })
}

/// Validates that output files match input files by comparing `CDEvents` directly
fn validate_output(env: &TestEnvironment, expected_events: &[CDEvent]) -> Result<()> {
    // Read and parse output files into CDEvents
    let mut actual_events = Vec::new();

    let output_dir_entries = std::fs::read_dir(&env.output_dir)
        .map_err(|e| miette::miette!("Failed to read output directory: {}", e))?;

    for entry in output_dir_entries {
        let entry =
            entry.map_err(|e| miette::miette!("Failed to read output directory entry: {}", e))?;
        let path = entry.path();

        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("json") {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| miette::miette!("Failed to read output file {:?}: {}", path, e))?;

            let cdevent: CDEvent = serde_json::from_str(&content).map_err(|e| {
                miette::miette!("Failed to parse CDEvent from file {:?}: {}", path, e)
            })?;

            actual_events.push(cdevent);
        }
    }

    tracing::info!(
        "Validating {} expected events against {} actual events",
        expected_events.len(),
        actual_events.len()
    );

    // Use the comparison utility to find differences
    let differences = compare_cdevents(expected_events, &actual_events)?;

    if !differences.is_empty() {
        let mut error_msg = String::from("CDEvent validation failed:\n");
        for diff in &differences {
            writeln!(&mut error_msg, "  - {diff}").into_diagnostic()?;
        }
        return Err(miette::miette!("{}", error_msg));
    }

    tracing::info!(
        "CDEvent validation successful - all {} events processed correctly",
        expected_events.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn test_environment_setup() {
        use crate::cdevent_utils::generate_random_cdevents;

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let events = generate_random_cdevents(2).unwrap();

            let env = setup_temp_environment(&events).unwrap();

            // Check directory structure
            assert!(env.input_dir.exists());
            assert!(env.output_dir.exists());
            assert!(env.config_dir.exists());

            let event1_path = env.input_dir.join(format!("{}.json", &events[0].id()));
            let event2_path = env.input_dir.join(format!("{}.json", &events[1].id()));

            assert!(event1_path.exists());
            assert!(event2_path.exists());

            // Verify the contents can be parsed back as CDEvents
            let content1 = std::fs::read_to_string(&event1_path).unwrap();
            let content2 = std::fs::read_to_string(&event2_path).unwrap();

            let parsed_event1: CDEvent = serde_json::from_str(&content1).unwrap();
            let parsed_event2: CDEvent = serde_json::from_str(&content2).unwrap();

            assert_eq!(parsed_event1.id(), events[0].id());
            assert_eq!(parsed_event2.id(), events[1].id());
        });
    }

    #[test]
    #[allow(clippy::similar_names)]
    fn test_config_generation() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let env = setup_temp_environment(&[]).unwrap();

            // Test connector A config generation
            let config_a_path = generate_connector_a_config(
                &env,
                0,
                indoc! {r#"
                [sinks.test_sink]
                enabled = true
                type = "debug"
                "#},
            )
            .unwrap();

            let config_a_content = std::fs::read_to_string(&config_a_path).unwrap();
            assert!(config_a_content.contains("folder_source"));
            assert!(config_a_content.contains("test_sink"));
            assert!(
                config_a_content.contains(&env.input_dir.to_string_lossy().replace('\\', "\\\\"))
            );

            // Test connector B config generation
            let config_b_path = generate_connector_b_config(
                &env,
                0,
                indoc! {r#"
                    [sources.test_source]
                    enabled = true
                    [sources.test_source.extractor]
                    type = "webhook"
                    id = "test"
                    "#
                },
            )
            .unwrap();

            let config_b_content = std::fs::read_to_string(&config_b_path).unwrap();
            assert!(config_b_content.contains("test_source"));
            assert!(config_b_content.contains("folder_sink"));
            assert!(
                config_b_content.contains(&env.output_dir.to_string_lossy().replace('\\', "\\\\"))
            );
        });
    }
}
