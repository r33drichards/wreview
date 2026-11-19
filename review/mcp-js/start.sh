#!/bin/sh
set -eu
: "${REVIEW_MCP_TOKEN:?set the private review bearer token}"

env -u REVIEW_MCP_TOKEN mcp-v8 --http-port 8080 --heap-store dir --heap-dir /tmp/wreview-heaps &
mcp_pid=$!
caddy run --config /opt/wreview/Caddyfile --adapter caddyfile &
proxy_pid=$!
trap 'kill "$mcp_pid" "$proxy_pid" 2>/dev/null || true' EXIT
trap 'exit 0' TERM INT
while kill -0 "$mcp_pid" 2>/dev/null && kill -0 "$proxy_pid" 2>/dev/null; do
    sleep 1
done
exit 1
