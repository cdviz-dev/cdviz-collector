//! Format conversion helpers for parsing various input formats to JSON.
//!
//! This module provides pure functions for converting different data formats
//! (XML, JSON, etc.) to `serde_json::Value`. These helpers are used by both
//! `OpenDAL` parsers and CLI source to avoid code duplication.

use crate::errors::{Error, IntoDiagnostic, Result};
use std::io::Cursor;

/// Convert JSON string to `serde_json::Value`.
///
/// This function provides JSON parsing with better error messages using
/// the `Error::from_serde_error` helper.
///
/// # Arguments
/// * `data` - The JSON content as a string
///
/// # Returns
/// A `serde_json::Value` representing the parsed JSON
///
/// # Errors
/// Returns an error if JSON parsing fails
pub(crate) fn parse_json(data: &str) -> Result<serde_json::Value> {
    Ok(serde_json::from_str(data).map_err(|cause| Error::from_serde_error(data, cause))?)
}

/// Convert XML string to JSON using quick-xml-to-json.
///
/// This function provides a consistent XML→JSON conversion that handles
/// both single elements and arrays uniformly, making it suitable for
/// parsing `JUnit` XML reports and other XML formats.
///
/// # Arguments
/// * `data` - The XML content as a string
///
/// # Returns
/// A `serde_json::Value` representing the XML structure as JSON
///
/// # Errors
/// Returns an error if:
/// - XML parsing fails (malformed XML)
/// - JSON conversion fails
/// - UTF-8 encoding issues
pub(crate) fn parse_xml(data: &str) -> Result<serde_json::Value> {
    let reader = Cursor::new(data.as_bytes());
    let mut writer = Vec::new();

    quick_xml_to_json::xml_to_json(reader, &mut writer)
        .map_err(|e| miette::miette!("Failed to convert XML to JSON: {}", e))?;

    let json_str = String::from_utf8(writer).into_diagnostic()?;

    let value: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|cause| Error::from_serde_error(&json_str, cause))?;

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_json_simple() {
        let json = r#"{"key": "value", "number": 42}"#;
        let result = parse_json(json).unwrap();
        assert!(result.is_object());
        assert_eq!(result["key"], "value");
        assert_eq!(result["number"], 42);
    }

    #[test]
    fn test_parse_json_array() {
        let json = r#"[{"id": 1}, {"id": 2}]"#;
        let result = parse_json(json).unwrap();
        assert!(result.is_array());
        assert_eq!(result.as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_parse_json_invalid() {
        let json = r#"{"invalid": json}"#;
        assert!(parse_json(json).is_err());
    }

    #[test]
    fn test_parse_xml_simple() {
        let xml = "<root><child>value</child></root>";
        let result = parse_xml(xml).unwrap();
        assert!(result.is_object());
    }

    #[test]
    fn test_parse_xml_with_attributes() {
        let xml = r#"<testsuite name="MyTests" tests="10"></testsuite>"#;
        let result = parse_xml(xml).unwrap();
        assert!(result.is_object());
    }

    #[test]
    fn test_parse_xml_invalid() {
        let xml = "<root><unclosed>";
        assert!(parse_xml(xml).is_err());
    }

    #[test]
    fn test_parse_xml_empty() {
        let xml = "";
        assert!(parse_xml(xml).is_err());
    }

    #[test]
    fn test_parse_xml_junit_example() {
        let xml = r#"
        <?xml version="1.0" encoding="UTF-8"?>
        <testsuite name="MyTestSuite" tests="2" failures="1">
            <testcase name="test1" classname="MyTests" time="0.5"/>
            <testcase name="test2" classname="MyTests" time="1.2">
                <failure message="assertion failed"/>
            </testcase>
        </testsuite>
        "#;
        let result = parse_xml(xml).unwrap();
        assert!(result.is_object());
        // The structure should contain testsuite data
    }

    #[test]
    fn test_parse_xml_nested_elements() {
        let xml = "
        <root>
            <level1>
                <level2>
                    <level3>deep value</level3>
                </level2>
            </level1>
        </root>
        ";
        let result = parse_xml(xml).unwrap();
        assert!(result.is_object());
    }

    #[test]
    fn test_parse_xml_multiple_children() {
        let xml = "
        <testsuite>
            <testcase name=\"test1\"/>
            <testcase name=\"test2\"/>
            <testcase name=\"test3\"/>
        </testsuite>
        ";
        let result = parse_xml(xml).unwrap();
        assert!(result.is_object());
        // quick-xml-to-json should handle multiple children consistently
    }
}
