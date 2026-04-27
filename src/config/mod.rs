#[cfg(feature = "config_remote")]
mod remote_file_adapter;
mod toml_provider;

pub(crate) const CONNECT_BASE_CONFIG: &str = include_str!("../assets/connect.base.toml");
pub(crate) const SEND_BASE_CONFIG: &str = include_str!("../assets/send.base.toml");

use crate::{
    errors::{Error, IntoDiagnostic, Result},
    http, pipeline, sinks, sources, state, transformers,
};
use figment::{
    Figment,
    providers::{Env, Format},
};
use figment_file_provider_adapter::FileAdapter;
#[cfg(feature = "config_remote")]
use remote_file_adapter::RemoteFileAdapter;
use serde::Deserialize;
use std::{collections::HashMap, path::PathBuf, str::FromStr};
pub use toml_provider::Toml;

/// A configuration source: either a local file path or an HTTP/HTTPS URL.
#[derive(Debug, Clone)]
pub(crate) enum ConfigSource {
    File(PathBuf),
    #[cfg(feature = "config_http")]
    Url(url::Url),
}

impl FromStr for ConfigSource {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if let Ok(url) = url::Url::parse(s) {
            match url.scheme() {
                "http" | "https" => {
                    #[cfg(feature = "config_http")]
                    return Ok(ConfigSource::Url(url));
                    #[cfg(not(feature = "config_http"))]
                    return Err(format!(
                        "URL config sources require the `config_http` feature: {s}"
                    ));
                }
                _ => {}
            }
        }
        Ok(ConfigSource::File(PathBuf::from(s)))
    }
}

#[derive(Clone, Debug, Deserialize, Default)]
pub(crate) struct Config {
    #[serde(default)]
    pub(crate) sources: HashMap<String, sources::Config>,
    #[serde(default)]
    pub(crate) sinks: HashMap<String, sinks::Config>,
    // extractors: HashMap<String, sources::extractors::Config>,
    #[serde(default)]
    pub(crate) transformers: HashMap<String, transformers::Config>,
    #[serde(default)]
    pub(crate) http: http::Config,
    #[serde(default)]
    pub(crate) pipeline: pipeline::PipelineConfig,
    #[serde(default)]
    pub(crate) state: state::Config,
}

/// Builder for Config with flexible configuration loading options
pub(crate) struct ConfigBuilder {
    base_config: Option<String>,
    config_file: Option<PathBuf>,
    config_content: Option<String>,
    cli_overrides: Option<String>,
    key_value_overrides: Option<String>,
    enable_env_vars: bool,
}

impl ConfigBuilder {
    /// Create a new `ConfigBuilder` with default base configuration
    pub fn new() -> Self {
        Self {
            base_config: Some(CONNECT_BASE_CONFIG.to_string()),
            config_file: None,
            config_content: None,
            cli_overrides: None,
            key_value_overrides: None,
            enable_env_vars: true,
        }
    }

    /// Set custom base configuration (useful for commands like send)
    pub fn with_base_config(mut self, base_config: &str) -> Self {
        self.base_config = Some(base_config.to_string());
        self
    }

    /// Add a user configuration file
    pub fn with_config_file(mut self, config_file: Option<PathBuf>) -> Self {
        self.config_file = config_file;
        self
    }

    /// Add pre-fetched TOML text (used for URL config sources)
    pub fn with_config_text(mut self, content: Option<String>) -> Self {
        self.config_content = content;
        self
    }

    /// Apply a resolved config source (file path or pre-fetched content).
    ///
    /// Prefer this over calling `with_config_file` + `with_config_text` separately,
    /// as it enforces the XOR invariant enforced by [`ResolvedConfigSource`].
    pub fn with_resolved_source(self, source: ResolvedConfigSource) -> Self {
        match source {
            ResolvedConfigSource::None => self,
            ResolvedConfigSource::File(path) => self.with_config_file(Some(path)),
            ResolvedConfigSource::Content(content) => self.with_config_text(Some(content)),
        }
    }

