#![allow(clippy::print_stdout)]
#![allow(clippy::print_stderr)]
#![allow(clippy::disallowed_macros)]
use std::path::PathBuf;

use clap::Args;

use crate::{
    config::Config,
    errors::{IntoDiagnostic, Result},
};

#[derive(Debug, Clone, Args)]
#[command(args_conflicts_with_subcommands = true, flatten_help = true)]
pub(crate) struct ConfigArgs {
    /// Configuration file path
    #[clap(long = "config", env("CDVIZ_COLLECTOR_CONFIG"))]
    config: Option<PathBuf>,

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

pub(crate) fn config_cmd(args: ConfigArgs) -> Result<bool> {
    if !args.print && !args.check && !args.print_raw {
        miette::bail!("specify at least one of --print, --print-raw, or --check");
    }

    if args.print_raw {
        let figment = Config::builder().with_config_file(args.config.clone()).build_raw_figment();
        let value: toml::Value = figment.extract().into_diagnostic()?;
        println!("{}", toml::to_string_pretty(&value).into_diagnostic()?);
    }

    if args.print {
        let figment = Config::builder().with_config_file(args.config.clone()).build_figment()?;
        let value: toml::Value = figment.extract().into_diagnostic()?;
        println!("{}", toml::to_string_pretty(&value).into_diagnostic()?);
    }

    if args.check {
        match Config::from_file(args.config) {
            Ok(config) => {
                let src_count = config.sources.len();
                let sink_count = config.sinks.len();
                let tx_count = config.transformers.len();
                println!(
                    "Configuration is valid ({src_count} source(s), {sink_count} sink(s), {tx_count} transformer(s))"
                );
            }
            Err(e) => {
                eprintln!("Configuration is invalid: {e:?}");
                return Ok(false);
            }
        }
    }

    Ok(true)
}
