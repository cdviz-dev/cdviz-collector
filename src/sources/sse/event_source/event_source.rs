use super::error::{CannotCloneRequestError, Error};
use core::pin::Pin;
use eventsource_stream::Eventsource;
pub use eventsource_stream::{Event as MessageEvent, EventStreamError};
#[cfg(not(target_arch = "wasm32"))]
use futures_core::future::BoxFuture;
use futures_core::future::Future;
#[cfg(target_arch = "wasm32")]
use futures_core::future::LocalBoxFuture;
#[cfg(not(target_arch = "wasm32"))]
use futures_core::stream::BoxStream;
#[cfg(target_arch = "wasm32")]
use futures_core::stream::LocalBoxStream;
use futures_core::stream::Stream;
use futures_core::task::{Context, Poll};
use pin_project_lite::pin_project;
use reqwest::header::HeaderValue;
use reqwest::{Error as ReqwestError, IntoUrl, RequestBuilder, Response, StatusCode};

#[cfg(not(target_arch = "wasm32"))]
type ResponseFuture = BoxFuture<'static, Result<Response, ReqwestError>>;
#[cfg(target_arch = "wasm32")]
type ResponseFuture = LocalBoxFuture<'static, Result<Response, ReqwestError>>;

#[cfg(not(target_arch = "wasm32"))]
type EventStream = BoxStream<'static, Result<MessageEvent, EventStreamError<ReqwestError>>>;
#[cfg(target_arch = "wasm32")]
type EventStream = LocalBoxStream<'static, Result<MessageEvent, EventStreamError<ReqwestError>>>;

pin_project! {
/// Provides the [`Stream`] implementation for the [`Event`] items. This wraps the
/// [`RequestBuilder`] and retries requests when they fail.
#[project = EventSourceProjection]
pub struct EventSource {
    builder: RequestBuilder,
    #[pin]
    next_response: Option<ResponseFuture>,
    #[pin]
    cur_stream: Option<EventStream>,
    is_closed: bool,
    last_event_id: String,
}
}

impl EventSource {
    /// Wrap a [`RequestBuilder`]
    pub fn new(builder: RequestBuilder) -> Result<Self, CannotCloneRequestError> {
        let builder =
            builder.header(reqwest::header::ACCEPT, HeaderValue::from_static("text/event-stream"));
        let res_future = Box::pin(builder.try_clone().ok_or(CannotCloneRequestError)?.send());
        Ok(Self {
            builder,
            next_response: Some(res_future),
            cur_stream: None,
            is_closed: false,
            last_event_id: String::new(),
        })
    }

    /// Create a simple `EventSource` based on a GET request
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn get<T: IntoUrl>(url: T) -> Result<Self, CannotCloneRequestError> {
        Self::new(reqwest::Client::new().get(url))
    }

    /// Get the last event id
    pub fn last_event_id(&self) -> &str {
        &self.last_event_id
    }
}

fn check_response(response: Response) -> Result<Response, Error> {
    match response.status() {
        StatusCode::OK => {}
        status => {
            return Err(Error::InvalidStatusCode(status /*, response*/));
        }
    }
    let Some(content_type) = response.headers().get(&reqwest::header::CONTENT_TYPE) else {
        return Err(Error::InvalidContentType(HeaderValue::from_static("") /*, response*/));
    };
    if content_type
        .to_str()
        .map_err(|_| ())
        .and_then(|s| s.parse::<mime::Mime>().map_err(|_| ()))
        .is_ok_and(|mime_type| {
            matches!((mime_type.type_(), mime_type.subtype()), (mime::TEXT, mime::EVENT_STREAM))
        })
    {
        Ok(response)
    } else {
        Err(Error::InvalidContentType(content_type.clone() /*, response*/))
    }
}

