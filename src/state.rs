use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Config {
    pub(crate) kind: String,
    pub(crate) parameters: HashMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            kind: "fs".to_string(),
            parameters: HashMap::from([(
                "root".to_string(),
                "./.cdviz-collector/state".to_string(),
            )]),
        }
    }
}

#[cfg(feature = "state")]
impl Config {
    pub(crate) fn make_operator(&self) -> crate::errors::Result<opendal::Operator> {
        use crate::errors::IntoDiagnostic;
        opendal::Operator::via_iter(&self.kind, self.parameters.clone()).into_diagnostic()
    }
}

#[cfg(feature = "state")]
pub(crate) async fn load_ts_after(
    op: &opendal::Operator,
    source_name: &str,
) -> Option<jiff::Timestamp> {
    let path = format!("{source_name}/checkpoint.json");
    let bytes = op.read(&path).await.ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes.to_bytes()).ok()?;
    let ts_str = value.get("ts_after")?.as_str()?;
    ts_str.parse().ok()
}

#[cfg(feature = "state")]
pub(crate) async fn save_ts_after(
    op: &opendal::Operator,
    source_name: &str,
    ts: jiff::Timestamp,
) -> crate::errors::Result<()> {
    use crate::errors::IntoDiagnostic;
    let path = format!("{source_name}/checkpoint.json");
    let value = serde_json::json!({ "ts_after": ts.to_string() });
    let bytes = serde_json::to_vec(&value).into_diagnostic()?;
    op.write(&path, bytes).await.into_diagnostic().map(|_| ())
}

#[cfg(test)]
#[allow(clippy::mixed_attributes_style)]
mod tests {
    #![cfg(feature = "state")]
    use super::*;
    use assert2::check;

    fn make_op() -> (tempfile::TempDir, opendal::Operator) {
        let dir = tempfile::TempDir::new().unwrap();
        let builder = opendal::services::Fs::default().root(dir.path().to_string_lossy().as_ref());
        let op = opendal::Operator::new(builder).unwrap();
        (dir, op)
    }

    #[tokio::test]
    async fn round_trips_saved_timestamp() {
        let (_dir, op) = make_op();
        let ts: jiff::Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
        save_ts_after(&op, "src1", ts).await.unwrap();
        check!(load_ts_after(&op, "src1").await == Some(ts));
    }

    #[tokio::test]
    async fn missing_checkpoint_returns_none() {
        let (_dir, op) = make_op();
        check!(load_ts_after(&op, "never-written").await == None);
    }

    #[tokio::test]
    async fn corrupt_json_returns_none() {
        let (_dir, op) = make_op();
        op.write("src1/checkpoint.json", b"not json".to_vec()).await.unwrap();
        check!(load_ts_after(&op, "src1").await == None);
    }

    #[tokio::test]
    async fn missing_ts_after_key_returns_none() {
        let (_dir, op) = make_op();
        op.write("src1/checkpoint.json", b"{\"other\":\"field\"}".to_vec()).await.unwrap();
        check!(load_ts_after(&op, "src1").await == None);
    }

    #[tokio::test]
    async fn unparseable_ts_after_returns_none() {
        let (_dir, op) = make_op();
        op.write("src1/checkpoint.json", b"{\"ts_after\":\"not-a-timestamp\"}".to_vec())
            .await
            .unwrap();
        check!(load_ts_after(&op, "src1").await == None);
    }
}
