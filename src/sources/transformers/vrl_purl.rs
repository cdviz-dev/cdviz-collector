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

/// Detect if a repository URL is an OCI registry
fn is_oci_registry(repo_url: &str) -> bool {
    !repo_url.starts_with("http://") && !repo_url.starts_with("https://")
}

/// Parse an `ArgoCD` Helm source into a PURL object
///
/// Handles both OCI and HTTP Helm repositories:
/// - OCI registries → `pkg:oci/...`
/// - HTTP repositories → `pkg:generic/...?type=helm`
fn parse_argocd_helm_to_purl(repo_url: &str, chart: &str, target_revision: &str) -> Value {
    let mut qualifiers = BTreeMap::new();
    let pkg_type;

    if is_oci_registry(repo_url) {
        // OCI registry
        pkg_type = "oci";
        qualifiers.insert(KeyString::from("repository_url"), Value::from(repo_url));
    } else {
        // HTTP Helm repository
        pkg_type = "generic";
        qualifiers.insert(KeyString::from("download_url"), Value::from(repo_url));
        qualifiers.insert(KeyString::from("type"), Value::from("helm"));
    }

    let mut purl_obj = BTreeMap::new();
    purl_obj.insert(KeyString::from("type"), Value::from(pkg_type));
    purl_obj.insert(KeyString::from("name"), Value::from(chart));
    purl_obj.insert(KeyString::from("version"), Value::from(target_revision));
    purl_obj.insert(KeyString::from("qualifiers"), Value::from(qualifiers));
    purl_obj.insert(KeyString::from("subpath"), Value::Null);

    Value::from(purl_obj)
}

/// Detect Git hosting platform from repository URL
fn detect_git_platform(repo_url: &str) -> &str {
    let url_lower = repo_url.to_lowercase();

    if url_lower.contains("github.com") {
        "github"
    } else if url_lower.contains("bitbucket.org") {
        "bitbucket"
    } else {
        "generic"
    }
}

/// Extract owner and repository name from Git URL
fn extract_git_owner_repo(repo_url: &str) -> Option<(String, String)> {
    // Remove protocol prefix
    let url = repo_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("git@")
        .replace(':', "/"); // Handle SSH format git@host:owner/repo

    // Split by '/' and find owner/repo pattern
    let parts: Vec<&str> = url.split('/').collect();

    (parts.len() >= 3).then(|| {
        let owner = parts[1];
        let mut repo = parts[2].to_string();

        // Remove .git suffix if present
        if std::path::Path::new(&repo)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("git"))
        {
            repo = repo[..repo.len() - 4].to_string();
        }

        (owner.to_string(), repo)
    })
}

