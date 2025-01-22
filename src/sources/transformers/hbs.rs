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
        let output: EventSource = serde_json::from_str(&res).into_diagnostic()?;
        self.next.send(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipes::collect_to_vec;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_empty_template() {
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut processor = Processor::new(
            indoc::indoc! { r#"{
                "metadata": {{ json_to_str metadata }},
                "header": {{ json_to_str header }},
                "body": {
                    "c" : {{ body.a }}{{ body.b }}
                }
            }"#},
            Box::new(collector.create_pipe()),
        )
        .unwrap();
        let input = EventSource {
            metadata: serde_json::json!({"foo": "bar"}),
            header: std::collections::HashMap::new(),
            body: serde_json::json!({"a": 1, "b": 2}),
        };
        processor.send(input.clone()).unwrap();
        let output = collector.try_into_iter().unwrap().next().unwrap();
        let expected = EventSource {
            metadata: serde_json::json!({"foo": "bar"}),
            header: std::collections::HashMap::new(),
            body: serde_json::json!({"c": 12}),
        };
        //dbg!(&output);
        assert_eq!(output, expected);
    }
}
