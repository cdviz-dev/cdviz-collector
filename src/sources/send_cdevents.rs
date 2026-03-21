use super::EventSource;
use crate::Message;
use crate::errors::{IntoDiagnostic, Result};
use crate::pipes::Pipe;
use cdevents_sdk::CDEvent;

use tokio::sync::broadcast::Sender;

pub(crate) struct Processor {
    next: Sender<Message>,
    default_source_url: String,
}

impl Processor {
    pub(crate) fn new(next: Sender<Message>, default_source_url: String) -> Self {
        Self { next, default_source_url }
    }
}

impl Pipe for Processor {
    type Input = EventSource;
    fn send(&mut self, mut input: Self::Input) -> Result<()> {
        let is_source_empty = |v: &serde_json::Value| {
            v.get("context")
                .and_then(|c| c.get("source"))
                .and_then(|s| s.as_str())
                .is_none_or(str::is_empty)
        };
        if is_source_empty(&input.body) && is_source_empty(&input.metadata) {
            input.metadata["context"]["source"] = serde_json::json!(self.default_source_url);
        }
        let cdevent = CDEvent::try_from(input.clone())?;

        // Include headers from EventSource into the message and capture trace context
        let message = Message::with_trace_context(cdevent, input.headers);

        self.next.send(message).into_diagnostic()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::EventSource;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn test_header_passthrough() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(10);
        let mut processor =
            Processor::new(tx.clone(), "http://test.example.com/?source=test".to_string());

        // Create an EventSource with headers
        let mut headers = HashMap::new();
        headers.insert("X-Source-Header".to_string(), "test-value".to_string());
        headers.insert("Authorization".to_string(), "Bearer token123".to_string());

        let event_source = EventSource {
            metadata: json!({}),
            headers: headers.clone(),
            body: json!({
                "context": {
                    "version": "0.4.0",
                    "id": "test-id",
                    "source": "test-source",
                    "type": "dev.cdevents.service.deployed.0.1.1",
                    "timestamp": "2024-03-14T10:30:00Z"
                },
                "subject": {
                    "id": "test-subject",
                    "source": "test-source",
                    "type": "service",
                    "content": {
                        "environment": {
                            "id": "test-env"
                        },
                        "artifactId": "pkg:test/artifact@1.0.0"
                    }
                }
            }),
        };

        // Send the event source through the processor
        processor.send(event_source).unwrap();

        // Verify the message was sent with headers preserved
        let message = rx.try_recv().unwrap();

        // Check that headers were preserved
        assert_eq!(message.headers.len(), 2);
        assert_eq!(message.headers.get("X-Source-Header").unwrap(), "test-value");
        assert_eq!(message.headers.get("Authorization").unwrap(), "Bearer token123");
    }
}
