use std::io::Write as _;

use clap::{Args, ValueEnum};

use crate::{
    config::{CONNECT_BASE_CONFIG, Config, ConfigSource, SEND_BASE_CONFIG, resolve_config_source},
    errors::{IntoDiagnostic, Result},
    sources::EventSource,
    transformers::discard_all,
};

/// Which subcommand's base configuration to use when inspecting or validating config.
#[derive(Debug, Clone, Default, ValueEnum)]
pub(crate) enum ForCommand {
    /// Base config for the `connect` server subcommand (default)
    #[default]
    Connect,
    /// Base config for the `send` subcommand
    Send,
    /// Base config for the `transform` subcommand (same embedded base as `connect`)
    Transform,
}

#[derive(Debug, Clone, Args)]
#[command(args_conflicts_with_subcommands = true, flatten_help = true)]
pub(crate) struct ConfigArgs {
    /// Configuration file path or HTTP/HTTPS URL
    #[clap(long = "config", env("CDVIZ_COLLECTOR_CONFIG"))]
    config: Option<ConfigSource>,

    /// HTTP headers to use when fetching config from a URL.
    ///
    /// Format: `"Header-Name: value"`. Can be repeated.
    /// Example: `--config-header "Authorization: Bearer token"`
    #[clap(long = "config-header")]
    config_headers: Vec<String>,

    /// Override individual config key/value pairs.
    ///
    /// Format: `key=value`. Can be repeated.
    /// Values are auto-typed: `true`/`false` → bool, integers → int, decimals → float,
    /// everything else → quoted string.
    ///
    /// Example: `--set sources.my-source.enabled=true`
    #[clap(long = "set")]
    set: Vec<String>,

    /// Print the resolved/consolidated configuration to stdout (TOML format),
    /// with `FileAdapter` and `RemoteFileAdapter` applied.
    /// Secrets (tokens, passwords, DB URLs, ...) are redacted.
    #[clap(long)]
    print: bool,

    /// Print merged config BEFORE `FileAdapter`/`RemoteFileAdapter` resolve paths and remote
    /// files. Unlike `--print`, this dumps the raw figment value, NOT the typed `Config` —
    /// secrets are NOT redacted here. Intended for debugging config merging only.
    #[clap(long)]
    print_raw: bool,

    /// Validate the configuration by parsing it into the typed `Config` structure,
    /// and compile all transformer templates (including VRL) to catch runtime errors early.
    #[clap(long)]
    check: bool,

    /// Select which subcommand's base configuration to apply.
    ///
    /// Controls which embedded base TOML is merged before your configuration file.
    /// Match this to the subcommand you intend to run the config with:
    /// `connect` (default) for server mode, `send` for the send subcommand,
    /// `transform` for batch transformation mode.
    #[clap(long = "for", default_value = "connect")]
    for_command: ForCommand,
}

