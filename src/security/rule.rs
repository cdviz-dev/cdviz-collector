use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use regex::Regex;
use secrecy::SecretString;
use serde::Deserialize;

use super::signature::{self, Encoding, SignatureOn};
use crate::errors::Result;

/// Configuration for a header rule (validation or generation)
#[derive(Debug, Clone, Deserialize)]
pub struct HeaderRuleConfig {
    /// The header name
    pub header: String,
    /// The rule to apply
    pub rule: Rule,
}

/// Different rules for header validation or generation
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum Rule {
    /// HMAC signature validation (reusing existing system)
    #[serde(rename = "signature")]
    Signature {
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

    /// Static value comparison
    #[serde(rename = "equals")]
    Equals {
        value: String,
        #[serde(default)]
        case_sensitive: bool,
    },

    /// Regex pattern matching
    #[serde(rename = "matches")]
    Matches { pattern: String },

    /// Header existence check
    #[serde(rename = "exists")]
    Exists,

    /// Multiple rule options (OR logic)
    #[serde(rename = "any_of")]
    AnyOf { rules: Vec<Rule> },

    /// All rules must pass (AND logic)
    #[serde(rename = "all_of")]
    AllOf { rules: Vec<Rule> },
}

impl From<signature::SignatureConfig> for HeaderRuleConfig {
    fn from(signature: signature::SignatureConfig) -> Self {
        Self {
            header: signature.header,
            rule: Rule::Signature {
                token: signature.token,
                token_encoding: signature.token_encoding,
                signature_prefix: signature.signature_prefix,
                signature_on: signature.signature_on,
                signature_encoding: signature.signature_encoding,
            },
        }
    }
}

/// Errors that can occur during header validation
#[derive(Debug, Clone, derive_more::Display, derive_more::Error)]
pub enum ValidationError {
    #[display("Header '{header}' not found")]
    HeaderNotFound { header: String },

    #[display("Header '{header}' value '{actual}' does not equal expected '{expected}'")]
    ValueMismatch { header: String, expected: String, actual: String },

    #[display("Header '{header}' value '{value}' does not match pattern '{pattern}'")]
    PatternMismatch { header: String, value: String, pattern: String },

    #[display("Header '{header}' signature validation failed: {error}")]
    SignatureValidation {
        header: String,
        error: String, // Convert SignatureError to String to avoid Clone issues
    },

    #[display("Regex compilation failed for pattern '{pattern}': {error}")]
    InvalidRegex { pattern: String, error: String },

    #[display("Header '{header}' value contains invalid UTF-8")]
    InvalidUtf8 { header: String },

    #[display("All validation methods failed")]
    AllMethodsFailed,
}

impl IntoResponse for ValidationError {
    fn into_response(self) -> Response {
        let status = match self {
            ValidationError::ValueMismatch { .. } | ValidationError::PatternMismatch { .. } => {
                StatusCode::FORBIDDEN
            }
            ValidationError::HeaderNotFound { .. }
            | ValidationError::SignatureValidation { .. }
            | ValidationError::AllMethodsFailed => StatusCode::UNAUTHORIZED,
            ValidationError::InvalidRegex { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            ValidationError::InvalidUtf8 { .. } => StatusCode::BAD_REQUEST,
        };

        (status, self.to_string()).into_response()
    }
}

/// Validates a single header using the specified rule
pub fn validate_header(
    headers: &HeaderMap,
    config: &HeaderRuleConfig,
    body: Option<&[u8]>,
) -> Result<(), ValidationError> {
    validate_rule(headers, &config.header, &config.rule, body)
}

/// Validates multiple headers using their respective rules
pub fn validate_headers(
    headers: &HeaderMap,
    configs: &[HeaderRuleConfig],
    body: Option<&[u8]>,
) -> Result<(), ValidationError> {
    for config in configs {
        validate_header(headers, config, body)?;
    }
    Ok(())
}

/// Internal function to validate a header using a specific rule
fn validate_rule(
    headers: &HeaderMap,
    header_name: &str,
    rule: &Rule,
    body: Option<&[u8]>,
) -> Result<(), ValidationError> {
    match rule {
        Rule::Exists => {
            if !headers.contains_key(header_name) {
                return Err(ValidationError::HeaderNotFound { header: header_name.to_string() });
            }
            Ok(())
        }

        Rule::Equals { value, case_sensitive } => {
            let header_value = get_header_value(headers, header_name)?;

            let matches = if *case_sensitive {
                header_value == *value
            } else {
                header_value.to_lowercase() == value.to_lowercase()
            };

            if !matches {
                return Err(ValidationError::ValueMismatch {
                    header: header_name.to_string(),
                    expected: value.clone(),
                    actual: header_value,
                });
            }
            Ok(())
        }

        Rule::Matches { pattern } => {
            let header_value = get_header_value(headers, header_name)?;

            let regex = Regex::new(pattern).map_err(|err| ValidationError::InvalidRegex {
                pattern: pattern.clone(),
                error: err.to_string(),
            })?;

            if !regex.is_match(&header_value) {
                return Err(ValidationError::PatternMismatch {
                    header: header_name.to_string(),
                    value: header_value,
                    pattern: pattern.clone(),
                });
            }
            Ok(())
        }

        Rule::Signature {
            token,
            token_encoding,
            signature_prefix,
            signature_on,
            signature_encoding,
        } => {
            // Convert to the existing SignatureConfig format
            let signature_config = signature::SignatureConfig {
                header: header_name.to_string(),
                token: token.clone(),
                token_encoding: token_encoding.clone(),
                signature_prefix: signature_prefix.clone(),
                signature_on: signature_on.clone(),
                signature_encoding: signature_encoding.clone(),
            };

            // Use the existing signature validation
            let body_bytes = body.unwrap_or(&[]);
            let body_bytes = bytes::Bytes::copy_from_slice(body_bytes);

            signature::check_signature(&signature_config, headers, &body_bytes).map_err(
                |error| ValidationError::SignatureValidation {
                    header: header_name.to_string(),
                    error: error.to_string(),
                },
            )?;

            Ok(())
        }

        Rule::AnyOf { rules } => {
            for rule in rules {
                if validate_rule(headers, header_name, rule, body).is_ok() {
                    return Ok(());
                }
            }
            Err(ValidationError::AllMethodsFailed)
        }

        Rule::AllOf { rules } => {
            for rule in rules {
                validate_rule(headers, header_name, rule, body)?;
            }
            Ok(())
        }
    }
}

/// Helper function to get header value as string
fn get_header_value(headers: &HeaderMap, header_name: &str) -> Result<String, ValidationError> {
    let header_value = headers
        .get(header_name)
        .ok_or_else(|| ValidationError::HeaderNotFound { header: header_name.to_string() })?;

    header_value
        .to_str()
        .map(std::string::ToString::to_string)
        .map_err(|_| ValidationError::InvalidUtf8 { header: header_name.to_string() })
}

/// Helper trait to make case-insensitive matching configurable with defaults
impl Default for Rule {
    fn default() -> Self {
        Rule::Exists
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderName, HeaderValue};
    use indoc::indoc;
    use std::str::FromStr;

    #[derive(serde::Deserialize)]
    struct HeaderConfigs {
        headers: Vec<HeaderRuleConfig>,
    }

    fn create_headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers
                .insert(HeaderName::from_str(name).unwrap(), HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn test_exists_validation_success() {
        let headers = create_headers(&[("Authorization", "Bearer token")]);
        let config = HeaderRuleConfig { header: "Authorization".to_string(), rule: Rule::Exists };

        assert!(validate_header(&headers, &config, None).is_ok());
    }

    #[test]
    fn test_exists_validation_failure() {
        let headers = HeaderMap::new();
        let config = HeaderRuleConfig { header: "Authorization".to_string(), rule: Rule::Exists };

        let result = validate_header(&headers, &config, None);
        assert!(matches!(result, Err(ValidationError::HeaderNotFound { .. })));
    }

    #[test]
    fn test_equals_validation_success() {
        let headers = create_headers(&[("X-API-Key", "secret123")]);
        let config = HeaderRuleConfig {
            header: "X-API-Key".to_string(),
            rule: Rule::Equals { value: "secret123".to_string(), case_sensitive: true },
        };

        assert!(validate_header(&headers, &config, None).is_ok());
    }

    #[test]
    fn test_equals_validation_case_insensitive() {
        let headers = create_headers(&[("X-API-Key", "Secret123")]);
        let config = HeaderRuleConfig {
            header: "X-API-Key".to_string(),
            rule: Rule::Equals { value: "secret123".to_string(), case_sensitive: false },
        };

        assert!(validate_header(&headers, &config, None).is_ok());
    }

    #[test]
    fn test_matches_validation_success() {
        let headers = create_headers(&[("Authorization", "Bearer abc123")]);
        let config = HeaderRuleConfig {
            header: "Authorization".to_string(),
            rule: Rule::Matches { pattern: r"^Bearer [a-zA-Z0-9]+$".to_string() },
        };

        assert!(validate_header(&headers, &config, None).is_ok());
    }

    #[test]
    fn test_matches_validation_failure() {
        let headers = create_headers(&[("Authorization", "Basic abc123")]);
        let config = HeaderRuleConfig {
            header: "Authorization".to_string(),
            rule: Rule::Matches { pattern: r"^Bearer [a-zA-Z0-9]+$".to_string() },
        };

        let result = validate_header(&headers, &config, None);
        assert!(matches!(result, Err(ValidationError::PatternMismatch { .. })));
    }

    #[test]
    fn test_any_of_validation_success() {
        let headers = create_headers(&[("Authorization", "Bearer token123")]);
        let config = HeaderRuleConfig {
            header: "Authorization".to_string(),
            rule: Rule::AnyOf {
                rules: vec![
                    Rule::Equals { value: "Basic auth".to_string(), case_sensitive: true },
                    Rule::Matches { pattern: r"^Bearer \w+$".to_string() },
                ],
            },
        };

        assert!(validate_header(&headers, &config, None).is_ok());
    }

    #[test]
    fn test_all_of_validation_success() {
        let headers = create_headers(&[("X-Custom", "Bearer-token123")]);
        let config = HeaderRuleConfig {
            header: "X-Custom".to_string(),
            rule: Rule::AllOf {
                rules: vec![
                    Rule::Matches { pattern: r"Bearer-.*".to_string() },
                    Rule::Matches { pattern: r".*token.*".to_string() },
                ],
            },
        };

        assert!(validate_header(&headers, &config, None).is_ok());
    }

    #[test]
    fn test_multiple_headers_validation() {
        let headers =
            create_headers(&[("Authorization", "Bearer token123"), ("X-API-Key", "api-secret")]);

        let configs = vec![
            HeaderRuleConfig {
                header: "Authorization".to_string(),
                rule: Rule::Matches { pattern: r"^Bearer \w+$".to_string() },
            },
            HeaderRuleConfig { header: "X-API-Key".to_string(), rule: Rule::Exists },
        ];

        assert!(validate_headers(&headers, &configs, None).is_ok());
    }

    #[test]
    fn test_toml_parsing_header_rule_config() {
        // Test parsing simple equals rule
        let toml_str = indoc! {r#"
            header = "Authorization"

            [rule]
            type = "equals"
            value = "Bearer token123"
            case_sensitive = true
            "#};

        let config: HeaderRuleConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.header, "Authorization");
        match config.rule {
            Rule::Equals { value, case_sensitive } => {
                assert_eq!(value, "Bearer token123");
                assert!(case_sensitive);
            }
            _ => panic!("Expected equals rule"),
        }
    }

    #[test]
    fn test_toml_parsing_signature_rule() {
        let toml_str = indoc! {r#"
            header = "X-Hub-Signature-256"

            [rule]
            type = "signature"
            token = "webhook-secret"
            signature_prefix = "sha256="
            signature_encoding = "hex"
            "#};

        let config: HeaderRuleConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.header, "X-Hub-Signature-256");
        match config.rule {
            Rule::Signature { signature_prefix, signature_encoding, .. } => {
                assert_eq!(signature_prefix, Some("sha256=".to_string()));
                assert_eq!(signature_encoding, super::super::signature::Encoding::Hex);
            }
            _ => panic!("Expected signature rule"),
        }
    }

    #[test]
    fn test_toml_parsing_matches_rule() {
        let toml_str = indoc! {r#"
            header = "User-Agent"

            [rule]
            type = "matches"
            pattern = "^MyApp/[0-9]+\\.[0-9]+$"
            "#};

        let config: HeaderRuleConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.header, "User-Agent");
        match config.rule {
            Rule::Matches { pattern } => {
                assert_eq!(pattern, r"^MyApp/[0-9]+\.[0-9]+$");
            }
            _ => panic!("Expected matches rule"),
        }
    }

    #[test]
    fn test_toml_parsing_exists_rule() {
        let toml_str = indoc! {r#"
            header = "X-API-Key"

            [rule]
            type = "exists"
            "#};

        let config: HeaderRuleConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.header, "X-API-Key");
        match config.rule {
            Rule::Exists => {}
            _ => panic!("Expected exists rule"),
        }
    }

    #[test]
    fn test_toml_parsing_any_of_rule() {
        let toml_str = indoc! {r#"
            header = "Authorization"

            [rule]
            type = "any_of"

            [[rule.rules]]
            type = "equals"
            value = "Bearer valid-token"
            case_sensitive = true

            [[rule.rules]]
            type = "matches"
            pattern = "^Basic [A-Za-z0-9+/=]+$"
            "#};

        let config: HeaderRuleConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.header, "Authorization");
        match config.rule {
            Rule::AnyOf { rules } => {
                assert_eq!(rules.len(), 2);
                match &rules[0] {
                    Rule::Equals { value, case_sensitive } => {
                        assert_eq!(value, "Bearer valid-token");
                        assert!(*case_sensitive);
                    }
                    _ => panic!("Expected equals rule"),
                }
                match &rules[1] {
                    Rule::Matches { pattern } => {
                        assert_eq!(pattern, "^Basic [A-Za-z0-9+/=]+$");
                    }
                    _ => panic!("Expected matches rule"),
                }
            }
            _ => panic!("Expected any_of rule"),
        }
    }

    #[test]
    fn test_toml_parsing_multiple_header_configs() {
        let toml_str = indoc! {r#"
            [[headers]]
            header = "Authorization"

            [headers.rule]
            type = "matches"
            pattern = "^Bearer [A-Za-z0-9-._~+/]+=*$"

            [[headers]]
            header = "X-API-Key"

            [headers.rule]
            type = "exists"

            [[headers]]
            header = "X-Hub-Signature-256"

            [headers.rule]
            type = "signature"
            token = "webhook-secret"
            signature_prefix = "sha256="
            "#};

        let configs: HeaderConfigs = toml::from_str(toml_str).unwrap();
        assert_eq!(configs.headers.len(), 3);

        // Validate Authorization header
        assert_eq!(configs.headers[0].header, "Authorization");
        match &configs.headers[0].rule {
            Rule::Matches { pattern } => {
                assert_eq!(pattern, "^Bearer [A-Za-z0-9-._~+/]+=*$");
            }
            _ => panic!("Expected matches rule"),
        }

        // Validate X-API-Key header
        assert_eq!(configs.headers[1].header, "X-API-Key");
        match &configs.headers[1].rule {
            Rule::Exists => {}
            _ => panic!("Expected exists rule"),
        }

        // Validate signature header
        assert_eq!(configs.headers[2].header, "X-Hub-Signature-256");
        match &configs.headers[2].rule {
            Rule::Signature { signature_prefix, .. } => {
                assert_eq!(signature_prefix, &Some("sha256=".to_string()));
            }
            _ => panic!("Expected signature rule"),
        }
    }
}
