//! Unified parser configuration and format conversion helpers.
//!
//! This module provides:
//! - [`Config`]: a shared parser-selection enum used by all source extractors
//! - [`parse_with_config`]: string-input parsing dispatcher for CLI-style sources
//! - Raw format helpers (`parse_json`, `parse_text`, etc.) shared across sources

#[cfg(any(
    feature = "parser_csv_row",
    feature = "parser_xml",
    feature = "parser_yaml",
    feature = "parser_tap"
))]
use crate::errors::IntoDiagnostic;
use crate::errors::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
#[cfg(feature = "parser_xml")]
use std::io::Cursor;

#[cfg(feature = "parser_tap")]
pub(crate) mod tap;

/// Parser selection shared by CLI and `OpenDAL` source extractors.
///
/// Determines how input data should be parsed. Use [`Config::Auto`] for
/// automatic format detection based on file extension, or pick an explicit variant.
///
/// `Metadata` is meaningful only for the `OpenDAL` source (emit resource metadata
/// without reading file content); calling [`parse_with_config`] with it returns
/// an error.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Config {
    /// Auto-detect format from file extension; falls back to JSON.
    #[default]
    Auto,
    /// Parse as JSON.
    Json,
    /// Parse as JSONL (one JSON object per non-empty line → one event per line).
    Jsonl,
    /// Parse as CSV (headers become keys; one event per data row).
    #[cfg(feature = "parser_csv_row")]
    CsvRow,
    /// Emit only resource metadata without reading file content (`OpenDAL` only).
    Metadata,
    /// Parse as XML.
    #[cfg(feature = "parser_xml")]
    Xml,
    /// Parse as YAML.
    #[cfg(feature = "parser_yaml")]
    Yaml,
    /// Parse as TAP (Test Anything Protocol).
    #[cfg(feature = "parser_tap")]
    Tap,
    /// Entire input as a single event with body `{"text": "..."}`.
    Text,
    /// Each non-empty line as a separate event with body `{"text": "..."}`.
    // backward-compat alias for configs that spelled it "textline" (no underscore)
    #[serde(alias = "textline")]
    TextLine,
}

// ── Raw format helpers ────────────────────────────────────────────────────────

/// Wrap raw text as `{"text": "..."}`.
pub(crate) fn parse_text(data: &str) -> serde_json::Value {
    json!({"text": data})
}

/// Parse a JSON string.
pub(crate) fn parse_json(data: &str) -> Result<serde_json::Value> {
    Ok(serde_json::from_str(data).map_err(|cause| Error::from_serde_error(data, cause))?)
}

/// Parse JSONL: each non-empty line is a JSON object; returns `Value::Array`.
pub(crate) fn parse_jsonl(data: &str) -> Result<serde_json::Value> {
    let mut events = Vec::new();
    for line in data.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let val: serde_json::Value =
            serde_json::from_str(line).map_err(|cause| Error::from_serde_error(line, cause))?;
        events.push(val);
    }
    Ok(serde_json::Value::Array(events))
}

/// Parse CSV: headers become keys; returns `Value::Array` of one object per row.
#[cfg(feature = "parser_csv_row")]
pub(crate) fn parse_csv_row(data: &str) -> Result<serde_json::Value> {
    let mut rdr = csv::Reader::from_reader(data.as_bytes());
    let headers = rdr.headers().into_diagnostic()?.clone();
    let mut events = Vec::new();
    for record in rdr.records() {
        let record = record.into_diagnostic()?;
        let mut body = serde_json::Map::with_capacity(headers.len());
        for (header, value) in headers.iter().zip(record.iter()) {
            body.insert(header.to_string(), serde_json::Value::String(value.to_string()));
        }
        events.push(serde_json::Value::Object(body));
    }
    Ok(serde_json::Value::Array(events))
}

/// Parse XML to JSON.
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

