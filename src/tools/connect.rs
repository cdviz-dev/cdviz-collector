//! Connect command for cdviz-collector.
//!
//! The `connect` command launches the collector as a server, enabling configured sources
//! to collect events and dispatch them to configured sinks through the pipeline.
//!
//! # Usage
//!
//! ```bash
//! cdviz-collector connect --config cdviz-collector.toml
//! ```
//!
//! The server runs with HTTP endpoints enabled, allowing webhook sources and
//! SSE sinks to register their routes. The pipeline orchestrates the complete
//! event collection and dispatch workflow.

use crate::{config::Config, errors::Result, pipeline::PipelineBuilder};
use clap::Args;
use std::path::PathBuf;

#[derive(Debug, Clone, Args)]
#[command(args_conflicts_with_subcommands = true,flatten_help = true, about, long_about = None)]
pub(crate) struct ConnectArgs {
    /// The configuration file to use.
    #[clap(long = "config", env("CDVIZ_COLLECTOR_CONFIG"))]
    config: Option<PathBuf>,
}

//TODO add transformers ( eg file/event info, into cdevents) for sources
//TODO integrations with cloudevents (sources & sink)
//TODO integrations with kafka / redpanda, nats,
/// Returns true if the connection service ran successfully
pub(crate) async fn connect(args: ConnectArgs) -> Result<bool> {
    let config = Config::from_file(args.config)?;
    let pipeline = PipelineBuilder::new(config);
    pipeline.run(true).await
}