/// Parse an `ArgoCD` Git source (Kustomize/YAML/directory) into a PURL object
///
/// Detects Git hosting platform and uses appropriate PURL type:
/// - GitHub → `pkg:github/...`
/// - Bitbucket → `pkg:bitbucket/...`
/// - Other → `pkg:generic/...?vcs_url=...&type=git`
fn parse_argocd_git_source_to_purl(
    repo_url: &str,
    path: Option<&str>,
    target_revision: &str,
) -> Value {
    let platform = detect_git_platform(repo_url);
    let mut qualifiers = BTreeMap::new();
    let mut purl_obj = BTreeMap::new();

    // Normalize path: filter out None, empty string, and "." (current directory)
    let normalized_path = path.filter(|p| !p.is_empty() && *p != ".");

    match platform {
        "github" | "bitbucket" => {
            // Use native PURL type for GitHub/Bitbucket
            purl_obj.insert(KeyString::from("type"), Value::from(platform));

            if let Some((owner, repo)) = extract_git_owner_repo(repo_url) {
                purl_obj.insert(KeyString::from("namespace"), Value::from(owner));
                purl_obj.insert(KeyString::from("name"), Value::from(repo));
            } else {
                // Fallback if parsing fails
                purl_obj.insert(KeyString::from("name"), Value::from("unknown"));
            }

            if let Some(p) = normalized_path {
                qualifiers.insert(KeyString::from("path"), Value::from(p));
            }
        }
        _ => {
            // Generic type for GitLab and other Git sources
            purl_obj.insert(KeyString::from("type"), Value::from("generic"));

            // Extract repo name from URL
            if let Some((_, repo)) = extract_git_owner_repo(repo_url) {
                purl_obj.insert(KeyString::from("name"), Value::from(repo));
            } else {
                purl_obj.insert(KeyString::from("name"), Value::from("unknown"));
            }

            qualifiers.insert(KeyString::from("vcs_url"), Value::from(repo_url));
            qualifiers.insert(KeyString::from("type"), Value::from("git"));

            if let Some(p) = normalized_path {
                qualifiers.insert(KeyString::from("path"), Value::from(p));
            }
        }
    }

    purl_obj.insert(KeyString::from("version"), Value::from(target_revision));
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
            example!(
                title: "parse short image name",
                source: r#"purl_from_oci_image("nginx")"#,
                result: Ok(
                    r#"{"type": "oci", "namespace": "library", "name": "nginx", "version": null, "qualifiers": {"repository_url": "docker.io/library", "tag": "latest"}, "subpath": null}"#,
                ),
            ),
            example!(
                title: "parse image with tag",
                source: r#"purl_from_oci_image("nginx:1.21")"#,
                result: Ok(
                    r#"{"type": "oci", "namespace": "library", "name": "nginx", "version": null, "qualifiers": {"repository_url": "docker.io/library", "tag": "1.21"}, "subpath": null}"#,
                ),
            ),
            example!(
                title: "parse image with digest",
                source: r#"purl_from_oci_image("nginx@sha256:abc123")"#,
                result: Ok(
                    r#"{"type": "oci", "namespace": "library", "name": "nginx", "version": "@sha256:abc123", "qualifiers": {"repository_url": "docker.io/library"}, "subpath": null}"#,
                ),
            ),
            example!(
                title: "parse fully qualified image",
                source: r#"purl_from_oci_image("ghcr.io/owner/image:v1.0@sha256:abc123")"#,
                result: Ok(
                    r#"{"type": "oci", "namespace": "owner", "name": "image", "version": "@sha256:abc123", "qualifiers": {"repository_url": "ghcr.io/owner", "tag": "v1.0"}, "subpath": null}"#,
                ),
            ),
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
            example!(
                title: "convert OCI PURL object to string",
                source: r#"purl_to_string({"type": "oci", "name": "nginx", "version": "@sha256:abc", "qualifiers": {"repository_url": "docker.io/library", "tag": "1.21"}})"#,
                result: Ok(r"pkg:oci/nginx@sha256:abc?repository_url=docker.io%2Flibrary&tag=1.21"),
            ),
            example!(
                title: "convert Helm PURL object to string",
                source: r#"purl_to_string({"type": "helm", "name": "wordpress", "version": "15.2.35", "qualifiers": {"repository_url": "https://charts.bitnami.com/bitnami"}})"#,
                result: Ok(
                    r"pkg:helm/wordpress@15.2.35?repository_url=https%3A%2F%2Fcharts.bitnami.com%2Fbitnami",
                ),
            ),
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
// VRL Function: purl_from_argocd_helm
// ============================================================================

#[derive(Clone, Copy, Debug)]
pub struct PurlFromArgoCdHelm;

impl Function for PurlFromArgoCdHelm {
    fn identifier(&self) -> &'static str {
        "purl_from_argocd_helm"
    }

    fn parameters(&self) -> &'static [Parameter] {
        &[
            Parameter { keyword: "repo_url", kind: kind::BYTES, required: true },
            Parameter { keyword: "chart", kind: kind::BYTES, required: true },
            Parameter { keyword: "target_revision", kind: kind::BYTES, required: true },
        ]
    }

    fn examples(&self) -> &'static [Example] {
        &[
            example!(
                title: "parse OCI Helm chart",
                source: r#"purl_from_argocd_helm("ghcr.io/owner", "nginx", "1.0.0")"#,
                result: Ok(
                    r#"{"type": "oci", "name": "nginx", "version": "1.0.0", "qualifiers": {"repository_url": "ghcr.io/owner"}, "subpath": null}"#,
                ),
            ),
            example!(
                title: "parse HTTP Helm repository",
                source: r#"purl_from_argocd_helm("https://charts.bitnami.com/bitnami", "wordpress", "15.2.35")"#,
                result: Ok(
                    r#"{"type": "generic", "name": "wordpress", "version": "15.2.35", "qualifiers": {"download_url": "https://charts.bitnami.com/bitnami", "type": "helm"}, "subpath": null}"#,
                ),
            ),
        ]
    }

    fn compile(
        &self,
        _state: &state::TypeState,
        _ctx: &mut FunctionCompileContext,
        arguments: ArgumentList,
    ) -> Compiled {
        let repo_url = arguments.required("repo_url");
        let chart = arguments.required("chart");
        let target_revision = arguments.required("target_revision");

        Ok(PurlFromArgoCdHelmFn { repo_url, chart, target_revision }.as_expr())
    }
}

