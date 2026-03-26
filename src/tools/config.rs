use std::io::Write as _;

use clap::Args;

use crate::{
    config::{Config, ConfigSource, resolve_config_source},
    errors::{IntoDiagnostic, Result},
};

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

    /// Validate the configuration by parsing it into the typed Config structure
    #[clap(long)]
    check: bool,
}

pub(crate) async fn config_cmd(args: ConfigArgs) -> Result<bool> {
    if !args.print && !args.check && !args.print_raw {
        miette::bail!("specify at least one of --print, --print-raw, or --check");
    }

    let resolved = resolve_config_source(args.config, &args.config_headers).await?;

    if args.print_raw {
        let figment = Config::builder()
            .with_resolved_source(resolved.clone())
            .with_keyvalue(&args.set)?
            .build_raw_figment();
        let value: toml::Value = figment.extract().into_diagnostic()?;
        writeln!(std::io::stdout(), "{}", toml::to_string_pretty(&value).into_diagnostic()?)
            .into_diagnostic()?;
    }

    if args.print {
        let figment = Config::builder()
            .with_resolved_source(resolved.clone())
            .with_keyvalue(&args.set)?
            .build_figment()?;
        let value: toml::Value = figment.extract().into_diagnostic()?;
        writeln!(std::io::stdout(), "{}", toml::to_string_pretty(&value).into_diagnostic()?)
            .into_diagnostic()?;
    }

    if args.check {
        match Config::builder().with_resolved_source(resolved).with_keyvalue(&args.set)?.build() {
            Ok(config) => {
                let src_count = config.sources.len();
                let sink_count = config.sinks.len();
                let tx_count = config.transformers.len();
                cliclack::log::success(format!(
                    "Configuration is valid ({src_count} source(s), {sink_count} sink(s), {tx_count} transformer(s))"
                ))
                .into_diagnostic()?;
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
