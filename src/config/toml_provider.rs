/// reimplementation of figment Toml provider to be able to tune & update it
/// Figment seems to have low activity an some pending PR (without response)
use figment::providers::Format;
use serde::de::DeserializeOwned;

//figment::providers::impl_format!(Toml "TOML"/"toml": toml_edit::de::from_str, toml_edit::de::Error);
pub struct Toml;

impl Format for Toml {
    type Error = toml::de::Error;

    const NAME: &'static str = "TOML";

    fn from_str<T: DeserializeOwned>(s: &str) -> Result<T, toml::de::Error> {
        toml::de::from_str(s)
    }
}