#[derive(Debug, Clone)]
struct PurlFromArgoCdHelmFn {
    repo_url: Box<dyn Expression>,
    chart: Box<dyn Expression>,
    target_revision: Box<dyn Expression>,
}

impl FunctionExpression for PurlFromArgoCdHelmFn {
    fn resolve(&self, ctx: &mut Context) -> Resolved {
        let repo_url = self.repo_url.resolve(ctx)?;
        let repo_url_str = repo_url.try_bytes_utf8_lossy()?;

        let chart = self.chart.resolve(ctx)?;
        let chart_str = chart.try_bytes_utf8_lossy()?;

        let target_revision = self.target_revision.resolve(ctx)?;
        let target_revision_str = target_revision.try_bytes_utf8_lossy()?;

        Ok(parse_argocd_helm_to_purl(
            repo_url_str.as_ref(),
            chart_str.as_ref(),
            target_revision_str.as_ref(),
        ))
    }

    fn type_def(&self, _state: &state::TypeState) -> TypeDef {
        TypeDef::object(Collection::from_unknown(Kind::any()))
    }
}

// ============================================================================
// VRL Function: purl_from_argocd_git_source
// ============================================================================

#[derive(Clone, Copy, Debug)]
pub struct PurlFromArgoCdGitSource;

impl Function for PurlFromArgoCdGitSource {
    fn identifier(&self) -> &'static str {
        "purl_from_argocd_git_source"
    }

    fn parameters(&self) -> &'static [Parameter] {
        &[
            Parameter { keyword: "repo_url", kind: kind::BYTES, required: true },
            Parameter { keyword: "path", kind: kind::BYTES, required: false },
            Parameter { keyword: "target_revision", kind: kind::BYTES, required: true },
        ]
    }

    fn examples(&self) -> &'static [Example] {
        &[
            example!(
                title: "parse GitHub source",
                source: r#"purl_from_argocd_git_source("https://github.com/kubernetes-sigs/kustomize", "examples/helloWorld", "v5.0.0")"#,
                result: Ok(
                    r#"{"type": "github", "namespace": "kubernetes-sigs", "name": "kustomize", "version": "v5.0.0", "qualifiers": {"path": "examples/helloWorld"}, "subpath": null}"#,
                ),
            ),
            example!(
                title: "parse Bitbucket source",
                source: r#"purl_from_argocd_git_source("https://bitbucket.org/myteam/myapp", "manifests", "main")"#,
                result: Ok(
                    r#"{"type": "bitbucket", "namespace": "myteam", "name": "myapp", "version": "main", "qualifiers": {"path": "manifests"}, "subpath": null}"#,
                ),
            ),
            example!(
                title: "parse generic Git source",
                source: r#"purl_from_argocd_git_source("https://gitlab.com/group/project", "k8s", "v1.2.3")"#,
                result: Ok(
                    r#"{"type": "generic", "name": "project", "version": "v1.2.3", "qualifiers": {"path": "k8s", "type": "git", "vcs_url": "https://gitlab.com/group/project"}, "subpath": null}"#,
                ),
            ),
        ]
    }

    fn compile(
        &self,
        _state: &state::TypeState,
        _ctx: &mut FunctionCompileContext,
        arguments: ArgumentList,
    ) -> Compiled {
        let repo_url = arguments.required("repo_url");
        let path = arguments.optional("path");
        let target_revision = arguments.required("target_revision");

        Ok(PurlFromArgoCdGitSourceFn { repo_url, path, target_revision }.as_expr())
    }
}

#[derive(Debug, Clone)]
struct PurlFromArgoCdGitSourceFn {
    repo_url: Box<dyn Expression>,
    path: Option<Box<dyn Expression>>,
    target_revision: Box<dyn Expression>,
}

