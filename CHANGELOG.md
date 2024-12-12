# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.2](https://github.com/cdviz-dev/cdviz-collector/compare/0.2.1...0.2.2) - 2024-12-12

### Other

- build & push container after the release
- try to trigger `release.yml` from `release-plz.yml`

## [0.2.1](https://github.com/cdviz-dev/cdviz-collector/compare/0.2.0...0.2.1) - 2024-12-12

### Other

- try to fix concurrent update of rustup
- use a non default GITHUB_TOKEN to trigger workflow
- release v0.2.0

## [0.2.0](https://github.com/cdviz-dev/cdviz-collector/releases/tag/0.2.0) - 2024-12-12

### Added

- add the transformer [Vector Remap Language (VRL)](https://vector.dev/docs/reference/vrl/)
- introduce the special `Pipe` collect_to_vec to use for test
- run transformers from cli on local fs with 3 modes
- support to exclude path_pattern when starts with '!'
- [**breaking**] introduce subcommand into the cli of cdviz-collector
- *(cdviz-collector)* allow to define transformers by reference (name)
- feat!(cdviz-db): use the stored procedure `store_cdevent` instead of direct sql insert
- *(cdviz-collector)* replace "0" by a CID in `context.id`
- add cli options to change the current working directory
- add a sink `folder` to write event into a folder (local, S3, ...)
- *(cdviz-collector)* allow to override configuration's entry with environment variable
- *(cdiz-collector)* allow to embed a defautl configuration with `disabled` sinks and sources

### Fixed

- configuration of deny
- configuration of test db
- minor upgrade and fix of warning
- usage of init_tracing_opentelemetry following upgrade
- *(deps)* update rust crate clap-verbosity-flag to v3
- *(deps)* update opentelemetry to 0.23
- hbs transformer and example
- *(deps)* align version of handlebars and handlebars_misc_helpers
- *(deps)* update rust crate tracing-opentelemetry-instrumentation-sdk to 0.21
- *(deps)* update rust crate opendal to 0.50
- *(deps)* update rust crate init-tracing-opentelemetry to 0.21
- *(deps)* update rust crate axum-tracing-opentelemetry to 0.21
- *(deps)* update rust crate init-tracing-opentelemetry to 0.20
- *(deps)* update rust crate opendal to 0.49 (#101)
- *(deps)* update rust crate handlebars to v6
- *(deps)* update rust crate sqlx to 0.8
- rename 'cargo-binstall' into 'binstall' to use the plugin and not the spcial behavior of mise

### Other

- configure to release for "aarch64-unknown-linux-musl"
- fix linter warning
- update README
- try to fix build
- prepare workflow to release with release-plz + cargo-dist
- import ci, and various shared config from cdviz
- publish container & chart to oci registry with version from from git
- build a multi-platform (x86_64 & arm64) container for cdviz-collector
- migrate from taskfile to `mise run`
- use `"` in `.mise.toml` & switch to `aqua:...task`
- replace home made asdf plugin by aqua and download from github
- rebrand reference from `davidB` by `cdviz-dev` & bump to 0.2.0
- group transformer under dedicated module and features flags for dependencies
- *(ui)* replace `println` by better output with cliclack
- introduce PathExt, to avoid duplication of small function about Path
- add vscode configuration to launch debugger
- rename folder `opendal_fs` into `inputs`  under `examples/assets`
- extract `resolve_transformer_refs`
- Add subcommant "transform" (nothing done)
- *(deps)* update dependency rust to v1.83.0
- replace `this_error` by `derive_more`
- update configuration of cargo-deny (for `all-features`)
- configure clippy more stricly
- *(deps)* update dependencies
- fix alignement of rust version (else break ci)
- *(deps)* update dependency rust to v1.82.0
- format
- *(deps)* bump `serde_with` and the description
- add a default configuration for sink `cdevents_local_json`
- move the sink `http` behind a feature `sink_http`
- *(deps)* update serde_with
- fix typo
- format
- fix security/linter warning
- fix security/linter warning
- use `docker buildx` and explicit platform
- switch to container from scratch
- fix build of container with chainguard
- *(deps)* update rust crate rstest to 0.23.0
- source flow with extractor and transformer (#131)
- add rules about dependencies version, licenses
- *(deps)* downgrade rustainer to avoid dependencies version conflict with opentelemetry.
- avoid conflict between sccache from mise and from github workflow
- *(deps)* update rust crate rustainers to 0.13
- reformat code
- add custom configuration for clippy and rustfmt
- build(deps) upgrade to cdevents-sdk 0.1
- upgrade rust version into container (align with the build stack)
- change the license from AGPL-3.0-or-later to Apache-2.0
- *(cdviz-collector)* use reqwest-middleware with cloudevents
- *(deps)* update rust crate rstest to 0.22.0
- disable run of `check'
- update test to reflect change in  transformer
- *(deps)* upgrade to rust 1.80.1
- 🚧 (cdviz-collector) enhance transformer/executor
- 👷 format build task
- 👷 try to build faster (linker + cache)
- ⬆️ (cdviz-collector) upgrade openetelemetry stack
- ✅ ignore files from demos
- Update Rust crate rstest to 0.21.0
- Update Rust crate handlebars_misc_helpers to 0.16
- ✨  cdviz-collector introduce transformers for opendal source ([#60](https://github.com/cdviz-dev/cdviz-collector/pull/60))
- 💚 fix upgrade to opendal 0.46
- ⬆️  update opendal to 0.46
- 🚧 fix compilation issue on cdviz-collector (2)
- 🚧 fix compilation issue on cdviz-collector
- Update Rust crate rstest to 0.19.0 ([#61](https://github.com/cdviz-dev/cdviz-collector/pull/61))
- Update Rust crate serde_with to 3.8.1 ([#66](https://github.com/cdviz-dev/cdviz-collector/pull/66))
- ✨ (cdviz-collectopr) http sink sends cloudevents [#21](https://github.com/cdviz-dev/cdviz-collector/pull/21) ([#57](https://github.com/cdviz-dev/cdviz-collector/pull/57))
- ✨ (cdviz-collector) opendal source support path's pattern and recursive
- 🔊 (cdviz-collector) add debug log
- Update Rust crate rustainers to 0.12
- Update Rust crate tokio to 1.37
- Update Rust crate clap-verbosity-flag to 2.2.0
- Update Rust crate serde_with to 3.7
- Update Rust crate tokio to 1.36
- 🚨 apply clippy suggestions
- 👷 update ci  to use taskfile and reflect the project split
- ✅ add basic test to write into db
- ⬆️ use rust 1.77.0
- ♻️ migrate froim justfile to taskfile
- ♻️ move all kubernetes/docker code from top level to sub folders
- ♻️ cdviz-collector: move all (rust) code under cdviz-collector folder
- Update Rust crate serde_with to 3.7
- Update Rust crate clap-verbosity-flag to 2.2.0
- Update Rust crate axum-tracing-opentelemetry to 0.18
- Update axum-tracing-opentelemetry requirement from 0.16 to 0.17
- ✨ cdviz-collector use logfmt as logging format
- 🚨 fix scope of rust's feature flag
- 🚨 apply clippy' suggestions
- 💥 merge cdviz-sensors into cdviz-collector
- 🎨 add missing info, reorder key,...
- 🚧 cdviz-watcher watch local folder
- 🗃️  define a cdevents lake table to store incoming cdevents as json
- ✅ update test for parallele execution
- 📦 deploy postgresql as part of helm chart + setup cdviz-collector to connect to DB
- 🗃️ introduce storage with postgresql in rust code (with sqlx)
- 👷 add kubernetes setup (tools, helm chart, skaffold)
- 🚧 introduce tracing & rebrand `cdviz-svc` into `cdviz-collector`
