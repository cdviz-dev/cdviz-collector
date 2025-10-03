//! VRL functions for working with Package URLs (PURLs)
//!
//! This module provides VRL functions for parsing and building Package URLs
//! according to the PURL specification: <https://github.com/package-url/purl-spec>
//!
//! # PURL Object Structure
//!
//! All functions work with VRL objects that match the PURL spec:
//!
//! ```json
//! {
//!   "type": "oci",
//!   "namespace": "owner/subnamespace",
//!   "name": "package-name",
//!   "version": "1.0.0",
//!   "qualifiers": {
//!     "repository_url": "ghcr.io/owner",
//!     "tag": "v1.0"
//!   },
//!   "subpath": "path/to/resource"
//! }
//! ```
//!
//! # Functions
//!
//! - `purl_to_string(object)` - Convert PURL object to spec-compliant string
//! - `purl_from_oci_image(string)` - Parse OCI/Docker image string to PURL object

use packageurl::PackageUrl;
use std::collections::BTreeMap;
use vrl::compiler::prelude::*;
use vrl::value::KeyString;

// ============================================================================
// Core Logic
// ============================================================================

/// Parse an OCI/Docker image string into a PURL object structure
///
/// Handles various image formats:
/// - `nginx` → `docker.io/library/nginx:latest`
/// - `nginx:1.21` → `docker.io/library/nginx:1.21`
/// - `nginx@sha256:abc` → digest-based reference
/// - `ghcr.io/owner/image:tag@sha256:abc` → fully qualified with both tag and digest
///
/// Returns a VRL Value object with PURL structure
fn parse_oci_image_to_purl(image_str: &str) -> Value {
    let mut digest = String::new();
    let mut tag = String::new();
    let mut repo_and_name = image_str.to_string();

    // Extract digest (@sha256:...)
    if let Some(at_pos) = image_str.find("@sha256:") {
        repo_and_name = image_str[..at_pos].to_string();
        digest = image_str[at_pos + 1..].to_string(); // Skip '@', keep 'sha256:...'
    } else if let Some(at_pos) = image_str.find('@') {
        repo_and_name = image_str[..at_pos].to_string();
        let digest_part = &image_str[at_pos + 1..];
        // If digest doesn't have algorithm prefix, add it
        digest = if digest_part.starts_with("sha256:") {
            digest_part.to_string()
        } else {
            format!("sha256:{digest_part}")
        };
    }

    // Extract tag (last :xxx that's not part of a hostname)
    if let Some(colon_pos) = repo_and_name.rfind(':') {
        let potential_tag = &repo_and_name[colon_pos + 1..];
        // Only treat as tag if it doesn't contain '/' (not part of registry URL)
        if !potential_tag.contains('/') {
            tag = potential_tag.to_string();
            repo_and_name = repo_and_name[..colon_pos].to_string();
        }
    }

    // Default tag to "latest" if no digest and no tag
    if tag.is_empty() && digest.is_empty() {
        tag = "latest".to_string();
    }

    // Parse repository path and image name
    let parts: Vec<&str> = repo_and_name.split('/').collect();
    let name = (*parts.last().unwrap_or(&"unknown")).to_string();
    let repo_parts = &parts[..parts.len().saturating_sub(1)];

    // Determine registry and namespace
    let (registry, namespace) = if repo_parts.is_empty() {
        // Short name like "nginx" → docker.io/library/nginx
        ("docker.io".to_string(), "library".to_string())
    } else if !repo_parts[0].contains('.') && repo_parts[0] != "localhost" {
        // docker.io implicit, e.g., "owner/image" → docker.io/owner/image
        ("docker.io".to_string(), repo_parts.join("/"))
    } else {
        // Explicit registry, e.g., "ghcr.io/owner/image"
        (repo_parts[0].to_string(), repo_parts[1..].join("/"))
    };

    // Build repository URL for qualifiers
    let repository_url =
        if namespace.is_empty() { registry.clone() } else { format!("{registry}/{namespace}") };

    // Build qualifiers
    let mut qualifiers = BTreeMap::new();
    qualifiers.insert(KeyString::from("repository_url"), Value::from(repository_url));
    if !tag.is_empty() {
        qualifiers.insert(KeyString::from("tag"), Value::from(tag));
    }

    // Build version (digest-based if available)
    let version = if digest.is_empty() { Value::Null } else { Value::from(format!("@{digest}")) };

    // Build PURL object
    let mut purl_obj = BTreeMap::new();
    purl_obj.insert(KeyString::from("type"), Value::from("oci"));
    // if !namespace.is_empty() {
    //     purl_obj.insert(KeyString::from("namespace"), Value::from(namespace));
    // }
    purl_obj.insert(KeyString::from("name"), Value::from(name));
    purl_obj.insert(KeyString::from("version"), version);
    purl_obj.insert(KeyString::from("qualifiers"), Value::from(qualifiers));
    purl_obj.insert(KeyString::from("subpath"), Value::Null);

    Value::from(purl_obj)
}

