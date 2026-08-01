//! iMessage channel (port of OpenClaw `extensions/imessage/src/*` at
//! v2026.7.1, `imsg` backend).
//!
//! Upstream removed the standalone BlueBubbles channel in favor of
//! `channels.imessage` with the `imsg` CLI backend; the legacy BlueBubbles
//! REST bridge remains selectable via `provider: "bluebubbles"`
//! (see `bluebubbles.rs`). The pure behavior — `imsg rpc` command
//! construction (`send-rich --file`, tapbacks, edits, polls), recovery-cursor
//! offline replay, inbound dedupe, echo suppression, poll rendering,
//! per-group system prompts, HEIC staging decisions and reaction gating — is
//! implemented and unit-tested here; the imsg process wiring plugs into
//! `IMessageChannel::start_account`.

use crate::config::types::{IMessageConfig, IMessageGroupConfig};
use crate::config::Config;
use crate::gateway::GatewayState;

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use anyhow::Result;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::info;

// ============================================================================
// Provider selection + BlueBubbles config migration
// (upstream: channels.imessage migration, extensions/imessage doctor rules)
// ============================================================================

/// iMessage backend provider. Upstream v2026.7.1 removed the BlueBubbles
/// channel; `imsg` is the default backend, `bluebubbles` is legacy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IMessageProvider {
    #[default]
    Imsg,
    BlueBubbles,
}

/// Resolve the configured provider. Unknown values fall back to `imsg`.
pub fn resolve_imessage_provider(provider: Option<&str>) -> IMessageProvider {
    match provider.map(str::trim).map(str::to_lowercase).as_deref() {
        Some("bluebubbles") => IMessageProvider::BlueBubbles,
        _ => IMessageProvider::Imsg,
    }
}

/// Config-migration helper: map a legacy `channels.extensions["bluebubbles"]`
/// JSON blob onto [`IMessageConfig`] with `provider: "bluebubbles"`. Doctor
/// wiring (CLI) calls this; it is a pure mapping.
pub fn migrate_bluebubbles_extension_config(value: &serde_json::Value) -> IMessageConfig {
    let get_str = |keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| {
            value.get(k).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(String::from)
        })
    };
    let get_str_list = |key: &str| -> Option<Vec<String>> {
        value.get(key).and_then(|v| v.as_array()).map(|items| {
            items.iter().filter_map(|i| i.as_str().map(String::from)).collect()
        })
    };
    IMessageConfig {
        enabled: value.get("enabled").and_then(|v| v.as_bool()),
        provider: Some("bluebubbles".to_string()),
        api_url: get_str(&["apiUrl", "serverUrl", "url"]),
        api_password: get_str(&["password", "apiPassword"]),
        allow_from: get_str_list("allowFrom"),
        group_allow_from: get_str_list("groupAllowFrom"),
        media_max_mb: value.get("mediaMaxMb").and_then(|v| v.as_f64()),
        reaction_notifications: get_str(&["reactionNotifications"]),
        ..Default::default()
    }
}

// ============================================================================
// imsg CLI command construction
// (upstream: extensions/imessage/src/send.ts, actions.runtime.ts)
// ============================================================================

fn s(v: &str) -> String {
    v.to_string()
}

/// Port of `buildIMessageCliJsonArgs`: append `--db <path>` when configured
/// and always `--json`.
pub fn build_imsg_cli_json_args(args: &[String], db_path: Option<&str>) -> Vec<String> {
    let mut out = args.to_vec();
    if let Some(db) = db_path.map(str::trim).filter(|d| !d.is_empty()) {
        out.push(s("--db"));
        out.push(s(db));
    }
    out.push(s("--json"));
    out
}

/// Parameters for `imsg send-rich` (upstream `sendRichMessage`).
#[derive(Debug, Clone, Default)]
pub struct SendRichParams<'a> {
    pub chat_guid: &'a str,
    pub text: &'a str,
    pub part_index: Option<u32>,
    pub effect_id: Option<&'a str>,
    pub reply_to_message_id: Option<&'a str>,
    /// Pre-extracted markdown format runs, JSON-encoded (upstream
    /// `extractMarkdownFormatRuns` -> `--format`).
    pub format_ranges_json: Option<&'a str>,
    /// Staged attachment temp file (`send-rich --file`, openclaw/imsg#114).
    /// Callers must stage buffers through the outbound media resolver first
    /// — never pass an attacker-controlled raw path.
    pub file_path: Option<&'a str>,
}

/// Port of the `send-rich` arg builder: replies, effects, typed format runs
/// and an optional `--file` attachment in one rich send.
pub fn build_send_rich_args(params: &SendRichParams<'_>) -> Vec<String> {
    let mut args = vec![
        s("send-rich"),
        s("--chat"),
        s(params.chat_guid),
        s("--text"),
        s(params.text),
        s("--part"),
        params.part_index.unwrap_or(0).to_string(),
    ];
    if let Some(effect) = params.effect_id {
        args.push(s("--effect"));
        args.push(s(effect));
    }
    if let Some(reply_to) = params.reply_to_message_id {
        args.push(s("--reply-to"));
        args.push(s(reply_to));
    }
    if let Some(format) = params.format_ranges_json {
        args.push(s("--format"));
        args.push(s(format));
    }
    if let Some(file) = params.file_path {
        args.push(s("--file"));
        args.push(s(file));
    }
    args
}

/// Port of `sendReaction`: `imsg tapback` add/remove.
pub fn build_tapback_args(
    chat_guid: &str,
    message_id: &str,
    kind: &str,
    part_index: Option<u32>,
    remove: bool,
) -> Vec<String> {
    let mut args = vec![
        s("tapback"),
        s("--chat"),
        s(chat_guid),
        s("--message"),
        s(message_id),
        s("--kind"),
        s(kind),
        s("--part"),
        part_index.unwrap_or(0).to_string(),
    ];
    if remove {
        args.push(s("--remove"));
    }
    args
}

/// Port of `editMessage`: `--bc-text` defaults to the new text.
pub fn build_edit_args(
    chat_guid: &str,
    message_id: &str,
    new_text: &str,
    backwards_compat_text: Option<&str>,
    part_index: Option<u32>,
) -> Vec<String> {
    vec![
        s("edit"),
        s("--chat"),
        s(chat_guid),
        s("--message"),
        s(message_id),
        s("--new-text"),
        s(new_text),
        s("--bc-text"),
        s(backwards_compat_text.unwrap_or(new_text)),
        s("--part"),
        part_index.unwrap_or(0).to_string(),
    ]
}

/// Port of `unsendMessage`.
pub fn build_unsend_args(chat_guid: &str, message_id: &str, part_index: Option<u32>) -> Vec<String> {
    vec![
        s("unsend"),
        s("--chat"),
        s(chat_guid),
        s("--message"),
        s(message_id),
        s("--part"),
        part_index.unwrap_or(0).to_string(),
    ]
}

/// Port of the `send-attachment` arg builder (`trySendAttachmentForTarget`).
pub fn build_send_attachment_args(
    chat_target: &str,
    file_path: &str,
    audio_as_voice: bool,
    reply_to_id: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        s("send-attachment"),
        s("--chat"),
        s(chat_target),
        s("--file"),
        s(file_path),
    ];
    if audio_as_voice {
        args.push(s("--audio"));
    }
    if let Some(reply_to) = reply_to_id {
        args.push(s("--reply-to"));
        args.push(s(reply_to));
    }
    args.push(s("--transport"));
    args.push(s("auto"));
    args
}

/// Port of `sendPoll`: `poll send` with repeated `--option` choices.
pub fn build_poll_send_args(
    chat_guid: &str,
    question: &str,
    choices: &[String],
    reply_to_message_id: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        s("poll"),
        s("send"),
        s("--chat"),
        s(chat_guid),
        s("--question"),
        s(question),
    ];
    for choice in choices {
        args.push(s("--option"));
        args.push(choice.clone());
    }
    if let Some(reply_to) = reply_to_message_id {
        args.push(s("--reply-to"));
        args.push(s(reply_to));
    }
    args
}

/// Exactly one vote selector; the CLI resolves index/text to the option UUID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollVoteSelector {
    OptionId(String),
    OptionIndex(u32),
    OptionText(String),
}

