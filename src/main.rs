use std::sync::Arc;
use tonic::transport::Server;
use tracing::info;
use tracing_subscriber;

mod config;
mod error;
mod grpc_server;
mod session;
mod transport;

use config::Config;
use grpc_server::create_grpc_server;
use session::SessionManager;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing/logging with file and line number
    tracing_subscriber::fmt()
        .with_file(true)
        .with_line_number(true)
        .with_target(true)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    info!("Starting RTP Proxy server");

    // Load configuration
    let config = Config::load("config.toml");
    info!("Configuration loaded: {:?}", config);

    // Create session manager
    let session_manager = Arc::new(SessionManager::new());
    info!("Session manager initialized");

    // Create gRPC server
    let grpc_server = create_grpc_server(Arc::clone(&session_manager));

    // Server address from config
    let addr = config.grpc_address().parse()?;
    info!("gRPC server listening on {}", addr);

    // Start server
    Server::builder()
        .add_service(grpc_server)
        .serve(addr)
        .await?;

    info!("RTP Proxy server stopped");

    Ok(())
}
