use super::{EventSource, EventSourcePipe};
use crate::errors::{IntoDiagnostic, Result};
use crate::pipes::Pipe;
use serde::{Deserialize, Serialize};
use std::io::Read;
use tokio_util::sync::CancellationToken;
use tracing::instrument;

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub(crate) struct Config {
    // CLI extractor doesn't need configuration - data comes from the stream
}

pub(crate) struct CliExtractor {
    reader: Box<dyn Read + Send>,
    next: EventSourcePipe,
}

impl CliExtractor {
    pub(crate) fn new(reader: Box<dyn Read + Send>, next: EventSourcePipe) -> Self {
        Self { reader, next }
    }

    #[instrument(skip(self))]
    pub(crate) async fn run(mut self, _cancel_token: CancellationToken) -> Result<()> {
        // Read all data from the stream
        let mut data = String::new();
        self.reader.read_to_string(&mut data).into_diagnostic()?;

        if data.trim().is_empty() {
            tracing::warn!("No data provided to CLI source");
            return Ok(());
        }

        // Parse JSON - could be single object or array
        let json_value: serde_json::Value = serde_json::from_str(&data).into_diagnostic()?;

        match json_value {
            serde_json::Value::Array(events) => {
                tracing::info!(count = events.len(), "processing array of events");
                for (index, event) in events.into_iter().enumerate() {
                    let event_source = EventSource {
                        body: event,
                        metadata: serde_json::json!({"source": "cli", "index": index}),
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
                    metadata: serde_json::json!({"source": "cli"}),
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
        let extractor = CliExtractor::new(reader, pipe);

        let cancel_token = CancellationToken::new();

        // Run the extractor
        extractor.run(cancel_token).await.unwrap();

        // Check collected events
        let events: Vec<EventSource> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].body["test"], "value");
        assert_eq!(events[0].metadata["source"], "cli");
    }

    #[tokio::test]
    async fn test_cli_extractor_array_events() {
        let json_data = r#"[{"test": "value1"}, {"test": "value2"}]"#;
        let reader = Box::new(Cursor::new(json_data));

        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let extractor = CliExtractor::new(reader, pipe);

        let cancel_token = CancellationToken::new();

        // Run the extractor
        extractor.run(cancel_token).await.unwrap();

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
        let extractor = CliExtractor::new(reader, pipe);

        let cancel_token = CancellationToken::new();

        // Should not fail with empty data
        extractor.run(cancel_token).await.unwrap();

        let events: Vec<EventSource> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 0);
    }

    #[tokio::test]
    async fn test_cli_extractor_invalid_json() {
        let json_data = "invalid json";
        let reader = Box::new(Cursor::new(json_data));

        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let extractor = CliExtractor::new(reader, pipe);

        let cancel_token = CancellationToken::new();

        // Should fail with invalid JSON
        assert!(extractor.run(cancel_token).await.is_err());
    }
}