/// Convert a PURL object to a spec-compliant PURL string
///
/// Uses the packageurl crate to ensure proper encoding and formatting
fn purl_object_to_string(purl_obj: &Value) -> Result<String, ExpressionError> {
    let obj = purl_obj.as_object().ok_or("purl_to_string requires an object")?;

    // Extract required fields
    let pkg_type =
        obj.get("type").and_then(Value::as_str).ok_or("PURL object must have a 'type' field")?;

    let name =
        obj.get("name").and_then(Value::as_str).ok_or("PURL object must have a 'name' field")?;

    // Extract optional fields (convert Cow to owned String to avoid lifetime issues)
    let namespace = obj.get("namespace").and_then(Value::as_str).map(|s| s.to_string());

    let version = obj.get("version").and_then(|v| match v {
        Value::Null => None,
        Value::Bytes(b) => std::str::from_utf8(b).ok().map(std::string::ToString::to_string),
        _ => v.as_str().map(|s| s.to_string()),
    });

    let subpath = obj.get("subpath").and_then(Value::as_str).map(|s| s.to_string());

    // Extract qualifiers
    let qualifiers: Vec<(String, String)> = obj
        .get("qualifiers")
        .and_then(|v| v.as_object())
        .map(|q| {
            q.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.to_string(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    // Build PackageUrl
    let mut purl =
        PackageUrl::new(pkg_type, name).map_err(|e| format!("Invalid PURL type or name: {e}"))?;

    if let Some(ref ns) = namespace {
        purl.with_namespace(ns.as_str());
    }

    if let Some(ref ver) = version {
        purl.with_version(ver.as_str());
    }

    if let Some(ref sp) = subpath {
        purl.with_subpath(sp.as_str()).map_err(|e| format!("Invalid subpath: {e}"))?;
    }

    for (key, value) in qualifiers {
        purl.add_qualifier(key, value).map_err(|e| format!("Invalid qualifier: {e}"))?;
    }

    Ok(purl.to_string())
}

// ============================================================================
// VRL Function: purl_from_oci_image
// ============================================================================

#[derive(Clone, Copy, Debug)]
pub struct PurlFromOciImage;

impl Function for PurlFromOciImage {
    fn identifier(&self) -> &'static str {
        "purl_from_oci_image"
    }

    fn parameters(&self) -> &'static [Parameter] {
        &[Parameter { keyword: "image", kind: kind::BYTES, required: true }]
    }

    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                title: "parse short image name",
                source: r#"purl_from_oci_image("nginx")"#,
                result: Ok(
                    r#"{"type": "oci", "namespace": "library", "name": "nginx", "version": null, "qualifiers": {"repository_url": "docker.io/library", "tag": "latest"}, "subpath": null}"#,
                ),
            },
            Example {
                title: "parse image with tag",
                source: r#"purl_from_oci_image("nginx:1.21")"#,
                result: Ok(
                    r#"{"type": "oci", "namespace": "library", "name": "nginx", "version": null, "qualifiers": {"repository_url": "docker.io/library", "tag": "1.21"}, "subpath": null}"#,
                ),
            },
            Example {
                title: "parse image with digest",
                source: r#"purl_from_oci_image("nginx@sha256:abc123")"#,
                result: Ok(
                    r#"{"type": "oci", "namespace": "library", "name": "nginx", "version": "@sha256:abc123", "qualifiers": {"repository_url": "docker.io/library"}, "subpath": null}"#,
                ),
            },
            Example {
                title: "parse fully qualified image",
                source: r#"purl_from_oci_image("ghcr.io/owner/image:v1.0@sha256:abc123")"#,
                result: Ok(
                    r#"{"type": "oci", "namespace": "owner", "name": "image", "version": "@sha256:abc123", "qualifiers": {"repository_url": "ghcr.io/owner", "tag": "v1.0"}, "subpath": null}"#,
                ),
            },
        ]
    }

    fn compile(
        &self,
        _state: &state::TypeState,
        _ctx: &mut FunctionCompileContext,
        arguments: ArgumentList,
    ) -> Compiled {
        let image = arguments.required("image");

        Ok(PurlFromOciImageFn { image }.as_expr())
    }
}

