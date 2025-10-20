use std::sync::Arc;
use tonic::transport::Server;
use tracing::info;
use tracing_subscriber::{self, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use logroller::{LogRollerBuilder, Rotation, RotationSize};

mod config;
mod error;
mod grpc_server;
mod session;
mod transport;

use config::{Config, LoggingConfig};
use grpc_server::create_grpc_server;
use session::SessionManager;

/// Initialize logging system with either console or file output
fn initialize_logging(logging_config: &LoggingConfig) -> Result<(), Box<dyn std::error::Error>> {
    // Create environment filter
    let env_filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive(tracing::Level::INFO.into());

    if logging_config.file_enabled {
        // Create log directory if it doesn't exist
        std::fs::create_dir_all(&logging_config.directory)?;

        // Build log file name
        let log_filename = format!("{}.log", logging_config.file_prefix);

        // Create logroller appender with size-based rotation
        let appender = LogRollerBuilder::new(&logging_config.directory, &log_filename)
            .rotation(Rotation::SizeBased(RotationSize::MB(logging_config.max_file_size)))
            .max_keep_files(logging_config.max_log_files as u64)
            .build()?;

        // Wrap with non-blocking writer
        let (non_blocking, _guard) = tracing_appender::non_blocking(appender);

        // Create file layer
        let file_layer = fmt::layer()
            .with_file(true)
            .with_line_number(true)
            .with_target(true)
            .with_writer(non_blocking)
            .with_ansi(false); // Disable ANSI colors in file output

        // Initialize with file layer only
        tracing_subscriber::registry()
            .with(env_filter)
            .with(file_layer)
            .init();

        // Keep the guard alive by leaking it (needed for the lifetime of the program)
        std::mem::forget(_guard);

        eprintln!(
            "File logging enabled: {}/{} (max_size: {}MB, max_files: {})",
            logging_config.directory,
            log_filename,
            logging_config.max_file_size,
            logging_config.max_log_files
        );
    } else {
        // Create console (stdout) layer
        let console_layer = fmt::layer()
            .with_file(true)
            .with_line_number(true)
            .with_target(true)
            .with_writer(std::io::stdout);

        // Initialize with console layer only
        tracing_subscriber::registry()
            .with(env_filter)
            .with(console_layer)
            .init();
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load configuration
    let config = Config::load("config.toml");

    // Initialize logging system
    initialize_logging(&config.logging)?;

    info!("Starting RTP Proxy server");
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
