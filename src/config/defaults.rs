/// Default configuration constants used across the system.

/// Default gateway port.
pub const DEFAULT_GATEWAY_PORT: u16 = 18789;

/// Default bind mode.
pub const DEFAULT_BIND_MODE: &str = "loopback";

/// Default WebSocket max payload size (25 MB).
pub const DEFAULT_WS_MAX_PAYLOAD: usize = 25 * 1024 * 1024;

/// Default embedding model.
pub const DEFAULT_EMBEDDING_MODEL: &str = "text-embedding-3-small";

/// Default embedding chunk size in tokens.
pub const DEFAULT_CHUNK_TOKENS: u32 = 256;

/// Default embedding chunk overlap.
pub const DEFAULT_CHUNK_OVERLAP: u32 = 32;

/// Default embedding batch max tokens.
pub const EMBEDDING_BATCH_MAX_TOKENS: u32 = 8000;

/// Default embedding index concurrency.
pub const EMBEDDING_INDEX_CONCURRENCY: u32 = 4;

/// Default embedding retry max attempts.
pub const EMBEDDING_RETRY_MAX_ATTEMPTS: u32 = 3;

/// Default remote embedding batch timeout (2 minutes).
pub const EMBEDDING_BATCH_TIMEOUT_REMOTE_MS: u64 = 2 * 60_000;

/// Default local embedding batch timeout (10 minutes).
pub const EMBEDDING_BATCH_TIMEOUT_LOCAL_MS: u64 = 10 * 60_000;

/// Default reconnect backoff initial delay.
pub const DEFAULT_RECONNECT_INITIAL_MS: u64 = 1000;

/// Default reconnect backoff max delay.
pub const DEFAULT_RECONNECT_MAX_MS: u64 = 30_000;

/// Default reconnect backoff factor.
pub const DEFAULT_RECONNECT_FACTOR: f64 = 2.0;

/// Default tick watch interval.
pub const DEFAULT_TICK_WATCH_MS: u64 = 30_000;

/// Protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

/// Max text chunk limits per channel.
pub const TELEGRAM_TEXT_CHUNK_LIMIT: u32 = 4000;
pub const TELEGRAM_MAX_TEXT: u32 = 4096;
pub const DISCORD_TEXT_CHUNK_LIMIT: u32 = 2000;
pub const SLACK_TEXT_CHUNK_LIMIT: u32 = 4000;
pub const WHATSAPP_TEXT_CHUNK_LIMIT: u32 = 4000;

/// Default model.
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
