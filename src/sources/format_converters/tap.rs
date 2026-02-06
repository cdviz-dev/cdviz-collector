//! TAP (Test Anything Protocol) v14 parser using nom.
//!
//! This module implements a parser for TAP v14 format, converting it to
//! structured JSON. See: <https://testanything.org/tap-version-14-specification.html>

use nom::{
    IResult,
    branch::alt,
    bytes::complete::{tag, take_until},
    character::complete::{char, digit1, line_ending, not_line_ending, space0, space1},
    combinator::{opt, value},
    sequence::{preceded, terminated, tuple},
};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TapDocument {
    pub version: Option<u8>,
    pub plan: Option<Plan>,
    pub tests: Vec<TestLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestLine {
    pub status: TestStatus,
    pub number: Option<usize>,
    pub description: Option<String>,
    pub directive: Option<Directive>,
    pub yaml_diagnostics: Option<JsonValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    Ok,
    NotOk,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Directive {
    pub kind: DirectiveKind,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum DirectiveKind {
    Skip,
    Todo,
}

/// Parse TAP version line: "TAP version 14"
fn tap_version(input: &str) -> IResult<&str, u8> {
    let (input, _) = tuple((tag("TAP"), space1, tag("version"), space1))(input)?;
    let (input, version) = digit1(input)?;
    let (input, _) = line_ending(input)?;
    Ok((input, version.parse().unwrap_or(14)))
}

/// Parse plan line: "1..100"
fn plan_line(input: &str) -> IResult<&str, Plan> {
    let (input, start) = digit1(input)?;
    let (input, _) = tag("..")(input)?;
    let (input, end) = digit1(input)?;
    let (input, _) = line_ending(input)?;
    Ok((input, Plan { start: start.parse().unwrap_or(1), end: end.parse().unwrap_or(0) }))
}

/// Parse test status: "ok" or "not ok"
fn test_status(input: &str) -> IResult<&str, TestStatus> {
    alt((value(TestStatus::Ok, tag("ok")), value(TestStatus::NotOk, tag("not ok"))))(input)
}

/// Parse directive: "# SKIP reason" or "# TODO reason"
fn directive(input: &str) -> IResult<&str, Directive> {
    let (input, _) = tuple((space0, char('#'), space1))(input)?;
    let (input, kind) = alt((
        value(DirectiveKind::Skip, tag("SKIP")),
        value(DirectiveKind::Todo, tag("TODO")),
    ))(input)?;
    let (input, reason) = opt(preceded(space1, not_line_ending))(input)?;
    Ok((
        input,
        Directive { kind, reason: reason.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) },
    ))
}

/// Parse YAML diagnostic block between "  ---" and "  ..."
fn yaml_block(input: &str) -> IResult<&str, String> {
    let (input, _) = tuple((tag("  ---"), line_ending))(input)?;
    let (input, yaml_lines) = take_until("  ...")(input)?;
    let (input, _) = tuple((tag("  ..."), line_ending))(input)?;

    // Remove the 2-space indentation from each line
    let yaml_content = yaml_lines
        .lines()
        .map(|line| line.strip_prefix("  ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");

    Ok((input, yaml_content))
}

/// Parse a test line: "ok 1 - description # directive"
fn test_line(input: &str) -> IResult<&str, TestLine> {
    let (input, status) = test_status(input)?;
    let (input, _) = space1(input)?;

    // Parse optional test number
    let (input, number) = opt(terminated(digit1, space1))(input)?;

    // Parse optional dash separator
    let (input, _) = opt(tuple((char('-'), space1)))(input)?;

    // Parse description and directive in one go
    let (input, rest) = not_line_ending(input)?;
    let (input, _) = line_ending(input)?;

    // Split description and directive
    let (description, dir) = if let Some(hash_pos) = rest.rfind('#') {
        let desc_part = rest[..hash_pos].trim();
        let directive_part = &rest[hash_pos..];

        // Try to parse the directive
        match directive(directive_part) {
            Ok((_, dir)) => {
                (if desc_part.is_empty() { None } else { Some(desc_part.to_string()) }, Some(dir))
            }
            Err(_) => {
                (if rest.trim().is_empty() { None } else { Some(rest.trim().to_string()) }, None)
            }
        }
    } else {
        (if rest.trim().is_empty() { None } else { Some(rest.trim().to_string()) }, None)
    };

    // Check for YAML diagnostic block
    let (input, yaml_str) = opt(yaml_block)(input)?;
    let yaml_diagnostics = yaml_str.and_then(|yaml| {
        // Parse YAML using parse_yaml from parent module
        super::parse_yaml(&yaml).ok()
    });

    Ok((
        input,
        TestLine {
            status,
            number: number.and_then(|n| n.parse().ok()),
            description,
            directive: dir,
            yaml_diagnostics,
        },
    ))
}

/// Parse a comment line: "# comment text"
fn comment_line(input: &str) -> IResult<&str, ()> {
    let (input, _) = char('#')(input)?;
    let (input, _) = not_line_ending(input)?;
    let (input, _) = line_ending(input)?;
    Ok((input, ()))
}

/// Parse an empty line
fn empty_line(input: &str) -> IResult<&str, ()> {
    let (input, _) = space0(input)?;
    let (input, _) = line_ending(input)?;
    Ok((input, ()))
}

/// Parse any ignorable line (comment or empty)
fn ignorable_line(input: &str) -> IResult<&str, ()> {
    alt((comment_line, empty_line))(input)
}

/// Parse TAP document
pub fn parse_tap_document(input: &str) -> TapDocument {
    let mut remaining = input;
    let mut version = None;
    let mut plan = None;
    let mut tests = Vec::new();

    // Skip leading ignorable lines
    #[allow(clippy::ignored_unit_patterns)]
    while let Ok((rest, _)) = ignorable_line(remaining) {
        remaining = rest;
    }

    // Try to parse version
    if let Ok((rest, v)) = tap_version(remaining) {
        version = Some(v);
        remaining = rest;
    }

    // Skip ignorable lines
    #[allow(clippy::ignored_unit_patterns)]
    while let Ok((rest, _)) = ignorable_line(remaining) {
        remaining = rest;
    }

    // Try to parse plan (can appear at start or end)
    if let Ok((rest, p)) = plan_line(remaining) {
        plan = Some(p);
        remaining = rest;
    }

    // Parse test lines and comments
    loop {
        // Skip ignorable lines
        #[allow(clippy::ignored_unit_patterns)]
        while let Ok((rest, _)) = ignorable_line(remaining) {
            remaining = rest;
        }

        if remaining.is_empty() {
            break;
        }

        // Try to parse test line
        if let Ok((rest, test)) = test_line(remaining) {
            tests.push(test);
            remaining = rest;
            continue;
        }

        // Try to parse plan if we haven't seen it yet (can appear at end)
        if plan.is_none()
            && let Ok((rest, p)) = plan_line(remaining)
        {
            plan = Some(p);
            remaining = rest;
            continue;
        }

        // If we can't parse anything, there might be an error
        // Try to skip the line to be lenient
        if let Some(newline_pos) = remaining.find('\n') {
            remaining = &remaining[newline_pos + 1..];
        } else {
            break;
        }
    }

    TapDocument { version, plan, tests }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_tap() {
        let input =
            "TAP version 14\n1..3\nok 1 - First test\nok 2 - Second test\nnot ok 3 - Third test\n";
        let doc = parse_tap_document(input);
        assert_eq!(doc.version, Some(14));
        assert_eq!(doc.plan.as_ref().unwrap().end, 3);
        assert_eq!(doc.tests.len(), 3);
    }

    #[test]
    fn test_parse_tap_with_directives() {
        let input = "1..2\nok 1 - Test # SKIP not implemented\nnot ok 2 - Fail # TODO fix later\n";
        let doc = parse_tap_document(input);
        assert_eq!(doc.tests.len(), 2);
        assert!(doc.tests[0].directive.is_some());
    }

    #[test]
    fn test_parse_tap_with_comments() {
        let input = "# This is a comment\n1..1\nok 1 - Test\n";
        let doc = parse_tap_document(input);
        assert_eq!(doc.tests.len(), 1);
    }
}
