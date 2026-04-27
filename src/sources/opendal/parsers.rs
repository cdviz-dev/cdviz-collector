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
    #[serde(alias = "text")]
    Text,
    #[serde(alias = "text_line")]
    TextLine,
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
            Config::Text => TextParser::new(base_metadata, next).into(),
            Config::TextLine => TextLineParser::new(base_metadata, next).into(),
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
    TextParser,
    TextLineParser,
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
    #[allow(clippy::too_many_lines)]
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
            Some("txt") => {
                let mut buf = String::new();
                bytes.reader().read_to_string(&mut buf).into_diagnostic()?;
                let body = super::super::format_converters::parse_text(&buf);
                let event = EventSource { metadata, headers, body };
                self.next.send(event)
            }
            Some("log") => {
                let mut reader = bytes.reader();
                let mut buf = String::new();
                while reader.read_line(&mut buf).into_diagnostic()? > 0 {
                    let line = buf.trim_end_matches('\n').trim_end_matches('\r');
                    if !line.is_empty() {
                        let body = super::super::format_converters::parse_text(line);
                        let event = EventSource {
                            metadata: metadata.clone(),
                            headers: headers.clone(),
                            body,
                        };
                        self.next.send(event)?;
                    }
                    buf.clear();
                }
                Ok(())
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
            if buf.trim().is_empty() {
                buf.clear();
                continue;
            }
            let body: serde_json::Value =
                serde_json::from_str(&buf).map_err(|cause| Error::from_serde_error(&buf, cause))?;
            let event = EventSource { metadata: metadata.clone(), headers: headers.clone(), body };
            self.next.send(event)?;
            buf.clear();
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

pub(crate) struct TextParser {
    base_metadata: serde_json::Value,
    next: EventSourcePipe,
}

impl TextParser {
    fn new(base_metadata: serde_json::Value, next: EventSourcePipe) -> Self {
        Self { base_metadata, next }
    }
}

impl Parser for TextParser {
    async fn parse(&mut self, op: &Operator, resource: &Resource) -> Result<()> {
        use std::io::Read;

        let bytes = op.read(resource.path()).await.into_diagnostic()?;
        let resource_metadata = resource.as_json_metadata();
        let headers = resource.as_headers();

        let mut buf = String::new();
        bytes.reader().read_to_string(&mut buf).into_diagnostic()?;

        let body = super::super::format_converters::parse_text(&buf);

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

pub(crate) struct TextLineParser {
    base_metadata: serde_json::Value,
    next: EventSourcePipe,
}

impl TextLineParser {
    fn new(base_metadata: serde_json::Value, next: EventSourcePipe) -> Self {
        Self { base_metadata, next }
    }
}

impl Parser for TextLineParser {
    async fn parse(&mut self, op: &Operator, resource: &Resource) -> Result<()> {
        let bytes = op.read(resource.path()).await.into_diagnostic()?;
        let resource_metadata = resource.as_json_metadata();
        let headers = resource.as_headers();

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
            let line = buf.trim_end_matches('\n').trim_end_matches('\r');
            if !line.is_empty() {
                let body = super::super::format_converters::parse_text(line);
                let event =
                    EventSource { metadata: metadata.clone(), headers: headers.clone(), body };
                self.next.send(event)?;
            }
            buf.clear();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transformers::collect_to_vec;
    use futures::TryStreamExt;
    use std::path::Path;

    /// Build a filesystem `Operator` rooted at the example input assets and return
    /// the first [`Resource`] whose path starts with `prefix`.
    async fn make_op_and_resource(prefix: &str) -> (Operator, Resource) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/assets/inputs");
        let builder = opendal::services::Fs::default().root(&root.to_string_lossy());
        let op = Operator::new(builder).unwrap().finish();
        let mut entries = op.lister_with(prefix).await.unwrap();
        let entry = entries.try_next().await.unwrap().expect("at least one entry for prefix");
        let resource = Resource::from_entry(&op, entry, false).await;
        (op, resource)
    }

    /// Write `content` to a temporary file named `filename`, build a filesystem
    /// `Operator` rooted at the temp dir, and return the operator with the
    /// corresponding [`Resource`].  Used for parsers that need specific file
    /// extensions or content not present in the example assets.
    ///
    /// The returned `TempDir` must be kept alive for the lifetime of the test.
    async fn make_tmp_op_and_resource(
        filename: &str,
        content: &[u8],
    ) -> (tempfile::TempDir, Operator, Resource) {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join(filename), content).unwrap();
        let builder = opendal::services::Fs::default().root(dir.path().to_string_lossy().as_ref());
        let op = Operator::new(builder).unwrap().finish();
        let mut entries = op.lister_with(filename).await.unwrap();
        let entry =
            entries.try_next().await.unwrap().expect("temp dir should have the written file");
        let resource = Resource::from_entry(&op, entry, false).await;
        (dir, op, resource)
    }

    #[tokio::test]
    async fn metadata_parser_emits_event_with_path() {
        let (op, resource) = make_op_and_resource("cdevents_json/artifact_published").await;
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let base_metadata = serde_json::json!({"source": "test"});
        let mut parser = MetadataParser::new(base_metadata, Box::new(collector.create_pipe()));
        parser.parse(&op, &resource).await.unwrap();

        let mut events = collector.try_into_iter().unwrap();
        let event = events.next().expect("MetadataParser should emit one event");
        // Body is empty (metadata only parser does not read file content)
        assert_eq!(event.body, serde_json::Value::Null);
        // Base metadata key is preserved
        assert_eq!(event.metadata["source"], "test");
        // Resource path is merged into metadata
        assert!(event.metadata["path"].is_string());
        assert_eq!(events.next(), None);
    }

    #[tokio::test]
    async fn json_parser_emits_event_with_json_body() {
        let (op, resource) = make_op_and_resource("cdevents_json/artifact_published").await;
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut parser = JsonParser::new(serde_json::json!({}), Box::new(collector.create_pipe()));
        parser.parse(&op, &resource).await.unwrap();

        let mut events = collector.try_into_iter().unwrap();
        let event = events.next().expect("JsonParser should emit one event");
        assert!(event.body.is_object(), "body should be a JSON object");
        // The sample file has a top-level "context" key
        assert!(event.body["context"].is_object(), "body should contain 'context'");
        assert_eq!(events.next(), None);
    }

    #[tokio::test]
    async fn auto_parser_dispatches_json_extension() {
        let (op, resource) = make_op_and_resource("cdevents_json/artifact_published").await;
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut parser = AutoParser::new(serde_json::json!({}), Box::new(collector.create_pipe()));
        parser.parse(&op, &resource).await.unwrap();

        let mut events = collector.try_into_iter().unwrap();
        let event = events.next().expect("AutoParser should emit one event for .json file");
        assert!(event.body.is_object());
        assert!(event.body["context"].is_object());
        assert_eq!(events.next(), None);
    }

    #[tokio::test]
    async fn auto_parser_dispatches_txt_as_text() {
        let (op, resource) = make_op_and_resource("dir1/file01").await;
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut parser = AutoParser::new(serde_json::json!({}), Box::new(collector.create_pipe()));
        parser.parse(&op, &resource).await.unwrap();

        let mut events = collector.try_into_iter().unwrap();
        let event = events.next().expect("AutoParser should emit one event for .txt file");
        // parse_text wraps content as {"text": "..."}
        assert!(event.body["text"].is_string(), "txt should produce {{\"text\": ...}} body");
        assert_eq!(events.next(), None);
    }

    // ── JsonlParser ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn jsonl_parser_emits_one_event_per_line() {
        let content = b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n";
        let (_dir, op, resource) = make_tmp_op_and_resource("events.jsonl", content).await;
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut parser = JsonlParser::new(serde_json::json!({}), Box::new(collector.create_pipe()));
        parser.parse(&op, &resource).await.unwrap();

        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].body, serde_json::json!({"a": 1}));
        assert_eq!(events[1].body, serde_json::json!({"b": 2}));
        assert_eq!(events[2].body, serde_json::json!({"c": 3}));
    }

    #[tokio::test]
    async fn jsonl_parser_skips_empty_lines() {
        let content = b"{\"a\":1}\n\n{\"b\":2}\n";
        let (_dir, op, resource) = make_tmp_op_and_resource("events.jsonl", content).await;
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut parser = JsonlParser::new(serde_json::json!({}), Box::new(collector.create_pipe()));
        parser.parse(&op, &resource).await.unwrap();

        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 2);
    }

    // ── CsvRowParser ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn csv_row_parser_emits_one_event_per_data_row() {
        let (op, resource) = make_op_and_resource("cdevents.csv").await;
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut parser =
            CsvRowParser::new(serde_json::json!({}), Box::new(collector.create_pipe()));
        parser.parse(&op, &resource).await.unwrap();

        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert!(!events.is_empty(), "CsvRowParser should emit at least one event");
        // Each event body should be an object keyed by the CSV header columns
        for event in &events {
            assert!(event.body.is_object(), "body should be a JSON object");
            assert!(event.body["id"].is_string(), "body should include 'id' column");
            assert!(event.body["env"].is_string(), "body should include 'env' column");
        }
    }

    #[tokio::test]
    async fn csv_row_parser_maps_headers_to_keys() {
        let content = b"name,age\nAlice,30\nBob,25\n";
        let (_dir, op, resource) = make_tmp_op_and_resource("data.csv", content).await;
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut parser =
            CsvRowParser::new(serde_json::json!({}), Box::new(collector.create_pipe()));
        parser.parse(&op, &resource).await.unwrap();

        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].body["name"], "Alice");
        assert_eq!(events[0].body["age"], "30");
        assert_eq!(events[1].body["name"], "Bob");
    }

    // ── TextParser ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn text_parser_emits_single_event_with_text_body() {
        let content = b"hello world\nsecond line\n";
        let (_dir, op, resource) = make_tmp_op_and_resource("note.txt", content).await;
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut parser = TextParser::new(serde_json::json!({}), Box::new(collector.create_pipe()));
        parser.parse(&op, &resource).await.unwrap();

        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 1, "TextParser should emit exactly one event");
        assert!(events[0].body["text"].is_string());
        let text = events[0].body["text"].as_str().unwrap();
        assert!(text.contains("hello world"));
    }

    // ── TextLineParser ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn text_line_parser_emits_one_event_per_non_empty_line() {
        let content = b"line one\nline two\n\nline four\n";
        let (_dir, op, resource) = make_tmp_op_and_resource("app.log", content).await;
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut parser =
            TextLineParser::new(serde_json::json!({}), Box::new(collector.create_pipe()));
        parser.parse(&op, &resource).await.unwrap();

        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        // Empty line is skipped → 3 events
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].body["text"], "line one");
        assert_eq!(events[1].body["text"], "line two");
        assert_eq!(events[2].body["text"], "line four");
    }

    // ── AutoParser extension dispatch ─────────────────────────────────────────

    #[tokio::test]
    async fn auto_parser_dispatches_csv_extension() {
        let content = b"x,y\n1,2\n3,4\n";
        let (_dir, op, resource) = make_tmp_op_and_resource("data.csv", content).await;
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut parser = AutoParser::new(serde_json::json!({}), Box::new(collector.create_pipe()));
        parser.parse(&op, &resource).await.unwrap();

        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 2, "AutoParser .csv should emit one event per row");
        assert_eq!(events[0].body["x"], "1");
        assert_eq!(events[1].body["x"], "3");
    }

    #[tokio::test]
    async fn auto_parser_dispatches_jsonl_extension() {
        let content = b"{\"n\":1}\n{\"n\":2}\n";
        let (_dir, op, resource) = make_tmp_op_and_resource("stream.jsonl", content).await;
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut parser = AutoParser::new(serde_json::json!({}), Box::new(collector.create_pipe()));
        parser.parse(&op, &resource).await.unwrap();

        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 2, "AutoParser .jsonl should emit one event per line");
        assert_eq!(events[0].body["n"], 1);
        assert_eq!(events[1].body["n"], 2);
    }

    #[tokio::test]
    async fn auto_parser_unknown_extension_falls_back_to_json() {
        let content = br#"{"fallback": true}"#;
        let (_dir, op, resource) = make_tmp_op_and_resource("data.xyz", content).await;
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut parser = AutoParser::new(serde_json::json!({}), Box::new(collector.create_pipe()));
        parser.parse(&op, &resource).await.unwrap();

        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].body["fallback"], true);
    }

    // ── Error paths ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn json_parser_returns_error_on_invalid_json() {
        let content = b"this is not json {{{";
        let (_dir, op, resource) = make_tmp_op_and_resource("bad.json", content).await;
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut parser = JsonParser::new(serde_json::json!({}), Box::new(collector.create_pipe()));
        let result = parser.parse(&op, &resource).await;
        assert!(result.is_err(), "JsonParser should return an error for invalid JSON");
    }

    #[tokio::test]
    async fn jsonl_parser_returns_error_on_invalid_line() {
        let content = b"{\"ok\":1}\nnot json\n";
        let (_dir, op, resource) = make_tmp_op_and_resource("bad.jsonl", content).await;
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let mut parser = JsonlParser::new(serde_json::json!({}), Box::new(collector.create_pipe()));
        let result = parser.parse(&op, &resource).await;
        assert!(result.is_err(), "JsonlParser should return an error for an invalid JSONL line");
    }

    // ── base_metadata merging ─────────────────────────────────────────────────

    #[tokio::test]
    async fn json_parser_merges_base_metadata_with_resource_metadata() {
        let content = br#"{"event": "deploy"}"#;
        let (_dir, op, resource) = make_tmp_op_and_resource("event.json", content).await;
        let collector = collect_to_vec::Collector::<EventSource>::new();
        let base = serde_json::json!({"service": "api", "env": "prod"});
        let mut parser = JsonParser::new(base, Box::new(collector.create_pipe()));
        parser.parse(&op, &resource).await.unwrap();

        let events: Vec<_> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 1);
        // base_metadata keys are present
        assert_eq!(events[0].metadata["service"], "api");
        assert_eq!(events[0].metadata["env"], "prod");
        // resource metadata (path) is merged in
        assert!(events[0].metadata["path"].is_string());
    }
}
