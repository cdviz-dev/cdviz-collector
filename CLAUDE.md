# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

cdviz-collector is a Rust (edition 2024) service and CLI tool for collecting SDLC/CI/CD events and dispatching them as CDEvents. It provides a configurable pipeline architecture that can receive events from various sources, transform them, and send them to multiple destinations.

### Operation Modes

cdviz-collector operates in three modes:
1. **Server mode** (`connect`) - Long-running service connecting sources to sinks
2. **One-shot mode** (`send`) - Direct event sending for testing/scripting
3. **Batch transformation** (`transform`) - Offline file transformation using configured transformers

### Workspace Structure

- **`cdviz-collector`** (root) - Main binary crate
- **`testkit/`** - Testing utility crate for connector chain integration tests
- **`transformers/`** - Git submodule pointing to community transformers repository (VRL-based, see `transformers/AGENTS.md` for conventions)

## Development Commands

This project uses [mise-en-place](https://mise.jdx.dev/) for task management. Run `mise tasks` for all available tasks.

### Core Development Tasks

- `mise run build` - Build the project
- `mise run test` - Run all tests (unit tests + examples)
- `mise run test:unit` - Run unit tests only
- `mise run lint` - Run all linting
- `mise run lint:rust` - Run Rust-specific linting (fmt, clippy, sort)
- `mise run format` - Format code and sort dependencies
- `mise run autofix` - Auto-fix linting issues and format code
- `mise run check` - Check compilation with all feature combinations
- `mise run ci` - Run the full CI pipeline (lint, test, deny)

### Running a Single Test

```bash
# By name pattern (preferred, uses nextest)
cargo nextest run test_name_pattern

# Run ignored/integration tests
cargo nextest run --run-ignored only
```

### Database Tasks

- `mise run db:start` - Start local PostgreSQL database
- `mise run db:stop` - Stop and remove local database
- `mise run db:prepare-offline` - Update SQLx offline mode definitions (`.sqlx/` directory)

### Running the Application

- `mise run run` - Start local server with example configuration
- `cargo run -- connect --config cdviz-collector.toml --directory ./examples/assets` - Basic server startup
- `cargo run -- transform --help` - Transform mode help

### Testing Examples

- `mise run examples:transform:passthrough` - Test passthrough transformation
- `mise run examples:transform:github_events` - Test GitHub events transformation

Use `--mode review` with example tasks to review and update expected outputs.

### Additional Testing Tasks

- `mise run test:ignored` - Run ignored/integration tests only
- `mise run test:coverage` - Generate test coverage report (outputs to `target/test-coverage/html/`)
- `mise run examples:httpyac` - Run httpyac tests on example files
- `mise run examples:debug-formats` - Demonstrate debug sink format options (rust_debug vs json)

## Architecture

### Pipeline Flow

Events flow through a three-stage pipeline:

1. **Sources** (`src/sources/`) - Input adapters: webhook (HTTP), opendal (filesystem/S3), SSE, Kafka, CLI
   - `extractors.rs` - Event extraction logic
   - `transformers/` - VRL transformation engine
2. **Pipes** (`src/pipes/`) - Processing middleware (passthrough, log, collect, discard)
3. **Sinks** (`src/sinks/`) - Output adapters: PostgreSQL (db), ClickHouse, HTTP, folder, SSE, Kafka, debug

### Message Flow

Raw input → `extractors::Extractor` → Transformation (VRL) → CDEvent → In-memory broadcast queue → Multiple sinks

### Configuration System

- TOML format with layered providers via `figment`
- Base configuration: `src/assets/cdviz-collector.base.toml`
- Environment variable overrides: `CDVIZ_COLLECTOR__SECTION__KEY__VALUE`
- See `examples/assets/` for sample configurations

## Feature Flags

Key Cargo features (combine with `_all` suffixes for groups):

- `sink_clickhouse`, `sink_db`, `sink_folder`, `sink_http`, `sink_kafka`, `sink_sse`
- `source_opendal`, `source_sse`, `source_kafka`
- `parser_tap`, `parser_xml`, `parser_yaml` (also: json, jsonl, csv_row, text, text_line built-in)
- `tool_transform` - Batch transformation CLI tool
- `transformer_vrl` - Vector Remap Language transformations

## Code Quality Constraints

- `unsafe_code = "forbid"` - No unsafe code allowed
- No `unwrap()` or `expect()` in non-test code (use proper error handling)
- No `print_stdout` or `dbg_macro` in production code
- Clippy pedantic + perf lints enabled
- Commits require sign-off: `git commit -s` (DCO)

## Testing Strategy

- Unit tests inline with `#[cfg(test)]`
- `cargo nextest run` as primary test runner
- Integration tests use `rustainers`/`testcontainers` for containers
- Connector chain testing via `testkit` crate
- Snapshot testing with `insta`
- Property-based testing with `proptest`
- Offline SQLx mode for database tests (`.sqlx/` query cache)

## Key Dependencies

- `tokio`/`axum` - Async runtime and HTTP server
- `sqlx` - PostgreSQL with offline mode
- `figment` - Configuration management
- `cdevents-sdk` - CDEvents specification
- `vrl` - Vector Remap Language for transformations
- `opendal` - Unified data access layer (S3, filesystem, GitHub, etc.)
- `rdkafka` - Kafka support
- `insta` - Snapshot testing
