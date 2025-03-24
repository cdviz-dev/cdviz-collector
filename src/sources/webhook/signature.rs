use std::string::FromUtf8Error;

use axum::{
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::{Bytes, BytesMut};
use faster_hex::{hex_decode, hex_string};
//use futures::future::BoxFuture;
//use futures::future::TryFutureExt;
use hmac::{
    Hmac, Mac,
    digest::{InvalidLength, MacError},
};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
//use std::task::{Context, Poll};
//use tower::{Layer, Service};

// Create alias for HMAC-SHA256
type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Deserialize)]
pub struct SignatureConfig {
    /// The header name of the signature to check
    header: String,
    /// The token used to sign the request (hmac-sha256)
    #[serde(default)]
    token: SecretString,
    /// Encoding of the token (how bytes are encoded in chars)
    /// If not set the bytes of the token are used.
    #[serde(default)]
    token_encoding: Option<Encoding>,
    /// The prefix of the signature. If not set, the signature is not prefixed.
    #[serde(default)]
    signature_prefix: Option<String>,
    /// On which part of the request the signature is computed
    #[serde(default)]
    signature_on: SignatureOn,
    /// Encoding of the signature (how bytes are encoded in chars)
    #[serde(default)]
    signature_encoding: Encoding,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[cfg_attr(test, derive(test_strategy::Arbitrary))]
//#[serde(untagged)]
pub enum SignatureOn {
    #[serde(rename = "body")]
    #[default]
    Body,
    #[serde(rename = "headers_then_body")]
    HeadersThenBody { separator: String, headers: Vec<String> },
}

#[derive(Debug, Clone, Deserialize, Default)]
#[cfg_attr(test, derive(test_strategy::Arbitrary))]
pub enum Encoding {
    /// base64 encoding (with padding and case sensitive)
    #[serde(rename = "base64")]
    Base64,
    #[serde(rename = "hex")]
    #[default]
    Hex,
}

pub(crate) fn build_signature(
    config: &SignatureConfig,
    http_headers: &HeaderMap,
    http_body: &[u8],
) -> Result<String, SignatureError> {
    let token = config.token.expose_secret();
    let token = match &config.token_encoding {
        Some(Encoding::Base64) => STANDARD.decode(token.as_bytes())?,
        Some(Encoding::Hex) => {
            let mut dst = Vec::with_capacity(token.len() * 2);
            hex_decode(token.as_bytes(), &mut dst)?;
            dst
        }
        None => token.as_bytes().to_vec(),
    };
    let payload_prefix = match &config.signature_on {
        SignatureOn::Body => None,
        SignatureOn::HeadersThenBody { separator, headers } => {
            let mut payload_prefix = BytesMut::new();
            let separator = separator.to_string().into_bytes();
            for header in headers {
                if let Some(value) = http_headers.get(header) {
                    payload_prefix.extend_from_slice(value.as_bytes());
                    payload_prefix.extend_from_slice(&separator);
                }
            }
            Some(payload_prefix.freeze())
        }
    };

    let mut mac = HmacSha256::new_from_slice(&token)?;
    if let Some(prefix) = payload_prefix {
        mac.update(&prefix);
    }
    mac.update(http_body);
    let result = mac.finalize();
    let mut signature = match config.signature_encoding {
        Encoding::Base64 => STANDARD.encode(&result.into_bytes()[..]),
        Encoding::Hex => hex_string(&result.into_bytes()[..]),
    };
    if let Some(prefix) = &config.signature_prefix {
        signature = format!("{prefix}{signature}");
    }
    Ok(signature)
}

/// Check if the signature is valid.
/// Use hmac-sha256 algorithm + token as secret.
/// The signature is in the header and the value can be prefixed with the `value_prefix`.
/// return an Error if the signature is invalid or does not exist or doesn't start by the `value_prefix`.
pub(crate) fn check_signature(
    config: &SignatureConfig,
    http_headers: &HeaderMap,
    http_body: &Bytes,
) -> Result<(), SignatureError> {
    let signature = http_headers.get(config.header.as_str()).and_then(|value| value.to_str().ok());
    if signature.is_none() {
        return Err(SignatureError::SignatureNotFound);
    }

    let expected_signature = build_signature(config, http_headers, http_body)?;
    if expected_signature != signature.unwrap_or_default() {
        return Err(SignatureError::VerificationMismatch);
    }
    Ok(())
}

#[derive(Debug, derive_more::Error, derive_more::Display, derive_more::From)]
#[non_exhaustive]
pub(crate) enum SignatureError {
    #[display("signature not found or readable")]
    SignatureNotFound,
    #[display("failed to encode utf8")]
    XcodingUtf8Failed(FromUtf8Error),
    #[display("failed to encode/decode hex")]
    XcodingHexFailed(faster_hex::Error),
    #[display("failed to encode/decode base64")]
    XcodingBase64Failed(base64::DecodeError),
    #[display("Signature verification mismatch")]
    VerificationMismatch,
    #[display("failed to verify signature")]
    VerificationFailed(MacError),
    #[display("failed to used token/key")]
    HmacCreationFailed(InvalidLength),
    #[cfg(test)]
    #[display("failed to read body")]
    BodyReadFailed,
}

// try to follow [RFC 9457: Problem Details for HTTP APIs](https://www.rfc-editor.org/rfc/rfc9457.html)
impl IntoResponse for SignatureError {
    fn into_response(self) -> Response {
        use axum::Json;
        use tracing_opentelemetry_instrumentation_sdk::find_current_trace_id;
        match self {
            SignatureError::HmacCreationFailed(err) => {
                let trace_id = find_current_trace_id();
                tracing::warn!(?err, "invalid length on token/key");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "title": StatusCode::INTERNAL_SERVER_ERROR.as_str(),
                        "status": StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                        "detail": err.to_string(),
                        "trace_id": trace_id,
                    })),
                )
                    .into_response()
            }
            err => (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "title": "Invalid Signature",
                    "status": StatusCode::UNAUTHORIZED.as_u16(),
                    "detail": err.to_string(),
                })),
            )
                .into_response(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::min_ident_chars)]
