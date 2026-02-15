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
    #[cfg(feature = "parser_xml")]
    Xml,
    /// Explicitly parse as YAML.
    #[cfg(feature = "parser_yaml")]
    Yaml,
    /// Explicitly parse as TAP (Test Anything Protocol).
    #[cfg(feature = "parser_tap")]
    Tap,
    /// Entire input as a single event with body `{"text": "..."}`.
    Text,
    /// Each non-empty line as a separate event with body `{"text": "..."}`.
    #[serde(alias = "text_line")]
    TextLine,
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
                #[cfg(feature = "parser_xml")]
                Some("xml") => super::super::format_converters::parse_xml(data),
                Some("json") => super::super::format_converters::parse_json(data),
                #[cfg(feature = "parser_yaml")]
                Some("yaml" | "yml") => super::super::format_converters::parse_yaml(data),
                #[cfg(feature = "parser_tap")]
                Some("tap") => super::super::format_converters::parse_tap(data),
                Some("txt") => Ok(super::super::format_converters::parse_text(data)),
                Some("log") => Ok(super::super::format_converters::parse_text(data)),
                _ => {
                    // Default fallback to JSON for unknown extensions
                    super::super::format_converters::parse_json(data)
                }
            }
        }
        Config::Json => super::super::format_converters::parse_json(data),
        #[cfg(feature = "parser_xml")]
        Config::Xml => super::super::format_converters::parse_xml(data),
        #[cfg(feature = "parser_yaml")]
        Config::Yaml => super::super::format_converters::parse_yaml(data),
        #[cfg(feature = "parser_tap")]
        Config::Tap => super::super::format_converters::parse_tap(data),
        Config::Text => Ok(super::super::format_converters::parse_text(data)),
        // TextLine is handled by the caller (cli/mod.rs) which splits lines;
        // if called directly, treat as whole-text.
        Config::TextLine => Ok(super::super::format_converters::parse_text(data)),
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
        let result = parse_with_config(data, &config, Some("data.unknown")).unwrap();
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
    fn test_parse_with_config_text() {
        let data = "hello world";
        let config = Config::Text;
        let result = parse_with_config(data, &config, None).unwrap();
        assert_eq!(result["text"], "hello world");
    }

    #[test]
    fn test_parse_with_config_text_line() {
        let data = "line1\nline2";
        let config = Config::TextLine;
        // When called directly, TextLine falls back to whole-text
        let result = parse_with_config(data, &config, None).unwrap();
        assert_eq!(result["text"], "line1\nline2");
    }

    #[test]
    fn test_parse_with_config_auto_txt_extension() {
        let data = "some raw text";
        let config = Config::Auto;
        let result = parse_with_config(data, &config, Some("output.txt")).unwrap();
        assert_eq!(result["text"], "some raw text");
    }

    #[test]
    fn test_parse_with_config_auto_log_extension() {
        let data = "2024-01-01 INFO started";
        let config = Config::Auto;
        let result = parse_with_config(data, &config, Some("app.log")).unwrap();
        assert_eq!(result["text"], "2024-01-01 INFO started");
    }

    #[test]
    fn test_config_deserialization() {
        let json = r#""auto""#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(matches!(config, Config::Auto));

        let json = r#""json""#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(matches!(config, Config::Json));

        #[cfg(feature = "parser_xml")]
        {
            let json = r#""xml""#;
            let config: Config = serde_json::from_str(json).unwrap();
            assert!(matches!(config, Config::Xml));
        }

        #[cfg(feature = "parser_yaml")]
        {
            let json = r#""yaml""#;
            let config: Config = serde_json::from_str(json).unwrap();
            assert!(matches!(config, Config::Yaml));
        }

        #[cfg(feature = "parser_tap")]
        {
            let json = r#""tap""#;
            let config: Config = serde_json::from_str(json).unwrap();
            assert!(matches!(config, Config::Tap));
        }

        let json = r#""text""#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(matches!(config, Config::Text));

        let json = r#""text_line""#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(matches!(config, Config::TextLine));
    }

    #[cfg(feature = "parser_xml")]
    #[test]
    fn test_parse_with_config_xml() {
        let data = "<root><item>value</item></root>";
        let config = Config::Xml;
        let result = parse_with_config(data, &config, None).unwrap();
        assert!(result.is_object());
    }

    #[cfg(feature = "parser_xml")]
    #[test]
    fn test_parse_with_config_auto_xml_extension() {
        let data = "<root><item>value</item></root>";
        let config = Config::Auto;
        let result = parse_with_config(data, &config, Some("data.xml")).unwrap();
        assert!(result.is_object());
    }

    #[cfg(feature = "parser_xml")]
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

    #[cfg(feature = "parser_yaml")]
    #[test]
    fn test_parse_with_config_yaml() {
        let data = "key: value\ncount: 42";
        let config = Config::Yaml;
        let result = parse_with_config(data, &config, None).unwrap();
        assert_eq!(result["key"], "value");
        assert_eq!(result["count"], 42);
    }

    #[cfg(feature = "parser_yaml")]
    #[test]
    fn test_parse_with_config_auto_yaml_extension() {
        let data = "key: value\ncount: 42";
        let config = Config::Auto;
        let result = parse_with_config(data, &config, Some("data.yaml")).unwrap();
        assert_eq!(result["key"], "value");

        let result = parse_with_config(data, &config, Some("data.yml")).unwrap();
        assert_eq!(result["key"], "value");
    }

    #[cfg(feature = "parser_tap")]
    #[test]
    fn test_parse_with_config_tap() {
        let data = "1..2\nok 1 - First test\nok 2 - Second test\n";
        let config = Config::Tap;
        let result = parse_with_config(data, &config, None).unwrap();
        assert!(result.is_object());
        assert!(result["tests"].is_array());
    }

    #[cfg(feature = "parser_tap")]
    #[test]
    fn test_parse_with_config_auto_tap_extension() {
        let data = "1..1\nok 1 - Test\n";
        let config = Config::Auto;
        let result = parse_with_config(data, &config, Some("results.tap")).unwrap();
        assert!(result.is_object());
        assert!(result["tests"].is_array());
    }
}
