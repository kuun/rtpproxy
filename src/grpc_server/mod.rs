use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::{wrappers::UnboundedReceiverStream, Stream};
use tonic::{Request, Response, Status};
use tracing::{error, info, warn};

use crate::error::ProxyError;
use crate::session::{SessionConfig, SessionEvent, SessionManager, SessionState};
use crate::transport::TransportType;

// Import generated protobuf code
pub mod proto {
    tonic::include_proto!("rtpproxy");
}

use proto::{
    rtp_proxy_server::{RtpProxy, RtpProxyServer},
    CreateSessionRequest, CreateSessionResponse, DestroySessionRequest, DestroySessionResponse,
    GetSessionStatusRequest, GetSessionStatusResponse, ListSessionsRequest, ListSessionsResponse,
    Protocol, SessionEvent as ProtoSessionEvent, SessionEventType, SessionInfo as ProtoSessionInfo,
    SessionState as ProtoSessionState, StreamSessionEventsRequest, TrafficStats,
};

/// gRPC service implementation
pub struct RtpProxyService {
    session_manager: Arc<SessionManager>,
}

impl RtpProxyService {
    pub fn new(session_manager: Arc<SessionManager>) -> Self {
        Self { session_manager }
    }
}

#[tonic::async_trait]
impl RtpProxy for RtpProxyService {
    /// Create a new RTP proxy session
    async fn create_session(
        &self,
        request: Request<CreateSessionRequest>,
    ) -> Result<Response<CreateSessionResponse>, Status> {
        let req = request.into_inner();

        // Validate session_id
        if req.session_id.is_empty() {
            return Err(Status::invalid_argument("session_id is required and cannot be empty"));
        }

        info!(
            "Received CreateSession request: session_id={}, listen={:?}, forward={:?}, dest={:?}",
            req.session_id, req.listen_endpoint, req.forward_endpoint, req.destination_endpoint
        );

        // Parse endpoints
        let listen_endpoint = req
            .listen_endpoint
            .ok_or_else(|| Status::invalid_argument("listen_endpoint is required"))?;
        let forward_endpoint = req
            .forward_endpoint
            .ok_or_else(|| Status::invalid_argument("forward_endpoint is required"))?;
        let destination_endpoint = req
            .destination_endpoint
            .ok_or_else(|| Status::invalid_argument("destination_endpoint is required"))?;

        let listen_addr = parse_endpoint(&listen_endpoint)?;
        let forward_addr = parse_endpoint(&forward_endpoint)?;
        let destination_addr = parse_endpoint(&destination_endpoint)?;

        // Parse protocol
        let protocol = match Protocol::try_from(req.protocol) {
            Ok(Protocol::Udp) => TransportType::Udp,
            Ok(Protocol::Tcp) => TransportType::Tcp,
            _ => return Err(Status::invalid_argument("Invalid or unsupported protocol")),
        };

        // Create session config
        let config = SessionConfig {
            listen_addr,
            forward_addr,
            destination_addr,
            protocol,
            timeout_seconds: req.timeout_seconds,
            stats_interval_seconds: req.stats_interval_seconds,
        };

        // Create session with user-specified session_id
        match self.session_manager.create_session(req.session_id.clone(), config).await {
            Ok(session_id) => {
                info!("Session {} created successfully", session_id);

                Ok(Response::new(CreateSessionResponse {
                    session_id,
                    created_at: chrono::Utc::now().timestamp(),
                }))
            }
            Err(ProxyError::SessionExists(id)) => {
                warn!("Session {} already exists", id);
                Err(Status::already_exists(format!("Session {} already exists", id)))
            }
            Err(e) => {
                error!("Failed to create session: {}", e);
                Err(Status::internal(format!("Failed to create session: {}", e)))
            }
        }
    }

    /// Destroy an existing session
    async fn destroy_session(
        &self,
        request: Request<DestroySessionRequest>,
    ) -> Result<Response<DestroySessionResponse>, Status> {
        let req = request.into_inner();
        info!("Received DestroySession request for session {}", req.session_id);

        match self.session_manager.destroy_session(&req.session_id).await {
            Ok(_) => {
                info!("Session {} destroyed successfully", req.session_id);
                Ok(Response::new(DestroySessionResponse {
                    success: true,
                    message: "Session destroyed successfully".to_string(),
                }))
            }
            Err(ProxyError::SessionNotFound(id)) => {
                warn!("Session {} not found", id);
                Err(Status::not_found(format!("Session {} not found", id)))
            }
            Err(e) => {
                error!("Failed to destroy session {}: {}", req.session_id, e);
                Err(Status::internal(format!("Failed to destroy session: {}", e)))
            }
        }
    }

