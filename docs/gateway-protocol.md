# Gateway Protocol

The MyLobster gateway provides a WebSocket server (JSON-RPC) and HTTP endpoints for client communication. Protocol version: 1. Maximum WebSocket payload: 25 MB.

## Connection Flow

```
Client                          Gateway
  │                                │
  │──── WS connect /ws ───────────▶│
  │                                │
  │◀──── HelloFrame ──────────────│  (protocol, version, capabilities)
  │                                │
  │──── ConnectAuth ──────────────▶│  (optional, if not using query token)
  │                                │
  │──── RequestFrame ─────────────▶│  (method: "chat.send", params: {...})
  │                                │
  │◀──── EventFrame ──────────────│  (event: "chat", data: ChatEvent)
  │◀──── EventFrame ──────────────│  (streaming deltas...)
  │◀──── EventFrame ──────────────│  (state: "final")
  │                                │
  │◀──── ResponseFrame ───────────│  (id matches request, result: ok)
```

## Frame Types

All frames are JSON objects sent over the WebSocket connection.

### HelloFrame (server → client)

Sent immediately after WebSocket connection is established.

```json
{
  "type": "hello",
  "protocol": "mylobster",
  "server": "mylobster-gateway",
  "version": "2026.2.14",
  "capabilities": ["chat", "sessions", "tools", "memory", "channels"],
  "challenge": null
}
```

### RequestFrame (client → server)

```json
{
  "type": "request",
  "id": "req-1",
  "method": "chat.send",
  "params": {
    "sessionKey": "session-abc",
    "message": "Hello, what can you do?"
  },
  "seq": 1
}
```

### ResponseFrame (server → client)

```json
{
  "type": "response",
  "id": "req-1",
  "result": { "status": "ok" },
  "error": null,
  "seq": 2
}
```

### EventFrame (server → client)

```json
{
  "type": "event",
  "event": "chat",
  "data": {
    "run_id": "run-xyz",
    "session_key": "session-abc",
    "seq": 1,
    "state": "delta",
    "message": "I can help you with..."
  },
  "seq": 3
}
```

## WebSocket Methods

| Method | Direction | Description |
|--------|-----------|-------------|
| `chat.send` | client → server | Send a message for AI processing; streams `ChatEvent` responses |
| `sessions.list` | client → server | List all sessions |
| `sessions.get` | client → server | Get a specific session by key |
| `sessions.patch` | client → server | Update session title, model, or thinking mode |
| `sessions.delete` | client → server | Delete a session |
| `tools.list` | client → server | List available agent tools |
| `channels.status` | client → server | Get status of all channel integrations |
| `memory.search` | client → server | Search the memory store |
| `gateway.info` | client → server | Get gateway version and capabilities |
| `config.reload` | client → server | Reload configuration from disk |
| `presence.set` | client → server | Set user presence status |
| `cron.list` | client → server | List scheduled cron jobs |

## Chat Protocol

### ChatSendParams

```json
{
  "sessionKey": "session-abc",
  "message": "What is quantum computing?",
  "thinking": true,
  "deliver": true,
  "attachments": [],
  "timeout_ms": 120000,
  "idempotency_key": "idem-123"
}
```

### ChatEvent (streamed via EventFrame)

```json
{
  "run_id": "run-xyz",
  "session_key": "session-abc",
  "seq": 1,
  "state": "delta",
  "message": "Quantum computing is...",
  "usage": {
    "input_tokens": 150,
    "output_tokens": 42,
    "cache_read_input_tokens": 0,
    "cache_creation_input_tokens": 0
  },
  "stop_reason": null
}
```

**State values:**

| State | Meaning |
|-------|---------|
| `delta` | Partial streaming content |
| `final` | Complete response, includes final usage |
| `aborted` | Generation was cancelled |
| `error` | An error occurred |

## HTTP Endpoints

### Core

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `GET` | `/api/health` | No | Health check (includes gateway connectivity status) |
| `GET` | `/ws` | Token | WebSocket upgrade (primary endpoint) |
| `GET` | `/api/chat` | Token | WebSocket upgrade (alias for compatibility) |

### Sessions

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `GET` | `/api/sessions` | Token | List all sessions |
| `GET` | `/api/sessions/{id}` | Token | Get session details |

### Tools & Memory

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `GET` | `/api/tools` | Token | List available tools |
| `POST` | `/api/memory/search` | Token | Search the memory store |

### Channels

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `GET` | `/api/channels/status` | Token | Status of all channel integrations |

### Gateway

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `GET` | `/api/gateway/info` | Token | Gateway version and info |

### OpenAI-Compatible

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `POST` | `/v1/chat/completions` | Bearer | OpenAI-compatible chat completions API |
| `POST` | `/v1/responses` | Bearer | OpenAI-compatible responses API |

## Authentication

Three auth modes, configured via `gateway.auth`:

| Mode | How it works |
|------|--------------|
| **Token** | Bearer token in `Authorization` header or `?token=` query param |
| **Password** | Username/password verified against bcrypt hash |
| **Tailscale** | Trusted identity from Tailscale network headers |

Local requests from loopback (`127.0.0.1`, `::1`) can bypass auth if `gateway.auth.allowLocalBypass` is enabled.

## OpenAI-Compatible API

The gateway exposes OpenAI-compatible endpoints for drop-in compatibility with tools that support the OpenAI API format.

### Chat Completions

```bash
curl -X POST http://localhost:18789/v1/chat/completions \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "anthropic/claude-sonnet-4-5-20250929",
    "messages": [{"role": "user", "content": "Hello"}],
    "stream": true
  }'
```

### Responses API

```bash
curl -X POST http://localhost:18789/v1/responses \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "anthropic/claude-sonnet-4-5-20250929",
    "input": "What is Rust?"
  }'
```