/// Port of `sendPollVote`.
pub fn build_poll_vote_args(
    chat_guid: &str,
    poll_guid: &str,
    selector: &PollVoteSelector,
) -> Vec<String> {
    let mut args = vec![
        s("poll"),
        s("vote"),
        s("--chat"),
        s(chat_guid),
        s("--poll"),
        s(poll_guid),
    ];
    match selector {
        PollVoteSelector::OptionId(id) => {
            args.push(s("--option-id"));
            args.push(id.clone());
        }
        PollVoteSelector::OptionIndex(index) => {
            args.push(s("--option-index"));
            args.push(index.to_string());
        }
        PollVoteSelector::OptionText(text) => {
            args.push(s("--option"));
            args.push(text.clone());
        }
    }
    args
}

/// Tapback kinds the `imsg tapback --kind` flag accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapbackKind {
    Love,
    Like,
    Dislike,
    Laugh,
    Emphasize,
    Question,
}

impl TapbackKind {
    pub const ALL: [TapbackKind; 6] = [
        TapbackKind::Love,
        TapbackKind::Like,
        TapbackKind::Dislike,
        TapbackKind::Laugh,
        TapbackKind::Emphasize,
        TapbackKind::Question,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            TapbackKind::Love => "love",
            TapbackKind::Like => "like",
            TapbackKind::Dislike => "dislike",
            TapbackKind::Laugh => "laugh",
            TapbackKind::Emphasize => "emphasize",
            TapbackKind::Question => "question",
        }
    }
}

/// Port of `mapTapbackReaction`: emoji/word -> tapback kind (variation
/// selectors stripped, case-insensitive).
pub fn map_tapback_reaction(emoji: Option<&str>) -> Option<TapbackKind> {
    let value: String = emoji?
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| *c != '\u{fe0f}')
        .collect();
    if value.is_empty() {
        return None;
    }
    let matches = |candidates: &[&str]| candidates.contains(&value.as_str());
    if matches(&["love", "heart", "❤"]) {
        Some(TapbackKind::Love)
    } else if matches(&["like", "+1", "thumbsup", "👍"]) {
        Some(TapbackKind::Like)
    } else if matches(&["dislike", "-1", "thumbsdown", "👎"]) {
        Some(TapbackKind::Dislike)
    } else if matches(&["laugh", "haha", "😂", "🤣"]) {
        Some(TapbackKind::Laugh)
    } else if matches(&["emphasize", "!!", "‼"]) {
        Some(TapbackKind::Emphasize)
    } else if matches(&["question", "?", "？", "❓"]) {
        Some(TapbackKind::Question)
    } else {
        None
    }
}

/// Port of `resolveIMessageCliFailure`: `success: false` responses carry the
/// error string.
pub fn resolve_imsg_cli_failure(result: &serde_json::Value) -> Option<String> {
    if result.get("success").and_then(|v| v.as_bool()) != Some(false) {
        return None;
    }
    let error = result
        .get("error")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|e| !e.is_empty());
    Some(error.unwrap_or("iMessage action failed").to_string())
}

/// Port of `isIMessageRpcSendTimeout`.
pub fn is_imsg_rpc_send_timeout(message: &str) -> bool {
    static RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)imsg rpc timeout \(send\)").unwrap());
    RE.is_match(message)
}

/// Port of `isAttachmentCommandFallbackError`: an imsg build without the
/// attachment/rich commands falls back to the plain send path.
pub fn is_attachment_command_fallback_error(message: &str) -> bool {
    static RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"(?iu)(?:unknown|unrecognized|invalid|unsupported)\s+(?:command|subcommand)|not a recognized command|send-attachment.*(?:not found|unsupported|unavailable)|private api bridge.*unavailable|requires the imsg private api bridge|run imsg launch",
        )
        .unwrap()
    });
    RE.is_match(message)
}

/// Port of `isThreadedReplyUnsupportedError`: threaded replies need the
/// private-API bridge; on AppleScript-only deployments resend unthreaded
/// instead of dropping the message.
pub fn is_threaded_reply_unsupported_error(message: &str) -> bool {
    static RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"(?iu)reply_to requires bridge transport|cannot send threaded repl|threaded repl(?:y|ies)\b.*(?:unsupported|not supported|requires|unavailable)|requires bridge transport",
        )
        .unwrap()
    });
    RE.is_match(message)
}

// ============================================================================
// Recovery cursor: offline replay with persisted cursor
// (upstream: extensions/imessage/src/monitor/recovery-cursor.ts,
//  monitor/inbound-dedupe.ts constants)
// ============================================================================

/// Deliver a replayed (catchup) message up to this old; suppress older.
pub const IMESSAGE_RECOVERY_MAX_AGE_MS: i64 = 2 * 60 * 60 * 1000;
/// Never set `since_rowid` more than this many rows below the current max.
pub const IMESSAGE_RECOVERY_MAX_ROWS: i64 = 500;
/// Drop a LIVE inbound row whose send date is older than this (stale backlog
/// flushed after a Push recovery).
pub const IMESSAGE_STALE_INBOUND_THRESHOLD_MS: i64 = 15 * 60 * 1000;
/// Inbound dedupe recency window.
pub const IMESSAGE_INBOUND_DEDUPE_TTL_MS: i64 = 4 * 60 * 60 * 1000;

/// Stable identity for the watched Messages database (port of
/// `resolveIMessageRecoveryCursorDbIdentity`). A cursor must never carry
/// across databases: rowids of different chat.db files share no ordering.
pub fn resolve_imessage_recovery_cursor_db_identity(
    cli_path: Option<&str>,
    db_path: Option<&str>,
    remote_host: Option<&str>,
    home_dir: Option<&str>,
) -> String {
    if let Some(host) = remote_host.map(str::trim).filter(|h| !h.is_empty()) {
        let db = db_path.map(str::trim).filter(|d| !d.is_empty()).unwrap_or("default");
        return format!("remote:{host}:{db}");
    }
    let normalize_local = |raw: &str| -> String {
        let trimmed = raw.trim();
        let expanded: PathBuf = if let Some(rest) = trimmed.strip_prefix('~') {
            match home_dir {
                Some(home) => Path::new(home).join(rest.trim_start_matches('/')),
                None => PathBuf::from(trimmed),
            }
        } else {
            PathBuf::from(trimmed)
        };
        // Lexical resolution (no filesystem access) keeps this pure/testable.
        expanded.to_string_lossy().to_string()
    };
    if let Some(db) = db_path.map(str::trim).filter(|d| !d.is_empty()) {
        return format!("local:{}", normalize_local(db));
    }
    let cli = cli_path.map(str::trim).filter(|c| !c.is_empty());
    let is_default_cli = match cli {
        None => true,
        Some(c) => c == "imsg" || Path::new(c).file_name().map(|f| f == "imsg").unwrap_or(false),
    };
    if is_default_cli {
        return match home_dir {
            Some(home) => format!(
                "local:{}",
                Path::new(home).join("Library/Messages/chat.db").to_string_lossy()
            ),
            None => "local:default".to_string(),
        };
    }
    format!("local:cli:{}", cli.unwrap_or_default())
}

/// Composite key: one high-water per (account, database). NUL separator
/// cannot appear in either part.
fn recovery_cursor_store_key(account_id: &str, db_identity: &str) -> String {
    format!("{account_id}\u{0}{db_identity}")
}

/// SQLite-backed monitor state store (upstream moved monitor state to
/// SQLite). Holds the recovery cursor and durable echo markers.
pub struct IMessageStateStore {
    conn: Mutex<rusqlite::Connection>,
}

impl IMessageStateStore {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(rusqlite::Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(rusqlite::Connection::open_in_memory()?)
    }

