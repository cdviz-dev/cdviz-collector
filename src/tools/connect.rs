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

use crate::{
    config::{Config, ConfigSource, resolve_config_source},
    errors::Result,
    pipeline::PipelineBuilder,
};
use clap::Args;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Args)]
#[command(args_conflicts_with_subcommands = true, flatten_help = true)]
pub(crate) struct ConnectArgs {
    /// Configuration file path or HTTP/HTTPS URL for sources, sinks, and transformers.
    ///
    /// Specifies the TOML configuration that defines the pipeline behavior.
    /// If not provided, the collector will use the base configuration with default
    /// settings. The configuration can also be specified via the `CDVIZ_COLLECTOR_CONFIG`
    /// environment variable.
    ///
    /// Example: `--config cdviz-collector.toml`
    /// Example: `--config https://config.example.com/cdviz.toml`
    #[clap(long = "config", env("CDVIZ_COLLECTOR_CONFIG"))]
    config: Option<ConfigSource>,

    /// HTTP headers to use when fetching config from a URL.
    ///
    /// Format: `"Header-Name: value"`. Can be repeated.
    /// Example: `--config-header "Authorization: Bearer token"`
    #[clap(long = "config-header")]
    config_headers: Vec<String>,
}

/// Returns true if the connection service ran successfully
pub(crate) async fn connect(args: ConnectArgs, shutdown_token: CancellationToken) -> Result<bool> {
    let resolved = resolve_config_source(args.config, &args.config_headers).await?;
    let config = Config::builder().with_resolved_source(resolved).build()?;
    let pipeline = PipelineBuilder::new(config);
    pipeline.run(true, shutdown_token).await
}
