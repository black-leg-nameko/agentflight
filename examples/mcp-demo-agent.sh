#!/usr/bin/env sh
set -eu

printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"agentflight-demo","version":"1.0"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo","arguments":{"token":"sk-abcdefghijklmnopqrstuv"}}}' \
  | agentflight mcp-proxy -- sh tests/fixtures/mock-mcp-server.sh
