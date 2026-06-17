# ADR 0001: Historical Backfill and Pagination Strategy

**Status:** Accepted (pagination mechanism superseded by [ADR 0002](0002-multi-pass-http-polling-request-driver.md))\
**Date:** 2026-06-02

---

> **Note (superseded in part):** The `follow_link_header` flag described below was
> replaced by the VRL request driver in [ADR 0002](0002-multi-pass-http-polling-request-driver.md).
> Link-header pagination is now expressed in the driver script (read the `Link`
> header off `.response.headers` and emit the next request with `route = "both"`).
> The backfill model, time-window strategy, and `RetryAfterMiddleware` from this
> ADR are unchanged.

---

## Context

CDviz needs to backfill a database with historical SDLC events (pipeline runs, artifacts,
pull requests, issues) from GitHub and GitLab before switching to live webhook-based
ingestion. Without backfill, the dashboard shows only future events and the historical
view is incomplete.

The same challenge applies to any CI/CD system that does not support push webhooks
(Jenkins, legacy CI servers, custom REST APIs): cdviz-collector must be able to pull data
from these systems via polling.

Two gaps existed in the `http_polling` source:

1. **No pagination** — one HTTP request per poll cycle. APIs that cap responses at 30–100
   items (GitHub REST API, GitLab REST API) silently drop older events when a time window
   contains more items than the page size.
2. **No HTTP-level retry signals** — `Retry-After` headers on `429 Too Many Requests` or
   `503 Service Unavailable` were ignored; only transport-level retries (network errors)
   were handled via exponential backoff.

---

## Decision

Enhance the existing `http_polling` source with:

1. **Link-header pagination** — opt-in `follow_link_header = true` config option that
   follows `Link: <url>; rel="next"` headers (RFC 5988) to consume all pages within a
   single time window before advancing.
2. **`RetryAfterMiddleware`** — a reqwest middleware stacked in the HTTP client pipeline
   that handles HTTP-level retry signals across multiple status codes (see below).
3. **`min_request_interval`** — optional minimum delay between consecutive HTTP requests,
   including pagination fetches, for APIs without rate-limit headers.

This makes a historical backfill possible with the existing `connect` subcommand and a
purpose-built TOML config. No new CLI subcommand is needed:

```sh
# Backfill GitHub workflow runs from 2024 to 2025, then stop.
cdviz-collector connect --config github_backfill.toml
# The source exits automatically when ts_before_limit is reached.
# Re-running is safe: state checkpoint resumes from the last window.
```

---

## Options Considered

| Option | Description                                                | Decision                                       |
| ------ | ---------------------------------------------------------- | ---------------------------------------------- |
| A      | `http_polling` + new REST API transformers, no pagination  | Rejected: silently truncates high-volume repos |
| **B**  | **Extend `http_polling` with pagination + rate limiting**  | **Accepted**                                   |
| C      | Third-party ETL (Steampipe, Airbyte, DLT) + file ingestion | Rejected: adds operational dependency          |
| D      | Script-based dump (`gh api --paginate`) + file ingestion   | Useful for one-off runs; complementary to B    |
| E      | New `backfill` subcommand                                  | Rejected: B + config achieves the same UX      |

---

## Retry-After Philosophy

Reference: https://dev.to/davidb31/add-a-friendly-http-polling-to-your-api-3jle

`Retry-After` is treated as a universal server-side signal, not just a rate-limit header.
The `RetryAfterMiddleware` handles it for:

| Status                    | Meaning                                       | Action                                          |
| ------------------------- | --------------------------------------------- | ----------------------------------------------- |
| `303 See Other`           | Async polling redirect — result not ready yet | Sleep `Retry-After`, re-fetch `Location` as GET |
| `429 Too Many Requests`   | Rate limited                                  | Sleep `Retry-After`, retry                      |
| `503 Service Unavailable` | Temporary downtime                            | Sleep `Retry-After`, retry                      |
| `301 Moved Permanently`   | Permanent redirect                            | Follow `Location` immediately                   |
| `302 Found`               | Temporary redirect                            | Follow `Location` immediately                   |
| `307 Temporary Redirect`  | Method-preserving redirect                    | Follow `Location` immediately                   |
| `308 Permanent Redirect`  | Method-preserving permanent redirect          | Follow `Location` immediately                   |

Automatic redirect following is disabled in the reqwest client so the middleware controls
all redirect behaviour. The existing `RetryTransientMiddleware` (exponential backoff) is
retained for transport-level failures (network errors, connection timeouts).

---

## Consequences

- `http_polling` becomes the general-purpose pull-API source for any REST API that exposes
  a time-filterable endpoint, with or without webhook support.
- GitHub/GitLab-specific VRL transformers for REST API responses are maintained in the
  [transformers-community repository](https://github.com/cdviz-dev/transformers-community)
  (separate from the webhook transformers already there).
- A backfill run is simply `cdviz-collector connect --config backfill.toml`. The source
  stops automatically when `ts_before_limit` is reached. State is checkpointed so the
  run is resumable after interruption.
- The `follow_link_header` option defaults to `false` to preserve backward compatibility
  with existing `http_polling` configurations.
