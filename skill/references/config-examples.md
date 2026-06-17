# cdviz-collector Configuration Examples

Full TOML configuration examples for each source and sink type.

## Global Settings

```toml
[http]
host     = "0.0.0.0"
port     = 8080
root_url = "https://cdviz-collector.example.com"  # used as context.source base

[pipeline]
queue_capacity = 1024   # in-memory broadcast channel size

[state]
kind = "fs"
[state.parameters]
root = "./.cdviz-collector/state"  # polling state persistence
```

---

## Sources

### Webhook (HTTP POST)

Receives events at `POST /webhook/<id>`. One config block per webhook.

```toml
[sources.github]
enabled          = true
transformer_refs = ["init_metadata", "github_events"]

[sources.github.extractor]
type = "webhook"
id   = "github"         # → POST /webhook/github
# headers_to_keep = ["X-GitHub-Event", "X-GitHub-Delivery"]

# Header validation (all must pass)
[sources.github.extractor.headers]
"X-GitHub-Event"      = { type = "exists" }
"X-Hub-Signature-256" = { type = "signature", token = "MY_WEBHOOK_SECRET", signature_prefix = "sha256=" }
# Other header check types:
# { type = "static", value = "Bearer abc" }       # exact match
# { type = "matches", pattern = "^Bearer .+" }    # regex match
# { type = "secret", value = "ENV_VAR_NAME" }     # compare to env var
```

### opendal — Filesystem

```toml
[sources.local_json]
enabled = true

[sources.local_json.extractor]
type             = "opendal"
kind             = "fs"
polling_interval = "10s"
parameters       = { root = "./inputs" }
recursive        = true
path_patterns    = ["**/*.json"]
parser           = "json"       # one JSON object per file
# parser = "jsonl"              # one JSON object per line
# parser = "csv_row"            # CSV rows
# parser = "yaml"               # YAML (requires parser_yaml feature)
# parser = "xml"                # XML (requires parser_xml feature)
# parser = "tap"                # TAP test output (requires parser_tap feature)
metadata         = { environment_id = "/cluster/dev" }
```

### opendal — S3

```toml
[sources.s3_events]
enabled = true

[sources.s3_events.extractor]
type             = "opendal"
kind             = "s3"
polling_interval = "60s"
path_patterns    = ["events/*.json"]
parser           = "json"
[sources.s3_events.extractor.parameters]
bucket    = "my-events-bucket"
region    = "us-east-1"
# endpoint = "http://minio:9000"      # for MinIO / custom S3
# access_key_id     = "..."           # or use env vars AWS_ACCESS_KEY_ID
# secret_access_key = "..."           # or use env vars AWS_SECRET_ACCESS_KEY
```

### opendal — GitHub Repository

```toml
[sources.github_repo]
enabled = true

[sources.github_repo.extractor]
type             = "opendal"
kind             = "github"
polling_interval = "120s"
path_patterns    = ["events/**/*.json"]
parser           = "json"
[sources.github_repo.extractor.parameters]
owner = "my-org"
repo  = "my-repo"
# token = "ghp_..."    # or env var GITHUB_TOKEN
```

### http_polling — REST API with a VRL request driver

The driver script builds a worklist of requests. `route = "both"` emits each page
downstream AND feeds it back so the driver can follow the `Link: rel="next"`
header. Multi-pass (discovery → per-item detail) is expressed the same way — see
`src/sources/http_polling/README.md`.

```toml
[sources.github_api]
enabled          = true
transformer_refs = ["github_to_cdevents"]

[sources.github_api.extractor]
type                 = "http_polling"
polling_interval     = "60s"
min_request_interval = "1s"    # min time between request starts
parser               = "json"  # whole body → one event (transformer splits the array)
ts_after             = "2024-01-01T00:00:00Z"  # bootstrap window start (backfill)

driver_vrl = """
if .response == null {
    .requests = [{
        "url": "https://api.github.com/repos/my-org/my-repo/events",
        "query": { "per_page": "100" },
        "route": "both"
    }]
} else {
    link = string(.response.headers.link) ?? ""
    matched = parse_regex(link, r'<(?P<next>[^>]+)>;\\s*rel="next"') ?? {}
    .requests = if exists(matched.next) { [{ "url": matched.next, "route": "both" }] } else { [] }
}
"""

[sources.github_api.extractor.headers]
"Authorization"        = { type = "static", value = "Bearer ghp_mytoken" }
"X-GitHub-Api-Version" = { type = "static", value = "2022-11-28" }
```

### SSE (Server-Sent Events inbound)

```toml
[sources.upstream_sse]
enabled = true

[sources.upstream_sse.extractor]
type        = "sse"
url         = "https://other-collector.example.com/sse/001"
max_retries = 10
[sources.upstream_sse.extractor.headers]
"Authorization" = { type = "static", value = "Bearer mytoken" }
```

---

## Sinks

### HTTP Webhook

```toml
[sinks.cdviz_server]
enabled                   = true
type                      = "http"
destination               = "https://cdviz.example.com/api/v1/events"
total_duration_of_retries = "30m"
log_full_response_on_error = false
transformer_refs          = ["filter_errors"]  # optional pre-send transform

[sinks.cdviz_server.headers]
"Authorization" = { type = "static",  value = "Bearer mytoken" }
"X-API-Key"     = { type = "secret",  value = "API_KEY_ENV_VAR" }  # reads env var
"Content-Type"  = { type = "static",  value = "application/json" }
```

