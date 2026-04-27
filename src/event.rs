use crate::errors::{IntoDiagnostic, Result};
use crate::pipes::Pipe;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Shared event type flowing through the pipeline on both source and sink sides.
///
/// - `.body` — the event payload (raw JSON on source side, `CDEvent` JSON on sink side)
/// - `.headers` — HTTP headers propagated through the pipeline
/// - `.metadata` — transformer-to-transformer communication; not sent to destinations
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct Event {
    pub metadata: Value,
    pub headers: HashMap<String, String>,
    pub body: Value,
}

// TODO explore enum_dispatch instead of Box<dyn> on EventPipe (recursive structure)
pub type EventPipe = Box<dyn Pipe<Input = Event> + Send + Sync>;

pub(crate) fn message_to_event(msg: &crate::Message) -> Result<Event> {
    let body = serde_json::to_value(&msg.cdevent).into_diagnostic()?;
    Ok(Event { body, metadata: serde_json::json!({}), headers: msg.headers.clone() })
}