    fn init(conn: rusqlite::Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS imessage_recovery_cursor (
                 key TEXT PRIMARY KEY,
                 last_rowid INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS imessage_echo_markers (
                 marker_key TEXT PRIMARY KEY,
                 scope TEXT NOT NULL,
                 text_key TEXT,
                 message_id TEXT,
                 pending INTEGER NOT NULL DEFAULT 0,
                 expires_at_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_imessage_echo_scope
                 ON imessage_echo_markers(scope);",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Last dispatched rowid for (account, database), or `None`. A cursor
    /// stored for a different database identity is never returned.
    pub fn load_recovery_cursor(&self, account_id: &str, db_identity: &str) -> Option<i64> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT last_rowid FROM imessage_recovery_cursor WHERE key = ?1",
            [recovery_cursor_store_key(account_id, db_identity)],
            |row| row.get(0),
        )
        .ok()
    }

    /// Advance the cursor forward (monotonic per database; never rewinds).
    pub fn advance_recovery_cursor(&self, account_id: &str, db_identity: &str, rowid: i64) {
        let conn = self.conn.lock();
        // Best effort: a failed write just means a little more replay next
        // startup, which the dedupe absorbs.
        let _ = conn.execute(
            "INSERT INTO imessage_recovery_cursor(key, last_rowid) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET last_rowid = excluded.last_rowid
             WHERE excluded.last_rowid > imessage_recovery_cursor.last_rowid",
            rusqlite::params![recovery_cursor_store_key(account_id, db_identity), rowid],
        );
    }

    /// Remember a durable (crash-surviving) echo marker. Returns the marker
    /// key so a failed send can forget it.
    pub fn remember_echo_marker(
        &self,
        scope: &str,
        text: Option<&str>,
        message_id: Option<&str>,
        pending: bool,
        ttl_ms: i64,
        now_ms: i64,
    ) -> Option<String> {
        let text_key = normalize_echo_text_key(text);
        let id_key = normalize_echo_message_id_key(message_id);
        if text_key.is_none() && id_key.is_none() {
            return None;
        }
        let marker_key = format!(
            "{scope}\u{0}{}\u{0}{}",
            text_key.as_deref().unwrap_or(""),
            id_key.as_deref().unwrap_or("")
        );
        let conn = self.conn.lock();
        let _ = conn.execute(
            "INSERT OR REPLACE INTO imessage_echo_markers
                 (marker_key, scope, text_key, message_id, pending, expires_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                marker_key,
                scope,
                text_key,
                id_key,
                pending as i64,
                now_ms + ttl_ms.max(0)
            ],
        );
        Some(marker_key)
    }

    /// Forget a pending marker after a failed send (port of
    /// `forgetPersistedIMessageEchoKey`).
    pub fn forget_echo_marker(&self, marker_key: &str) {
        let conn = self.conn.lock();
        let _ = conn.execute(
            "DELETE FROM imessage_echo_markers WHERE marker_key = ?1",
            [marker_key],
        );
    }

    /// Port of `hasPersistedIMessageEcho`: match by message id or normalized
    /// text within TTL; pending markers only when requested.
    pub fn has_echo_marker(
        &self,
        scope: &str,
        text: Option<&str>,
        message_id: Option<&str>,
        include_pending: bool,
        now_ms: i64,
    ) -> bool {
        let text_key = normalize_echo_text_key(text);
        let id_key = normalize_echo_message_id_key(message_id);
        let conn = self.conn.lock();
        let _ = conn.execute(
            "DELETE FROM imessage_echo_markers WHERE expires_at_ms < ?1",
            [now_ms],
        );
        let mut stmt = match conn.prepare(
            "SELECT 1 FROM imessage_echo_markers
             WHERE scope = ?1 AND expires_at_ms >= ?2
               AND (pending = 0 OR ?3 = 1)
               AND ((?4 IS NOT NULL AND message_id = ?4)
                 OR (?5 IS NOT NULL AND text_key = ?5))
             LIMIT 1",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return false,
        };
        stmt.exists(rusqlite::params![scope, now_ms, include_pending as i64, id_key, text_key])
            .unwrap_or(false)
    }
}

/// Clamp the replay start so a months-down gateway does not stream its whole
/// history: `since_rowid >= current_max - IMESSAGE_RECOVERY_MAX_ROWS`.
pub fn clamp_recovery_since_rowid(cursor: Option<i64>, current_max_rowid: i64) -> Option<i64> {
    let floor = (current_max_rowid - IMESSAGE_RECOVERY_MAX_ROWS).max(0);
    match cursor {
        Some(rowid) => Some(rowid.max(floor)),
        None => None,
    }
}

// ============================================================================
// Inbound dedupe + age fence
// (upstream: extensions/imessage/src/monitor/inbound-dedupe.ts)
// ============================================================================

/// Fields identifying an inbound message for replay protection.
#[derive(Debug, Clone, Default)]
pub struct IMessageInboundIdentity<'a> {
    pub guid: Option<&'a str>,
    pub sender: Option<&'a str>,
    pub chat_id: Option<i64>,
    pub chat_guid: Option<&'a str>,
    pub chat_identifier: Option<&'a str>,
    pub created_at: Option<&'a str>,
    pub text: Option<&'a str>,
}

/// Port of `buildIMessageInboundReplayKey`: prefer the Apple GUID; fall back
/// to a bounded composite hash; `None` fails open (never suppress an
/// unidentifiable message).
pub fn build_imessage_inbound_replay_key(
    account_id: &str,
    message: &IMessageInboundIdentity<'_>,
) -> Option<String> {
    if let Some(guid) = message.guid.map(str::trim).filter(|g| !g.is_empty()) {
        return Some(format!("{account_id}:guid:{guid}"));
    }
    let sender = message.sender.map(str::trim).filter(|v| !v.is_empty())?;
    let conversation = match message.chat_id {
        Some(chat_id) => format!("chat:{chat_id}"),
        None => message
            .chat_guid
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .or_else(|| message.chat_identifier.map(str::trim).filter(|v| !v.is_empty()))?
            .to_string(),
    };
    let created_at = message.created_at.map(str::trim).filter(|v| !v.is_empty())?;
    let text = message.text.unwrap_or("").trim();
    let mut hasher = Sha256::new();
    hasher.update(format!("{conversation}\u{0}{sender}\u{0}{created_at}\u{0}{text}"));
    let digest = hex::encode_upper(hasher.finalize());
    // Match upstream's lowercase hex, 32 chars.
    Some(format!("{account_id}:c:{}", digest[..32].to_lowercase()))
}

mod hex {
    pub fn encode_upper(bytes: impl AsRef<[u8]>) -> String {
        bytes.as_ref().iter().map(|b| format!("{b:02X}")).collect()
    }
}

fn parse_created_at_ms(created_at: &str) -> Option<i64> {
    let trimmed = created_at.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Some(dt.timestamp_millis());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S") {
        return Some(dt.and_utc().timestamp_millis());
    }
    None
}

/// Port of `isStaleIMessageBacklog`: true when the message's own send date is
/// materially older than now. Fails open on missing/unparseable dates.
pub fn is_stale_imessage_backlog(
    created_at: Option<&str>,
    now_ms: i64,
    threshold_ms: i64,
) -> bool {
    let Some(created_at) = created_at else { return false };
    let Some(sent_ms) = parse_created_at_ms(created_at) else {
        return false;
    };
    now_ms - sent_ms > threshold_ms
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimPhase {
    InFlight,
    Committed,
}

/// Claimable inbound replay guard (port of the persistent-dedupe usage):
/// claims are atomic — a duplicate emitted while the first copy is in flight
/// reports as duplicate instead of racing through; release on dispatch
/// failure lets transient failures retry.
#[derive(Default)]
pub struct InboundReplayGuard {
    entries: Mutex<HashMap<String, (ClaimPhase, i64)>>,
}

impl InboundReplayGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attempt to claim `key`. `false` = duplicate/inflight -> drop.
    pub fn claim(&self, key: &str, now_ms: i64) -> bool {
        let mut entries = self.entries.lock();
        entries.retain(|_, (_, at)| now_ms - *at <= IMESSAGE_INBOUND_DEDUPE_TTL_MS);
        match entries.get(key) {
            Some(_) => false,
            None => {
                entries.insert(key.to_string(), (ClaimPhase::InFlight, now_ms));
                true
            }
        }
    }

    /// Commit after successful dispatch: the key stays blocked for the TTL.
    pub fn commit(&self, key: &str, now_ms: i64) {
        self.entries
            .lock()
            .insert(key.to_string(), (ClaimPhase::Committed, now_ms));
    }

    /// Release after failed dispatch: the key may be retried.
    pub fn release(&self, key: &str) {
        let mut entries = self.entries.lock();
        if let Some((ClaimPhase::InFlight, _)) = entries.get(key) {
            entries.remove(key);
        }
    }
}

// ============================================================================
// Echo suppression (upstream: extensions/imessage/src/monitor/echo-cache.ts)
// ============================================================================

/// Echo arrival observed at ~2.2s; 4s gives ~80% margin. Degrades to
/// duplicate delivery on expiry — never message loss.
pub const SENT_MESSAGE_TEXT_TTL_MS: i64 = 4_000;
pub const SENT_MESSAGE_ID_TTL_MS: i64 = 60_000;

fn normalize_echo_text_key(text: Option<&str>) -> Option<String> {
    let text = text?;
    let unified = text.replace("\r\n", "\n").replace('\r', "\n");
    // Port of stripLeadingEchoTextCorruptionMarkers: drop leading object
    // replacement / replacement chars Messages prepends to corrupted echoes.
    let stripped = unified
        .trim()
        .trim_start_matches(['\u{fffc}', '\u{fffd}'])
        .trim();
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_string())
    }
}

