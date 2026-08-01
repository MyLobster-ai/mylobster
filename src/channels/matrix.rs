//! Matrix channel: Client-Server API transport plus the approval, streaming,
//! E2EE-surface, and v7.1 routing/rendering behavior of the OpenClaw Matrix
//! plugin.
//!
//! Ports the observable behavior of OpenClaw v2026.7.1
//! `extensions/matrix/src/` (`exec-approvals.ts`, `approval-native.ts`,
//! `approval-reactions.ts`, `matrix/draft-stream.ts`, `matrix/format.ts`,
//! `matrix/target-ids.ts`, `matrix/direct-room.ts`, `matrix/sync-state.ts`,
//! `matrix/sdk/transport.ts`, `outbound.ts`, `setup-bootstrap.ts`,
//! `matrix/actions/verification.ts`, `cli.ts` encryption flags):
//!
//! - Live exec approvals: exec metadata on approval messages, chunked
//!   fallback for long approval bodies, thread targeting for approval
//!   replies (v2026.4.27 + v2026.5.2 carryover).
//! - Streaming tool-progress updates as room-message edits with
//!   chunk/edit coalescing ([`MatrixDraftStream`], v2026.4.27; MSC4357 live
//!   markers per v2026.7.1).
//! - E2EE setup flow, **non-crypto surface only**: `encryption` config,
//!   recovery-key handling shape, device-verification state machine, and
//!   crypto-store path resolution. The olm/megolm boundary is stubbed behind
//!   [`MatrixCryptoProvider`]; a real implementation needs a
//!   vodozemac-class crate (new dependency, out of scope here). The
//!   `matrix encryption` CLI subcommand lives in the CLI cluster — this file
//!   exposes the channel-side functions it calls
//!   ([`resolve_encryption_setup_mode`], [`maybe_bootstrap_new_encrypted_account`]).
//! - v2026.7.1: forced cross-signing reset w/ recovery key (state machine,
//!   stubbed crypto), SQLite-backed sync-token/state cache with crypto
//!   sidecars ([`MatrixStateCache`]), MSC4222 `state_after` handling,
//!   `com.openclaw.presentation` rich-render metadata on outbound events,
//!   markdown tables → bullet lists fallback, bracketed display-name
//!   mentions, room-id case preservation, two-person-room DM routing
//!   preferred over stale `m.direct`, and approval-reaction persistence
//!   across restarts.

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::config::Config;
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

// ============================================================================
// Extension configuration (config.channels.extensions["matrix"])
// ============================================================================

/// Exec-approval section of the Matrix extension config (upstream
/// `execApprovals`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MatrixExecApprovalsConfig {
    pub enabled: Option<bool>,
    pub approvers: Option<Vec<String>>,
    pub agent_filter: Option<Vec<String>>,
    pub session_filter: Option<Vec<String>>,
}

/// DM section (upstream `dm.allowFrom` feeds the approver fallback).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MatrixDmConfig {
    pub allow_from: Option<Vec<String>>,
}

/// Matrix channel configuration read from the flattened
/// `channels.extensions` map (there is no typed `ChannelsConfig` entry).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MatrixExtensionConfig {
    pub enabled: Option<bool>,
    /// Homeserver URL (aliases: `homeserverUrl`).
    pub homeserver: Option<String>,
    pub homeserver_url: Option<String>,
    pub access_token: Option<String>,
    pub user_id: Option<String>,
    /// End-to-end encryption opt-in (upstream `encryption: boolean`).
    pub encryption: Option<bool>,
    /// Override for the state/crypto store root directory.
    pub state_dir: Option<String>,
    pub exec_approvals: Option<MatrixExecApprovalsConfig>,
    pub dm: Option<MatrixDmConfig>,
}

impl MatrixExtensionConfig {
    pub fn effective_homeserver(&self) -> Option<&str> {
        self.homeserver
            .as_deref()
            .or(self.homeserver_url.as_deref())
    }
}

/// Resolves the Matrix extension config from the channels extensions map.
pub fn resolve_matrix_extension_config(config: &Config) -> Option<MatrixExtensionConfig> {
    let raw = config.channels.extensions.get("matrix")?;
    serde_json::from_value(raw.clone()).ok()
}

// ============================================================================
// Target identity + room-id case preservation (matrix/target-ids.ts)
// ============================================================================

/// A resolved Matrix messaging target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatrixTarget {
    Room(String),
    User(String),
}

fn strip_known_prefixes(raw: &str, prefixes: &[&str]) -> String {
    let mut normalized = raw.trim().to_string();
    loop {
        let lowered = normalized.to_lowercase();
        let Some(prefix) = prefixes.iter().find(|p| lowered.starts_with(**p)) else {
            return normalized;
        };
        normalized = normalized[prefix.len()..].trim().to_string();
        if normalized.is_empty() {
            return normalized;
        }
    }
}

/// True for `@user:server` MXIDs.
pub fn is_matrix_qualified_user_id(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.starts_with('@') && trimmed.contains(':')
}

/// Resolves a raw target string into a room or user target. Prefixes
/// (`matrix:`, `room:`, `channel:`, `user:`) are matched
/// **case-insensitively** but the id itself keeps its original case:
/// Matrix room ids are case-sensitive, so `!AbC:server` must never be
/// lowercased (v2026.7.1 room-id case preservation).
pub fn resolve_matrix_target_identity(raw: &str) -> Option<MatrixTarget> {
    let normalized = strip_known_prefixes(raw, &["matrix:"]);
    if normalized.is_empty() {
        return None;
    }
    let lowered = normalized.to_lowercase();
    let strip = |prefix: &str| normalized[prefix.len()..].trim().to_string();
    if lowered.starts_with("user:") {
        let id = strip("user:");
        return (!id.is_empty()).then_some(MatrixTarget::User(id));
    }
    if lowered.starts_with("room:") {
        let id = strip("room:");
        return (!id.is_empty()).then_some(MatrixTarget::Room(id));
    }
    if lowered.starts_with("channel:") {
        let id = strip("channel:");
        return (!id.is_empty()).then_some(MatrixTarget::Room(id));
    }
    if is_matrix_qualified_user_id(&normalized) {
        return Some(MatrixTarget::User(normalized));
    }
    Some(MatrixTarget::Room(normalized))
}

/// Strips routing prefixes for message sends, preserving id case.
pub fn normalize_matrix_messaging_target(raw: &str) -> Option<String> {
    let normalized = strip_known_prefixes(raw, &["matrix:", "room:", "channel:", "user:"]);
    (!normalized.is_empty()).then_some(normalized)
}

// ============================================================================
// Mentions: MXIDs, @room, bracketed display names (matrix/format.ts)
// ============================================================================

/// A mention found in outbound markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MentionKind {
    /// `@room` broadcast mention.
    Room,
    /// Fully-qualified `@user:server` MXID mention.
    User(String),
    /// Bracketed display-name mention `@[Ada Lovelace]` (v2026.7.1):
    /// resolved to an MXID downstream via the room member list.
    DisplayName(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionCandidate {
    pub raw: String,
    pub start: usize,
    pub end: usize,
    pub kind: MentionKind,
}

const ESCAPED_MENTION_SENTINEL: char = '\u{E000}';
const TRIMMABLE_MENTION_SUFFIX: &[char] = &[')', ',', '.', '!', '?', ':', ';', ']'];

/// Masks `\@` escapes outside code spans with a sentinel so mention
/// collection never fires on escaped mentions, while code-span content is
/// left untouched (upstream `maskEscapedMentions`).
pub fn mask_escaped_mentions(markdown: &str) -> String {
    let chars: Vec<char> = markdown.chars().collect();
    let mut masked = String::with_capacity(markdown.len());
    let mut idx = 0usize;
    let mut code_fence_len = 0usize;
    while idx < chars.len() {
        if chars[idx] == '`' && !is_markdown_escaped(&chars, idx) {
            let mut run = 1usize;
            while idx + run < chars.len() && chars[idx + run] == '`' {
                run += 1;
            }
            if code_fence_len == 0 {
                code_fence_len = run;
            } else if run == code_fence_len {
                code_fence_len = 0;
            }
            for c in &chars[idx..idx + run] {
                masked.push(*c);
            }
            idx += run;
            continue;
        }
        if code_fence_len == 0 && chars[idx] == '\\' && chars.get(idx + 1) == Some(&'@') {
            masked.push(ESCAPED_MENTION_SENTINEL);
            idx += 2;
            continue;
        }
        masked.push(chars[idx]);
        idx += 1;
    }
    masked
}

fn is_markdown_escaped(chars: &[char], idx: usize) -> bool {
    let mut slashes = 0usize;
    let mut cursor = idx;
    while cursor > 0 && chars[cursor - 1] == '\\' {
        slashes += 1;
        cursor -= 1;
    }
    slashes % 2 == 1
}

/// Restores masked escapes to visible `@` for rendered output.
pub fn restore_escaped_mentions(text: &str) -> String {
    text.replace(ESCAPED_MENTION_SENTINEL, "@")
}

fn is_mention_start_boundary(char_before: Option<char>) -> bool {
    match char_before {
        None => true,
        Some(c) => !(c.is_ascii_alphanumeric() || c == '_'),
    }
}

fn is_matrix_mention_user_id(raw: &str) -> bool {
    if !is_matrix_qualified_user_id(raw) {
        return false;
    }
    let Some(colon) = raw.find(':') else {
        return false;
    };
    let localpart = &raw[1..colon];
    let server = &raw[colon + 1..];
    if localpart.is_empty() || server.is_empty() {
        return false;
    }
    localpart
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._=+-/".contains(c))
        && server
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c))
}

fn trim_mention_suffix(raw: &str) -> &str {
    let mut trimmed = raw;
    while trimmed.len() > 1 {
        let last = trimmed.chars().last().unwrap();
        if !TRIMMABLE_MENTION_SUFFIX.contains(&last) {
            break;
        }
        // Keep IPv6-literal server names (`@u:[::1]:8448`) intact.
        if last == ']' && looks_like_ipv6_literal_tail(trimmed) {
            break;
        }
        trimmed = &trimmed[..trimmed.len() - last.len_utf8()];
    }
    trimmed
}