impl FunctionExpression for PurlFromArgoCdGitSourceFn {
    fn resolve(&self, ctx: &mut Context) -> Resolved {
        let repo_url = self.repo_url.resolve(ctx)?;
        let repo_url_str = repo_url.try_bytes_utf8_lossy()?;

        let path_opt = if let Some(path_expr) = &self.path {
            let path_val = path_expr.resolve(ctx)?;
            Some(path_val.try_bytes_utf8_lossy()?.to_string())
        } else {
            None
        };

        let target_revision = self.target_revision.resolve(ctx)?;
        let target_revision_str = target_revision.try_bytes_utf8_lossy()?;

        Ok(parse_argocd_git_source_to_purl(
            repo_url_str.as_ref(),
            path_opt.as_deref(),
            target_revision_str.as_ref(),
        ))
    }

    fn type_def(&self, _state: &state::TypeState) -> TypeDef {
        TypeDef::object(Collection::from_unknown(Kind::any()))
    }
}

// ============================================================================
// Public API
// ============================================================================

/// Returns all custom VRL functions for PURL handling
pub fn all_custom_functions() -> Vec<Box<dyn Function>> {
    vec![
        Box::new(PurlFromOciImage),
        Box::new(PurlToString),
        Box::new(PurlFromArgoCdHelm),
        Box::new(PurlFromArgoCdGitSource),
    ]
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use rstest::rstest;

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

    #[rstest]
    #[case("ghcr.io/owner", true)]
    #[case("registry-1.docker.io/bitnamicharts", true)]
    #[case("https://charts.bitnami.com/bitnami", false)]
    #[case("http://example.com/charts", false)]
    fn test_is_oci_registry(#[case] repo_url: &str, #[case] expected: bool) {
        assert_eq!(is_oci_registry(repo_url), expected);
    }

    // ArgoCD Helm → PURL string tests
    #[rstest]
    #[case("ghcr.io/owner", "nginx", "1.0.0", "pkg:oci/nginx@1.0.0?repository_url=ghcr.io/owner")]
    #[case(
        "https://charts.bitnami.com/bitnami",
        "wordpress",
        "15.2.35",
        "pkg:generic/wordpress@15.2.35?download_url=https://charts.bitnami.com/bitnami&type=helm"
    )]
    fn test_argocd_helm_to_purl_string(
        #[case] repo_url: &str,
        #[case] chart: &str,
        #[case] version: &str,
        #[case] expected: &str,
    ) {
        let purl = parse_argocd_helm_to_purl(repo_url, chart, version);
        assert_eq!(purl_object_to_string(&purl).unwrap(), expected);
    }

    // ArgoCD Git source → PURL string tests
    #[rstest]
    #[case(
        "https://github.com/kubernetes-sigs/kustomize",
        Some("examples/helloWorld"),
        "v5.0.0",
        "pkg:github/kubernetes-sigs/kustomize@v5.0.0?path=examples/helloWorld"
    )]
    #[case(
        "https://github.com/argoproj/argo-cd",
        None,
        "stable",
        "pkg:github/argoproj/argo-cd@stable"
    )]
    #[case(
        "git@github.com:owner/repo.git",
        Some("config"),
        "master",
        "pkg:github/owner/repo@master?path=config"
    )]
    #[case(
        "https://bitbucket.org/myteam/myapp",
        Some("manifests"),
        "main",
        "pkg:bitbucket/myteam/myapp@main?path=manifests"
    )]
    #[case(
        "https://gitlab.com/group/project",
        Some("k8s"),
        "v1.2.3",
        "pkg:generic/project@v1.2.3?path=k8s&type=git&vcs_url=https://gitlab.com/group/project"
    )]
    // Path normalization: empty string should be omitted
    #[case("https://github.com/owner/repo", Some(""), "v1.0.0", "pkg:github/owner/repo@v1.0.0")]
    // Path normalization: "." (current directory) should be omitted
    #[case("https://github.com/owner/repo", Some("."), "v1.0.0", "pkg:github/owner/repo@v1.0.0")]
    fn test_argocd_git_source_to_purl_string(
        #[case] repo_url: &str,
        #[case] path: Option<&str>,
        #[case] version: &str,
        #[case] expected: &str,
    ) {
        let purl = parse_argocd_git_source_to_purl(repo_url, path, version);
        assert_eq!(purl_object_to_string(&purl).unwrap(), expected);
    }
}
