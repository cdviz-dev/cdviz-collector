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

use super::ParserConfig;

/// Where the response of a request should go once fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Route {
    /// Parse the response body and emit `EventSource`(s) downstream (default).
    #[default]
    Pipeline,
    /// Feed the response back into the driver to compute further requests.
    Feedback,
    /// Both emit downstream **and** feed back into the driver (e.g. GraphQL:
    /// emit `nodes` while feeding `pageInfo` back for cursor pagination).
    Both,
}

impl Route {
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "feedback" => Self::Feedback,
            "both" => Self::Both,
            _ => Self::Pipeline,
        }
    }

    /// Whether the response is parsed and emitted downstream.
    pub(crate) fn emits(self) -> bool {
        matches!(self, Self::Pipeline | Self::Both)
    }

    /// Whether the response is fed back into the driver for further requests.
    pub(crate) fn feeds_back(self) -> bool {
        matches!(self, Self::Feedback | Self::Both)
    }
}

/// A single HTTP request emitted by the driver, with its routing decision.
pub(crate) struct RequestSpec {
    pub(crate) url: String,
    pub(crate) method: String,
    /// VRL-computed request headers (merged with static config headers).
    pub(crate) headers: HashMap<String, String>,
    /// Optional request body.
    pub(crate) body: Option<Vec<u8>>,
    /// Query parameters appended to the URL.
    pub(crate) query: HashMap<String, String>,
    /// What to do with the response.
    pub(crate) route: Route,
    /// Optional per-request parser override (falls back to the source default).
    pub(crate) parser: Option<ParserConfig>,
}

/// The result of one driver invocation.
pub(crate) struct DriverOutput {
    /// New state snapshot, cloned into every request below. `Null` when the
    /// driver did not set `.state`.
    pub(crate) state: JsonValue,
    /// Requests to enqueue. May be empty (terminates the branch).
    pub(crate) requests: Vec<RequestSpec>,
}

pub(crate) struct DriverProgram {
    program: Program,
    source: String,
}

impl DriverProgram {
    /// Compile the VRL script that drives HTTP requests.
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
    /// The VRL script receives a target shaped as:
    /// ```json
    /// {
    ///   "metadata": { "ts_after": "...", "ts_before": "...", ... },
    ///   "state":    <value> | null,
    ///   "request":  { "url": "...", "method": "...", "headers": {...} } | null,
    ///   "response": { "status": 200, "headers": {...}, "body": <value> } | null
    /// }
    /// ```
    /// and must set `.requests` to an array of request objects (each with a
    /// required `url` plus optional `method`, `headers`, `body`, `query`,
    /// `route`, `parser`). It may set `.state` to any value, carried (as an
    /// immutable snapshot) into each emitted request and handed back to the
    /// driver when that request's response feeds back.
    pub(crate) fn execute(&self, input: &JsonValue) -> Result<DriverOutput> {
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

        // Read the (mutated) target value to extract the driver output.
        let value: JsonValue = serde_json::to_value(target.value).into_diagnostic()?;

        let new_state = value.get("state").cloned().unwrap_or(JsonValue::Null);

        let requests = value
            .get("requests")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                MietteDiagnostic::new(".requests (array) is required in VRL driver script")
            })?
            .iter()
            .filter_map(Self::parse_request_spec)
            .collect();

        Ok(DriverOutput { state: new_state, requests })
    }

    /// Convert one `.requests[]` entry into a [`RequestSpec`]. Entries missing a
    /// string `url` are dropped (with a warning) rather than aborting the poll.
    fn parse_request_spec(spec: &JsonValue) -> Option<RequestSpec> {
        let Some(url) = spec.get("url").and_then(|v| v.as_str()) else {
            tracing::warn!(?spec, "driver request entry missing string .url, skipped");
            return None;
        };
        let url = url.to_string();

        let method = spec.get("method").and_then(|v| v.as_str()).unwrap_or("GET").to_string();

        let headers = string_map(spec.get("headers"));
        let query = string_map(spec.get("query"));

        let body = spec.get("body").and_then(|v| v.as_str()).map(|s| s.as_bytes().to_vec());

        let route =
            spec.get("route").and_then(|v| v.as_str()).map_or(Route::default(), Route::parse);

        let parser = spec
            .get("parser")
            .cloned()
            .and_then(|v| serde_json::from_value::<ParserConfig>(v).ok());

        Some(RequestSpec { url, method, headers, body, query, route, parser })
    }
}

