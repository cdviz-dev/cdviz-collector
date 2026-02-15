//! Format conversion helpers for parsing various input formats to JSON.
//!
//! This module provides pure functions for converting different data formats
//! (XML, JSON, YAML, TAP, etc.) to `serde_json::Value`. These helpers are used by both
//! `OpenDAL` parsers and CLI source to avoid code duplication.

use crate::errors::{Error, IntoDiagnostic, Result};
use serde_json::json;
use std::io::Cursor;

#[cfg(feature = "parser_tap")]
pub(crate) mod tap;

/// Wrap raw text content as a JSON object with a `text` field.
///
/// This is the simplest parser — it wraps unstructured text into
/// `{"text": "..."}` so that downstream VRL transformers can apply
/// regex / key-value parsing on the `text` field.
///
/// # Arguments
/// * `data` - The raw text content
///
/// # Returns
/// A `serde_json::Value` of the form `{"text": "<data>"}`
pub(crate) fn parse_text(data: &str) -> serde_json::Value {
    json!({"text": data})
}

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
#[cfg(feature = "parser_xml")]
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

/// Convert YAML string to JSON.
///
/// Parses YAML content and converts it to `serde_json::Value` using a
/// three-step conversion process to handle YAML's richer type system
/// (e.g., non-string map keys are converted to strings).
///
/// # Arguments
/// * `data` - The YAML content as a string
///
/// # Returns
/// A `serde_json::Value` representing the parsed YAML
///
/// # Errors
/// Returns an error if YAML parsing or JSON conversion fails
#[cfg(feature = "parser_yaml")]
pub(crate) fn parse_yaml(data: &str) -> Result<serde_json::Value> {
    // Step 1: Parse YAML to serde_yaml::Value
    let yaml_value: serde_yaml::Value = serde_yaml::from_str(data)
        .map_err(|cause| miette::miette!("Failed to parse YAML: {}", cause))?;

    // Step 2: Serialize YAML to JSON string (handles type conversions)
    let json_str = serde_json::to_string(&yaml_value).into_diagnostic()?;

    // Step 3: Parse JSON string to serde_json::Value
    let json_value: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|cause| Error::from_serde_error(&json_str, cause))?;

    Ok(json_value)
}

