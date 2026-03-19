use crate::errors::{IntoDiagnostic, Result};
use miette::MietteDiagnostic;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use vrl::compiler::{Program, TargetValue};
use vrl::core::Value;
use vrl::diagnostic::{Diagnostic, Formatter};
use vrl::prelude::state::RuntimeState;
use vrl::prelude::{Context, TimeZone};
use vrl::value::Secrets;

pub(crate) struct RequestProgram {
    program: Program,
    source: String,
}

pub(crate) struct RequestParams {
    pub(crate) url: String,
    pub(crate) method: String,
    /// VRL-computed request headers (merged with static config headers).
    pub(crate) headers: HashMap<String, String>,
    /// Optional request body.
    pub(crate) body: Option<Vec<u8>>,
    /// Query parameters appended to the URL.
    pub(crate) query: HashMap<String, String>,
}

impl RequestProgram {
    /// Compile the VRL script that generates HTTP request parameters.
    pub(crate) fn compile(src: &str) -> Result<Self> {
        let mut fns = vrl::stdlib::all();
        fns.extend(crate::transformers::vrl_purl::all_custom_functions());
        match vrl::compiler::compile(src, &fns) {
            Err(err) => {
                let formatter = Formatter::new(src, err);
                Err(MietteDiagnostic::new(formatter.to_string()))?
            }
            Ok(res) => Ok(Self { program: res.program, source: src.to_string() }),
        }
    }

    /// Execute the program with `input` as the target value.
    ///
    /// The VRL script should mutate `.url` (required), `.method`, `.headers`,
    /// `.body`, `.query` on the target. The time window is available via
    /// `.metadata.ts_after` and `.metadata.ts_before`.
    pub(crate) fn execute(&self, input: &JsonValue) -> Result<RequestParams> {
        let mut target = TargetValue {
            value: serde_json::to_value(input).into_diagnostic()?.into(),
            metadata: Value::Object(std::collections::BTreeMap::new()),
            secrets: Secrets::default(),
        };
        let mut state = RuntimeState::default();
        let timezone = TimeZone::default();
        let mut ctx = Context::new(&mut target, &mut state, &timezone);

        self.program.resolve(&mut ctx).map_err(|cause| {
            let vrl_diag = Diagnostic::from(cause);
            let message = Formatter::new(&self.source, vec![vrl_diag]).to_string();
            MietteDiagnostic::new(message)
        })?;

        // Read the (mutated) target value to extract request parameters
        let value: JsonValue = serde_json::to_value(target.value).into_diagnostic()?;

        let url = value
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MietteDiagnostic::new(".url is required in VRL request script"))?
            .to_string();

        let method = value.get("method").and_then(|v| v.as_str()).unwrap_or("GET").to_string();

        let headers = value
            .get("headers")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        let body = value.get("body").and_then(|v| v.as_str()).map(|s| s.as_bytes().to_vec());

        let query = value
            .get("query")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        Ok(RequestParams { url, method, headers, body, query })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_compile_valid_program() {
        let result =
            RequestProgram::compile(r#".url = "https://example.com/api"; .method = "GET""#);
        assert!(result.is_ok());
    }

    #[test]
    fn test_compile_invalid_program() {
        let result = RequestProgram::compile("this is not valid vrl !!!");
        assert!(result.is_err());
    }

    #[test]
    fn test_execute_sets_url() {
        let program =
            RequestProgram::compile(r#".url = "https://example.com/api"; .method = "GET""#)
                .unwrap();
        let input = json!({ "metadata": { "ts_after": "2024-01-01T00:00:00Z" } });
        let params = program.execute(&input).unwrap();
        assert_eq!(params.url, "https://example.com/api");
        assert_eq!(params.method, "GET");
    }

    #[test]
    fn test_execute_requires_url() {
        let program = RequestProgram::compile(".method = \"GET\"").unwrap();
        let input = json!({});
        assert!(program.execute(&input).is_err());
    }

    #[test]
    fn test_execute_with_query_params() {
        let program = RequestProgram::compile(
            r#"
            .url = "https://example.com/api"
            .query.from = to_string!(.metadata.ts_after)
            "#,
        )
        .unwrap();
        let input = json!({ "metadata": { "ts_after": "2024-01-01T00:00:00Z" } });
        let params = program.execute(&input).unwrap();
        assert_eq!(params.query.get("from"), Some(&"2024-01-01T00:00:00Z".to_string()));
    }

    #[test]
    fn test_execute_default_method_is_get() {
        let program = RequestProgram::compile(r#".url = "https://example.com""#).unwrap();
        let params = program.execute(&json!({})).unwrap();
        assert_eq!(params.method, "GET");
    }

    #[test]
    fn test_execute_with_headers() {
        let program = RequestProgram::compile(
            r#"
            .url = "https://example.com"
            .headers."X-Token" = "secret"
            "#,
        )
        .unwrap();
        let params = program.execute(&json!({})).unwrap();
        assert_eq!(params.headers.get("X-Token"), Some(&"secret".to_string()));
    }
}
