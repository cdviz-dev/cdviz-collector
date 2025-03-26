use std::{collections::HashMap, io::BufRead};

use crate::{
    errors::{IntoDiagnostic, Result},
    sources::{EventSource, EventSourcePipe},
};
use bytes::Buf;
use enum_dispatch::enum_dispatch;
use opendal::Operator;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::Resource;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) enum Config {
    #[serde(alias = "csv_row")]
    CsvRow,
    #[serde(alias = "json")]
    Json,
    #[serde(alias = "jsonl")]
    Jsonl,
    #[serde(alias = "metadata")]
    Metadata,
}

impl Config {
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn make_parser(&self, next: EventSourcePipe) -> Result<ParserEnum> {
        let out = match self {
            Config::CsvRow => CsvRowParser::new(next).into(),
            Config::Json => JsonParser::new(next).into(),
            Config::Jsonl => JsonlParser::new(next).into(),
            Config::Metadata => MetadataParser::new(next).into(),
        };
        Ok(out)
    }
}

#[enum_dispatch]
#[allow(clippy::enum_variant_names)]
pub(crate) enum ParserEnum {
    CsvRowParser,
    JsonParser,
    JsonlParser,
    MetadataParser,
}

#[enum_dispatch(ParserEnum)]
pub(crate) trait Parser {
    async fn parse(&mut self, op: &Operator, resource: &Resource) -> Result<()>;
}

pub(crate) struct MetadataParser {
    next: EventSourcePipe,
}

impl MetadataParser {
    fn new(next: EventSourcePipe) -> Self {
        Self { next }
    }
}

impl Parser for MetadataParser {
    async fn parse(&mut self, _op: &Operator, resource: &Resource) -> Result<()> {
        let metadata = resource.as_json_metadata();
        let event = EventSource { metadata, ..Default::default() };
        self.next.send(event)
    }
}

pub(crate) struct JsonParser {
    next: EventSourcePipe,
}

impl JsonParser {
    fn new(next: EventSourcePipe) -> Self {
        Self { next }
    }
}

impl Parser for JsonParser {
    async fn parse(&mut self, op: &Operator, resource: &Resource) -> Result<()> {
        let bytes = op.read(resource.path()).await.into_diagnostic()?;
        let metadata = resource.as_json_metadata();
        let body: serde_json::Value = serde_json::from_reader(bytes.reader()).into_diagnostic()?;
        let event = EventSource { metadata, body, ..Default::default() };
        self.next.send(event)
    }
}

pub(crate) struct JsonlParser {
    next: EventSourcePipe,
}

impl JsonlParser {
    fn new(next: EventSourcePipe) -> Self {
        Self { next }
    }
}

impl Parser for JsonlParser {
    async fn parse(&mut self, op: &Operator, resource: &Resource) -> Result<()> {
        let bytes = op.read(resource.path()).await.into_diagnostic()?;
        let metadata = resource.as_json_metadata();
        let mut reader = bytes.reader();
        let mut buf = String::new();
        while reader.read_line(&mut buf).into_diagnostic()? > 0 {
            if buf.is_empty() {
                continue;
            }
            let body: serde_json::Value = serde_json::from_str(&buf).into_diagnostic()?;
            let event = EventSource { metadata: metadata.clone(), body, ..Default::default() };
            self.next.send(event)?;
        }
        Ok(())
    }
}

pub(crate) struct CsvRowParser {
    next: EventSourcePipe,
}

impl CsvRowParser {
    fn new(next: EventSourcePipe) -> Self {
        Self { next }
    }
}

impl Parser for CsvRowParser {
    async fn parse(&mut self, op: &Operator, resource: &Resource) -> Result<()> {
        use csv::Reader;

        let bytes = op.read(resource.path()).await.into_diagnostic()?;
        let mut rdr = Reader::from_reader(bytes.reader());
        let headers = rdr.headers().into_diagnostic()?.clone();
        let metadata = resource.as_json_metadata();
        for record in rdr.records() {
            let record = record.into_diagnostic()?;
            let body = json!(headers.iter().zip(record.iter()).collect::<HashMap<&str, &str>>());
            let event = EventSource { metadata: metadata.clone(), body, ..Default::default() };
            self.next.send(event)?;
        }
        Ok(())
    }
}