fn looks_like_ipv6_literal_tail(raw: &str) -> bool {
    let Some(open) = raw.rfind('[') else {
        return false;
    };
    raw[open + 1..raw.len() - 1]
        .chars()
        .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
}

/// Collects mention candidates from masked text: `@room`, qualified MXIDs,
/// and bracketed display names `@[Name With Spaces]`. Escaped mentions and
/// code spans never produce candidates.
pub fn collect_mention_candidates(text: &str) -> Vec<MentionCandidate> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] != b'@' {
            idx += 1;
            continue;
        }
        let char_before = text[..idx].chars().next_back();
        if !is_mention_start_boundary(char_before) {
            idx += 1;
            continue;
        }
        // Bracketed display-name mention: @[Ada Lovelace]
        if bytes.get(idx + 1) == Some(&b'[') {
            if let Some(close_rel) = text[idx + 2..].find(']') {
                let name = &text[idx + 2..idx + 2 + close_rel];
                let end = idx + 2 + close_rel + 1;
                if !name.trim().is_empty() && !name.contains('\n') {
                    out.push(MentionCandidate {
                        raw: text[idx..end].to_string(),
                        start: idx,
                        end,
                        kind: MentionKind::DisplayName(name.trim().to_string()),
                    });
                    idx = end;
                    continue;
                }
            }
            idx += 1;
            continue;
        }
        // Plain token: @word or @user:server (only the leading '@' belongs
        // to the token).
        let rest = &text[idx..];
        let mut token_end = 0usize;
        for (i, c) in rest.char_indices() {
            if i == 0 {
                token_end = c.len_utf8();
                continue;
            }
            if c.is_ascii_alphanumeric() || "._=+-/:[]".contains(c) {
                token_end = i + c.len_utf8();
            } else {
                break;
            }
        }
        let raw = trim_mention_suffix(&rest[..token_end]);
        if raw == "@" {
            idx += 1;
            continue;
        }
        let end = idx + raw.len();
        if raw.eq_ignore_ascii_case("@room") {
            out.push(MentionCandidate {
                raw: raw.to_string(),
                start: idx,
                end,
                kind: MentionKind::Room,
            });
        } else if is_matrix_mention_user_id(raw) {
            out.push(MentionCandidate {
                raw: raw.to_string(),
                start: idx,
                end,
                kind: MentionKind::User(raw.to_string()),
            });
        } else {
            idx += 1;
            continue;
        }
        idx = end;
    }
    out
}

/// matrix.to permalink for a mention link.
pub fn matrix_to_mention_href(user_id: &str) -> String {
    format!(
        "https://matrix.to/#/{}",
        url::form_urlencoded::byte_serialize(user_id.as_bytes()).collect::<String>()
    )
}

// ============================================================================
// Markdown tables → bullet lists (v2026.7.1 rich-render fallback)
// ============================================================================

fn split_table_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let inner = trimmed
        .strip_prefix('|')
        .unwrap_or(trimmed)
        .strip_suffix('|')
        .unwrap_or(trimmed.strip_prefix('|').unwrap_or(trimmed));
    inner.split('|').map(|c| c.trim().to_string()).collect()
}

fn is_table_separator_row(line: &str) -> bool {
    let cells = split_table_row(line);
    !cells.is_empty()
        && cells.iter().all(|c| {
            !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':')
        })
}

fn looks_like_table_row(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') && trimmed.matches('|').count() >= 2
}

/// Converts GitHub-style pipe tables into bullet lists for clients that
/// render tables poorly: each data row becomes one bullet with
/// `**Header:** value` fields. Non-table text passes through unchanged.
pub fn markdown_tables_to_bullets(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut idx = 0usize;
    while idx < lines.len() {
        let is_table_start = looks_like_table_row(lines[idx])
            && idx + 1 < lines.len()
            && is_table_separator_row(lines[idx + 1]);
        if !is_table_start {
            out.push(lines[idx].to_string());
            idx += 1;
            continue;
        }
        let headers = split_table_row(lines[idx]);
        idx += 2; // skip header + separator
        while idx < lines.len() && looks_like_table_row(lines[idx]) {
            let cells = split_table_row(lines[idx]);
            let fields: Vec<String> = headers
                .iter()
                .enumerate()
                .filter_map(|(col, header)| {
                    let value = cells.get(col).map(String::as_str).unwrap_or("").trim();
                    if value.is_empty() {
                        return None;
                    }
                    if header.is_empty() {
                        Some(value.to_string())
                    } else {
                        Some(format!("**{}:** {}", header, value))
                    }
                })
                .collect();
            if !fields.is_empty() {
                out.push(format!("- {}", fields.join(" — ")));
            }
            idx += 1;
        }
    }
    let mut result = out.join("\n");
    if text.ends_with('\n') {
        result.push('\n');
    }
    result
}

// ============================================================================
// com.openclaw.presentation rich-render metadata (outbound.ts)
// ============================================================================

/// Custom event-content key carrying rich-render metadata.
pub const MATRIX_PRESENTATION_KEY: &str = "com.openclaw.presentation";
/// Declared content type inside the presentation payload.
pub const MATRIX_PRESENTATION_TYPE: &str = "message.presentation";
/// Fallback body when a presentation-only payload has no text (Matrix
/// requires a non-empty `body`).
pub const MATRIX_EMPTY_PRESENTATION_FALLBACK_TEXT: &str = "---";

/// Stamps a presentation payload with the versioned envelope fields.
pub fn build_matrix_presentation_content(presentation: serde_json::Value) -> serde_json::Value {
    let mut content = match presentation {
        serde_json::Value::Object(map) => serde_json::Value::Object(map),
        other => serde_json::json!({ "value": other }),
    };
    content["version"] = serde_json::json!(1);
    content["type"] = serde_json::json!(MATRIX_PRESENTATION_TYPE);
    content
}

/// Extracts a valid presentation payload (`version == 1`,
/// `type == "message.presentation"`) from a channel-data extra-content map.
pub fn resolve_matrix_presentation_content(
    extra_content: &serde_json::Value,
) -> Option<serde_json::Value> {
    let presentation = extra_content.get(MATRIX_PRESENTATION_KEY)?;
    if !presentation.is_object() {
        return None;
    }
    if presentation.get("version") != Some(&serde_json::json!(1)) {
        return None;
    }
    if presentation.get("type").and_then(|v| v.as_str()) != Some(MATRIX_PRESENTATION_TYPE) {
        return None;
    }
    Some(presentation.clone())
}

/// Resolves the event body text: presentation-only payloads with empty text
/// fall back to `"---"` so the event stays valid.
pub fn resolve_matrix_payload_text(text: &str, has_presentation: bool) -> String {
    if !text.trim().is_empty() || !has_presentation {
        return text.to_string();
    }
    MATRIX_EMPTY_PRESENTATION_FALLBACK_TEXT.to_string()
}

// ============================================================================
// MSC4222 `state_after` handling (matrix/sdk/transport.ts)
// ============================================================================

/// Sync filter parameter opting into MSC4222 `state_after` semantics.
pub const MATRIX_STATE_AFTER_SYNC_PARAM: &str = "org.matrix.msc4222.use_state_after";

/// Strips the MSC4222 opt-in parameter from a `/sync` URL so the request can
/// be retried against servers that reject the unstable parameter. Non-sync
/// URLs and URLs without the parameter pass through unchanged.
pub fn without_matrix_state_after_sync_param(raw_url: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(raw_url) else {
        return raw_url.to_string();
    };
    if !parsed.path().ends_with("/sync") {
        return raw_url.to_string();
    }
    let has_param = parsed
        .query_pairs()
        .any(|(k, _)| k == MATRIX_STATE_AFTER_SYNC_PARAM);
    if !has_param {
        return raw_url.to_string();
    }
    let remaining: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| k != MATRIX_STATE_AFTER_SYNC_PARAM)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    parsed.set_query(None);
    if !remaining.is_empty() {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in &remaining {
            serializer.append_pair(k, v);
        }
        parsed.set_query(Some(&serializer.finish()));
    }
    parsed.to_string()
}

/// Selects the effective room-state events from a sync room section:
/// MSC4222 `state_after` (post-timeline state) wins over legacy `state`
/// when present, so state computed from the timeline never regresses.
pub fn select_sync_state_events<'a>(
    state_after: Option<&'a serde_json::Value>,
    state: Option<&'a serde_json::Value>,
) -> Option<&'a serde_json::Value> {
    state_after.or(state)
}

// ============================================================================
// Sync-state phases (matrix/sync-state.ts)
// ============================================================================

/// Client sync lifecycle phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixSyncPhase {
    Prepared,
    Syncing,
    Catchup,
    Reconnecting,
    Error,
    Stopped,
}

pub fn is_matrix_ready_sync_phase(phase: MatrixSyncPhase) -> bool {
    matches!(
        phase,
        MatrixSyncPhase::Prepared | MatrixSyncPhase::Syncing | MatrixSyncPhase::Catchup
    )
}

pub fn is_matrix_disconnected_sync_phase(phase: MatrixSyncPhase) -> bool {
    matches!(
        phase,
        MatrixSyncPhase::Reconnecting | MatrixSyncPhase::Error | MatrixSyncPhase::Stopped
    )
}

/// The client can recover from `Error` to `Prepared` during initial sync;
/// only `Stopped` is terminal.
pub fn is_matrix_terminal_sync_phase(phase: MatrixSyncPhase) -> bool {
    phase == MatrixSyncPhase::Stopped
}

// ============================================================================
// SQLite state cache: sync tokens, crypto sidecars, approval reactions
// ============================================================================

/// Persistence TTL for approval-reaction targets (24 h).
pub const APPROVAL_REACTION_TARGET_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const APPROVAL_REACTION_MAX_ENTRIES: usize = 1000;