impl EventSourceProjection<'_> {
    fn clear_fetch(&mut self) {
        self.next_response.take();
        self.cur_stream.take();
    }

    fn handle_response(&mut self, res: Response) {
        let mut stream = res.bytes_stream().eventsource();
        stream.set_last_event_id(self.last_event_id.clone());
        self.cur_stream.replace(Box::pin(stream));
    }

    fn handle_event(&mut self, event: &MessageEvent) {
        self.last_event_id.clone_from(&event.id);
    }

    /// On any error the stream ends; reconnection (with backoff) is the caller's
    /// responsibility, using [`EventSource::last_event_id`] to resume.
    fn handle_error(&mut self) {
        self.clear_fetch();
        *self.is_closed = true;
    }
}

/// Events created by the [`EventSource`]
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Event {
    /// The event fired when the connection is opened
    Open,
    /// The event fired when a [`MessageEvent`] is received
    Message(MessageEvent),
}

impl From<MessageEvent> for Event {
    fn from(event: MessageEvent) -> Self {
        Event::Message(event)
    }
}

impl Stream for EventSource {
    type Item = Result<Event, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        if *this.is_closed {
            return Poll::Ready(None);
        }

        if let Some(response_future) = this.next_response.as_mut().as_pin_mut() {
            match response_future.poll(cx) {
                Poll::Ready(Ok(res)) => {
                    this.clear_fetch();
                    match check_response(res) {
                        Ok(res) => {
                            this.handle_response(res);
                            return Poll::Ready(Some(Ok(Event::Open)));
                        }
                        Err(err) => {
                            *this.is_closed = true;
                            return Poll::Ready(Some(Err(err)));
                        }
                    }
                }
                Poll::Ready(Err(err)) => {
                    let err = Error::Transport(err);
                    this.handle_error();
                    return Poll::Ready(Some(Err(err)));
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }

        let Some(mut cur_stream) = this.cur_stream.as_mut().as_pin_mut() else {
            return Poll::Ready(Some(Err(Error::CurrentStreamRetrievalFailed)));
        };
        match cur_stream.as_mut().poll_next(cx) {
            Poll::Ready(Some(Err(err))) => {
                let err = err.into();
                this.handle_error();
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(Some(Ok(event))) => {
                this.handle_event(&event);
                Poll::Ready(Some(Ok(event.into())))
            }
            Poll::Ready(None) => {
                this.handle_error();
                Poll::Ready(Some(Err(Error::StreamEnded)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use wiremock::matchers::method;

    #[tokio::test]
    async fn check_response_accepts_event_stream() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream"))
            .mount(&server)
            .await;
        let res = reqwest::get(server.uri()).await.unwrap();
        assert!(check_response(res).is_ok());
    }

    #[tokio::test]
    async fn check_response_rejects_non_200_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
        let res = reqwest::get(server.uri()).await.unwrap();
        assert!(matches!(check_response(res), Err(Error::InvalidStatusCode(_))));
    }

    #[tokio::test]
    async fn check_response_rejects_wrong_content_type() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).insert_header("content-type", "application/json"))
            .mount(&server)
            .await;
        let res = reqwest::get(server.uri()).await.unwrap();
        assert!(matches!(check_response(res), Err(Error::InvalidContentType(_))));
    }

    #[tokio::test]
    async fn check_response_rejects_missing_content_type() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        let res = reqwest::get(server.uri()).await.unwrap();
        assert!(matches!(check_response(res), Err(Error::InvalidContentType(_))));
    }

    #[tokio::test]
    async fn stream_emits_open_then_message_and_tracks_last_event_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw("id: 42\ndata: hello\n\n", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let mut es = EventSource::get(server.uri()).unwrap();
        assert_eq!(es.next().await.unwrap().unwrap(), Event::Open);
        let Event::Message(msg) = es.next().await.unwrap().unwrap() else {
            panic!("expected a Message event");
        };
        assert_eq!(msg.data, "hello");
        assert_eq!(es.last_event_id(), "42");
    }
}
