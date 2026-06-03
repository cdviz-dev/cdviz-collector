use http::Extensions;
use reqwest::{Request, Response, StatusCode};
use reqwest_middleware::{Middleware, Next};
use std::time::{Duration, SystemTime};
use tokio::time::sleep;

/// Maximum number of redirects (301/302/303/307/308) to follow before giving up.
const MAX_REDIRECTS: u32 = 10;
/// Maximum number of server-requested retries (429/503) before giving up.
const MAX_RETRIES: u32 = 10;

/// Middleware that handles HTTP-level retry and redirect signals:
///
/// - `303 See Other` with `Retry-After` — async polling redirect (server not ready yet).
///   Sleeps for the indicated duration, then re-issues a GET to the `Location` URL.
/// - `429 Too Many Requests` — rate limited. Sleeps for `Retry-After`, then retries.
/// - `503 Service Unavailable` — temporary downtime. Sleeps for `Retry-After`, then retries.
/// - `301 Moved Permanently` / `302 Found` / `307 Temporary Redirect` / `308 Permanent Redirect`
///   — follows `Location` immediately.
///
/// If no `Retry-After` is present on a 429/503, the response is returned as-is (letting
/// the [`reqwest_retry::RetryTransientMiddleware`] stacked behind this one handle it via
/// exponential backoff for network-level transient failures).
pub(crate) struct RetryAfterMiddleware;

#[async_trait::async_trait]
impl Middleware for RetryAfterMiddleware {
    async fn handle(
        &self,
        req: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<Response> {
        let mut current = req;
        let mut redirect_count: u32 = 0;
        let mut retry_count: u32 = 0;

        loop {
            let cloned = current.try_clone();
            let resp = next.clone().run(current, extensions).await?;
            match resp.status() {
                StatusCode::SEE_OTHER => {
                    if redirect_count >= MAX_REDIRECTS {
                        tracing::warn!(MAX_REDIRECTS, "redirect limit reached");
                        return Ok(resp);
                    }
                    let Some(mut req) = cloned else {
                        tracing::warn!("cannot follow 303 redirect: request body is not cloneable");
                        return Ok(resp);
                    };
                    if let Some(d) = parse_retry_after(resp.headers()) {
                        sleep(d).await;
                    }
                    let Some(loc) = resolve_location(resp.headers(), req.url()) else {
                        return Ok(resp);
                    };
                    strip_credentials_if_cross_origin(&mut req, &loc);
                    *req.method_mut() = reqwest::Method::GET;
                    *req.url_mut() = loc;
                    *req.body_mut() = None;
                    current = req;
                    redirect_count += 1;
                }
                StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                    if retry_count >= MAX_RETRIES {
                        tracing::warn!(MAX_RETRIES, "server-requested retry limit reached");
                        return Ok(resp);
                    }
                    let Some(req) = cloned else {
                        tracing::warn!("cannot retry: request body is not cloneable");
                        return Ok(resp);
                    };
                    // If no Retry-After present, return the response as-is so the outer
                    // RetryTransientMiddleware can apply exponential backoff.
                    let Some(d) = parse_retry_after(resp.headers()) else {
                        return Ok(resp);
                    };
                    sleep(d).await;
                    current = req;
                    retry_count += 1;
                }
                StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT => {
                    if redirect_count >= MAX_REDIRECTS {
                        tracing::warn!(MAX_REDIRECTS, "redirect limit reached");
                        return Ok(resp);
                    }
                    let Some(mut req) = cloned else {
                        tracing::warn!("cannot follow redirect: request body is not cloneable");
                        return Ok(resp);
                    };
                    let Some(loc) = resolve_location(resp.headers(), req.url()) else {
                        return Ok(resp);
                    };
                    strip_credentials_if_cross_origin(&mut req, &loc);
                    *req.url_mut() = loc;
                    current = req;
                    redirect_count += 1;
                }
                _ => return Ok(resp),
            }
        }
    }
}

fn resolve_location(
    headers: &reqwest::header::HeaderMap,
    base: &reqwest::Url,
) -> Option<reqwest::Url> {
    let raw = headers.get(reqwest::header::LOCATION)?.to_str().ok()?;
    base.join(raw).ok()
}