fn normalize_echo_message_id_key(message_id: Option<&str>) -> Option<String> {
    let normalized = message_id?.trim();
    if normalized.is_empty() || normalized == "ok" || normalized == "unknown" {
        return None;
    }
    Some(normalized.to_string())
}

/// In-memory sent-message cache (port of `DefaultSentMessageCache`).
/// Time is injected for testability.
#[derive(Default)]
pub struct SentMessageCache {
    text_cache: Mutex<HashMap<String, i64>>,
    text_backed_by_id: Mutex<HashMap<String, i64>>,
    message_id_cache: Mutex<HashMap<String, i64>>,
}

impl SentMessageCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn remember(&self, scope: &str, text: Option<&str>, message_id: Option<&str>, now_ms: i64) {
        let text_key = normalize_echo_text_key(text);
        if let Some(key) = &text_key {
            self.text_cache.lock().insert(format!("{scope}:{key}"), now_ms);
        }
        if let Some(id_key) = normalize_echo_message_id_key(message_id) {
            self.message_id_cache.lock().insert(format!("{scope}:{id_key}"), now_ms);
            if let Some(key) = &text_key {
                self.text_backed_by_id.lock().insert(format!("{scope}:{key}"), now_ms);
            }
        }
        self.cleanup(now_ms);
    }

    /// `skip_id_short_circuit`: for self-chat `is_from_me=true` rows the
    /// inbound ID is a numeric rowid that never matches outbound GUIDs, so
    /// fall through to text matching.
    pub fn has(
        &self,
        scope: &str,
        text: Option<&str>,
        message_id: Option<&str>,
        skip_id_short_circuit: bool,
        now_ms: i64,
    ) -> bool {
        self.cleanup(now_ms);
        let text_key = normalize_echo_text_key(text);
        let id_key = normalize_echo_message_id_key(message_id);
        if let Some(id_key) = &id_key {
            if let Some(at) = self.message_id_cache.lock().get(&format!("{scope}:{id_key}")) {
                if now_ms - at <= SENT_MESSAGE_ID_TTL_MS {
                    return true;
                }
            }
            // A text remembered WITHOUT an id (more recently than any
            // id-backed copy) may still match; otherwise an id mismatch is
            // final unless the caller opted out of the short circuit.
            let text_ts = text_key
                .as_ref()
                .and_then(|k| self.text_cache.lock().get(&format!("{scope}:{k}")).copied());
            let backed_ts = text_key
                .as_ref()
                .and_then(|k| self.text_backed_by_id.lock().get(&format!("{scope}:{k}")).copied());
            let has_text_only_match = match (text_ts, backed_ts) {
                (Some(t), Some(b)) => t > b,
                (Some(_), None) => true,
                _ => false,
            };
            if !skip_id_short_circuit && !has_text_only_match {
                return false;
            }
        }
        if let Some(key) = &text_key {
            if let Some(at) = self.text_cache.lock().get(&format!("{scope}:{key}")) {
                if now_ms - at <= SENT_MESSAGE_TEXT_TTL_MS {
                    return true;
                }
            }
        }
        false
    }

    fn cleanup(&self, now_ms: i64) {
        self.text_cache.lock().retain(|_, at| now_ms - *at <= SENT_MESSAGE_TEXT_TTL_MS);
        self.text_backed_by_id.lock().retain(|_, at| now_ms - *at <= SENT_MESSAGE_TEXT_TTL_MS);
        self.message_id_cache.lock().retain(|_, at| now_ms - *at <= SENT_MESSAGE_ID_TTL_MS);
    }
}

// ============================================================================
// Self-chat cache with timestamp-skew tolerance
// (upstream: extensions/imessage/src/monitor/self-chat-cache.ts)
// ============================================================================

pub const SELF_CHAT_TTL_MS: i64 = 10_000;
pub const SELF_CHAT_CREATED_AT_TOLERANCE_MS: i64 = 1_000;
const MAX_SELF_CHAT_CACHE_ENTRIES: usize = 512;

/// Lookup/remember parameters for the self-chat suppression cache.
#[derive(Debug, Clone)]
pub struct SelfChatLookup<'a> {
    pub account_id: &'a str,
    pub sender: &'a str,
    pub is_group: bool,
    pub chat_id: Option<i64>,
    pub text: Option<&'a str>,
    pub created_at_ms: Option<i64>,
    /// Remote-bridge deployments see small created_at skew between the
    /// outbound write and the inbound echo row; tolerate up to 1s when set.
    pub allow_created_at_skew: bool,
}

#[derive(Debug, Clone)]
struct SelfChatEntry {
    created_at: i64,
    skew_tolerance_ms: i64,
    remembered_at: i64,
}

/// Port of `DefaultSelfChatCache` (bounded, TTL'd, exact-or-skew matching).
#[derive(Default)]
pub struct SelfChatCache {
    entries: Mutex<Vec<(String, SelfChatEntry)>>,
}

impl SelfChatCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn bucket_key(lookup: &SelfChatLookup<'_>) -> Option<String> {
        let text = normalize_echo_text_key(lookup.text)?;
        let mut hasher = Sha256::new();
        hasher.update(&text);
        let digest = hex::encode_upper(hasher.finalize()).to_lowercase();
        let scope = if lookup.is_group {
            let chat = lookup
                .chat_id
                .map(|id| format!("chat_id:{id}"))
                .unwrap_or_else(|| "chat_id:unknown".to_string());
            format!("{}:{}:imessage:{}", lookup.account_id, chat, lookup.sender)
        } else {
            format!("{}:imessage:{}", lookup.account_id, lookup.sender)
        };
        Some(format!("{scope}:{digest}"))
    }

    pub fn remember(&self, lookup: &SelfChatLookup<'_>, now_ms: i64) {
        let (Some(key), Some(created_at)) = (Self::bucket_key(lookup), lookup.created_at_ms)
        else {
            return;
        };
        let mut entries = self.entries.lock();
        entries.retain(|(_, e)| now_ms - e.remembered_at <= SELF_CHAT_TTL_MS);
        entries.push((
            key,
            SelfChatEntry {
                created_at,
                skew_tolerance_ms: if lookup.allow_created_at_skew {
                    SELF_CHAT_CREATED_AT_TOLERANCE_MS
                } else {
                    0
                },
                remembered_at: now_ms,
            },
        ));
        let len = entries.len();
        if len > MAX_SELF_CHAT_CACHE_ENTRIES {
            entries.drain(..len - MAX_SELF_CHAT_CACHE_ENTRIES);
        }
    }

    pub fn has(&self, lookup: &SelfChatLookup<'_>, now_ms: i64) -> bool {
        let (Some(key), Some(created_at)) = (Self::bucket_key(lookup), lookup.created_at_ms)
        else {
            return false;
        };
        let entries = self.entries.lock();
        entries.iter().any(|(k, entry)| {
            if *k != key || now_ms - entry.remembered_at > SELF_CHAT_TTL_MS {
                return false;
            }
            let delta = (entry.created_at - created_at).abs();
            delta == 0 || delta < entry.skew_tolerance_ms
        })
    }
}

// ============================================================================
// Native polls (upstream: extensions/imessage/src/monitor/poll-render.ts,
// poll-comment.ts)
// ============================================================================

/// Decoded native poll payload from imsg.
#[derive(Debug, Clone, Default)]
pub struct IMessagePoll {
    /// `"vote"` for vote updates; anything else renders the poll balloon.
    pub kind: Option<String>,
    pub question: Option<String>,
    pub options: Vec<IMessagePollOption>,
    pub votes: Vec<IMessagePollVote>,
    pub vote: Option<IMessagePollVote>,
}

