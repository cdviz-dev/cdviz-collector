//! Remote file adapter for Figment configuration provider using `OpenDAL`.

use figment::{
    Figment, Profile, Provider,
    error::{Error as FigmentError, Kind as ErrorKind},
    value::{Dict, Map, Value},
};
use opendal::{Operator, Scheme};
use serde::Deserialize;
use std::{collections::HashMap, str::FromStr};

/// Error type for remote file adapter operations
#[derive(Debug, derive_more::Display, derive_more::Error)]
pub enum RemoteFileError {
    /// Invalid URI format
    #[display("Invalid remote URI format '{}', expected '<remote_name>:///<path>'", _0)]
    InvalidUri(#[error(ignore)] String),
    /// Remote configuration not found
    #[display("Remote configuration '{}' not found", _0)]
    RemoteNotFound(#[error(ignore)] String),
    /// Invalid `OpenDAL` scheme
    #[display("Invalid OpenDAL scheme '{}'", _0)]
    InvalidScheme(#[error(ignore)] String),
    /// `OpenDAL` operation failed
    #[display("OpenDAL operation failed: {}", _0)]
    OpenDalError(opendal::Error),
    /// File content is not valid UTF-8
    #[display("Remote file '{}' contains invalid UTF-8", _0)]
    InvalidUtf8(#[error(ignore)] String),
    /// IO or network error
    #[display("IO error: {}", _0)]
    IoError(#[error(ignore)] String),
}

impl From<opendal::Error> for RemoteFileError {
    fn from(err: opendal::Error) -> Self {
        RemoteFileError::OpenDalError(err)
    }
}

impl From<std::string::FromUtf8Error> for RemoteFileError {
    fn from(err: std::string::FromUtf8Error) -> Self {
        RemoteFileError::InvalidUtf8(format!("UTF-8 conversion error: {err}"))
    }
}

/// Convert `RemoteFileError` to Figment error
impl From<RemoteFileError> for FigmentError {
    fn from(err: RemoteFileError) -> Self {
        FigmentError::from(ErrorKind::Message(err.to_string()))
    }
}

type Result<T> = std::result::Result<T, RemoteFileError>;

/// A Figment adapter that reads remote files using `OpenDAL` for configuration keys ending with "_rfile".
///
/// This adapter scans configuration values for keys ending with "_rfile" suffix, extracts the
/// remote name and path from URI format `<remote_name>:///<path>`, looks up the corresponding
/// `[remote.<remote_name>]` configuration section, creates an `OpenDAL` operator, reads the file
/// content, and replaces the `<key>_rfile` key with `<key>` containing the file content as a string.
pub struct RemoteFileAdapter<T> {
    provider: T,
    suffix: String,
}

impl<T> RemoteFileAdapter<T> {
    /// Create a new `RemoteFileAdapter` wrapping the given provider.
    pub fn wrap(provider: T) -> Self {
        Self { provider, suffix: "_rfile".to_string() }
    }

    /// Change the suffix used to identify remote file references.
    /// Default is "_rfile".
    #[allow(dead_code)]
    pub fn with_suffix<S: Into<String>>(mut self, suffix: S) -> Self {
        self.suffix = suffix.into();
        self
    }
}

/// Configuration for a remote provider (maps to `OpenDAL` service configuration)
#[derive(Debug, Deserialize)]
struct RemoteConfig {
    #[serde(rename = "type")]
    service_type: String,
    #[serde(flatten)]
    parameters: HashMap<String, String>,
}

/// URI parsing result
#[derive(Debug)]
struct RemoteUri {
    remote_name: String,
    path: String,
}

impl RemoteUri {
    /// Parse a URI in the format `<remote_name>:///<path>`
    fn parse(uri: &str) -> Result<Self> {
        let parts: Vec<&str> = uri.splitn(2, ":///").collect();
        if parts.len() != 2 {
            return Err(RemoteFileError::InvalidUri(uri.to_string()));
        }

        Ok(RemoteUri { remote_name: parts[0].to_string(), path: parts[1].to_string() })
    }
}

impl<T: Provider> RemoteFileAdapter<T> {
    /// Process a configuration map and resolve remote file references
    #[allow(clippy::result_large_err)]
    fn process_map(&self, mut map: Map<Profile, Dict>) -> figment::Result<Map<Profile, Dict>> {
        // First, extract the base configuration to get remote definitions
        let figment = Figment::from(&self.provider);
        let base_config: HashMap<String, Value> = figment.extract()?;

        // Extract remote configurations
        let remote_configs = Self::extract_remote_configs(&base_config)?;

        // Process each profile
        for dict in map.values_mut() {
            self.process_dict(dict, &remote_configs)?;
        }

        Ok(map)
    }

    /// Extract remote configurations from the base config
    #[allow(clippy::result_large_err)]
    fn extract_remote_configs(
        config: &HashMap<String, Value>,
    ) -> figment::Result<HashMap<String, RemoteConfig>> {
        let mut remote_configs = HashMap::new();

        if let Some(Value::Dict(_, remote_dict)) = config.get("remote") {
            for (remote_name, config_value) in remote_dict {
                let remote_config: RemoteConfig = config_value.deserialize()?;
                remote_configs.insert(remote_name.clone(), remote_config);
            }
        }

        Ok(remote_configs)
    }

    /// Process a dictionary and resolve remote file references
    #[allow(clippy::result_large_err)]
    fn process_dict(
        &self,
        dict: &mut Dict,
        remote_configs: &HashMap<String, RemoteConfig>,
    ) -> figment::Result<()> {
        let mut keys_to_process = Vec::new();

        // Find keys ending with the suffix
        for (key, _) in dict.iter() {
            if key.ends_with(&self.suffix) {
                keys_to_process.push(key.clone());
            }
        }

        // Process each remote file reference
        for key in keys_to_process {
            if let Some(value) = dict.remove(&key) {
                let Some(uri_str) = value.into_string() else {
                    return Err(FigmentError::from(ErrorKind::Message(format!(
                        "Expected string URI for key '{key}', found non-string value"
                    ))));
                };

                // Parse the URI
                let uri = RemoteUri::parse(&uri_str).map_err(FigmentError::from)?;

                // Get the remote configuration
                let remote_config = remote_configs.get(&uri.remote_name).ok_or_else(|| {
                    FigmentError::from(RemoteFileError::RemoteNotFound(uri.remote_name.clone()))
                })?;

                // Read the remote file
                let file_content =
                    Self::read_remote_file(remote_config, &uri.path).map_err(|e| {
                        FigmentError::from(ErrorKind::Message(format!(
                            "Failed to read remote file '{}' from '{}': {e}",
                            uri.path, uri.remote_name
                        )))
                    })?;

                // Create the new key (remove suffix)
                // We know the key ends with suffix because we found it in the loop above
                #[allow(clippy::expect_used)]
                let new_key =
                    key.strip_suffix(&self.suffix).expect("key should end with suffix").to_string();
                dict.insert(new_key, Value::from(file_content));
            }
        }

        // Recursively process nested dictionaries
        for (_, value) in dict.iter_mut() {
            if let Value::Dict(_, nested_dict) = value {
                self.process_dict(nested_dict, remote_configs)?;
            }
        }

        Ok(())
    }

    /// Read a file from a remote using `OpenDAL`
    fn read_remote_file(remote_config: &RemoteConfig, path: &str) -> Result<String> {
        // Parse the scheme
        let scheme = Scheme::from_str(&remote_config.service_type).map_err(|e| {
            RemoteFileError::InvalidScheme(format!("{}: {e}", remote_config.service_type))
        })?;

        // Create `OpenDAL` operator
        let operator = Operator::via_iter(scheme, remote_config.parameters.clone())?;

        // Read the file content
        let rt = tokio::runtime::Handle::current();
        let content_bytes = rt.block_on(async { operator.read(path).await })?;

        // Convert to string
        String::from_utf8(content_bytes.to_vec())
            .map_err(|_| RemoteFileError::InvalidUtf8(path.to_string()))
    }
}

impl<T: Provider> Provider for RemoteFileAdapter<T> {
    fn metadata(&self) -> figment::Metadata {
        figment::Metadata::named("Remote File Adapter")
    }

    fn data(&self) -> figment::Result<Map<Profile, Dict>> {
        let map = self.provider.data()?;
        self.process_map(map)
    }

    fn profile(&self) -> Option<Profile> {
        self.provider.profile()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::providers::Serialized;
    use std::collections::HashMap;

    #[test]
    fn test_remote_uri_parsing() {
        let uri = RemoteUri::parse("my_remote:///path/to/file.txt").unwrap();
        assert_eq!(uri.remote_name, "my_remote");
        assert_eq!(uri.path, "path/to/file.txt");

        let uri = RemoteUri::parse("s3_bucket:///templates/config.json").unwrap();
        assert_eq!(uri.remote_name, "s3_bucket");
        assert_eq!(uri.path, "templates/config.json");

        // Test invalid format
        assert!(RemoteUri::parse("invalid-uri").is_err());
        assert!(RemoteUri::parse("missing://path").is_err());
    }

    #[test]
    fn test_adapter_creation() {
        let provider = Serialized::defaults(HashMap::<String, String>::new());
        let adapter = RemoteFileAdapter::wrap(provider);
        assert_eq!(adapter.suffix, "_rfile");

        let adapter = adapter.with_suffix("_remote");
        assert_eq!(adapter.suffix, "_remote");
    }

    #[test]
    fn test_error_handling() {
        // Test invalid URI
        let result = RemoteUri::parse("invalid-uri");
        assert!(result.is_err());
        match result.unwrap_err() {
            RemoteFileError::InvalidUri(uri) => assert_eq!(uri, "invalid-uri"),
            _ => panic!("Expected InvalidUri error"),
        }

        // Test valid URI
        let result = RemoteUri::parse("remote:///path/to/file");
        assert!(result.is_ok());
        let uri = result.unwrap();
        assert_eq!(uri.remote_name, "remote");
        assert_eq!(uri.path, "path/to/file");

        // Test error conversion to Figment error
        let remote_error = RemoteFileError::RemoteNotFound("test_remote".to_string());
        let figment_error: FigmentError = remote_error.into();
        assert!(figment_error.to_string().contains("Remote configuration 'test_remote' not found"));
    }
}
