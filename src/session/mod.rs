use dashmap::DashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, error, info};

use crate::error::{ProxyError, Result};
use crate::transport::{create_transport, TransportAdapter, TransportType, UdpTransport};
use async_trait::async_trait;
use bytes::Bytes;

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
        // For UDP, create and store the UdpTransport to access RTCP sockets
        let (transport, udp_transport): (Box<dyn TransportAdapter>, Option<Arc<UdpTransport>>) =
            if matches!(config.protocol, TransportType::Udp) {
                let udp = UdpTransport::new(config.listen_addr, config.forward_addr).await?;
                let udp_arc = Arc::new(udp);
                // Clone the Arc and box it as a trait object
                let boxed: Box<dyn TransportAdapter> = Box::new(UdpTransportWrapper(Arc::clone(&udp_arc)));
                (boxed, Some(udp_arc))
            } else {
                let transport = create_transport(
                    config.protocol.clone(),
                    config.listen_addr,
                    config.forward_addr,
                ).await?;
                (transport, None)
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

        let session_id = self.id.clone();
        let destination = increment_port_addr(self.config.destination_addr)?;
        let mut stop_rx = self.stop_tx.subscribe();

        info!("Starting RTCP forwarding for session {}", session_id);

        let rtcp_handle = tokio::spawn(async move {
            info!("Session {} RTCP forwarding task started", session_id);

            loop {
                let mut buf = vec![0u8; 65535];

                tokio::select! {
                    _ = stop_rx.recv() => {
                        info!("Session {} RTCP task received stop signal", session_id);
                        break;
                    }
                    recv_result = rtcp_listen.recv_from(&mut buf) => {
                        match recv_result {
                            Ok((len, src)) => {
                                debug!("Session {}: received RTCP {} bytes from {}", session_id, len, src);

                                // Forward RTCP packet to destination
                                match rtcp_forward.send_to(&buf[..len], destination).await {
                                    Ok(sent) => {
                                        debug!("Session {}: forwarded RTCP {} bytes to {}", session_id, sent, destination);
                                    }
                                    Err(e) => {
                                        error!("Session {}: failed to forward RTCP: {}", session_id, e);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Session {}: RTCP recv error: {}", session_id, e);
                                break;
                            }
                        }
                    }
                }
            }

            info!("Session {} RTCP forwarding task stopped", session_id);
        });

        self.rtcp_task_handle = Some(rtcp_handle);
        Ok(())
    }

    /// Start reverse RTP forwarding task (destination -> client)
    fn start_reverse_rtp_forwarding(&mut self) -> Result<()> {
        // Get RTP sockets from UdpTransport
        let udp_transport = self.udp_transport.as_ref()
            .ok_or_else(|| ProxyError::Transport("UDP transport not available for reverse RTP".to_string()))?;

        let rtp_listen = udp_transport.rtp_listen_socket();
        let rtp_forward = udp_transport.rtp_forward_socket();

        let session_id = self.id.clone();
        let stats = Arc::clone(&self.stats);
        let last_activity = Arc::clone(&self.last_activity);
        let mut stop_rx = self.stop_tx.subscribe();

        info!("Starting reverse RTP forwarding for session {}", session_id);

        let reverse_handle = tokio::spawn(async move {
            info!("Session {} reverse RTP forwarding task started", session_id);

            loop {
                let mut buf = vec![0u8; 65535];

                tokio::select! {
                    _ = stop_rx.recv() => {
                        info!("Session {} reverse RTP task received stop signal", session_id);
                        break;
                    }
                    recv_result = rtp_forward.recv_from(&mut buf) => {
                        match recv_result {
                            Ok((len, src)) => {
                                debug!("Session {}: received reverse RTP {} bytes from {}", session_id, len, src);
                                stats.record_received(len as u64);
                                *last_activity.write().await = current_timestamp();

                                // Forward back to client via listen socket
                                match rtp_listen.send_to(&buf[..len], src).await {
                                    Ok(sent) => {
                                        stats.record_sent(sent as u64);
                                        debug!("Session {}: forwarded reverse RTP {} bytes to client", session_id, sent);
                                    }
                                    Err(e) => {
                                        error!("Session {}: failed to forward reverse RTP: {}", session_id, e);
                                        stats.record_lost();
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Session {}: reverse RTP recv error: {}", session_id, e);
                                break;
                            }
                        }
                    }
                }
            }

            info!("Session {} reverse RTP forwarding task stopped", session_id);
        });

        self.reverse_rtp_task_handle = Some(reverse_handle);
        Ok(())
    }

    /// Start reverse RTCP forwarding task (destination -> client)
    fn start_reverse_rtcp_forwarding(&mut self) -> Result<()> {
        // Get RTCP sockets from UdpTransport
        let udp_transport = self.udp_transport.as_ref()
            .ok_or_else(|| ProxyError::Transport("UDP transport not available for reverse RTCP".to_string()))?;

        let rtcp_listen = udp_transport.rtcp_listen_socket();
        let rtcp_forward = udp_transport.rtcp_forward_socket();

        let session_id = self.id.clone();
        let mut stop_rx = self.stop_tx.subscribe();

        info!("Starting reverse RTCP forwarding for session {}", session_id);

        let reverse_rtcp_handle = tokio::spawn(async move {
            info!("Session {} reverse RTCP forwarding task started", session_id);

            loop {
                let mut buf = vec![0u8; 65535];

                tokio::select! {
                    _ = stop_rx.recv() => {
                        info!("Session {} reverse RTCP task received stop signal", session_id);
                        break;
                    }
                    recv_result = rtcp_forward.recv_from(&mut buf) => {
                        match recv_result {
                            Ok((len, src)) => {
                                debug!("Session {}: received reverse RTCP {} bytes from {}", session_id, len, src);

                                // Forward RTCP packet back to client
                                match rtcp_listen.send_to(&buf[..len], src).await {
                                    Ok(sent) => {
                                        debug!("Session {}: forwarded reverse RTCP {} bytes to client", session_id, sent);
                                    }
                                    Err(e) => {
                                        error!("Session {}: failed to forward reverse RTCP: {}", session_id, e);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Session {}: reverse RTCP recv error: {}", session_id, e);
                                break;
                            }
                        }
                    }
                }
            }

            info!("Session {} reverse RTCP forwarding task stopped", session_id);
        });

        self.reverse_rtcp_task_handle = Some(reverse_rtcp_handle);
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
