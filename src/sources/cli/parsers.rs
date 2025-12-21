//! Parser configuration and implementation for CLI source.
//!
//! This module provides format parsing for CLI input data, including
//! auto-detection based on file extension and explicit format specification.

use crate::errors::Result;
use serde::{Deserialize, Serialize};

/// Parser configuration for CLI source.
///
/// Determines how input data should be parsed. Supports auto-detection
/// based on file extension or explicit format specification.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Config {
    /// Auto-detect format based on file extension.
    /// Falls back to JSON for unknown extensions.
    #[default]
    Auto,
    /// Explicitly parse as JSON.
    Json,
    /// Explicitly parse as XML.
    Xml,
}

/// Parse data according to the specified parser configuration.
///
/// # Arguments
/// * `data` - The input data as a string
/// * `config` - The parser configuration specifying the format
/// * `filename` - Optional filename for auto-detection (used when config is Auto)
///
/// # Returns
/// A `serde_json::Value` representing the parsed data
///
/// # Errors
/// Returns an error if parsing fails for the specified format
pub(crate) fn parse_with_config(
    data: &str,
    config: &Config,
    filename: Option<&str>,
) -> Result<serde_json::Value> {
    match config {
        Config::Auto => {
            // Auto-detect based on file extension
            match filename.and_then(|f| std::path::Path::new(f).extension()?.to_str()) {
                Some("xml") => super::super::format_converters::parse_xml(data),
                Some("json") => super::super::format_converters::parse_json(data),
                _ => {
                    // Default fallback to JSON for unknown extensions
                    super::super::format_converters::parse_json(data)
                }
            }
        }
        Config::Json => super::super::format_converters::parse_json(data),
        Config::Xml => super::super::format_converters::parse_xml(data),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_with_config_json() {
        let data = r#"{"test": "value"}"#;
        let config = Config::Json;
        let result = parse_with_config(data, &config, None).unwrap();
        assert_eq!(result["test"], "value");
    }

    #[test]
    fn test_parse_with_config_auto_json_extension() {
        let data = r#"{"test": "value"}"#;
        let config = Config::Auto;
        let result = parse_with_config(data, &config, Some("data.json")).unwrap();
        assert_eq!(result["test"], "value");
    }

    #[test]
    fn test_parse_with_config_auto_fallback() {
        let data = r#"{"test": "value"}"#;
        let config = Config::Auto;
        // Unknown extension should fall back to JSON
        let result = parse_with_config(data, &config, Some("data.txt")).unwrap();
        assert_eq!(result["test"], "value");
    }

    #[test]
    fn test_parse_with_config_auto_no_filename() {
        let data = r#"{"test": "value"}"#;
        let config = Config::Auto;
        // No filename should fall back to JSON
        let result = parse_with_config(data, &config, None).unwrap();
        assert_eq!(result["test"], "value");
    }

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert!(matches!(config, Config::Auto));
    }

    #[test]
    fn test_config_deserialization() {
        let json = r#""auto""#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(matches!(config, Config::Auto));

        let json = r#""json""#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(matches!(config, Config::Json));

        let json = r#""xml""#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(matches!(config, Config::Xml));
    }

    #[test]
    fn test_parse_with_config_xml() {
        let data = "<root><item>value</item></root>";
        let config = Config::Xml;
        let result = parse_with_config(data, &config, None).unwrap();
        assert!(result.is_object());
    }

    #[test]
    fn test_parse_with_config_auto_xml_extension() {
        let data = "<root><item>value</item></root>";
        let config = Config::Auto;
        let result = parse_with_config(data, &config, Some("data.xml")).unwrap();
        assert!(result.is_object());
    }

    #[test]
    fn test_parse_with_config_auto_mixed_extensions() {
        // Test JSON detection
        let json_data = r#"{"test": "value"}"#;
        let config = Config::Auto;
        let result = parse_with_config(json_data, &config, Some("data.json")).unwrap();
        assert_eq!(result["test"], "value");

        // Test XML detection
        let xml_data = "<root><item>value</item></root>";
        let result = parse_with_config(xml_data, &config, Some("data.xml")).unwrap();
        assert!(result.is_object());
    }
}
