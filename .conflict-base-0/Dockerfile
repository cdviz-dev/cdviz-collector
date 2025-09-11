# hadolint global ignore=DL3006,DL3008,DL3003
# see [Multi-platform | Docker Docs](https://docs.docker.com/build/building/multi-platform/)
# see [Fast multi-arch Docker build for Rust projects - DEV Community](https://dev.to/vladkens/fast-multi-arch-docker-build-for-rust-projects-an1)
# see [How to create small Docker images for Rust](https://kerkour.com/rust-small-docker-image)
# alternative:
# - https://edu.chainguard.dev/chainguard/chainguard-images/reference/rust/overview
# - https://images.chainguard.dev/directory/image/static/overview

#---------------------------------------------------------------------------------------------------
# Buinding 'build' on CI takes ~30min
# Rust compilation in docker is slow (especially for arm64)
# and the setup/tune of cache/sccache to speed up is not trivial
# vs using using pre-built binaries for releases (built in 6 min for each platform)
#
# We keep this target for reference
#
# checkov:skip=CKV_DOCKER_7:Ensure the base image uses a non latest version tag
# trivy:ignore:AVD-DS-0001
FROM --platform=$BUILDPLATFORM rust:1.89.0-alpine AS build
ARG PROFILE=release

ENV PKG_CONFIG_SYSROOT_DIR=/
RUN <<EOT
  set -eux
  # musl-dev is required for the musl target
  # zig + cargo-zigbuild are used to build cross platform C code
  # make is used by jmealloc and some C code
  apk add --no-cache musl-dev zig make # openssl-dev
  update-ca-certificates
EOT

RUN <<EOT
  set -eux
  rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
  cargo install --locked cargo-zigbuild
EOT

# Create appuser
ENV USER=nonroot
ENV UID=10001

RUN adduser \
  --disabled-password \
  --gecos "" \
  --home "/nonexistent" \
  --shell "/sbin/nologin" \
  --no-create-home \
  --uid "${UID}" \
  "${USER}"

WORKDIR /work

COPY ./ .

# reduce size of target (good for caching)
ENV CARGO_PROFILE_TEST_DEBUG=0
# disable incremental compilation for faster from-scratch builds
ENV CARGO_INCREMENTAL=0
# TODO `upx /work/target/*/${PROFILE}/cdviz-collector`
RUN <<EOT
  set -eux
  cargo zigbuild --locked --target x86_64-unknown-linux-musl --target aarch64-unknown-linux-musl "--$PROFILE"
  mkdir -p /app/linux
  ls target
  cp "target/aarch64-unknown-linux-musl/${PROFILE}/cdviz-collector" /app/linux/arm64
  cp "target/x86_64-unknown-linux-musl/${PROFILE}/cdviz-collector" /app/linux/amd64
EOT

HEALTHCHECK NONE

#---------------------------------------------------------------------------------------------------
# Instead of building from source, download the binary from github release
# Buinding 'download' on CI takes ~2min
#
# checkov:skip=CKV_DOCKER_7:Ensure the base image uses a non latest version tag
# trivy:ignore:AVD-DS-0001
FROM --platform=$BUILDPLATFORM alpine:3 AS download
ARG VERSION

RUN <<EOT
  set -eux
  apk add --no-cache --no-check-certificate ca-certificates curl tar xz
  update-ca-certificates
EOT

# Create appuser
ENV USER=nonroot
ENV UID=10001

RUN adduser \
  --disabled-password \
  --gecos "" \
  --home "/nonexistent" \
  --shell "/sbin/nologin" \
  --no-create-home \
  --uid "${UID}" \
  "${USER}"

WORKDIR /work

RUN <<EOT
  set -eux
  mkdir -p /app/linux

  mkdir x86_64
  cd x86_64
  curl -L -o cdviz-collector.tar.xz "https://github.com/cdviz-dev/cdviz-collector/releases/download/$VERSION/cdviz-collector-x86_64-unknown-linux-musl.tar.xz"
  tar -xvf cdviz-collector.tar.xz --strip-components=1
  mv cdviz-collector /app/linux/amd64
  cd ..
  # ldd /app/linux/amd64

  mkdir aarch64
  cd aarch64
  curl -L -o cdviz-collector.tar.xz "https://github.com/cdviz-dev/cdviz-collector/releases/download/$VERSION/cdviz-collector-aarch64-unknown-linux-musl.tar.xz"
  tar -xvf cdviz-collector.tar.xz --strip-components=1
  mv cdviz-collector /app/linux/arm64
  cd ..
  # ldd /app/linux/arm64
EOT

HEALTHCHECK NONE

#---------------------------------------------------------------------------------------------------
# checkov:skip=CKV_DOCKER_7:Ensure the base image uses a non latest version tag
# trivy:ignore:AVD-DS-0001
# TARGETPLATFORM usage to copy right binary from builder stage
# ARG populated by docker itself

# !! musl is not fully staticlly linked (ldd =>  /lib/ld-musl-x86_64.so.1) So scratch and chainguard/static do not work as is
# issue above seems to be fixed by `rustflags = ["-C", "target-feature=+crt-static"]` in `.cargo/config.toml`
# but keep the comments for reminder

FROM scratch AS cdviz-collector
# # use chainguard if need certificates,...
# FROM --platform=$BUILDPLATFORM cgr.dev/chainguard/static:latest AS cdviz-collector
# # use alpine if need certificates, lib/ld-musl-x86_64.so.1, shell,...
# FROM --platform=$BUILDPLATFORM alpine:3 AS cdviz-collector

LABEL org.opencontainers.image.source="https://github.com/cdviz-dev/cdviz-collector"
LABEL org.opencontainers.image.licenses="Apache-2.0"
LABEL org.opencontainers.image.description="A service & cli to collect SDLC/CI/CD events and to dispatch as cdevents."

ARG TARGETPLATFORM

# COPY --from=build /etc/passwd /etc/passwd
# COPY --from=build /etc/group /etc/group
# USER nonroot
# COPY --from=build /app/${TARGETPLATFORM} /app

COPY --from=download /etc/passwd /etc/passwd
COPY --from=download /etc/group /etc/group
USER nonroot
COPY --from=download /app/${TARGETPLATFORM} /usr/local/bin/cdviz-collector
# use wildcard to copy/follow all files from the directory and not copy as symlinks
COPY ./config/transformers/* /etc/cdviz-collector/transformers/.

ENV \
  OTEL_EXPORTER_OTLP_TRACES_ENDPOINT="http://127.0.0.1:4317" \
  OTEL_TRACES_SAMPLER="always_off"
HEALTHCHECK NONE
#see https://stackoverflow.com/questions/21553353/what-is-the-difference-between-cmd-and-entrypoint-in-a-dockerfile
ENTRYPOINT ["/usr/local/bin/cdviz-collector"]
CMD []
