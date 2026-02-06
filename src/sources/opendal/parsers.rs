use std::{collections::HashMap, io::BufRead};

use crate::{
    errors::{Error, IntoDiagnostic, Result},
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
    #[serde(alias = "auto")]
    Auto,
    #[serde(alias = "csv_row")]
    CsvRow,
    #[serde(alias = "json")]
    Json,
    #[serde(alias = "jsonl")]
    Jsonl,
    #[serde(alias = "metadata")]
    Metadata,
    #[cfg(feature = "parser_xml")]
    #[serde(alias = "xml")]
    Xml,
    #[cfg(feature = "parser_yaml")]
    #[serde(alias = "yaml")]
    Yaml,
    #[cfg(feature = "parser_tap")]
    #[serde(alias = "tap")]
    Tap,
}

impl Config {
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn make_parser(
        &self,
        base_metadata: serde_json::Value,
        next: EventSourcePipe,
    ) -> Result<ParserEnum> {
        let out = match self {
            Config::Auto => AutoParser::new(base_metadata, next).into(),
            Config::CsvRow => CsvRowParser::new(base_metadata, next).into(),
            Config::Json => JsonParser::new(base_metadata, next).into(),
            Config::Jsonl => JsonlParser::new(base_metadata, next).into(),
            Config::Metadata => MetadataParser::new(base_metadata, next).into(),
            #[cfg(feature = "parser_xml")]
            Config::Xml => XmlParser::new(base_metadata, next).into(),
            #[cfg(feature = "parser_yaml")]
            Config::Yaml => YamlParser::new(base_metadata, next).into(),
            #[cfg(feature = "parser_tap")]
            Config::Tap => TapParser::new(base_metadata, next).into(),
        };
        Ok(out)
    }
}

