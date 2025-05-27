use crate::errors::{IntoDiagnostic, Result};
use crate::pipes::Pipe;
use crate::sources::{EventSource, EventSourcePipe};
use handlebars::Handlebars;

pub(crate) struct Processor {
    next: EventSourcePipe,
    renderer: Handlebars<'static>,
}

impl Processor {
    pub(crate) fn new(template: &str, next: EventSourcePipe) -> Result<Self> {
        let mut renderer = Handlebars::new();
        renderer.set_dev_mode(false);
        renderer.set_strict_mode(true);
        renderer.register_escape_fn(handlebars::no_escape);
        handlebars_misc_helpers::register(&mut renderer);
        renderer.register_template_string("tpl", template).into_diagnostic()?;
        Ok(Self { next, renderer })
    }
}

impl Pipe for Processor {
    type Input = EventSource;
    fn send(&mut self, input: Self::Input) -> Result<()> {
        let res = self.renderer.render("tpl", &input).into_diagnostic()?;
        let output: Option<Vec<EventSource>> = serde_json::from_str(&res).into_diagnostic()?;
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

    #[test]
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

    #[test]
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

    #[test]
    fn test_transform() {
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut processor = Processor::new(
            indoc::indoc! { r#"[
            {
                "metadata": {{ json_to_str metadata }},
                "headers": {{ json_to_str headers }},
                "body": {
                    "c" : {{ body.a }}{{ body.b }}
                }
            }
            ]"#},
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