### PostgreSQL

```toml
[sinks.database]
enabled                = true
type                   = "db"
url                    = "postgresql://user:pass@localhost:5432/cdviz?search_path=cdviz"
pool_connections_min   = 0
pool_connections_max   = 10
pool_acquire_timeout   = "30s"
pool_idle_timeout      = "10m"
pool_max_lifetime      = "30m"
pool_test_before_acquire = true
lazy_connection        = false
```

### ClickHouse

```toml
[sinks.clickhouse]
enabled   = true
type      = "clickhouse"
url       = "http://localhost:8123"
database  = "default"
# user     = "default"
# password = ""
query = "INSERT INTO cdevents_lake (id, type, source, subject, predicate, specversion, timestamp, payload) VALUES ({id}, {type}, {source}, {subject}, {predicate}, {specversion}, {timestamp}, {payload})"
```

### Folder (local filesystem)

```toml
[sinks.local_output]
enabled    = true
type       = "folder"
kind       = "fs"
parameters = { root = "./output" }
```

### Debug (stdout)

```toml
[sinks.debug]
enabled = true
type    = "debug"
# format = "json"       # default: rust_debug
# format = "rust_debug"
```

### SSE (Server-Sent Events outbound)

```toml
# Creates endpoint: GET /sse/001
[sinks.sse_out]
enabled = true
type    = "sse"
id      = "001"

# Optional: require headers from SSE clients
[sinks.sse_out.headers]
"Authorization" = { type = "exists" }
```

### Kafka

```toml
[sinks.kafka_out]
enabled = true
type    = "kafka"
topic   = "cdevents"
[sinks.kafka_out.configuration]
"bootstrap.servers" = "localhost:9092"
"message.timeout.ms" = "5000"
```

---

## Transformers

### Inline VRL

```toml
[transformers.service_deployed]
type     = "vrl"
template = """
[{
  "metadata": .metadata,
  "headers":  .headers,
  "body": {
    "context": {
      "version":   "0.4.1",
      "id":        "0",
      "source":    .metadata.context.source,
      "type":      "dev.cdevents.service.deployed.0.1.1",
      "timestamp": .body.timestamp,
    },
    "subject": {
      "id":   .body.service_url,
      "type": "service",
      "content": {
        "environment": { "id": .metadata.environment_id },
        "artifactId":  .body.artifact_id,
      }
    }
  }
}]
"""
```

### From External File

```toml
[transformers.my_transformer]
type          = "vrl"
template_file = "./transformers/my_transformer.vrl"
```

### Remote (GitHub)

```toml
[remote.community]
type  = "github"
owner = "cdviz-dev"
repo  = "transformers-community"

[transformers.github_events]
type           = "vrl"
template_rfile = "community:///github_events/to_v0_x.vrl"

[transformers.argocd]
type           = "vrl"
template_rfile = "community:///argocd_notifications/to_v0_x.vrl"
```

### Built-in Pipes

```toml
[transformers.passthrough]
type = "passthrough"

[transformers.log_events]
type   = "log"
target = "transformers::log"

[transformers.drop_all]
type = "discard_all"

[transformers.dedup]
type      = "deduplicate"
capacity  = 100
key_path  = "/context/id"
```

---

## Wiring: Source → Transformer → Sink

Sinks receive all events from the broadcast channel (fan-out). Sources push into it.
Transformers are applied per-source (pre-broadcast) or per-sink (post-broadcast).

```toml
[sources.github]
enabled          = true
transformer_refs = ["init_github", "github_events"]   # applied in order, pre-broadcast

[sources.github.extractor]
type = "webhook"
id   = "github"

[sinks.cdviz]
enabled          = true
type             = "http"
destination      = "https://cdviz.example.com/api/events"
transformer_refs = ["filter_by_type"]                 # applied post-broadcast

[transformers.init_github]
type     = "vrl"
template = """
.metadata = object(.metadata) ?? {}
[{ "metadata": merge(.metadata, {"source": "github"}), "headers": .headers, "body": .body }]
"""

[transformers.github_events]
type           = "vrl"
template_rfile = "community:///github_events/to_v0_x.vrl"

[transformers.filter_by_type]
type     = "vrl"
template = """
# only forward service events
if starts_with(string(.body.context.type) ?? "", "dev.cdevents.service.") {
  [{ "metadata": .metadata, "headers": .headers, "body": .body }]
} else {
  []
}
"""
```

---

## Environment Variable Overrides

```bash
# Format: CDVIZ_COLLECTOR__<SECTION>__<SUBSECTION>__<KEY>=value
# Double underscore (__) = nesting level

# HTTP server
CDVIZ_COLLECTOR__HTTP__PORT=9090
CDVIZ_COLLECTOR__HTTP__ROOT_URL=https://my-collector.example.com

# Enable/disable sources and sinks
CDVIZ_COLLECTOR__SOURCES__GITHUB__ENABLED=true
CDVIZ_COLLECTOR__SINKS__DATABASE__ENABLED=true

# Override nested values
CDVIZ_COLLECTOR__SINKS__DATABASE__URL=postgresql://user:pass@db:5432/cdviz
CDVIZ_COLLECTOR__SINKS__CDVIZ__DESTINATION=https://cdviz.prod.example.com/api/events
```
