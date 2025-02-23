use super::EventSource;
use crate::Message;
use crate::errors::{IntoDiagnostic, Result};
use crate::pipes::Pipe;
use cdevents_sdk::CDEvent;

use tokio::sync::broadcast::Sender;

pub(crate) struct Processor {
    next: Sender<Message>,
}

impl Processor {
    pub(crate) fn new(next: Sender<Message>) -> Self {
        Self { next }
    }
}

impl Pipe for Processor {
    type Input = EventSource;
    fn send(&mut self, input: Self::Input) -> Result<()> {
        // TODO if source is empty, set a default value based on configuration TBD
        let cdevent = CDEvent::try_from(input)?;

        // TODO include headers into message
        self.next.send(cdevent.into()).into_diagnostic()?;
        Ok(())
    }
}