/// Crypto-store and state paths for one Matrix account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixStatePaths {
    /// Root directory for this account's Matrix state.
    pub root_dir: PathBuf,
    /// SQLite database holding sync tokens, crypto sidecars, and
    /// approval-reaction targets.
    pub sqlite_db: PathBuf,
    /// Directory for the (vodozemac-backed) crypto store sidecar files.
    pub crypto_store_dir: PathBuf,
    /// Legacy recovery-key import location (`recovery-key.json`), consumed
    /// once by doctor-style migration into the SQLite sidecar.
    pub legacy_recovery_key_path: PathBuf,
}

/// Resolves per-account state paths under `state_dir/matrix/<account_id>/`.
pub fn resolve_matrix_state_paths(state_dir: &Path, account_id: &str) -> MatrixStatePaths {
    let account = if account_id.trim().is_empty() {
        "default"
    } else {
        account_id.trim()
    };
    let root_dir = state_dir.join("matrix").join(account);
    MatrixStatePaths {
        sqlite_db: root_dir.join("matrix.sqlite"),
        crypto_store_dir: root_dir.join("crypto"),
        legacy_recovery_key_path: root_dir.join("recovery-key.json"),
        root_dir,
    }
}

/// SQLite-backed Matrix state cache (v2026.7.1): sync tokens survive
/// restarts (no full re-sync), crypto sidecar values (recovery key,
/// backup version, device id) live next to them, and approval-reaction
/// targets persist so a reaction cast after a gateway restart still
/// resolves its approval.
pub struct MatrixStateCache {
    conn: rusqlite::Connection,
}

impl MatrixStateCache {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::init(rusqlite::Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(rusqlite::Connection::open_in_memory()?)
    }

    fn init(conn: rusqlite::Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS matrix_sync_state (
                account_id TEXT PRIMARY KEY,
                since_token TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS matrix_crypto_sidecar (
                account_id TEXT NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (account_id, key)
            );
            CREATE TABLE IF NOT EXISTS matrix_approval_reactions (
                target_key TEXT PRIMARY KEY,
                approval_id TEXT NOT NULL,
                allowed_decisions TEXT NOT NULL,
                expires_at INTEGER NOT NULL
            );",
        )?;
        Ok(Self { conn })
    }

    // ── Sync tokens ─────────────────────────────────────────────────────

    pub fn set_sync_token(&self, account_id: &str, since: &str, now_ms: u64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO matrix_sync_state (account_id, since_token, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id) DO UPDATE SET since_token = ?2, updated_at = ?3",
            rusqlite::params![account_id, since, now_ms as i64],
        )?;
        Ok(())
    }

    pub fn sync_token(&self, account_id: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT since_token FROM matrix_sync_state WHERE account_id = ?1")?;
        let mut rows = stmt.query(rusqlite::params![account_id])?;
        Ok(match rows.next()? {
            Some(row) => Some(row.get(0)?),
            None => None,
        })
    }

    // ── Crypto sidecars (recovery key, backup version, device id) ───────

    pub fn set_crypto_sidecar(&self, account_id: &str, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO matrix_crypto_sidecar (account_id, key, value)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id, key) DO UPDATE SET value = ?3",
            rusqlite::params![account_id, key, value],
        )?;
        Ok(())
    }

    pub fn crypto_sidecar(&self, account_id: &str, key: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT value FROM matrix_crypto_sidecar WHERE account_id = ?1 AND key = ?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![account_id, key])?;
        Ok(match rows.next()? {
            Some(row) => Some(row.get(0)?),
            None => None,
        })
    }

    /// Imports a legacy recovery key into the sidecar unless a **different**
    /// key is already present (upstream legacy-crypto migration: never
    /// overwrite existing state with a conflicting key). Returns whether the
    /// import happened.
    pub fn import_recovery_key(&self, account_id: &str, recovery_key: &str) -> Result<bool> {
        match self.crypto_sidecar(account_id, "recovery_key")? {
            Some(existing) if existing != recovery_key => Ok(false),
            Some(_) => Ok(true),
            None => {
                self.set_crypto_sidecar(account_id, "recovery_key", recovery_key)?;
                Ok(true)
            }
        }
    }

    // ── Approval-reaction targets (persist across restarts) ─────────────

    pub fn register_approval_reaction_target(
        &self,
        room_id: &str,
        event_id: &str,
        approval_id: &str,
        allowed_decisions: &[ApprovalDecision],
        now_ms: u64,
    ) -> Result<()> {
        let Some(key) = approval_reaction_target_key(room_id, event_id) else {
            return Ok(());
        };
        let approval_id = approval_id.trim();
        let mut decisions: Vec<&str> = allowed_decisions
            .iter()
            .map(ApprovalDecision::as_str)
            .collect();
        decisions.dedup();
        if approval_id.is_empty() || decisions.is_empty() {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO matrix_approval_reactions
                (target_key, approval_id, allowed_decisions, expires_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(target_key) DO UPDATE SET
                approval_id = ?2, allowed_decisions = ?3, expires_at = ?4",
            rusqlite::params![
                key,
                approval_id,
                decisions.join(","),
                (now_ms + APPROVAL_REACTION_TARGET_TTL_MS) as i64
            ],
        )?;
        self.prune_approval_reactions(now_ms)?;
        Ok(())
    }

    pub fn unregister_approval_reaction_target(
        &self,
        room_id: &str,
        event_id: &str,
    ) -> Result<()> {
        if let Some(key) = approval_reaction_target_key(room_id, event_id) {
            self.conn.execute(
                "DELETE FROM matrix_approval_reactions WHERE target_key = ?1",
                rusqlite::params![key],
            )?;
        }
        Ok(())
    }

    /// Resolves a reaction on a persisted approval message to a decision,
    /// including after a restart.
    pub fn resolve_approval_reaction(
        &self,
        room_id: &str,
        event_id: &str,
        reaction_key: &str,
        now_ms: u64,
    ) -> Result<Option<(String, ApprovalDecision)>> {
        let Some(key) = approval_reaction_target_key(room_id, event_id) else {
            return Ok(None);
        };
        let mut stmt = self.conn.prepare(
            "SELECT approval_id, allowed_decisions FROM matrix_approval_reactions
             WHERE target_key = ?1 AND expires_at > ?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![key, now_ms as i64])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let approval_id: String = row.get(0)?;
        let allowed_raw: String = row.get(1)?;
        let allowed: Vec<ApprovalDecision> = allowed_raw
            .split(',')
            .filter_map(ApprovalDecision::parse)
            .collect();
        Ok(resolve_approval_reaction_decision(reaction_key, &allowed)
            .map(|decision| (approval_id, decision)))
    }

    fn prune_approval_reactions(&self, now_ms: u64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM matrix_approval_reactions WHERE expires_at <= ?1",
            rusqlite::params![now_ms as i64],
        )?;
        self.conn.execute(
            "DELETE FROM matrix_approval_reactions WHERE target_key NOT IN (
                SELECT target_key FROM matrix_approval_reactions
                ORDER BY expires_at DESC LIMIT ?1
            )",
            rusqlite::params![APPROVAL_REACTION_MAX_ENTRIES as i64],
        )?;
        Ok(())
    }
}

fn approval_reaction_target_key(room_id: &str, event_id: &str) -> Option<String> {
    let room = room_id.trim();
    let event = event_id.trim();
    if room.is_empty() || event.is_empty() {
        return None;
    }
    Some(format!("{}:{}", room, event))
}

// ============================================================================
// Approval reactions (approval-reactions.ts)
// ============================================================================

/// Exec-approval decision carried by a reaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    AllowOnce,
    AllowAlways,
    Deny,
}

impl ApprovalDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AllowOnce => "allow-once",
            Self::AllowAlways => "allow-always",
            Self::Deny => "deny",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "allow-once" => Some(Self::AllowOnce),
            "allow-always" => Some(Self::AllowAlways),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }

    /// Matrix keeps its own reaction emoji set (checkmark/cross render
    /// reliably across Matrix clients).
    pub fn emoji(&self) -> &'static str {
        match self {
            Self::AllowOnce => "✅",
            Self::AllowAlways => "♾️",
            Self::Deny => "❌",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::AllowOnce => "Allow once",
            Self::AllowAlways => "Allow always",
            Self::Deny => "Deny",
        }
    }
}

const APPROVAL_DECISION_ORDER: [ApprovalDecision; 3] = [
    ApprovalDecision::AllowOnce,
    ApprovalDecision::AllowAlways,
    ApprovalDecision::Deny,
];

/// Renders the reaction hint appended to approval messages
/// (`React here: ✅ Allow once, ♾️ Allow always, ❌ Deny`).
pub fn build_approval_reaction_hint(allowed: &[ApprovalDecision]) -> Option<String> {
    let bindings: Vec<String> = APPROVAL_DECISION_ORDER
        .iter()
        .filter(|d| allowed.contains(d))
        .map(|d| format!("{} {}", d.emoji(), d.label()))
        .collect();
    if bindings.is_empty() {
        return None;
    }
    Some(format!("React here: {}", bindings.join(", ")))
}

/// Maps a reaction key back to an allowed decision.
pub fn resolve_approval_reaction_decision(
    reaction_key: &str,
    allowed: &[ApprovalDecision],
) -> Option<ApprovalDecision> {
    let normalized = reaction_key.trim();
    if normalized.is_empty() {
        return None;
    }
    APPROVAL_DECISION_ORDER
        .iter()
        .find(|d| allowed.contains(d) && d.emoji() == normalized)
        .copied()
}

// ============================================================================
// Live exec approvals (exec-approvals.ts / approval-native.ts)
// ============================================================================

/// Single-event text limit used for approval chunking and draft previews
/// (upstream `textChunkLimit: 4000`).
pub const MATRIX_TEXT_CHUNK_LIMIT: usize = 4000;

/// Exec metadata carried on an approval message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecApprovalMetadata {
    pub approval_id: String,
    /// `"exec"` or `"plugin"` (plugin ids are prefixed `plugin:`).
    pub approval_kind: String,
    pub command: Option<String>,
    pub agent_id: Option<String>,
    pub session_key: Option<String>,
}

