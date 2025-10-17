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
        cause: serde_json::Error,
        #[source_code]
        input: String,
        #[label("{cause}")]
        location: SourceOffset,
    },
}

#[derive(Debug, derive_more::Display, derive_more::From)]
pub(crate) struct ReportWrapper(Report);

impl Error {
    /// Takes the input and the `serde_json::Error` and returns a `Error::Serde`
    /// that can be rendered nicely with miette.
    /// ```no_run
    /// serde_json::from_str(&input).map_err(|cause| Error::from_serde_error(input, cause))?
    /// ```
    pub fn from_serde_error(input: impl Into<String>, cause: serde_json::Error) -> Self {
        let input = input.into();
        let location = SourceOffset::from_location(&input, cause.line(), cause.column());
        Self::Serde { cause, input, location }
    }
}