#[derive(Debug, Clone, Default)]
pub struct IMessagePollOption {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct IMessagePollVote {
    pub participant: Option<String>,
    pub option_id: Option<String>,
    pub option_text: Option<String>,
    /// `"removed"` when a vote was retracted.
    pub event_type: Option<String>,
}

/// Port of `renderIMessagePollBody`: renders the raw 0xFFFD poll balloon into
/// readable text with numbered options and an explicit poll-vote call to
/// action (echo of the spoken answer is suppressed separately).
pub fn render_imessage_poll_body(poll: &IMessagePoll) -> Option<String> {
    if poll.kind.as_deref() == Some("vote") || (poll.vote.is_some() && poll.options.is_empty()) {
        let Some(vote) = &poll.vote else {
            return Some("\u{1F4CA} Poll vote received".to_string());
        };
        let who = vote
            .participant
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or("someone");
        let what = vote
            .option_text
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .or(vote.option_id.as_deref())
            .unwrap_or("an option");
        let verb = if vote.event_type.as_deref() == Some("removed") {
            "removed their vote for"
        } else {
            "voted for"
        };
        return Some(format!("\u{1F4CA} Poll vote: {who} {verb} \"{what}\""));
    }

    if poll.options.is_empty() {
        return None;
    }

    let mut tally: HashMap<&str, u32> = HashMap::new();
    for vote in &poll.votes {
        if vote.event_type.as_deref() == Some("removed") {
            continue;
        }
        if let Some(option_id) = vote.option_id.as_deref().filter(|v| !v.is_empty()) {
            *tally.entry(option_id).or_default() += 1;
        }
    }

    let option_list = poll
        .options
        .iter()
        .enumerate()
        .map(|(index, option)| {
            let count = tally.get(option.id.as_str()).copied().unwrap_or(0);
            let suffix = if count > 0 { format!(" [{count}]") } else { String::new() };
            format!("{}) {}{suffix}", index + 1, option.text)
        })
        .collect::<Vec<_>>()
        .join("  ");
    let question = poll.question.as_deref().map(str::trim).filter(|q| !q.is_empty());
    Some(format!(
        "\u{1F4CA} Poll{} — options: {option_list}. Cast your vote on this poll with the poll-vote action (pollOptionIndex = the option number); do not answer in a text reply.",
        question.map(|q| format!(": {q}")).unwrap_or_default()
    ))
}

/// Default window inside which a reply to a poll balloon from the poll's
/// creator is folded as the poll caption.
pub const POLL_COMMENT_WINDOW_MS: i64 = 15_000;

/// Port of `createPollCommentFolder`: folds a poll's caption (delivered as a
/// separate inline reply to the poll balloon) into the poll message instead
/// of dispatching it standalone. Fails CLOSED on identity: fold only when
/// both the poll creator and the reply sender are known and identical.
pub struct PollCommentFolder {
    window_ms: i64,
    seen_polls: Mutex<HashMap<String, (i64, String)>>,
}

impl Default for PollCommentFolder {
    fn default() -> Self {
        Self::new(POLL_COMMENT_WINDOW_MS)
    }
}

impl PollCommentFolder {
    pub fn new(window_ms: i64) -> Self {
        Self { window_ms, seen_polls: Mutex::new(HashMap::new()) }
    }

    fn normalize_sender(sender: Option<&str>) -> String {
        sender.map(str::trim).unwrap_or("").to_lowercase()
    }

    pub fn remember_poll(&self, guid: Option<&str>, at_ms: i64, sender: Option<&str>) {
        let Some(key) = guid.map(str::trim).filter(|g| !g.is_empty()) else {
            return;
        };
        let mut seen = self.seen_polls.lock();
        let window = self.window_ms;
        seen.retain(|_, (at, _)| at_ms - *at <= window);
        seen.insert(key.to_string(), (at_ms, Self::normalize_sender(sender)));
    }

    pub fn is_poll_comment(
        &self,
        reply_to_guid: Option<&str>,
        at_ms: i64,
        sender: Option<&str>,
    ) -> bool {
        let Some(key) = reply_to_guid.map(str::trim).filter(|g| !g.is_empty()) else {
            return false;
        };
        let seen = self.seen_polls.lock();
        let Some((poll_at, poll_sender)) = seen.get(key) else {
            return false;
        };
        if at_ms < *poll_at || at_ms - poll_at > self.window_ms {
            return false;
        }
        let reply_sender = Self::normalize_sender(sender);
        !poll_sender.is_empty() && !reply_sender.is_empty() && *poll_sender == reply_sender
    }
}

// ============================================================================
// Per-group systemPrompt + wildcard
// (upstream: extensions/imessage/src/monitor/inbound-processing.ts)
// ============================================================================

/// Look up the per-group config for a chat id plus the `*` wildcard entry.
pub fn lookup_imessage_group_config<'a>(
    groups: Option<&'a HashMap<String, IMessageGroupConfig>>,
    chat_id: &str,
) -> (Option<&'a IMessageGroupConfig>, Option<&'a IMessageGroupConfig>) {
    match groups {
        Some(groups) => (groups.get(chat_id), groups.get("*")),
        None => (None, None),
    }
}

/// Port of `resolveIMessageGroupSystemPrompt`:
/// 1. A matched per-chat entry with a present `systemPrompt` wins; empty
///    after trim suppresses the wildcard ("this group has no prompt").
/// 2. Otherwise the `groups["*"]` wildcard prompt (trimmed, empty -> None).
pub fn resolve_imessage_group_system_prompt(
    specific: Option<&IMessageGroupConfig>,
    wildcard: Option<&IMessageGroupConfig>,
) -> Option<String> {
    if let Some(prompt) = specific.and_then(|g| g.system_prompt.as_deref()) {
        let trimmed = prompt.trim();
        return if trimmed.is_empty() { None } else { Some(trimmed.to_string()) };
    }
    let prompt = wildcard.and_then(|g| g.system_prompt.as_deref())?;
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

// ============================================================================
// HEIC -> JPEG staging decision
// (upstream: extensions/imessage/src/monitor/media-staging.ts)
// ============================================================================

/// Port of `isHeicAttachment`: MIME wins, extension fallback.
pub fn is_heic_attachment(attachment_path: &str, mime_type: Option<&str>) -> bool {
    if let Some(mime) = mime_type.map(str::to_lowercase) {
        if mime == "image/heic" || mime == "image/heif" {
            return true;
        }
    }
    let ext = Path::new(attachment_path)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase());
    matches!(ext.as_deref(), Some("heic") | Some("heif"))
}

/// Port of `jpegFilenameForAttachment`: converted HEIC stages as `<stem>.jpg`.
pub fn jpeg_filename_for_attachment(attachment_path: &str) -> String {
    let stem = Path::new(attachment_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "imessage-attachment".to_string());
    format!("{stem}.jpg")
}

// ============================================================================
// Inbound tapbacks (upstream: extensions/imessage/src/monitor/
// inbound-processing.ts reaction gating, reaction-system-event.ts)
// ============================================================================

/// `reactionNotifications` mode (default `own`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IMessageReactionNotificationMode {
    Off,
    Own,
    All,
}

pub fn resolve_imessage_reaction_notification_mode(
    value: Option<&str>,
) -> IMessageReactionNotificationMode {
    match value.map(str::trim).map(str::to_lowercase).as_deref() {
        Some("off") => IMessageReactionNotificationMode::Off,
        Some("all") => IMessageReactionNotificationMode::All,
        _ => IMessageReactionNotificationMode::Own,
    }
}

/// Gate an inbound tapback: `off` drops all, `own` only reactions targeting
/// the agent's own messages (echo cache / known from-me GUIDs), `all`
/// surfaces every reaction.
pub fn should_emit_imessage_reaction_notification(
    mode: IMessageReactionNotificationMode,
    target_is_own: bool,
) -> bool {
    match mode {
        IMessageReactionNotificationMode::Off => false,
        IMessageReactionNotificationMode::Own => target_is_own,
        IMessageReactionNotificationMode::All => true,
    }
}

/// Dedupe context key for a reaction system event (upstream `reactionKey`).
pub fn build_imessage_reaction_context_key(
    action: &str,
    conversation: &str,
    target: &str,
    sender: &str,
    emoji: &str,
) -> String {
    format!("imessage:reaction:{action}:{conversation}:{target}:{sender}:{emoji}")
}

// ============================================================================
// Channel plugin
// ============================================================================

/// iMessage channel.
///
/// Live integration point: the `imsg` CLI (`imsg rpc` for sends, `imsg watch
/// --subscribe --since-rowid <recovery cursor>` for the inbound monitor).
/// Legacy `provider: "bluebubbles"` routes through `bluebubbles.rs`.
pub struct IMessageChannel {
    enabled: bool,
    provider: IMessageProvider,
}

impl IMessageChannel {
    pub fn new(config: &Config) -> Self {
        let imessage = &config.channels.imessage;
        Self {
            enabled: imessage.enabled.unwrap_or(false),
            provider: resolve_imessage_provider(imessage.provider.as_deref()),
        }
    }

    pub fn provider(&self) -> IMessageProvider {
        self.provider
    }
}

