# RTP Proxy

A high-performance RTP (Real-time Transport Protocol) proxy server implemented in Rust with gRPC control interface.

## Features

- **Multi-network Interface Support**: Bind to different network interfaces for listening and forwarding traffic
- **Protocol Support**: Both UDP and TCP transport layers (UDP fully implemented)
- **RTCP Support**: Automatic RTCP forwarding on port + 1 for UDP sessions
- **gRPC Control Interface**: Modern API for session management and monitoring
- **Real-time Statistics**: Track packets, bytes, packet loss rates, and session activity
- **Event Streaming**: Subscribe to session state changes and statistics updates
- **Concurrent Session Management**: Handle multiple RTP sessions simultaneously

## Architecture

```
┌─────────────────┐
│  SIP Controller │
│   (gRPC Client) │
└────────┬────────┘
         │ gRPC
         │
┌────────▼────────────────────────────┐
│      RTP Proxy Server               │
│  ┌──────────────────────────────┐  │
│  │   Session Manager            │  │
│  └──────────────────────────────┘  │
│  ┌──────────────────────────────┐  │
│  │   Transport Adapters         │  │
│  │   - UDP (implemented)        │  │
│  │   - TCP (placeholder)        │  │
│  └──────────────────────────────┘  │
└─────────────────────────────────────┘
         │                 │
    Listen Address    Forward Address
         │                 │
    ┌────▼────┐      ┌────▼────────┐
    │  Client │      │ Destination │
    └─────────┘      └─────────────┘
```

## Building

```bash
cargo build --release
```

## Running

Start the RTP proxy server:

```bash
cargo run --release
```

By default, the gRPC server listens on `[::]:50051`.

Set log level (optional):
```bash
RUST_LOG=info cargo run --release
# or
RUST_LOG=debug cargo run --release
```

## gRPC API

### Service Definition

The proxy provides the following gRPC services:

#### CreateSession
Create a new RTP proxy session.

**Request:**
- `session_id`: User-specified session identifier (required, must be unique)
- `listen_endpoint`: Address/port to listen for incoming RTP traffic
- `forward_endpoint`: Address/port to use for outgoing traffic (source binding)
- `destination_endpoint`: Address/port of the final destination
- `protocol`: UDP (1) or TCP (2)
- `timeout_seconds`: Session timeout (0 = no timeout)
- `stats_interval_seconds`: Statistics reporting interval (0 = no automatic reporting)

**Response:**
- `session_id`: Echo of the user-specified session ID
- `created_at`: Unix timestamp of session creation

#### DestroySession
Destroy an existing session.

**Request:**
- `session_id`: ID of the session to destroy

**Response:**
- `success`: Whether the operation succeeded
- `message`: Status message

#### GetSessionStatus
Get detailed status of a session.

**Request:**
- `session_id`: ID of the session to query

**Response:**
- `session`: Complete session information including statistics

#### ListSessions
List all active sessions with optional filtering.

**Request:**
- `state_filter`: Optional filter by session state

**Response:**
- `sessions`: List of session information
- `total_count`: Number of sessions returned

#### StreamSessionEvents
Stream real-time session events.

**Request:**
- `session_ids`: Optional list of session IDs to monitor (empty = all sessions)

**Response Stream:**
- `event_type`: Type of event (CREATED, STATE_CHANGED, STATS_UPDATE, ERROR, CLOSED)
- `session`: Associated session information
- `timestamp`: Event timestamp
- `description`: Human-readable event description

## Usage Example

Using `grpcurl` to interact with the proxy:

### Create a Session

```bash
grpcurl -plaintext -d '{
  "session_id": "my-rtp-session-001",
  "listen_endpoint": {"address": "0.0.0.0", "port": 20000},
  "forward_endpoint": {"address": "0.0.0.0", "port": 20001},
  "destination_endpoint": {"address": "192.168.1.100", "port": 30000},
  "protocol": 1,
  "timeout_seconds": 0,
  "stats_interval_seconds": 5
}' localhost:50051 rtpproxy.RtpProxy/CreateSession
```