#[derive(Debug, Clone)]
struct PurlFromOciImageFn {
    image: Box<dyn Expression>,
}

impl FunctionExpression for PurlFromOciImageFn {
    fn resolve(&self, ctx: &mut Context) -> Resolved {
        let image = self.image.resolve(ctx)?;
        let image_str = image.try_bytes_utf8_lossy()?;

        Ok(parse_oci_image_to_purl(image_str.as_ref()))
    }

    fn type_def(&self, _state: &state::TypeState) -> TypeDef {
        TypeDef::object(Collection::from_unknown(Kind::any()))
    }
}

// ============================================================================
// VRL Function: purl_to_string
// ============================================================================

#[derive(Clone, Copy, Debug)]
pub struct PurlToString;

impl Function for PurlToString {
    fn identifier(&self) -> &'static str {
        "purl_to_string"
    }

    fn parameters(&self) -> &'static [Parameter] {
        &[Parameter { keyword: "purl_object", kind: kind::OBJECT, required: true }]
    }

    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                title: "convert OCI PURL object to string",
                source: r#"purl_to_string({"type": "oci", "name": "nginx", "version": "@sha256:abc", "qualifiers": {"repository_url": "docker.io/library", "tag": "1.21"}})"#,
                result: Ok(r"pkg:oci/nginx@sha256:abc?repository_url=docker.io%2Flibrary&tag=1.21"),
            },
            Example {
                title: "convert Helm PURL object to string",
                source: r#"purl_to_string({"type": "helm", "name": "wordpress", "version": "15.2.35", "qualifiers": {"repository_url": "https://charts.bitnami.com/bitnami"}})"#,
                result: Ok(
                    r"pkg:helm/wordpress@15.2.35?repository_url=https%3A%2F%2Fcharts.bitnami.com%2Fbitnami",
                ),
            },
        ]
    }

    fn compile(
        &self,
        _state: &state::TypeState,
        _ctx: &mut FunctionCompileContext,
        arguments: ArgumentList,
    ) -> Compiled {
        let purl_object = arguments.required("purl_object");

        Ok(PurlToStringFn { purl_object }.as_expr())
    }
}

#[derive(Debug, Clone)]
struct PurlToStringFn {
    purl_object: Box<dyn Expression>,
}

impl FunctionExpression for PurlToStringFn {
    fn resolve(&self, ctx: &mut Context) -> Resolved {
        let purl_obj = self.purl_object.resolve(ctx)?;
        let purl_string = purl_object_to_string(&purl_obj)?;

        Ok(Value::from(purl_string))
    }

    fn type_def(&self, _state: &state::TypeState) -> TypeDef {
        TypeDef::bytes().fallible()
    }
}

// ============================================================================
// Public API
// ============================================================================

