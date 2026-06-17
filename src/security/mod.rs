pub mod header;
pub mod rule;
pub mod signature;

/// Enclose `value` with an optional `prefix` and `suffix`.
///
/// Empty `prefix`/`suffix` leave the value unchanged. Used to wrap a header
/// value (e.g. prepend `"Bearer "` to a token read from an env var or file)
/// without mutating the underlying source.
pub(crate) fn enclose(value: &str, prefix: &str, suffix: &str) -> String {
    format!("{prefix}{value}{suffix}")
}
