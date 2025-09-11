use axum::http::{HeaderMap, HeaderName, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;

use super::signature::{self, Encoding, SignatureOn};

/// This module uses `axum::http` types which are the standard `http` crate types.
/// These are compatible with both Axum and reqwest, avoiding unnecessary conversions.
/// Both libraries use the same underlying `HeaderMap`, `HeaderName`, and `HeaderValue` types.
/// Configuration for generating outgoing request headers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingHeaderConfig {
    /// The header name (consistent with `HeaderRuleConfig`)
    pub header: String,
    /// How to generate the header value (consistent with `HeaderRuleConfig` structure)
    pub rule: HeaderSource,
}

/// Different ways to generate header values
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HeaderSource {
    /// Static header value
    #[serde(rename = "static")]
    Static { value: String },

    /// Secret header value (e.g., from environment)
    #[serde(rename = "secret")]
    Secret {
        #[serde(skip_serializing)]
        value: SecretString,
    },

    /// Generate HMAC signature for the request
    #[serde(rename = "signature")]
    Signature {
        #[serde(skip_serializing)]
        token: SecretString,
        #[serde(default)]
        token_encoding: Option<Encoding>,
        #[serde(default)]
        signature_prefix: Option<String>,
        #[serde(default)]
        signature_on: SignatureOn,
        #[serde(default)]
        signature_encoding: Encoding,
    },
}

/// Errors that can occur during header generation
#[derive(Debug, Clone, derive_more::Display, derive_more::Error)]
pub enum HeaderError {
    #[display("Invalid header name: {name}")]
    InvalidHeaderName { name: String },

    #[display("Invalid header value for header '{name}': {error}")]
    InvalidHeaderValue { name: String, error: String },

    #[display("Signature generation failed for header '{name}': {error}")]
    SignatureGeneration { name: String, error: String },
}

/// Generate headers for an outgoing HTTP request
pub fn generate_headers(
    configs: &[OutgoingHeaderConfig],
    body: Option<&[u8]>,
) -> Result<HeaderMap, HeaderError> {
    generate_headers_with_existing(configs, body, None)
}

/// Generate headers for an outgoing HTTP request, potentially using existing headers for signature computation
pub fn generate_headers_with_existing(
    configs: &[OutgoingHeaderConfig],
    body: Option<&[u8]>,
    existing_headers: Option<&HeaderMap>,
) -> Result<HeaderMap, HeaderError> {
    let mut headers = existing_headers.cloned().unwrap_or_default();

    for config in configs {
        let header_name = HeaderName::from_str(&config.header)
            .map_err(|_| HeaderError::InvalidHeaderName { name: config.header.clone() })?;

        let header_value = generate_header_value_with_context(config, body, &headers)?;
        headers.insert(header_name, header_value);
    }

    Ok(headers)
}

/// Generate a single header value based on configuration
#[allow(dead_code)]
fn generate_header_value(
    config: &OutgoingHeaderConfig,
    body: Option<&[u8]>,
) -> Result<HeaderValue, HeaderError> {
    generate_header_value_with_context(config, body, &HeaderMap::new())
}

/// Generate a single header value based on configuration with access to existing headers
fn generate_header_value_with_context(
    config: &OutgoingHeaderConfig,
    body: Option<&[u8]>,
    existing_headers: &HeaderMap,
) -> Result<HeaderValue, HeaderError> {
    let value_str = match &config.rule {
        HeaderSource::Static { value } => value.clone(),
        HeaderSource::Secret { value } => value.expose_secret().to_string(),
        HeaderSource::Signature {
            token,
            token_encoding,
            signature_prefix,
            signature_on,
            signature_encoding,
        } => {
            // Generate signature using existing signature module
            let signature_config = signature::SignatureConfig {
                header: config.header.clone(),
                token: token.clone(),
                token_encoding: token_encoding.clone(),
                signature_prefix: signature_prefix.clone(),
                signature_on: signature_on.clone(),
                signature_encoding: signature_encoding.clone(),
            };

            // Since we're now using http::HeaderMap directly, no conversion needed
            match signature::build_signature(
                &signature_config,
                existing_headers,
                body.unwrap_or(&[]),
            ) {
                Ok(signature) => signature,
                Err(e) => {
                    return Err(HeaderError::SignatureGeneration {
                        name: config.header.clone(),
                        error: e.to_string(),
                    });
                }
            }
        }
    };

    HeaderValue::from_str(&value_str).map_err(|e| HeaderError::InvalidHeaderValue {
        name: config.header.clone(),
        error: e.to_string(),
    })
}

