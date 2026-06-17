# HTTP Polling Source

Periodically polls one or more HTTP endpoints and creates one `EventSource` per
parsed response item. Useful for services that expose data via a pull API rather
than pushing webhooks, and for bootstrapping historical data before switching to
webhook-based ingestion.

Each poll is driven by a single [VRL] **driver script** that builds a worklist of
HTTP requests. A response can be emitted downstream, fed back into the driver to
compute further requests, or both — which makes **multi-pass** flows possible:
list/discovery followed by per-item detail fetches (Jenkins, GitHub REST
packages), or cursor pagination where the cursor lives in the response body
(GraphQL).

## Use Cases

- **Services without webhook support** — Jenkins Remote API, legacy CI servers,
  custom REST APIs that expose build/deploy history through query parameters.
- **Multi-pass APIs** — first list resources, then fetch details per resource
  (e.g. Jenkins: list jobs → per-job builds → per-build detail; GitHub: list
  packages → versions → detail).
- **Cursor pagination** — GraphQL or APIs whose "next" cursor is in the body.
- **Historical backfill** — set `ts_after` to a past date and `ts_before_limit`
  to "now" to ingest a bounded historical window, then switch to a webhook
  source going forward.
- **Slow-changing data** — poll a configuration or inventory endpoint on a long
  interval where push is not available.

## How It Works

1. At the start of each poll the **driver** VRL script is invoked once
   (**bootstrap**, with `.response = null`) to seed an initial list of requests
   from the current **time window** (`ts_after` … `ts_before`).
2. Requests are fetched (with retry and distributed tracing), up to
   `max_concurrency` in flight at once, the start of each spaced by at least
   `min_request_interval`.
3. Each response is **routed** according to its request's `route`:
   - `pipeline` (default) — the body is parsed into one or more `EventSource`
     values and forwarded downstream.
   - `feedback` — the response is handed back to the driver, which may emit
     further requests.
   - `both` — emit **and** feed back (e.g. GraphQL: emit `nodes`, feed
     `pageInfo` back for the next page).
