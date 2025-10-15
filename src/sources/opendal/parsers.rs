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
    pub(crate) fn make_parser(
        &self,
        base_metadata: serde_json::Value,
        next: EventSourcePipe,
    ) -> Result<ParserEnum> {
        let out = match self {
            Config::CsvRow => CsvRowParser::new(base_metadata, next).into(),
            Config::Json => JsonParser::new(base_metadata, next).into(),
            Config::Jsonl => JsonlParser::new(base_metadata, next).into(),
            Config::Metadata => MetadataParser::new(base_metadata, next).into(),
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
    base_metadata: serde_json::Value,
    next: EventSourcePipe,
}

impl MetadataParser {
    fn new(base_metadata: serde_json::Value, next: EventSourcePipe) -> Self {
        Self { base_metadata, next }
    }
}

impl Parser for MetadataParser {
    async fn parse(&mut self, _op: &Operator, resource: &Resource) -> Result<()> {
        let resource_metadata = resource.as_json_metadata();
        // Merge base_metadata with resource metadata
        let mut metadata = self.base_metadata.clone();
        if let (Some(base_obj), Some(resource_obj)) =
            (metadata.as_object_mut(), resource_metadata.as_object())
        {
            for (key, value) in resource_obj {
                base_obj.insert(key.clone(), value.clone());
            }
        }
        let event = EventSource { metadata, ..Default::default() };
        self.next.send(event)
    }
}

pub(crate) struct JsonParser {
    base_metadata: serde_json::Value,
    next: EventSourcePipe,
}

impl JsonParser {
    fn new(base_metadata: serde_json::Value, next: EventSourcePipe) -> Self {
        Self { base_metadata, next }
    }
}

impl Parser for JsonParser {
    async fn parse(&mut self, op: &Operator, resource: &Resource) -> Result<()> {
        let bytes = op.read(resource.path()).await.into_diagnostic()?;
        let resource_metadata = resource.as_json_metadata();
        let headers = resource.as_headers();
        let body: serde_json::Value = serde_json::from_reader(bytes.reader()).into_diagnostic()?;

        // Merge base_metadata with resource metadata
        let mut metadata = self.base_metadata.clone();
        if let (Some(base_obj), Some(resource_obj)) =
            (metadata.as_object_mut(), resource_metadata.as_object())
        {
            for (key, value) in resource_obj {
                base_obj.insert(key.clone(), value.clone());
            }
        }

        let event = EventSource { metadata, headers, body };
        self.next.send(event)
    }
}

pub(crate) struct JsonlParser {
    base_metadata: serde_json::Value,
    next: EventSourcePipe,
}

impl JsonlParser {
    fn new(base_metadata: serde_json::Value, next: EventSourcePipe) -> Self {
        Self { base_metadata, next }
    }
}

impl Parser for JsonlParser {
    async fn parse(&mut self, op: &Operator, resource: &Resource) -> Result<()> {
        let bytes = op.read(resource.path()).await.into_diagnostic()?;
        let resource_metadata = resource.as_json_metadata();
        let headers = resource.as_headers();

        // Merge base_metadata with resource metadata once for all lines
        let mut metadata = self.base_metadata.clone();
        if let (Some(base_obj), Some(resource_obj)) =
            (metadata.as_object_mut(), resource_metadata.as_object())
        {
            for (key, value) in resource_obj {
                base_obj.insert(key.clone(), value.clone());
            }
        }

        let mut reader = bytes.reader();
        let mut buf = String::new();
        while reader.read_line(&mut buf).into_diagnostic()? > 0 {
            if buf.is_empty() {
                continue;
            }
            let body: serde_json::Value = serde_json::from_str(&buf).into_diagnostic()?;
            let event = EventSource { metadata: metadata.clone(), headers: headers.clone(), body };
            self.next.send(event)?;
        }
        Ok(())
    }
}

pub(crate) struct CsvRowParser {
    base_metadata: serde_json::Value,
    next: EventSourcePipe,
}

impl CsvRowParser {
    fn new(base_metadata: serde_json::Value, next: EventSourcePipe) -> Self {
        Self { base_metadata, next }
    }
}

impl Parser for CsvRowParser {
    async fn parse(&mut self, op: &Operator, resource: &Resource) -> Result<()> {
        use csv::Reader;

        let bytes = op.read(resource.path()).await.into_diagnostic()?;
        let mut rdr = Reader::from_reader(bytes.reader());
        let csv_headers = rdr.headers().into_diagnostic()?.clone();
        let resource_metadata = resource.as_json_metadata();
        let headers = resource.as_headers();

        // Merge base_metadata with resource metadata once for all rows
        let mut metadata = self.base_metadata.clone();
        if let (Some(base_obj), Some(resource_obj)) =
            (metadata.as_object_mut(), resource_metadata.as_object())
        {
            for (key, value) in resource_obj {
                base_obj.insert(key.clone(), value.clone());
            }
        }

        for record in rdr.records() {
            let record = record.into_diagnostic()?;
            let body =
                json!(csv_headers.iter().zip(record.iter()).collect::<HashMap<&str, &str>>());
            let event = EventSource { metadata: metadata.clone(), headers: headers.clone(), body };
            self.next.send(event)?;
        }
        Ok(())
    }
}