/// Parse YAML to JSON.
#[cfg(feature = "parser_yaml")]
pub(crate) fn parse_yaml(data: &str) -> Result<serde_json::Value> {
    let yaml_value: serde_yaml::Value = serde_yaml::from_str(data)
        .map_err(|cause| miette::miette!("Failed to parse YAML: {}", cause))?;

    let json_str = serde_json::to_string(&yaml_value).into_diagnostic()?;

    let json_value: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|cause| Error::from_serde_error(&json_str, cause))?;

    Ok(json_value)
}

/// Parse TAP v14 to JSON.
#[cfg(feature = "parser_tap")]
pub(crate) fn parse_tap(data: &str) -> Result<serde_json::Value> {
    use tap::parse_tap_document;
    let document = parse_tap_document(data);
    let json_value = serde_json::to_value(&document).into_diagnostic()?;
    Ok(json_value)
}

// ── Dispatcher ────────────────────────────────────────────────────────────────

/// Parse `data` according to `config`, using `filename` for auto-detection.
///
/// Returns `Value::Array` for multi-record formats (`Jsonl`, `CsvRow`) so that
/// callers with existing array-dispatch logic emit one event per element.
///
/// # Errors
/// Returns an error if:
/// - Parsing fails for the specified format
/// - `config` is [`Config::Metadata`] (not meaningful for string input)
pub(crate) fn parse_with_config(
    data: &str,
    config: &Config,
    filename: Option<&str>,
) -> Result<serde_json::Value> {
    match config {
        Config::Auto => {
            let ext = filename
                .and_then(|f| std::path::Path::new(f).extension()?.to_str())
                .map(str::to_ascii_lowercase);
            match ext.as_deref() {
                #[cfg(feature = "parser_xml")]
                Some("xml") => parse_xml(data),
                Some("json") => parse_json(data),
                Some("jsonl") => parse_jsonl(data),
                #[cfg(feature = "parser_csv_row")]
                Some("csv") => parse_csv_row(data),
                #[cfg(feature = "parser_yaml")]
                Some("yaml" | "yml") => parse_yaml(data),
                #[cfg(feature = "parser_tap")]
                Some("tap") => parse_tap(data),
                Some("txt") => Ok(parse_text(data)),
                Some("log") => Ok(parse_text(data)),
                _ => parse_json(data),
            }
        }
        Config::Json => parse_json(data),
        Config::Jsonl => parse_jsonl(data),
        #[cfg(feature = "parser_csv_row")]
        Config::CsvRow => parse_csv_row(data),
        Config::Metadata => {
            Err(miette::miette!("Metadata parser is not supported for string input"))
        }
        #[cfg(feature = "parser_xml")]
        Config::Xml => parse_xml(data),
        #[cfg(feature = "parser_yaml")]
        Config::Yaml => parse_yaml(data),
        #[cfg(feature = "parser_tap")]
        Config::Tap => parse_tap(data),
        Config::Text => Ok(parse_text(data)),
        // TextLine splitting is handled by the caller (CliExtractor); called directly → whole text
        Config::TextLine => Ok(parse_text(data)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_text ────────────────────────────────────────────────────────────

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

    // ── parse_json ────────────────────────────────────────────────────────────

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

    // ── parse_jsonl ───────────────────────────────────────────────────────────

    #[test]
    fn test_parse_jsonl_multiple_lines() {
        let data = "{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n";
        let result = parse_jsonl(data).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["a"], 1);
        assert_eq!(arr[1]["b"], 2);
        assert_eq!(arr[2]["c"], 3);
    }

    #[test]
    fn test_parse_jsonl_skips_empty_lines() {
        let data = "{\"a\":1}\n\n{\"b\":2}\n";
        let result = parse_jsonl(data).unwrap();
        assert_eq!(result.as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_parse_jsonl_single_line() {
        let data = "{\"x\":42}\n";
        let result = parse_jsonl(data).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["x"], 42);
    }

    #[test]
    fn test_parse_jsonl_invalid_line() {
        let data = "{\"ok\":1}\nnot json\n";
        assert!(parse_jsonl(data).is_err());
    }

    // ── parse_csv_row ─────────────────────────────────────────────────────────

    #[cfg(feature = "parser_csv_row")]
    #[test]
    fn test_parse_csv_row_basic() {
        let data = "name,age\nAlice,30\nBob,25\n";
        let result = parse_csv_row(data).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], "Alice");
        assert_eq!(arr[0]["age"], "30");
        assert_eq!(arr[1]["name"], "Bob");
    }

    #[cfg(feature = "parser_csv_row")]
    #[test]
    fn test_parse_csv_row_single_row() {
        let data = "x,y\n1,2\n";
        let result = parse_csv_row(data).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["x"], "1");
        assert_eq!(arr[0]["y"], "2");
    }

    // ── parse_with_config ─────────────────────────────────────────────────────

    #[test]
    fn test_parse_with_config_json() {
        let data = r#"{"test": "value"}"#;
        let result = parse_with_config(data, &Config::Json, None).unwrap();
        assert_eq!(result["test"], "value");
    }

    #[test]
    fn test_parse_with_config_jsonl() {
        let data = "{\"a\":1}\n{\"b\":2}\n";
        let result = parse_with_config(data, &Config::Jsonl, None).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[cfg(feature = "parser_csv_row")]
    #[test]
    fn test_parse_with_config_csv_row() {
        let data = "col1,col2\nfoo,bar\n";
        let result = parse_with_config(data, &Config::CsvRow, None).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["col1"], "foo");
    }

    #[test]
    fn test_parse_with_config_metadata_errors() {
        let result = parse_with_config("{}", &Config::Metadata, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_with_config_text() {
        let data = "hello world";
        let result = parse_with_config(data, &Config::Text, None).unwrap();
        assert_eq!(result["text"], "hello world");
    }

    #[test]
    fn test_parse_with_config_text_line_falls_back_to_text() {
        let data = "line1\nline2";
        let result = parse_with_config(data, &Config::TextLine, None).unwrap();
        assert_eq!(result["text"], "line1\nline2");
    }

    #[test]
    fn test_parse_with_config_auto_json_extension() {
        let data = r#"{"test": "value"}"#;
        let result = parse_with_config(data, &Config::Auto, Some("data.json")).unwrap();
        assert_eq!(result["test"], "value");
    }

    #[test]
    fn test_parse_with_config_auto_jsonl_extension() {
        let data = "{\"a\":1}\n{\"b\":2}\n";
        let result = parse_with_config(data, &Config::Auto, Some("events.jsonl")).unwrap();
        assert_eq!(result.as_array().unwrap().len(), 2);
    }

    #[cfg(feature = "parser_csv_row")]
    #[test]
    fn test_parse_with_config_auto_csv_extension() {
        let data = "x,y\n1,2\n3,4\n";
        let result = parse_with_config(data, &Config::Auto, Some("data.csv")).unwrap();
        assert_eq!(result.as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_parse_with_config_auto_txt_extension() {
        let result = parse_with_config("some raw text", &Config::Auto, Some("output.txt")).unwrap();
        assert_eq!(result["text"], "some raw text");
    }

    #[test]
    fn test_parse_with_config_auto_log_extension() {
        let result =
            parse_with_config("2024-01-01 INFO started", &Config::Auto, Some("app.log")).unwrap();
        assert_eq!(result["text"], "2024-01-01 INFO started");
    }

    #[test]
    fn test_parse_with_config_auto_fallback_to_json() {
        let data = r#"{"test": "value"}"#;
        let result = parse_with_config(data, &Config::Auto, Some("data.unknown")).unwrap();
        assert_eq!(result["test"], "value");
    }

    #[test]
    fn test_parse_with_config_auto_no_filename() {
        let data = r#"{"test": "value"}"#;
        let result = parse_with_config(data, &Config::Auto, None).unwrap();
        assert_eq!(result["test"], "value");
    }

    #[test]
    fn test_config_default_is_auto() {
        assert!(matches!(Config::default(), Config::Auto));
    }

    #[test]
    fn test_config_deserialization() {
        assert!(matches!(serde_json::from_str::<Config>(r#""auto""#).unwrap(), Config::Auto));
        assert!(matches!(serde_json::from_str::<Config>(r#""json""#).unwrap(), Config::Json));
        assert!(matches!(serde_json::from_str::<Config>(r#""jsonl""#).unwrap(), Config::Jsonl));
        assert!(matches!(
            serde_json::from_str::<Config>(r#""metadata""#).unwrap(),
            Config::Metadata
        ));
        assert!(matches!(serde_json::from_str::<Config>(r#""text""#).unwrap(), Config::Text));
        assert!(matches!(
            serde_json::from_str::<Config>(r#""text_line""#).unwrap(),
            Config::TextLine
        ));
        // backward-compat alias
        assert!(matches!(
            serde_json::from_str::<Config>(r#""textline""#).unwrap(),
            Config::TextLine
        ));
    }

    #[cfg(feature = "parser_csv_row")]
    #[test]
    fn test_config_deserialization_csv_row() {
        assert!(matches!(serde_json::from_str::<Config>(r#""csv_row""#).unwrap(), Config::CsvRow));
    }

    // ── XML ───────────────────────────────────────────────────────────────────

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
        assert!(parse_xml("<root><unclosed>").is_err());
    }

    #[cfg(feature = "parser_xml")]
    #[test]
    fn test_parse_xml_empty() {
        assert!(parse_xml("").is_err());
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
        assert!(parse_xml(xml).unwrap().is_object());
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
        assert!(parse_xml(xml).unwrap().is_object());
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
        assert!(parse_xml(xml).unwrap().is_object());
    }

    #[cfg(feature = "parser_xml")]
    #[test]
    fn test_parse_with_config_xml() {
        let data = "<root><item>value</item></root>";
        let result = parse_with_config(data, &Config::Xml, None).unwrap();
        assert!(result.is_object());
    }

    #[cfg(feature = "parser_xml")]
    #[test]
    fn test_parse_with_config_auto_xml_extension() {
        let data = "<root><item>value</item></root>";
        let result = parse_with_config(data, &Config::Auto, Some("data.xml")).unwrap();
        assert!(result.is_object());
    }

    // ── YAML ──────────────────────────────────────────────────────────────────

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
        assert!(parse_yaml("invalid: yaml: syntax::").is_err());
    }

    #[cfg(feature = "parser_yaml")]
    #[test]
    fn test_parse_with_config_yaml() {
        let data = "key: value\ncount: 42";
        let result = parse_with_config(data, &Config::Yaml, None).unwrap();
        assert_eq!(result["key"], "value");
        assert_eq!(result["count"], 42);
    }

    #[cfg(feature = "parser_yaml")]
    #[test]
    fn test_parse_with_config_auto_yaml_extension() {
        let data = "key: value\ncount: 42";
        let r1 = parse_with_config(data, &Config::Auto, Some("data.yaml")).unwrap();
        assert_eq!(r1["key"], "value");
        let r2 = parse_with_config(data, &Config::Auto, Some("data.yml")).unwrap();
        assert_eq!(r2["key"], "value");
    }

    // ── TAP ───────────────────────────────────────────────────────────────────

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
        // Should still parse but with empty/minimal structure
        assert!(parse_tap("not valid tap format").is_ok());
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

    #[cfg(feature = "parser_tap")]
    #[test]
    fn test_parse_with_config_tap() {
        let data = "1..2\nok 1 - First test\nok 2 - Second test\n";
        let result = parse_with_config(data, &Config::Tap, None).unwrap();
        assert!(result.is_object());
        assert!(result["tests"].is_array());
    }

    #[cfg(feature = "parser_tap")]
    #[test]
    fn test_parse_with_config_auto_tap_extension() {
        let data = "1..1\nok 1 - Test\n";
        let result = parse_with_config(data, &Config::Auto, Some("results.tap")).unwrap();
        assert!(result.is_object());
        assert!(result["tests"].is_array());
    }
}
