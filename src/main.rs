use std::sync::Arc;
use tonic::transport::Server;
use tracing::info;
use tracing_subscriber;

mod error;
mod grpc_server;
mod session;
mod transport;

use grpc_server::create_grpc_server;
use session::SessionManager;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing/logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    info!("Starting RTP Proxy server");

    // Create session manager
    let session_manager = Arc::new(SessionManager::new());
    info!("Session manager initialized");

    // Create gRPC server
    let grpc_server = create_grpc_server(Arc::clone(&session_manager));

    // Server address
    let addr = "0.0.0.0:50051".parse()?;
    info!("gRPC server listening on {}", addr);

    // Start server
    Server::builder()
        .add_service(grpc_server)
        .serve(addr)
        .await?;

    info!("RTP Proxy server stopped");

    Ok(())
}
