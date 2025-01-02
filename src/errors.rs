pub(crate) use miette::{miette, IntoDiagnostic, Report, Result};

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
}

#[derive(Debug, derive_more::Display, derive_more::From)]
pub(crate) struct ReportWrapper(Report);