#[async_trait]
impl ChannelPlugin for IMessageChannel {
    fn id(&self) -> &str {
        "imessage"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "iMessage".to_string(),
            description: "Apple iMessage channel (imsg backend; BlueBubbles legacy)".to_string(),
            enabled: self.enabled,
            multi_account: false,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
            ChannelCapability::SendMedia,
            ChannelCapability::ReceiveMedia,
            ChannelCapability::Groups,
            ChannelCapability::Threads,
            ChannelCapability::ReadReceipts,
            ChannelCapability::TypingIndicators,
            ChannelCapability::Reactions,
            ChannelCapability::EditMessage,
            ChannelCapability::DeleteMessage,
            ChannelCapability::Polls,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        info!(provider = ?self.provider, "iMessage channel starting");
        // Integration point: open IMessageStateStore, resolve the db
        // identity, load the recovery cursor, spawn `imsg watch` with
        // since_rowid (clamped via clamp_recovery_since_rowid), and run the
        // inbound pipeline: replay guard claim -> age fence -> echo/self-chat
        // suppression -> poll folding -> reaction gating -> dispatch,
        // advancing the cursor after each dispatched row.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.enabled {
            info!("iMessage channel stopping");
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, _message: &str) -> Result<()> {
        info!(to = to, "iMessage: sending message (imsg rpc wiring pending)");
        // Integration point: build_send_rich_args / build_send_attachment_args
        // -> spawn `imsg <args> --db <path> --json` (build_imsg_cli_json_args)
        // and parse the last stdout JSON line (resolve_imsg_cli_failure).
        Ok(())
    }
}