**Response:**
```json
{
  "session_id": "my-rtp-session-001",
  "created_at": "1234567890"
}
```

### Get Session Status

```bash
grpcurl -plaintext -d '{
  "session_id": "my-rtp-session-001"
}' localhost:50051 rtpproxy.RtpProxy/GetSessionStatus
```

### List All Sessions

```bash
grpcurl -plaintext -d '{}' localhost:50051 rtpproxy.RtpProxy/ListSessions
```

### Stream Session Events

```bash
grpcurl -plaintext -d '{
  "session_ids": ["my-rtp-session-001"]
}' localhost:50051 rtpproxy.RtpProxy/StreamSessionEvents
```

### Destroy a Session

```bash
grpcurl -plaintext -d '{
  "session_id": "my-rtp-session-001"
}' localhost:50051 rtpproxy.RtpProxy/DestroySession
```

## Session Flow

1. **SIP Controller** creates a session via `CreateSession` (specifying RTP port, e.g., 20000)
2. **Proxy** automatically binds:
   - RTP: listen address port 20000 → forward to destination port 30000
   - RTCP: listen address port 20001 → forward to destination port 30001
3. **Client** sends RTP packets to port 20000, RTCP to port 20001
4. **Proxy** receives packets, updates statistics, and forwards to destination
5. **Statistics** are collected and optionally streamed to the controller
6. When done, **SIP Controller** destroys the session via `DestroySession`

## Statistics

Each session tracks:
- **Packets Received**: Number of packets received from the client
- **Bytes Received**: Total bytes received
- **Packets Sent**: Number of packets forwarded to destination
- **Bytes Sent**: Total bytes sent
- **Packets Lost**: Packets that failed to forward
- **Packet Loss Rate**: Calculated as (packets_lost / packets_received) * 100

## Network Interface Binding

The proxy supports binding to specific network interfaces through address configuration:

- **Listen Endpoint**: Specifies which interface receives traffic (e.g., `192.168.1.10:20000` for specific interface, or `0.0.0.0:20000` for all interfaces)
- **Forward Endpoint**: Specifies which interface/source address is used for outgoing traffic

This allows traffic to enter via one network interface and exit via another, which is essential for multi-homed proxy scenarios.

## Project Structure

```
rtpproxy/
├── Cargo.toml              # Project dependencies
├── build.rs                # Build script for protobuf
├── proto/
│   └── rtpproxy.proto      # gRPC service definition
├── src/
│   ├── main.rs             # Application entry point
│   ├── error/
│   │   └── mod.rs          # Error types
│   ├── transport/
│   │   └── mod.rs          # Transport layer adapters (UDP/TCP)
│   ├── session/
│   │   └── mod.rs          # Session management and statistics
│   └── grpc_server/
│       └── mod.rs          # gRPC service implementation
└── README.md
```

## RTCP Support

For UDP sessions, the proxy automatically handles RTCP traffic:
- RTCP uses the same address as RTP but port + 1
- Listen port 20000 (RTP) → automatically creates port 20001 (RTCP)
- Forward port 20001 (RTP) → automatically creates port 20002 (RTCP)
- Destination port 30000 (RTP) → RTCP forwarded to port 30001
- Separate async task handles RTCP forwarding independently from RTP

## Current Limitations

- **TCP Transport**: Currently a placeholder; only UDP is fully implemented
- **Latency Metrics**: Average latency calculation not yet implemented
- **Bidirectional Traffic**: Current implementation is primarily unidirectional (client → destination)

## Future Enhancements

- Complete TCP transport implementation
- Bidirectional RTP traffic handling
- Advanced packet manipulation (transcoding, filtering)
- Persistent session storage
- Authentication and authorization
- Metrics export (Prometheus, etc.)

## License

MIT

## Contributing

Contributions are welcome! Please feel free to submit issues or pull requests.
