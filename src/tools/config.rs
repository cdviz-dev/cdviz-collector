use std::io::Write as _;

use clap::{Args, ValueEnum};

use crate::{
    config::{CONNECT_BASE_CONFIG, SEND_BASE_CONFIG, Config, ConfigSource, resolve_config_source},
    errors::{IntoDiagnostic, Result},
    pipes::discard_all,
    sources::EventSource,
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
    /// with `FileAdapter` and `RemoteFileAdapter` applied
    #[clap(long)]
    print: bool,

    /// Print merged config BEFORE `FileAdapter`/`RemoteFileAdapter` resolve paths and remote files
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
        let figment = Config::builder()
            .with_base_config(base_config)
            .with_resolved_source(resolved.clone())
            .with_keyvalue(&args.set)?
            .build_figment()?;
        let value: toml::Value = figment.extract().into_diagnostic()?;
        writeln!(std::io::stdout(), "{}", toml::to_string_pretty(&value).into_diagnostic()?)
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
                    try_compile(
                        format!("transformer '{name}'"),
                        &config.transformers[name],
                    );
                }

                for (i, tconfig) in config.pipeline.transformers.iter().enumerate() {
                    try_compile(format!("pipeline transformer [{i}]"), tconfig);
                }

                let mut source_names: Vec<_> = config.sources.keys().collect();
                source_names.sort();
                for name in source_names {
                    for (i, tconfig) in config.sources[name].transformers.iter().enumerate() {
                        try_compile(format!("source '{name}' transformer [{i}]"), tconfig);
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
