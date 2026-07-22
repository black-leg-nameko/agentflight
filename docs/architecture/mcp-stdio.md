# MCP stdio capture

`agentflight mcp-proxy -- <server command>` is a transparent stdio proxy. It forwards newline-delimited UTF-8 JSON-RPC messages in both directions and leaves server stderr on stderr. When launched inside `agentflight record`, the parent recorder supplies a private capture path through `AGENTFLIGHT_MCP_CAPTURE`.

The proxy applies the standard Redactor before appending a capture record. The original message is forwarded to its intended peer but is never persisted by AgentFlight. At Run completion, capture records are correlated by JSON-RPC `id` and normalized into:

- `mcp.initialize` / `mcp.initialize.result`
- `mcp.tools.list` / `mcp.tools.list.result`
- `mcp.tool.call` / `mcp.tool.result` / `mcp.tool.error`
- generic request, response, notification, and error events

The raw redacted capture is retained for crash recovery but excluded from exported `.afrun` bundles. The normalized Event stream is the portable interface.

## Configuring a client

Replace the configured MCP server executable with AgentFlight and place the original command after `--`. For example:

```sh
agentflight mcp-proxy -- npx -y @modelcontextprotocol/server-filesystem .
```

Run the client itself under the recorder so the proxy inherits the capture context:

```sh
agentflight record -- your-agent
agentflight inspect latest --events
```

The current adapter targets stdio. Streamable HTTP and legacy HTTP/SSE are separate adapters.
