//! Fork/vendoring of <https://github.com/jpopesculian/reqwest-eventsource> (no updates since 0.6.0 2024-03-29 2y)
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
#![allow(dead_code)]
#![allow(unused_imports)]

mod error;
mod event_source;
mod reqwest_ext;
pub mod retry;

pub use error::{CannotCloneRequestError, Error};
pub use event_source::{Event, EventSource, ReadyState};
pub use reqwest_ext::RequestBuilderExt;
