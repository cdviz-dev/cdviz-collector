use crate::errors::{IntoDiagnostic, Result};
use crate::transformers::Pipe;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Shared event type flowing through the pipeline on both source and sink sides.
///
/// - `.body` — the event payload (raw JSON on source side, `CDEvent` JSON on sink side)
/// - `.headers` — HTTP headers propagated through the pipeline
/// - `.metadata` — transformer-to-transformer communication; not sent to destinations
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Event {
    pub metadata: Value,
    pub headers: HashMap<String, String>,
    pub body: Value,
}

// TODO explore enum_dispatch instead of Box<dyn> on EventPipe (recursive structure)
/// Internal pipeline plumbing type, exposed only because `Event` (its type parameter) is
/// public. `Pipe` itself lives in a private module and isn't implementable from outside
/// this crate — `EventPipe` isn't meant as an extension point for external consumers.
pub type EventPipe = Box<dyn Pipe<Input = Event> + Send + Sync>;

pub(crate) fn message_to_event(msg: &crate::Message) -> Result<Event> {
    let body = serde_json::to_value(&msg.cdevent).into_diagnostic()?;
    Ok(Event { body, metadata: serde_json::json!({}), headers: msg.headers.clone() })
}
