# Contributing

Every contribution is welcome!

By contributing to this project, you agree to the [Contributor License Agreement (CLA)](https://cla-assistant.io/cdviz-dev/cdviz-collector).

## How to contribute

### Reporting bugs

If you found a bug, please open an [issue].

### Suggesting enhancements

If you have an idea for an enhancement, please open an [issue].

### Contributing documentation

If you want to contribute documentation, please open a pull request, and start a discussion on the PR.

### Contributing code

If you want to contribute code, please open an [issue] first to discuss what you want to do. Do not open a new [issue] to work on an existing [issue]. Then fork the repository, create a branch, and open a pull request.

1. Fork the repository.
2. Create a branch for your changes.
3. Commit with `git commit -s` to sign off your work.
4. Submit a pull request.

All contributions must include a `Signed-off-by` line.

### Writing documentation

Doc comments and rustdoc should explain **why**, **how to use**, and
**purpose/goal** — not paraphrase **what** the code already says. If a
comment just restates the function/struct name in prose, it's not adding
information; skip it.

## How to build

The repository is composed of multiple subfolders / modules.

### Prerequisites

- [mise-en-place](https://mise.jdx.dev/) to download tools, setup local environment (tools, environment variables) and to run the tasks
- [docker](https://docs.docker.com/get-started/) to run the containers and to execute some tests.
  To build the container locally you have to configure the container image store
  (see [Multi-platform | Docker Docs](https://docs.docker.com/build/building/multi-platform/#prerequisites)
  & [containerd image store | Docker Docs](https://docs.docker.com/engine/storage/containerd/))

### Build

```bash
mise install

# to have list of tasks
mise tasks

# to run a task
mise run {{task}}

# to run the CI tasks
mise run ci
```

## How to test

```bash
mise run test
```

## How to release

Releases are automated, there is no manual release step:

1. `release-plz` watches `main`. After merges land, it opens (or updates) a
   "release PR" bumping crate versions and updating the changelog.
2. Merging that PR into `main` runs `release-plz`'s release job, which
   publishes the crate to crates.io and pushes a matching git tag.
3. The tag push triggers [`release.yml`](.github/workflows/release.yml)
   (via [`cargo-dist`](https://github.com/axodotdev/cargo-dist)): it builds
   the cross-platform binaries, creates the GitHub release with artifacts,
   and updates the Homebrew formula.
4. That release then triggers
   [`release-after.yml`](.github/workflows/release-after.yml), which builds
   and pushes the `cdviz-collector` and `cdviz-collector-with-transformers`
   container images to `ghcr.io`.

No manual tagging or `cargo publish` is needed or expected.

[issue]: https://github.com/cdviz-dev/cdviz-collector/issues "CDviz collector's issue tracker"
