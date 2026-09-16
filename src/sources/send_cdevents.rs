use super::EventSource;
use crate::Message;
use crate::errors::{Error, IntoDiagnostic, Result};
use crate::transformers::Pipe;
use cdevents_sdk::CDEvent;

use tokio::sync::broadcast::Sender;

/// Fraction of `queue_capacity` above which the queue is considered saturated.
/// Leaves headroom so a burst that is already in flight still fits.
const HIGH_WATERMARK_RATIO: f64 = 0.8;

pub(crate) struct Processor {
    next: Sender<Message>,
    default_source_url: String,
    /// Queue depth above which `send` refuses new events instead of enqueuing them, or
    /// `None` for a source that cannot propagate backpressure (see
    /// [`crate::sources::extractors::Config::supports_backpressure`]).
    ///
    /// The source→sink channel is a `broadcast`, which drops the *oldest* queued messages
    /// for any receiver that falls behind (`RecvError::Lagged`), silently and
    /// unrecoverably. Refusing here turns that silent loss into an error the source can
    /// surface — the webhook source answers 503 so a bulk sender (a backfill job) retries
    /// and throttles itself to the slowest sink.
    high_watermark: Option<usize>,
}

impl Processor {
    /// `queue_capacity` is `Some` only for a source that can make its producer retry;
    /// `None` disables the check entirely.
    pub(crate) fn new(
        next: Sender<Message>,
        default_source_url: String,
        queue_capacity: Option<usize>,
    ) -> Self {
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]
        let high_watermark = queue_capacity
            .map(|cap| (((cap as f64) * HIGH_WATERMARK_RATIO) as usize).max(1));
        Self { next, default_source_url, high_watermark }
    }
}

impl Pipe for Processor {
    type Input = EventSource;
    fn send(&mut self, mut input: Self::Input) -> Result<()> {
        // Checked before any work: an event we would have to drop is better refused.
        if let Some(watermark) = self.high_watermark {
            let queued = self.next.len();
            if queued >= watermark {
                crate::otel::source_queue_saturated_counter().add(1, &[]);
                return Err(Error::QueueSaturated { queued, capacity: watermark }.into());
            }
        }
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

        // Carry the source trace context across the broadcast queue by injecting the current
        // span's W3C `traceparent` into the message headers (extracted on the sink side).
        let mut headers = input.headers;
        crate::otel::inject_current_context(&mut headers);
        let message = Message::new(cdevent, headers);

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
            Processor::new(tx.clone(), "http://test.example.com/?source=test".to_string(), Some(10));

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
