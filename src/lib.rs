#![doc = include_str!("../README.md")]
#![doc(html_favicon_url = "https://cdviz.dev/favicon.ico")]
#![doc(html_logo_url = "https://cdviz.dev/favicon.svg")]

mod config;
mod errors;
mod http;
mod message;
mod pipeline;
mod pipes;
#[cfg(feature = "reqwest-eventsource")]
mod reqwest_eventsource;
mod security;
mod sinks;
mod sources;
mod tools;
mod utils;

// re-export
pub mod cdevent_utils;

use std::{ffi::OsString, path::PathBuf};

use clap::{Parser, Subcommand};
use clap_verbosity_flag::Verbosity;
use errors::{IntoDiagnostic, Result};
use init_tracing_opentelemetry::{Guard, TracingConfig};
pub(crate) use message::{Message, Receiver, Sender};
use tokio_util::sync::CancellationToken;

// Use Jemalloc only for musl-64 bits platforms
// see [Default musl allocator considered harmful (to performance)](https://nickb.dev/blog/default-musl-allocator-considered-harmful-to-performance/)
#[cfg(all(target_env = "musl", target_pointer_width = "64"))]
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

// TODO add options (or subcommand) to `check-configuration` (regardless of enabled), `configuration-dump` (after consolidation (with filter or not enabled) and exit or not),
// TODO add options to overide config from cli arguments (like from env)
#[derive(Debug, Clone, Parser)]
#[command(
    flatten_help = true,
    version,
    about,
    long_about = "cdviz-collector provides a configurable pipeline architecture that can receive events \
from various sources (webhooks, files, SSE), transform them using VRL or Handlebars, and send them \
to multiple destinations (HTTP endpoints, databases, files). It supports real-time event processing \
with CloudEvents format and CDEvents specification compliance."
)]
pub(crate) struct Cli {
    #[command(flatten)]
    verbose: clap_verbosity_flag::Verbosity,

    /// Disable OpenTelemetry initialization and use minimal tracing setup.
    ///
    /// This is useful for testing environments or when you want to avoid
    /// OpenTelemetry overhead. When disabled, only basic console logging
    /// will be available without distributed tracing capabilities.
    #[clap(long = "disable-otel", global = true)]
    disable_otel: bool,

    /// Change working directory before executing the command.
    ///
    /// This affects relative paths in configuration files and data files.
    /// Useful when running the collector from a different location than
    /// where your configuration and data files are located.
    #[clap(short = 'C', long = "directory", global = true)]
    directory: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Subcommand)]
enum Command {
    /// Launch collector as a server to connect sources to sinks.
    ///
    /// Runs the collector in server mode, enabling configured sources to collect
    /// events and dispatch them to configured sinks through the pipeline. The server
    /// provides HTTP endpoints for webhook sources and SSE sinks.
    #[command(arg_required_else_help = true)]
    Connect(tools::connect::ConnectArgs),

    /// Send JSON data directly to a sink for testing and scripting.
    ///
    /// This command allows sending JSON data directly to configured sinks without
    /// running a full server. Useful for testing transformations, debugging sink
    /// configurations, or scripting event dispatch.
    #[command(arg_required_else_help = true)]
    Send(tools::send::SendArgs),

    /// Transform local JSON files using configured transformers.
    ///
    /// Processes JSON files from an input directory through configured transformers
    /// and writes results to an output directory. Supports validation against `CDEvents`
    /// specification and provides interactive review, overwrite, or check modes for
    /// managing output files.
    #[cfg(feature = "tool_transform")]
    #[command(arg_required_else_help = true)]
    Transform(tools::transform::TransformArgs),
}

fn init_log(verbosity: Verbosity, disable_otel: bool) -> Result<Guard> {
    let mut config = TracingConfig::production();
    if verbosity.is_present() {
        config = config.with_log_directives(verbosity.log_level_filter().as_str().to_lowercase());
    }
    let guard = config
        .with_stderr()
        .with_timer(init_tracing_opentelemetry::LogTimer::Uptime)
        .with_logfmt_format()
        .with_otel(!disable_otel)
        .init_subscriber()
        .into_diagnostic()?;
    Ok(guard)
}

/// to call from main.rs.
/// read args from sys.args
/// on error, exit with code 1
#[allow(clippy::missing_errors_doc)]
pub async fn run_with_sys_args() -> Result<()> {
    let cli = Cli::parse();
    let ok = run(cli, true, CancellationToken::new()).await?;
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

/// to ease call from other crates, test,...
#[allow(clippy::missing_errors_doc)]
pub async fn run_with_args<I, T>(args: I) -> Result<bool>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    run_with_args_and_log(args, true, CancellationToken::new()).await
}

#[allow(clippy::missing_errors_doc)]
pub async fn run_with_args_and_log<I, T>(
    args: I,
    with_init_log: bool,
    shutdown_token: CancellationToken,
) -> Result<bool>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    // insert at first position a fake name of the current binary as expected by clap
    let mut cmdline = vec![OsString::from("cdviz-collector-lib")];
    cmdline.extend(args.into_iter().map(Into::into));
    let cli = Cli::try_parse_from(cmdline).into_diagnostic()?;
    run(cli, with_init_log, shutdown_token).await
}

#[allow(clippy::expect_used, clippy::if_then_some_else_none)]
pub(crate) async fn run(
    cli: Cli,
    with_init_log: bool,
    shutdown_token: CancellationToken,
) -> Result<bool> {
    let _guard = if with_init_log { Some(init_log(cli.verbose, cli.disable_otel)?) } else { None };
    if let Some(dir) = &cli.directory {
        std::env::set_current_dir(dir).into_diagnostic()?;
    }
    match cli.command {
        Command::Connect(args) => tools::connect::connect(args, shutdown_token).await,
        Command::Send(args) => tools::send::send(args, shutdown_token).await,
        #[cfg(feature = "tool_transform")]
        Command::Transform(args) => tools::transform::transform(args).await,
    }
}
