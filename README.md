# cdviz-collector

keywords: `cdevents`, `sdlc`, `cicd`
status: wip

A service & cli to collect SDLC/CI/CD events and to dispatch as [cdevents].

Goals:

- to create cdevents by polling some sources (folder on fs, S3, AWS ECR, ...)
- to receive cdevents from http, kafka, nats
- to send (broadcast) cdevents to various destination database, http, kafka, nats
- to expose some metrics (TBD)

cdviz-collector is configured via a config file + override by environment variables.

see [documentation](https://cdviz.dev/docs/cdviz-collector/)

```mermaid
---
config:
  theme: 'base'
  look: 'handDrawn'
  themeVariables:
    darkMode: true
    mainBkg: '#00000000'
    background: '#00000000'
    primaryColor: '#00000000'
    primaryTextColor: '#f08c00'
    secondaryTextColor: '#f08c00'
    tertiaryTextColor: '#f08c00'
    primaryBorderColor: '#f08c00'
    secondaryBorderColor: '#f08c00'
    tertiaryBorderColor: '#f08c00'
    noteTextColor: '#f08c00'
    noteBorderColor: '#f08c00'
    lineColor: '#f08c00'
    lineWidth: 2
---
flowchart LR
  classDef future stroke-dasharray: 5 5

  q>in memory queue of cdevents]

  subgraph sources
    src_http(HTTP)
    src_fs_content(FS folder with cdevents)
    src_fs_activity(FS folder activity):::future
    src_s3_content(S3 with cdevents)
    src_s3_activity(S3 activity):::future
    src_kafka(Kafka):::future
    src_nats(NATS):::future
    src_ecr(AWS ECR):::future
    src_misc(...):::future
  end
  src_http --> q
  src_fs_content --> q
  src_fs_activity --> q
  src_s3_content --> q
  src_s3_activity --> q
  src_kafka --> q
  src_nats --> q
  src_ecr --> q
  src_misc --> q

  subgraph sinks
    sink_stdout(stdout)
    sink_db(DB)
    sink_http(HTTP)
    sink_kafka(Kafka):::future
    sink_nats(NATS):::future
  end
  q --> sink_stdout
  q --> sink_http
  q --> sink_db
  q --> sink_kafka
  q --> sink_nats
```

## License

This software is distributed under a dual-licensing model:

1. Affero General Public License Version 3.0 ([AGPL-3.0-only](LICENSE)):
  - The software is available under the AGPL 3.0 license. This means you can use, modify, and distribute the software under the terms of the AGPL 3.0. Any modifications or derivative works must also be licensed under AGPL 3.0.
  - For more details, see the [AGPL-3.0-only](LICENSE).
2. Proprietary License:
  - Alternatively, the software is available under a proprietary license. This option is suitable for users who require different terms or additional permissions not granted by the AGPL 3.0.
  - For more information about obtaining a proprietary license, please contact us at <contact@cdviz.dev>.

By using this software, you agree to comply with the terms of one of the above licenses.

For exceptions, see [LICENSING.md](LICENSING.md).

[cdevents]: <https://cdevents.dev/>
