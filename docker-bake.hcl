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
    {type="image" , compression="zstd", oci-mediatypes="true"},
  ]
  attest = [
    {type = "provenance", mode="max"},
    {type = "sbom"},
  ]
  platforms = [
    "linux/amd64",
    "linux/arm64",
  ]
}
