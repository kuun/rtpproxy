#!/bin/bash

# Example script demonstrating how to interact with the RTP Proxy via gRPC
# Requires grpcurl to be installed: https://github.com/fullstorydev/grpcurl

PROXY_ADDR="localhost:50051"

echo "=== RTP Proxy Client Example ==="
echo ""

# 1. Create a session
echo "1. Creating a new session..."
SESSION_ID="example-session-$(date +%s)"
SESSION_RESPONSE=$(grpcurl --proto proto/rtpproxy.proto -plaintext -d "{
  \"session_id\": \"$SESSION_ID\",
  \"listen_endpoint\": {\"address\": \"0.0.0.0\", \"port\": 20000},
  \"forward_endpoint\": {\"address\": \"0.0.0.0\", \"port\": 20001},
  \"destination_endpoint\": {\"address\": \"127.0.0.1\", \"port\": 30000},
  \"protocol\": 1,
  \"timeout_seconds\": 0,
  \"stats_interval_seconds\": 5
}" $PROXY_ADDR rtpproxy.RtpProxy/CreateSession 2>&1)

echo "$SESSION_RESPONSE"
echo ""
echo "Created session: $SESSION_ID"
echo ""

# 2. Get session status
echo "2. Getting session status..."
grpcurl -plaintext -d "{
  \"session_id\": \"$SESSION_ID\"
}" $PROXY_ADDR rtpproxy.RtpProxy/GetSessionStatus
echo ""

# 3. List all sessions
echo "3. Listing all sessions..."
grpcurl -plaintext -d '{}' $PROXY_ADDR rtpproxy.RtpProxy/ListSessions
echo ""

# 4. Wait a bit
echo "4. Waiting 10 seconds (you can send UDP packets to port 20000 now)..."
sleep 10
echo ""

# 5. Check status again to see statistics
echo "5. Checking session status again..."
grpcurl -plaintext -d "{
  \"session_id\": \"$SESSION_ID\"
}" $PROXY_ADDR rtpproxy.RtpProxy/GetSessionStatus
echo ""

# 6. Destroy the session
echo "6. Destroying the session..."
grpcurl -plaintext -d "{
  \"session_id\": \"$SESSION_ID\"
}" $PROXY_ADDR rtpproxy.RtpProxy/DestroySession
echo ""

echo "=== Example Complete ==="
