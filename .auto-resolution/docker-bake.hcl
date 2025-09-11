group "default" { // build all targets in the group
  targets = ["cdviz-collector"]
}

variable "VERSION" {
  default = "0.0.0"
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