impl ExecApprovalMetadata {
    /// Upstream `resolveMatrixApprovalKind`: plugin approvals carry a
    /// `plugin:` id prefix.
    pub fn kind_from_id(approval_id: &str) -> &'static str {
        if approval_id.starts_with("plugin:") {
            "plugin"
        } else {
            "exec"
        }
    }
}

/// Normalizes an approver id: strips `matrix:`/`user:` prefixes and
/// lowercases MXIDs. `*` is the wildcard.
pub fn normalize_matrix_approver_id(value: &str) -> Option<String> {
    let stripped = strip_known_prefixes(value, &["matrix:", "user:"]);
    let trimmed = stripped.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "*" {
        return Some("*".to_string());
    }
    Some(trimmed.to_lowercase())
}

/// Resolves the exec-approval approver list: explicit `approvers` first,
/// falling back to `dm.allowFrom`; wildcards are dropped for exec approvals
/// (upstream `normalizeMatrixExecApproverId` maps `*` to undefined).
pub fn resolve_exec_approval_approvers(
    explicit: Option<&[String]>,
    dm_allow_from: Option<&[String]>,
) -> Vec<String> {
    let source = match explicit {
        Some(list) if !list.is_empty() => list,
        _ => dm_allow_from.unwrap_or(&[]),
    };
    let mut out = Vec::new();
    for entry in source {
        if let Some(normalized) = normalize_matrix_approver_id(entry) {
            if normalized != "*" && !out.contains(&normalized) {
                out.push(normalized);
            }
        }
    }
    out
}

/// Native exec approvals are client-enabled only when the feature is not
/// explicitly disabled **and** at least one approver resolves.
pub fn is_exec_approval_client_enabled(
    enabled: Option<bool>,
    approver_count: usize,
) -> bool {
    enabled.unwrap_or(true) && approver_count > 0
}

/// A resolved approval delivery target: room/user plus optional thread so
/// approval replies land in the originating thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixOriginTarget {
    pub to: MatrixTarget,
    pub thread_id: Option<String>,
}

/// Where the approval-triggering turn came from.
#[derive(Debug, Clone, Default)]
pub struct ApprovalRequestOrigin {
    pub turn_source_channel: Option<String>,
    pub turn_source_to: Option<String>,
    pub turn_source_thread_id: Option<String>,
    /// Session-bound conversation target (fallback when the turn source is
    /// not Matrix).
    pub session_to: Option<String>,
    pub session_thread_id: Option<String>,
}

/// Resolves the origin target for approval delivery with thread targeting
/// (upstream `createChannelNativeOriginTargetResolver` chain): turn-source
/// target first (only when the turn came from Matrix), then the session
/// conversation fallback. The thread id rides along so the approval prompt
/// and its replies stay in the originating thread.
pub fn resolve_approval_origin_target(origin: &ApprovalRequestOrigin) -> Option<MatrixOriginTarget> {
    let channel = origin
        .turn_source_channel
        .as_deref()
        .map(str::to_lowercase)
        .unwrap_or_default();
    if channel == "matrix" {
        if let Some(target) = origin
            .turn_source_to
            .as_deref()
            .and_then(resolve_matrix_target_identity)
        {
            return Some(MatrixOriginTarget {
                to: target,
                thread_id: origin
                    .turn_source_thread_id
                    .clone()
                    .filter(|t| !t.trim().is_empty()),
            });
        }
    }
    let target = origin
        .session_to
        .as_deref()
        .and_then(resolve_matrix_target_identity)?;
    Some(MatrixOriginTarget {
        to: target,
        thread_id: origin
            .session_thread_id
            .clone()
            .filter(|t| !t.trim().is_empty()),
    })
}

/// Builds the approval prompt body with exec metadata and the reaction
/// hint. Long command bodies are fenced.
pub fn build_exec_approval_message(
    meta: &ExecApprovalMetadata,
    allowed: &[ApprovalDecision],
) -> String {
    let mut lines: Vec<String> = Vec::new();
    if meta.approval_kind == "plugin" {
        lines.push("**Plugin Approval Required**".to_string());
    } else {
        lines.push("**Exec Approval Required**".to_string());
    }
    if let Some(command) = meta.command.as_deref().filter(|c| !c.trim().is_empty()) {
        lines.push(format!("```\n{}\n```", command.trim_end()));
    }
    let mut meta_bits: Vec<String> = Vec::new();
    if let Some(agent) = meta.agent_id.as_deref().filter(|a| !a.is_empty()) {
        meta_bits.push(format!("agent: {}", agent));
    }
    if let Some(session) = meta.session_key.as_deref().filter(|s| !s.is_empty()) {
        meta_bits.push(format!("session: {}", session));
    }
    meta_bits.push(format!("id: {}", meta.approval_id));
    lines.push(meta_bits.join(" · "));
    if let Some(hint) = build_approval_reaction_hint(allowed) {
        lines.push(hint);
    }
    lines.join("\n")
}

/// Chunks a long approval body for delivery: bodies that fit go out as one
/// event; longer ones split on line boundaries under `limit`, with the
/// reaction target registered on the **final** chunk so the reaction hint
/// and metadata stay adjacent (chunked fallback, v2026.4.27).
pub fn chunk_approval_body(body: &str, limit: usize) -> Vec<String> {
    if limit == 0 || body.chars().count() <= limit {
        return vec![body.to_string()];
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for line in body.split_inclusive('\n') {
        let line_len = line.chars().count();
        if current_len + line_len > limit && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_len = 0;
        }
        if line_len > limit {
            // A single oversized line splits hard at the char limit.
            let mut buffer = String::new();
            let mut buffer_len = 0usize;
            for ch in line.chars() {
                if buffer_len == limit {
                    chunks.push(std::mem::take(&mut buffer));
                    buffer_len = 0;
                }
                buffer.push(ch);
                buffer_len += 1;
            }
            if !buffer.is_empty() {
                current = buffer;
                current_len = buffer_len;
            }
            continue;
        }
        current.push_str(line);
        current_len += line_len;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

// ============================================================================
// Streaming tool-progress draft stream (matrix/draft-stream.ts)
// ============================================================================

/// Draft edit throttle (upstream `DEFAULT_THROTTLE_MS`): progress updates
/// coalesce into at most one send/edit per second.
pub const MATRIX_DRAFT_THROTTLE_MS: u64 = 1000;

/// Decision produced by [`MatrixDraftStream::prepare_update`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftStreamAction {
    /// Nothing to send (empty text, unchanged content, throttled, stream
    /// stopped, or a prior create failed).
    Skip,
    /// Create the initial preview message (with the MSC4357 live marker when
    /// enabled).
    Create { text: String, live: bool },
    /// Edit the existing preview message in place.
    Edit {
        event_id: String,
        text: String,
        live: bool,
    },
    /// The preview no longer fits a single event: the stream stops and the
    /// caller must deliver the final text through normal chunked delivery.
    ExceededLimit,
}

/// Streaming tool-progress / partial-reply window for one Matrix room:
/// tool progress events become one room message that is durably **edited**
/// in place, with chunk/edit coalescing (throttle + dedupe), a hard
/// single-event limit that falls back to normal delivery, and MSC4357 live
/// markers cleared by a final edit. This is the decision state machine; the
/// live send/edit HTTP calls are the integration point.
#[derive(Debug)]
pub struct MatrixDraftStream {
    live: bool,
    limit: usize,
    current_event_id: Option<String>,
    last_sent_text: String,
    last_flush_at_ms: Option<u64>,
    stopped: bool,
    send_failed: bool,
    finalize_in_place_blocked: bool,
    live_finalized: bool,
}

impl MatrixDraftStream {
    pub fn new(live: bool) -> Self {
        Self {
            live,
            limit: MATRIX_TEXT_CHUNK_LIMIT,
            current_event_id: None,
            last_sent_text: String::new(),
            last_flush_at_ms: None,
            stopped: false,
            send_failed: false,
            finalize_in_place_blocked: false,
            live_finalized: false,
        }
    }

    pub fn with_limit(live: bool, limit: usize) -> Self {
        Self {
            limit,
            ..Self::new(live)
        }
    }

    pub fn event_id(&self) -> Option<&str> {
        self.current_event_id.as_deref()
    }

    /// Prepares the next update. `force` bypasses the throttle (used by
    /// flush/stop). Coalescing: unchanged text and updates inside the 1 s
    /// throttle window are skipped.
    pub fn prepare_update(&mut self, text: &str, now_ms: u64, force: bool) -> DraftStreamAction {
        if self.stopped || self.send_failed {
            return DraftStreamAction::Skip;
        }
        let trimmed = text.trim_end();
        if trimmed.is_empty() {
            return DraftStreamAction::Skip;
        }
        if trimmed.chars().count() > self.limit {
            self.finalize_in_place_blocked = true;
            if self.current_event_id.is_none() {
                self.send_failed = true;
            }
            self.stopped = true;
            return DraftStreamAction::ExceededLimit;
        }
        if trimmed == self.last_sent_text {
            return DraftStreamAction::Skip;
        }
        if !force {
            if let Some(last) = self.last_flush_at_ms {
                if now_ms.saturating_sub(last) < MATRIX_DRAFT_THROTTLE_MS {
                    return DraftStreamAction::Skip;
                }
            }
        }
        match &self.current_event_id {
            None => DraftStreamAction::Create {
                text: trimmed.to_string(),
                live: self.live,
            },
            Some(event_id) => DraftStreamAction::Edit {
                event_id: event_id.clone(),
                text: trimmed.to_string(),
                live: self.live,
            },
        }
    }

    /// Records the outcome of an attempted create/edit.
    pub fn on_send_result(&mut self, text: &str, event_id: Option<String>, ok: bool, now_ms: u64) {
        if !ok {
            if self.current_event_id.is_none() {
                self.send_failed = true;
            }
            self.stopped = true;
            return;
        }
        if let Some(id) = event_id {
            self.current_event_id = Some(id);
        }
        self.last_sent_text = text.trim_end().to_string();
        self.last_flush_at_ms = Some(now_ms);
    }

    /// Final edit clearing the MSC4357 live marker so supporting clients
    /// stop the streaming animation. Returns the edit to issue, or `None`
    /// when no finalize is needed.
    pub fn prepare_finalize_live(&mut self) -> Option<DraftStreamAction> {
        if self.live && !self.live_finalized && self.current_event_id.is_some()
            && !self.last_sent_text.is_empty()
        {
            self.live_finalized = true;
            return Some(DraftStreamAction::Edit {
                event_id: self.current_event_id.clone().unwrap(),
                text: self.last_sent_text.clone(),
                live: false,
            });
        }
        None
    }

    /// A failed finalize leaves the live marker on the last edit; flag the
    /// stream so callers fall back to normal delivery instead of leaving the
    /// message stuck "still streaming".
    pub fn on_finalize_failed(&mut self) {
        self.finalize_in_place_blocked = true;
    }

    pub fn stop(&mut self) -> Option<String> {
        self.stopped = true;
        self.current_event_id.clone()
    }

    /// True when preview streaming must fall back to normal final delivery.
    pub fn must_deliver_final_normally(&self) -> bool {
        self.send_failed || self.finalize_in_place_blocked
    }

    /// True when the given text matches the last rendered draft payload
    /// (the final reply can then adopt the draft in place).
    pub fn matches_prepared_text(&self, text: &str) -> bool {
        text.trim_end() == self.last_sent_text
    }

    /// Resets state for the next text block (after tool calls).
    pub fn reset(&mut self) {
        self.current_event_id = None;
        self.last_sent_text.clear();
        self.last_flush_at_ms = None;
        self.stopped = false;
        self.send_failed = false;
        self.finalize_in_place_blocked = false;
        self.live_finalized = false;
    }
}

// ============================================================================
// E2EE surface: crypto boundary, verification, setup (stubbed crypto)
// ============================================================================

/// Boundary trait for the olm/megolm crypto engine. mylobster's dependency
/// set has no Matrix crypto implementation; a real backend needs a
/// **vodozemac**-class crate (olm/megolm in Rust, as used by matrix-rust-sdk)
/// plugged in behind this trait. All non-crypto flow (config, state paths,
/// verification phases, recovery-key sidecars, reset planning) is
/// implemented and testable without it.
pub trait MatrixCryptoProvider: Send + Sync {
    /// Bootstraps cross-signing, optionally forcing a reset of existing
    /// keys (v2026.7.1 forced cross-signing reset).
    fn bootstrap_cross_signing(&self, force_reset: bool) -> Result<()>;
    /// Restores key backup access from a recovery key.
    fn restore_from_recovery_key(&self, recovery_key: &str) -> Result<()>;
    /// Current key-backup version, when the account has one.
    fn backup_version(&self) -> Result<Option<String>>;
}

/// Stub crypto provider: every operation reports the missing vodozemac
/// dependency. Wired as the default so the setup flow degrades with an
/// actionable error instead of a crash.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableCryptoProvider;

impl MatrixCryptoProvider for UnavailableCryptoProvider {
    fn bootstrap_cross_signing(&self, _force_reset: bool) -> Result<()> {
        anyhow::bail!(
            "Matrix E2EE crypto is not available in this build: olm/megolm support \
             requires a vodozemac-class crate behind MatrixCryptoProvider"
        )
    }

    fn restore_from_recovery_key(&self, _recovery_key: &str) -> Result<()> {
        anyhow::bail!(
            "Matrix E2EE crypto is not available in this build: olm/megolm support \
             requires a vodozemac-class crate behind MatrixCryptoProvider"
        )
    }

    fn backup_version(&self) -> Result<Option<String>> {
        Ok(None)
    }
}

/// Device-verification phase (upstream `MatrixVerificationSummary` phases).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationPhase {
    Requested,
    Ready,
    Started,
    Cancelled,
    Completed,
}