    /// Add CLI overrides as TOML string
    pub fn with_cli_overrides(mut self, cli_overrides: Option<String>) -> Self {
        self.cli_overrides = cli_overrides;
        self
    }

    /// Override individual config values from `key=value` pairs.
    ///
    /// Values are auto-typed in order: `true`/`false` → bool, integers → `i64`,
    /// decimals → `f64`, everything else → quoted string.
    /// Only the first `=` is treated as separator, so values like URLs are safe.
    /// These overrides are applied after `with_cli_overrides` and take highest priority.
    pub fn with_keyvalue(mut self, kvs: &[String]) -> Result<Self> {
        if !kvs.is_empty() {
            self.key_value_overrides = Some(keyvalue_to_toml(kvs)?);
        }
        Ok(self)
    }

    /// Enable or disable environment variable support
    pub fn with_env_vars(mut self, enable_env_vars: bool) -> Self {
        self.enable_env_vars = enable_env_vars;
        self
    }

    /// Assemble the `Figment` without any adapters (`FileAdapter` / `RemoteFileAdapter`).
    /// Useful for inspecting the raw merged configuration before path/remote resolution.
    pub fn build_raw_figment(self) -> Figment {
        let mut figment = Figment::new();

        if let Some(base_config) = self.base_config {
            figment = figment.merge(Toml::string(&base_config));
        }

        if let Some(content) = self.config_content {
            figment = figment.merge(Toml::string(&content));
        } else if let Some(config_file) = self.config_file {
            figment = figment.merge(Toml::file(config_file.as_path()));
        }

        if self.enable_env_vars {
            figment = figment.merge(Env::prefixed("CDVIZ_COLLECTOR__").split("__"));
        }

        if let Some(cli_overrides) = self.cli_overrides {
            figment = figment.merge(Toml::string(&cli_overrides));
        }

        if let Some(kv_overrides) = self.key_value_overrides {
            figment = figment.merge(Toml::string(&kv_overrides));
        }

        figment
    }

    /// Assemble the `Figment` with adapters applied once to the fully merged config.
    ///
    /// Uses a two-phase approach: first merge all plain sources, then apply
    /// `FileAdapter` and `RemoteFileAdapter` once on the combined result.
    /// This ensures remote config definitions from any layer are visible when
    /// resolving `_rfile` references from any other layer.
    pub fn build_figment(self) -> Result<Figment> {
        // Phase 1: merge all plain sources without adapters
        let raw = self.build_raw_figment();
        let raw_value: toml::Value = raw.extract().into_diagnostic()?;
        let raw_toml = toml::to_string(&raw_value).into_diagnostic()?;

        // Phase 2: apply adapters once on the fully merged config
        let provider = FileAdapter::wrap(Toml::string(&raw_toml));
        #[cfg(feature = "config_remote")]
        let provider = RemoteFileAdapter::wrap(provider);
        Ok(Figment::new().merge(provider))
    }

    /// Build the final Config
    pub fn build(self) -> Result<Config> {
        // Check if config file exists if specified
        if let Some(ref config_file) = self.config_file
            && !config_file.exists()
        {
            return Err(Error::ConfigNotFound { path: config_file.to_string_lossy().to_string() })
                .into_diagnostic();
        }

        let figment = self.build_figment()?;

        // Extract final configuration
        // TODO improve error reporting with :
        // - use [deserr - crates.io: Rust Package Registry](https://crates.io/crates/deserr)
        //   currently not a valid choise because
        //   - it doesn't support unttaged enum (used by signature)
        //   - deserialisation of externat type like `SecretString`
        // - use [eserde - crates.io: Rust Package Registry](https://crates.io/crates/eserde)
        // So we lost details on error like file name, path,...
        // let value: serde_json::Value = figment.extract().into_diagnostic()?;
        // let mut config: Config = serde_json::from_value(value).into_diagnostic()?;
        // let mut config = Config::deserialize_from_value(value).into_diagnostic()?;
        let mut config: Config = figment.extract().into_diagnostic()?;

        // resolve transformers references
        config.sources.iter_mut().try_for_each(|(_name, source_config)| {
            source_config.resolve_transformers(&config.transformers)
        })?;

        // Resolve pipeline-level global transformer chain
        let global_transformers = transformers::resolve_transformer_refs(
            &config.pipeline.transformer_refs,
            &config.transformers,
        )?;
        config.pipeline.transformers = global_transformers;

        // Append global transformers to every source's chain
        if !config.pipeline.transformers.is_empty() {
            let global = config.pipeline.transformers.clone();
            config.sources.iter_mut().for_each(|(_name, source_config)| {
                source_config.append_transformers(&global);
            });
        }

        Ok(config)
    }
}