    /// Get status of a specific session
    async fn get_session_status(
        &self,
        request: Request<GetSessionStatusRequest>,
    ) -> Result<Response<GetSessionStatusResponse>, Status> {
        let req = request.into_inner();

        match self.session_manager.get_session_info(&req.session_id).await {
            Ok(info) => Ok(Response::new(GetSessionStatusResponse {
                session: Some(convert_session_info_to_proto(info)),
            })),
            Err(ProxyError::SessionNotFound(id)) => {
                Err(Status::not_found(format!("Session {} not found", id)))
            }
            Err(e) => Err(Status::internal(format!("Failed to get session status: {}", e))),
        }
    }

    /// List all sessions
    async fn list_sessions(
        &self,
        request: Request<ListSessionsRequest>,
    ) -> Result<Response<ListSessionsResponse>, Status> {
        let req = request.into_inner();

        // Parse state filter
        let state_filter = req.state_filter.and_then(|s| {
            ProtoSessionState::try_from(s).ok().and_then(|proto_state| match proto_state {
                ProtoSessionState::Connecting => Some(SessionState::Connecting),
                ProtoSessionState::Active => Some(SessionState::Active),
                ProtoSessionState::Error => Some(SessionState::Error),
                ProtoSessionState::Closed => Some(SessionState::Closed),
                _ => None,
            })
        });

        let sessions = self.session_manager.list_sessions(state_filter).await;
        let total_count = sessions.len() as u32;

        let proto_sessions: Vec<ProtoSessionInfo> = sessions
            .into_iter()
            .map(convert_session_info_to_proto)
            .collect();

        Ok(Response::new(ListSessionsResponse {
            sessions: proto_sessions,
            total_count,
        }))
    }

    /// Stream session events
    type StreamSessionEventsStream =
        Pin<Box<dyn Stream<Item = Result<ProtoSessionEvent, Status>> + Send>>;

    async fn stream_session_events(
        &self,
        request: Request<StreamSessionEventsRequest>,
    ) -> Result<Response<Self::StreamSessionEventsStream>, Status> {
        let req = request.into_inner();
        let filter_session_ids: Option<Vec<String>> = if req.session_ids.is_empty() {
            None
        } else {
            Some(req.session_ids)
        };

        info!(
            "Starting event stream with filter: {:?}",
            filter_session_ids
        );

        // Create a new channel for this stream
        let (tx, rx) = mpsc::unbounded_channel();

        // Get event receiver from session manager
        let event_rx = self.session_manager.subscribe_events();

        // Spawn task to forward events to this stream
        tokio::spawn(async move {
            let mut event_rx = event_rx.lock().await;

            while let Some(event) = event_rx.recv().await {
                // Apply session ID filter if specified
                let session_id = match &event {
                    SessionEvent::Created { session_id, .. } => session_id,
                    SessionEvent::StateChanged { session_id, .. } => session_id,
                    SessionEvent::StatsUpdate { session_id, .. } => session_id,
                    SessionEvent::Error { session_id, .. } => session_id,
                    SessionEvent::Closed { session_id, .. } => session_id,
                };

                if let Some(ref filter) = filter_session_ids {
                    if !filter.contains(session_id) {
                        continue;
                    }
                }

                // Convert and send event
                if let Some(proto_event) = convert_event_to_proto(event) {
                    if tx.send(Ok(proto_event)).is_err() {
                        break;
                    }
                }
            }
        });

        let output_stream = UnboundedReceiverStream::new(rx);
        Ok(Response::new(Box::pin(output_stream)))
    }
}

/// Parse endpoint from protobuf message
fn parse_endpoint(endpoint: &proto::Endpoint) -> Result<SocketAddr, Status> {
    let addr_str = format!("{}:{}", endpoint.address, endpoint.port);
    addr_str
        .parse()
        .map_err(|e| Status::invalid_argument(format!("Invalid endpoint address: {}", e)))
}