/// Snapshot of one in-flight verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationSummary {
    pub phase: VerificationPhase,
    pub has_sas: bool,
    pub chosen_method: Option<String>,
    pub completed: bool,
}

/// Ready to show/confirm SAS emoji (upstream
/// `isMatrixVerificationReadyForSas`).
pub fn is_verification_ready_for_sas(summary: &VerificationSummary) -> bool {
    summary.completed
        || summary.has_sas
        || matches!(summary.phase, VerificationPhase::Ready | VerificationPhase::Started)
}

/// SAS should be started by our side (upstream
/// `shouldStartMatrixSasVerification`).
pub fn should_start_sas_verification(summary: &VerificationSummary) -> bool {
    !summary.has_sas && summary.phase != VerificationPhase::Started && !summary.completed
}

fn is_sas_method(method: Option<&str>) -> bool {
    matches!(method, Some("m.sas.v1") | Some("sas"))
}

/// Failure text while waiting for SAS, or `None` when the wait may continue
/// (upstream `getMatrixVerificationSasWaitFailure`).
pub fn verification_sas_wait_failure(
    summary: &VerificationSummary,
    label: &str,
) -> Option<String> {
    if summary.has_sas || summary.phase == VerificationPhase::Cancelled {
        return None;
    }
    let method_suffix = summary
        .chosen_method
        .as_deref()
        .map(|m| format!(" (method: {})", m))
        .unwrap_or_default();
    if summary.completed {
        return Some(format!(
            "Matrix self-verification completed without SAS while waiting to {}{}",
            label, method_suffix
        ));
    }
    if summary.phase == VerificationPhase::Started
        && summary.chosen_method.is_some()
        && !is_sas_method(summary.chosen_method.as_deref())
    {
        return Some(format!(
            "Matrix self-verification started without SAS while waiting to {}{}",
            label, method_suffix
        ));
    }
    None
}

/// How the encryption setup entry point should proceed (upstream `cli.ts`:
/// `verifyOnly = !encryptionChanged && !recoveryKey && !forceResetCrossSigning`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionSetupMode {
    /// Nothing changed and no key/reset was supplied — only verify current
    /// device state.
    VerifyOnly,
    /// Run the bootstrap (cross-signing + backup), optionally forcing a
    /// cross-signing reset with the supplied recovery key.
    Bootstrap { force_reset_cross_signing: bool },
}

/// Channel-side resolver for the `matrix encryption setup` CLI subcommand
/// (the subcommand itself lives in the CLI cluster).
pub fn resolve_encryption_setup_mode(
    encryption_changed: bool,
    has_recovery_key: bool,
    force_reset_cross_signing: bool,
) -> EncryptionSetupMode {
    if !encryption_changed && !has_recovery_key && !force_reset_cross_signing {
        EncryptionSetupMode::VerifyOnly
    } else {
        EncryptionSetupMode::Bootstrap {
            force_reset_cross_signing,
        }
    }
}

/// Result of a verification bootstrap attempt (upstream
/// `MatrixSetupVerificationBootstrapResult`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VerificationBootstrapResult {
    pub attempted: bool,
    pub success: bool,
    pub recovery_key_created_at: Option<String>,
    pub backup_version: Option<String>,
    pub error: Option<String>,
}

/// Bootstrap gating for config writes (upstream
/// `maybeBootstrapNewEncryptedMatrixAccount`): bootstrap runs only when
/// encryption is newly turned on — accounts that already had
/// `encryption: true` are left alone.
pub fn should_bootstrap_new_encrypted_account(
    previous_encryption: Option<bool>,
    next_encryption: Option<bool>,
) -> bool {
    next_encryption == Some(true) && previous_encryption != Some(true)
}

/// Runs the encryption bootstrap through the crypto boundary. With the
/// default [`UnavailableCryptoProvider`] this reports the missing
/// vodozemac dependency in `error` instead of failing hard.
pub fn maybe_bootstrap_new_encrypted_account(
    previous_encryption: Option<bool>,
    next_encryption: Option<bool>,
    crypto: &dyn MatrixCryptoProvider,
    force_reset_cross_signing: bool,
) -> VerificationBootstrapResult {
    if !should_bootstrap_new_encrypted_account(previous_encryption, next_encryption) {
        return VerificationBootstrapResult::default();
    }
    match crypto.bootstrap_cross_signing(force_reset_cross_signing) {
        Ok(()) => {
            let backup_version = crypto.backup_version().ok().flatten();
            VerificationBootstrapResult {
                attempted: true,
                success: true,
                recovery_key_created_at: None,
                backup_version,
                error: None,
            }
        }
        Err(err) => VerificationBootstrapResult {
            attempted: true,
            success: false,
            recovery_key_created_at: None,
            backup_version: None,
            error: Some(err.to_string()),
        },
    }
}

// ============================================================================
// DM routing: two-person rooms over stale m.direct (matrix/direct-room.ts)
// ============================================================================

/// Strict DM evidence: exactly the bot and the remote user are joined.
pub fn is_strict_direct_membership(
    self_user_id: Option<&str>,
    remote_user_id: Option<&str>,
    joined_members: &[String],
) -> bool {
    let (Some(self_id), Some(remote_id)) = (
        self_user_id.map(str::trim).filter(|s| !s.is_empty()),
        remote_user_id.map(str::trim).filter(|s| !s.is_empty()),
    ) else {
        return false;
    };
    joined_members.len() == 2
        && joined_members.iter().any(|m| m == self_id)
        && joined_members.iter().any(|m| m == remote_id)
}

/// Routing evidence for one candidate DM room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmRoomEvidence {
    pub room_id: String,
    /// Exactly two joined members: the bot and the remote user.
    pub strict_two_person: bool,
    /// The room is listed for the user in `m.direct` account data.
    pub listed_in_m_direct: bool,
}

/// Chooses the DM room for a user: live two-person-room membership wins over
/// stale `m.direct` account data (which survives the other party leaving or
/// the room growing); `m.direct` only breaks ties among strict candidates
/// and is the last resort when no strict room exists (v2026.7.1).
pub fn resolve_preferred_dm_room(candidates: &[DmRoomEvidence]) -> Option<&DmRoomEvidence> {
    candidates
        .iter()
        .find(|c| c.strict_two_person && c.listed_in_m_direct)
        .or_else(|| candidates.iter().find(|c| c.strict_two_person))
        .or_else(|| candidates.iter().find(|c| c.listed_in_m_direct))
}

