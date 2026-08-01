//! The registry server.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use nebula_registry::{Publisher, Registry, router};

/// Serve the Nebula extension marketplace.
#[derive(Debug, Parser)]
#[command(name = "nebula-registry", version, about)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8080", env = "NEBULA_REGISTRY_ADDR")]
    listen: SocketAddr,

    /// A JSON file listing the registered publishers.
    ///
    /// Required: a registry with no publishers accepts nothing, which is the
    /// correct default but not a useful one to start a server with silently.
    #[arg(long, env = "NEBULA_REGISTRY_PUBLISHERS")]
    publishers: PathBuf,

    /// Log filter, in `tracing` syntax.
    #[arg(long, default_value = "info", env = "NEBULA_LOG")]
    log: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::new(&args.log)).init();

    let publishers: Vec<Publisher> =
        serde_json::from_str(&std::fs::read_to_string(&args.publishers)?)?;
    if publishers.is_empty() {
        return Err("the publisher file lists no publishers; nothing could be published".into());
    }

    let registry = Arc::new(Registry::new());
    for publisher in publishers {
        tracing::info!(
            publisher = %publisher.name,
            namespaces = ?publisher.namespaces,
            keys = publisher.keys.len(),
            "registered a publisher"
        );
        registry.register_publisher(publisher);
    }

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(address = %listener.local_addr()?, "registry listening");

    axum::serve(listener, router(registry)).with_graceful_shutdown(shutdown_signal()).await?;
    Ok(())
}

/// Resolve when the process is asked to stop.
///
/// Graceful shutdown matters here: an upload cut off mid-request would leave a
/// publisher unsure whether their version was accepted.
async fn shutdown_signal() {
    let interrupt = async {
        tokio::signal::ctrl_c().await.expect("failed to install the interrupt handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install the terminate handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => tracing::info!("interrupted; shutting down"),
        () = terminate => tracing::info!("terminated; shutting down"),
    }
}
