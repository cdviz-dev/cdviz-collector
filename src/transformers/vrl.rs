use super::Pipe;
use crate::errors::{Error, IntoDiagnostic, Result};
use crate::event::{Event as EventSource, EventPipe as EventSourcePipe};
use miette::MietteDiagnostic;
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

impl Pipe for Processor {
    type Input = EventSource;
    //TODO optimize: EventSource currently round-trips through serde_json::Value to enter/exit the
    // VRL runtime; a direct Value↔EventSource conversion would avoid two alloc+parse passes.
    // Add a microbenchmark before optimising.
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
        // A runtime failure here is treated as a rejection of this specific payload, not an
        // internal server error: the program is compiled once at startup, so a per-request
        // failure is almost always the payload not matching the transform's contract.
        let res = self.renderer.resolve(&mut ctx).map_err(|cause| {
            let vrl_diag = Diagnostic::from(cause);
            // Formatter converts byte offsets to line/col in the main message
            let message = Formatter::new(&self.source, vec![vrl_diag]).to_string();
            Error::Rejected { reason: message }
        })?;

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
    use crate::transformers::collect_to_vec;
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
        let err = result.expect_err("runtime VRL error should propagate as Err");
        assert!(
            err.downcast_ref::<Error>().is_some_and(|e| matches!(e, Error::Rejected { .. })),
            "runtime VRL error should downcast to Error::Rejected so it maps to a 4xx response"
        );
    }

    #[test_trace::test]
    fn test_fanout_one_to_many() {
        // A VRL program can return an array of multiple EventSource objects.
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut processor = Processor::new(
            // Return the same input twice
            "[., .]",
            Box::new(collector.create_pipe()),
        )
        .unwrap();
        let input = EventSource {
            metadata: serde_json::json!({}),
            headers: std::collections::HashMap::new(),
            body: serde_json::json!({"k": "v"}),
        };
        processor.send(input.clone()).unwrap();
        let outputs: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0], input);
        assert_eq!(outputs[1], input);
    }

    #[test_trace::test]
    fn test_runtime_error_diagnostic_contains_location() {
        // Verify the error message produced by a runtime failure carries location info
        // (non-empty message with the offending source).
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let src = ".body.x = to_int!(.body.not_a_number)\n[.]";
        let mut processor = Processor::new(src, Box::new(collector.create_pipe())).unwrap();
        let input = EventSource {
            metadata: serde_json::json!({}),
            headers: std::collections::HashMap::new(),
            body: serde_json::json!({"not_a_number": "hello"}),
        };
        let err = processor.send(input).unwrap_err();
        let msg = err.to_string();
        // The diagnostic must be non-empty and contain the problematic source fragment.
        assert!(!msg.is_empty(), "runtime error should produce a non-empty diagnostic");
        assert!(
            msg.contains("to_int") || msg.contains("not_a_number") || msg.contains("1:"),
            "diagnostic should reference the error location or relevant source; got: {msg}"
        );
    }
}
