use super::Sink;
use crate::Message;
use crate::errors::{Report, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub(crate) struct Config {
    /// Is the sink is enabled?
    pub(crate) enabled: bool,
}

impl TryFrom<Config> for DebugSink {
    type Error = Report;

    fn try_from(_value: Config) -> Result<Self> {
        Ok(DebugSink {})
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DebugSink {}

impl Sink for DebugSink {
    #[tracing::instrument(skip(self, msg), fields(cdevent_id = %msg.cdevent.id()))]
    async fn send(&self, msg: &Message) -> Result<()> {
        tracing::info!(cdevent=?msg.cdevent, "mock sending");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::let_assert;

    #[test_strategy::proptest(
        async = "tokio",
        proptest::prelude::ProptestConfig::default(),
        cases = 100
    )]
    async fn test_debug_sink_successful_send(msg: Message) {
        let config = Config { enabled: true };
        let sink = DebugSink::try_from(config).unwrap();

        let_assert!(Ok(()) = sink.send(&msg).await);
    }

    #[test_strategy::proptest(
        async = "tokio",
        proptest::prelude::ProptestConfig::default(),
        cases = 10
    )]
    async fn test_debug_sink_concurrent_sends(msg: Message) {
        let config = Config { enabled: true };
        let sink = DebugSink::try_from(config).unwrap();

        let tasks = (0..10).map(|_| {
            let sink = sink.clone();
            let msg = msg.clone();
            tokio::spawn(async move { sink.send(&msg).await })
        });

        let results = futures::future::join_all(tasks).await;
        for result in results {
            let_assert!(Ok(Ok(())) = result);
        }
    }

    #[test]
    fn test_debug_sink_config_creation() {
        let config = Config { enabled: true };
        let_assert!(Ok(_) = DebugSink::try_from(config));

        let config = Config { enabled: false };
        let_assert!(Ok(_) = DebugSink::try_from(config));
    }

    #[test]
    fn test_debug_sink_serialization() {
        let config = Config { enabled: true };
        let serialized = serde_json::to_string(&config).unwrap();
        assert!(serialized.contains("true"));

        let config = Config { enabled: false };
        let serialized = serde_json::to_string(&config).unwrap();
        assert!(serialized.contains("false"));
    }

    #[test]
    fn test_debug_sink_deserialization() {
        let json = r#"{"enabled": true}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(config.enabled);

        let json = r#"{"enabled": false}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(!config.enabled);
    }
}
