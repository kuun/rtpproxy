use async_trait::async_trait;
use bytes::Bytes;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

    /// Get RTP listen socket for bidirectional communication
    pub fn rtp_listen_socket(&self) -> Arc<UdpSocket> {
        Arc::clone(&self.rtp_listen_socket)
    }

    /// Get RTP forward socket for bidirectional communication
    pub fn rtp_forward_socket(&self) -> Arc<UdpSocket> {
        Arc::clone(&self.rtp_forward_socket)
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

/// TCP transport adapter implementation
pub struct TcpTransport {
    listener: tokio::sync::Mutex<Option<TcpListener>>,
    destination_addr: SocketAddr,
    client_stream: Arc<tokio::sync::Mutex<Option<TcpStream>>>,
    dest_stream: Arc<tokio::sync::Mutex<Option<TcpStream>>>,
    client_addr: Arc<tokio::sync::OnceCell<SocketAddr>>,
    /// Notify when both connections are established
    connections_ready: Arc<tokio::sync::Notify>,
}

impl TcpTransport {
    pub async fn new(listen_addr: SocketAddr, forward_addr: SocketAddr) -> Result<Self> {
        info!("Creating TCP transport: listen={}, forward={}", listen_addr, forward_addr);

        // Bind TCP listener immediately
        let listener = TcpListener::bind(listen_addr).await
            .map_err(|e| ProxyError::Transport(format!("Failed to bind TCP listener: {}", e)))?;
        info!("TCP listener bound to {}", listener.local_addr()?);

        Ok(Self {
            listener: tokio::sync::Mutex::new(Some(listener)),
            destination_addr: forward_addr,
            client_stream: Arc::new(tokio::sync::Mutex::new(None)),
            dest_stream: Arc::new(tokio::sync::Mutex::new(None)),
            client_addr: Arc::new(tokio::sync::OnceCell::new()),
            connections_ready: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// Get client stream for bidirectional communication
    pub fn client_stream(&self) -> Arc<tokio::sync::Mutex<Option<TcpStream>>> {
        Arc::clone(&self.client_stream)
    }

    /// Get destination stream for bidirectional communication
    pub fn dest_stream(&self) -> Arc<tokio::sync::Mutex<Option<TcpStream>>> {
        Arc::clone(&self.dest_stream)
    }

    /// Get client address (returns None if client not yet connected)
    pub fn client_addr(&self) -> Option<SocketAddr> {
        self.client_addr.get().copied()
    }

    /// Get client address cell for sharing across tasks
    pub fn client_addr_cell(&self) -> Arc<tokio::sync::OnceCell<SocketAddr>> {
        Arc::clone(&self.client_addr)
    }

    /// Accept client connection and immediately connect to destination
    /// The listener is closed after accepting the first connection
    async fn accept_client(&self) -> Result<()> {
        // Take the listener from Option (can only be called once)
        let listener = self.listener.lock().await.take()
            .ok_or_else(|| ProxyError::Transport("Listener already closed".to_string()))?;

        // Accept the first connection
        let (stream, addr) = listener.accept().await
            .map_err(|e| ProxyError::Transport(format!("Failed to accept connection: {}", e)))?;

        info!("Accepted TCP connection from {}", addr);

        // Listener is dropped here, closing it after first connection
        drop(listener);
        info!("TCP listener closed after accepting first connection");

        // Store client stream and address
        *self.client_stream.lock().await = Some(stream);
        let _ = self.client_addr.set(addr); // Set once, ignore if already set

        // Immediately connect to destination
        info!("Connecting to TCP destination {} immediately after accepting client", self.destination_addr);
        let dest_stream = TcpStream::connect(self.destination_addr).await
            .map_err(|e| ProxyError::Transport(format!("Failed to connect to destination: {}", e)))?;

        info!("Connected to TCP destination {}", self.destination_addr);
        *self.dest_stream.lock().await = Some(dest_stream);

        // Notify all waiters that both connections are ready
        self.connections_ready.notify_waiters();

        Ok(())
    }

    /// Check if both client and destination are connected
    pub fn is_fully_connected(&self) -> bool {
        // This is a non-blocking check, but for simplicity we use try_lock
        // In production, you might want a more robust approach
        if let Ok(client) = self.client_stream.try_lock() {
            if let Ok(dest) = self.dest_stream.try_lock() {
                return client.is_some() && dest.is_some();
            }
        }
        false
    }

    /// Wait until both client and destination connections are established
    pub async fn wait_for_connections(&self) -> Result<()> {
        // Check if already connected
        {
            let client_ready = self.client_stream.lock().await.is_some();
            let dest_ready = self.dest_stream.lock().await.is_some();

            if client_ready && dest_ready {
                return Ok(());
            }
        }

        // Wait for notification that connections are ready
        self.connections_ready.notified().await;

        Ok(())
    }
}

#[async_trait]
impl TransportAdapter for TcpTransport {
    async fn start(&self) -> Result<()> {
        info!("TCP transport starting - waiting for client connection");
        Ok(())
    }

    async fn recv(&self) -> Result<(Bytes, SocketAddr)> {
        // Accept client connection if not already connected
        {
            let client = self.client_stream.lock().await;
            if client.is_none() {
                drop(client);
                self.accept_client().await?;
            }
        }

        // Get client address
        let client_addr = self.client_addr.get()
            .copied()
            .ok_or_else(|| ProxyError::Transport("Client address not available".to_string()))?;

        // Read data from client
        let mut stream_guard = self.client_stream.lock().await;
        let stream = stream_guard.as_mut()
            .ok_or_else(|| ProxyError::Transport("Client stream not available".to_string()))?;

        let mut buf = vec![0u8; 65535];
        match stream.read(&mut buf).await {
            Ok(0) => {
                info!("TCP client disconnected");
                Err(ProxyError::Transport("Connection closed by client".to_string()))
            }
            Ok(len) => {
                buf.truncate(len);
                debug!("Received TCP {} bytes from {}", len, client_addr);
                Ok((Bytes::from(buf), client_addr))
            }
            Err(e) => {
                error!("TCP recv error: {}", e);
                Err(ProxyError::Io(e))
            }
        }
    }

    async fn send(&self, data: Bytes, _dest: SocketAddr) -> Result<usize> {
        // Destination should already be connected by accept_client()
        // Write data to destination
        let mut stream_guard = self.dest_stream.lock().await;
        let stream = stream_guard.as_mut()
            .ok_or_else(|| ProxyError::Transport("Destination stream not available".to_string()))?;

        match stream.write_all(&data).await {
            Ok(_) => {
                debug!("Sent TCP {} bytes to destination", data.len());
                Ok(data.len())
            }
            Err(e) => {
                error!("TCP send error: {}", e);
                Err(ProxyError::Io(e))
            }
        }
    }

    async fn close(&self) -> Result<()> {
        info!("Closing TCP transport");

        // Close client connection
        if let Some(mut stream) = self.client_stream.lock().await.take() {
            let _ = stream.shutdown().await;
        }

        // Close destination connection
        if let Some(mut stream) = self.dest_stream.lock().await.take() {
            let _ = stream.shutdown().await;
        }

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
