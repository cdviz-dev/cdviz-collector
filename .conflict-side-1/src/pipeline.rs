//! Pipeline orchestration for cdviz-collector.
//!
//! This module provides shared pipeline infrastructure for both `connect` and `send` commands,
//! eliminating code duplication and providing consistent behavior across commands.
//!
//! # Architecture
//!
//! The pipeline follows a consistent flow:
//! 1. **Configuration**: Load and merge configuration from files and CLI overrides
//! 2. **Message Queue**: Create broadcast channel for internal message passing
//! 3. **Sinks**: Create and start sink components for event dispatch
//! 4. **Sources**: Create and start source components for event collection
//! 5. **Lifecycle**: Manage component lifecycle and graceful shutdown
//!
//! # Usage
//!
//! ```rust
//! // For server mode (connect command)
//! let config = load_config_from_file(config_path)?;
//! let pipeline = PipelineBuilder::new(config)?;
//! pipeline.run_server_with_configured_sources(&shutdown_token).await?;
//!
//! // For oneshot mode (send command)
//! let config = load_config_with_overrides(&args)?;
//! let pipeline = PipelineBuilder::new(config)?;
//! pipeline.run_oneshot_with_cli_source(reader).await?;
//! ```

use crate::{
    config::Config,
    errors::{Error, IntoDiagnostic, Result},
    message::{Message, Sender},
    sinks, sources,
};
use axum::Router;
use futures::future::TryJoinAll;
use tokio::{sync::broadcast, task::JoinHandle};
use tokio_util::sync::CancellationToken;

/// Pipeline builder for creating and managing cdviz-collector pipelines.
///
/// Provides a unified interface for both server mode (connect) and oneshot mode (send)
/// operations while sharing common infrastructure components.
pub(crate) struct PipelineBuilder {
    config: Config,
    tx: Sender<Message>,
}

impl PipelineBuilder {
    /// Create a new pipeline builder with the provided configuration.
    ///
    /// This sets up the internal broadcast channel for message passing between
    /// sources and sinks.
    pub fn new(config: Config) -> Self {
        let (tx, rx) = broadcast::channel::<Message>(1024);
        // Drop the receiver immediately - we want the channel to close naturally
        // when all senders are dropped
        drop(rx);
        Self { config, tx }
    }

    /// Create sinks from configuration and return both task handles and HTTP routes.
    ///
    /// Returns a tuple of (`sink_handles`, `http_routes`) where:
    /// - `sink_handles`: Task handles for running sinks
    /// - `http_routes`: HTTP routes that some sinks may need to register
    ///
    /// Failed if no sinks configured
    #[allow(clippy::type_complexity)]
    pub fn create_sinks(
        &self,
        enable_http_server: bool,
    ) -> Result<(Vec<JoinHandle<Result<()>>>, Vec<Router>)> {
        let (sink_handles, sink_routes) =
            sinks::create_sinks_and_routes(self.config.sinks.clone(), &self.tx)?;

        if sink_handles.is_empty() && (!enable_http_server || sink_routes.is_empty()) {
            tracing::error!("no sink configured or started");
            return Err(Error::NoSink).into_diagnostic();
        }

        Ok((sink_handles, sink_routes))
    }

    /// Create sources from configuration and return both task handles and HTTP routes.
    ///
    /// Returns a tuple of (`sources_handles`, `http_routes`) where:
    /// - `source_handles`: Task handles for running sources
    /// - `http_routes`: HTTP routes that some sinks may need to register
    ///
    /// Failed if no sources configured
    #[allow(clippy::type_complexity)]
    pub fn create_sources(
        &self,
        shutdown_token: &'static CancellationToken,
        enable_http_server: bool,
    ) -> Result<(Vec<JoinHandle<Result<()>>>, Vec<Router>)> {
        let (source_handles, source_routes) = sources::create_sources_and_routes(
            self.config.sources.clone(),
            &self.tx,
            shutdown_token,
        )?;

        if source_handles.is_empty() && (!enable_http_server || source_routes.is_empty()) {
            tracing::error!("no source configured or started");
            return Err(Error::NoSource).into_diagnostic();
        }

        Ok((source_handles, source_routes))
    }

    /// Run pipeline with configured sources and sinks.
    ///
    /// This unified method handles both server mode (with HTTP server) and oneshot mode.
    /// The pipeline runs until all sources complete and disconnect from the channel,
    /// which then causes sinks to stop naturally.
    pub async fn run(self, enable_http_server: bool) -> Result<bool> {
        // Setup the handler to (try to) shutdown gracefully
        tokio::spawn(handle_shutdown_signal());

        // Create sinks with routes
        let (sink_handles, sink_routes) = self.create_sinks(enable_http_server)?;

        // Create sources using standard pattern
        let (source_handles, source_routes) =
            self.create_sources(&crate::SHUTDOWN_TOKEN, enable_http_server)?;

        let operation = if enable_http_server { "server" } else { "send" };
        tracing::info!("Starting {} operation", operation);

        // Collect all handles that need to complete
        let mut all_handles = Vec::new();
        all_handles.extend(source_handles);
        all_handles.extend(sink_handles);

        // Optionally start HTTP server
        if enable_http_server {
            // Combine all routes and start HTTP server
            let mut routes = Vec::new();
            routes.extend(source_routes);
            routes.extend(sink_routes);

            let server_handle =
                crate::http::launch(&self.config.http, routes, &crate::SHUTDOWN_TOKEN);
            all_handles.push(server_handle);
        }

        // Drop the sender to allow graceful shutdown when sources finish
        // When all senders are dropped, the channel will close naturally
        // and sinks will exit when they can no longer receive messages
        drop(self.tx);

        // Wait for all components to complete
        // Sinks will naturally exit when all senders are dropped and the channel closes
        all_handles.into_iter().collect::<TryJoinAll<_>>().await.into_diagnostic()?;

        tracing::info!("{} operation completed", operation);
        Ok(true)
    }
}

#[allow(clippy::expect_used)]
async fn handle_shutdown_signal() {
    use tokio::signal;
    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    crate::SHUTDOWN_TOKEN.cancel();
}
