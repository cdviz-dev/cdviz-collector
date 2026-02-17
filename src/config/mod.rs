#[cfg(feature = "config_remote")]
mod remote_file_adapter;
mod toml_provider;

use crate::{
    errors::{Error, IntoDiagnostic, Result},
    http, pipeline, sinks, sources,
};
use figment::{
    Figment,
    providers::{Env, Format},
};
use figment_file_provider_adapter::FileAdapter;
#[cfg(feature = "config_remote")]
use remote_file_adapter::RemoteFileAdapter;
use serde::Deserialize;
use std::{collections::HashMap, path::PathBuf};
pub use toml_provider::Toml;

#[derive(Clone, Debug, Deserialize, Default)]
pub(crate) struct Config {
    #[serde(default)]
    pub(crate) sources: HashMap<String, sources::Config>,
    #[serde(default)]
    pub(crate) sinks: HashMap<String, sinks::Config>,
    // extractors: HashMap<String, sources::extractors::Config>,
    #[serde(default)]
    pub(crate) transformers: HashMap<String, sources::transformers::Config>,
    #[serde(default)]
    pub(crate) http: http::Config,
    #[serde(default)]
    pub(crate) pipeline: pipeline::PipelineConfig,
}

/// Builder for Config with flexible configuration loading options
pub(crate) struct ConfigBuilder {
    base_config: Option<String>,
    config_file: Option<PathBuf>,
    cli_overrides: Option<String>,
    enable_env_vars: bool,
}

impl ConfigBuilder {
    /// Create a new `ConfigBuilder` with default base configuration
    pub fn new() -> Self {
        Self {
            base_config: Some(include_str!("../assets/cdviz-collector.base.toml").to_string()),
            config_file: None,
            cli_overrides: None,
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

    /// Add CLI overrides as TOML string
    pub fn with_cli_overrides(mut self, cli_overrides: Option<String>) -> Self {
        self.cli_overrides = cli_overrides;
        self
    }

    /// Enable or disable environment variable support
    pub fn with_env_vars(mut self, enable_env_vars: bool) -> Self {
        self.enable_env_vars = enable_env_vars;
        self
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

        let mut figment = Figment::new();

        // Add base configuration if provided
        if let Some(base_config) = self.base_config {
            let provider = FileAdapter::wrap(Toml::string(&base_config));
            #[cfg(feature = "config_remote")]
            let provider = RemoteFileAdapter::wrap(provider);
            figment = figment.merge(provider);
        }

        // Add user config file if provided
        if let Some(config_file) = self.config_file {
            let provider = FileAdapter::wrap(Toml::file(config_file.as_path()));
            #[cfg(feature = "config_remote")]
            let provider = RemoteFileAdapter::wrap(provider);
            figment = figment.merge(provider);
        }

        // Add environment variables if enabled
        if self.enable_env_vars {
            let provider = FileAdapter::wrap(Env::prefixed("CDVIZ_COLLECTOR__").split("__"));
            #[cfg(feature = "config_remote")]
            let provider = RemoteFileAdapter::wrap(provider);
            figment = figment.merge(provider);
        }

        // Add CLI overrides if provided
        if let Some(cli_overrides) = self.cli_overrides {
            let provider = FileAdapter::wrap(Toml::string(&cli_overrides));
            #[cfg(feature = "config_remote")]
            let provider = RemoteFileAdapter::wrap(provider);
            figment = figment.merge(provider);
        }

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

        // inject context.source into each source's metadata
        let root_url = &config.http.root_url;
        config.sources.iter_mut().for_each(|(name, source_config)| {
            source_config.inject_context_source(name, root_url);
        });

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

#[cfg(test)]
mod tests {
    use super::*;
    use figment::Jail;
    use rstest::*;

    #[rstest]
    fn read_base_config_only() {
        Jail::expect_with(|_jail| {
            let config: Config = Config::from_file(None).unwrap();
            assert!(!config.sinks.get("debug").unwrap().is_enabled());
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
}
