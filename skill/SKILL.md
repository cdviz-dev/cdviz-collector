---
name: cdviz-collector
description: Help configure cdviz-collector pipelines, write and debug VRL transformers, map events to CDEvent types, and run/debug the collector. Use when working with cdviz-collector TOML configs, VRL transformation templates, CDEvents schema, or the connect/send/transform CLI commands.
metadata:
  author: cdviz-dev
  version: "1.0"
license: Apache-2.0
---

# cdviz-collector Skill

cdviz-collector collects SDLC/CI/CD events from various sources and dispatches them as [CDEvents](https://cdevents.dev). Configuration is TOML. Transformations use VRL (Vector Remap Language).

## Operation Modes

| Command                                   | Purpose                                             |
| ----------------------------------------- | --------------------------------------------------- |
| `cdviz-collector connect --config <file>`            | Long-running server: sources → transformers → sinks        |
| `cdviz-collector send <event-type>`                  | One-shot event dispatch (testing/scripting)                |
| `cdviz-collector transform --help`                   | Offline batch transformation of files                      |
| `cdviz-collector config --config <file> --check`     | Validate config + compile VRL templates (pre-flight check) |
| `cdviz-collector config --config <file> --print`     | Print resolved config as TOML (debug env var overrides)    |


---

## VRL Transformers

Transformers live in `[transformers.<name>]` blocks and are referenced by sources/sinks via `transformer_refs = ["name1", "name2"]`.

### Template Structure

A transformer **must return an array** of `{ metadata, headers, body }` objects. Return `[]` to discard.

```toml
[transformers.my_transformer]
type = "vrl"
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
      "timestamp": .body.updated_at,
    },
    "subject": {
      "id":   .body.service_url,
      "type": "service",
      "content": {
        "environment": { "id": .metadata.environment_id },
        "artifactId":  .body.artifact_purl,
      }
    }
  }
}]
"""
```

### Critical Rules

| Rule                | Detail                                                             |
| ------------------- | ------------------------------------------------------------------ |
| `context.id = "0"`  | Always — lets collector generate content-based IDs                 |
| `context.source`    | Use `.metadata.context.source` (injected by collector)             |
| `context.timestamp` | Extract from input, never use `now()` — needed for reproducibility |
| `subject.source`    | **Do not set** — deprecated; make `subject.id` globally unique     |
| Return type         | Always array `[{...}]`, never a bare object                        |

### Error Handling

```vrl
# ! = fail loudly (abort transformer)
ts = parse_timestamp!(.body.updated_at, "%+")
s  = to_string!(.body.name)

# ?? = fallback for Result/error (NOT null coalescing)
ts = parse_timestamp(.body.updated_at, "%+") ?? now()
s  = string(.body.optional_field) ?? "default"

# exists() = check before accessing
if exists(.body.workflow_run) {
  # safe to use .body.workflow_run.*
}

# Null coalescing uses ||
name = .body.name || "unnamed"
```

### Timestamp Pattern

```vrl
ts = parse_timestamp(.body.created_at, "%+") ?? now()
ts = format_timestamp!(ts, format: "%+")
# then use `ts` in context.timestamp
```

### Metadata Init (first transformer in a chain)

```toml
[transformers.init_metadata]
type = "vrl"
template = """
.metadata = object(.metadata) ?? {}
[{
  "metadata": merge(.metadata, {
    "environment_id": "/cluster/prod",
    "context": { "source": "https://cdviz-collector.example.com/?source=github" }
  }),
  "headers": .headers,
  "body":    .body,
}]
"""
```

### Remote Transformer (from transformers-community)

```toml
[remote.transformers-community]
type  = "github"
owner = "cdviz-dev"
repo  = "transformers-community"

[transformers.github_events]
type          = "vrl"
template_rfile = "transformers-community:///github_events/to_v0_x.vrl"
```

### Built-in Pipe Transformers

```toml
type = "passthrough"   # no-op
type = "log"           # log event to stdout
type = "discard_all"   # drop all events
type = "deduplicate"   # deduplicate by key_path
```

---

## TOML Configuration

### Sources

```toml
# Webhook (receives HTTP POST at /webhook/<id>)
[sources.github]
enabled = true
transformer_refs = ["github_events"]

[sources.github.extractor]
type = "webhook"
id   = "github"       # → POST /webhook/github

# File / S3 / GitHub polling
[sources.local_events]
enabled = true

[sources.local_events.extractor]
type             = "opendal"
kind             = "fs"         # fs | s3 | github | ...
polling_interval = "30s"
parameters       = { root = "./inputs" }
recursive        = false
path_patterns    = ["**/*.json"]
parser           = "json"       # json | jsonl | csv_row | yaml | xml | tap | text

# HTTP REST API polling (with pagination + rate-limit handling)
[sources.github_api]
enabled = true

[sources.github_api.extractor]
type                 = "http_polling"
url                  = "https://api.github.com/repos/org/repo/events"
polling_interval     = "60s"
follow_link_header   = true     # follow RFC 5988 Link: next headers
min_request_interval = "1s"     # rate limit guard
parser               = "jsonl"
```

### Sinks

```toml
# HTTP webhook out
[sinks.cdviz]
enabled                  = true
type                     = "http"
destination              = "https://cdviz.example.com/api/events"
total_duration_of_retries = "30m"
[sinks.cdviz.headers]
"Authorization" = { type = "static", value = "Bearer ${CDVIZ_TOKEN}" }

# PostgreSQL
[sinks.database]
enabled              = true
type                 = "db"
url                  = "postgresql://user:pass@localhost:5432/cdviz?search_path=cdviz"
pool_connections_min = 0
pool_connections_max = 10

# Local folder (JSON files)
[sinks.local]
enabled    = false
type       = "folder"
kind       = "fs"
parameters = { root = "./sink" }

# Debug (stdout)
[sinks.debug]
enabled = true
type    = "debug"
# format = "json"  # or "rust_debug" (default)
```

### Environment Variable Overrides

```bash
# Pattern: CDVIZ_COLLECTOR__<SECTION>__<KEY>
CDVIZ_COLLECTOR__HTTP__PORT=9090
CDVIZ_COLLECTOR__SINKS__DATABASE__ENABLED=true
CDVIZ_COLLECTOR__SINKS__DATABASE__URL=postgresql://...
CDVIZ_COLLECTOR__SOURCES__GITHUB__ENABLED=false
```

---

## CDEvent Types

| Subject        | Predicates                                                   |
| -------------- | ------------------------------------------------------------ |
| `environment`  | `created`, `modified`, `deleted`                             |
| `service`      | `deployed`, `upgraded`, `rolledback`, `removed`, `published` |
| `build`        | `queued`, `started`, `finished`                              |
| `artifact`     | `packaged`, `signed`, `published`, `downloaded`, `deleted`   |
| `incident`     | `detected`, `reported`, `resolved`                           |
| `ticket`       | `created`, `updated`, `closed`                               |
| `pipelineRun`  | `queued`, `started`, `finished`                              |
| `taskRun`      | `started`, `finished`                                        |
| `repository`   | `created`, `modified`, `deleted`                             |
| `branch`       | `created`, `deleted`                                         |
| `change`       | `created`, `reviewed`, `merged`, `abandoned`, `updated`      |
| `testCaseRun`  | `queued`, `started`, `finished`, `skipped`                   |
| `testSuiteRun` | `queued`, `started`, `finished`                              |
| `testOutput`   | `published`                                                  |

Event type string format: `dev.cdevents.<subject>.<predicate>.<version>`
Example: `dev.cdevents.service.deployed.0.1.1`

---

## CDEvent Field Conventions

### `context.source` — Event origin URI

Use the URI of the cdviz-collector instance (not the triggering system):

```
✅ "https://cdviz-collector.example.com/?source=github_webhook"
✅ "https://jenkins.example.com/job/my-pipeline"
❌ "github.com/myorg/myrepo"   # too generic
```

Use `.metadata.context.source` in VRL (injected from `http.root_url` config).

### `subject.id` — Globally unique subject identifier

Prefer API URIs or absolute paths:

```
✅ "https://api.github.com/repos/org/repo/actions/runs/12345"
✅ "/cluster/prod/namespace/my-service"
✅ "pkg:oci/my-app@sha256:abc123...?repository_url=ghcr.io/org/my-app"
❌ "run-12345"        # not globally unique
❌ "production"       # too generic
```

### `environment.id` — Deployment environment

Absolute path, hierarchical, consistent:

```
✅ "/production"
✅ "/pro/us-east-1/cluster-a"
✅ "/staging"
❌ "prod"             # not an absolute path
```

### `artifactId` — Package URL (PURL)

```
OCI:    pkg:oci/my-app@sha256:abc123...?repository_url=ghcr.io/org/my-app&tag=v1.2
NPM:    pkg:npm/lodash@4.17.21
Maven:  pkg:maven/org.springframework/spring-core@5.3.10
Generic: pkg:generic/my-tool@1.0.0
```

For OCI: use image digest (not git SHA). `pkg:oci/` has no namespace — use `repository_url` query param.

### `customData` — Source-specific data

```json
{
  "customData": {
    "github": { "repository": { "url": "..." }, "sender": { "login": "..." } },
    "links": [
      {
        "linkKind": "storedAt",
        "target": { "subject": { "type": "repository", "id": "..." } }
      }
    ]
  }
}
```

---

## Debug & Run

```bash
# Inspect resolved config (merges base + file + env vars), print as TOML
cdviz-collector config --config cdviz-collector.toml --print

# Print raw config before remote/file adapters resolve paths
cdviz-collector config --config cdviz-collector.toml --print-raw

# Validate config + compile all VRL templates (catch errors before running)
cdviz-collector config --config cdviz-collector.toml --check

# Check config for a specific subcommand's base (connect | send | transform)
cdviz-collector config --config cdviz-collector.toml --check --for send

# Override a key inline while checking
cdviz-collector config --config cdviz-collector.toml --check --set sources.github.enabled=true

# Start server
cdviz-collector connect --config cdviz-collector.toml

# Test a transformer offline
cdviz-collector transform \
  --transformer ./my-transformer.vrl \
  --input ./inputs/event.json

# Health check (server running)
curl http://localhost:8080/healthz
curl http://localhost:8080/readyz

# Send a test event to webhook
curl -X POST http://localhost:8080/webhook/000 \
  -H "Content-Type: application/json" \
  -d @examples/assets/inputs/cdevents_json/service_deployed.json
```

---

## Reference Files

For detailed guides, see:

- [VRL patterns and gotchas](references/vrl-patterns.md) — functions, error handling, common patterns
- [Config examples](references/config-examples.md) — full TOML examples for each source/sink type
