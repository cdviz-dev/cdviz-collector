mod config;
mod connect;
mod errors;
mod pipes;
mod sinks;
mod sources;
mod tools;
mod utils;

use clap::{Parser, Subcommand};
use clap_verbosity_flag::Verbosity;
use errors::{Error, Result};
use init_tracing_opentelemetry::tracing_subscriber_ext::TracingGuard;

pub(crate) use connect::{Message, Receiver, Sender};

// Use Jemalloc only for musl-64 bits platforms
#[cfg(all(target_env = "musl", target_pointer_width = "64"))]
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

// TODO add options (or subcommand) to `check-configuration` (regardless of enabled), `configuration-dump` (after consolidation (with filter or not enabled) and exit or not),
// TODO add options to overide config from cli arguments (like from env)
#[derive(Debug, Clone, Parser)]
#[command(args_conflicts_with_subcommands = true,flatten_help = true,version, about, long_about = None)]
pub(crate) struct Cli {
    #[command(flatten)]
    verbose: clap_verbosity_flag::Verbosity,

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

//TODO use logfmt
fn init_log(verbose: Verbosity) -> Result<TracingGuard> {
    let level = verbose.log_level().map(|level| level.to_string());
    std::env::set_var(
        "RUST_LOG",
        std::env::var("RUST_LOG")
            .ok()
            .or_else(|| level.map(|level| format!("{level},otel::setup={level}")))
            .unwrap_or_else(|| "off".to_string()),
    );
    // very opinionated init of tracing, look as is source to make your own
    init_tracing_opentelemetry::tracing_subscriber_ext::init_subscribers().map_err(Error::from)
}

//TODO document the architecture and the configuration
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let _guard = init_log(cli.verbose)?;
    let ok = match cli.command {
        Command::Connect(args) => connect::connect(args).await?,
        #[cfg(feature = "tool_transform")]
        Command::Transform(args) => tools::transform::transform(args).await?,
    };
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}
