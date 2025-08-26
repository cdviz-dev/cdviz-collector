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
