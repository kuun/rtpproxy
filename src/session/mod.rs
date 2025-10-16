use dashmap::DashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, error, info};
use uuid::Uuid;

use crate::error::{ProxyError, Result};
use crate::transport::{create_transport, TransportAdapter, TransportType};

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
    stop_tx: broadcast::Sender<()>,
    task_handle: Option<JoinHandle<()>>,
}

impl Session {
    /// Create a new session with given configuration
    pub async fn new(
        config: SessionConfig,
        event_tx: mpsc::UnboundedSender<SessionEvent>,
    ) -> Result<Self> {
        let id = Uuid::new_v4().to_string();
        let created_at = current_timestamp();

        info!("Creating session {}: listen={}, forward={}, dest={}, protocol={:?}",
            id, config.listen_addr, config.forward_addr, config.destination_addr, config.protocol);

        // Create transport adapter
        let transport = create_transport(
            config.protocol.clone(),
            config.listen_addr,
            config.forward_addr,
        )
        .await?;

        let (stop_tx, _stop_rx) = broadcast::channel::<()>(1);

        let session = Self {
            id: id.clone(),
            config: config.clone(),
            state: Arc::new(tokio::sync::RwLock::new(SessionState::Connecting)),
            stats: Arc::new(TrafficStats::new()),
            created_at,
            last_activity: Arc::new(tokio::sync::RwLock::new(created_at)),
            error_message: Arc::new(tokio::sync::RwLock::new(None)),
            transport: Arc::new(transport),
            stop_tx,
            task_handle: None,
        };

        // Send creation event
        let _ = event_tx.send(SessionEvent::Created {
            session_id: id.clone(),
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

        // Start forwarding task
        let session_id = self.id.clone();
        let transport = Arc::clone(&self.transport);
        let stats = Arc::clone(&self.stats);
        let last_activity = Arc::clone(&self.last_activity);
        let destination = self.config.destination_addr;
        let state = Arc::clone(&self.state);
        let error_message = Arc::clone(&self.error_message);
        let event_tx_clone = event_tx.clone();
        let mut stop_rx = self.stop_tx.subscribe();

        let task_handle = tokio::spawn(async move {
            info!("Session {} forwarding task started", session_id);

            loop {
                tokio::select! {
                    _ = stop_rx.recv() => {
                        info!("Session {} received stop signal", session_id);
                        break;
                    }
                    recv_result = transport.recv() => {
                        match recv_result {
                            Ok((data, src)) => {
                                let len = data.len() as u64;
                                stats.record_received(len);
                                *last_activity.write().await = current_timestamp();

                                debug!("Session {}: received {} bytes from {}", session_id, len, src);

                                // Forward data to destination
                                match transport.send(data, destination).await {
                                    Ok(sent) => {
                                        stats.record_sent(sent as u64);
                                        debug!("Session {}: forwarded {} bytes to {}", session_id, sent, destination);
                                    }
                                    Err(e) => {
                                        error!("Session {}: failed to forward data: {}", session_id, e);
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
                                error!("Session {}: recv error: {}", session_id, e);
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

            info!("Session {} forwarding task stopped", session_id);
        });

        self.task_handle = Some(task_handle);

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

        // Wait for task to finish
        if let Some(handle) = self.task_handle.take() {
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

    /// Create a new session with the given configuration
    pub async fn create_session(&self, config: SessionConfig) -> Result<String> {
        let mut session = Session::new(config, self.event_tx.clone()).await?;
        let session_id = session.id.clone();

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
