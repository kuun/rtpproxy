use dashmap::DashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, error, info};

use crate::error::{ProxyError, Result};
use crate::transport::{create_transport, TransportAdapter, TransportType, UdpTransport, TcpTransport};
use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Wrapper for UdpTransport to implement TransportAdapter for Arc<UdpTransport>
struct UdpTransportWrapper(Arc<UdpTransport>);

#[async_trait]
impl TransportAdapter for UdpTransportWrapper {
    async fn start(&self) -> Result<()> {
        self.0.start().await
    }

    async fn recv(&self) -> Result<(Bytes, SocketAddr)> {
        self.0.recv().await
    }

    async fn send(&self, data: Bytes, dest: SocketAddr) -> Result<usize> {
        self.0.send(data, dest).await
    }

    async fn close(&self) -> Result<()> {
        self.0.close().await
    }
}

/// Session state enumeration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Connecting,
    Active,
    Error,
    Closed,
}

impl SessionState {
    pub fn to_proto(&self) -> i32 {
        match self {
            SessionState::Connecting => 1,
            SessionState::Active => 2,
            SessionState::Error => 3,
            SessionState::Closed => 4,
        }
    }
}

/// Traffic statistics with atomic counters
#[derive(Debug, Default)]
pub struct TrafficStats {
    packets_received: AtomicU64,
    bytes_received: AtomicU64,
    packets_sent: AtomicU64,
    bytes_sent: AtomicU64,
    packets_lost: AtomicU64,
}

impl TrafficStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_received(&self, bytes: u64) {
        self.packets_received.fetch_add(1, Ordering::Relaxed);
        self.bytes_received.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn record_sent(&self, bytes: u64) {
        self.packets_sent.fetch_add(1, Ordering::Relaxed);
        self.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn record_lost(&self) {
        self.packets_lost.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get_snapshot(&self) -> TrafficStatsSnapshot {
        TrafficStatsSnapshot {
            packets_received: self.packets_received.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            packets_sent: self.packets_sent.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            packets_lost: self.packets_lost.load(Ordering::Relaxed),
        }
    }
}

/// Snapshot of traffic statistics for querying
#[derive(Debug, Clone)]
pub struct TrafficStatsSnapshot {
    pub packets_received: u64,
    pub bytes_received: u64,
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub packets_lost: u64,
}

impl TrafficStatsSnapshot {
    pub fn packet_loss_rate(&self) -> f64 {
        if self.packets_received == 0 {
            0.0
        } else {
            (self.packets_lost as f64 / self.packets_received as f64) * 100.0
        }
    }
}

/// Session configuration
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub listen_addr: SocketAddr,
    pub forward_addr: SocketAddr,
    pub destination_addr: SocketAddr,
    pub protocol: TransportType,
    pub timeout_seconds: u32,
    pub stats_interval_seconds: u32,
}

/// Session event types
#[derive(Debug, Clone)]
pub enum SessionEvent {
    Created {
        session_id: String,
        timestamp: i64,
    },
    StateChanged {
        session_id: String,
        old_state: SessionState,
        new_state: SessionState,
        timestamp: i64,
    },
    StatsUpdate {
        session_id: String,
        stats: TrafficStatsSnapshot,
        timestamp: i64,
    },
    Error {
        session_id: String,
        error: String,
        timestamp: i64,
    },
    Closed {
        session_id: String,
        timestamp: i64,
    },
}

/// RTP proxy session
pub struct Session {
    pub id: String,
    pub config: SessionConfig,
    pub state: Arc<tokio::sync::RwLock<SessionState>>,
    pub stats: Arc<TrafficStats>,
    pub created_at: i64,
    pub last_activity: Arc<tokio::sync::RwLock<i64>>,
    pub error_message: Arc<tokio::sync::RwLock<Option<String>>>,
    transport: Arc<Box<dyn TransportAdapter>>,
    // Store UDP transport separately to access RTCP sockets
    udp_transport: Option<Arc<UdpTransport>>,
    // Store TCP transport separately to access streams for bidirectional forwarding
    tcp_transport: Option<Arc<TcpTransport>>,
    stop_tx: broadcast::Sender<()>,
    task_handle: Option<JoinHandle<()>>,
    reverse_rtp_task_handle: Option<JoinHandle<()>>,
    rtcp_task_handle: Option<JoinHandle<()>>,
    reverse_rtcp_task_handle: Option<JoinHandle<()>>,
}

impl Session {
    /// Create a new session with given configuration and session ID
    pub async fn new(
        session_id: String,
        config: SessionConfig,
        event_tx: mpsc::UnboundedSender<SessionEvent>,
    ) -> Result<Self> {
        let created_at = current_timestamp();

        info!("Creating session {}: listen={}, forward={}, dest={}, protocol={:?}",
            session_id, config.listen_addr, config.forward_addr, config.destination_addr, config.protocol);

        // Create transport adapter
        // For UDP/TCP, create and store the concrete transport to access specific features
        let (transport, udp_transport, tcp_transport) =
            if matches!(config.protocol, TransportType::Udp) {
                let udp = UdpTransport::new(config.listen_addr, config.forward_addr).await?;
                let udp_arc = Arc::new(udp);
                let boxed: Box<dyn TransportAdapter> = Box::new(UdpTransportWrapper(Arc::clone(&udp_arc)));
                (boxed, Some(udp_arc), None)
            } else {
                let tcp = TcpTransport::new(config.listen_addr, config.forward_addr).await?;
                let tcp_arc = Arc::new(tcp);
                let transport = create_transport(
                    config.protocol.clone(),
                    config.listen_addr,
                    config.forward_addr,
                ).await?;
                (transport, None, Some(tcp_arc))
            };

        let (stop_tx, _stop_rx) = broadcast::channel::<()>(1);

        let session = Self {
            id: session_id.clone(),
            config: config.clone(),
            state: Arc::new(tokio::sync::RwLock::new(SessionState::Connecting)),
            stats: Arc::new(TrafficStats::new()),
            created_at,
            last_activity: Arc::new(tokio::sync::RwLock::new(created_at)),
            error_message: Arc::new(tokio::sync::RwLock::new(None)),
            transport: Arc::new(transport),
            udp_transport,
            tcp_transport,
            stop_tx,
            task_handle: None,
            reverse_rtp_task_handle: None,
            rtcp_task_handle: None,
            reverse_rtcp_task_handle: None,
        };

        // Send creation event
        let _ = event_tx.send(SessionEvent::Created {
            session_id: session_id.clone(),
            timestamp: created_at,
        });

        Ok(session)
    }

