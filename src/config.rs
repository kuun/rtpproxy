use config::{Config as ConfigBuilder, ConfigError, File};
use serde::Deserialize;
use std::path::Path;

/// Server configuration
#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    /// gRPC server listen address
    pub grpc_listen_address: String,
    /// gRPC server listen port
    pub grpc_listen_port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            grpc_listen_address: "0.0.0.0".to_string(),
            grpc_listen_port: 50051,
        }
    }
}

/// Logging configuration
#[derive(Debug, Deserialize, Clone)]
pub struct LoggingConfig {
    /// Enable file logging
    pub file_enabled: bool,
    /// Log directory path
    pub directory: String,
    /// Log file name prefix
    pub file_prefix: String,
    /// Maximum log file size in MB (default: 100)
    pub max_file_size: u64,
    /// Maximum number of log files to keep (default: 5)
    pub max_log_files: usize,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            file_enabled: true,
            directory: "./logs".to_string(),
            file_prefix: "rtpproxy".to_string(),
            max_file_size: 100, // 100 MB
            max_log_files: 5,
        }
    }
}

/// Application configuration
#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub logging: LoggingConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

impl Config {
    /// Load configuration from file
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let builder = ConfigBuilder::builder()
            .add_source(File::from(path.as_ref()))
            .build()?;

        builder.try_deserialize()
    }

    /// Load configuration from file with fallback to default
    pub fn load<P: AsRef<Path>>(path: P) -> Self {
        Self::from_file(path).unwrap_or_else(|e| {
            tracing::warn!("Failed to load config file: {}, using defaults", e);
            Self::default()
        })
    }

    /// Get gRPC server socket address
    pub fn grpc_address(&self) -> String {
        format!(
            "{}:{}",
            self.server.grpc_listen_address, self.server.grpc_listen_port
        )
    }
}
