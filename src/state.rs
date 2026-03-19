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
        use std::str::FromStr;
        let scheme = opendal::Scheme::from_str(&self.kind).into_diagnostic()?;
        opendal::Operator::via_iter(scheme, self.parameters.clone()).into_diagnostic()
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