    /// Start the session and begin forwarding traffic
    pub async fn start(
        &mut self,
        event_tx: mpsc::UnboundedSender<SessionEvent>,
    ) -> Result<()> {
        info!("Starting session {}", self.id);

        // Start transport layer
        self.transport.start().await?;

        // Update state to Active
        self.set_state(SessionState::Active).await;

        // Start RTP forwarding task
        self.start_rtp_forwarding(event_tx.clone())?;

        // Start reverse RTP forwarding and RTCP tasks for UDP sessions
        if matches!(self.config.protocol, TransportType::Udp) {
            self.start_reverse_rtp_forwarding()?;
            self.start_rtcp_forwarding()?;
            self.start_reverse_rtcp_forwarding()?;
        }

        // For TCP sessions, wait for both connections then start reverse forwarding
        if matches!(self.config.protocol, TransportType::Tcp) {
            if let Some(ref tcp_transport) = self.tcp_transport {
                let tcp_clone = Arc::clone(tcp_transport);
                let session_id = self.id.clone();
                let event_tx_clone = event_tx.clone();
                let mut stop_rx = self.stop_tx.subscribe();

                // Spawn a task to wait for connections and then start reverse forwarding
                tokio::spawn(async move {
                    tokio::select! {
                        _ = stop_rx.recv() => {
                            info!("Session {} stopped before TCP connections established", session_id);
                            return;
                        }
                        result = tcp_clone.wait_for_connections() => {
                            match result {
                                Ok(()) => {
                                    info!("Session {} TCP connections established, reverse forwarding will be started by main task", session_id);
                                }
                                Err(e) => {
                                    error!("Session {} error waiting for TCP connections: {}", session_id, e);
                                    let _ = event_tx_clone.send(SessionEvent::Error {
                                        session_id: session_id.clone(),
                                        error: e.to_string(),
                                        timestamp: current_timestamp(),
                                    });
                                }
                            }
                        }
                    }
                });

                // Wait for connections before starting reverse forwarding
                tcp_transport.wait_for_connections().await?;
                info!("TCP connections established for session {}, starting reverse forwarding", self.id);
                self.start_reverse_tcp_forwarding()?;
            }
        }

        // Start statistics reporting task if interval is configured
        if self.config.stats_interval_seconds > 0 {
            let session_id = self.id.clone();
            let stats = Arc::clone(&self.stats);
            let interval = self.config.stats_interval_seconds;
            let event_tx_clone = event_tx.clone();

            tokio::spawn(async move {
                let mut interval = tokio::time::interval(
                    tokio::time::Duration::from_secs(interval as u64)
                );

                loop {
                    interval.tick().await;

                    let snapshot = stats.get_snapshot();
                    let _ = event_tx_clone.send(SessionEvent::StatsUpdate {
                        session_id: session_id.clone(),
                        stats: snapshot,
                        timestamp: current_timestamp(),
                    });
                }
            });
        }

        Ok(())
    }

