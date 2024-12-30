use crate::errors::ReportWrapper;
use crate::sources::EventSource;
use axum::extract::State;
use axum::routing::{post, Router};
use axum::Json;
use futures::lock::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::EventSourcePipe;

/// The webhook config
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Config {
    /// id of the webhook
    pub(crate) id: String,
}

pub(crate) fn make_route(config: &Config, next: EventSourcePipe) -> Router {
    Router::new()
        .route(&format!("/webhook/{}", config.id), post(webhook))
        .with_state(Arc::new(Mutex::new(next)))
}

//TODO support events in cloudevents format (extract info from headers)
//TODO try [deser](https://crates.io/crates/deserr) to return good error
//TODO use cloudevents
//TODO add metadata & headers info into SourceEvent
//TODO log & convert error
#[tracing::instrument(skip(next, body))]
async fn webhook(
    State(next): State<Arc<Mutex<EventSourcePipe>>>,
    Json(body): Json<serde_json::Value>,
) -> std::result::Result<axum::http::StatusCode, ReportWrapper> {
    tracing::trace!("received {:?}", &body);
    let event = EventSource { body, ..Default::default() };
    next.lock().await.send(event)?;
    Ok(axum::http::StatusCode::CREATED)
}
