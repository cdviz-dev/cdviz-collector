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
}

impl Processor {
    pub(crate) fn new(template: &str, next: EventSourcePipe) -> Result<Self> {
        // Use all of the std library functions
        let mut fns = vrl::stdlib::all();
        // Add custom PURL functions
        fns.extend(super::vrl_purl::all_custom_functions());
        // Compile the program (and panic if it's invalid)
        //TODO check result of compilation, log the error, warning, etc.
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
            Ok(res) => Ok(Self { next, renderer: res.program }),
        }
    }
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
            //tracing::error!(diagnostics = %formatter, "VRL compilation error");
            MietteDiagnostic::new(vrl_diag.message)
                .with_severity(match vrl_diag.severity {
                    vrl::diagnostic::Severity::Error => miette::Severity::Error,
                    vrl::diagnostic::Severity::Warning | vrl::diagnostic::Severity::Bug => {
                        miette::Severity::Warning
                    }
                    vrl::diagnostic::Severity::Note => miette::Severity::Advice,
                })
                .with_labels(vrl_diag.labels.iter().map(|label| {
                    LabeledSpan::new(
                        Some(label.message.clone()),
                        label.span.start(),
                        label.span.end() - label.span.start(),
                    )
                }))
                .with_help(
                    vrl_diag.notes.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n"),
                )
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
}
