# Connector Chain Test Framework

A simple, opinionated framework for testing chained connectors in cdviz-collector.

## Overview

This framework allows you to test scenarios where:

- **Connector A** reads events from a folder and sends them to a configurable sink
- **Connector B** receives events from a configurable source and writes them to a folder
- The test validates that events processed through both connectors match the original input

## API

### Main Test Function

```rust
pub async fn test_connector_chain(
    connector_a_http_port: Option<u16>, // the HTTP port to use for connector A (None => random)
    connector_a_config: &str,      // TOML fragment for connector A sink
    connector_a_http_port: Option<u16>, // the HTTP port to use for connector B (None => random)
    connector_b_config: &str,      // TOML fragment for connector B source
    input_events: &[CDEvent],      // CDEvents to process
    wait_duration: Duration,       // How long to wait before checking output
) -> Result<()>
```

### Utility Functions

#### Random CDEvent Generation

```rust
// Generate random CDEvents using Arbitrary trait
pub fn generate_random_cdevents(count: usize) -> Result<Vec<CDEvent>>
```

#### CDEvent Conversion

```rust
// Convert single CDEvent to JSON string
pub fn cdevent_to_json(event: &CDEvent) -> Result<String>

// Compare two lists of CDEvents and return differences
pub fn compare_cdevents(expected: &[CDEvent], actual: &[CDEvent]) -> Result<Vec<String>>
```

### Framework Responsibilities

1. **Environment Setup**: Creates temporary directory structure:

   ```
   temp_dir/
   ├── input/           # Input events as JSON files
   ├── output/          # Output folder for connector B
   ├── config/          # Generated configurations
   │   ├── connector_a.toml
   │   └── connector_b.toml
   ```

2. **Configuration Generation**:
   - Connector A: folder source (input/) + provided sink config
   - Connector B: provided source config + folder sink (output/)

3. **Connector Lifecycle**: Launch both connectors, wait, then shutdown

4. **Validation**: Compare input events with output events (CDEvent comparison by ID)

## Usage Examples

### Using Random CDEvents

```rust
use framework::{test_connector_chain, generate_random_cdevents};

#[tokio::test]
async fn test_with_random_events() {
    // Generate 10 random CDEvents
    let input_events = generate_random_cdevents(10).unwrap();

    test_connector_chain(
        // connector configs...
        &input_events,
        Duration::from_secs(5)
    ).await.unwrap();
}
```

## Environment Setup

**User Responsibility**: Set up third-party services (containers) before calling the framework:

- Kafka/Redpanda containers for Kafka tests
- Mock HTTP servers for webhook tests
- Database containers for DB sink tests

**Framework Responsibility**:

- Temporary directory management
- Configuration generation and merging
- Connector process lifecycle
- Input/output validation

## Validation

The framework performs CDEvent comparison between input and output files:

- Input events stored as `<context.id>.json` files using sanitized event IDs
- Output JSONs parsed back to CDEvents for direct comparison
- Both lists sorted by event ID for consistent comparison
- Detailed difference reporting for validation failures

## Supported Combinations

The framework supports testing any combination of sources and sinks:

- **Sources**: folder, webhook, SSE, Kafka, database
- **Sinks**: folder, HTTP, SSE, Kafka, database, debug

Popular test scenarios:

- Folder → HTTP → SSE → Folder
- Folder → Kafka → HTTP → Folder
- Folder → Webhook → Kafka → Folder
- Function → HTTP → Database → Folder

## Features

- **Simple API**: Single function call with string configurations
- **Automatic Cleanup**: Temporary directories cleaned up automatically
- **JSON Validation**: Content-aware comparison ignoring formatting
- **Error Handling**: Detailed error reporting for troubleshooting
- **Security Testing**: Built-in support for header/trace validation
- **Container Agnostic**: Works with any container management approach
