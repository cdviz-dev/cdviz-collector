use super::Pipe;
use crate::{errors::Result, event::Event};
use indexmap::IndexSet;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Config {
    /// Max number of keys to remember (FIFO eviction when full). `0` disables deduplication.
    capacity: usize,
    /// JSON Pointer (RFC 6901) into `event.body`, e.g. `/context/id`.
    key_path: String,
}

pub(crate) struct Processor<N> {
    capacity: usize,
    key_path: String,
    /// Insertion-ordered set: O(1) lookup + `shift_remove` for FIFO eviction.
    seen: IndexSet<String>,
    next: N,
}

impl<N> Processor<N> {
    pub(crate) fn new(config: &Config, next: N) -> Self {
        Self {
            capacity: config.capacity,
            key_path: config.key_path.clone(),
            seen: IndexSet::with_capacity(config.capacity),
            next,
        }
    }
}

impl<N> Pipe for Processor<N>
where
    N: Pipe<Input = Event>,
{
    type Input = Event;
    fn send(&mut self, event: Event) -> Result<()> {
        if self.capacity == 0 {
            return self.next.send(event);
        }
        let key = event.body.pointer(&self.key_path).map(std::string::ToString::to_string);
        match key {
            None => {
                tracing::warn!(
                    key_path = self.key_path,
                    "deduplicate: key not found, passing through"
                );
                self.next.send(event)
            }
            Some(k) if self.seen.contains(&k) => {
                tracing::warn!(key = k, "deduplicate: duplicate event dropped");
                Ok(())
            }
            Some(k) => {
                if self.seen.len() >= self.capacity {
                    self.seen.shift_remove_index(0);
                }
                self.seen.insert(k);
                self.next.send(event)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transformers::collect_to_vec::Collector;
    use serde_json::json;

    fn make_processor(
        capacity: usize,
        key_path: &str,
    ) -> (Processor<crate::transformers::collect_to_vec::Processor<Event>>, Collector<Event>) {
        let collector = Collector::<Event>::new();
        let config = Config { capacity, key_path: key_path.to_string() };
        let proc = Processor::new(&config, collector.create_pipe());
        (proc, collector)
    }

    fn event_with_id(id: &str) -> Event {
        Event { body: json!({ "context": { "id": id } }), ..Default::default() }
    }

    #[test]
    fn unique_events_pass_through() {
        let (mut proc, collector) = make_processor(10, "/context/id");
        proc.send(event_with_id("a")).unwrap();
        proc.send(event_with_id("b")).unwrap();
        proc.send(event_with_id("c")).unwrap();
        assert_eq!(collector.drain().unwrap().len(), 3);
    }

    #[test]
    fn duplicate_is_dropped() {
        let (mut proc, collector) = make_processor(10, "/context/id");
        proc.send(event_with_id("x")).unwrap();
        proc.send(event_with_id("x")).unwrap();
        assert_eq!(collector.drain().unwrap().len(), 1);
    }

    #[test]
    fn fifo_eviction_allows_reuse_of_oldest_key() {
        let (mut proc, collector) = make_processor(2, "/context/id");
        proc.send(event_with_id("a")).unwrap(); // seen: {a}
        proc.send(event_with_id("b")).unwrap(); // seen: {a, b}
        proc.send(event_with_id("c")).unwrap(); // seen: {b, c} — "a" evicted
        proc.send(event_with_id("a")).unwrap(); // "a" no longer seen → passes through
        assert_eq!(collector.drain().unwrap().len(), 4);
    }

    #[test]
    fn missing_key_passes_through() {
        let (mut proc, collector) = make_processor(10, "/nonexistent/path");
        proc.send(event_with_id("a")).unwrap();
        assert_eq!(collector.drain().unwrap().len(), 1);
    }

    #[test]
    fn capacity_zero_disables_deduplication() {
        let (mut proc, collector) = make_processor(0, "/context/id");
        proc.send(event_with_id("a")).unwrap();
        proc.send(event_with_id("a")).unwrap();
        assert_eq!(collector.drain().unwrap().len(), 2);
    }
}
