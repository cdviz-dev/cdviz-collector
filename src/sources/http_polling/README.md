# HTTP Polling Source

Periodically polls an HTTP endpoint and creates one `EventSource` per successful
response. Useful for services that expose data via a pull API rather than
pushing webhooks, and for bootstrapping historical data before switching to
webhook-based ingestion.

## Use Cases

- **Services without webhook support** — Jenkins Remote API, legacy CI servers,
  custom REST APIs that expose build/deploy history through query parameters.
- **Historical backfill** — Set `ts_after` to a past date and `ts_before_limit`
  to "now" at startup to ingest a bounded historical window, then switch to a
  webhook source going forward.
- **Slow-changing data** — Poll a configuration API or inventory endpoint on a
  long interval (e.g., hourly) where push is not available.

## How It Works

1. A [VRL] script (`request_vrl`) generates the HTTP request (URL, method,
   headers, query parameters) from the current **time window** (`ts_after` …
   `ts_before`).
2. `cdviz-collector` sends the request with retry and distributed tracing.
3. On HTTP 200, the response body is parsed into one or more `EventSource`
   values (depending on `parser`) and forwarded downstream for transformation.
4. On success the time window advances: `ts_after` = old `ts_before`,
   `ts_before` = `now − 1 s`. On any failure the window stays put and the
   next poll retries the same window.
5. When `ts_before_limit` is reached the source exits — useful for bounded
   backfills.

## Time Window

| Field             | Description                                                                                                                                                                    |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `ts_after`        | Exclusive lower bound. Defaults to `Timestamp::MIN` (the beginning of time). Loaded from persisted state on restart if available; config value used only as initial bootstrap. |
| `ts_before`       | Exclusive upper bound. Always computed as `now − 1 s` (capped at `ts_before_limit`).                                                                                           |
| `ts_before_limit` | Optional cap. When `ts_after` reaches this value the source stops.                                                                                                             |

Both values are exposed as `metadata.ts_after` and `metadata.ts_before` (ISO
8601 strings) in the VRL request script and in every `EventSource` sent
downstream.

## VRL Request Script

The `request_vrl` program receives a target with the shape:

```json
{
  "metadata": {
    "ts_after": "2024-01-01T00:00:00Z",
    "ts_before": "2024-01-02T00:00:00Z",
    "context": { "source": "cdviz-collector://...?source=my-source" }
  }
}
```

It must **set `.url`** (required) and may set any of the following on the
target:

| Field      | Type             | Default | Description                                                                                     |
| ---------- | ---------------- | ------- | ----------------------------------------------------------------------------------------------- |
| `.url`     | String           | —       | Request URL (required).                                                                         |
| `.method`  | String           | `"GET"` | HTTP method.                                                                                    |
| `.headers` | Object (strings) | `{}`    | Additional request headers. Merged with static `headers` config; config values take precedence. |
| `.body`    | String           | none    | Request body.                                                                                   |
| `.query`   | Object (strings) | `{}`    | Query parameters appended to the URL.                                                           |

> **VRL note**: use `to_string!()` (the infallible `!` variant) when converting
> field values to strings, because `to_string()` is fallible and requires error
> handling.

### Example — Jenkins Build History

```vrl
.url = "https://jenkins.example.com/job/my-pipeline/api/json"
.query.tree = "builds[number,result,timestamp,duration,url]"
.query.xpath = "{not(null)}"
```

### Example — Custom REST API with Time Window

```vrl
.url = "https://api.example.com/events"
.query.after  = to_string!(.metadata.ts_after)
.query.before = to_string!(.metadata.ts_before)
.query.limit  = "500"
```

## Response Parsing (`parser`)

| Value   | Accept header sent                                   | Parsing                                        |
| ------- | ---------------------------------------------------- | ---------------------------------------------- |
| `auto`  | `application/json, application/x-ndjson, text/plain` | Detected from `Content-Type` (default).        |
| `json`  | `application/json`                                   | Whole body → one `EventSource`.                |
| `jsonl` | `application/x-ndjson`                               | One `EventSource` per non-empty line.          |
| `text`  | `text/plain`                                         | Whole body as JSON string → one `EventSource`. |

With `jsonl` a single poll may emit multiple `EventSource` events.
Timestamp advancement still happens as long as parsing succeeds, even when zero
lines are present.

## `EventSource` Metadata

Every `EventSource` created by this source carries the following in its
`metadata` field (merged on top of the static `metadata` config value):

```json
{
  "ts_after": "2024-01-01T00:00:00Z",
  "ts_before": "2024-01-02T00:00:00Z",
  "http_polling": {
    "url": "https://api.example.com/events?after=...",
    "method": "GET",
    "status": 200
  },
  "context": { "source": "cdviz-collector://...?source=my-source" }
}
```

VRL transformers downstream access these fields exactly as shown above.

## Configuration Reference

```toml
[sources.my_source.extractor]
type = "http_polling"

## Polling interval (humantime format: "30s", "5m", "1h").
polling_interval = "1m"

## VRL script to build the HTTP request.
request_vrl = """
.url = "https://api.example.com/events"
.query.after  = to_string!(.metadata.ts_after)
.query.before = to_string!(.metadata.ts_before)
"""

## Bootstrap start for the time window (optional, defaults to the beginning of time).
## Overridden by persisted state on restart.
# ts_after = "2024-01-01T00:00:00Z"

## Stop the source once ts_after reaches this timestamp (optional).
## Useful for bounded historical backfills.
# ts_before_limit = "2025-01-01T00:00:00Z"

## How to parse the response body.  Options: "auto" (default), "json", "jsonl", "text".
# parser = "auto"

## Retry budget for transient HTTP failures (humantime format).  Default: 30s.
# total_duration_of_retries = "30s"

## Static or secret request headers (same format as the http sink).
# [sources.my_source.extractor.headers]
# "Authorization" = { type = "secret", value = "Bearer TOKEN" }
# "X-Custom"      = { type = "static", value = "hello" }

## Base metadata merged into every EventSource (optional).
# [sources.my_source.extractor.metadata]
# environment_id = "/production"
```

## Full Example — Jenkins Backfill

The following configuration ingests the last 30 days of Jenkins build history
and then stops.

```toml
[sources.jenkins_backfill]
enabled = true
transformer_refs = ["jenkins_builds"]

[sources.jenkins_backfill.extractor]
type             = "http_polling"
polling_interval = "10s"
ts_after         = "2024-12-01T00:00:00Z"
ts_before_limit  = "2025-01-01T00:00:00Z"
parser           = "json"

request_vrl = """
.url    = "https://jenkins.example.com/job/my-pipeline/api/json"
.method = "GET"
.query.tree = "builds[number,result,timestamp,duration,url]"
"""

[sources.jenkins_backfill.extractor.headers]
Authorization = { type = "secret", value = "Basic BASE64_ENCODED_CREDENTIALS" }

[sources.jenkins_backfill.extractor.metadata]
"context.source" = "https://jenkins.example.com"
```

## State Persistence

When the `source_opendal` feature is also enabled (included in the default
feature set) and a `[state]` section is configured, the current `ts_after`
checkpoint is saved to the state store after each successful poll. On restart,
the persisted value is loaded automatically so polling resumes from where it
left off rather than restarting from the configured `ts_after`.

```toml
[state]
kind = "fs"

[state.parameters]
root = "./.cdviz-collector/state"
```

[VRL]: https://vrl.dev
