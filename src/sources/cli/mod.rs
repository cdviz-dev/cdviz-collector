pub(crate) mod parsers;

use super::{EventSource, EventSourcePipe};
use crate::errors::{IntoDiagnostic, Result};
use crate::pipes::Pipe;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Cursor, Read},
};
use tracing::instrument;

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub(crate) struct Config {
    /// Data source specification - can be direct JSON, @filename, or @- for stdin
    pub data: Option<String>,
    /// Parser configuration - specifies how to parse the input data
    #[serde(default)]
    pub parser: parsers::Config,
    /// Base metadata to include in all `EventSource` instances created by this extractor.
    /// The `context.source` field will be automatically populated if not set.
    #[serde(default)]
    pub metadata: serde_json::Value,
}

pub(crate) struct CliExtractor {
    reader: Box<dyn Read + Send>,
    next: EventSourcePipe,
    base_metadata: serde_json::Value,
    parser_config: parsers::Config,
    data_source: Option<String>, // Filename for auto-detection
}

impl CliExtractor {
    pub(crate) fn new(
        reader: Box<dyn Read + Send>,
        base_metadata: serde_json::Value,
        parser_config: parsers::Config,
        data_source: Option<String>,
        next: EventSourcePipe,
    ) -> Self {
        Self { reader, next, base_metadata, parser_config, data_source }
    }

    pub(crate) fn from_config(config: &Config, next: EventSourcePipe) -> Result<Self> {
        let data = config
            .data
            .as_ref()
            .ok_or_else(|| miette::miette!("CLI source requires 'data' configuration"))?;

        let reader = create_reader_from_data(data)?;

        // Extract filename from data source for auto-detection
        let filename = extract_filename(data);

        Ok(Self::new(reader, config.metadata.clone(), config.parser.clone(), filename, next))
    }

    #[instrument(skip(self))]
    pub(crate) async fn run(mut self) -> Result<()> {
        // Read all data from the stream
        let mut data = String::new();
        self.reader.read_to_string(&mut data).into_diagnostic()?;

        if data.trim().is_empty() {
            tracing::warn!("No data provided to CLI source");
            return Ok(());
        }

        // Parse using configured parser
        let json_value: serde_json::Value =
            parsers::parse_with_config(&data, &self.parser_config, self.data_source.as_deref())?;

        match json_value {
            serde_json::Value::Array(events) => {
                tracing::info!(count = events.len(), "processing array of events");
                for (index, event) in events.into_iter().enumerate() {
                    let mut metadata = self.base_metadata.clone();
                    // Add index to metadata for array events
                    if let Some(obj) = metadata.as_object_mut() {
                        obj.insert("index".to_string(), serde_json::json!(index));
                    }

                    let event_source = EventSource {
                        body: event,
                        metadata,
                        headers: std::collections::HashMap::new(),
                    };

                    if let Err(err) = self.next.send(event_source) {
                        tracing::warn!(?err, index, "failed to send event");
                    }
                }
            }
            single_event => {
                tracing::info!("processing single event");
                let event_source = EventSource {
                    body: single_event,
                    metadata: self.base_metadata.clone(),
                    headers: std::collections::HashMap::new(),
                };

                if let Err(err) = self.next.send(event_source) {
                    tracing::warn!(?err, "failed to send event");
                }
            }
        }

        Ok(())
    }
}

/// Create a reader from data specification (direct string, @file, or @- for stdin).
fn create_reader_from_data(data: &str) -> Result<Box<dyn Read + Send>> {
    if let Some(path) = data.strip_prefix('@') {
        if path == "-" {
            // Read from stdin
            Ok(Box::new(std::io::stdin()))
        } else {
            // Read from file
            let file = File::open(path).into_diagnostic()?;
            Ok(Box::new(file))
        }
    } else {
        // Direct JSON string
        Ok(Box::new(Cursor::new(data.to_string())))
    }
}

/// Extract filename from data specification for auto-detection.
///
/// Returns the filename if data is a file reference (@filename), None otherwise.
fn extract_filename(data: &str) -> Option<String> {
    data.strip_prefix('@').and_then(|path| {
        if path == "-" {
            None // stdin has no filename
        } else {
            Some(path.to_string())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipes::collect_to_vec::Collector;
    use std::io::Cursor;

    #[tokio::test]
    async fn test_cli_extractor_single_event() {
        let json_data = r#"{"test": "value"}"#;
        let reader = Box::new(Cursor::new(json_data));

        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let base_metadata =
            serde_json::json!({"context": {"source": "http://example.com?source=cli"}});
        let parser_config = parsers::Config::default();
        let extractor = CliExtractor::new(reader, base_metadata, parser_config, None, pipe);

        // Run the extractor
        extractor.run().await.unwrap();

        // Check collected events
        let events: Vec<EventSource> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].body["test"], "value");
        assert_eq!(events[0].metadata["context"]["source"], "http://example.com?source=cli");
    }

    #[tokio::test]
    async fn test_cli_extractor_array_events() {
        let json_data = r#"[{"test": "value1"}, {"test": "value2"}]"#;
        let reader = Box::new(Cursor::new(json_data));

        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let base_metadata =
            serde_json::json!({"context": {"source": "http://example.com?source=cli"}});
        let parser_config = parsers::Config::default();
        let extractor = CliExtractor::new(reader, base_metadata, parser_config, None, pipe);

        // Run the extractor
        extractor.run().await.unwrap();

        // Check collected events
        let events: Vec<EventSource> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].body["test"], "value1");
        assert_eq!(events[0].metadata["index"], 0);
        assert_eq!(events[1].body["test"], "value2");
        assert_eq!(events[1].metadata["index"], 1);
    }

    #[tokio::test]
    async fn test_cli_extractor_empty_data() {
        let json_data = "";
        let reader = Box::new(Cursor::new(json_data));

        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let base_metadata = serde_json::json!({});
        let parser_config = parsers::Config::default();
        let extractor = CliExtractor::new(reader, base_metadata, parser_config, None, pipe);

        // Should not fail with empty data
        extractor.run().await.unwrap();

        let events: Vec<EventSource> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 0);
    }

    #[tokio::test]
    async fn test_cli_extractor_invalid_json() {
        let json_data = "invalid json";
        let reader = Box::new(Cursor::new(json_data));

        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let base_metadata = serde_json::json!({});
        let parser_config = parsers::Config::default();
        let extractor = CliExtractor::new(reader, base_metadata, parser_config, None, pipe);

        // Should fail with invalid JSON
        assert!(extractor.run().await.is_err());
    }
}