4. The poll ends when the worklist drains (or a guard trips — see
   [Guards](#guards-termination)).
5. On success the time window advances: `ts_after` = old `ts_before`,
   `ts_before` = `now − 1 s`. The window does **not** advance if the bootstrap
   driver call failed, or if requests were issued but none reached the server
   (a transport/HTTP outage) — the next poll retries the same window. A
   successful (2xx) response whose body fails to parse still advances the window
   (re-polling would not fix a malformed body).
6. When `ts_before_limit` is reached the source exits — useful for bounded
   backfills.

> **State is per-poll.** The driver `state` (below) is in-memory and reset at the
> start of every poll. Only the `ts_after`/`ts_before` time window is persisted
> across restarts (see [State Persistence](#state-persistence)).

## Time Window

| Field             | Description                                                                                                                                                                    |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `ts_after`        | Exclusive lower bound. Defaults to `Timestamp::MIN` (the beginning of time). Loaded from persisted state on restart if available; config value used only as initial bootstrap. |
| `ts_before`       | Exclusive upper bound. Always computed as `now − 1 s` (capped at `ts_before_limit`).                                                                                           |
| `ts_before_limit` | Optional cap. When `ts_after` reaches this value the source stops.                                                                                                             |

Both values are exposed as `metadata.ts_after` and `metadata.ts_before` (ISO
8601 strings) to the driver script and in every `EventSource` sent downstream.

## VRL Driver Script

The `driver_vrl` program is invoked once at bootstrap and again for every
response whose `route` feeds back. It receives a target shaped as:

```json
{
  "metadata": {
    "ts_after": "2024-01-01T00:00:00Z",
    "ts_before": "2024-01-02T00:00:00Z",
    "context": { "source": "cdviz-collector://...?source=my-source" }
  },
  "state": null,
  "request": null,
  "response": null
}
```

- `state` — the immutable snapshot carried by the request that produced this
  response. `null` at bootstrap and whenever the previous driver call set no
  `.state`.
- `request` — `{ url, method, headers }` of the request that produced this
  response, or `null` at bootstrap.
- `response` — `{ status, headers, body }` of the response, or `null` at
  bootstrap. `body` is the parsed JSON value when the body is JSON, otherwise the
  raw string.

The script **must set `.requests`** to an array (possibly empty) and may set
`.state`:

| Output field | Type            | Description                                                                                 |
| ------------ | --------------- | ------------------------------------------------------------------------------------------- |
| `.requests`  | Array (objects) | Requests to issue. Each entry needs a `url`; entries without one are skipped. **Required.** |
| `.state`     | any \| null     | New state, cloned (as an immutable snapshot) into every request in `.requests`. Optional.   |

Each `.requests[]` object:

| Field     | Type             | Default      | Description                                                                                     |
| --------- | ---------------- | ------------ | ----------------------------------------------------------------------------------------------- |
| `url`     | String           | —            | Request URL (required).                                                                         |
| `method`  | String           | `"GET"`      | HTTP method.                                                                                    |
| `headers` | Object (strings) | `{}`         | Additional request headers. Merged with static `headers` config; config values take precedence. |
| `body`    | String           | none         | Request body.                                                                                   |
| `query`   | Object (strings) | `{}`         | Query parameters appended to the URL.                                                           |
| `route`   | String           | `"pipeline"` | `pipeline`, `feedback`, or `both`.                                                              |
| `parser`  | String           | source dflt  | Per-request parser override: `auto`, `json`, `jsonl`, `text`.                                   |

> **VRL notes**
>
> - Use the infallible `!` variants (`to_string!()`, `string!()`, `array!()`)
>   when converting values whose type VRL cannot verify (e.g. fields read from
>   `.response.body`), otherwise the assignment is "fallible" and won't compile.
> - Closures (`for_each`, `map_values`) may read and mutate variables declared
>   outside them; the mutations persist.

### Example — single request (simplest)

```vrl
.requests = [{ "url": "https://api.example.com/events" }]
```

### Example — time-windowed request

```vrl
.requests = [{
  "url": "https://api.example.com/events",
  "query": {
    "after":  to_string!(.metadata.ts_after),
    "before": to_string!(.metadata.ts_before),
    "limit":  "500"
  }
}]
```

### Example — Link-header pagination

Pagination is no longer a built-in flag; express it with `feedback`. Read the
`Link` header off `.response.headers`, extract the `rel="next"` URL with a regex,
and emit the next page until it is absent:

```vrl
if .response == null {
    # bootstrap: first page; emit its items AND feed it back to paginate
    .requests = [{ "url": "https://api.example.com/events", "route": "both" }]
} else {
    link = string(.response.headers.link) ?? ""
    matched = parse_regex(link, r'<(?P<next>[^>]+)>;\s*rel="next"') ?? {}
    if exists(matched.next) {
        .requests = [{ "url": matched.next, "route": "both" }]
    } else {
        .requests = []
    }
}
```

### Example — multi-pass (discovery → detail)

List item ids, then fetch each item's detail. The discovery response feeds back;
the detail responses are emitted. Stash any discovery context you need in
`.state` so it rides along into `metadata.http_polling.state` on the emitted
events (a downstream VRL transformer can merge it).

```vrl
if .response == null {
    # pass 1: discovery list — feed back, do not emit
    .requests = [{ "url": "https://api.example.com/jobs", "route": "feedback" }]
} else {
    # pass 2: one detail request per discovered job
    reqs = []
    for_each(array!(.response.body)) -> |_index, job| {
        reqs = push(reqs, {
            "url": "https://api.example.com/jobs/" + string!(job.id),
            "route": "pipeline"
        })
    }
    .requests = reqs
}
```

### Example — GraphQL cursor pagination (`both`)

```vrl
query = {
  "query": "query($after:String){ builds(after:$after){ nodes{ id status } pageInfo{ endCursor hasNextPage } } }",
  "variables": { "after": null }
}

if .response != null {
    query.variables.after = .response.body.data.builds.pageInfo.endCursor
}

cont = if .response == null {
    true
} else {
    .response.body.data.builds.pageInfo.hasNextPage == true
}

if cont {
    .requests = [{
        "url": "https://api.example.com/graphql",
        "method": "POST",
        "headers": { "content-type": "application/json" },
        "body": encode_json(query),
        "route": "both"
    }]
} else {
    .requests = []
}
```

With `route = "both"` each page's body is emitted downstream (use a transformer
to pull out `data.builds.nodes`) while the same body feeds the cursor back.

## Response Parsing (`parser`)

The source-level `parser` is the default; `requests[].parser` overrides it per
request.

| Value   | Accept header sent                                   | Parsing                                          |
| ------- | ---------------------------------------------------- | ------------------------------------------------ |
| `auto`  | `application/json, application/x-ndjson, text/plain` | Detected from `Content-Type` (default).          |
| `json`  | `application/json`                                   | Whole body → one `EventSource`.                  |
| `jsonl` | `application/x-ndjson`                               | One `EventSource` per non-empty line.            |
| `text`  | `text/plain`                                         | Whole body as a JSON string → one `EventSource`. |

With `jsonl` a single response may emit multiple `EventSource` events.

## `EventSource` Metadata

Every `EventSource` carries the following in its `metadata` field (merged on top
of the static `metadata` config value):

```json
{
  "ts_after": "2024-01-01T00:00:00Z",
  "ts_before": "2024-01-02T00:00:00Z",
  "http_polling": {
    "url": "https://api.example.com/jobs/42",
    "method": "GET",
    "status": 200,
    "state": { "discovery_id": "42" }
  },
  "context": { "source": "cdviz-collector://...?source=my-source" }
}
```

`http_polling.state` is present only when the driver set `.state` for the
request that produced the event — use it to carry discovery/parent data into a
downstream transformer.

## Guards (termination)

A driver loop is bounded by three guards, all configurable:

| Field             | Default | Guards against                                                 |
| ----------------- | ------- | -------------------------------------------------------------- |
| `max_requests`    | `1000`  | Runaway worklists — total requests issued in a single poll.    |
| `max_depth`       | `50`    | Unbounded feedback recursion — bootstrap requests are depth 0. |
| `max_concurrency` | `4`     | Too many concurrent in-flight requests.                        |

When `max_requests` or `max_depth` is reached the remaining work is dropped and
logged; the poll still completes and the window advances.

## Rate Limiting and Retry-After

`cdviz-collector` automatically handles HTTP-level retry and redirect signals via
the `RetryAfterMiddleware` built into every `http_polling` source:

| Status                     | Action                                                                  |
| -------------------------- | ----------------------------------------------------------------------- |
| `303 See Other`            | Sleep `Retry-After`, re-fetch `Location` as GET (async polling pattern) |
| `429 Too Many Requests`    | Sleep `Retry-After`, retry                                              |
| `503 Service Unavailable`  | Sleep `Retry-After`, retry                                              |
| `301`, `302`, `307`, `308` | Follow `Location` immediately                                           |

`Retry-After` accepts both integer seconds (`Retry-After: 60`) and HTTP-date
format (`Retry-After: Wed, 21 Oct 2015 07:28:00 GMT`).

Use `min_request_interval` to space out the start of consecutive requests
(applies across the whole worklist, including concurrent fetches):

```toml
min_request_interval = "720ms"  # ≈ 83 req/min
```

Automatic redirect following is disabled in the underlying HTTP client; all
redirect and retry behaviour is managed by the middleware stack.

## Backfill Pattern

A historical backfill is just a `connect` run with `ts_after` and
`ts_before_limit` set. The source exits automatically when it reaches the limit;
re-running is safe because state checkpoints are saved after each successful
window:

```sh
# Ingest one year of GitHub workflow runs, then stop.
cdviz-collector connect --config github_backfill.toml
```

## Configuration Reference

```toml
[sources.my_source.extractor]
type = "http_polling"

## Polling interval (humantime format: "30s", "5m", "1h").
polling_interval = "1m"

## VRL driver script. Must set `.requests` (array). May set `.state`.
## Receives { metadata, state, request, response }.
driver_vrl = """
.requests = [{
  "url": "https://api.example.com/events",
  "query": {
    "after":  to_string!(.metadata.ts_after),
    "before": to_string!(.metadata.ts_before)
  }
}]
"""

## Bootstrap start for the time window (optional, defaults to the beginning of time).
## Overridden by persisted state on restart.
# ts_after = "2024-01-01T00:00:00Z"

## Stop the source once ts_after reaches this timestamp (optional).
## Useful for bounded historical backfills.
# ts_before_limit = "2025-01-01T00:00:00Z"

## Default parser for response bodies. Options: "auto" (default), "json", "jsonl", "text".
## Override per request via requests[].parser.
# parser = "auto"

## Minimum delay between the start of consecutive requests. Default: none.
# min_request_interval = "720ms"

## Max requests fetched concurrently within one poll. Default: 4.
# max_concurrency = 4

## Hard budget on total requests per poll (runaway guard). Default: 1000.
# max_requests = 1000

## Max feedback-chain depth (recursion guard). Default: 50.
# max_depth = 50

## Retry budget for transient HTTP failures (humantime format). Default: 30s.
# total_duration_of_retries = "30s"

## Static or secret request headers (same format as the http sink).
# [sources.my_source.extractor.headers]
# "Authorization" = { type = "secret", value = "Bearer TOKEN" }
# "X-Custom"      = { type = "static", value = "hello" }

## Base metadata merged into every EventSource (optional).
# [sources.my_source.extractor.metadata]
# environment_id = "/production"
```

## Full Example — Jenkins multi-pass backfill

List a pipeline's builds, then fetch each build's detail, ingesting the last
month and then stopping.

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
min_request_interval = "200ms"

driver_vrl = """
if .response == null {
    # pass 1: list builds for the pipeline, feed back
    .requests = [{
        "url": "https://jenkins.example.com/job/my-pipeline/api/json",
        "query": { "tree": "builds[number,url]" },
        "route": "feedback"
    }]
} else {
    # pass 2: fetch each build's detail
    reqs = []
    for_each(array!(.response.body.builds)) -> |_index, build| {
        reqs = push(reqs, {
            "url": string!(build.url) + "api/json",
            "route": "pipeline"
        })
    }
    .requests = reqs
}
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

Only the time window is persisted — the driver `state` is always reset at the
start of each poll.

```toml
[state]
kind = "fs"

[state.parameters]
root = "./.cdviz-collector/state"
```

[VRL]: https://vrl.dev
