use crate::errors::{IntoDiagnostic, Result};
use crate::pipes::Pipe;
use crate::sources::{EventSource, EventSourcePipe};
use miette::{LabeledSpan, MietteDiagnostic};
use vrl::compiler::{Program, TargetValue};
use vrl::core::Value;
use vrl::diagnostic::{Diagnostic, Formatter};
use vrl::prelude::state::RuntimeState;
use vrl::prelude::{Context, TimeZone};
use vrl::value::Secrets;

pub(crate) struct Processor {
    next: EventSourcePipe,
    renderer: Program,
    source: String,
}

impl Processor {
    pub(crate) fn new(template: &str, next: EventSourcePipe) -> Result<Self> {
        // Use all of the std library functions
        let mut fns = vrl::stdlib::all();
        // Add custom PURL functions
        fns.extend(super::vrl_purl::all_custom_functions());
        let src = if template.is_empty() {
            // empty fallback to identity (array of one element: the input)
            "[.]"
        } else {
            template
        };
        match vrl::compiler::compile(src, &fns) {
            Err(err) => {
                let formatter = Formatter::new(src, err);
                //tracing::error!(diagnostics = %formatter, "VRL compilation error");
                Err(MietteDiagnostic::new(formatter.to_string()))?
                // bail!("VRL compilation error")
            }
            Ok(res) => Ok(Self { next, renderer: res.program, source: src.to_string() }),
        }
    }
}