#[enum_dispatch]
#[allow(clippy::enum_variant_names)]
pub(crate) enum ParserEnum {
    AutoParser,
    CsvRowParser,
    JsonParser,
    JsonlParser,
    MetadataParser,
    #[cfg(feature = "parser_xml")]
    XmlParser,
    #[cfg(feature = "parser_yaml")]
    YamlParser,
    #[cfg(feature = "parser_tap")]
    TapParser,
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

pub(crate) struct AutoParser {
    base_metadata: serde_json::Value,
    next: EventSourcePipe,
}

impl AutoParser {
    fn new(base_metadata: serde_json::Value, next: EventSourcePipe) -> Self {
        Self { base_metadata, next }
    }
}

impl Parser for AutoParser {
    async fn parse(&mut self, op: &Operator, resource: &Resource) -> Result<()> {
        use std::io::Read;

        // Detect format from file extension
        let path_str = resource.path();
        let extension = std::path::Path::new(path_str).extension().and_then(|e| e.to_str());

        // Read file data once
        let bytes = op.read(path_str).await.into_diagnostic()?;
        let resource_metadata = resource.as_json_metadata();
        let headers = resource.as_headers();

        // Merge base_metadata with resource metadata
        let mut metadata = self.base_metadata.clone();
        if let (Some(base_obj), Some(resource_obj)) =
            (metadata.as_object_mut(), resource_metadata.as_object())
        {
            for (key, value) in resource_obj {
                base_obj.insert(key.clone(), value.clone());
            }
        }

        // Parse based on extension
        match extension {
            Some("json") => {
                let mut buf = String::new();
                bytes.reader().read_to_string(&mut buf).into_diagnostic()?;
                let body: serde_json::Value = serde_json::from_str(&buf)
                    .map_err(|cause| Error::from_serde_error(&buf, cause))?;
                let event = EventSource { metadata, headers, body };
                self.next.send(event)
            }
            Some("jsonl") => {
                let mut reader = bytes.reader();
                let mut buf = String::new();
                while reader.read_line(&mut buf).into_diagnostic()? > 0 {
                    if buf.trim().is_empty() {
                        buf.clear();
                        continue;
                    }
                    let body: serde_json::Value = serde_json::from_str(&buf)
                        .map_err(|cause| Error::from_serde_error(&buf, cause))?;
                    let event =
                        EventSource { metadata: metadata.clone(), headers: headers.clone(), body };
                    self.next.send(event)?;
                    buf.clear();
                }
                Ok(())
            }
            Some("csv") => {
                use csv::Reader;
                let mut rdr = Reader::from_reader(bytes.reader());
                let csv_headers = rdr.headers().into_diagnostic()?.clone();
                for record in rdr.records() {
                    let record = record.into_diagnostic()?;
                    let body = json!(
                        csv_headers.iter().zip(record.iter()).collect::<HashMap<&str, &str>>()
                    );
                    let event =
                        EventSource { metadata: metadata.clone(), headers: headers.clone(), body };
                    self.next.send(event)?;
                }
                Ok(())
            }
            #[cfg(feature = "parser_xml")]
            Some("xml") => {
                let mut buf = String::new();
                bytes.reader().read_to_string(&mut buf).into_diagnostic()?;
                let body = super::super::format_converters::parse_xml(&buf)?;
                let event = EventSource { metadata, headers, body };
                self.next.send(event)
            }
            #[cfg(feature = "parser_yaml")]
            Some("yaml" | "yml") => {
                let mut buf = String::new();
                bytes.reader().read_to_string(&mut buf).into_diagnostic()?;
                let body = super::super::format_converters::parse_yaml(&buf)?;
                let event = EventSource { metadata, headers, body };
                self.next.send(event)
            }
            #[cfg(feature = "parser_tap")]
            Some("tap") => {
                let mut buf = String::new();
                bytes.reader().read_to_string(&mut buf).into_diagnostic()?;
                let body = super::super::format_converters::parse_tap(&buf)?;
                let event = EventSource { metadata, headers, body };
                self.next.send(event)
            }
            _ => {
                // Default fallback to JSON
                let mut buf = String::new();
                bytes.reader().read_to_string(&mut buf).into_diagnostic()?;
                let body: serde_json::Value = serde_json::from_str(&buf)
                    .map_err(|cause| Error::from_serde_error(&buf, cause))?;
                let event = EventSource { metadata, headers, body };
                self.next.send(event)
            }
        }
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
        use std::io::Read;
        let bytes = op.read(resource.path()).await.into_diagnostic()?;
        let resource_metadata = resource.as_json_metadata();
        let headers = resource.as_headers();
        let mut buf = String::new();
        bytes.reader().read_to_string(&mut buf).into_diagnostic()?;
        let body: serde_json::Value =
            serde_json::from_str(&buf).map_err(|cause| Error::from_serde_error(&buf, cause))?;

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
            let body: serde_json::Value =
                serde_json::from_str(&buf).map_err(|cause| Error::from_serde_error(&buf, cause))?;
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

#[cfg(feature = "parser_xml")]
pub(crate) struct XmlParser {
    base_metadata: serde_json::Value,
    next: EventSourcePipe,
}

#[cfg(feature = "parser_xml")]
impl XmlParser {
    fn new(base_metadata: serde_json::Value, next: EventSourcePipe) -> Self {
        Self { base_metadata, next }
    }
}

#[cfg(feature = "parser_xml")]
impl Parser for XmlParser {
    async fn parse(&mut self, op: &Operator, resource: &Resource) -> Result<()> {
        use std::io::Read;

        // Read file bytes
        let bytes = op.read(resource.path()).await.into_diagnostic()?;
        let resource_metadata = resource.as_json_metadata();
        let headers = resource.as_headers();

        // Read to string
        let mut buf = String::new();
        bytes.reader().read_to_string(&mut buf).into_diagnostic()?;

        // Convert XML to JSON using shared format converter
        let body = super::super::format_converters::parse_xml(&buf)?;

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

#[cfg(feature = "parser_yaml")]
pub(crate) struct YamlParser {
    base_metadata: serde_json::Value,
    next: EventSourcePipe,
}

#[cfg(feature = "parser_yaml")]
impl YamlParser {
    fn new(base_metadata: serde_json::Value, next: EventSourcePipe) -> Self {
        Self { base_metadata, next }
    }
}

#[cfg(feature = "parser_yaml")]
impl Parser for YamlParser {
    async fn parse(&mut self, op: &Operator, resource: &Resource) -> Result<()> {
        use std::io::Read;

        // Read file bytes
        let bytes = op.read(resource.path()).await.into_diagnostic()?;
        let resource_metadata = resource.as_json_metadata();
        let headers = resource.as_headers();

        // Read to string
        let mut buf = String::new();
        bytes.reader().read_to_string(&mut buf).into_diagnostic()?;

        // Convert YAML to JSON using shared format converter
        let body = super::super::format_converters::parse_yaml(&buf)?;

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

#[cfg(feature = "parser_tap")]
pub(crate) struct TapParser {
    base_metadata: serde_json::Value,
    next: EventSourcePipe,
}

#[cfg(feature = "parser_tap")]
impl TapParser {
    fn new(base_metadata: serde_json::Value, next: EventSourcePipe) -> Self {
        Self { base_metadata, next }
    }
}

#[cfg(feature = "parser_tap")]
impl Parser for TapParser {
    async fn parse(&mut self, op: &Operator, resource: &Resource) -> Result<()> {
        use std::io::Read;

        // Read file bytes
        let bytes = op.read(resource.path()).await.into_diagnostic()?;
        let resource_metadata = resource.as_json_metadata();
        let headers = resource.as_headers();

        // Read to string
        let mut buf = String::new();
        bytes.reader().read_to_string(&mut buf).into_diagnostic()?;

        // Convert TAP to JSON using shared format converter
        let body = super::super::format_converters::parse_tap(&buf)?;

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

// #[cfg(test)]
// mod tests {
//     use crate::errors::Error;
//     //use miette::IntoDiagnostic;

//     #[test]
//     fn test_parse_json_with_null() {
//         let input = r#"
//         {
//           "a_null": null
//         }
//         "#;
//         let _body: serde_json::Value = serde_json::from_str(&input)
//             .map_err(|cause| Error::from_serde_error(input, cause))
//             //.into_diagnostic()
//             .unwrap();
//     }
// }