mod tests {

    use super::*;
    use assert2::let_assert;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use pretty_assertions::assert_eq;
    use test_strategy::proptest;
    use toml;

    /// Check if the signature is valid.
    /// Use hmac-sha256 algorithm + token as secret.
    /// The signature is in the header and the value can be prefixed with the `value_prefix`.
    /// return an Error if the signature is invalid or does not exist or doesn't start by the `value_prefix`.
    async fn check_signature_on_request(
        request: Request<Body>,
        config: &SignatureConfig,
    ) -> Result<Request<Body>, SignatureError> {
        let (parts, body) = request.into_parts();
        let bytes = to_bytes(body, usize::MAX).await.map_err(|_| SignatureError::BodyReadFailed)?;
        check_signature(config, &parts.headers, &bytes)?;
        Ok(Request::from_parts(parts, Body::from(bytes)))
    }

    #[proptest(async = "tokio", cases = 10)]
    async fn test_check_use_build_signature(
        #[any] body_str: String,
        #[any] signature_encoding: Encoding,
        #[strategy("[a-zA-Z0-9_=-]{0,10}")] signature_prefix: String,
    ) {
        if body_str.is_empty() {
            return Ok(());
        }
        let config = SignatureConfig {
            header: "X-Signature".to_string(),
            token: "myToken".to_string().into(),
            token_encoding: None,
            signature_prefix: Some(signature_prefix),
            signature_on: SignatureOn::Body,
            signature_encoding,
        };
        let_assert!(
            Ok(signature) = build_signature(&config, &HeaderMap::new(), body_str.as_bytes())
        );
        let request = Request::builder()
            .uri("/webhook/test")
            .method("POST")
            .header("x-signature", signature.to_string())
            .body(Body::from(body_str))
            .unwrap();

        let_assert!(Ok(_) = check_signature_on_request(request, &config).await);
    }

    #[tokio::test]
    async fn test_signature_not_found() {
        let config = SignatureConfig {
            header: "X-Signature".to_string(),
            token: "mySecretToken".to_string().into(),
            token_encoding: None,
            signature_prefix: None,
            signature_on: SignatureOn::Body,
            signature_encoding: Encoding::Hex,
        };
        let body_str = r#"{"action":"in_progress","workflow_job":{ ... }}"#;
        let request = Request::builder()
            .uri("/webhook/test")
            .method("POST")
            //.header("x-signature", signature.to_string())
            .body(Body::from(body_str))
            .unwrap();

        let_assert!(
            Err(SignatureError::SignatureNotFound) =
                check_signature_on_request(request, &config).await
        );
    }

    #[tokio::test]
    async fn test_signature_mismatch() {
        let config = SignatureConfig {
            header: "X-Signature".to_string(),
            token: "mySecretToken".to_string().into(),
            token_encoding: None,
            signature_prefix: None,
            signature_on: SignatureOn::Body,
            signature_encoding: Encoding::Hex,
        };
        let body_str = r#"{"action":"in_progress","workflow_job":{ ... }}"#;
        let request = Request::builder()
            .uri("/webhook/test")
            .method("POST")
            .header("x-signature", "123456789".to_string())
            .body(Body::from(body_str))
            .unwrap();

        let_assert!(
            Err(SignatureError::VerificationMismatch) =
                check_signature_on_request(request, &config).await
        );
    }

