#![allow(unused_assignments)] // fields are used by miette
pub(crate) use miette::{IntoDiagnostic, Report, Result, SourceOffset, miette};

// Nightly requires enabling this feature:
// #![feature(error_generic_member_access)]
#[derive(
    Debug, derive_more::Error, derive_more::Display, derive_more::From, miette::Diagnostic,
)]
#[non_exhaustive]
pub(crate) enum Error {
    #[from(ignore)]
    #[display("config file not found: {path}")]
    ConfigNotFound { path: String },
    #[from(ignore)]
    #[display("config of transformer not found: {}", _0)]
    ConfigTransformerNotFound(#[error(ignore)] String),
    #[display("no source found (configured or started)")]
    NoSource,
    #[display("no sink found (configured or started)")]
    NoSink,
    #[display("malformed json provided")]
    Serde {
        source: serde_json::Error,
        #[source_code]
        input: String,
        #[label("{source}")]
        location: SourceOffset,
    },
    #[from(ignore)]
    #[display("{reason}")]
    Rejected {
        #[error(ignore)]
        reason: String,
    },
}

#[derive(Debug, derive_more::Display, derive_more::From)]
pub(crate) struct ReportWrapper(pub(crate) Report);

impl Error {
    /// Takes the input and the `serde_json::Error` and returns a `Error::Serde`
    /// that can be rendered nicely with miette.
    /// ```no_run
    /// serde_json::from_str(&input).map_err(|cause| Error::from_serde_error(input, cause))?
    /// ```
    pub fn from_serde_error(input: impl Into<String>, cause: serde_json::Error) -> Self {
        let input = input.into();
        let location = SourceOffset::from_location(&input, cause.line(), cause.column());
        Self::Serde { source: cause, input, location }
    }

    /// Maps an error variant to the HTTP status code it should be reported as.
    /// Variants caused by the caller's payload (schema mismatch, transform
    /// rejection) map to a 4xx; anything else is treated as an internal failure.
    pub(crate) fn status_code(&self) -> axum::http::StatusCode {
        match self {
            Self::Serde { .. } => axum::http::StatusCode::BAD_REQUEST,
            Self::Rejected { .. } => axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            _ => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}
