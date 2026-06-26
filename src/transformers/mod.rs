pub mod collect_to_vec;
pub(crate) mod deduplicate;
pub(crate) mod discard_all;
pub(crate) mod log;
pub(crate) mod passthrough;
#[cfg(feature = "transformer_vrl")]
mod vrl;
#[cfg(feature = "transformer_vrl")]
pub(crate) mod vrl_purl;

use crate::{
    errors::{Error, IntoDiagnostic, Result, miette},
    event::{Event, EventPipe},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A pipe is an interface to implement processor for inputs.
///
/// The composition of Pipes to create pipeline could be done by configuration,
/// and the behavior of the pipe should be internal,
/// so chaining of pipes should not depends of method `map`, `fold`, `filter`,
/// `filter_map`, `drop`,... like for `Iterator`, `Stream`, `RxRust`.
/// Also being able to return Error to the sender could help the Sender to ease handling (vs `Stream`)
/// like retry, buffering, forward to its caller...
pub trait Pipe {
    type Input;

    fn send(&mut self, input: Self::Input) -> Result<()>;
}

impl<I, T: Pipe<Input = I> + ?Sized> Pipe for Box<T> {
    type Input = I;
    fn send(&mut self, input: Self::Input) -> Result<()> {
        T::send(self, input)
    }
}

/// Wraps a pipe so each `send` runs inside a tracing span.
///
/// The source-side pipe chain is fully synchronous within one `send` call, so the
/// `span.enter()` guard nests correctly and the trace context captured at the terminal
/// (`send_cdevents`) reflects the innermost span. The span macro name is static; the
/// human-readable name (source name or transformer name) is carried in the `name` field.
pub(crate) struct SpanPipe<P> {
    next: P,
    kind: &'static str,
    name: String,
}

impl<P> SpanPipe<P> {
    pub(crate) fn new(next: P, kind: &'static str, name: String) -> Self {
        Self { next, kind, name }
    }
}

impl<P: Pipe> Pipe for SpanPipe<P> {
    type Input = P::Input;
    fn send(&mut self, input: Self::Input) -> Result<()> {
        // `otel.status_code`/`error` are declared Empty so they can be recorded on failure:
        // marks the span as errored (red in trace viewers) and attaches the error message to
        // the span itself — without re-logging to stderr (the extractor still logs the skip).
        let span = tracing::info_span!(
            "pipe",
            kind = self.kind,
            name = %self.name,
            otel.status_code = tracing::field::Empty,
            error = tracing::field::Empty,
        );
        let _g = span.enter();
        let res = self.next.send(input);
        if let Err(ref err) = res {
            span.record("otel.status_code", "error");
            span.record("error", tracing::field::display(err));
        }
        res
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(tag = "type")]
pub(crate) enum Config {
    #[serde(alias = "passthrough")]
    #[default]
    Passthrough,
    #[serde(alias = "log")]
    Log(log::Config),
    #[serde(alias = "deduplicate")]
    Deduplicate(deduplicate::Config),
    #[serde(alias = "discard_all")]
    DiscardAll,
    #[cfg(feature = "transformer_vrl")]
    #[serde(alias = "vrl")]
    Vrl { template: String },
}

impl Config {
    /// Stable type label used to name spans / derive default transformer names.
    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Config::Passthrough => "passthrough",
            Config::Log(_) => "log",
            Config::Deduplicate(_) => "deduplicate",
            Config::DiscardAll => "discard_all",
            #[cfg(feature = "transformer_vrl")]
            Config::Vrl { .. } => "vrl",
        }
    }

    pub(crate) fn make_transformer(&self, next: EventPipe) -> Result<EventPipe> {
        let out: EventPipe = match &self {
            Config::Passthrough => Box::new(passthrough::Processor::new(next)),
            Config::Log(config) => Box::new(log::Processor::try_from(config, next)?),
            Config::Deduplicate(config) => Box::new(deduplicate::Processor::new(config, next)),
            Config::DiscardAll => Box::new(discard_all::Processor::new()),
            #[cfg(feature = "transformer_vrl")]
            Config::Vrl { template } => Box::new(vrl::Processor::new(template, next)?),
        };
        Ok(out)
    }
}