    /// Stop the session and cleanup resources
    pub async fn stop(&mut self) -> Result<()> {
        info!("Stopping session {}", self.id);

        // Send stop signal
        let _ = self.stop_tx.send(());

        // Wait for RTP task to finish
        if let Some(handle) = self.task_handle.take() {
            let _ = handle.await;
        }

        // Wait for reverse RTP task to finish
        if let Some(handle) = self.reverse_rtp_task_handle.take() {
            let _ = handle.await;
        }

        // Wait for RTCP task to finish
        if let Some(handle) = self.rtcp_task_handle.take() {
            let _ = handle.await;
        }

        // Wait for reverse RTCP task to finish
        if let Some(handle) = self.reverse_rtcp_task_handle.take() {
            let _ = handle.await;
        }

        // Close transport layer
        self.transport.close().await?;

        // Update state
        self.set_state(SessionState::Closed).await;

        Ok(())
    }

    async fn set_state(&self, new_state: SessionState) {
        let mut state = self.state.write().await;
        *state = new_state;
    }

    pub async fn get_state(&self) -> SessionState {
        *self.state.read().await
    }

    pub async fn get_last_activity(&self) -> i64 {
        *self.last_activity.read().await
    }

    pub async fn get_error_message(&self) -> Option<String> {
        self.error_message.read().await.clone()
    }

    pub fn get_stats_snapshot(&self) -> TrafficStatsSnapshot {
        self.stats.get_snapshot()
    }

