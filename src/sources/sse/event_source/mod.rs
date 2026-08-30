//! Forked from <https://github.com/jpopesculian/reqwest-eventsource> (no updates since 0.6.0,
//! 2024-03-29). Upstream has been inactive 2+ years, so this is now internalized permanently
//! rather than pending a relink — treat it as this crate's own module, not vendored code to
//! reconcile with upstream. Future changes (e.g. folding it into our internal reqwest
//! middleware/helper stack) should edit it directly, no need to preserve a minimal diff.
//! - update to reqwest 0.13 (first motivation)
//! - replace thiserror by `derive_more` (better integration, remove dependencies to old thiserror 1.x)
//! - apply clippy suggestion to follow recent feature from rust
//! - remove "`unwrap()`" calls
//!
//! Provides a simple wrapper for [`reqwest`] to provide an Event Source implementation.
//! You can learn more about Server Sent Events (SSE) take a look at [the MDN
//! docs](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events/Using_server-sent_events)
//! This crate uses [`eventsource_stream`] to wrap the underlying Bytes stream, and retries failed
//! requests.
//!
//! # Example
//!
//! ```ignore
//! let mut es = EventSource::get("http://localhost:8000/events");
//! while let Some(event) = es.next().await {
//!     match event {
//!         Ok(Event::Open) => println!("Connection Open!"),
//!         Ok(Event::Message(message)) => println!("Message: {:#?}", message),
//!         Err(err) => {
//!             println!("Error: {}", err);
//!             es.close();
//!         }
//!     }
//! }
//! ```
mod error;
mod event_source;
mod reqwest_ext;

// `EventSource`/`CannotCloneRequestError`/`Error` are only named directly by tests (a bare SSE
// client in `sinks/sse.rs`'s integration test); everyday callers only need `Event` and
// `RequestBuilderExt::eventsource()`.
#[allow(unused_imports)]
pub(crate) use error::{CannotCloneRequestError, Error};
pub(crate) use event_source::Event;
#[allow(unused_imports)]
pub(crate) use event_source::EventSource;
pub(crate) use reqwest_ext::RequestBuilderExt;
