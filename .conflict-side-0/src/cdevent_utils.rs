use crate::errors::{IntoDiagnostic, Result};
use cdevents_sdk::Id;

#[must_use]
pub fn sanitize_id_str(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect::<String>()
}

#[allow(clippy::missing_errors_doc)]
pub fn sanitize_id(id: &Id) -> Result<Id> {
    sanitize_id_str(id.as_str()).try_into().into_diagnostic()
}
