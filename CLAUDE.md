# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

cdviz-collector is a Rust service and CLI tool for collecting SDLC/CI/CD events and dispatching them as CDEvents. It provides a configurable pipeline architecture that can receive events from various sources, transform them, and send them to multiple destinations.

## Development Commands

This project uses [mise-en-place](https://mise.jdx.dev/) for task management. All commands should be run via `mise`.

### Core Development Tasks

- `mise run build` - Build the project
- `mise run test` - Run all tests (unit tests + examples)
- `mise run test:unit` - Run unit tests only
- `mise run lint` - Run all linting (includes rust linting)
- `mise run lint:rust` - Run Rust-specific linting (fmt, clippy, sort)
- `mise run format` - Format code and sort dependencies
- `mise run check` - Check compilation with all feature combinations
- `mise run ci` - Run the full CI pipeline (lint, test, deny)

### Database Tasks

- `mise run db:start` - Start local PostgreSQL database
- `mise run db:stop` - Stop and remove local database
- `mise run db:prepare-offline` - Update SQLx offline mode definitions

### Security and Dependencies

- `mise run deny` - Run cargo-deny checks (licenses, advisories, bans)
- `mise run lint:dependencies` - Check for unused/outdated dependencies

### Running the Application

- `mise run run` - Start local server with example configuration
- `cargo run -- connect --config cdviz-collector.toml --directory ./examples/assets` - Basic server startup
- `cargo run -- transform --help` - Transform mode help

### Testing Examples

- `mise run examples:transform:passthrough` - Test passthrough transformation
- `mise run examples:transform:github_events` - Test GitHub events transformation
- `mise run examples:transform:kubewatch_cloudevents` - Test Kubewatch CloudEvents transformation

Use `--mode review` with example tasks to review and update expected outputs.

## Architecture

### Core Components

The application follows a pipeline architecture with three main stages:

1. **Sources** (`src/sources/`) - Input adapters that receive/collect events
   - `webhook/` - HTTP webhook endpoints
   - `opendal/` - File system and S3 sources
   - `sse/` - Server-Sent Events source for connecting to SSE endpoints
   - `extractors.rs` - Event extraction logic
   - `transformers/` - Event transformation (VRL, Handlebars)

2. **Pipes** (`src/pipes/`) - Processing middleware
   - `passthrough.rs` - Direct passthrough
   - `log.rs` - Logging pipe
   - `collect_to_vec.rs` - Collection utilities

3. **Sinks** (`src/sinks/`) - Output adapters that dispatch events
   - `db.rs` - PostgreSQL database sink
   - `http.rs` - HTTP endpoint sink
   - `folder.rs` - File system sink
   - `sse.rs` - Server-Sent Events sink for streaming events to clients
   - `debug.rs` - Debug/stdout sink

### Configuration System

- Configuration uses TOML format with environment variable overrides
- Base configuration in `src/assets/cdviz-collector.base.toml`
- Environment variables: `CDVIZ_COLLECTOR__SECTION__KEY__VALUE`
- Configuration resolved via `figment` crate with layered providers

### Message Flow

Events flow through the system as:
1. Raw input → `extractors::Extractor` 
2. Transformation via VRL or Handlebars → CDEvent
3. In-memory broadcast queue → Multiple sinks
4. Final dispatch to configured destinations

## Feature Flags

Key Cargo features that affect compilation:
- `sink_db` - PostgreSQL database sink
- `sink_folder` - File system folder sink  
- `sink_http` - HTTP endpoint sink
- `sink_sse` - Server-Sent Events sink
- `source_opendal` - OpenDAL file/S3 sources
- `source_sse` - Server-Sent Events source
- `tool_transform` - Transformation CLI tool
- `transformer_hbs` - Handlebars transformations
- `transformer_vrl` - Vector Remap Language transformations

## Testing Strategy

- Unit tests: `cargo nextest run`
- Integration tests use `rustainers` for containers
- Property-based testing with `proptest`
- Example transformation validation
- Offline SQLx mode for database tests

## Key Dependencies

- `tokio` - Async runtime
- `axum` - HTTP server framework  
- `sqlx` - Database operations
- `figment` - Configuration management
- `serde`/`serde_json` - Serialization
- `cdevents-sdk` - CDEvents specification
- `vrl` - Vector Remap Language for transformations
- `handlebars` - Template engine for transformations
- `opendal` - Unified data access layer
- `reqwest-eventsource` - Server-Sent Events client for SSE source

## Configuration Examples

See `examples/assets/` for sample configurations and test data covering various event sources and transformation scenarios.