    #[test]
    fn status_code_for_signature_error() {
        assert_eq!(
            StatusCode::UNAUTHORIZED,
            SignatureError::SignatureNotFound.into_response().status()
        );
        assert_eq!(
            StatusCode::UNAUTHORIZED,
            SignatureError::VerificationMismatch.into_response().status()
        );
        assert_eq!(
            StatusCode::INTERNAL_SERVER_ERROR,
            SignatureError::HmacCreationFailed(InvalidLength).into_response().status()
        );
    }

    #[test]
    fn test_check_manual_github_signature() {
        let config = SignatureConfig {
            header: "X-Hub-Signature-256".to_string(),
            token: "mySecretToken".to_string().into(),
            token_encoding: None,
            signature_prefix: Some("sha256=".to_string()),
            signature_on: SignatureOn::Body,
            signature_encoding: Encoding::Hex,
        };
        let body_str = r#"{"action":"in_progress","workflow_job":{ ... }}"#;
        let_assert!(
            Ok(signature) = build_signature(&config, &HeaderMap::new(), body_str.as_bytes())
        );
        assert_eq!(
            signature,
            "sha256=87e3e2d8cd7cb08800390443ccfcf9e287c0c2538467bff9068293ccf98fc264"
        );
    }

    /// unit test adapated from <https://docs.rs/svix/1.56.0/src/svix/webhooks.rs.html#247>
    #[test]
    fn test_check_manual_svix_signature() {
        let config = SignatureConfig {
            header: "X-Hub-Signature-256".to_string(),
            // from secret "whsec_C2FVsBQIhrscChlQIMV+b5sSYspob7oD"
            token: "C2FVsBQIhrscChlQIMV+b5sSYspob7oD".to_string().into(), //gitleaks:allow
            token_encoding: Some(Encoding::Base64),
            signature_prefix: Some("v1,".to_string()),
            signature_on: SignatureOn::HeadersThenBody {
                headers: vec!["svix-id".to_string(), "svix-timestamp".to_string()],
                separator: ".".to_string(),
            },
            signature_encoding: Encoding::Base64,
        };
        let body_str = r#"{"email":"test@example.com","username":"test_user"}"#;
        let headers = vec![
            ("svix-id", "msg_27UH4WbU6Z5A5EzD8u03UvzRbpk"), //gitleaks:allow
            ("svix-timestamp", "1649367553"),
            ("x-foo", "bar"),
        ]
        .into_iter()
        .map(|(k, v)| (k.parse().unwrap(), v.parse().unwrap()))
        .collect::<HeaderMap<_>>();
        let_assert!(Ok(signature) = build_signature(&config, &headers, body_str.as_bytes()));
        assert_eq!(signature, "v1,tZ1I4/hDygAJgO5TYxiSd6Sd0kDW6hPenDe+bTa3Kkw=");
    }

    #[test]
    fn test_parse_signature_on_headers_then_body() {
        let toml_str = r#"
            header = "X-Signature"
            token = "myToken"
            token_encoding = "base64"
            signature_prefix = "v1,"
            signature_on = { headers_then_body = {separator = ".", headers = ["header1", "header2"] }}
            signature_encoding = "hex"
        "#;

        let config: SignatureConfig = toml::from_str(toml_str).unwrap();

        assert_eq!(config.header, "X-Signature");
        assert_eq!(config.token.expose_secret(), "myToken");
        let_assert!(Some(Encoding::Base64) = config.token_encoding);
        assert_eq!(config.signature_prefix, Some("v1,".to_string()));
        let_assert!(Encoding::Hex = config.signature_encoding);
        let_assert!(SignatureOn::HeadersThenBody { separator, headers } = config.signature_on);
        assert_eq!(separator, ".");
        assert_eq!(headers, ["header1", "header2"]);
    }

    #[test]
    fn test_parse_signature_on_body() {
        let toml_str = r#"
            header = "X-Signature"
            token = "myToken"
            token_encoding = "hex"
            signature_on = "body"
            signature_encoding = "hex"
        "#;

        let config: SignatureConfig = toml::from_str(toml_str).unwrap();

        assert_eq!(config.header, "X-Signature");
        assert_eq!(config.token.expose_secret(), "myToken");
        let_assert!(Some(Encoding::Hex) = config.token_encoding);
        assert_eq!(config.signature_prefix, None);
        let_assert!(Encoding::Hex = config.signature_encoding);
        let_assert!(SignatureOn::Body = config.signature_on);
    }
}