/// A transformer config plus its derived key — the ref name for referenced transformers,
/// `None` for inline ones (which key off their `type`). The key is set programmatically
/// during resolution, never authored in TOML (so the `type` tag + variant fields stay flat).
/// The span / log name is `key#position`; position disambiguates the same transformer
/// appearing several times in one chain.
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub(crate) struct NamedConfig {
    #[serde(skip)]
    pub(crate) key: Option<String>,
    #[serde(flatten)]
    pub(crate) config: Config,
}

impl NamedConfig {
    fn span_name(&self, index: usize) -> String {
        let key = self.key.as_deref().unwrap_or_else(|| self.config.type_name());
        format!("{key}#{index}")
    }
}

pub(crate) fn build_transformer_chain(
    configs: &[NamedConfig],
    terminal: EventPipe,
) -> Result<EventPipe> {
    let mut pipe = terminal;
    for (i, nc) in configs.iter().enumerate().rev() {
        let inner = nc.config.make_transformer(pipe)?;
        pipe = Box::new(SpanPipe::new(inner, "transformer", nc.span_name(i)));
    }
    Ok(pipe)
}

pub fn resolve_transformer_refs(
    transformer_refs: &[String],
    configs: &HashMap<String, Config>,
) -> Result<Vec<Config>> {
    let transformers = transformer_refs
        .iter()
        .map(|name| {
            configs
                .get(name)
                .cloned()
                .ok_or_else(|| Error::ConfigTransformerNotFound(name.clone()))
                .into_diagnostic()
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(transformers)
}

/// Like `resolve_transformer_refs`, but keys each resolved transformer by its ref name.
pub fn resolve_transformer_refs_named(
    transformer_refs: &[String],
    configs: &HashMap<String, Config>,
) -> Result<Vec<NamedConfig>> {
    transformer_refs
        .iter()
        .map(|name| {
            configs
                .get(name)
                .cloned()
                .map(|config| NamedConfig { key: Some(name.clone()), config })
                .ok_or_else(|| Error::ConfigTransformerNotFound(name.clone()))
                .into_diagnostic()
        })
        .collect()
}

/// Shared config for transformer chains used by both sources and sinks.
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub(crate) struct TransformerChainConfig {
    #[serde(default)]
    pub(crate) transformer_refs: Vec<String>,
    #[serde(default)]
    pub(crate) transformers: Vec<NamedConfig>,
}

impl TransformerChainConfig {
    pub(crate) fn resolve(&mut self, configs: &HashMap<String, Config>) -> Result<()> {
        let mut named = resolve_transformer_refs_named(&self.transformer_refs, configs)?;
        self.transformers.append(&mut named);
        Ok(())
    }

    pub(crate) fn append(&mut self, extra: &[NamedConfig]) {
        self.transformers.extend_from_slice(extra);
    }
}

/// Push-mode transformer chain wrapped in `Arc<Mutex>`.
/// The terminal pipe is caller-provided; sources use `build_transformer_chain` directly.
#[derive(Clone)]
pub(crate) struct TransformerChain(Arc<Mutex<EventPipe>>);

impl std::fmt::Debug for TransformerChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransformerChain").finish_non_exhaustive()
    }
}

impl TransformerChain {
    pub(crate) fn try_new(configs: &[NamedConfig], terminal: EventPipe) -> Result<Self> {
        let chain = build_transformer_chain(configs, terminal)?;
        Ok(Self(Arc::new(Mutex::new(chain))))
    }