impl Config {
    /// Create Config from file (compact wrapper around `ConfigBuilder`)
    #[inline]
    pub fn from_file(config_file: Option<PathBuf>) -> Result<Self> {
        ConfigBuilder::new().with_config_file(config_file).build()
    }

    /// Create Config using the builder pattern
    pub fn builder() -> ConfigBuilder {
        ConfigBuilder::new()
    }
}

/// The result of resolving a [`ConfigSource`]: either a local file path or pre-fetched TOML.
///
/// Exactly one variant is populated — the enum enforces the XOR invariant that
/// `ConfigBuilder::with_config_file` and `with_config_text` cannot both be used.
#[derive(Debug, Clone)]
pub(crate) enum ResolvedConfigSource {
    /// No user config was provided.
    None,
    /// A local file path to read at build time.
    File(PathBuf),
    /// Pre-fetched TOML text (e.g. from an HTTP/HTTPS URL).
    Content(String),
}

/// Parse `"Name: value"` header strings into key/value pairs.
pub(crate) fn parse_config_headers(raw: &[String]) -> Result<Vec<(String, String)>> {
    raw.iter()
        .map(|h| {
            h.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).ok_or_else(
                || miette::miette!("invalid --config-header '{h}': expected 'Name: value' format"),
            )
        })
        .collect()
}

/// Fetch a TOML configuration string from an HTTP/HTTPS URL.
#[cfg(feature = "config_http")]
pub(crate) async fn fetch_url_config(
    url: &url::Url,
    headers: &[(String, String)],
) -> Result<String> {
    use reqwest::header::{HeaderName, HeaderValue};
    let mut req = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .into_diagnostic()?
        .get(url.as_str());
    for (name, value) in headers {
        let header_name = HeaderName::from_bytes(name.as_bytes()).into_diagnostic()?;
        let header_value = HeaderValue::from_str(value).into_diagnostic()?;
        req = req.header(header_name, header_value);
    }
    let response = req.send().await.into_diagnostic()?;
    let status = response.status();
    if !status.is_success() {
        miette::bail!("Failed to fetch config from {url}: HTTP {status}");
    }
    response.text().await.into_diagnostic()
}

/// Resolve a [`ConfigSource`] into a [`ResolvedConfigSource`], fetching content if needed.
///
/// When `--config-header` values are provided for a file source, a warning is emitted
/// since headers have no effect on local file loading.
pub(crate) async fn resolve_config_source(
    source: Option<ConfigSource>,
    raw_headers: &[String],
) -> Result<ResolvedConfigSource> {
    match source {
        None => Ok(ResolvedConfigSource::None),
        Some(ConfigSource::File(path)) => {
            if !raw_headers.is_empty() {
                tracing::warn!(
                    "--config-header flags are ignored when --config is a local file path"
                );
            }
            Ok(ResolvedConfigSource::File(path))
        }
        #[cfg(feature = "config_http")]
        Some(ConfigSource::Url(url)) => {
            let headers = parse_config_headers(raw_headers)?;
            let content = fetch_url_config(&url, &headers).await?;
            Ok(ResolvedConfigSource::Content(content))
        }
    }
}

