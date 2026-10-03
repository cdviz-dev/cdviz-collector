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
#[serde(deny_unknown_fields)]
pub struct OutgoingHeaderConfig {
    /// The header name (consistent with `HeaderRuleConfig`)
    pub header: String,
    /// How to generate the header value (consistent with `HeaderRuleConfig` structure)
    pub rule: HeaderSource,
}

/// Different ways to generate header values
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum HeaderSource {
    /// Static header value
    #[serde(rename = "static")]
    Static {
        value: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        prefix: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        suffix: String,
    },

    /// Secret header value (e.g., from environment)
    #[serde(rename = "secret")]
    Secret {
        #[serde(skip_serializing)]
        value: SecretString,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        prefix: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        suffix: String,
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
        HeaderSource::Static { value, prefix, suffix } => super::enclose(value, prefix, suffix),
        HeaderSource::Secret { value, prefix, suffix } => {
            super::enclose(value.expose_secret(), prefix, suffix)
        }
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

/// Same origin = same scheme, host and port (an unknown default port counts as different).
pub(crate) fn is_cross_origin(a: &url::Url, b: &url::Url) -> bool {
    if a.scheme() != b.scheme() || a.host() != b.host() {
        return true;
    }
    match (a.port_or_known_default(), b.port_or_known_default()) {
        (Some(pa), Some(pb)) => pa != pb,
        _ => true,
    }
}

/// Compile `trusted_redirect_hosts` glob patterns (e.g. `"*.example.com"`), matched
/// case-insensitively against the redirect target's host.
pub(crate) fn trusted_hosts(patterns: &[String]) -> crate::errors::Result<globset::GlobSet> {
    use crate::errors::IntoDiagnostic;
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            globset::GlobBuilder::new(pattern).case_insensitive(true).build().into_diagnostic()?,
        );
    }
    builder.build().into_diagnostic()
}

/// Whether a redirect from `from` to `to` may carry the configured headers: same origin, or a
/// host listed in `trusted_redirect_hosts` (without an https → http downgrade).
pub(crate) fn may_forward_headers(
    from: &url::Url,
    to: &url::Url,
    trusted: &globset::GlobSet,
) -> bool {
    !is_cross_origin(from, to)
        || (to.host_str().is_some_and(|host| trusted.is_match(host))
            && !(from.scheme() == "https" && to.scheme() != "https"))
}

/// Redirect policy for a client sending configured `headers` (API keys, signatures).
///
/// reqwest's default policy only strips `Authorization`/`Cookie`/`Proxy-Authorization` on a
/// cross-origin redirect, so a configured `X-API-Key` would be replayed to whatever host the
/// server redirects to. With configured headers, same-origin redirects and redirects to a
/// `trusted` host are followed (they keep the headers); any other is not: the 3xx is returned
/// to the caller. Note: reqwest itself still drops `Authorization`/`Cookie` on any cross-host
/// redirect, so only custom headers (e.g. `X-API-Key`) reach a trusted host.
#[cfg(any(feature = "sink_http", feature = "source_sse"))]
pub(crate) fn redirect_policy(
    has_configured_headers: bool,
    trusted: globset::GlobSet,
) -> reqwest::redirect::Policy {
    if !has_configured_headers {
        return reqwest::redirect::Policy::default();
    }
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() > 10 {
            return attempt.error("too many redirects");
        }
        let allowed = attempt
            .previous()
            .first()
            .is_none_or(|origin| may_forward_headers(origin, attempt.url(), &trusted));
        if allowed {
            attempt.follow()
        } else {
            tracing::warn!(
                location = %attempt.url(),
                "not following cross-origin redirect: it would forward the configured headers; \
                 add its host to `trusted_redirect_hosts` or update the configured URL"
            );
            attempt.stop()
        }
    })
}

