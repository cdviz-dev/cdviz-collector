//! Allowlisted replacement for VRL's `get_env_var`.
//!
//! The collector's own secrets (DB url, HMAC tokens, S3 keys, ...) are commonly passed as
//! environment variables, and transformers can come from remote, unpinned sources
//! (`github://`). The stock `get_env_var` would let any transformer read — and forward to
//! a sink — any of them. This version only returns variables whose name matches one of the
//! glob patterns in `vrl.allowed_env_vars`; any other name fails like an unset variable, so
//! `get_env_var("X") ?? "default"` keeps working.

use crate::errors::{IntoDiagnostic, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::sync::{Arc, RwLock};
use vrl::compiler::prelude::*;

// ponytail: process-wide, set from the loaded config (one config per process); thread it
// through the transformer builders if several configs must coexist in one process.
static ALLOWED_ENV_VARS: RwLock<Option<Arc<GlobSet>>> = RwLock::new(None);

/// Replace the allowlist used by `get_env_var` in VRL programs compiled from now on.
pub(crate) fn set_allowed_env_vars(patterns: &[String]) -> Result<()> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).into_diagnostic()?);
    }
    let set = Arc::new(builder.build().into_diagnostic()?);
    *ALLOWED_ENV_VARS.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(set);
    Ok(())
}

fn allowed_env_vars() -> Arc<GlobSet> {
    ALLOWED_ENV_VARS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .unwrap_or_else(|| Arc::new(GlobSet::empty()))
}

/// VRL functions available to every program: stdlib with `get_env_var` swapped for the
/// allowlisted one, plus the custom PURL functions.
pub(crate) fn functions() -> Vec<Box<dyn Function>> {
    let mut fns = vrl::stdlib::all();
    fns.retain(|f| f.identifier() != "get_env_var");
    fns.push(Box::new(GetEnvVar));
    fns.extend(super::vrl_purl::all_custom_functions());
    fns
}

#[derive(Clone, Copy, Debug)]
struct GetEnvVar;

impl Function for GetEnvVar {
    fn identifier(&self) -> &'static str {
        "get_env_var"
    }

    fn usage(&self) -> &'static str {
        "Returns the value of the environment variable `name`, if allowed by `vrl.allowed_env_vars`."
    }

    fn category(&self) -> &'static str {
        Category::System.as_ref()
    }

    fn internal_failure_reasons(&self) -> &'static [&'static str] {
        &[
            "Environment variable `name` does not exist.",
            "Environment variable `name` is not allowed by `vrl.allowed_env_vars`.",
            "The value of environment variable `name` is not valid Unicode",
        ]
    }

    fn return_kind(&self) -> u16 {
        kind::BYTES
    }

    fn parameters(&self) -> &'static [Parameter] {
        const PARAMETERS: &[Parameter] = &[Parameter::required(
            "name",
            kind::BYTES,
            "The name of the environment variable.",
        )];
        PARAMETERS
    }

    fn examples(&self) -> &'static [Example] {
        &[]
    }

    fn compile(
        &self,
        _state: &state::TypeState,
        _ctx: &mut FunctionCompileContext,
        arguments: ArgumentList,
    ) -> Compiled {
        let name = arguments.required("name");
        Ok(GetEnvVarFn { name, allowed: allowed_env_vars() }.as_expr())
    }
}

#[derive(Debug, Clone)]
struct GetEnvVarFn {
    name: Box<dyn Expression>,
    allowed: Arc<GlobSet>,
}

impl FunctionExpression for GetEnvVarFn {
    fn resolve(&self, ctx: &mut Context) -> Resolved {
        let value = self.name.resolve(ctx)?;
        let name = value.try_bytes_utf8_lossy()?;
        if !self.allowed.is_match(name.as_ref()) {
            return Err(format!(
                "environment variable `{name}` is not allowed (see `vrl.allowed_env_vars`)"
            )
            .into());
        }
        std::env::var(name.as_ref()).map(Into::into).map_err(|e| e.to_string().into())
    }

    fn type_def(&self, _: &state::TypeState) -> TypeDef {
        TypeDef::bytes().fallible()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vrl::compiler::TargetValue;
    use vrl::prelude::state::RuntimeState;
    use vrl::value::Secrets;

    fn run(allowed: &[&str], src: &str) -> vrl::value::Value {
        set_allowed_env_vars(&allowed.iter().map(ToString::to_string).collect::<Vec<_>>())
            .unwrap();
        let program = vrl::compiler::compile(src, &functions()).unwrap().program;
        let mut target = TargetValue {
            value: vrl::value::Value::Null,
            metadata: vrl::value::Value::Null,
            secrets: Secrets::default(),
        };
        let mut state = RuntimeState::default();
        let tz = vrl::prelude::TimeZone::default();
        let mut ctx = Context::new(&mut target, &mut state, &tz);
        program.resolve(&mut ctx).unwrap()
    }

    #[test]
    fn get_env_var_only_reads_allowlisted_names() {
        // PATH is always set; the allowlist alone decides whether VRL may read it.
        let src = r#"get_env_var("PATH") ?? "denied""#;
        assert_eq!(run(&[], src), vrl::value::Value::from("denied"));
        assert_eq!(run(&["HOME"], src), vrl::value::Value::from("denied"));
        assert_ne!(run(&["PA*"], src), vrl::value::Value::from("denied"));
        assert_ne!(run(&["PATH"], src), vrl::value::Value::from("denied"));
    }

    #[test]
    fn invalid_glob_is_a_config_error() {
        assert!(set_allowed_env_vars(&["[".to_string()]).is_err());
    }
}
