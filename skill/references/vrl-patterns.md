# VRL Patterns for cdviz-collector Transformers

VRL (Vector Remap Language) is a safe, sandboxed expression language. Transformers receive `{ .body, .metadata, .headers }` and must return an array.

## Input / Output Contract

```
Input:  { .body: any, .metadata: object, .headers: object }
Output: [{ "body": ..., "metadata": ..., "headers": ... }, ...]
        []  → discard (no CDEvent emitted)
        [a, b]  → emit two CDEvents from one input
```

## Error Handling: `!` vs `??`

| Syntax             | Meaning                          | Use when                        |
| ------------------ | -------------------------------- | ------------------------------- |
| `fn!()`            | Abort transformer on error       | Field must exist; missing = bug |
| `fn() ?? fallback` | Use fallback if fn returns error | Field is optional               |
| `\|\|`             | Null / falsy coalescing          | Field may be null/missing       |
| `exists(.path)`    | Check presence before access     | Conditional branching           |

```vrl
# ! = fail loudly
name = to_string!(.body.required_field)
ts   = format_timestamp!(ts, format: "%+")

# ?? = fallback on Result error (NOT on null)
ts = parse_timestamp(.body.created_at, "%+") ?? now()
s  = string(.body.optional_str) ?? "default"

# || = null / falsy fallback
name = .body.name || "unnamed"
env  = .metadata.environment_id || "/unknown"

# exists = presence check
if exists(.body.workflow_run) {
  # safe to access .body.workflow_run.*
}
```

**Common mistake**: `?? "default"` does NOT work on plain field access (`.body.foo ?? "default"` fails compile if `.body.foo` is infallible). Use `string(.body.foo) ?? "default"` instead — `string()` returns a `Result`.

## Timestamp Handling

Always extract from source data (never `now()`) for reproducible outputs:

```vrl
# Parse then reformat to ISO 8601
ts = parse_timestamp(.body.created_at, "%+") ?? now()
ts = format_timestamp!(ts, format: "%+")

# Unix epoch to ISO
ts = from_unix_timestamp!(.body.epoch_ms, unit: "milliseconds")
ts = format_timestamp!(ts, format: "%+")

# Common format strings
# "%+"  → ISO 8601 / RFC 3339  (2024-01-15T12:00:00Z)
# "%s"  → Unix seconds
# "%Y-%m-%dT%H:%M:%SZ"  → explicit ISO
```

## String Coercion

```vrl
s = to_string!(.body.value)           # any → string, fails on error
s = string(.body.value) ?? "default"  # string → Result, use ?? for fallback
s = to_string(.body.value) ?? ""      # same but accepts any type
```

## Array Operations

```vrl
# Collect multiple events
output = []
if exists(.body.workflow_run) {
  output = push(output, { "body": { ... }, "metadata": .metadata, "headers": .headers })
}
if exists(.body.deployment) {
  output = push(output, { "body": { ... }, "metadata": .metadata, "headers": .headers })
}
output  # return the array

# Map over array
containers = map_values(array!(.body.containers || [])) -> |container| {
  { "name": container.name, "image": container.image }
}

# Filter array
active = filter(array!(.body.items || [])) -> |_index, item| {
  item.status == "active"
}

# Filter nulls
clean = filter(array) -> |_index, item| { !is_nullish(item) }
```

## Object Operations

```vrl
# Merge objects (second wins on conflict)
merged = merge(.metadata, { "environment_id": "/prod" })

# Safe object init (metadata may be null from some extractors)
.metadata = object(.metadata) ?? {}

# Nested object construction
custom_data = {
  "github": {
    "repository": { "url": .body.repository.url },
    "sender":     { "login": .body.sender.login },
  }
}
```

## Transformer Chaining via Metadata

Pass data between transformers using `.metadata`:

```vrl
# Transformer 1: inject environment
[{
  "metadata": merge(.metadata, { "environment_id": "/staging" }),
  "headers":  .headers,
  "body":     .body,
}]

# Transformer 2: use injected value
env_id = .metadata.environment_id || "/unknown"
```

## Metadata Init Pattern

Use as first transformer when sharing init logic across sources:

```toml
[transformers.init_github]
type = "vrl"
template = """
.metadata = object(.metadata) ?? {}
[{
  "metadata": merge(.metadata, {
    "environment_id": "/production",
  }),
  "headers": .headers,
  "body":    .body,
}]
"""
```

## Type Detection (Webhook Event Routing)

```vrl
# By header (GitHub)
event_type = string(.headers["X-GitHub-Event"]) ?? ""

# By field existence
if exists(.body.workflow_run) { ... }
else if exists(.body.pull_request) { ... }
else if exists(.body.package) { ... }

# By value
if .body.action == "published" { ... }
else if includes(["opened", "reopened"], .body.action) { ... }
```

## Complete Minimal Transformer

```vrl
# Converts arbitrary JSON to a pipelineRun:finished CDEvent
ts = parse_timestamp(.body.finished_at, "%+") ?? now()
ts = format_timestamp!(ts, format: "%+")

[{
  "metadata": .metadata,
  "headers":  .headers,
  "body": {
    "context": {
      "version":   "0.4.1",
      "id":        "0",
      "source":    .metadata.context.source,
      "type":      "dev.cdevents.pipelinerun.finished.0.2.0",
      "timestamp": ts,
    },
    "subject": {
      "id":   to_string!(.body.html_url),
      "type": "pipelineRun",
      "content": {
        "pipelineName": string(.body.name) ?? "unknown",
        "url":          to_string!(.body.html_url),
        "outcome":      if .body.conclusion == "success" { "success" } else { "failure" },
        "errors":       "",
      }
    },
    "customData": {
      "myservice": .body
    }
  }
}]
```

## TOML: Inline vs File

```toml
# Inline template
[transformers.my_event]
type     = "vrl"
template = """
[{ "metadata": .metadata, "headers": .headers, "body": .body }]
"""

# External file
[transformers.my_event]
type          = "vrl"
template_file = "./transformers/my_event.vrl"

# Remote file (GitHub)
[remote.community]
type  = "github"
owner = "cdviz-dev"
repo  = "transformers-community"

[transformers.github_events]
type           = "vrl"
template_rfile = "community:///github_events/to_v0_x.vrl"
```

## Common Pitfalls

| Mistake                       | Fix                                                                   |
| ----------------------------- | --------------------------------------------------------------------- |
| `return [{...}]`              | Just write `[{...}]` — last expression is the return value            |
| `?? "default"` on plain field | `string(.field) ?? "default"`                                         |
| Forgetting `context.id = "0"` | Always set it; omitting produces invalid CDEvents                     |
| Using `now()` for timestamps  | Extract from input for reproducibility                                |
| Setting `subject.source`      | Don't — deprecated in CDEvents 0.6+; use `subject.id` globally unique |
| Multi-value TOML in template  | Use individual dotted keys, not JSON blobs                            |
