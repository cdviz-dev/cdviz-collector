use serde::{Deserialize, Serialize};

/// The webhook config
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Config {
    /// id of the webhook
    pub(crate) id: String,
}