/// Strip credential headers when a redirect crosses origins (scheme + host + port).
///
/// Prevents `Authorization`, `Cookie`, and `Proxy-Authorization` from being
/// forwarded to a different server. Called before following any redirect.
fn strip_credentials_if_cross_origin(req: &mut Request, new_url: &reqwest::Url) {
    if is_cross_origin(req.url(), new_url) {
        let headers = req.headers_mut();
        headers.remove(reqwest::header::AUTHORIZATION);
        headers.remove(reqwest::header::COOKIE);
        headers.remove(reqwest::header::PROXY_AUTHORIZATION);
    }
}

fn is_cross_origin(a: &reqwest::Url, b: &reqwest::Url) -> bool {
    if a.scheme() != b.scheme() || a.host() != b.host() {
        return true;
    }
    match (a.port_or_known_default(), b.port_or_known_default()) {
        (Some(pa), Some(pb)) => pa != pb,
        // Unknown scheme: treat as cross-origin to be conservative
        _ => true,
    }
}

/// Parse `Retry-After` header value as a `Duration`.
///
/// Supports both integer seconds (`Retry-After: 60`) and HTTP-date format
/// (`Retry-After: Wed, 21 Oct 2015 07:28:00 GMT`).
pub(crate) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    if let Ok(system_time) = httpdate::parse_http_date(value) {
        if let Ok(d) = system_time.duration_since(SystemTime::now()) {
            return Some(d);
        }
        tracing::debug!(value, "Retry-After date is in the past, ignoring");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    fn headers_with_retry_after(value: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(RETRY_AFTER, HeaderValue::from_str(value).unwrap());
        map
    }

    #[test]
    fn test_parse_retry_after_seconds() {
        let h = headers_with_retry_after("42");
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(42)));
    }

    #[test]
    fn test_parse_retry_after_zero() {
        let h = headers_with_retry_after("0");
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(0)));
    }

    #[test]
    fn test_parse_retry_after_http_date_past() {
        // A date in the past should return None (duration_since would fail)
        let h = headers_with_retry_after("Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn test_parse_retry_after_missing() {
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn test_parse_retry_after_invalid() {
        let h = headers_with_retry_after("not-a-number");
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn test_is_cross_origin_same() {
        let a = reqwest::Url::parse("https://api.example.com/v1/foo").unwrap();
        let b = reqwest::Url::parse("https://api.example.com/v1/bar").unwrap();
        assert!(!is_cross_origin(&a, &b));
    }

    #[test]
    fn test_is_cross_origin_different_host() {
        let a = reqwest::Url::parse("https://api.example.com/foo").unwrap();
        let b = reqwest::Url::parse("https://evil.example.com/foo").unwrap();
        assert!(is_cross_origin(&a, &b));
    }

    #[test]
    fn test_is_cross_origin_different_scheme() {
        let a = reqwest::Url::parse("https://api.example.com/foo").unwrap();
        let b = reqwest::Url::parse("http://api.example.com/foo").unwrap();
        assert!(is_cross_origin(&a, &b));
    }

    #[test]
    fn test_is_cross_origin_unknown_scheme() {
        // Unknown scheme: both ports are None → treated as cross-origin (conservative)
        let a = reqwest::Url::parse("custom://api.example.com/foo").unwrap();
        let b = reqwest::Url::parse("custom://api.example.com/bar").unwrap();
        assert!(is_cross_origin(&a, &b));
    }

    #[test]
    fn test_strip_credentials_cross_origin_removes_auth() {
        use reqwest::header::{AUTHORIZATION, HeaderValue};

        let original = reqwest::Url::parse("https://api.example.com/foo").unwrap();
        let new_url = reqwest::Url::parse("https://evil.example.com/foo").unwrap();

        let mut req = reqwest::Request::new(reqwest::Method::GET, original);
        req.headers_mut().insert(AUTHORIZATION, HeaderValue::from_static("Bearer secret"));

        strip_credentials_if_cross_origin(&mut req, &new_url);
        assert!(req.headers().get(AUTHORIZATION).is_none(), "Authorization should be stripped");
    }

    #[test]
    fn test_strip_credentials_same_origin_preserves_auth() {
        use reqwest::header::{AUTHORIZATION, HeaderValue};

        let original = reqwest::Url::parse("https://api.example.com/foo").unwrap();
        let new_url = reqwest::Url::parse("https://api.example.com/bar").unwrap();

        let mut req = reqwest::Request::new(reqwest::Method::GET, original);
        req.headers_mut().insert(AUTHORIZATION, HeaderValue::from_static("Bearer secret"));

        strip_credentials_if_cross_origin(&mut req, &new_url);
        assert!(req.headers().get(AUTHORIZATION).is_some(), "Authorization should be preserved");
    }
}
