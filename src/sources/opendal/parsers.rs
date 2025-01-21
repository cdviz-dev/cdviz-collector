use std::collections::HashMap;

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
    #[serde(alias = "json")]
    Json,
    #[serde(alias = "metadata")]
    Metadata,
    #[serde(alias = "csv_row")]
    CsvRow,
}

impl Config {
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn make_parser(&self, next: EventSourcePipe) -> Result<ParserEnum> {
        let out = match self {
            Config::Json => JsonParser::new(next).into(),
            Config::Metadata => MetadataParser::new(next).into(),
            Config::CsvRow => CsvRowParser::new(next).into(),
        };
        Ok(out)
    }
}

#[enum_dispatch]
#[allow(clippy::enum_variant_names)]
pub(crate) enum ParserEnum {
    JsonParser,
    MetadataParser,
    CsvRowParser,
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