/// Convert `key=value` pairs into TOML dotted-key lines.
fn keyvalue_to_toml(kvs: &[String]) -> Result<String> {
    use std::fmt::Write as _;
    let mut out = String::new();
    for kv in kvs {
        let (key, raw) = kv
            .split_once('=')
            .ok_or_else(|| miette::miette!("invalid --set '{kv}': expected 'key=value' format"))?;
        let toml_val = infer_toml_value(raw);
        writeln!(&mut out, "{} = {}", key.trim(), toml_val).into_diagnostic()?;
    }
    Ok(out)
}

fn infer_toml_value(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("true") {
        "true".to_string()
    } else if trimmed.eq_ignore_ascii_case("false") {
        "false".to_string()
    } else if trimmed.parse::<i64>().is_ok() || trimmed.parse::<f64>().is_ok() {
        trimmed.to_string()
    } else {
        format!("\"{}\"", escape_toml_string(raw))
    }
}

fn escape_toml_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

#[cfg(test)]
#[allow(clippy::result_large_err)]
mod tests {
    use super::*;
    use figment::Jail;
    use rstest::*;

    #[test]
    fn with_keyvalue_bool_detection() {
        let config = ConfigBuilder::new()
            .with_env_vars(false)
            .with_keyvalue(&["sinks.debug.enabled=true".to_string()])
            .unwrap()
            .build()
            .unwrap();
        assert!(config.sinks.get("debug").unwrap().is_enabled());
    }

    #[test]
    fn with_keyvalue_dashed_key() {
        // Keys with dashes are the primary motivation: env vars can't express them.
        // We only verify parsing succeeds (config won't have a "my-source" unless defined).
        let toml = keyvalue_to_toml(&["sources.my-source.enabled=false".to_string()]).unwrap();
        assert!(toml.contains("sources.my-source.enabled = false"));
    }

    #[test]
    fn keyvalue_to_toml_int_and_float() {
        let toml = keyvalue_to_toml(&[
            "pipeline.max_retries=5".to_string(),
            "http.timeout=2.5".to_string(),
        ])
        .unwrap();
        assert!(toml.contains("pipeline.max_retries = 5"));
        assert!(toml.contains("http.timeout = 2.5"));
    }

