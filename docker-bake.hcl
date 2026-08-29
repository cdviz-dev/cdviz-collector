group "default" { // build all targets in the group
  targets = ["cdviz-collector", "cdviz-collector-with-transformers"]
}

variable "VERSION" {
  default = "0.0.0"
}

variable "TRANSFORMERS_COMMUNITY_REF" {
  default = "main"
}

target "cdviz-collector" {
  target = "cdviz-collector"
  args = {
    VERSION = VERSION
  }
  tags = [
    "ghcr.io/cdviz-dev/cdviz-collector:${VERSION}",
    "ghcr.io/cdviz-dev/cdviz-collector:latest",
  ]
  output = [
    { type = "image", compression = "zstd", oci-mediatypes = "true" },
  ]
  attest = [
    { type = "provenance", mode = "max" },
    { type = "sbom" },
  ]
  platforms = [
    "linux/amd64",
    "linux/arm64",
  ]
  annotations = [
    "org.opencontainers.image.source=https://github.com/cdviz-dev/cdviz-collector",
    "org.opencontainers.image.licenses=Apache-2.0",
    "org.opencontainers.image.description=A service & cli to collect SDLC/CI/CD events and to dispatch as cdevents.",
    "org.opencontainers.image.authors=CDviz team <contact@cdviz.dev>",
  ]
}

target "cdviz-collector-with-transformers" {
  target = "cdviz-collector-with-transformers"
  args = {
    VERSION                    = VERSION
    TRANSFORMERS_COMMUNITY_REF = TRANSFORMERS_COMMUNITY_REF
  }
  # At release time (both targets built together) this substitutes the `FROM
  # ghcr.io/cdviz-dev/cdviz-collector:${VERSION}` reference with the local `cdviz-collector` target's
  # own build output, so no push-then-pull round-trip is needed even though the base isn't on ghcr.io
  # yet. When this target is built alone (e.g. the monthly transformers-refresh workflow), there's no
  # override and Docker pulls the real already-published image, which is exactly what's wanted there.
  contexts = {
    "ghcr.io/cdviz-dev/cdviz-collector:${VERSION}" = "target:cdviz-collector"
  }
  tags = [
    "ghcr.io/cdviz-dev/cdviz-collector-with-transformers:${VERSION}-t${TRANSFORMERS_COMMUNITY_REF}",
    "ghcr.io/cdviz-dev/cdviz-collector-with-transformers:latest",
  ]
  output = [
    { type = "image", compression = "zstd", oci-mediatypes = "true" },
  ]
  attest = [
    { type = "provenance", mode = "max" },
    { type = "sbom" },
  ]
  platforms = [
    "linux/amd64",
    "linux/arm64",
  ]
  annotations = [
    "org.opencontainers.image.source=https://github.com/cdviz-dev/cdviz-collector",
    "org.opencontainers.image.licenses=Apache-2.0",
    "org.opencontainers.image.description=cdviz-collector with bundled transformers-community VRL transformers.",
    "org.opencontainers.image.authors=CDviz team <contact@cdviz.dev>",
    "dev.cdviz.transformers-community.revision=${TRANSFORMERS_COMMUNITY_REF}",
  ]
}