/// Convenience function called by the top-level `send_message` dispatcher.
pub(crate) async fn send_message(config: &Config, to: &str, message: &str) -> Result<()> {
    let channel = IMessageChannel::new(config);
    channel.send_message(to, message).await
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- provider + migration ----

    #[test]
    fn provider_selection_defaults_to_imsg() {
        assert_eq!(resolve_imessage_provider(None), IMessageProvider::Imsg);
        assert_eq!(resolve_imessage_provider(Some("imsg")), IMessageProvider::Imsg);
        assert_eq!(
            resolve_imessage_provider(Some("BlueBubbles")),
            IMessageProvider::BlueBubbles
        );
        assert_eq!(resolve_imessage_provider(Some("bogus")), IMessageProvider::Imsg);
    }

    #[test]
    fn bluebubbles_config_migration_maps_fields() {
        let value = serde_json::json!({
            "enabled": true,
            "serverUrl": "http://192.168.1.10:1234",
            "password": "secret",
            "allowFrom": ["+15550001111"],
            "mediaMaxMb": 25,
        });
        let migrated = migrate_bluebubbles_extension_config(&value);
        assert_eq!(migrated.enabled, Some(true));
        assert_eq!(migrated.provider.as_deref(), Some("bluebubbles"));
        assert_eq!(migrated.api_url.as_deref(), Some("http://192.168.1.10:1234"));
        assert_eq!(migrated.api_password.as_deref(), Some("secret"));
        assert_eq!(migrated.allow_from, Some(vec!["+15550001111".to_string()]));
        assert_eq!(migrated.media_max_mb, Some(25.0));
        // apiUrl takes precedence over serverUrl.
        let value = serde_json::json!({ "apiUrl": "http://a", "serverUrl": "http://b" });
        assert_eq!(
            migrate_bluebubbles_extension_config(&value).api_url.as_deref(),
            Some("http://a")
        );
    }

    // ---- command builders ----

    #[test]
    fn cli_json_args_append_db_and_json() {
        let args = build_imsg_cli_json_args(&["chats".to_string()], Some("/tmp/chat.db"));
        assert_eq!(args, vec!["chats", "--db", "/tmp/chat.db", "--json"]);
        let args = build_imsg_cli_json_args(&["chats".to_string()], None);
        assert_eq!(args, vec!["chats", "--json"]);
        let args = build_imsg_cli_json_args(&["chats".to_string()], Some("  "));
        assert_eq!(args, vec!["chats", "--json"]);
    }

    #[test]
    fn send_rich_args_full_and_minimal() {
        let params = SendRichParams {
            chat_guid: "iMessage;-;+15550001111",
            text: "hello",
            part_index: Some(2),
            effect_id: Some("invisible-ink"),
            reply_to_message_id: Some("GUID-1"),
            format_ranges_json: Some("[{\"start\":0}]"),
            file_path: Some("/tmp/pic.jpg"),
        };
        assert_eq!(
            build_send_rich_args(&params),
            vec![
                "send-rich", "--chat", "iMessage;-;+15550001111", "--text", "hello",
                "--part", "2", "--effect", "invisible-ink", "--reply-to", "GUID-1",
                "--format", "[{\"start\":0}]", "--file", "/tmp/pic.jpg",
            ]
        );
        let minimal = SendRichParams { chat_guid: "c", text: "t", ..Default::default() };
        assert_eq!(
            build_send_rich_args(&minimal),
            vec!["send-rich", "--chat", "c", "--text", "t", "--part", "0"]
        );
    }

    #[test]
    fn tapback_edit_unsend_args() {
        assert_eq!(
            build_tapback_args("c", "m", "like", None, false),
            vec!["tapback", "--chat", "c", "--message", "m", "--kind", "like", "--part", "0"]
        );
        assert_eq!(
            build_tapback_args("c", "m", "love", Some(1), true).last().unwrap(),
            "--remove"
        );
        assert_eq!(
            build_edit_args("c", "m", "new", None, None),
            vec![
                "edit", "--chat", "c", "--message", "m", "--new-text", "new",
                "--bc-text", "new", "--part", "0",
            ]
        );
        // Index 8 is the `--bc-text` value (index 9 is the `--part` flag).
        assert_eq!(build_edit_args("c", "m", "new", Some("old"), None)[8], "old");
        assert_eq!(
            build_unsend_args("c", "m", Some(3)),
            vec!["unsend", "--chat", "c", "--message", "m", "--part", "3"]
        );
    }

    #[test]
    fn attachment_and_poll_args() {
        assert_eq!(
            build_send_attachment_args("any;-;+1555", "/tmp/f.m4a", true, Some("G1")),
            vec![
                "send-attachment", "--chat", "any;-;+1555", "--file", "/tmp/f.m4a",
                "--audio", "--reply-to", "G1", "--transport", "auto",
            ]
        );
        assert_eq!(
            build_poll_send_args("c", "Lunch?", &["Pizza".to_string(), "Sushi".to_string()], None),
            vec![
                "poll", "send", "--chat", "c", "--question", "Lunch?",
                "--option", "Pizza", "--option", "Sushi",
            ]
        );
        assert_eq!(
            build_poll_vote_args("c", "P1", &PollVoteSelector::OptionIndex(2)),
            vec!["poll", "vote", "--chat", "c", "--poll", "P1", "--option-index", "2"]
        );
        assert_eq!(
            build_poll_vote_args("c", "P1", &PollVoteSelector::OptionId("o".into()))[6],
            "--option-id"
        );
        assert_eq!(
            build_poll_vote_args("c", "P1", &PollVoteSelector::OptionText("Pizza".into()))[7],
            "Pizza"
        );
    }

    #[test]
    fn tapback_mapping() {
        assert_eq!(map_tapback_reaction(Some("❤️")), Some(TapbackKind::Love));
        assert_eq!(map_tapback_reaction(Some("👍")), Some(TapbackKind::Like));
        assert_eq!(map_tapback_reaction(Some("THUMBSDOWN")), Some(TapbackKind::Dislike));
        assert_eq!(map_tapback_reaction(Some("🤣")), Some(TapbackKind::Laugh));
        assert_eq!(map_tapback_reaction(Some("‼️")), Some(TapbackKind::Emphasize));
        assert_eq!(map_tapback_reaction(Some("?")), Some(TapbackKind::Question));
        assert_eq!(map_tapback_reaction(Some("🎉")), None);
        assert_eq!(map_tapback_reaction(None), None);
        assert_eq!(TapbackKind::ALL.len(), 6);
    }

    #[test]
    fn cli_failure_and_error_classifiers() {
        assert_eq!(
            resolve_imsg_cli_failure(&serde_json::json!({"success": false, "error": " boom "})),
            Some("boom".to_string())
        );
        assert_eq!(
            resolve_imsg_cli_failure(&serde_json::json!({"success": false})),
            Some("iMessage action failed".to_string())
        );
        assert_eq!(resolve_imsg_cli_failure(&serde_json::json!({"success": true})), None);
        assert_eq!(resolve_imsg_cli_failure(&serde_json::json!({})), None);

        assert!(is_imsg_rpc_send_timeout("imsg rpc timeout (send)"));
        assert!(!is_imsg_rpc_send_timeout("imsg rpc timeout (watch)"));

        assert!(is_attachment_command_fallback_error("Unknown command: send-attachment"));
        assert!(is_attachment_command_fallback_error(
            "requires the imsg private api bridge"
        ));
        assert!(!is_attachment_command_fallback_error("connection refused"));

        assert!(is_threaded_reply_unsupported_error("reply_to requires bridge transport"));
        assert!(is_threaded_reply_unsupported_error("cannot send threaded replies"));
        assert!(!is_threaded_reply_unsupported_error("timeout"));
    }

    // ---- recovery cursor ----

    #[test]
    fn db_identity_scopes_cursor_per_database() {
        let home = Some("/Users/me");
        assert_eq!(
            resolve_imessage_recovery_cursor_db_identity(None, None, Some("mac.local"), home),
            "remote:mac.local:default"
        );
        assert_eq!(
            resolve_imessage_recovery_cursor_db_identity(None, Some("/x/chat.db"), None, home),
            "local:/x/chat.db"
        );
        // Tilde expansion matches the implicit default.
        assert_eq!(
            resolve_imessage_recovery_cursor_db_identity(
                None,
                Some("~/Library/Messages/chat.db"),
                None,
                home
            ),
            resolve_imessage_recovery_cursor_db_identity(None, None, None, home)
        );
        // Default cli (basename imsg) resolves the default chat.db.
        assert_eq!(
            resolve_imessage_recovery_cursor_db_identity(Some("/opt/bin/imsg"), None, None, home),
            "local:/Users/me/Library/Messages/chat.db"
        );
        // Custom cli wrapper keeps a distinct identity.
        assert_eq!(
            resolve_imessage_recovery_cursor_db_identity(Some("/opt/ssh-imsg"), None, None, home),
            "local:cli:/opt/ssh-imsg"
        );
        assert_eq!(
            resolve_imessage_recovery_cursor_db_identity(None, None, None, None),
            "local:default"
        );
    }

    #[test]
    fn recovery_cursor_store_is_monotonic_and_db_scoped() {
        let store = IMessageStateStore::open_in_memory().unwrap();
        assert_eq!(store.load_recovery_cursor("acct", "db-a"), None);
        store.advance_recovery_cursor("acct", "db-a", 100);
        assert_eq!(store.load_recovery_cursor("acct", "db-a"), Some(100));
        // Never rewinds.
        store.advance_recovery_cursor("acct", "db-a", 50);
        assert_eq!(store.load_recovery_cursor("acct", "db-a"), Some(100));
        store.advance_recovery_cursor("acct", "db-a", 150);
        assert_eq!(store.load_recovery_cursor("acct", "db-a"), Some(150));
        // A different database identity never inherits the cursor.
        assert_eq!(store.load_recovery_cursor("acct", "db-b"), None);
    }

    #[test]
    fn recovery_since_rowid_is_clamped() {
        assert_eq!(clamp_recovery_since_rowid(None, 10_000), None);
        assert_eq!(clamp_recovery_since_rowid(Some(9_900), 10_000), Some(9_900));
        // A months-old cursor is clamped to max - 500.
        assert_eq!(clamp_recovery_since_rowid(Some(10), 10_000), Some(9_500));
        assert_eq!(clamp_recovery_since_rowid(Some(10), 300), Some(10));
    }

    // ---- inbound dedupe ----

    #[test]
    fn replay_key_prefers_guid_then_composite() {
        let guid_msg = IMessageInboundIdentity { guid: Some(" G-1 "), ..Default::default() };
        assert_eq!(
            build_imessage_inbound_replay_key("a", &guid_msg),
            Some("a:guid:G-1".to_string())
        );
        let composite = IMessageInboundIdentity {
            sender: Some("+1555"),
            chat_id: Some(7),
            created_at: Some("2026-07-01T00:00:00Z"),
            text: Some("hi"),
            ..Default::default()
        };
        let key = build_imessage_inbound_replay_key("a", &composite).unwrap();
        assert!(key.starts_with("a:c:"));
        assert_eq!(key.len(), "a:c:".len() + 32);
        // Same identity -> same key; different text -> different key.
        assert_eq!(build_imessage_inbound_replay_key("a", &composite).unwrap(), key);
        let other = IMessageInboundIdentity { text: Some("bye"), ..composite.clone() };
        assert_ne!(build_imessage_inbound_replay_key("a", &other).unwrap(), key);
        // Unidentifiable -> None (fail open).
        let missing = IMessageInboundIdentity { sender: Some("+1555"), ..Default::default() };
        assert_eq!(build_imessage_inbound_replay_key("a", &missing), None);
    }

    #[test]
    fn stale_backlog_fence() {
        let now = 1_752_000_000_000i64;
        let old = chrono::DateTime::from_timestamp_millis(now - 16 * 60 * 1000)
            .unwrap()
            .to_rfc3339();
        let fresh = chrono::DateTime::from_timestamp_millis(now - 60 * 1000).unwrap().to_rfc3339();
        assert!(is_stale_imessage_backlog(Some(&old), now, IMESSAGE_STALE_INBOUND_THRESHOLD_MS));
        assert!(!is_stale_imessage_backlog(Some(&fresh), now, IMESSAGE_STALE_INBOUND_THRESHOLD_MS));
        // Fails open on missing/unparseable dates.
        assert!(!is_stale_imessage_backlog(None, now, IMESSAGE_STALE_INBOUND_THRESHOLD_MS));
        assert!(!is_stale_imessage_backlog(Some("garbage"), now, IMESSAGE_STALE_INBOUND_THRESHOLD_MS));
    }

    #[test]
    fn replay_guard_claim_commit_release() {
        let guard = InboundReplayGuard::new();
        let now = 1_000i64;
        assert!(guard.claim("k1", now));
        // Duplicate while in flight is rejected.
        assert!(!guard.claim("k1", now + 1));
        // Release on failure allows retry.
        guard.release("k1");
        assert!(guard.claim("k1", now + 2));
        guard.commit("k1", now + 3);
        // Committed keys stay blocked within TTL...
        assert!(!guard.claim("k1", now + 4));
        // Release does not evict a committed claim.
        guard.release("k1");
        assert!(!guard.claim("k1", now + 5));
        // ...and expire after the TTL.
        assert!(guard.claim("k1", now + 3 + IMESSAGE_INBOUND_DEDUPE_TTL_MS + 1));
    }

    // ---- echo suppression ----

    #[test]
    fn echo_cache_text_and_id_matching() {
        let cache = SentMessageCache::new();
        let now = 10_000i64;
        cache.remember("scope", Some("hello world"), Some("GUID-1"), now);
        // NOTE: assertions must advance `now` monotonically. `has()` sweeps
        // expired entries using the clock it is handed, so probing a far-future
        // timestamp first would evict the 4s text entry that the earlier-time
        // assertions below still need.
        //
        // Scope isolation.
        assert!(!cache.has("other", None, Some("GUID-1"), false, now + 1));
        // Id mismatch short-circuits text matching for id-backed text...
        assert!(!cache.has("scope", Some("hello world"), Some("OTHER"), false, now + 1_000));
        // ...unless the caller opts out (self-chat numeric row ids).
        assert!(cache.has("scope", Some("hello world"), Some("12345"), true, now + 1_000));
        // Text match within 4s (id backs the text; matching id also passes).
        assert!(cache.has("scope", Some("hello world"), Some("GUID-1"), false, now + 3_000));
        // Text expires after 4s.
        assert!(!cache.has("scope", Some("hello world"), None, false, now + 5_000));
        // Id match still good within 60s, after the text copy has aged out.
        assert!(cache.has("scope", None, Some("GUID-1"), false, now + 30_000));
        // "ok"/"unknown" ids are not identities.
        cache.remember("scope", None, Some("ok"), now);
        assert!(!cache.has("scope", None, Some("ok"), false, now + 1));
    }

    #[test]
    fn echo_cache_normalizes_text_and_corruption_markers() {
        let cache = SentMessageCache::new();
        let now = 0i64;
        cache.remember("s", Some("line1\r\nline2"), None, now);
        assert!(cache.has("s", Some("line1\nline2"), None, false, now + 1));
        // Leading corruption markers stripped on the inbound copy.
        assert!(cache.has("s", Some("\u{fffc}\u{fffd} line1\nline2"), None, false, now + 1));
    }

    #[test]
    fn durable_echo_markers_pending_and_forget() {
        let store = IMessageStateStore::open_in_memory().unwrap();
        let now = 1_000_000i64;
        // Pending marker is only visible when include_pending is set.
        let key = store
            .remember_echo_marker("s", Some("draft"), None, true, 30_000, now)
            .unwrap();
        assert!(!store.has_echo_marker("s", Some("draft"), None, false, now + 1));
        assert!(store.has_echo_marker("s", Some("draft"), None, true, now + 1));
        // Failed send forgets the pending marker.
        store.forget_echo_marker(&key);
        assert!(!store.has_echo_marker("s", Some("draft"), None, true, now + 2));
        // Committed marker matches by text or id and survives (durable).
        store.remember_echo_marker("s", Some("sent text"), Some("G-9"), false, 60_000, now);
        assert!(store.has_echo_marker("s", None, Some("G-9"), false, now + 1));
        assert!(store.has_echo_marker("s", Some("sent text"), None, false, now + 1));
        assert!(!store.has_echo_marker("other", None, Some("G-9"), false, now + 1));
        // Expires after TTL.
        assert!(!store.has_echo_marker("s", None, Some("G-9"), false, now + 60_001));
    }

    // ---- self-chat cache ----

    #[test]
    fn self_chat_cache_exact_and_skew_matching() {
        let cache = SelfChatCache::new();
        let base = SelfChatLookup {
            account_id: "a",
            sender: "+1555",
            is_group: false,
            chat_id: None,
            text: Some("note to self"),
            created_at_ms: Some(1_000_000),
            allow_created_at_skew: false,
        };
        let now = 500_000i64;
        cache.remember(&base, now);
        assert!(cache.has(&base, now + 1_000));
        // Exact-only without skew tolerance.
        let skewed = SelfChatLookup { created_at_ms: Some(1_000_500), ..base.clone() };
        assert!(!cache.has(&skewed, now + 1_000));
        // With skew tolerance a <1s delta matches.
        let tolerant = SelfChatLookup { allow_created_at_skew: true, ..base.clone() };
        cache.remember(&tolerant, now);
        assert!(cache.has(&skewed, now + 1_000));
        let too_skewed = SelfChatLookup { created_at_ms: Some(1_001_500), ..base.clone() };
        assert!(!cache.has(&too_skewed, now + 1_000));
        // TTL expiry.
        assert!(!cache.has(&base, now + SELF_CHAT_TTL_MS + 1));
        // Group scope isolates by chat id.
        let group = SelfChatLookup { is_group: true, chat_id: Some(9), ..base.clone() };
        assert!(!cache.has(&group, now + 1_000));
    }

    // ---- polls ----

    #[test]
    fn poll_render_options_votes_and_vote_updates() {
        let poll = IMessagePoll {
            question: Some("Lunch?".to_string()),
            options: vec![
                IMessagePollOption { id: "o1".into(), text: "Pizza".into() },
                IMessagePollOption { id: "o2".into(), text: "Sushi".into() },
            ],
            votes: vec![
                IMessagePollVote { option_id: Some("o1".into()), ..Default::default() },
                IMessagePollVote {
                    option_id: Some("o2".into()),
                    event_type: Some("removed".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let body = render_imessage_poll_body(&poll).unwrap();
        assert!(body.contains("Poll: Lunch?"));
        assert!(body.contains("1) Pizza [1]"));
        assert!(body.contains("2) Sushi"));
        assert!(!body.contains("Sushi [1]"));
        assert!(body.contains("poll-vote action"));

        let vote_update = IMessagePoll {
            kind: Some("vote".to_string()),
            vote: Some(IMessagePollVote {
                participant: Some("+1555".into()),
                option_text: Some("Pizza".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            render_imessage_poll_body(&vote_update).unwrap(),
            "\u{1F4CA} Poll vote: +1555 voted for \"Pizza\""
        );
        let removed = IMessagePoll {
            kind: Some("vote".to_string()),
            vote: Some(IMessagePollVote {
                event_type: Some("removed".into()),
                option_id: Some("o1".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(render_imessage_poll_body(&removed).unwrap().contains("removed their vote"));
        assert_eq!(render_imessage_poll_body(&IMessagePoll::default()), None);
    }

    #[test]
    fn poll_comment_folding_fails_closed_on_identity() {
        let folder = PollCommentFolder::default();
        folder.remember_poll(Some("P1"), 1_000, Some("+1555"));
        // Caption from the creator within the window folds.
        assert!(folder.is_poll_comment(Some("P1"), 2_000, Some("+1555")));
        // Different sender never folds.
        assert!(!folder.is_poll_comment(Some("P1"), 2_000, Some("+1666")));
        // Unknown sender never folds (fail closed).
        assert!(!folder.is_poll_comment(Some("P1"), 2_000, None));
        // Outside the window is a genuine reply.
        assert!(!folder.is_poll_comment(Some("P1"), 1_000 + POLL_COMMENT_WINDOW_MS + 1, Some("+1555")));
        // Before the poll is impossible echo ordering.
        assert!(!folder.is_poll_comment(Some("P1"), 500, Some("+1555")));
        // Unknown poll guid.
        assert!(!folder.is_poll_comment(Some("P2"), 2_000, Some("+1555")));
        // Poll without sender identity is never tracked as foldable.
        folder.remember_poll(Some("P3"), 1_000, None);
        assert!(!folder.is_poll_comment(Some("P3"), 2_000, Some("+1555")));
    }

    // ---- per-group systemPrompt ----

    #[test]
    fn group_system_prompt_specific_wins_and_empty_suppresses_wildcard() {
        let mut groups: HashMap<String, IMessageGroupConfig> = HashMap::new();
        groups.insert(
            "7".to_string(),
            IMessageGroupConfig { system_prompt: Some("Be terse.".into()), ..Default::default() },
        );
        groups.insert(
            "8".to_string(),
            IMessageGroupConfig { system_prompt: Some("  ".into()), ..Default::default() },
        );
        groups.insert("9".to_string(), IMessageGroupConfig::default());
        groups.insert(
            "*".to_string(),
            IMessageGroupConfig { system_prompt: Some("Wildcard.".into()), ..Default::default() },
        );

        let (specific, wildcard) = lookup_imessage_group_config(Some(&groups), "7");
        assert_eq!(
            resolve_imessage_group_system_prompt(specific, wildcard),
            Some("Be terse.".to_string())
        );
        // Present-but-empty suppresses the wildcard.
        let (specific, wildcard) = lookup_imessage_group_config(Some(&groups), "8");
        assert_eq!(resolve_imessage_group_system_prompt(specific, wildcard), None);
        // Absent key falls through to the wildcard.
        let (specific, wildcard) = lookup_imessage_group_config(Some(&groups), "9");
        assert_eq!(
            resolve_imessage_group_system_prompt(specific, wildcard),
            Some("Wildcard.".to_string())
        );
        // Unknown chat id -> wildcard.
        let (specific, wildcard) = lookup_imessage_group_config(Some(&groups), "unknown");
        assert_eq!(
            resolve_imessage_group_system_prompt(specific, wildcard),
            Some("Wildcard.".to_string())
        );
        assert_eq!(
            resolve_imessage_group_system_prompt(None, None),
            None
        );
    }

    // ---- HEIC staging ----

    #[test]
    fn heic_detection_and_jpeg_filename() {
        assert!(is_heic_attachment("/x/IMG_1.HEIC", None));
        assert!(is_heic_attachment("/x/IMG_1.heif", None));
        assert!(is_heic_attachment("/x/whatever.bin", Some("image/heic")));
        assert!(is_heic_attachment("/x/whatever.bin", Some("IMAGE/HEIF")));
        assert!(!is_heic_attachment("/x/IMG_1.jpg", Some("image/jpeg")));
        assert_eq!(jpeg_filename_for_attachment("/x/IMG_1.HEIC"), "IMG_1.jpg");
        assert_eq!(jpeg_filename_for_attachment(""), "imessage-attachment.jpg");
    }

    // ---- inbound tapbacks ----

    #[test]
    fn reaction_notification_gating_modes() {
        use IMessageReactionNotificationMode as Mode;
        assert_eq!(resolve_imessage_reaction_notification_mode(None), Mode::Own);
        assert_eq!(resolve_imessage_reaction_notification_mode(Some("off")), Mode::Off);
        assert_eq!(resolve_imessage_reaction_notification_mode(Some("ALL")), Mode::All);
        assert!(!should_emit_imessage_reaction_notification(Mode::Off, true));
        assert!(should_emit_imessage_reaction_notification(Mode::Own, true));
        assert!(!should_emit_imessage_reaction_notification(Mode::Own, false));
        assert!(should_emit_imessage_reaction_notification(Mode::All, false));
        assert_eq!(
            build_imessage_reaction_context_key("added", "7", "G-1", "+1555", "👍"),
            "imessage:reaction:added:7:G-1:+1555:👍"
        );
    }
}