/// Extract a `{string: string}` map from an optional JSON object value.
fn string_map(value: Option<&JsonValue>) -> HashMap<String, String> {
    value
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn single_request(out: &DriverOutput) -> &RequestSpec {
        assert_eq!(out.requests.len(), 1, "expected exactly one request");
        &out.requests[0]
    }

    #[test]
    fn test_compile_valid_program() {
        let result = DriverProgram::compile(r#".requests = [{ "url": "https://example.com" }]"#);
        assert!(result.is_ok());
    }

    #[test]
    fn test_compile_invalid_program() {
        let result = DriverProgram::compile("this is not valid vrl !!!");
        assert!(result.is_err());
    }

    #[test]
    fn test_execute_sets_url_and_default_method() {
        let program =
            DriverProgram::compile(r#".requests = [{ "url": "https://example.com/api" }]"#)
                .unwrap();
        let input = json!({ "metadata": { "ts_after": "2024-01-01T00:00:00Z" } });
        let out = program.execute(&input).unwrap();
        let req = single_request(&out);
        assert_eq!(req.url, "https://example.com/api");
        assert_eq!(req.method, "GET");
        assert_eq!(req.route, Route::Pipeline);
        assert!(req.parser.is_none());
        assert!(out.state.is_null());
    }

    #[test]
    fn test_execute_requires_requests() {
        let program = DriverProgram::compile(r#".state = { "x": 1 }"#).unwrap();
        assert!(program.execute(&json!({})).is_err());
    }

    #[test]
    fn test_execute_with_query_from_window() {
        let program = DriverProgram::compile(
            r#"
            .requests = [{
                "url": "https://example.com/api",
                "query": { "from": to_string!(.metadata.ts_after) }
            }]
            "#,
        )
        .unwrap();
        let input = json!({ "metadata": { "ts_after": "2024-01-01T00:00:00Z" } });
        let req_query = single_request(&program.execute(&input).unwrap()).query.clone();
        assert_eq!(req_query.get("from"), Some(&"2024-01-01T00:00:00Z".to_string()));
    }

    #[test]
    fn test_execute_with_headers() {
        let program = DriverProgram::compile(
            r#".requests = [{ "url": "https://example.com", "headers": { "X-Token": "secret" } }]"#,
        )
        .unwrap();
        let req = single_request(&program.execute(&json!({})).unwrap()).headers.clone();
        assert_eq!(req.get("X-Token"), Some(&"secret".to_string()));
    }

    #[test]
    fn test_execute_route_and_parser_and_state() {
        let program = DriverProgram::compile(
            r#"
            .state = { "cursor": "abc" }
            .requests = [{ "url": "https://example.com", "route": "both", "parser": "jsonl" }]
            "#,
        )
        .unwrap();
        let out = program.execute(&json!({})).unwrap();
        assert_eq!(out.state, json!({ "cursor": "abc" }));
        let req = single_request(&out);
        assert_eq!(req.route, Route::Both);
        assert_eq!(req.parser, Some(ParserConfig::Jsonl));
    }

    #[test]
    fn test_execute_feedback_reads_response() {
        let program = DriverProgram::compile(
            r#"
            cursor = .response.body.next
            if cursor != null {
                .requests = [{ "url": "https://example.com?cursor=" + string!(cursor), "route": "feedback" }]
            } else {
                .requests = []
            }
            "#,
        )
        .unwrap();
        let input = json!({ "response": { "body": { "next": "p2" } } });
        let out = program.execute(&input).unwrap();
        let req = single_request(&out);
        assert_eq!(req.url, "https://example.com?cursor=p2");
        assert_eq!(req.route, Route::Feedback);

        let stop = json!({ "response": { "body": { "next": null } } });
        assert!(program.execute(&stop).unwrap().requests.is_empty());
    }

    /// The driver snippets documented in the module README must compile.
    #[test]
    fn test_readme_examples_compile() {
        let examples = [
            // single request
            r#".requests = [{ "url": "https://api.example.com/events" }]"#,
            // Link-header pagination
            r#"
            if .response == null {
                .requests = [{ "url": "https://api.example.com/events", "route": "both" }]
            } else {
                link = string(.response.headers.link) ?? ""
                matched = parse_regex(link, r'<(?P<next>[^>]+)>;\s*rel="next"') ?? {}
                if exists(matched.next) {
                    .requests = [{ "url": matched.next, "route": "both" }]
                } else {
                    .requests = []
                }
            }
            "#,
            // multi-pass discovery -> detail
            r#"
            if .response == null {
                .requests = [{ "url": "https://api.example.com/jobs", "route": "feedback" }]
            } else {
                reqs = []
                for_each(array!(.response.body)) -> |_index, job| {
                    reqs = push(reqs, {
                        "url": "https://api.example.com/jobs/" + string!(job.id),
                        "route": "pipeline"
                    })
                }
                .requests = reqs
            }
            "#,
            // GraphQL cursor pagination
            r#"
            query = {
              "query": "query($after:String){ builds(after:$after){ nodes{ id } pageInfo{ endCursor hasNextPage } } }",
              "variables": { "after": null }
            }
            if .response != null {
                query.variables.after = .response.body.data.builds.pageInfo.endCursor
            }
            cont = if .response == null {
                true
            } else {
                .response.body.data.builds.pageInfo.hasNextPage == true
            }
            if cont {
                .requests = [{
                    "url": "https://api.example.com/graphql",
                    "method": "POST",
                    "headers": { "content-type": "application/json" },
                    "body": encode_json(query),
                    "route": "both"
                }]
            } else {
                .requests = []
            }
            "#,
        ];
        for src in examples {
            assert!(DriverProgram::compile(src).is_ok(), "README example failed to compile: {src}");
        }
    }

    #[test]
    fn test_bad_request_entries_are_skipped() {
        let program = DriverProgram::compile(
            r#".requests = [{ "method": "GET" }, { "url": "https://ok.example.com" }]"#,
        )
        .unwrap();
        let out = program.execute(&json!({})).unwrap();
        let req = single_request(&out);
        assert_eq!(req.url, "https://ok.example.com");
    }
}