/// Convert an HTTP [`HeaderMap`] to a `HashMap<String, String>`, keeping only
/// headers that appear in `headers_to_keep` and are not marked sensitive.
///
/// Used by HTTP-based sources (e.g. webhook) to extract forwarded headers from
/// incoming requests before passing them into the pipeline.
#[allow(clippy::min_ident_chars)]
pub(crate) fn filter_http_headers(
    headers: &HeaderMap,
    headers_to_keep: &[HeaderName],
) -> HashMap<String, String> {
    headers
        .iter()
        .filter(|(_, v)| !v.is_sensitive())
        .filter(|(k, _)| headers_to_keep.contains(k))
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), v.to_string())))
        .collect()
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
            SimpleHeaderValue::Simple { value } => {
                HeaderSource::Static { value, prefix: String::new(), suffix: String::new() }
            }
            SimpleHeaderValue::Complex(source) => source,
        };

        OutgoingHeaderConfig { header: simple.header, rule: source }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn may_forward_headers_same_origin_or_trusted_host_only() {
        let url = |s: &str| url::Url::parse(s).unwrap();
        let trusted = trusted_hosts(&["*.example.com".to_string()]).unwrap();
        let from = url("https://api.example.com/a");
        assert!(may_forward_headers(&from, &url("https://api.example.com/b"), &trusted));
        assert!(may_forward_headers(&from, &url("https://CDN.Example.com/b"), &trusted));
        assert!(!may_forward_headers(&from, &url("https://example.com/b"), &trusted));
        assert!(!may_forward_headers(&from, &url("https://evil.test/b"), &trusted));
        assert!(
            !may_forward_headers(&from, &url("http://cdn.example.com/b"), &trusted),
            "downgrade"
        );
        assert!(trusted_hosts(&["[".to_string()]).is_err());
    }

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
            rule: HeaderSource::Static {
                value: "Bearer token123".to_string(),
                prefix: String::new(),
                suffix: String::new(),
            },
        }];

        let headers = generate_headers(&configs, None).unwrap();
        assert_eq!(headers.get("Authorization").unwrap(), "Bearer token123");
    }

    #[test]
    fn test_secret_header_generation() {
        let configs = vec![OutgoingHeaderConfig {
            header: "X-API-Key".to_string(),
            rule: HeaderSource::Secret {
                value: "secret123".into(),
                prefix: String::new(),
                suffix: String::new(),
            },
        }];

        let headers = generate_headers(&configs, None).unwrap();
        assert_eq!(headers.get("X-API-Key").unwrap(), "secret123");
    }

    #[test]
    fn test_static_header_with_prefix() {
        let configs = vec![OutgoingHeaderConfig {
            header: "Authorization".to_string(),
            rule: HeaderSource::Static {
                value: "token123".to_string(),
                prefix: "Bearer ".to_string(),
                suffix: String::new(),
            },
        }];

        let headers = generate_headers(&configs, None).unwrap();
        assert_eq!(headers.get("Authorization").unwrap(), "Bearer token123");
    }

    #[test]
    fn test_secret_header_with_prefix_and_suffix() {
        let configs = vec![OutgoingHeaderConfig {
            header: "X-API-Key".to_string(),
            rule: HeaderSource::Secret {
                value: "secret".into(),
                prefix: "[".to_string(),
                suffix: "]".to_string(),
            },
        }];

        let headers = generate_headers(&configs, None).unwrap();
        assert_eq!(headers.get("X-API-Key").unwrap(), "[secret]");
    }

    #[test]
    fn test_toml_parsing_secret_with_prefix() {
        use secrecy::ExposeSecret;

        let toml_str = indoc! {r#"
            header = "Authorization"

            [rule]
            type = "secret"
            value = "ghp_token"
            prefix = "Bearer "
            "#};

        let config: OutgoingHeaderConfig = toml::from_str(toml_str).unwrap();
        match config.rule {
            HeaderSource::Secret { value, prefix, suffix } => {
                assert_eq!(value.expose_secret(), "ghp_token");
                assert_eq!(prefix, "Bearer ");
                assert_eq!(suffix, "");
            }
            _ => panic!("Expected secret header source"),
        }
    }

    #[test]
    fn test_invalid_header_name() {
        let configs = vec![OutgoingHeaderConfig {
            header: "Invalid Header Name".to_string(), // Spaces not allowed
            rule: HeaderSource::Static {
                value: "value".to_string(),
                prefix: String::new(),
                suffix: String::new(),
            },
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
            HeaderSource::Static { value, .. } => assert_eq!(value, "Bearer token"),
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
            HeaderSource::Static { value, .. } => {
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
            HeaderSource::Secret { value, .. } => {
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
            HeaderSource::Static { value, .. } => {
                assert_eq!(value, "Bearer static-token");
            }
            _ => panic!("Expected static header source"),
        }

        // Validate X-API-Key header
        assert_eq!(configs.headers[1].header, "X-API-Key");
        match &configs.headers[1].rule {
            HeaderSource::Secret { value, .. } => {
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
            HeaderSource::Static { value, .. } => {
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
            rule: HeaderSource::Static {
                value: "static-value".to_string(),
                prefix: String::new(),
                suffix: String::new(),
            },
        };

        let serialized = toml::to_string(&original_config).unwrap();
        let deserialized: OutgoingHeaderConfig = toml::from_str(&serialized).unwrap();

        assert_eq!(original_config.header, deserialized.header);
        match (&original_config.rule, &deserialized.rule) {
            (
                HeaderSource::Static { value: orig, .. },
                HeaderSource::Static { value: deser, .. },
            ) => {
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
            HeaderSource::Static {
                value: "Bearer token".to_string(),
                prefix: String::new(),
                suffix: String::new(),
            },
        );
        map.insert(
            "X-API-Key".to_string(),
            HeaderSource::Secret {
                value: "secret123".into(),
                prefix: String::new(),
                suffix: String::new(),
            },
        );

        let configs = outgoing_header_map_to_configs(&map);
        assert_eq!(configs.len(), 2);

        // Find the authorization header
        let auth_header = configs.iter().find(|h| h.header == "Authorization").unwrap();
        match &auth_header.rule {
            HeaderSource::Static { value, .. } => assert_eq!(value, "Bearer token"),
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
            HeaderSource::Static { value, .. } => assert_eq!(value, "Bearer token123"),
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

    #[test]
    fn test_filter_http_headers_with_valid_headers() {
        use std::str::FromStr;
        let mut headers = HeaderMap::new();
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));
        headers.insert("X-Custom-Header", HeaderValue::from_static("custom_value"));
        headers.insert("Authorization", HeaderValue::from_static("Bearer token"));

        let headers_to_keep = vec![
            HeaderName::from_str("Content-Type").unwrap(),
            HeaderName::from_str("X-Custom-Header").unwrap(),
        ];

        let result = filter_http_headers(&headers, &headers_to_keep);
        let mut expected = HashMap::new();
        expected.insert("content-type".to_string(), "application/json".to_string());
        expected.insert("x-custom-header".to_string(), "custom_value".to_string());
        assert_eq!(result, expected);
    }

    #[test]
    fn test_filter_http_headers_excludes_sensitive() {
        use std::str::FromStr;
        let mut headers = HeaderMap::new();
        let mut authorization = HeaderValue::from_static("Bearer token");
        authorization.set_sensitive(true);
        headers.insert("Authorization", authorization);
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));

        let headers_to_keep = vec![
            HeaderName::from_str("Authorization").unwrap(),
            HeaderName::from_str("Content-Type").unwrap(),
        ];

        let result = filter_http_headers(&headers, &headers_to_keep);
        let mut expected = HashMap::new();
        expected.insert("content-type".to_string(), "application/json".to_string());
        // Authorization is sensitive and must be excluded
        assert_eq!(result, expected);
    }

    #[test]
    fn test_filter_http_headers_empty_map() {
        use std::str::FromStr;
        let headers = HeaderMap::new();
        let headers_to_keep = vec![
            HeaderName::from_str("Content-Type").unwrap(),
            HeaderName::from_str("X-Custom-Header").unwrap(),
        ];

        let result = filter_http_headers(&headers, &headers_to_keep);
        assert!(result.is_empty());
    }

    #[test]
    fn test_filter_http_headers_no_keeplist_match() {
        use std::str::FromStr;
        let mut headers = HeaderMap::new();
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));

        let headers_to_keep = vec![HeaderName::from_str("X-Custom-Header").unwrap()];

        let result = filter_http_headers(&headers, &headers_to_keep);
        assert!(result.is_empty());
    }
}