/// Convert TAP (Test Anything Protocol) v14 to JSON.
///
/// Parses TAP v14 format and converts it to a structured JSON representation.
/// YAML diagnostic blocks embedded in TAP output are automatically parsed
/// and converted to JSON objects.
///
/// # Arguments
/// * `data` - The TAP content as a string
///
/// # Returns
/// A `serde_json::Value` with structure:
/// ```json
/// {
///   "version": 14,
///   "plan": {"start": 1, "end": N},
///   "tests": [
///     {
///       "status": "ok"|"not_ok",
///       "number": N,
///       "description": "...",
///       "directive": {"kind": "SKIP"|"TODO", "reason": "..."},
///       "yaml_diagnostics": {...}
///     }
///   ]
/// }
/// ```
///
/// # Errors
/// Returns an error if TAP parsing, embedded YAML parsing, or JSON conversion fails
#[cfg(feature = "parser_tap")]
pub(crate) fn parse_tap(data: &str) -> Result<serde_json::Value> {
    use tap::parse_tap_document;

    // Parse TAP structure using custom nom parser
    let document = parse_tap_document(data);

    // Convert to JSON
    let json_value = serde_json::to_value(&document).into_diagnostic()?;

    Ok(json_value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_text_simple() {
        let result = parse_text("hello world");
        assert_eq!(result["text"], "hello world");
    }

    #[test]
    fn test_parse_text_empty() {
        let result = parse_text("");
        assert_eq!(result["text"], "");
    }

    #[test]
    fn test_parse_text_multiline() {
        let result = parse_text("line1\nline2\nline3");
        assert_eq!(result["text"], "line1\nline2\nline3");
    }

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

    #[cfg(feature = "parser_xml")]
    #[test]
    fn test_parse_xml_simple() {
        let xml = "<root><child>value</child></root>";
        let result = parse_xml(xml).unwrap();
        assert!(result.is_object());
    }

    #[cfg(feature = "parser_xml")]
    #[test]
    fn test_parse_xml_with_attributes() {
        let xml = r#"<testsuite name="MyTests" tests="10"></testsuite>"#;
        let result = parse_xml(xml).unwrap();
        assert!(result.is_object());
    }

    #[cfg(feature = "parser_xml")]
    #[test]
    fn test_parse_xml_invalid() {
        let xml = "<root><unclosed>";
        assert!(parse_xml(xml).is_err());
    }

    #[cfg(feature = "parser_xml")]
    #[test]
    fn test_parse_xml_empty() {
        let xml = "";
        assert!(parse_xml(xml).is_err());
    }

    #[cfg(feature = "parser_xml")]
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

    #[cfg(feature = "parser_xml")]
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

    #[cfg(feature = "parser_xml")]
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

    #[cfg(feature = "parser_yaml")]
    #[test]
    fn test_parse_yaml_simple_object() {
        use insta::assert_yaml_snapshot;

        let yaml = r"
name: MyTestSuite
version: '1.0.0'
count: 42
";
        let result = parse_yaml(yaml).unwrap();
        assert_yaml_snapshot!(result);
    }

    #[cfg(feature = "parser_yaml")]
    #[test]
    fn test_parse_yaml_nested_structure() {
        use insta::assert_yaml_snapshot;

        let yaml = r"
stats:
  total: 4
  passed: 3
  failed: 1
nested:
  deep:
    value: found
";
        let result = parse_yaml(yaml).unwrap();
        assert_yaml_snapshot!(result);
    }

    #[cfg(feature = "parser_yaml")]
    #[test]
    fn test_parse_yaml_array() {
        use insta::assert_yaml_snapshot;

        let yaml = r"
- name: test1
  status: passed
- name: test2
  status: failed
";
        let result = parse_yaml(yaml).unwrap();
        assert_yaml_snapshot!(result);
    }

    #[cfg(feature = "parser_yaml")]
    #[test]
    fn test_parse_yaml_invalid() {
        let yaml = "invalid: yaml: syntax::";
        let result = parse_yaml(yaml);
        assert!(result.is_err());
    }

    #[cfg(feature = "parser_tap")]
    #[test]
    fn test_parse_tap_simple() {
        use insta::assert_yaml_snapshot;

        let tap = r"TAP version 14
1..3
ok 1 - First test
ok 2 - Second test
not ok 3 - Third test
";
        let result = parse_tap(tap).unwrap();
        assert_yaml_snapshot!(result);
    }

    #[cfg(feature = "parser_tap")]
    #[test]
    fn test_parse_tap_with_yaml_diagnostics() {
        use insta::assert_yaml_snapshot;

        let tap = r"TAP version 14
1..2
ok 1 - Passing test
not ok 2 - Failing test
  ---
  message: Assertion failed
  severity: fail
  expected: 42
  actual: 0
  ...
";
        let result = parse_tap(tap).unwrap();
        assert_yaml_snapshot!(result);
    }

    #[cfg(feature = "parser_tap")]
    #[test]
    fn test_parse_tap_with_directives() {
        use insta::assert_yaml_snapshot;

        let tap = r"TAP version 14
1..3
ok 1 - Normal test
ok 2 - Skipped # SKIP not implemented
not ok 3 - Known issue # TODO fix later
";
        let result = parse_tap(tap).unwrap();
        assert_yaml_snapshot!(result);
    }

    #[cfg(feature = "parser_tap")]
    #[test]
    fn test_parse_tap_with_comments() {
        use insta::assert_yaml_snapshot;

        let tap = r"# Subtest: Database
TAP version 14
1..2
ok 1 - Connect
ok 2 - Query
";
        let result = parse_tap(tap).unwrap();
        assert_yaml_snapshot!(result);
    }

    #[cfg(feature = "parser_tap")]
    #[test]
    fn test_parse_tap_plan_at_end() {
        use insta::assert_yaml_snapshot;

        let tap = r"TAP version 14
ok 1 - First test
ok 2 - Second test
1..2
";
        let result = parse_tap(tap).unwrap();
        assert_yaml_snapshot!(result);
    }

    #[cfg(feature = "parser_tap")]
    #[test]
    fn test_parse_tap_invalid() {
        let tap = "not valid tap format";
        // Should still parse but with empty/minimal structure
        let result = parse_tap(tap);
        assert!(result.is_ok());
    }

    #[cfg(feature = "parser_tap")]
    #[test]
    fn test_parse_tap_no_plan() {
        use insta::assert_yaml_snapshot;

        let tap = r"ok 1 - Test without plan
ok 2 - Another test
";
        let result = parse_tap(tap).unwrap();
        assert_yaml_snapshot!(result);
    }
}
