use crate::{
    errors::{Error, IntoDiagnostic, Result},
    http, sinks, sources,
};
use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use serde::Deserialize;
use std::{collections::HashMap, path::PathBuf};

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
}

impl Config {
    pub fn from_file(config_file: Option<PathBuf>) -> Result<Self> {
        if let Some(ref config_file) = config_file {
            if !config_file.exists() {
                return Err(Error::ConfigNotFound {
                    path: config_file.to_string_lossy().to_string(),
                })
                .into_diagnostic();
            }
        }
        let config_file_base = include_str!("assets/cdviz-collector.base.toml");

        let mut figment = Figment::new().merge(Toml::string(config_file_base));
        if let Some(config_file) = config_file {
            figment = figment.merge(Toml::file(config_file.as_path()));
        }
        let mut config: Config = figment
            .merge(Env::prefixed("CDVIZ_COLLECTOR__").split("__"))
            .extract()
            .into_diagnostic()?;

        // resolve transformers references
        config.sources.iter_mut().try_for_each(|(_name, source_config)| {
            source_config.resolve_transformers(&config.transformers)
        })?;

        Ok(config)
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
    fn read_samples_config(#[files("./**/cdviz-collector.toml")] path: PathBuf) {
        Jail::expect_with(|_jail| {
            assert!(path.exists());
            let _config: Config = Config::from_file(Some(path)).unwrap();
            Ok(())
        });
    }
}