    pub(crate) fn push(&self, event: Event) -> Result<()> {
        self.0.lock().map_err(|e| miette!("{e}"))?.send(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transformers::collect_to_vec;
    use serde_json::json;

    fn named(configs: &[Config]) -> Vec<NamedConfig> {
        configs.iter().map(|c| NamedConfig { key: None, config: c.clone() }).collect()
    }

    fn make_chain(configs: &[Config]) -> (TransformerChain, collect_to_vec::Collector<Event>) {
        let collector = collect_to_vec::Collector::<Event>::new();
        let terminal: EventPipe = Box::new(collector.create_pipe());
        let chain = TransformerChain::try_new(&named(configs), terminal).unwrap();
        (chain, collector)
    }

    fn sample_event() -> Event {
        Event { metadata: json!({}), body: json!({"key": "value"}), ..Default::default() }
    }

    #[test]
    fn span_pipe_propagates_inner_error() {
        struct Failing;
        impl Pipe for Failing {
            type Input = Event;
            fn send(&mut self, _input: Event) -> Result<()> {
                Err(miette!("boom"))
            }
        }
        let mut pipe = SpanPipe::new(Failing, "transformer", "x#0".to_string());
        // SpanPipe records error status on the span but must NOT swallow the error.
        let err = pipe.send(sample_event()).unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn span_name_is_key_then_position() {
        let mut chain = TransformerChainConfig {
            transformers: named(&[Config::Passthrough]), // inline -> keys off its type
            transformer_refs: vec!["my_github".to_string(), "my_github".to_string()],
        };
        let mut registry = HashMap::new();
        registry.insert("my_github".to_string(), Config::DiscardAll);
        chain.resolve(&registry).unwrap(); // append the two refs
        chain.append(&named(&[Config::Passthrough])); // an inline-keyed global

        let names: Vec<String> =
            chain.transformers.iter().enumerate().map(|(i, nc)| nc.span_name(i)).collect();
        // inline -> type#pos; the same ref twice -> disambiguated by position; global -> type#pos
        assert_eq!(names, vec!["passthrough#0", "my_github#1", "my_github#2", "passthrough#3"]);
    }

    #[test]
    fn empty_chain_passes_event_through() {
        let (chain, collector) = make_chain(&[]);
        chain.push(sample_event()).unwrap();
        let events = collector.drain().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].body, json!({"key": "value"}));
    }

    #[test]
    fn passthrough_preserves_event() {
        let (chain, collector) = make_chain(&[Config::Passthrough]);
        chain.push(sample_event()).unwrap();
        let events = collector.drain().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].body, json!({"key": "value"}));
    }

    #[test]
    fn discard_all_drops_events() {
        let (chain, collector) = make_chain(&[Config::DiscardAll]);
        chain.push(sample_event()).unwrap();
        assert!(collector.drain().unwrap().is_empty());
    }

    #[test]
    fn multiple_events_are_collected_in_order() {
        let (chain, collector) = make_chain(&[Config::Passthrough]);
        for i in 0..3u32 {
            chain.push(Event { body: json!(i), ..Default::default() }).unwrap();
        }
        let events = collector.drain().unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].body, json!(0));
        assert_eq!(events[1].body, json!(1));
        assert_eq!(events[2].body, json!(2));
    }

    #[test]
    fn discard_all_first_drops_before_passthrough() {
        // DiscardAll is applied first → event never reaches Passthrough or terminal
        let (chain, collector) = make_chain(&[Config::DiscardAll, Config::Passthrough]);
        chain.push(sample_event()).unwrap();
        assert!(collector.drain().unwrap().is_empty());
    }

    #[test]
    fn passthrough_first_then_discard_all_drops_at_second_step() {
        // Passthrough is applied first (passes through), DiscardAll is applied second (drops)
        let (chain, collector) = make_chain(&[Config::Passthrough, Config::DiscardAll]);
        chain.push(sample_event()).unwrap();
        assert!(collector.drain().unwrap().is_empty());
    }

    /// Verifies that configs are applied left-to-right: configs[0] runs first, configs[1] second.
    #[cfg(feature = "transformer_vrl")]
    #[test]
    fn vrl_chain_applies_transformers_in_config_order() {
        // First transformer sets counter = 10.
        // Second transformer increments it.
        // If order is respected, the result is 11; reversed order would give 10.
        let configs = vec![
            Config::Vrl { template: ".metadata.counter = 10\n[.]".to_string() },
            Config::Vrl {
                template: ".metadata.counter = int!(.metadata.counter) + 1\n[.]".to_string(),
            },
        ];
        let (chain, collector) = make_chain(&configs);
        chain.push(sample_event()).unwrap();
        let events = collector.drain().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].metadata["counter"], json!(11));
    }

    /// Verifies that a VRL transformer can filter (emit zero events).
    #[cfg(feature = "transformer_vrl")]
    #[test]
    fn vrl_can_discard_events_by_returning_empty_array() {
        let configs = vec![Config::Vrl { template: "[]".to_string() }];
        let (chain, collector) = make_chain(&configs);
        chain.push(sample_event()).unwrap();
        assert!(collector.drain().unwrap().is_empty());
    }

    /// Verifies that a VRL transformer can fan out (emit multiple events).
    #[cfg(feature = "transformer_vrl")]
    #[test]
    fn vrl_can_fan_out_one_event_into_many() {
        let configs = vec![Config::Vrl { template: "[., .]".to_string() }];
        let (chain, collector) = make_chain(&configs);
        chain.push(sample_event()).unwrap();
        assert_eq!(collector.drain().unwrap().len(), 2);
    }
}