/// Convert internal SessionInfo to protobuf SessionInfo
fn convert_session_info_to_proto(info: crate::session::SessionInfo) -> ProtoSessionInfo {
    ProtoSessionInfo {
        session_id: info.session_id,
        state: match info.state {
            SessionState::Connecting => ProtoSessionState::Connecting as i32,
            SessionState::Active => ProtoSessionState::Active as i32,
            SessionState::Error => ProtoSessionState::Error as i32,
            SessionState::Closed => ProtoSessionState::Closed as i32,
        },
        listen_endpoint: Some(proto::Endpoint {
            address: info.config.listen_addr.ip().to_string(),
            port: info.config.listen_addr.port() as u32,
        }),
        forward_endpoint: Some(proto::Endpoint {
            address: info.config.forward_addr.ip().to_string(),
            port: info.config.forward_addr.port() as u32,
        }),
        destination_endpoint: Some(proto::Endpoint {
            address: info.config.destination_addr.ip().to_string(),
            port: info.config.destination_addr.port() as u32,
        }),
        protocol: match info.config.protocol {
            TransportType::Udp => Protocol::Udp as i32,
            TransportType::Tcp => Protocol::Tcp as i32,
        },
        created_at: info.created_at,
        last_activity_at: info.last_activity,
        stats: Some(TrafficStats {
            packets_received: info.stats.packets_received,
            bytes_received: info.stats.bytes_received,
            packets_sent: info.stats.packets_sent,
            bytes_sent: info.stats.bytes_sent,
            packets_lost: info.stats.packets_lost,
            packet_loss_rate: info.stats.packet_loss_rate(),
            avg_latency_ms: 0.0, // Not implemented yet
        }),
        error_message: info.error_message.unwrap_or_default(),
    }
}

/// Convert internal SessionEvent to protobuf SessionEvent
fn convert_event_to_proto(event: SessionEvent) -> Option<ProtoSessionEvent> {
    match event {
        SessionEvent::Created {
            session_id,
            timestamp,
        } => Some(ProtoSessionEvent {
            event_type: SessionEventType::SessionCreated as i32,
            session: None,
            timestamp,
            description: format!("Session {} created", session_id),
        }),
        SessionEvent::StateChanged {
            session_id,
            new_state,
            timestamp,
            ..
        } => Some(ProtoSessionEvent {
            event_type: SessionEventType::SessionStateChanged as i32,
            session: None,
            timestamp,
            description: format!("Session {} state changed to {:?}", session_id, new_state),
        }),
        SessionEvent::StatsUpdate {
            session_id,
            stats,
            timestamp,
        } => Some(ProtoSessionEvent {
            event_type: SessionEventType::SessionStatsUpdate as i32,
            session: Some(ProtoSessionInfo {
                session_id: session_id.clone(),
                state: ProtoSessionState::Active as i32,
                listen_endpoint: None,
                forward_endpoint: None,
                destination_endpoint: None,
                protocol: Protocol::Udp as i32,
                created_at: 0,
                last_activity_at: timestamp,
                stats: Some(TrafficStats {
                    packets_received: stats.packets_received,
                    bytes_received: stats.bytes_received,
                    packets_sent: stats.packets_sent,
                    bytes_sent: stats.bytes_sent,
                    packets_lost: stats.packets_lost,
                    packet_loss_rate: stats.packet_loss_rate(),
                    avg_latency_ms: 0.0,
                }),
                error_message: String::new(),
            }),
            timestamp,
            description: format!("Session {} statistics updated", session_id),
        }),
        SessionEvent::Error {
            session_id,
            error,
            timestamp,
        } => Some(ProtoSessionEvent {
            event_type: SessionEventType::SessionError as i32,
            session: None,
            timestamp,
            description: format!("Session {} error: {}", session_id, error),
        }),
        SessionEvent::Closed {
            session_id,
            timestamp,
        } => Some(ProtoSessionEvent {
            event_type: SessionEventType::SessionClosed as i32,
            session: None,
            timestamp,
            description: format!("Session {} closed", session_id),
        }),
    }
}

/// Create and return the gRPC server
pub fn create_grpc_server(
    session_manager: Arc<SessionManager>,
) -> RtpProxyServer<RtpProxyService> {
    let service = RtpProxyService::new(session_manager);
    RtpProxyServer::new(service)
}
