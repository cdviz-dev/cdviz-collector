#![doc = include_str!("../README.md")]
#![doc(html_favicon_url = "https://cdviz.dev/favicon.ico")]
#![doc(html_logo_url = "https://cdviz.dev/favicon.svg")]

mod config;
mod connect;
mod errors;
mod http;
mod pipes;
mod security;
mod sinks;
mod sources;
mod tools;
mod utils;

use std::ffi::OsString;

use clap::{Parser, Subcommand};
use clap_verbosity_flag::Verbosity;
pub(crate) use connect::{Message, Receiver, Sender};
use errors::{IntoDiagnostic, Result};
use init_tracing_opentelemetry::otlp::OtelGuard;
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::layer::SubscriberExt;

// Use Jemalloc only for musl-64 bits platforms
// see [Default musl allocator considered harmful (to performance)](https://nickb.dev/blog/default-musl-allocator-considered-harmful-to-performance/)
#[cfg(all(target_env = "musl", target_pointer_width = "64"))]
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

// TODO add options (or subcommand) to `check-configuration` (regardless of enabled), `configuration-dump` (after consolidation (with filter or not enabled) and exit or not),
// TODO add options to overide config from cli arguments (like from env)
#[derive(Debug, Clone, Parser)]
#[command(flatten_help = true,version, about, long_about = None)]
pub(crate) struct Cli {
    #[command(flatten)]
    verbose: clap_verbosity_flag::Verbosity,

    /// Disable OpenTelemetry initialization, use minimal tracing setup (useful for testing)
    #[clap(long = "disable-otel", global = true)]
    disable_otel: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Subcommand)]
enum Command {
    /// Launch as a server and connect sources to sinks.
    #[command(arg_required_else_help = true)]
    Connect(connect::ConnectArgs),

    /// Transform local files and potentially check them.
    /// The input & output files are expected to be json files.
    /// The filename of output files will be the same as the input file but with a '.out.json'
    /// extension (input file with this extension will be ignored).
    /// The output files include body, metdata and header fields.
    #[cfg(feature = "tool_transform")]
    #[command(arg_required_else_help = true)]
    Transform(tools::transform::TransformArgs),
}

// wrapper for various possible guard
// that should trigger drop on wrapped guard when enum is dropped
#[allow(dead_code)]
enum ObservabilityGuard {
    DefaultGuard(DefaultGuard),
    OtelGuard(OtelGuard),
}

fn init_log(verbose: Verbosity, disable_otel: bool) -> Result<ObservabilityGuard> {
    use init_tracing_opentelemetry::tracing_subscriber_ext::{
        build_level_filter_layer, build_logger_text, init_subscribers_and_loglevel,
    };
    let level = if verbose.is_present() {
        verbose.log_level().map(|level| level.to_string()).unwrap_or_default()
    } else {
        // in this case environment variable RUST_LOG (or OTEL_LOG_LEVEL) will be used, else "info"
        String::new()
    };

    if disable_otel {
        let subscriber = tracing_subscriber::registry()
            .with(build_level_filter_layer(&level).into_diagnostic()?)
            .with(build_logger_text());
        Ok(ObservabilityGuard::DefaultGuard(tracing::subscriber::set_default(subscriber)))
    } else {
        // Full OpenTelemetry setup for production use
        init_subscribers_and_loglevel(&level).map(ObservabilityGuard::OtelGuard).into_diagnostic()
    }
}

/// to call from main.rs.
/// read args from sys.args
/// on error, exit with code 1
#[allow(clippy::missing_errors_doc)]
pub async fn run_with_sys_args() -> Result<()> {
    let cli = Cli::parse();
    let ok = run(cli).await?;
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
    // insert at first position a fake name of the current binary as expected by clap
    let mut cmdline = vec![OsString::from("cdviz-collector-lib")];
    cmdline.extend(args.into_iter().map(Into::into));
    let cli = Cli::try_parse_from(cmdline).into_diagnostic()?;
    run(cli).await
}

pub(crate) async fn run(cli: Cli) -> Result<bool> {
    let _guard = init_log(cli.verbose, cli.disable_otel)?;
    let _guard = init_log(cli.verbose)?;
    match cli.command {
        Command::Connect(args) => connect::connect(args).await,
        #[cfg(feature = "tool_transform")]
        Command::Transform(args) => tools::transform::transform(args).await,
    }
}
