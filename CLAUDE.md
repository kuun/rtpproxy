# Development Requirements and Specifications

## Project Requirements

This document records the requirements and specifications provided during the development of the RTP Proxy project.

### Initial Requirements

**Date**: 2025-10-17

**Core Functionality Requirements**:

1. **Multi-Network Interface Support**
   - Support listening on one network interface and forwarding traffic through another
   - Bind listen address and forward address separately for multi-homed proxy scenarios
   - Allow explicit specification of network interface/address for both inbound and outbound traffic
   - Enable traffic to enter via one interface and exit via another

2. **Protocol Layer Support**
   - Support UDP-based RTP traffic forwarding (primary requirement)
   - Support TCP-based RTP traffic forwarding (secondary requirement)
   - Implement separate I/O handling logic for UDP (connectionless) and TCP (connection-oriented)
   - Create transport layer adapters for each protocol type

3. **gRPC Control Interface Design**
   - `CreateSession`: Create session with parameters:
     - Listen address/port (where to receive traffic)
     - Connect address/port (source address for outbound traffic)
     - Destination address/port (final destination)
     - Protocol type (UDP/TCP)
   - `DestroySession`: Destroy specified session
   - `GetSessionStatus`: Query real-time session status with media statistics (packet loss rate, latency, bytes forwarded)
   - `ListSessions`: List all active sessions

4. **Session State Monitoring**
   - Track detailed statistics per session:
     - Inbound traffic stats (packets received, bytes, packet loss)
     - Outbound traffic stats
     - Session establishment time
     - Last activity time
     - Connection state (normal forwarding or error)
   - Support periodic push to SIP controller or on-demand query

5. **Architecture Components**
   - `SessionManager`: Responsible for session creation and destruction
   - `TransportAdapter`: Handle UDP/TCP protocol differences with unified interface
   - `gRPC Server`: Handle control requests
   - `Statistics Collector`: Real-time media data collection

6. **Traffic Forwarding Logic**
   - Establish two forwarding channels per session:
     - Listen address → forward to destination address
     - Handle reverse traffic from destination (optional)
   - Maintain source IP/port to session mapping for UDP scenarios

7. **Configuration Parameters**
   - Local listen interface/address and port
   - Proxy forward interface/address and port
   - Destination endpoint address and port
   - Protocol type (UDP or TCP)
   - Optional: timeout settings, statistics reporting interval

8. **Real-time Feedback Mechanism**
   - Passive query via `GetSessionStatus`
   - Active push mode using gRPC streaming
   - Notify SIP controller when session state changes or alert conditions trigger (e.g., high packet loss)

9. **Fault Handling**
   - Handle destination unreachable scenarios
   - Network interruption handling
   - Connection timeout handling
   - Reconnection strategy and state transition logic (connecting, forwarding, error, closed states)

### Implementation Language

**Requirement**: Implement in Rust

### Code Quality Requirements

**Date**: 2025-10-17

1. **Rust Edition**: Use Rust 2024 Edition
2. **Comments and Logs**: All comments and log messages must be in English
3. **Network Interface**: Endpoint message should NOT include interface field (only address and port)
4. **Session ID**: Session ID must be user-specified (string type) when creating a session via gRPC API, not auto-generated

### Technical Decisions Made

1. **Transport Layer**:
   - UDP: Fully implemented with dual-socket architecture
   - TCP: Framework/placeholder implementation for future development

2. **Concurrency Model**:
   - Lock-free statistics using atomic operations
   - Broadcast channels for graceful shutdown
   - DashMap for concurrent session storage

3. **Statistics Tracking**:
   - Real-time atomic counters for packets and bytes
   - Automatic packet loss rate calculation
   - Per-session last activity timestamp

4. **Event System**:
   - Unbounded MPSC channels for event propagation
   - Five event types: Created, StateChanged, StatsUpdate, Error, Closed
   - Optional periodic statistics reporting

### Current Limitations

1. TCP transport is a placeholder (not fully implemented)
2. Latency metrics (avg_latency_ms) not yet calculated
3. Primarily unidirectional traffic (client → destination)
4. No RTCP support
5. No packet validation or RTP header parsing

### Future Considerations

1. Complete TCP transport implementation
2. Bidirectional RTP traffic handling
3. RTCP support
4. Packet validation and RTP header parsing
5. Session persistence
6. Authentication and authorization
7. Rate limiting and traffic shaping
8. Metrics export (Prometheus, etc.)

---

## Notes

This document should be updated whenever new requirements or specifications are provided.
