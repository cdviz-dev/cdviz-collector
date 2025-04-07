# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0](https://github.com/cdviz-dev/cdviz-collector/compare/0.6.4...0.7.0) - 2025-04-07

### Added

- allow CORS on http endpoint

### Fixed

- *(deps)* update rust crate vrl to 0.23

### Other

- update deny rules following update of dependencies `vrl`
- [**breaking**] move to from ApacheLicense v2.0 to AGPLv3

## [0.6.4](https://github.com/cdviz-dev/cdviz-collector/compare/0.6.3...0.6.4) - 2025-04-03

### Fixed

- kubewatch failed to generate valid cdevent (on webhook)

### Other

- try to reduce the "ci" time & re-compilation by using same
- *(deps)* upgrade rust to 1.86.0

## [0.6.3](https://github.com/cdviz-dev/cdviz-collector/compare/0.6.2...0.6.3) - 2025-04-02

### Other

- workaround to avoid expension in cat

## [0.6.2](https://github.com/cdviz-dev/cdviz-collector/compare/0.6.1...0.6.2) - 2025-04-02

### Fixed

- use `RUST_LOG` or `OTEL_LOG_LEVEL` to define log level (default to `info`) when no cli flags is defined

## [0.6.1](https://github.com/cdviz-dev/cdviz-collector/compare/0.6.0...0.6.1) - 2025-04-01

### Other

- upgrade sccache-action to fix the build of release

## [0.6.0](https://github.com/cdviz-dev/cdviz-collector/compare/0.5.1...0.6.0) - 2025-04-01

### Added

- add basic vrl template to transform cloudevent from kubewatch
- add a parser for jsonl
- allow http sink to sign request (same configuration as source)
- feat!(github): change the computation of workflow_xxx ids & name

### Fixed

- MegaLinter apply linters fixes
- pin version of transitive dependency 'time' to allow compilation of domain
- conistency of configuration for  `signature_on`
- *(deps)* update opentelemetry to 0.28

### Other

- update sccache-action (fix error or cache ?)
- use `docker bake` to build & push container
- move `signature` to module `security`
- *(deps)* update to rust 1.85.1
- Add link to documentation site
- *(renovate)* try to disable update of  just lock file
- *(deps)* update dependencies

## [0.5.1](https://github.com/cdviz-dev/cdviz-collector/compare/0.5.0...0.5.1) - 2025-03-07

### Fixed

- *(deps)* update rust crate opendal to 0.52
- *(deps)* update rust crate vrl to 0.22
- *(deps)* update opentelemetry to 0.26
- misconfiguration of the github webhook example

### Other

- *(deps)* update
- *(deps)* update rust crate rstest to 0.25
- *(deps)* update rust crate uuid to v1.15.0
- *(deps)* upgrade to rust 1.85 (edition 2024)
- format code (using rust 1.85)
- *(deps)* update rust crate uuid to v1.14.0
- *(deps)* update rust crate tempfile to v3.17.1
- try to speed-up ci workflow ([#52](https://github.com/cdviz-dev/cdviz-collector/pull/52))

## [0.5.0](https://github.com/cdviz-dev/cdviz-collector/compare/0.4.0...0.5.0) - 2025-02-09

### Added

- `transform`  command check by if a valid cdevent is generated
- include transformer files (vrl) into container

### Fixed

- transformer github_events
- apply `--directory` before loading config files
- *(deps)* update rust crates
- *(deps)* update rust crates
- *(deps)* update rust crate miette to v7.5.0

### Other

- document the task `build:container`
- *(deps)* update crates dependencies
- *(deps)* complete upgrade to rustainers 0.15

## [0.4.0](https://github.com/cdviz-dev/cdviz-collector/compare/0.3.0...0.4.0) - 2025-01-28

### Added

- *(transform)* tool transform support multi-level of folder
- *(config)* allow field of configuration to read from a file

### Fixed

- wrong secret leak

### Other

- start example to convert github events
- try to better format error reported by vrl (in log,...)
- include transformation as part of the test flow
- store examples of github_events (for examples & test)
- release & package on git tag
- *(deps)* update rust crate rustainers to 0.15

## [0.3.0](https://github.com/cdviz-dev/cdviz-collector/compare/0.2.3...0.3.0) - 2025-01-24

### Added

- [**breaking**] transformer template/script should return an array or none/null/nil
- *(webhook)* trace_id reported in json error
- *(security, webhook)* allow to verify (or reject) request against a signature
- *(webhook)* forward http's headers into the pipe
- *(webhook)* enable compression, timeout (3s) and bodylimit (1MB)
- [**breaking**] replace the single http extractor by a webhook

### Fixed

- return UNAUTHORIZE (and not FORBIDDEN) on invalid signature
- gracefull shutdown to close sources & sink on kill/ctrl-c
- content_length was zero for every resources / entry
- use Secret<T> for sensitive configuration
- *(deps)* update rust crate vrl to 0.21
- *(deps)* update rust crate handlebars to v6.3.0
- *(deps)* update rust crate serde_with to v3.12.0
- *(deps)* update rust crate init-tracing-opentelemetry to 0.25

### Other

- [MegaLinter] Apply linters fixes
- add basic test for hbs transformer
- add basic test for base pipe
- add a basic test for sink http
- add a task to estimate "test coverage"
- *(deps)* upgrade to opendal 0.51
- configure rust cli to align with rust-analyzer
- *(webhook)* enable support for multiple compression (gzip, broli, zstd, flat)
- *(deps)* upgrade cloudevents-sdk to 0.8
- *(deps)* upgrade to axum 0.8
- format import/use
- integrate `miette` for error reporting (to cont.)
- *(deps)* pin only major for version >=1.0.0
- fix configuration of release-plz
- complete the rust upgrade to 1.84
- upgrade cargo-dist
- use INTBOT app instead of the pat RELEASE_PLZ_TOKEN
- *(deps)* update dependency rust to v1.84.0
- *(deps)* update rust crate rstest to 0.24
- *(deps)* update rust crate rustainers to 0.13
- *(deps)* commit the Cargo.lock of executable
- capture samples of kubewatch events
- enable renovate as github-action

## [0.2.3](https://github.com/cdviz-dev/cdviz-collector/compare/0.2.2...0.2.3) - 2024-12-12

### Other

- fix release-after
- ci; build container by downloading pre-built binaries (vs build from source)
- try to build rust with the same toolchain
- switch to manual release (for tuning,...)
- reduce the number of call of megalinter (avoid trigger on tag)
- tune dist/release to focus on what is needed (and reduce cost)

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
