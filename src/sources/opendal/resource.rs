use std::collections::HashMap;

use jiff::Timestamp;
use opendal::{Entry, EntryMode, Metadata, Operator};

/// Information about the Resource
/// NOTE: Resource was build from the information of the Entry, Metadata and the path
/// It's like an `OpenDAL`'s Entry but with information prefetched (`last_modified`) (no longer available via list on FS since `OpenDAL` 0.51)
/// Rebuild entry with `metadata.last_modified` if not present.
/// see [opendal::docs::rfcs::rfc\_5314\_remove\_metakey - Rust](https://docs.rs/opendal/latest/opendal/docs/rfcs/rfc_5314_remove_metakey/index.html)
#[derive(Clone, Debug)]
pub(crate) struct Resource {
    entry: Entry,
    root: String,
    last_modified: Option<Timestamp>,
    content_length: u64,
    x_headers: HashMap<String, String>,
}

impl Resource {
    pub(crate) async fn from_entry(
        op: &Operator,
        entry: Entry,
        try_to_load_headers_json: bool,
    ) -> Self {
        let mut stats: Option<Metadata> = None;
        let mut last_modified = entry.metadata().last_modified();
        let mut content_length = entry.metadata().content_length();
        let mut x_headers = HashMap::new();

        if last_modified.is_none() {
            if stats.is_none() {
                stats = op.stat(entry.path()).await.ok();
            }
            last_modified = stats.as_ref().and_then(Metadata::last_modified);
        }
        let last_modified = last_modified.map(opendal::raw::Timestamp::into_inner);
        if content_length == 0 {
            if stats.is_none() {
                stats = op.stat(entry.path()).await.ok();
            }
            content_length = stats.as_ref().map(Metadata::content_length).unwrap_or_default();
        }
        if try_to_load_headers_json && let Some(headers) = load_header_json(entry.path(), op).await
        {
            x_headers.extend(headers);
        }
        Self { entry, root: op.info().root().clone(), last_modified, content_length, x_headers }
    }

    pub(crate) fn name(&self) -> &str {
        self.entry.name()
    }

    pub(crate) fn path(&self) -> &str {
        self.entry.path()
    }

    pub(crate) fn last_modified(&self) -> Option<Timestamp> {
        self.last_modified
    }

    pub(crate) fn content_length(&self) -> u64 {
        self.content_length
    }

    pub(crate) fn is_file(&self) -> bool {
        self.entry.metadata().mode() == EntryMode::FILE
    }

    pub(crate) fn as_json_metadata(&self) -> serde_json::Value {
        let mut value = serde_json::json!({
            "name": self.name(),
            "path": self.path(),
            "root": self.root,
        });
        if let Some(last_modified) = self.last_modified() {
            value["last_modified"] = serde_json::Value::String(last_modified.to_string()); //rfc3339
        }
        value
    }

    pub(crate) fn as_headers(&self) -> HashMap<String, String> {
        self.x_headers.clone()
    }
}

async fn load_header_json(path: &str, op: &Operator) -> Option<HashMap<String, String>> {
    use bytes::Buf;

    let (base, _) = path.rsplit_once('.')?;
    let headers_path = format!("{base}.headers.json");
    if op.exists(&headers_path).await.ok()? {
        let bytes = op.read(&headers_path).await.ok()?;
        let headers: Option<HashMap<String, String>> = serde_json::from_reader(bytes.reader()).ok();
        headers
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use futures::TryStreamExt;
    use std::path::Path;

    async fn provide_op_resource(prefix: &str) -> (Operator, Resource) {
        // Create fs backend builder.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/assets/inputs");
        let builder = opendal::services::Fs::default().root(&root.to_string_lossy());
        let op: Operator = Operator::new(builder).unwrap().finish();
        let mut entries = op.lister_with(prefix).await.unwrap();
        assert2::assert!(let Ok(Some(entry)) = entries.try_next().await);
        let resource = Resource::from_entry(&op, entry, true).await;
        (op, resource)
    }

    #[tokio::test]
    async fn extract_metadata_works() {
        let (_, resource) = provide_op_resource("dir1/file01").await;
        check!(resource.is_file());
        check!(resource.name() == "file01.txt");
        check!(resource.path() == "dir1/file01.txt");
        check!(resource.content_length() > 0);
        assert2::assert!(let Some(_) = resource.last_modified());
    }

    #[tokio::test]
    async fn as_json_metadata_works() {
        let (_, resource) = provide_op_resource("dir1/file01").await;
        // Extract the metadata and check that it's what we expect
        let result = resource.as_json_metadata();
        check!(result["name"] == "file01.txt");
        check!(result["path"] == "dir1/file01.txt");
        assert2::assert!(let Some(abs_root) = result["root"].as_str());
        check!(abs_root.ends_with("examples/assets/inputs"));
        assert2::assert!(let
            Ok(_) = result["last_modified"].as_str().unwrap_or_default().parse::<Timestamp>()
        );
    }

    #[tokio::test]
    async fn as_headers_on_exists() {
        let (_, resource) = provide_op_resource("dir1/file02.txt").await;
        let result = resource.as_headers();
        assert2::assert!(let Some(value) = result.get("header1"));
        check!(value == "value1");
    }

    #[tokio::test]
    async fn as_headers_on_nonexists() {
        let (_, resource) = provide_op_resource("dir1/file01").await;
        let result = resource.as_headers();
        check!(result.is_empty());
    }

    // TODO
    // #[tokio::test]
    // async fn csv_row_via_template_works() {
    //     let (op, entry) = provide_op_entry("cdevents.").await;
    //     let dest = collect_to_vec::Processor::new();
    //     let collector = dest.collector();
    //     let sut = CsvRowParser::new(Box::new(collector));
    //     assert!(Ok(()) == sut.parse(&op, &entry).await);
    //     check!(collector.len() == 3);
    //     // TODO check!(collector[0]. == "dev".as_bytes());
    // }
}
