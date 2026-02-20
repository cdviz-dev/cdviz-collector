use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;

use super::signature::{self, Encoding, SignatureOn};

/// Configuration for validating incoming request headers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeaderRuleConfig {
    /// The header name to validate
    pub header: String,
    /// The validation rule to apply
    pub rule: Rule,
}

impl From<signature::SignatureConfig> for HeaderRuleConfig {
    fn from(sig_config: signature::SignatureConfig) -> Self {
        HeaderRuleConfig {
            header: sig_config.header.clone(),
            rule: Rule::Signature {
                token: sig_config.token,
                token_encoding: sig_config.token_encoding,
                signature_prefix: sig_config.signature_prefix,
                signature_on: sig_config.signature_on,
                signature_encoding: sig_config.signature_encoding,
            },
        }
    }
}

/// Validation rules for header values
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Rule {
    /// Header must exist (any non-empty value)
    #[serde(rename = "exists")]
    Exists,

    /// Header must match exact value
    #[serde(rename = "equals")]
    Equals {
        value: String,
        #[serde(default = "default_case_sensitive")]
        case_sensitive: bool,
    },

    /// Header must match regex pattern
    #[serde(rename = "matches")]
    Matches { pattern: String },

    /// Header must contain valid HMAC signature
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

fn default_case_sensitive() -> bool {
    true
}

/// Errors that can occur during header validation
#[derive(Debug, Clone, derive_more::Display, derive_more::Error)]
pub enum ValidationError {
    #[display("Header '{header}' is missing")]
    MissingHeader { header: String },

    #[display("Header '{header}' has invalid value: {reason}")]
    InvalidValue { header: String, reason: String },

    #[display("Header '{header}' signature validation failed: {error}")]
    SignatureValidation { header: String, error: String },

    #[display("Header '{header}' pattern validation failed")]
    PatternMismatch { header: String },
}

impl IntoResponse for ValidationError {
    fn into_response(self) -> Response {
        let status = match self {
            ValidationError::MissingHeader { .. } | ValidationError::SignatureValidation { .. } => {
                StatusCode::UNAUTHORIZED
            }
            ValidationError::InvalidValue { .. } | ValidationError::PatternMismatch { .. } => {
                StatusCode::FORBIDDEN
            }
        };

        let body = format!("Header validation failed: {self}");
        (status, body).into_response()
    }
}

/// Validate headers against a list of header rule configurations
pub fn validate_headers(
    headers: &HeaderMap,
    rules: &[HeaderRuleConfig],
    body: Option<&[u8]>,
) -> Result<(), ValidationError> {
    for rule_config in rules {
        validate_header(headers, rule_config, body)?;
    }
    Ok(())
}

/// Validate a single header against its rule configuration
pub fn validate_header(
    headers: &HeaderMap,
    rule_config: &HeaderRuleConfig,
    body: Option<&[u8]>,
) -> Result<(), ValidationError> {
    let header_name = HeaderName::from_str(&rule_config.header).map_err(|err| {
        ValidationError::InvalidValue {
            header: rule_config.header.clone(),
            reason: err.to_string(),
        }
    })?;

    let header_value = headers.get(&header_name);

    match &rule_config.rule {
        Rule::Exists => {
            if header_value.is_none_or(axum::http::HeaderValue::is_empty) {
                return Err(ValidationError::MissingHeader { header: rule_config.header.clone() });
            }
        }
        Rule::Equals { value, case_sensitive } => {
            let actual_value = header_value.and_then(|v| v.to_str().ok()).ok_or_else(|| {
                ValidationError::MissingHeader { header: rule_config.header.clone() }
            })?;

            let matches = if *case_sensitive {
                actual_value == value
            } else {
                actual_value.to_lowercase() == value.to_lowercase()
            };

            if !matches {
                return Err(ValidationError::InvalidValue {
                    header: rule_config.header.clone(),
                    reason: "value does not match expected".to_string(),
                });
            }
        }
        Rule::Matches { pattern } => {
            let actual_value = header_value.and_then(|v| v.to_str().ok()).ok_or_else(|| {
                ValidationError::MissingHeader { header: rule_config.header.clone() }
            })?;

            let regex = regex::Regex::new(pattern).map_err(|err| ValidationError::InvalidValue {
                header: rule_config.header.clone(),
                reason: err.to_string(),
            })?;

            if !regex.is_match(actual_value) {
                return Err(ValidationError::PatternMismatch {
                    header: rule_config.header.clone(),
                });
            }
        }
        Rule::Signature {
            token,
            token_encoding,
            signature_prefix,
            signature_on,
            signature_encoding,
        } => {
            let signature_config = signature::SignatureConfig {
                header: rule_config.header.clone(),
                token: token.clone(),
                token_encoding: token_encoding.clone(),
                signature_prefix: signature_prefix.clone(),
                signature_on: signature_on.clone(),
                signature_encoding: signature_encoding.clone(),
            };

            let body_vec = body.unwrap_or(&[]).to_vec();
            let body_bytes = axum::body::Bytes::from(body_vec);
            match signature::check_signature(&signature_config, headers, &body_bytes) {
                Ok(()) => {}
                Err(e) => {
                    return Err(ValidationError::SignatureValidation {
                        header: rule_config.header.clone(),
                        error: e.to_string(),
                    });
                }
            }
        }
    }

    Ok(())
}

/// Map-based configuration for header validation rules
/// Maps header names directly to their validation rules
pub type HeaderRuleMap = HashMap<String, Rule>;

/// Convert map-based configuration to the internal Vec<HeaderRuleConfig> format
pub fn header_rule_map_to_configs(map: &HeaderRuleMap) -> Vec<HeaderRuleConfig> {
    map.iter()
        .map(|(header, rule)| HeaderRuleConfig { header: header.clone(), rule: rule.clone() })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};
    use indoc::indoc;

    #[test]
    fn test_rule_exists_validation() {
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", HeaderValue::from_static("Bearer token"));

        let rule_config =
            HeaderRuleConfig { header: "Authorization".to_string(), rule: Rule::Exists };

        assert!(validate_header(&headers, &rule_config, None).is_ok());

        // Test missing header
        let empty_headers = HeaderMap::new();
        assert!(validate_header(&empty_headers, &rule_config, None).is_err());
    }

    #[test]
    fn test_rule_equals_validation() {
        let mut headers = HeaderMap::new();
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));

        let rule_config = HeaderRuleConfig {
            header: "Content-Type".to_string(),
            rule: Rule::Equals { value: "application/json".to_string(), case_sensitive: true },
        };

        assert!(validate_header(&headers, &rule_config, None).is_ok());

        // Test case insensitive
        let rule_config_insensitive = HeaderRuleConfig {
            header: "Content-Type".to_string(),
            rule: Rule::Equals { value: "APPLICATION/JSON".to_string(), case_sensitive: false },
        };

        assert!(validate_header(&headers, &rule_config_insensitive, None).is_ok());

        // Test mismatch
        let rule_config_mismatch = HeaderRuleConfig {
            header: "Content-Type".to_string(),
            rule: Rule::Equals { value: "text/plain".to_string(), case_sensitive: true },
        };

        assert!(validate_header(&headers, &rule_config_mismatch, None).is_err());
    }

    #[test]
    fn test_rule_matches_validation() {
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", HeaderValue::from_static("Bearer abc123"));

        let rule_config = HeaderRuleConfig {
            header: "Authorization".to_string(),
            rule: Rule::Matches { pattern: r"^Bearer \w+$".to_string() },
        };

        assert!(validate_header(&headers, &rule_config, None).is_ok());

        // Test pattern mismatch
        let rule_config_mismatch = HeaderRuleConfig {
            header: "Authorization".to_string(),
            rule: Rule::Matches { pattern: r"^Basic \w+$".to_string() },
        };

        assert!(validate_header(&headers, &rule_config_mismatch, None).is_err());
    }

    #[test]
    fn test_header_rule_config_from_signature_config() {
        use secrecy::ExposeSecret;
        let sig_config = signature::SignatureConfig {
            header: "X-Hub-Signature-256".to_string(),
            token: "secret".into(),
            token_encoding: None,
            signature_prefix: Some("sha256=".to_string()),
            signature_on: SignatureOn::Body,
            signature_encoding: Encoding::Hex,
        };

        let rule_config = HeaderRuleConfig::from(sig_config.clone());
        assert_eq!(rule_config.header, sig_config.header);

        match rule_config.rule {
            Rule::Signature { token, signature_prefix, .. } => {
                assert_eq!(token.expose_secret(), sig_config.token.expose_secret());
                assert_eq!(signature_prefix, sig_config.signature_prefix);
            }
            _ => panic!("Expected signature rule"),
        }
    }

    #[test]
    fn test_multiple_header_validation() {
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", HeaderValue::from_static("Bearer token123"));
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));

        let rules = vec![
            HeaderRuleConfig {
                header: "Authorization".to_string(),
                rule: Rule::Matches { pattern: r"^Bearer \w+$".to_string() },
            },
            HeaderRuleConfig {
                header: "Content-Type".to_string(),
                rule: Rule::Equals { value: "application/json".to_string(), case_sensitive: true },
            },
        ];

        assert!(validate_headers(&headers, &rules, None).is_ok());

        // Test when one rule fails
        let rules_with_failure = vec![
            HeaderRuleConfig {
                header: "Authorization".to_string(),
                rule: Rule::Matches { pattern: r"^Bearer \w+$".to_string() },
            },
            HeaderRuleConfig { header: "X-API-Key".to_string(), rule: Rule::Exists },
        ];

        assert!(validate_headers(&headers, &rules_with_failure, None).is_err());
    }

    #[test]
    fn test_toml_parsing_header_rule_config_exists() {
        let toml_str = indoc! {r#"
            header = "X-API-Key"

            [rule]
            type = "exists"
            "#};

        let config: HeaderRuleConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.header, "X-API-Key");
        assert!(matches!(config.rule, Rule::Exists));
    }

    #[test]
    fn test_toml_parsing_header_rule_config_equals() {
        let toml_str = indoc! {r#"
            header = "Content-Type"

            [rule]
            type = "equals"
            value = "application/json"
            case_sensitive = false
            "#};

        let config: HeaderRuleConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.header, "Content-Type");
        match config.rule {
            Rule::Equals { value, case_sensitive } => {
                assert_eq!(value, "application/json");
                assert!(!case_sensitive);
            }
            _ => panic!("Expected equals rule"),
        }
    }

    #[test]
    fn test_toml_parsing_header_rule_config_matches() {
        let toml_str = indoc! {r#"
            header = "User-Agent"

            [rule]
            type = "matches"
            pattern = "^Mozilla.*"
            "#};

        let config: HeaderRuleConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.header, "User-Agent");
        match config.rule {
            Rule::Matches { pattern } => {
                assert_eq!(pattern, "^Mozilla.*");
            }
            _ => panic!("Expected matches rule"),
        }
    }

    #[test]
    fn test_toml_parsing_header_rule_config_signature() {
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

        let config: HeaderRuleConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.header, "X-Hub-Signature-256");
        match config.rule {
            Rule::Signature {
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
            _ => panic!("Expected signature rule"),
        }
    }

    #[test]
    fn test_header_rule_map_to_configs() {
        let mut map = HeaderRuleMap::new();
        map.insert("Authorization".to_string(), Rule::Exists);
        map.insert(
            "Content-Type".to_string(),
            Rule::Equals { value: "application/json".to_string(), case_sensitive: true },
        );

        let configs = header_rule_map_to_configs(&map);
        assert_eq!(configs.len(), 2);

        // Find the authorization rule
        let auth_rule = configs.iter().find(|r| r.header == "Authorization").unwrap();
        assert!(matches!(auth_rule.rule, Rule::Exists));

        // Find the content-type rule
        let content_type_rule = configs.iter().find(|r| r.header == "Content-Type").unwrap();
        match &content_type_rule.rule {
            Rule::Equals { value, case_sensitive } => {
                assert_eq!(value, "application/json");
                assert!(*case_sensitive);
            }
            _ => panic!("Expected equals rule"),
        }
    }

    #[test]
    fn test_toml_parsing_header_rule_map() {
        #[derive(serde::Deserialize)]
        struct Config {
            headers: HeaderRuleMap,
        }

        let toml_str = indoc! {r#"
            [headers]
            "Authorization" = { type = "exists" }
            "X-API-Key" = { type = "equals", value = "secret123", case_sensitive = true }
            "X-Hub-Signature-256" = { type = "signature", token = "webhook-secret", signature_prefix = "sha256=" }
            "#};

        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.headers.len(), 3);

        // Check Authorization rule
        match config.headers.get("Authorization").unwrap() {
            Rule::Exists => {}
            _ => panic!("Expected exists rule"),
        }

        // Check X-API-Key rule
        match config.headers.get("X-API-Key").unwrap() {
            Rule::Equals { value, case_sensitive } => {
                assert_eq!(value, "secret123");
                assert!(*case_sensitive);
            }
            _ => panic!("Expected equals rule"),
        }

        // Check signature rule
        match config.headers.get("X-Hub-Signature-256").unwrap() {
            Rule::Signature { signature_prefix, .. } => {
                assert_eq!(signature_prefix, &Some("sha256=".to_string()));
            }
            _ => panic!("Expected signature rule"),
        }
    }

    #[test]
    fn test_comprehensive_figment_configuration_with_toml_override() {
        use crate::config::Toml;
        use figment::{Figment, providers::Format};
        use figment_file_provider_adapter::FileAdapter;
        use secrecy::ExposeSecret;

        #[derive(serde::Deserialize)]
        struct WebhookConfig {
            id: String,
            headers: HeaderRuleMap,
        }

        #[derive(serde::Deserialize)]
        struct TestConfig {
            webhook: WebhookConfig,
        }

        // Test 1: Base TOML configuration with signature token
        let base_toml_config = indoc! {r#"
            [webhook]
            id = "github-webhook"

            [webhook.headers]
            "x-hub-signature-256" = { type = "signature", signature_encoding = "hex", signature_on = "body", signature_prefix = "sha256=", token = "default-secret-token" }
            "authorization" = { type = "matches", pattern = "^Bearer \\w+$" }
            "content-type" = { type = "equals", value = "application/json", case_sensitive = false }
            "#};

        let config: TestConfig = Figment::new()
            .merge(FileAdapter::wrap(Toml::string(base_toml_config)))
            .extract()
            .expect("Failed to parse base TOML configuration");

        assert_eq!(config.webhook.id, "github-webhook");
        assert_eq!(config.webhook.headers.len(), 3);

        // Verify signature rule with default token
        match config.webhook.headers.get("x-hub-signature-256").unwrap() {
            Rule::Signature { token, signature_prefix, signature_encoding, .. } => {
                assert_eq!(token.expose_secret(), "default-secret-token");
                assert_eq!(signature_prefix, &Some("sha256=".to_string()));
                assert_eq!(*signature_encoding, Encoding::Hex);
            }
            _ => panic!("Expected signature rule"),
        }

        // Test 2: Override configuration with different token
        let override_toml_config = indoc! {r#"
            [webhook.headers]
            "x-hub-signature-256" = { type = "signature", signature_encoding = "hex", signature_on = "body", signature_prefix = "sha256=", token = "overridden-secret-token" }
            "#};

        let config_with_override: TestConfig = Figment::new()
            .merge(FileAdapter::wrap(Toml::string(base_toml_config)))
            .merge(FileAdapter::wrap(Toml::string(override_toml_config)))
            .extract()
            .expect("Failed to parse configuration with override");

        // Verify signature rule with overridden token
        match config_with_override.webhook.headers.get("x-hub-signature-256").unwrap() {
            Rule::Signature { token, signature_prefix, signature_encoding, .. } => {
                assert_eq!(token.expose_secret(), "overridden-secret-token");
                assert_eq!(signature_prefix, &Some("sha256=".to_string()));
                assert_eq!(*signature_encoding, Encoding::Hex);
            }
            _ => panic!("Expected signature rule"),
        }

        // Test 3: Complex override for multiple signature parameters
        let complex_override_toml = indoc! {r#"
            [webhook.headers]
            "x-hub-signature-256" = { type = "signature", signature_encoding = "base64", signature_on = "body", signature_prefix = "custom=", token = "complex-override-token" }
            "authorization" = { type = "matches", pattern = "^Token \\w+$" }
            "#};

        let config_complex_override: TestConfig = Figment::new()
            .merge(FileAdapter::wrap(Toml::string(base_toml_config)))
            .merge(FileAdapter::wrap(Toml::string(complex_override_toml)))
            .extract()
            .expect("Failed to parse configuration with complex override");

        // Verify all overridden signature parameters
        match config_complex_override.webhook.headers.get("x-hub-signature-256").unwrap() {
            Rule::Signature { token, signature_prefix, signature_encoding, .. } => {
                assert_eq!(token.expose_secret(), "complex-override-token");
                assert_eq!(signature_prefix, &Some("custom=".to_string()));
                assert_eq!(*signature_encoding, Encoding::Base64);
            }
            _ => panic!("Expected signature rule"),
        }

        // Verify overridden authorization pattern
        match config_complex_override.webhook.headers.get("authorization").unwrap() {
            Rule::Matches { pattern } => {
                assert_eq!(pattern, "^Token \\w+$");
            }
            _ => panic!("Expected matches rule"),
        }

        // Test 4: Add new header rule via configuration override
        let add_header_toml = indoc! {r#"
            [webhook.headers]
            "x-api-key" = { type = "exists" }
            "#};

        let config_new_header: TestConfig = Figment::new()
            .merge(FileAdapter::wrap(Toml::string(base_toml_config)))
            .merge(FileAdapter::wrap(Toml::string(add_header_toml)))
            .extract()
            .expect("Failed to parse configuration with new header");

        // Verify new header rule was added
        assert_eq!(config_new_header.webhook.headers.len(), 4);
        match config_new_header.webhook.headers.get("x-api-key").unwrap() {
            Rule::Exists => {}
            _ => panic!("Expected exists rule"),
        }
    }

    #[test]
    fn test_figment_environment_variable_override() {
        use crate::config::Toml;
        use figment::{
            Figment, Jail,
            providers::{Env, Format},
        };
        use figment_file_provider_adapter::FileAdapter;
        use secrecy::ExposeSecret;

        Jail::expect_with(|jail| {
            #[derive(serde::Deserialize)]
            struct WebhookConfig {
                id: String,
                headers: HeaderRuleMap,
            }

            #[derive(serde::Deserialize)]
            struct TestConfig {
                webhook: WebhookConfig,
            }

            // Base TOML configuration with signature token
            let base_toml_config = indoc! {r#"
                [webhook]
                id = "github-webhook"

                [webhook.headers]
                "x-hub-signature-256" = { type = "signature", signature_encoding = "hex", signature_on = "body", signature_prefix = "sha256=", token = "default-secret-token" }
                "authorization" = { type = "matches", pattern = "^Bearer \\w+$" }
            "#};

            // Set environment variables that should override the signature configuration
            jail.set_env("TEST_WEBHOOK__HEADERS__X_HUB_SIGNATURE_256__TYPE", "signature");
            jail.set_env(
                "TEST_WEBHOOK__HEADERS__X_HUB_SIGNATURE_256__TOKEN",
                "env-overridden-token",
            );
            jail.set_env("TEST_WEBHOOK__HEADERS__X_HUB_SIGNATURE_256__SIGNATURE_ENCODING", "hex");
            jail.set_env("TEST_WEBHOOK__HEADERS__X_HUB_SIGNATURE_256__SIGNATURE_ON", "body");
            jail.set_env("TEST_WEBHOOK__HEADERS__X_HUB_SIGNATURE_256__SIGNATURE_PREFIX", "sha256=");

            let config: TestConfig = Figment::new()
                .merge(FileAdapter::wrap(Toml::string(base_toml_config)))
                .merge(FileAdapter::wrap(Env::prefixed("TEST_").split("__")))
                .extract()
                .expect("Failed to parse configuration with environment override");

            assert_eq!(config.webhook.id, "github-webhook");

            // Figment creates entries for both hyphenated and underscored keys
            // This test verifies that environment variables can override configuration values
            assert!(config.webhook.headers.len() >= 2);

            // Environment variables create underscored key names
            let env_override_rule = config
                .webhook
                .headers
                .get("x_hub_signature_256")
                .expect("Environment variable should create x_hub_signature_256 key");

            match env_override_rule {
                Rule::Signature { token, signature_prefix, signature_encoding, .. } => {
                    assert_eq!(token.expose_secret(), "env-overridden-token");
                    assert_eq!(signature_prefix, &Some("sha256=".to_string()));
                    assert_eq!(*signature_encoding, Encoding::Hex);
                }
                _ => panic!("Expected signature rule for environment override"),
            }

            // Verify other header remains unchanged
            match config.webhook.headers.get("authorization").unwrap() {
                Rule::Matches { pattern } => {
                    assert_eq!(pattern, "^Bearer \\w+$");
                }
                _ => panic!("Expected matches rule"),
            }

            Ok(())
        });
    }

    #[test]
    fn test_figment_signature_validation_roundtrip() {
        use crate::config::Toml;
        use axum::http::HeaderValue;
        use figment::{Figment, providers::Format};
        use figment_file_provider_adapter::FileAdapter;
        use secrecy::ExposeSecret;

        #[derive(serde::Deserialize)]
        struct SignatureTestConfig {
            headers: HeaderRuleMap,
        }

        // TOML with signature configuration
        let toml_config = indoc! {r#"
            [headers]
            "x-webhook-signature" = { type = "signature", token = "original-secret", signature_prefix = "sig=", signature_on = "body", signature_encoding = "hex" }
            "#};

        let config: SignatureTestConfig = Figment::new()
            .merge(FileAdapter::wrap(Toml::string(toml_config)))
            .extract()
            .expect("Failed to parse signature configuration");

        // Convert to HeaderRuleConfig for validation
        let rule_configs = header_rule_map_to_configs(&config.headers);
        assert_eq!(rule_configs.len(), 1);

        let rule_config = &rule_configs[0];
        assert_eq!(rule_config.header, "x-webhook-signature");

        // Verify the configuration was parsed correctly
        match &rule_config.rule {
            Rule::Signature { token, signature_prefix, .. } => {
                assert_eq!(token.expose_secret(), "original-secret");
                assert_eq!(signature_prefix, &Some("sig=".to_string()));
            }
            _ => panic!("Expected signature rule"),
        }

        // Test header validation with the parsed configuration
        let mut headers = HeaderMap::new();
        let body = b"test payload";

        // Generate signature using the original token
        let signature_config = signature::SignatureConfig {
            header: "x-webhook-signature".to_string(),
            token: "original-secret".into(),
            token_encoding: None,
            signature_prefix: Some("sig=".to_string()),
            signature_on: SignatureOn::Body,
            signature_encoding: Encoding::Hex,
        };

        let generated_signature =
            signature::build_signature(&signature_config, &HeaderMap::new(), body)
                .expect("Failed to generate signature");

        headers.insert("x-webhook-signature", HeaderValue::from_str(&generated_signature).unwrap());

        // Validation should succeed with the original token
        assert!(validate_header(&headers, rule_config, Some(body)).is_ok());

        // Test override with different token
        let override_toml_config = indoc! {r#"
            [headers]
            "x-webhook-signature" = { type = "signature", token = "overridden-secret", signature_prefix = "sig=", signature_on = "body", signature_encoding = "hex" }
            "#};

        let config_override: SignatureTestConfig = Figment::new()
            .merge(FileAdapter::wrap(Toml::string(toml_config)))
            .merge(FileAdapter::wrap(Toml::string(override_toml_config)))
            .extract()
            .expect("Failed to parse overridden signature configuration");

        // Convert to HeaderRuleConfig for validation
        let override_configs = header_rule_map_to_configs(&config_override.headers);
        let override_config = &override_configs[0];

        // Verify the override worked
        match &override_config.rule {
            Rule::Signature { token, signature_prefix, .. } => {
                assert_eq!(token.expose_secret(), "overridden-secret");
                assert_eq!(signature_prefix, &Some("sig=".to_string()));
            }
            _ => panic!("Expected signature rule"),
        }
    }

    #[test]
    fn test_invalid_regex_pattern_includes_reason() {
        let mut headers = HeaderMap::new();
        headers.insert("X-Custom", HeaderValue::from_static("value"));

        let rule_config = HeaderRuleConfig {
            header: "X-Custom".to_string(),
            // Deliberately invalid regex: unmatched `(`
            rule: Rule::Matches { pattern: r"(invalid[".to_string() },
        };

        let err = validate_header(&headers, &rule_config, None).unwrap_err();
        match err {
            ValidationError::InvalidValue { header, reason } => {
                assert_eq!(header, "X-Custom");
                assert!(!reason.is_empty(), "regex compile error should be included in reason");
            }
            other => panic!("Expected InvalidValue, got {other:?}"),
        }
    }
}