// ============================================================================
// Matrix Channel Implementation
// ============================================================================

/// Matrix channel integration using the Client-Server API.
///
/// Communicates with a Matrix homeserver via the Matrix Client-Server API
/// (`/_matrix/client/v3/`). Sends messages using the `/rooms/{roomId}/send`
/// endpoint with `m.room.message` events.
pub struct MatrixChannel {
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// Matrix homeserver URL (e.g. `https://matrix.org`).
    homeserver_url: Option<String>,
    /// Access token for the Matrix account.
    access_token: Option<String>,
    /// Matrix user ID (e.g. `@bot:matrix.org`).
    user_id: Option<String>,
    /// E2EE opt-in from config (crypto itself is stubbed; see
    /// [`MatrixCryptoProvider`]).
    encryption: Option<bool>,
    /// HTTP client for API calls.
    client: Client,
}

impl MatrixChannel {
    pub fn new() -> Self {
        Self {
            enabled: None,
            homeserver_url: None,
            access_token: None,
            user_id: None,
            encryption: None,
            client: Client::new(),
        }
    }

    /// Create a configured Matrix channel.
    pub fn with_config(
        homeserver_url: String,
        access_token: String,
        user_id: String,
    ) -> Self {
        Self {
            enabled: Some(true),
            homeserver_url: Some(homeserver_url),
            access_token: Some(access_token),
            user_id: Some(user_id),
            encryption: None,
            client: Client::new(),
        }
    }

    /// Create a channel from the flattened extensions config
    /// (`channels.extensions["matrix"]`).
    pub fn from_config(config: &Config) -> Self {
        match resolve_matrix_extension_config(config) {
            Some(ext) => Self {
                enabled: ext.enabled,
                homeserver_url: ext.effective_homeserver().map(String::from),
                access_token: ext.access_token,
                user_id: ext.user_id,
                encryption: ext.encryption,
                client: Client::new(),
            },
            None => Self::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }
}

#[async_trait]
impl ChannelPlugin for MatrixChannel {
    fn id(&self) -> &str {
        "matrix"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Matrix".to_string(),
            description: "Matrix protocol channel via Client-Server API".to_string(),
            enabled: self.is_enabled(),
            multi_account: true,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
            ChannelCapability::SendMedia,
            ChannelCapability::ReceiveMedia,
            ChannelCapability::Reactions,
            ChannelCapability::Groups,
            ChannelCapability::Threads,
            ChannelCapability::EditMessage,
            ChannelCapability::DeleteMessage,
            ChannelCapability::ReadReceipts,
            ChannelCapability::TypingIndicators,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        let homeserver = match &self.homeserver_url {
            Some(url) => url,
            None => {
                warn!("Matrix channel enabled but no homeserver_url configured");
                return Ok(());
            }
        };

        if self.access_token.is_none() {
            warn!("Matrix channel enabled but no access_token configured");
            return Ok(());
        }

        let user_id = self.user_id.as_deref().unwrap_or("(unknown)");
        info!(
            homeserver = %homeserver,
            user_id = %user_id,
            encryption = self.encryption.unwrap_or(false),
            "Matrix channel starting"
        );

        if self.encryption == Some(true) {
            // E2EE requested: the crypto engine is stubbed (needs a
            // vodozemac-class crate behind MatrixCryptoProvider); rooms with
            // encryption enabled will not decrypt until it is wired.
            warn!(
                "Matrix encryption is enabled in config but olm/megolm crypto is \
                 stubbed in this build (vodozemac-class crate required)"
            );
        }

        // Integration point: the /sync long-poll loop resumes from
        // `MatrixStateCache::sync_token`, requests MSC4222 `state_after`
        // (retrying via `without_matrix_state_after_sync_param` on servers
        // that reject it), dispatches `m.room.message` events, and resolves
        // `m.reaction` events on approval prompts through
        // `MatrixStateCache::resolve_approval_reaction`.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("Matrix channel stopping");
            // Integration point: cancel the sync loop task and persist the
            // final sync token via MatrixStateCache::set_sync_token.
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let homeserver = self
            .homeserver_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Matrix homeserver_url not configured"))?;

        let access_token = self
            .access_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Matrix access_token not configured"))?;

        // `to` is a Matrix room ID (e.g. "!abc123:matrix.org"); prefixes are
        // stripped with the id case preserved.
        let room_id = normalize_matrix_messaging_target(to)
            .ok_or_else(|| anyhow::anyhow!("Matrix target is empty"))?;
        let txn_id = uuid::Uuid::new_v4().to_string();

        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            homeserver.trim_end_matches('/'),
            urlencoded(&room_id),
            txn_id,
        );

        let body = serde_json::json!({
            "msgtype": "m.text",
            "body": message,
        });

        info!(room_id = %room_id, "Matrix: sending message");

        let resp = self
            .client
            .put(&url)
            .bearer_token(access_token)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Matrix send failed ({}): {}", status, text);
        }

        Ok(())
    }
}