/// Returns all custom VRL functions for PURL handling
pub fn all_custom_functions() -> Vec<Box<dyn Function>> {
    vec![Box::new(PurlFromOciImage), Box::new(PurlToString)]
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_oci_image_short_name() {
        let result = parse_oci_image_to_purl("nginx");
        let obj = result.as_object().unwrap();

        assert_eq!(obj.get("type").unwrap().as_str().unwrap(), "oci");
        assert_eq!(obj.get("name").unwrap().as_str().unwrap(), "nginx");
        assert_eq!(obj.get("namespace"), None);

        let qualifiers = obj.get("qualifiers").unwrap().as_object().unwrap();
        assert_eq!(
            qualifiers.get("repository_url").unwrap().as_str().unwrap(),
            "docker.io/library"
        );
        assert_eq!(qualifiers.get("tag").unwrap().as_str().unwrap(), "latest");
    }

    #[test]
    fn test_parse_oci_image_with_tag() {
        let result = parse_oci_image_to_purl("nginx:1.21");
        let obj = result.as_object().unwrap();

        assert_eq!(obj.get("name").unwrap().as_str().unwrap(), "nginx");

        let qualifiers = obj.get("qualifiers").unwrap().as_object().unwrap();
        assert_eq!(qualifiers.get("tag").unwrap().as_str().unwrap(), "1.21");
    }

    #[test]
    fn test_parse_oci_image_with_digest() {
        let result = parse_oci_image_to_purl("nginx@sha256:abc123");
        let obj = result.as_object().unwrap();

        assert_eq!(obj.get("name").unwrap().as_str().unwrap(), "nginx");
        assert_eq!(obj.get("version").unwrap().as_str().unwrap(), "@sha256:abc123");

        let qualifiers = obj.get("qualifiers").unwrap().as_object().unwrap();
        assert!(!qualifiers.contains_key("tag"));
    }

    #[test]
    fn test_parse_oci_image_fully_qualified() {
        let result = parse_oci_image_to_purl("ghcr.io/owner/image:v1.0@sha256:abc123");
        let obj = result.as_object().unwrap();

        assert_eq!(obj.get("type").unwrap().as_str().unwrap(), "oci");
        assert_eq!(obj.get("namespace"), None);
        assert_eq!(obj.get("name").unwrap().as_str().unwrap(), "image");
        assert_eq!(obj.get("version").unwrap().as_str().unwrap(), "@sha256:abc123");

        let qualifiers = obj.get("qualifiers").unwrap().as_object().unwrap();
        assert_eq!(qualifiers.get("repository_url").unwrap().as_str().unwrap(), "ghcr.io/owner");
        assert_eq!(qualifiers.get("tag").unwrap().as_str().unwrap(), "v1.0");
    }

    #[test]
    fn test_parse_oci_image_nested_namespace() {
        let result = parse_oci_image_to_purl("ghcr.io/org/team/image:latest");
        let obj = result.as_object().unwrap();

        assert_eq!(obj.get("namespace"), None);
        assert_eq!(obj.get("name").unwrap().as_str().unwrap(), "image");

        let qualifiers = obj.get("qualifiers").unwrap().as_object().unwrap();
        assert_eq!(qualifiers.get("repository_url").unwrap().as_str().unwrap(), "ghcr.io/org/team");
    }

    #[test]
    fn test_purl_object_to_string_oci() {
        let mut qualifiers = BTreeMap::new();
        qualifiers.insert(KeyString::from("repository_url"), Value::from("docker.io/library"));
        qualifiers.insert(KeyString::from("tag"), Value::from("1.21"));

        let mut purl_obj = BTreeMap::new();
        purl_obj.insert(KeyString::from("type"), Value::from("oci"));
        purl_obj.insert(KeyString::from("namespace"), Value::from("library"));
        purl_obj.insert(KeyString::from("name"), Value::from("nginx"));
        purl_obj.insert(KeyString::from("version"), Value::from("@sha256:abc"));
        purl_obj.insert(KeyString::from("qualifiers"), Value::from(qualifiers));
        purl_obj.insert(KeyString::from("subpath"), Value::Null);

        let result = purl_object_to_string(&Value::from(purl_obj)).unwrap();

        // PackageUrl sorts qualifiers alphabetically and encodes @ in version
        assert!(result.starts_with("pkg:oci/library/nginx@%40sha256:abc?"));
        assert!(result.contains("repository_url=docker.io/library"));
        assert!(result.contains("tag=1.21"));
    }

    #[test]
    fn test_purl_object_to_string_helm() {
        let mut qualifiers = BTreeMap::new();
        qualifiers.insert(
            KeyString::from("repository_url"),
            Value::from("https://charts.bitnami.com/bitnami"),
        );

        let mut purl_obj = BTreeMap::new();
        purl_obj.insert(KeyString::from("type"), Value::from("helm"));
        purl_obj.insert(KeyString::from("name"), Value::from("wordpress"));
        purl_obj.insert(KeyString::from("version"), Value::from("15.2.35"));
        purl_obj.insert(KeyString::from("qualifiers"), Value::from(qualifiers));

        let result = purl_object_to_string(&Value::from(purl_obj)).unwrap();

        assert!(result.starts_with("pkg:helm/wordpress@15.2.35?"));
        assert!(result.contains("repository_url=https://charts.bitnami.com/bitnami"));
    }

    #[test]
    fn test_purl_object_to_string_minimal() {
        let mut purl_obj = BTreeMap::new();
        purl_obj.insert(KeyString::from("type"), Value::from("generic"));
        purl_obj.insert(KeyString::from("name"), Value::from("package"));

        let result = purl_object_to_string(&Value::from(purl_obj)).unwrap();

        assert_eq!(result, "pkg:generic/package");
    }
}