pub(crate) async fn config_cmd(args: ConfigArgs) -> Result<bool> {
    if !args.print && !args.check && !args.print_raw {
        miette::bail!("specify at least one of --print, --print-raw, or --check");
    }

    let base_config = match args.for_command {
        ForCommand::Connect | ForCommand::Transform => CONNECT_BASE_CONFIG,
        ForCommand::Send => SEND_BASE_CONFIG,
    };

    let resolved = resolve_config_source(args.config, &args.config_headers).await?;

    if args.print_raw {
        let figment = Config::builder()
            .with_base_config(base_config)
            .with_resolved_source(resolved.clone())
            .with_keyvalue(&args.set)?
            .build_raw_figment();
        let value: toml::Value = figment.extract().into_diagnostic()?;
        writeln!(std::io::stdout(), "{}", toml::to_string_pretty(&value).into_diagnostic()?)
            .into_diagnostic()?;
    }

    if args.print {
        // Extract into the typed `Config` (not a raw `toml::Value`) so `SecretString`
        // fields are redacted rather than serialized in plaintext.
        let config = Config::builder()
            .with_base_config(base_config)
            .with_resolved_source(resolved.clone())
            .with_keyvalue(&args.set)?
            .build()?;
        writeln!(std::io::stdout(), "{}", toml::to_string_pretty(&config).into_diagnostic()?)
            .into_diagnostic()?;
    }

    if args.check {
        match Config::builder()
            .with_base_config(base_config)
            .with_resolved_source(resolved)
            .with_keyvalue(&args.set)?
            .build()
        {
            Ok(config) => {
                let src_count = config.sources.len();
                let sink_count = config.sinks.len();
                let tx_count = config.transformers.len();

                // Compile every transformer template to catch VRL errors before runtime.
                // Covers: global pool, pipeline-level chain, and per-source chains
                // (the last two may include inline transformers not in the global pool).
                let mut compile_errors: Vec<String> = Vec::new();
                let mut try_compile = |label: String, tconfig: &crate::transformers::Config| {
                    let discard: crate::sources::EventSourcePipe =
                        Box::new(discard_all::Processor::<EventSource>::new());
                    if let Err(e) = tconfig.make_transformer(discard) {
                        compile_errors.push(format!("{label}: {e:?}"));
                    }
                };

                let mut transformer_names: Vec<_> = config.transformers.keys().collect();
                transformer_names.sort();
                for name in transformer_names {
                    try_compile(format!("transformer '{name}'"), &config.transformers[name]);
                }

                for (i, tconfig) in config.pipeline.transformers.iter().enumerate() {
                    try_compile(format!("pipeline transformer [{i}]"), &tconfig.config);
                }

                let mut source_names: Vec<_> = config.sources.keys().collect();
                source_names.sort();
                for name in source_names {
                    for (i, tconfig) in config.sources[name].chain.transformers.iter().enumerate() {
                        try_compile(format!("source '{name}' transformer [{i}]"), &tconfig.config);
                    }
                }

                let mut sink_names: Vec<_> = config.sinks.keys().collect();
                sink_names.sort();
                for name in sink_names {
                    for (i, tconfig) in config.sinks[name].chain_transformers().iter().enumerate() {
                        try_compile(format!("sink '{name}' transformer [{i}]"), &tconfig.config);
                    }
                }

                if compile_errors.is_empty() {
                    cliclack::log::success(format!(
                        "Configuration is valid ({src_count} source(s), {sink_count} sink(s), {tx_count} transformer(s))"
                    ))
                    .into_diagnostic()?;
                } else {
                    for err in &compile_errors {
                        cliclack::log::error(err).into_diagnostic()?;
                    }
                    return Ok(false);
                }
            }
            Err(e) => {
                cliclack::log::error(format!("Configuration is invalid: {e:?}"))
                    .into_diagnostic()?;
                return Ok(false);
            }
        }
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(config: Option<ConfigSource>, print: bool, print_raw: bool, check: bool) -> ConfigArgs {
        ConfigArgs {
            config,
            config_headers: Vec::new(),
            set: Vec::new(),
            print,
            print_raw,
            check,
            for_command: ForCommand::Connect,
        }
    }

    #[tokio::test]
    async fn no_flags_is_an_error() {
        let result = config_cmd(args(None, false, false, false)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn check_on_invalid_config_returns_false_without_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "this is not valid toml @@@").unwrap();
        let config = ConfigSource::File(path);
        let result = config_cmd(args(Some(config), false, false, true)).await;
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn print_redacts_secrets() {
        let toml = r#"
            [sinks.debug]
            enabled = false

            [sinks.db]
            type = "db"
            enabled = true
            url = "postgres://user:super-secret-password@localhost/db"

            [sinks.clickhouse]
            type = "clickhouse"
            enabled = true
            url = "http://localhost:8123"
            database = "default"
            password = "clickhouse-secret"
            query = "INSERT INTO t VALUES ({payload})"

            [sources.wh]
            type = "webhook"
            enabled = true
            id = "wh"

            [sources.wh.headers.x-signature]
            type = "signature"
            header = "x-signature"
            token = "signing-secret"
        "#;
        let config = crate::config::Config::builder()
            .with_env_vars(false)
            .with_config_text(Some(toml.to_string()))
            .build()
            .unwrap();
        let printed = toml::to_string_pretty(&config).unwrap();

        assert!(!printed.contains("super-secret-password"), "db url leaked: {printed}");
        assert!(!printed.contains("clickhouse-secret"), "clickhouse password leaked: {printed}");
        assert!(!printed.contains("signing-secret"), "signature token leaked: {printed}");
    }

    /// A4 regression: a sink's own transformer chain (`debug`/`http`) must be compiled by
    /// `config --check`, not just the global pool / pipeline / source chains.
    #[tokio::test]
    async fn sink_transformer_chain_is_checked() {
        let toml = r#"
            [sinks.debug]
            enabled = true

            [[sinks.debug.transformers]]
            type = "vrl"
            template = "this is not valid vrl @@@"
        "#;
        let config = crate::config::Config::builder()
            .with_env_vars(false)
            .with_config_text(Some(toml.to_string()))
            .build()
            .unwrap();

        let bad = &config.sinks["debug"].chain_transformers()[0];
        let discard: crate::sources::EventSourcePipe =
            Box::new(crate::transformers::discard_all::Processor::<crate::sources::EventSource>::new());
        assert!(
            bad.config.make_transformer(discard).is_err(),
            "expected invalid VRL in sink chain to fail compilation"
        );
    }
}
