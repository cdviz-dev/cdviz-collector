use core::fmt;
use eventsource_stream::EventStreamError;
use nom::error::Error as NomError;
use reqwest::Error as ReqwestError;
// use reqwest::Response;
use reqwest::StatusCode;
use reqwest::header::HeaderValue;
use std::string::FromUtf8Error;

#[cfg(doc)]
use reqwest::RequestBuilder;

/// Error raised when a [`RequestBuilder`] cannot be cloned. See [`RequestBuilder::try_clone`] for
/// more information
#[derive(Debug, Clone, Copy)]
pub struct CannotCloneRequestError;

impl fmt::Display for CannotCloneRequestError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("expected a cloneable request")
    }
}

impl std::error::Error for CannotCloneRequestError {}

/// Error raised by the `EventSource` stream fetching and parsing
#[derive(
    Debug, derive_more::Error, derive_more::Display, derive_more::From, miette::Diagnostic,
)]
#[allow(clippy::result_large_err)]
pub enum Error {
    /// Source stream is not valid UTF8
    Utf8(FromUtf8Error),
    /// Source stream is not a valid `EventStream`
    Parser(NomError<String>),
    /// The HTTP Request could not be completed
    Transport(ReqwestError),
    /// The `Content-Type` returned by the server is invalid
    #[from(ignore)]
    #[display("Invalid header value: {:?}", _0)]
    InvalidContentType(#[error(not(source))] HeaderValue), // Box<Response>),
    /// The status code returned by the server is invalid
    #[from(ignore)]
    #[display("Invalid status code: {}", _0)]
    InvalidStatusCode(#[error(not(source))] StatusCode), //, Box<Response>),
    /// The `Last-Event-ID` cannot be formed into a Header to be submitted to the server
    #[display("Invalid `Last-Event-ID`: {}", _0)]
    InvalidLastEventId(#[error(not(source))] String),
    /// The stream ended
    #[display("Stream ended")]
    StreamEnded,
    #[display("Failed to clone the RequestBuilder")]
    CloneRequestBuilderFailed,
    #[display("Failed to retreive curren stream from memory")]
    CurrentStreamRetrievalFailed,
}

impl From<EventStreamError<ReqwestError>> for Error {
    fn from(err: EventStreamError<ReqwestError>) -> Self {
        match err {
            EventStreamError::Utf8(err) => Self::Utf8(err),
            EventStreamError::Parser(err) => Self::Parser(err),
            EventStreamError::Transport(err) => Self::Transport(err),
        }
    }
}
