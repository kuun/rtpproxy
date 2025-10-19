use async_trait::async_trait;
use bytes::Bytes;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use tracing::{debug, error, info};

use crate::error::{ProxyError, Result};

/// Transport adapter trait for abstracting UDP/TCP protocols
#[async_trait]
pub trait TransportAdapter: Send + Sync {
    /// Start the transport layer
    async fn start(&self) -> Result<()>;

    /// Receive data from the transport
    async fn recv(&self) -> Result<(Bytes, SocketAddr)>;

    /// Send data to destination
    async fn send(&self, data: Bytes, dest: SocketAddr) -> Result<usize>;

    /// Close the transport and release resources
    async fn close(&self) -> Result<()>;
}

/// UDP transport adapter implementation with RTCP support
pub struct UdpTransport {
    // RTP sockets
    rtp_listen_socket: Arc<UdpSocket>,
    rtp_forward_socket: Arc<UdpSocket>,
    // RTCP sockets (port + 1)
    rtcp_listen_socket: Arc<UdpSocket>,
    rtcp_forward_socket: Arc<UdpSocket>,
}

impl UdpTransport {
    pub async fn new(listen_addr: SocketAddr, forward_addr: SocketAddr) -> Result<Self> {
        info!("Creating UDP transport with RTCP support: listen={}, forward={}", listen_addr, forward_addr);

        // Bind RTP listen socket
        let rtp_listen_socket = UdpSocket::bind(listen_addr).await
            .map_err(|e| ProxyError::Transport(format!("Failed to bind RTP listen socket: {}", e)))?;
        info!("RTP listen socket bound to {}", rtp_listen_socket.local_addr()?);

        // Bind RTP forward socket
        let rtp_forward_socket = UdpSocket::bind(forward_addr).await
            .map_err(|e| ProxyError::Transport(format!("Failed to bind RTP forward socket: {}", e)))?;
        info!("RTP forward socket bound to {}", rtp_forward_socket.local_addr()?);

        // Calculate RTCP addresses (port + 1)
        let rtcp_listen_addr = increment_port(listen_addr)?;
        let rtcp_forward_addr = increment_port(forward_addr)?;

        // Bind RTCP listen socket
        let rtcp_listen_socket = UdpSocket::bind(rtcp_listen_addr).await
            .map_err(|e| ProxyError::Transport(format!("Failed to bind RTCP listen socket: {}", e)))?;
        info!("RTCP listen socket bound to {}", rtcp_listen_socket.local_addr()?);

        // Bind RTCP forward socket
        let rtcp_forward_socket = UdpSocket::bind(rtcp_forward_addr).await
            .map_err(|e| ProxyError::Transport(format!("Failed to bind RTCP forward socket: {}", e)))?;
        info!("RTCP forward socket bound to {}", rtcp_forward_socket.local_addr()?);

        Ok(Self {
            rtp_listen_socket: Arc::new(rtp_listen_socket),
            rtp_forward_socket: Arc::new(rtp_forward_socket),
            rtcp_listen_socket: Arc::new(rtcp_listen_socket),
            rtcp_forward_socket: Arc::new(rtcp_forward_socket),
        })
    }

    /// Get RTCP listen socket for concurrent receiving
    pub fn rtcp_listen_socket(&self) -> Arc<UdpSocket> {
        Arc::clone(&self.rtcp_listen_socket)
    }

    /// Get RTCP forward socket for sending
    pub fn rtcp_forward_socket(&self) -> Arc<UdpSocket> {
        Arc::clone(&self.rtcp_forward_socket)
    }
}

/// Increment the port number of a SocketAddr by 1
fn increment_port(addr: SocketAddr) -> Result<SocketAddr> {
    let mut new_addr = addr;
    let new_port = addr.port().checked_add(1)
        .ok_or_else(|| ProxyError::Transport("Port overflow when calculating RTCP port".to_string()))?;
    new_addr.set_port(new_port);
    Ok(new_addr)
}

#[async_trait]
impl TransportAdapter for UdpTransport {
    async fn start(&self) -> Result<()> {
        info!("UDP transport with RTCP support started");
        Ok(())
    }

    async fn recv(&self) -> Result<(Bytes, SocketAddr)> {
        let mut buf = vec![0u8; 65535]; // Maximum UDP packet size

        match self.rtp_listen_socket.recv_from(&mut buf).await {
            Ok((len, src)) => {
                buf.truncate(len);
                debug!("Received RTP {} bytes from {}", len, src);
                Ok((Bytes::from(buf), src))
            }
            Err(e) => {
                error!("RTP recv error: {}", e);
                Err(ProxyError::Io(e))
            }
        }
    }

    async fn send(&self, data: Bytes, dest: SocketAddr) -> Result<usize> {
        match self.rtp_forward_socket.send_to(&data, dest).await {
            Ok(len) => {
                debug!("Sent RTP {} bytes to {}", len, dest);
                Ok(len)
            }
            Err(e) => {
                error!("RTP send error: {}", e);
                Err(ProxyError::Io(e))
            }
        }
    }

    async fn close(&self) -> Result<()> {
        info!("Closing UDP transport (RTP and RTCP)");
        Ok(())
    }
}

/// TCP transport adapter implementation (placeholder)
pub struct TcpTransport {
    listen_addr: SocketAddr,
    forward_addr: SocketAddr,
    listener: Option<Arc<TcpListener>>,
    connection: Option<Arc<tokio::sync::Mutex<TcpStream>>>,
    forward_connection: Option<Arc<tokio::sync::Mutex<TcpStream>>>,
}

impl TcpTransport {
    pub async fn new(listen_addr: SocketAddr, forward_addr: SocketAddr) -> Result<Self> {
        info!("Creating TCP transport: listen={}, forward={}", listen_addr, forward_addr);

        Ok(Self {
            listen_addr,
            forward_addr,
            listener: None,
            connection: None,
            forward_connection: None,
        })
    }
}

#[async_trait]
impl TransportAdapter for TcpTransport {
    async fn start(&self) -> Result<()> {
        info!("TCP transport starting");
        // TCP connections need to be established when accepting connections
        Ok(())
    }

    async fn recv(&self) -> Result<(Bytes, SocketAddr)> {
        // Check if listener is initialized
        if self.listener.is_none() {
            return Err(ProxyError::Transport("TCP listener not initialized".to_string()));
        }

        // TCP implementation is more complex and requires connection state management
        // This is a simplified placeholder; proper implementation should be in the session layer
        Err(ProxyError::Transport("TCP recv not implemented in this simplified version".to_string()))
    }

    async fn send(&self, _data: Bytes, _dest: SocketAddr) -> Result<usize> {
        // TCP send requires connection establishment
        Err(ProxyError::Transport("TCP send not implemented in this simplified version".to_string()))
    }

    async fn close(&self) -> Result<()> {
        info!("Closing TCP transport");
        Ok(())
    }
}

/// Transport adapter factory
#[derive(Debug, Clone, Copy)]
pub enum TransportType {
    Udp,
    Tcp,
}

pub async fn create_transport(
    transport_type: TransportType,
    listen_addr: SocketAddr,
    forward_addr: SocketAddr,
) -> Result<Box<dyn TransportAdapter>> {
    match transport_type {
        TransportType::Udp => {
            Ok(Box::new(UdpTransport::new(listen_addr, forward_addr).await?))
        }
        TransportType::Tcp => {
            Ok(Box::new(TcpTransport::new(listen_addr, forward_addr).await?))
        }
    }
}
