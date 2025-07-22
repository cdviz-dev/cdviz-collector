use crate::security::header::OutgoingHeaderConfig;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// The SSE endpoint URL to connect to
    pub url: String,

    /// Headers to include in the SSE request
    #[serde(default)]
    pub headers: Vec<OutgoingHeaderConfig>,

    /// Maximum number of reconnection attempts (default: 10)
    pub max_retries: Option<u32>,

    /// Whether the SSE source is enabled
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            url: "http://localhost:8080/sse/001".to_string(),
            headers: vec![],
            max_retries: Some(10),
            enabled: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.url, "http://localhost:8080/sse/001");
        assert!(config.headers.is_empty());
        assert_eq!(config.max_retries, Some(10));
        assert!(config.enabled);
    }

    #[test]
    fn test_config_serialization() {
        use crate::security::header::{HeaderSource, OutgoingHeaderConfig};

        let config = Config {
            url: "https://example.com/events".to_string(),
            headers: vec![OutgoingHeaderConfig {
                header: "Authorization".to_string(),
                rule: HeaderSource::Static { value: "Bearer token".to_string() },
            }],
            max_retries: Some(5),
            enabled: true,
        };

        let serialized = toml::to_string(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();

        assert_eq!(config.url, deserialized.url);
        assert_eq!(config.headers.len(), deserialized.headers.len());
        assert_eq!(config.max_retries, deserialized.max_retries);
        assert_eq!(config.enabled, deserialized.enabled);
    }
}