/// Map-based configuration for outgoing headers
/// Maps header names directly to their source configurations
pub type OutgoingHeaderMap = HashMap<String, HeaderSource>;

/// Convert map-based configuration to the internal Vec<OutgoingHeaderConfig> format
pub fn outgoing_header_map_to_configs(map: &OutgoingHeaderMap) -> Vec<OutgoingHeaderConfig> {
    map.iter()
        .map(|(header, source)| OutgoingHeaderConfig {
            header: header.clone(),
            rule: source.clone(),
        })
        .collect()
}

/// Simplified configuration format that maps to `OutgoingHeaderConfig`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimpleHeaderConfig {
    pub header: String,
    #[serde(flatten)]
    pub config: SimpleHeaderValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SimpleHeaderValue {
    /// Just a value field - assumed to be static
    Simple { value: String },
    /// More complex configuration with explicit type
    Complex(HeaderSource),
}

impl From<SimpleHeaderConfig> for OutgoingHeaderConfig {
    fn from(simple: SimpleHeaderConfig) -> Self {
        let source = match simple.config {
            SimpleHeaderValue::Simple { value } => HeaderSource::Static { value },
            SimpleHeaderValue::Complex(source) => source,
        };

        OutgoingHeaderConfig { header: simple.header, rule: source }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[derive(serde::Deserialize)]
    struct HeaderConfigs {
        headers: Vec<OutgoingHeaderConfig>,
    }

    #[test]
    fn test_static_header_generation() {
        let configs = vec![OutgoingHeaderConfig {
            header: "Authorization".to_string(),
            rule: HeaderSource::Static { value: "Bearer token123".to_string() },
        }];

        let headers = generate_headers(&configs, None).unwrap();
        assert_eq!(headers.get("Authorization").unwrap(), "Bearer token123");
    }

    #[test]
    fn test_secret_header_generation() {
        let configs = vec![OutgoingHeaderConfig {
            header: "X-API-Key".to_string(),
            rule: HeaderSource::Secret { value: "secret123".into() },
        }];

        let headers = generate_headers(&configs, None).unwrap();
        assert_eq!(headers.get("X-API-Key").unwrap(), "secret123");
    }

    #[test]
    fn test_invalid_header_name() {
        let configs = vec![OutgoingHeaderConfig {
            header: "Invalid Header Name".to_string(), // Spaces not allowed
            rule: HeaderSource::Static { value: "value".to_string() },
        }];

        let result = generate_headers(&configs, None);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), HeaderError::InvalidHeaderName { .. }));
    }

    #[test]
    fn test_simple_header_config_conversion() {
        let simple = SimpleHeaderConfig {
            header: "Authorization".to_string(),
            config: SimpleHeaderValue::Simple { value: "Bearer token".to_string() },
        };

        let outgoing: OutgoingHeaderConfig = simple.into();
        assert_eq!(outgoing.header, "Authorization");
        match outgoing.rule {
            HeaderSource::Static { value } => assert_eq!(value, "Bearer token"),
            _ => panic!("Expected static source"),
        }
    }

    #[test]
    fn test_toml_parsing_outgoing_header_config_static() {
        let toml_str = indoc! {r#"
            header = "Authorization"

            [rule]
            type = "static"
            value = "Bearer token123"
            "#};

        let config: OutgoingHeaderConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.header, "Authorization");
        match config.rule {
            HeaderSource::Static { value } => {
                assert_eq!(value, "Bearer token123");
            }
            _ => panic!("Expected static header source"),
        }
    }

    #[test]
    fn test_toml_parsing_outgoing_header_config_secret() {
        use secrecy::ExposeSecret;

        let toml_str = indoc! {r#"
            header = "X-API-Key"

            [rule]
            type = "secret"
            value = "secret-from-env"
            "#};

        let config: OutgoingHeaderConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.header, "X-API-Key");
        match config.rule {
            HeaderSource::Secret { value } => {
                assert_eq!(value.expose_secret(), "secret-from-env");
            }
            _ => panic!("Expected secret header source"),
        }
    }

    #[test]
    fn test_toml_parsing_outgoing_header_config_signature() {
        use secrecy::ExposeSecret;

        let toml_str = indoc! {r#"
            header = "X-Hub-Signature-256"

            [rule]
            type = "signature"
            token = "webhook-secret"
            token_encoding = "hex"
            signature_prefix = "sha256="
            signature_on = "body"
            signature_encoding = "hex"
            "#};

        let config: OutgoingHeaderConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.header, "X-Hub-Signature-256");
        match config.rule {
            HeaderSource::Signature {
                token,
                token_encoding,
                signature_prefix,
                signature_on,
                signature_encoding,
            } => {
                assert_eq!(token.expose_secret(), "webhook-secret");
                assert_eq!(token_encoding, Some(Encoding::Hex));
                assert_eq!(signature_prefix, Some("sha256=".to_string()));
                assert_eq!(signature_on, SignatureOn::Body);
                assert_eq!(signature_encoding, Encoding::Hex);
            }
            _ => panic!("Expected signature header source"),
        }
    }

    #[test]
    fn test_toml_parsing_multiple_outgoing_headers() {
        use secrecy::ExposeSecret;

        let toml_str = indoc! {r#"
            [[headers]]
            header = "Authorization"

            [headers.rule]
            type = "static"
            value = "Bearer static-token"

            [[headers]]
            header = "X-API-Key"

            [headers.rule]
            type = "secret"
            value = "api-key-secret"

            [[headers]]
            header = "X-Custom-Signature"

            [headers.rule]
            type = "signature"
            token = "signing-secret"
            signature_prefix = "custom="
            "#};

        let configs: HeaderConfigs = toml::from_str(toml_str).unwrap();
        assert_eq!(configs.headers.len(), 3);

        // Validate Authorization header
        assert_eq!(configs.headers[0].header, "Authorization");
        match &configs.headers[0].rule {
            HeaderSource::Static { value } => {
                assert_eq!(value, "Bearer static-token");
            }
            _ => panic!("Expected static header source"),
        }

        // Validate X-API-Key header
        assert_eq!(configs.headers[1].header, "X-API-Key");
        match &configs.headers[1].rule {
            HeaderSource::Secret { value } => {
                assert_eq!(value.expose_secret(), "api-key-secret");
            }
            _ => panic!("Expected secret header source"),
        }

        // Validate signature header
        assert_eq!(configs.headers[2].header, "X-Custom-Signature");
        match &configs.headers[2].rule {
            HeaderSource::Signature { signature_prefix, .. } => {
                assert_eq!(signature_prefix, &Some("custom=".to_string()));
            }
            _ => panic!("Expected signature header source"),
        }
    }

    #[test]
    #[allow(clippy::match_wildcard_for_single_variants)]
    fn test_toml_parsing_simple_header_config() {
        // Test the simplified format that flattens the config
        let toml_str = indoc! {r#"
            header = "Authorization"
            value = "Bearer simple-token"
            "#};

        let config: SimpleHeaderConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.header, "Authorization");
        match &config.config {
            SimpleHeaderValue::Simple { value } => {
                assert_eq!(value, "Bearer simple-token");
            }
            _ => panic!("Expected simple header value"),
        }

        // Test conversion to OutgoingHeaderConfig
        let outgoing: OutgoingHeaderConfig = config.into();
        assert_eq!(outgoing.header, "Authorization");
        match outgoing.rule {
            HeaderSource::Static { value } => {
                assert_eq!(value, "Bearer simple-token");
            }
            _ => panic!("Expected static header source"),
        }
    }

    #[test]
    fn test_toml_parsing_simple_header_config_with_complex_rule() {
        let toml_str = indoc! {r#"
            header = "X-Signature"
            type = "signature"
            token = "webhook-secret"
            signature_prefix = "sha256="
            "#};

        let config: SimpleHeaderConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.header, "X-Signature");
        match config.config {
            SimpleHeaderValue::Complex(HeaderSource::Signature { signature_prefix, .. }) => {
                assert_eq!(signature_prefix, Some("sha256=".to_string()));
            }
            _ => panic!("Expected complex signature header value"),
        }
    }

    #[test]
    fn test_toml_parsing_header_generation_static_roundtrip() {
        // Test that we can serialize and deserialize static configs
        // (Secret configs can't be serialized due to SecretString security)
        let original_config = OutgoingHeaderConfig {
            header: "X-Custom-Header".to_string(),
            rule: HeaderSource::Static { value: "static-value".to_string() },
        };

        let serialized = toml::to_string(&original_config).unwrap();
        let deserialized: OutgoingHeaderConfig = toml::from_str(&serialized).unwrap();

        assert_eq!(original_config.header, deserialized.header);
        match (&original_config.rule, &deserialized.rule) {
            (HeaderSource::Static { value: orig }, HeaderSource::Static { value: deser }) => {
                assert_eq!(orig, deser);
            }
            _ => panic!("Config types don't match"),
        }
    }

    #[test]
    fn test_toml_parsing_signature_deserialization() {
        use secrecy::ExposeSecret;

        // Test that we can deserialize signature configs from TOML
        // (We can't serialize SecretString, but we can deserialize it)
        let toml_str = indoc! {r#"
            header = "X-Signature"

            [rule]
            type = "signature"
            token = "secret-key"
            token_encoding = "base64"
            signature_prefix = "custom="
            signature_on = "body"
            signature_encoding = "hex"
            "#};

        let config: OutgoingHeaderConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.header, "X-Signature");

        match config.rule {
            HeaderSource::Signature {
                token,
                token_encoding,
                signature_prefix,
                signature_on,
                signature_encoding,
            } => {
                assert_eq!(token.expose_secret(), "secret-key");
                assert_eq!(token_encoding, Some(Encoding::Base64));
                assert_eq!(signature_prefix, Some("custom=".to_string()));
                assert_eq!(signature_on, SignatureOn::Body);
                assert_eq!(signature_encoding, Encoding::Hex);
            }
            _ => panic!("Expected signature header source"),
        }
    }

    #[test]
    fn test_outgoing_header_map_to_configs() {
        let mut map = OutgoingHeaderMap::new();
        map.insert(
            "Authorization".to_string(),
            HeaderSource::Static { value: "Bearer token".to_string() },
        );
        map.insert("X-API-Key".to_string(), HeaderSource::Secret { value: "secret123".into() });

        let configs = outgoing_header_map_to_configs(&map);
        assert_eq!(configs.len(), 2);

        // Find the authorization header
        let auth_header = configs.iter().find(|h| h.header == "Authorization").unwrap();
        match &auth_header.rule {
            HeaderSource::Static { value } => assert_eq!(value, "Bearer token"),
            _ => panic!("Expected static source"),
        }

        // Find the API key header
        let api_key_header = configs.iter().find(|h| h.header == "X-API-Key").unwrap();
        match &api_key_header.rule {
            HeaderSource::Secret { .. } => {}
            _ => panic!("Expected secret source"),
        }
    }

    #[test]
    fn test_toml_parsing_outgoing_header_map() {
        #[derive(serde::Deserialize)]
        struct Config {
            headers: OutgoingHeaderMap,
        }

        let toml_str = indoc! {r#"
            [headers]
            "Authorization" = { type = "static", value = "Bearer token123" }
            "X-API-Key" = { type = "secret", value = "secret-from-env" }
            "X-Hub-Signature-256" = { type = "signature", token = "webhook-secret", signature_prefix = "sha256=" }
            "#};

        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.headers.len(), 3);

        // Check Authorization header
        match config.headers.get("Authorization").unwrap() {
            HeaderSource::Static { value } => assert_eq!(value, "Bearer token123"),
            _ => panic!("Expected static header source"),
        }

        // Check X-API-Key header
        match config.headers.get("X-API-Key").unwrap() {
            HeaderSource::Secret { .. } => {}
            _ => panic!("Expected secret header source"),
        }

        // Check signature header
        match config.headers.get("X-Hub-Signature-256").unwrap() {
            HeaderSource::Signature { signature_prefix, .. } => {
                assert_eq!(signature_prefix, &Some("sha256=".to_string()));
            }
            _ => panic!("Expected signature header source"),
        }
    }
}