    #[test]
    fn keyvalue_to_toml_string_with_equals_in_value() {
        let toml = keyvalue_to_toml(&["sources.foo.url=http://x.com?a=1&b=2".to_string()]).unwrap();
        assert!(toml.contains(r#"sources.foo.url = "http://x.com?a=1&b=2""#));
    }

    #[test]
    fn keyvalue_to_toml_empty_list() {
        assert!(keyvalue_to_toml(&[]).unwrap().is_empty());
    }

    #[test]
    fn keyvalue_to_toml_missing_equals_is_error() {
        assert!(keyvalue_to_toml(&["no-equals-sign".to_string()]).is_err());
    }

    #[rstest]
    fn read_base_config_only() {
        Jail::expect_with(|_jail| {
            let config: Config = Config::from_file(None).unwrap();
            assert!(!config.sinks.get("debug").unwrap().is_enabled());
            Ok(())
        });
    }

    #[rstest]
    fn global_transformer_refs_appended_to_sources() {
        Jail::expect_with(|_jail| {
            let config: Config = ConfigBuilder::new()
                .with_env_vars(false)
                .with_base_config(
                    r#"
[pipeline]
transformer_refs = ["passthrough"]

[transformers.passthrough]
type = "passthrough"

[sources.dummy]
enabled = true
"#,
                )
                .build()
                .unwrap();

            let source = config.sources.get("dummy").unwrap();
            // The global passthrough transformer must be present in the source's list
            assert!(!source.chain.transformers.is_empty());
            // The pipeline-level resolved list must also contain it
            assert!(!config.pipeline.transformers.is_empty());
            Ok(())
        });
    }

    #[rstest]
    fn read_base_config_with_env_override() {
        Jail::expect_with(|jail| {
            jail.set_env("CDVIZ_COLLECTOR__SINKS__DEBUG__ENABLED", "true");
            let config: Config = Config::from_file(None).unwrap();
            assert!(config.sinks.get("debug").unwrap().is_enabled());
            Ok(())
        });
    }

    #[rstest]
    fn read_config_from_examples(#[files("./examples/**/cdviz-collector.toml")] path: PathBuf) {
        Jail::expect_with(|_jail| {
            assert!(path.exists());
            //HACK change the current dir to the parent of the config file, not thread safe/ test isolation
            //jail.change_dir(path.parent().unwrap()).unwrap();
            std::env::set_current_dir(path.parent().unwrap()).unwrap();
            let _config: Config = Config::from_file(Some(path)).unwrap();
            Ok(())
        });
    }

    #[rstest]
    fn read_config_from_tests(#[files("./tests/assets/config_samples/*.toml")] path: PathBuf) {
        Jail::expect_with(|_jail| {
            assert!(path.exists());
            //HACK change the current dir to the parent of the config file, not thread safe/ test isolation
            //jail.change_dir(path.parent().unwrap()).unwrap();
            std::env::set_current_dir(path.parent().unwrap()).unwrap();
            let _config: Config = Config::from_file(Some(path)).unwrap();
            Ok(())
        });
    }

    #[cfg(feature = "config_http")]
    #[tokio::test]
    async fn fetch_url_config_returns_toml_body() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        let toml_content = "[sinks.debug]\nenabled = true\n";

        Mock::given(method("GET"))
            .and(path("/config.toml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(toml_content))
            .expect(1)
            .mount(&mock_server)
            .await;

        let url: url::Url = format!("{}/config.toml", mock_server.uri()).parse().unwrap();
        let content = fetch_url_config(&url, &[]).await.unwrap();
        assert_eq!(content, toml_content);
    }

    #[cfg(feature = "config_http")]
    #[tokio::test]
    async fn fetch_url_config_sends_auth_header() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        let toml_content = "[sinks.debug]\nenabled = true\n";

        Mock::given(method("GET"))
            .and(path("/config.toml"))
            .and(header("Authorization", "Bearer mytoken"))
            .respond_with(ResponseTemplate::new(200).set_body_string(toml_content))
            .expect(1)
            .mount(&mock_server)
            .await;

        let url: url::Url = format!("{}/config.toml", mock_server.uri()).parse().unwrap();
        let headers = vec![("Authorization".to_string(), "Bearer mytoken".to_string())];
        let content = fetch_url_config(&url, &headers).await.unwrap();
        assert_eq!(content, toml_content);
    }

    #[cfg(feature = "config_http")]
    #[tokio::test]
    async fn fetch_url_config_errors_on_non_200() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/config.toml"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let url: url::Url = format!("{}/config.toml", mock_server.uri()).parse().unwrap();
        assert!(fetch_url_config(&url, &[]).await.is_err());
    }

    #[cfg(feature = "config_http")]
    #[tokio::test]
    async fn resolve_config_source_url_loads_and_parses_config() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        let toml_content = "[sinks.debug]\nenabled = true\n";

        Mock::given(method("GET"))
            .and(path("/config.toml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(toml_content))
            .mount(&mock_server)
            .await;

        let url_str = format!("{}/config.toml", mock_server.uri());
        let source: ConfigSource = url_str.parse().unwrap();
        let resolved = resolve_config_source(Some(source), &[]).await.unwrap();
        assert!(matches!(resolved, ResolvedConfigSource::Content(_)));
        // Should be parseable and override the debug sink
        let config = ConfigBuilder::new()
            .with_env_vars(false)
            .with_resolved_source(resolved)
            .build()
            .unwrap();
        assert!(config.sinks.get("debug").unwrap().is_enabled());
    }
}