    /// Start RTP forwarding task (client -> destination)
    fn start_rtp_forwarding(&mut self, event_tx: mpsc::UnboundedSender<SessionEvent>) -> Result<()> {
        let session_id = self.id.clone();
        let transport = Arc::clone(&self.transport);
        let stats = Arc::clone(&self.stats);
        let last_activity = Arc::clone(&self.last_activity);
        let destination = self.config.destination_addr;
        let state = Arc::clone(&self.state);
        let error_message = Arc::clone(&self.error_message);
        let event_tx_clone = event_tx.clone();
        let mut stop_rx = self.stop_tx.subscribe();

        info!("Starting RTP forwarding for session {}", session_id);

        let task_handle = tokio::spawn(async move {
            info!("Session {} RTP forwarding task started", session_id);

            loop {
                tokio::select! {
                    _ = stop_rx.recv() => {
                        info!("Session {} RTP task received stop signal", session_id);
                        break;
                    }
                    recv_result = transport.recv() => {
                        match recv_result {
                            Ok((data, src)) => {
                                let len = data.len() as u64;
                                stats.record_received(len);
                                *last_activity.write().await = current_timestamp();

                                debug!("Session {}: received RTP {} bytes from {}", session_id, len, src);

                                // Forward data to destination
                                match transport.send(data, destination).await {
                                    Ok(sent) => {
                                        stats.record_sent(sent as u64);
                                        debug!("Session {}: forwarded RTP {} bytes to {}", session_id, sent, destination);
                                    }
                                    Err(e) => {
                                        error!("Session {}: failed to forward RTP: {}", session_id, e);
                                        stats.record_lost();

                                        *error_message.write().await = Some(e.to_string());
                                        *state.write().await = SessionState::Error;

                                        let _ = event_tx_clone.send(SessionEvent::Error {
                                            session_id: session_id.clone(),
                                            error: e.to_string(),
                                            timestamp: current_timestamp(),
                                        });
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Session {}: RTP recv error: {}", session_id, e);
                                *error_message.write().await = Some(e.to_string());
                                *state.write().await = SessionState::Error;

                                let _ = event_tx_clone.send(SessionEvent::Error {
                                    session_id: session_id.clone(),
                                    error: e.to_string(),
                                    timestamp: current_timestamp(),
                                });
                                break;
                            }
                        }
                    }
                }
            }

            info!("Session {} RTP forwarding task stopped", session_id);
        });

        self.task_handle = Some(task_handle);
        Ok(())
    }

    /// Start RTCP forwarding task (client -> destination)
    fn start_rtcp_forwarding(&mut self) -> Result<()> {
        // Get RTCP sockets from UdpTransport
        let udp_transport = self.udp_transport.as_ref()
            .ok_or_else(|| ProxyError::Transport("UDP transport not available for RTCP".to_string()))?;

        let rtcp_listen = udp_transport.rtcp_listen_socket();
        let rtcp_forward = udp_transport.rtcp_forward_socket();
        let destination = increment_port_addr(self.config.destination_addr)?;

        info!("Starting RTCP forwarding for session {}", self.id);

        let handle = spawn_udp_forwarding_task(
            self.id.clone(),
            "RTCP".to_string(),
            rtcp_listen,
            rtcp_forward,
            move |_| destination, // Always forward to destination+1
            None, // No stats tracking for RTCP
            None, // No last activity tracking for RTCP
            self.stop_tx.subscribe(),
        );

        self.rtcp_task_handle = Some(handle);
        Ok(())
    }

    /// Start reverse RTP forwarding task (destination -> client)
    fn start_reverse_rtp_forwarding(&mut self) -> Result<()> {
        // Get RTP sockets from UdpTransport
        let udp_transport = self.udp_transport.as_ref()
            .ok_or_else(|| ProxyError::Transport("UDP transport not available for reverse RTP".to_string()))?;

        let rtp_listen = udp_transport.rtp_listen_socket();
        let rtp_forward = udp_transport.rtp_forward_socket();

        info!("Starting reverse RTP forwarding for session {}", self.id);

        let handle = spawn_udp_forwarding_task(
            self.id.clone(),
            "reverse RTP".to_string(),
            rtp_forward,
            rtp_listen,
            |src| src, // Forward back to source address
            Some(Arc::clone(&self.stats)),
            Some(Arc::clone(&self.last_activity)),
            self.stop_tx.subscribe(),
        );

        self.reverse_rtp_task_handle = Some(handle);
        Ok(())
    }

    /// Start reverse RTCP forwarding task (destination -> client)
    fn start_reverse_rtcp_forwarding(&mut self) -> Result<()> {
        // Get RTCP sockets from UdpTransport
        let udp_transport = self.udp_transport.as_ref()
            .ok_or_else(|| ProxyError::Transport("UDP transport not available for reverse RTCP".to_string()))?;

        let rtcp_listen = udp_transport.rtcp_listen_socket();
        let rtcp_forward = udp_transport.rtcp_forward_socket();

        info!("Starting reverse RTCP forwarding for session {}", self.id);

        let handle = spawn_udp_forwarding_task(
            self.id.clone(),
            "reverse RTCP".to_string(),
            rtcp_forward,
            rtcp_listen,
            |src| src, // Forward back to source address
            None, // No stats tracking for RTCP
            None, // No last activity tracking for RTCP
            self.stop_tx.subscribe(),
        );

        self.reverse_rtcp_task_handle = Some(handle);
        Ok(())
    }

    /// Start reverse TCP forwarding task (destination -> client)
    fn start_reverse_tcp_forwarding(&mut self) -> Result<()> {
        // Get TCP streams from TcpTransport
        let tcp_transport = self.tcp_transport.as_ref()
            .ok_or_else(|| ProxyError::Transport("TCP transport not available for reverse forwarding".to_string()))?;

        let client_stream = tcp_transport.client_stream();
        let dest_stream = tcp_transport.dest_stream();
        let client_addr_cell = tcp_transport.client_addr_cell();

        let session_id = self.id.clone();
        let stats = Arc::clone(&self.stats);
        let last_activity = Arc::clone(&self.last_activity);
        let mut stop_rx = self.stop_tx.subscribe();

        info!("Starting reverse TCP forwarding for session {}", session_id);

        let handle = tokio::spawn(async move {
            info!("Session {} reverse TCP forwarding task started", session_id);

            // Get client address once at the beginning (it won't change during the session)
            let client_addr_val = client_addr_cell.get()
                .copied()
                .unwrap_or_else(|| "unknown".parse().unwrap());

            loop {
                tokio::select! {
                    _ = stop_rx.recv() => {
                        info!("Session {} reverse TCP task received stop signal", session_id);
                        break;
                    }
                    result = async {
                        // Read from destination stream
                        let mut dest_guard = dest_stream.lock().await;
                        if let Some(ref mut stream) = *dest_guard {
                            let mut buf = vec![0u8; 65535];
                            let read_result = stream.read(&mut buf).await;
                            drop(dest_guard); // Release lock early

                            match read_result {
                                Ok(0) => {
                                    info!("TCP destination disconnected");
                                    Err(ProxyError::Transport("Connection closed by destination".to_string()))
                                }
                                Ok(len) => {
                                    buf.truncate(len);
                                    debug!("Session {}: received reverse TCP {} bytes from destination", session_id, len);
                                    stats.record_received(len as u64);
                                    *last_activity.write().await = current_timestamp();

                                    // Forward to client
                                    let mut client_guard = client_stream.lock().await;
                                    if let Some(ref mut client) = *client_guard {
                                        match client.write_all(&buf).await {
                                            Ok(_) => {
                                                stats.record_sent(len as u64);
                                                debug!("Session {}: forwarded reverse TCP {} bytes to {}", session_id, len, client_addr_val);
                                                Ok(())
                                            }
                                            Err(e) => {
                                                error!("Session {}: failed to forward reverse TCP: {}", session_id, e);
                                                stats.record_lost();
                                                Err(ProxyError::Io(e))
                                            }
                                        }
                                    } else {
                                        Err(ProxyError::Transport("Client stream not available".to_string()))
                                    }
                                }
                                Err(e) => {
                                    error!("Session {}: reverse TCP recv error: {}", session_id, e);
                                    Err(ProxyError::Io(e))
                                }
                            }
                        } else {
                            Err(ProxyError::Transport("Destination stream not available".to_string()))
                        }
                    } => {
                        if let Err(e) = result {
                            error!("Session {}: reverse TCP error: {}", session_id, e);
                            break;
                        }
                    }
                }
            }

            info!("Session {} reverse TCP forwarding task stopped", session_id);
        });

        self.reverse_rtp_task_handle = Some(handle);
        Ok(())
    }
}

/// Session manager for managing multiple sessions
pub struct SessionManager {
    sessions: Arc<DashMap<String, Arc<tokio::sync::Mutex<Session>>>>,
    event_tx: mpsc::UnboundedSender<SessionEvent>,
    event_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<SessionEvent>>>,
}

impl SessionManager {
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        Self {
            sessions: Arc::new(DashMap::new()),
            event_tx,
            event_rx: Arc::new(tokio::sync::Mutex::new(event_rx)),
        }
    }

    /// Create a new session with the given session ID and configuration
    pub async fn create_session(&self, session_id: String, config: SessionConfig) -> Result<String> {
        // Check if session already exists
        if self.sessions.contains_key(&session_id) {
            return Err(ProxyError::SessionExists(session_id));
        }

        let mut session = Session::new(session_id.clone(), config, self.event_tx.clone()).await?;

        // Start the session
        session.start(self.event_tx.clone()).await?;

        // Store the session
        self.sessions.insert(
            session_id.clone(),
            Arc::new(tokio::sync::Mutex::new(session)),
        );

        info!("Session {} created and started", session_id);

        Ok(session_id)
    }

    /// Destroy an existing session
    pub async fn destroy_session(&self, session_id: &str) -> Result<()> {
        let session_arc = self
            .sessions
            .get(session_id)
            .ok_or_else(|| ProxyError::SessionNotFound(session_id.to_string()))?
            .clone();

        let mut session = session_arc.lock().await;
        session.stop().await?;

        drop(session);

        // Remove from manager
        self.sessions.remove(session_id);

        // Send closed event
        let _ = self.event_tx.send(SessionEvent::Closed {
            session_id: session_id.to_string(),
            timestamp: current_timestamp(),
        });

        info!("Session {} destroyed", session_id);

        Ok(())
    }

    /// Get session information
    pub async fn get_session_info(&self, session_id: &str) -> Result<SessionInfo> {
        let session_arc = self
            .sessions
            .get(session_id)
            .ok_or_else(|| ProxyError::SessionNotFound(session_id.to_string()))?
            .clone();

        let session = session_arc.lock().await;

        Ok(SessionInfo {
            session_id: session.id.clone(),
            config: session.config.clone(),
            state: session.get_state().await,
            created_at: session.created_at,
            last_activity: session.get_last_activity().await,
            stats: session.get_stats_snapshot(),
            error_message: session.get_error_message().await,
        })
    }

    /// List all sessions with optional state filtering
    pub async fn list_sessions(&self, state_filter: Option<SessionState>) -> Vec<SessionInfo> {
        let mut infos = Vec::new();

        for entry in self.sessions.iter() {
            let session = entry.value().lock().await;
            let state = session.get_state().await;

            // Apply state filter if provided
            if let Some(filter) = state_filter {
                if state != filter {
                    continue;
                }
            }

            infos.push(SessionInfo {
                session_id: session.id.clone(),
                config: session.config.clone(),
                state,
                created_at: session.created_at,
                last_activity: session.get_last_activity().await,
                stats: session.get_stats_snapshot(),
                error_message: session.get_error_message().await,
            });
        }

        infos
    }

    /// Subscribe to session events
    pub fn subscribe_events(&self) -> Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<SessionEvent>>> {
        Arc::clone(&self.event_rx)
    }
}

/// Session information for querying
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub config: SessionConfig,
    pub state: SessionState,
    pub created_at: i64,
    pub last_activity: i64,
    pub stats: TrafficStatsSnapshot,
    pub error_message: Option<String>,
}

/// Get current Unix timestamp in seconds
fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Increment port number by 1 for RTCP
fn increment_port_addr(addr: SocketAddr) -> Result<SocketAddr> {
    let mut new_addr = addr;
    let new_port = addr.port().checked_add(1)
        .ok_or_else(|| ProxyError::Transport("Port overflow when calculating RTCP port".to_string()))?;
    new_addr.set_port(new_port);
    Ok(new_addr)
}

/// Generic UDP forwarding task spawner
fn spawn_udp_forwarding_task(
    session_id: String,
    name: String,
    recv_socket: Arc<tokio::net::UdpSocket>,
    send_socket: Arc<tokio::net::UdpSocket>,
    get_destination: impl Fn(SocketAddr) -> SocketAddr + Send + 'static,
    stats: Option<Arc<TrafficStats>>,
    last_activity: Option<Arc<tokio::sync::RwLock<i64>>>,
    mut stop_rx: broadcast::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        info!("Session {} {} forwarding task started", session_id, name);

        loop {
            let mut buf = vec![0u8; 65535];

            tokio::select! {
                _ = stop_rx.recv() => {
                    info!("Session {} {} task received stop signal", session_id, name);
                    break;
                }
                recv_result = recv_socket.recv_from(&mut buf) => {
                    match recv_result {
                        Ok((len, src)) => {
                            debug!("Session {}: received {} {} bytes from {}", session_id, name, len, src);

                            // Update statistics if provided
                            if let Some(ref stats) = stats {
                                stats.record_received(len as u64);
                            }
                            if let Some(ref last_activity) = last_activity {
                                *last_activity.write().await = current_timestamp();
                            }

                            // Forward packet
                            let dest = get_destination(src);
                            match send_socket.send_to(&buf[..len], dest).await {
                                Ok(sent) => {
                                    if let Some(ref stats) = stats {
                                        stats.record_sent(sent as u64);
                                    }
                                    debug!("Session {}: forwarded {} {} bytes to {}", session_id, name, sent, dest);
                                }
                                Err(e) => {
                                    error!("Session {}: failed to forward {}: {}", session_id, name, e);
                                    if let Some(ref stats) = stats {
                                        stats.record_lost();
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            error!("Session {}: {} recv error: {}", session_id, name, e);
                            break;
                        }
                    }
                }
            }
        }

        info!("Session {} {} forwarding task stopped", session_id, name);
    })
}
