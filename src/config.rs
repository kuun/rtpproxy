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

/// Application configuration
#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
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