/// Converts a byte offset in `source` to a 1-based `(line, col)` pair.
fn byte_offset_to_line_col(source: &str, offset: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut col = 1usize;
    for (i, ch) in source.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

impl Pipe for Processor {
    type Input = EventSource;
    //TODO optimize EventSource to avoid serialization/deserialization via json to convert to/from Value
    //TODO build a microbenchmark to compare the performance of converting to/from Value
    fn send(&mut self, input: Self::Input) -> Result<()> {
        // This is the target that can be accessed / modified in the VRL program.
        // You can use any custom type that implements `Target`, but `TargetValue` is also provided for convenience.

        let mut target = TargetValue {
            // the value starts as just an object with a single field "x" set to 1
            value: serde_json::to_value(&input).into_diagnostic()?.into(),
            // the metadata is empty
            metadata: Value::Object(std::collections::BTreeMap::new()),
            // and there are no secrets associated with the target
            secrets: Secrets::default(),
        };

        // The current state of the runtime (i.e. local variables)
        let mut state = RuntimeState::default();

        let timezone = TimeZone::default();

        // A context bundles all the info necessary for the runtime to resolve a value.
        let mut ctx = Context::new(&mut target, &mut state, &timezone);

        // This executes the VRL program, making any modifications to the target, and returning a result.
        let res = self.renderer.resolve(&mut ctx).map_err(|cause| {
            let vrl_diag = Diagnostic::from(cause);
            // Extract before consuming vrl_diag with Formatter
            let severity = vrl_diag.severity;
            let labels: Vec<_> = vrl_diag
                .labels
                .iter()
                .map(|label| {
                    let start = label.span.start();
                    let len = label.span.end() - start;
                    let (line, col) = byte_offset_to_line_col(&self.source, start);
                    (format!("line {line}:{col} - {}", label.message), start, len)
                })
                .collect();
            let help =
                vrl_diag.notes.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n");
            // Formatter converts byte offsets to line/col in the main message
            let message = Formatter::new(&self.source, vec![vrl_diag]).to_string();
            MietteDiagnostic::new(message)
                .with_severity(match severity {
                    vrl::diagnostic::Severity::Error => miette::Severity::Error,
                    vrl::diagnostic::Severity::Warning | vrl::diagnostic::Severity::Bug => {
                        miette::Severity::Warning
                    }
                    vrl::diagnostic::Severity::Note => miette::Severity::Advice,
                })
                .with_labels(labels.into_iter().map(|(msg, offset, len)| {
                    LabeledSpan::new(Some(msg), offset, len)
                }))
                .with_help(help)
        })?;

        //TODO serde from Value to EventSource without json serialization/deserialization
        let output: Option<Vec<EventSource>> =
            serde_json::from_value(serde_json::to_value(res).into_diagnostic()?)
                .into_diagnostic()?;
        if let Some(outputs) = output {
            for output in outputs {
                self.next.send(output)?;
            }
            Ok(())
        } else {
            self.next.send(input)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipes::collect_to_vec;
    use pretty_assertions::assert_eq;

    #[test_trace::test]
    fn test_empty_template() {
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut processor = Processor::new("", Box::new(collector.create_pipe())).unwrap();
        let input = EventSource {
            metadata: serde_json::json!({"foo": "bar"}),
            headers: std::collections::HashMap::new(),
            body: serde_json::json!({"a": 1, "b": 2}),
        };
        processor.send(input.clone()).unwrap();
        let mut outputs = collector.try_into_iter().unwrap();
        assert_eq!(outputs.next(), Some(input));
        assert_eq!(outputs.next(), None);
    }

    #[test_trace::test]
    fn test_skip() {
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut processor = Processor::new("null", Box::new(collector.create_pipe())).unwrap();
        let input = EventSource {
            metadata: serde_json::json!({"foo": "bar"}),
            headers: std::collections::HashMap::new(),
            body: serde_json::json!({"a": 1, "b": 2}),
        };
        processor.send(input.clone()).unwrap();
        let mut outputs = collector.try_into_iter().unwrap();
        assert_eq!(outputs.next(), Some(input));
        assert_eq!(outputs.next(), None);
    }

    #[test_trace::test]
    fn test_drop() {
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut processor = Processor::new("[]", Box::new(collector.create_pipe())).unwrap();
        let input = EventSource {
            metadata: serde_json::json!({"foo": "bar"}),
            headers: std::collections::HashMap::new(),
            body: serde_json::json!({"a": 1, "b": 2}),
        };
        processor.send(input.clone()).unwrap();
        let mut outputs = collector.try_into_iter().unwrap();
        assert_eq!(outputs.next(), None);
    }

    #[test_trace::test]
    fn test_transform() {
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut processor = Processor::new(
            indoc::indoc! { r#"
            .body, err = { "c": (.body.a * 10 + .body.b) }
            if err != null {
                log(err, level: "error")
            }
            [.]"#},
            Box::new(collector.create_pipe()),
        )
        .unwrap();
        let input = EventSource {
            metadata: serde_json::json!({"foo": "bar"}),
            headers: std::collections::HashMap::new(),
            body: serde_json::json!({"a": 1, "b": 2}),
        };
        processor.send(input.clone()).unwrap();

        let expected = EventSource {
            metadata: serde_json::json!({"foo": "bar"}),
            headers: std::collections::HashMap::new(),
            body: serde_json::json!({"c": 12}),
        };
        let mut outputs = collector.try_into_iter().unwrap();
        assert_eq!(outputs.next(), Some(expected));
        assert_eq!(outputs.next(), None);
    }

    #[test_trace::test]
    fn test_compilation_error_returns_err() {
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let result = Processor::new("this is not valid vrl !!!", Box::new(collector.create_pipe()));
        assert!(result.is_err(), "invalid VRL should return an error");
        let err_msg = result.err().expect("is_err checked above").to_string();
        assert!(!err_msg.is_empty(), "compilation error should produce a non-empty message");
    }

    #[test_trace::test]
    fn test_runtime_error_is_propagated() {
        // `.nonexistent_field` on a missing path at runtime causes an error in VRL
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut processor = Processor::new(
            // del on a required field that doesn't exist raises a runtime error
            indoc::indoc! { r"
            .body.x = to_int!(.body.not_a_number)
            [.]"},
            Box::new(collector.create_pipe()),
        )
        .unwrap();
        let input = EventSource {
            metadata: serde_json::json!({}),
            headers: std::collections::HashMap::new(),
            body: serde_json::json!({"not_a_number": "hello"}),
        };
        let result = processor.send(input);
        assert!(result.is_err(), "runtime VRL error should propagate as Err");
    }

    #[test_trace::test]
    fn test_byte_offset_to_line_col_first_char() {
        assert_eq!(byte_offset_to_line_col("abc", 0), (1, 1));
    }

    #[test_trace::test]
    fn test_byte_offset_to_line_col_second_line() {
        assert_eq!(byte_offset_to_line_col("abc\ndef", 4), (2, 1));
    }

    #[test_trace::test]
    fn test_byte_offset_to_line_col_mid_line() {
        assert_eq!(byte_offset_to_line_col("abc\ndef", 5), (2, 2));
    }
}
