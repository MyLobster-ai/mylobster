# Sessions

MyLobster maintains per-session conversation state using an in-memory session store. Sessions track conversation history, model preferences, and metadata.

## Architecture

Sessions are stored in a `DashMap<String, SessionHandle>` for lock-free concurrent access. There is no persistence — sessions are lost on gateway restart.

## SessionStore (`src/sessions/mod.rs`)

```rust
let store = SessionStore::new();

// Create or retrieve a session
let handle = store.get_or_create_session("my-session", &config);

// List all sessions
let sessions = store.list_sessions();

// Update session metadata
store.patch_session(SessionPatchParams {
    session_key: "my-session".into(),
    title: Some("My Chat".into()),
    model: None,
    thinking: None,
});

// Delete a session
store.delete_session("my-session");
```

### Methods

| Method | Description |
|--------|-------------|
| `get_or_create_session(key, config)` | Returns existing session or creates a new one with defaults from config |
| `get_session(key)` | Returns session info if it exists |
| `list_sessions()` | Returns all sessions as `Vec<SessionInfo>` |
| `active_count()` | Number of active sessions |
| `patch_session(params)` | Update title, model, or thinking mode |
| `delete_session(key)` | Remove a session and its history |

## SessionHandle

Each session wraps a `SessionInfo` and a conversation history:

```rust
pub struct SessionHandle {
    info: SessionInfo,
    history: Vec<ProviderMessage>,
}
```

### SessionInfo

```rust
pub struct SessionInfo {
    pub id: String,
    pub session_key: String,
    pub agent_id: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub thinking: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

### Methods

| Method | Description |
|--------|-------------|
| `get_history()` | Returns a clone of the conversation message history |
| `add_message(msg)` | Appends a `ProviderMessage` to the history |

Thread safety is provided by `parking_lot::RwLock`.

## WebSocket Access

Sessions are managed via WebSocket JSON-RPC methods:

| Method | Description |
|--------|-------------|
| `sessions.list` | List all sessions |
| `sessions.get` | Get a session by key |
| `sessions.patch` | Update session title/model/thinking |
| `sessions.delete` | Delete a session |

The `chat.send` method automatically creates a session if the provided `sessionKey` doesn't exist yet.

## HTTP Access

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/sessions` | List all sessions |
| `GET` | `/api/sessions/{id}` | Get a specific session |

## Limitations

- **No persistence** — Sessions exist only in memory. Gateway restart clears all sessions.
- **No session limits** — No built-in cap on number of sessions or history length.
- **No cross-gateway sync** — Each gateway instance has its own session store.
