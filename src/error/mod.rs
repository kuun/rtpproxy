use thiserror::Error;

#[derive(Error, Debug)]
pub enum ProxyError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Address parse error: {0}")]
    AddrParse(#[from] std::net::AddrParseError),

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("Invalid protocol: {0}")]
    InvalidProtocol(String),

    #[error("Transport error: {0}")]
    Transport(String),

    #[error("Connection timeout")]
    ConnectionTimeout,

    #[error("Session already exists: {0}")]
    SessionExists(String),

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),
}

pub type Result<T> = std::result::Result<T, ProxyError>;