/// Percent-encode a Matrix room ID for use in URL paths.
fn urlencoded(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Helper trait to add bearer_token to reqwest::RequestBuilder.
trait BearerToken {
    fn bearer_token(self, token: &str) -> Self;
}

impl BearerToken for reqwest::RequestBuilder {
    fn bearer_token(self, token: &str) -> Self {
        self.header("Authorization", format!("Bearer {}", token))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── Target ids / room-id case preservation ──────────────────────────

    #[test]
    fn matrix_target_identity_preserves_room_id_case() {
        assert_eq!(
            resolve_matrix_target_identity("ROOM:!AbC123:Matrix.Org"),
            Some(MatrixTarget::Room("!AbC123:Matrix.Org".to_string()))
        );
        assert_eq!(
            resolve_matrix_target_identity("matrix:user:@Alice:server"),
            Some(MatrixTarget::User("@Alice:server".to_string()))
        );
        assert_eq!(
            resolve_matrix_target_identity("@bob:server"),
            Some(MatrixTarget::User("@bob:server".to_string()))
        );
        assert_eq!(
            resolve_matrix_target_identity("channel:#room:server"),
            Some(MatrixTarget::Room("#room:server".to_string()))
        );
        assert_eq!(resolve_matrix_target_identity("  "), None);
        assert_eq!(
            normalize_matrix_messaging_target("room:!CaseKept:HS"),
            Some("!CaseKept:HS".to_string())
        );
    }

    // ── Mentions ────────────────────────────────────────────────────────

    #[test]
    fn matrix_collects_mxid_room_and_bracketed_mentions() {
        let text = "ping @alice:example.org and @[Ada Lovelace] plus @room now";
        let mentions = collect_mention_candidates(text);
        assert_eq!(mentions.len(), 3);
        assert_eq!(mentions[0].kind, MentionKind::User("@alice:example.org".to_string()));
        assert_eq!(mentions[1].kind, MentionKind::DisplayName("Ada Lovelace".to_string()));
        assert_eq!(mentions[1].raw, "@[Ada Lovelace]");
        assert_eq!(mentions[2].kind, MentionKind::Room);
    }

    #[test]
    fn matrix_mentions_trim_suffix_and_respect_boundaries() {
        let mentions = collect_mention_candidates("(cc @alice:example.org).");
        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].raw, "@alice:example.org");
        // Email-like tokens (letter before @) are not mentions.
        assert!(collect_mention_candidates("mail me at foo@bar:baz ok").is_empty());
        // Bare @word without a server is not a user mention.
        assert!(collect_mention_candidates("hi @somebody there").is_empty());
    }

    #[test]
    fn matrix_escaped_mentions_masked_outside_code() {
        let masked = mask_escaped_mentions(r"say \@alice:x but `\@code` stays");
        assert!(!masked.contains(r"\@alice"));
        assert!(masked.contains(r"`\@code`"));
        assert!(collect_mention_candidates(&masked)
            .iter()
            .all(|m| !m.raw.contains("alice")));
        assert_eq!(
            restore_escaped_mentions(&mask_escaped_mentions(r"\@x")),
            "@x"
        );
    }

    // ── Markdown tables → bullets ───────────────────────────────────────

    #[test]
    fn matrix_tables_convert_to_bullets() {
        let text = "intro\n\n| Name | Role |\n| --- | :--- |\n| Ada | Engineer |\n| Grace | Admiral |\n\ntail";
        let converted = markdown_tables_to_bullets(text);
        assert!(converted.contains("- **Name:** Ada — **Role:** Engineer"));
        assert!(converted.contains("- **Name:** Grace — **Role:** Admiral"));
        assert!(!converted.contains("| --- |"));
        assert!(converted.starts_with("intro"));
        assert!(converted.ends_with("tail"));
    }

    #[test]
    fn matrix_non_table_text_unchanged() {
        let text = "no tables here\n| lonely pipe line\nplain";
        assert_eq!(markdown_tables_to_bullets(text), text);
    }

    // ── Presentation metadata ───────────────────────────────────────────

    #[test]
    fn matrix_presentation_content_roundtrip() {
        let content = build_matrix_presentation_content(serde_json::json!({
            "blocks": [{ "kind": "button", "label": "Go" }]
        }));
        assert_eq!(content["version"], 1);
        assert_eq!(content["type"], MATRIX_PRESENTATION_TYPE);
        let extra = serde_json::json!({ MATRIX_PRESENTATION_KEY: content });
        assert!(resolve_matrix_presentation_content(&extra).is_some());
        // Wrong version/type rejected.
        let bad = serde_json::json!({ MATRIX_PRESENTATION_KEY: { "version": 2, "type": MATRIX_PRESENTATION_TYPE } });
        assert!(resolve_matrix_presentation_content(&bad).is_none());
        // Empty presentation-only text falls back to "---".
        assert_eq!(resolve_matrix_payload_text("  ", true), "---");
        assert_eq!(resolve_matrix_payload_text("body", true), "body");
        assert_eq!(resolve_matrix_payload_text("", false), "");
    }

    // ── MSC4222 ─────────────────────────────────────────────────────────

    #[test]
    fn matrix_state_after_param_stripped_only_from_sync() {
        let url = format!(
            "https://hs.example/_matrix/client/v3/sync?since=s1&{}=true",
            MATRIX_STATE_AFTER_SYNC_PARAM
        );
        let stripped = without_matrix_state_after_sync_param(&url);
        assert!(!stripped.contains(MATRIX_STATE_AFTER_SYNC_PARAM));
        assert!(stripped.contains("since=s1"));
        // Non-sync URLs untouched.
        let other = format!("https://hs.example/_matrix/client/v3/rooms?{}=true", MATRIX_STATE_AFTER_SYNC_PARAM);
        assert_eq!(without_matrix_state_after_sync_param(&other), other);
        // Not a URL → passthrough.
        assert_eq!(without_matrix_state_after_sync_param("nope"), "nope");
        // state_after wins over legacy state.
        let after = serde_json::json!(["a"]);
        let legacy = serde_json::json!(["l"]);
        assert_eq!(select_sync_state_events(Some(&after), Some(&legacy)), Some(&after));
        assert_eq!(select_sync_state_events(None, Some(&legacy)), Some(&legacy));
    }

    // ── Sync phases ─────────────────────────────────────────────────────

    #[test]
    fn matrix_sync_phase_classification() {
        assert!(is_matrix_ready_sync_phase(MatrixSyncPhase::Prepared));
        assert!(is_matrix_ready_sync_phase(MatrixSyncPhase::Catchup));
        assert!(is_matrix_disconnected_sync_phase(MatrixSyncPhase::Error));
        // ERROR can recover during initial sync; only STOPPED is terminal.
        assert!(!is_matrix_terminal_sync_phase(MatrixSyncPhase::Error));
        assert!(is_matrix_terminal_sync_phase(MatrixSyncPhase::Stopped));
    }

    // ── SQLite state cache ──────────────────────────────────────────────

    #[test]
    fn matrix_state_cache_sync_tokens_and_sidecars() {
        let cache = MatrixStateCache::open_in_memory().unwrap();
        assert_eq!(cache.sync_token("default").unwrap(), None);
        cache.set_sync_token("default", "s72594_4483", 1).unwrap();
        cache.set_sync_token("default", "s72595_4484", 2).unwrap();
        assert_eq!(cache.sync_token("default").unwrap().as_deref(), Some("s72595_4484"));
        // Crypto sidecars.
        cache.set_crypto_sidecar("default", "backup_version", "3").unwrap();
        assert_eq!(
            cache.crypto_sidecar("default", "backup_version").unwrap().as_deref(),
            Some("3")
        );
        // Recovery-key import never overwrites a different existing key.
        assert!(cache.import_recovery_key("default", "EsT1 aBcD").unwrap());
        assert!(cache.import_recovery_key("default", "EsT1 aBcD").unwrap());
        assert!(!cache.import_recovery_key("default", "OTHER KEY").unwrap());
        assert_eq!(
            cache.crypto_sidecar("default", "recovery_key").unwrap().as_deref(),
            Some("EsT1 aBcD")
        );
    }

    #[test]
    fn matrix_approval_reactions_persist_and_expire() {
        let cache = MatrixStateCache::open_in_memory().unwrap();
        let allowed = [ApprovalDecision::AllowOnce, ApprovalDecision::Deny];
        cache
            .register_approval_reaction_target("!room:hs", "$evt", "appr-1", &allowed, 0)
            .unwrap();
        // A ✅ reaction resolves (also "after restart": store is durable state).
        let resolved = cache
            .resolve_approval_reaction("!room:hs", "$evt", "✅", 100)
            .unwrap()
            .unwrap();
        assert_eq!(resolved, ("appr-1".to_string(), ApprovalDecision::AllowOnce));
        // Disallowed decision emoji resolves to nothing.
        assert!(cache
            .resolve_approval_reaction("!room:hs", "$evt", "♾️", 100)
            .unwrap()
            .is_none());
        // TTL expiry (24 h).
        assert!(cache
            .resolve_approval_reaction("!room:hs", "$evt", "✅", APPROVAL_REACTION_TARGET_TTL_MS + 1)
            .unwrap()
            .is_none());
        // Unregister removes the target.
        cache
            .register_approval_reaction_target("!room:hs", "$evt2", "appr-2", &allowed, 0)
            .unwrap();
        cache.unregister_approval_reaction_target("!room:hs", "$evt2").unwrap();
        assert!(cache
            .resolve_approval_reaction("!room:hs", "$evt2", "✅", 1)
            .unwrap()
            .is_none());
    }

    // ── Approval reactions + hint ───────────────────────────────────────

    #[test]
    fn matrix_reaction_hint_and_decisions() {
        let hint = build_approval_reaction_hint(&[
            ApprovalDecision::AllowOnce,
            ApprovalDecision::AllowAlways,
            ApprovalDecision::Deny,
        ])
        .unwrap();
        assert_eq!(hint, "React here: ✅ Allow once, ♾️ Allow always, ❌ Deny");
        assert!(build_approval_reaction_hint(&[]).is_none());
        assert_eq!(
            resolve_approval_reaction_decision("❌", &[ApprovalDecision::Deny]),
            Some(ApprovalDecision::Deny)
        );
        assert_eq!(resolve_approval_reaction_decision("👍", &[ApprovalDecision::Deny]), None);
        assert_eq!(resolve_approval_reaction_decision("", &[ApprovalDecision::Deny]), None);
    }

    // ── Exec approvals ──────────────────────────────────────────────────

    #[test]
    fn matrix_approvers_resolution_and_normalization() {
        assert_eq!(
            normalize_matrix_approver_id("user:@Admin:HS.example"),
            Some("@admin:hs.example".to_string())
        );
        assert_eq!(normalize_matrix_approver_id("*"), Some("*".to_string()));
        assert_eq!(normalize_matrix_approver_id("  "), None);
        // Explicit approvers win; wildcard dropped for exec approvals.
        let explicit = vec!["@a:hs".to_string(), "*".to_string(), "@A:hs".to_string()];
        let allow_from = vec!["@dm:hs".to_string()];
        assert_eq!(
            resolve_exec_approval_approvers(Some(&explicit), Some(&allow_from)),
            vec!["@a:hs".to_string()]
        );
        // Fallback to dm.allowFrom.
        assert_eq!(
            resolve_exec_approval_approvers(None, Some(&allow_from)),
            vec!["@dm:hs".to_string()]
        );
        assert!(is_exec_approval_client_enabled(None, 1));
        assert!(!is_exec_approval_client_enabled(None, 0));
        assert!(!is_exec_approval_client_enabled(Some(false), 3));
    }

    #[test]
    fn matrix_approval_message_carries_exec_metadata() {
        let meta = ExecApprovalMetadata {
            approval_id: "abc123".to_string(),
            approval_kind: "exec".to_string(),
            command: Some("rm -rf ./build".to_string()),
            agent_id: Some("main".to_string()),
            session_key: Some("matrix:!r:hs".to_string()),
        };
        let body = build_exec_approval_message(&meta, &[ApprovalDecision::AllowOnce, ApprovalDecision::Deny]);
        assert!(body.contains("Exec Approval Required"));
        assert!(body.contains("```\nrm -rf ./build\n```"));
        assert!(body.contains("agent: main"));
        assert!(body.contains("session: matrix:!r:hs"));
        assert!(body.contains("id: abc123"));
        assert!(body.contains("React here: ✅ Allow once, ❌ Deny"));
        assert_eq!(ExecApprovalMetadata::kind_from_id("plugin:x"), "plugin");
        assert_eq!(ExecApprovalMetadata::kind_from_id("e-1"), "exec");
    }

    #[test]
    fn matrix_approval_chunked_fallback() {
        // Fits → single chunk.
        assert_eq!(chunk_approval_body("short", 100), vec!["short".to_string()]);
        // Splits on line boundaries under the limit.
        // Lines are 61/61/10 chars including their newlines, so with a 70-char
        // limit no two of them can share a chunk — each lands on its own.
        // (Packing "b" and "c" together would be 71 chars, over the limit.)
        let body = format!("{}\n{}\n{}", "a".repeat(60), "b".repeat(60), "c".repeat(10));
        let chunks = chunk_approval_body(&body, 70);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.chars().count() <= 70));
        assert!(chunks[2].contains(&"c".repeat(10)));
        // A single oversized line splits hard.
        let chunks = chunk_approval_body(&"x".repeat(250), 100);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[2].chars().count(), 50);
    }

    #[test]
    fn matrix_approval_origin_target_thread_targeting() {
        // Matrix turn source wins and carries its thread.
        let origin = ApprovalRequestOrigin {
            turn_source_channel: Some("Matrix".to_string()),
            turn_source_to: Some("room:!Origin:hs".to_string()),
            turn_source_thread_id: Some("$thread".to_string()),
            session_to: Some("!Session:hs".to_string()),
            session_thread_id: None,
        };
        let target = resolve_approval_origin_target(&origin).unwrap();
        assert_eq!(target.to, MatrixTarget::Room("!Origin:hs".to_string()));
        assert_eq!(target.thread_id.as_deref(), Some("$thread"));
        // Non-Matrix turn source falls back to the session conversation.
        let origin = ApprovalRequestOrigin {
            turn_source_channel: Some("telegram".to_string()),
            turn_source_to: Some("12345".to_string()),
            session_to: Some("!Session:hs".to_string()),
            session_thread_id: Some("$sthread".to_string()),
            ..Default::default()
        };
        let target = resolve_approval_origin_target(&origin).unwrap();
        assert_eq!(target.to, MatrixTarget::Room("!Session:hs".to_string()));
        assert_eq!(target.thread_id.as_deref(), Some("$sthread"));
        // Nothing resolvable → None.
        assert!(resolve_approval_origin_target(&ApprovalRequestOrigin::default()).is_none());
    }

    // ── Draft stream (streaming tool progress) ──────────────────────────

    #[test]
    fn matrix_draft_stream_create_edit_coalesce() {
        let mut stream = MatrixDraftStream::new(true);
        // First update creates (live marker on).
        let action = stream.prepare_update("Running tests…", 0, false);
        assert_eq!(
            action,
            DraftStreamAction::Create { text: "Running tests…".to_string(), live: true }
        );
        stream.on_send_result("Running tests…", Some("$evt1".to_string()), true, 0);
        // Unchanged content dedupes.
        assert_eq!(stream.prepare_update("Running tests…", 2_000, false), DraftStreamAction::Skip);
        // Throttle window coalesces edits (<1 s since last flush).
        assert_eq!(stream.prepare_update("Running tests… 50%", 500, false), DraftStreamAction::Skip);
        // Past the throttle the update edits in place.
        let action = stream.prepare_update("Running tests… 50%", 1_100, false);
        assert_eq!(
            action,
            DraftStreamAction::Edit {
                event_id: "$evt1".to_string(),
                text: "Running tests… 50%".to_string(),
                live: true
            }
        );
        stream.on_send_result("Running tests… 50%", None, true, 1_100);
        // force=true (flush) bypasses the throttle.
        assert!(matches!(
            stream.prepare_update("done", 1_200, true),
            DraftStreamAction::Edit { .. }
        ));
    }

    #[test]
    fn matrix_draft_stream_limit_and_finalize() {
        let mut stream = MatrixDraftStream::with_limit(true, 10);
        stream.on_send_result("short", Some("$e".to_string()), true, 0);
        // Exceeding the single-event limit stops the stream and forces
        // normal final delivery.
        assert_eq!(
            stream.prepare_update(&"y".repeat(50), 2_000, false),
            DraftStreamAction::ExceededLimit
        );
        assert!(stream.must_deliver_final_normally());
        // Finalize clears the MSC4357 live marker with one last edit.
        let mut stream = MatrixDraftStream::new(true);
        stream.on_send_result("final text", Some("$evt".to_string()), true, 0);
        let finalize = stream.prepare_finalize_live().unwrap();
        assert_eq!(
            finalize,
            DraftStreamAction::Edit {
                event_id: "$evt".to_string(),
                text: "final text".to_string(),
                live: false
            }
        );
        // Finalize is one-shot.
        assert!(stream.prepare_finalize_live().is_none());
        assert!(stream.matches_prepared_text("final text"));
        // Failed create marks the stream failed.
        let mut stream = MatrixDraftStream::new(false);
        stream.on_send_result("x", None, false, 0);
        assert!(stream.must_deliver_final_normally());
        assert_eq!(stream.prepare_update("more", 5_000, true), DraftStreamAction::Skip);
        // Reset clears state for the next block.
        stream.reset();
        assert!(!stream.must_deliver_final_normally());
        assert!(matches!(
            stream.prepare_update("next block", 0, false),
            DraftStreamAction::Create { .. }
        ));
    }

    // ── E2EE surface ────────────────────────────────────────────────────

    #[test]
    fn matrix_encryption_setup_mode_resolution() {
        assert_eq!(
            resolve_encryption_setup_mode(false, false, false),
            EncryptionSetupMode::VerifyOnly
        );
        assert_eq!(
            resolve_encryption_setup_mode(true, false, false),
            EncryptionSetupMode::Bootstrap { force_reset_cross_signing: false }
        );
        assert_eq!(
            resolve_encryption_setup_mode(false, true, false),
            EncryptionSetupMode::Bootstrap { force_reset_cross_signing: false }
        );
        // Forced cross-signing reset always bootstraps.
        assert_eq!(
            resolve_encryption_setup_mode(false, false, true),
            EncryptionSetupMode::Bootstrap { force_reset_cross_signing: true }
        );
    }

    #[test]
    fn matrix_bootstrap_gating_and_stubbed_crypto() {
        // Only newly-enabled encryption bootstraps.
        assert!(should_bootstrap_new_encrypted_account(None, Some(true)));
        assert!(should_bootstrap_new_encrypted_account(Some(false), Some(true)));
        assert!(!should_bootstrap_new_encrypted_account(Some(true), Some(true)));
        assert!(!should_bootstrap_new_encrypted_account(None, Some(false)));
        // Not attempted when gating says no.
        let result = maybe_bootstrap_new_encrypted_account(
            Some(true),
            Some(true),
            &UnavailableCryptoProvider,
            false,
        );
        assert!(!result.attempted);
        // Attempted with the stub → structured error naming vodozemac.
        let result = maybe_bootstrap_new_encrypted_account(
            None,
            Some(true),
            &UnavailableCryptoProvider,
            true,
        );
        assert!(result.attempted);
        assert!(!result.success);
        assert!(result.error.unwrap().contains("vodozemac"));
    }

    #[test]
    fn matrix_verification_state_machine() {
        let summary = VerificationSummary {
            phase: VerificationPhase::Ready,
            has_sas: false,
            chosen_method: None,
            completed: false,
        };
        assert!(is_verification_ready_for_sas(&summary));
        assert!(should_start_sas_verification(&summary));
        assert!(verification_sas_wait_failure(&summary, "show emoji").is_none());
        // Completed without SAS while waiting is a hard failure.
        let done = VerificationSummary { completed: true, ..summary.clone() };
        let failure = verification_sas_wait_failure(&done, "show emoji").unwrap();
        assert!(failure.contains("completed without SAS"));
        // Started with a non-SAS method fails the wait.
        let qr = VerificationSummary {
            phase: VerificationPhase::Started,
            has_sas: false,
            chosen_method: Some("m.qr_code.show.v1".to_string()),
            completed: false,
        };
        assert!(verification_sas_wait_failure(&qr, "confirm").unwrap().contains("without SAS"));
        // SAS chosen: no failure, no restart.
        let sas = VerificationSummary {
            phase: VerificationPhase::Started,
            has_sas: true,
            chosen_method: Some("m.sas.v1".to_string()),
            completed: false,
        };
        assert!(verification_sas_wait_failure(&sas, "confirm").is_none());
        assert!(!should_start_sas_verification(&sas));
        // Cancelled stops the wait quietly (caller raises the cancel error).
        let cancelled = VerificationSummary {
            phase: VerificationPhase::Cancelled,
            has_sas: false,
            chosen_method: None,
            completed: false,
        };
        assert!(verification_sas_wait_failure(&cancelled, "x").is_none());
    }

    #[test]
    fn matrix_state_paths_resolution() {
        let paths = resolve_matrix_state_paths(Path::new("/state"), "work");
        assert_eq!(paths.root_dir, PathBuf::from("/state/matrix/work"));
        assert_eq!(paths.sqlite_db, PathBuf::from("/state/matrix/work/matrix.sqlite"));
        assert_eq!(paths.crypto_store_dir, PathBuf::from("/state/matrix/work/crypto"));
        assert_eq!(
            paths.legacy_recovery_key_path,
            PathBuf::from("/state/matrix/work/recovery-key.json")
        );
        let default_paths = resolve_matrix_state_paths(Path::new("/state"), "  ");
        assert_eq!(default_paths.root_dir, PathBuf::from("/state/matrix/default"));
    }

    // ── DM routing ──────────────────────────────────────────────────────

    #[test]
    fn matrix_dm_routing_prefers_two_person_room_over_stale_m_direct() {
        assert!(is_strict_direct_membership(
            Some("@bot:hs"),
            Some("@user:hs"),
            &["@bot:hs".to_string(), "@user:hs".to_string()]
        ));
        assert!(!is_strict_direct_membership(
            Some("@bot:hs"),
            Some("@user:hs"),
            &["@bot:hs".to_string(), "@user:hs".to_string(), "@third:hs".to_string()]
        ));
        assert!(!is_strict_direct_membership(None, Some("@user:hs"), &[]));

        let stale_m_direct = DmRoomEvidence {
            room_id: "!old:hs".to_string(),
            strict_two_person: false,
            listed_in_m_direct: true,
        };
        let live_dm = DmRoomEvidence {
            room_id: "!live:hs".to_string(),
            strict_two_person: true,
            listed_in_m_direct: false,
        };
        // Strict two-person room beats the stale m.direct entry.
        let stale_then_live = [stale_m_direct.clone(), live_dm.clone()];
        let chosen = resolve_preferred_dm_room(&stale_then_live).unwrap();
        assert_eq!(chosen.room_id, "!live:hs");
        // Strict + m.direct is the best evidence when present.
        let both = DmRoomEvidence {
            room_id: "!both:hs".to_string(),
            strict_two_person: true,
            listed_in_m_direct: true,
        };
        let all_three = [stale_m_direct.clone(), live_dm, both];
        let chosen = resolve_preferred_dm_room(&all_three).unwrap();
        assert_eq!(chosen.room_id, "!both:hs");
        // m.direct is the last resort.
        let stale_only = [stale_m_direct];
        let chosen = resolve_preferred_dm_room(&stale_only).unwrap();
        assert_eq!(chosen.room_id, "!old:hs");
        assert!(resolve_preferred_dm_room(&[]).is_none());
    }

    // ── Extension config ────────────────────────────────────────────────

    #[test]
    fn matrix_extension_config_parses() {
        let mut config = Config::default();
        config.channels.extensions.insert(
            "matrix".to_string(),
            serde_json::json!({
                "enabled": true,
                "homeserver": "https://hs.example",
                "accessToken": "syt_x",
                "userId": "@bot:hs.example",
                "encryption": true,
                "execApprovals": { "enabled": true, "approvers": ["@admin:hs.example"] },
                "dm": { "allowFrom": ["@admin:hs.example"] }
            }),
        );
        let ext = resolve_matrix_extension_config(&config).unwrap();
        assert_eq!(ext.effective_homeserver(), Some("https://hs.example"));
        assert_eq!(ext.encryption, Some(true));
        let approvals = ext.exec_approvals.unwrap();
        assert_eq!(
            resolve_exec_approval_approvers(
                approvals.approvers.as_deref(),
                ext.dm.and_then(|d| d.allow_from).as_deref()
            ),
            vec!["@admin:hs.example".to_string()]
        );
        assert!(resolve_matrix_extension_config(&Config::default()).is_none());
    }
}
