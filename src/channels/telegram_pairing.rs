//! Telegram `/login` pairing flow (port of the OpenClaw v2026.7.1 Codex
//! channel-login behavior in `bot-native-commands.ts` + the shared
//! channel-login runtime): owner-gated, DM-only pairing with per-chat flow
//! reservation and expiring device codes.
//!
//! Seam: the actual provider device-login exchange lives behind
//! [`LoginFlowRunner`]; the default runner issues a local pairing code and
//! reports it for out-of-band completion, because the port has no Codex
//! app-server runtime yet. All Telegram-side gating, reservation, expiry and
//! messaging match upstream.

use rand::Rng;
use std::collections::HashMap;
use std::sync::Mutex;

/// Pairing code validity window (10 minutes).
pub const TELEGRAM_LOGIN_CODE_TTL_MS: u64 = 10 * 60 * 1000;

/// Providers accepted by `/login` (upstream: only `codex`, also the default).
pub fn resolve_login_provider(arg: Option<&str>) -> Option<&'static str> {
    match arg.map(|a| a.trim().to_lowercase()) {
        None => Some("codex"),
        Some(value) if value.is_empty() || value == "codex" => Some("codex"),
        Some(_) => None,
    }
}

/// Gate decision for a `/login` request, with the upstream user-facing text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginGateDecision {
    /// Proceed with the pairing flow.
    Allowed,
    /// Sender is not a configured owner (or no owner allowlist configured).
    NotOwner,
    /// Group chat — login codes are DM-only for safety.
    GroupChat,
    /// Unsupported provider argument.
    UnsupportedProvider,
}

impl LoginGateDecision {
    /// The reply text for a rejected request (upstream strings).
    pub fn rejection_text(&self) -> Option<&'static str> {
        match self {
            Self::Allowed => None,
            Self::NotOwner => {
                Some("Only a configured owner can start login pairing from Telegram.")
            }
            Self::GroupChat => Some(
                "For safety, login codes are only sent in a private chat with this bot. \
                 DM this bot `/login codex` to pair.",
            ),
            Self::UnsupportedProvider => Some("Unsupported login provider. Use `/login codex`."),
        }
    }
}

/// Evaluates the `/login` gate: owner allowlist must be configured AND the
/// sender must be an owner; groups are rejected; the provider must be known.
pub fn evaluate_login_gate(params: LoginGateParams) -> LoginGateDecision {
    if !params.owner_allowlist_configured || !params.sender_is_owner {
        return LoginGateDecision::NotOwner;
    }
    if params.is_group {
        return LoginGateDecision::GroupChat;
    }
    if resolve_login_provider(params.provider_arg).is_none() {
        return LoginGateDecision::UnsupportedProvider;
    }
    LoginGateDecision::Allowed
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LoginGateParams<'a> {
    pub owner_allowlist_configured: bool,
    pub sender_is_owner: bool,
    pub is_group: bool,
    pub provider_arg: Option<&'a str>,
}

/// Flow key: one active pairing flow per (account, chat, thread, provider).
pub fn build_login_flow_key(
    account_id: &str,
    chat_id: &str,
    thread_id: Option<i64>,
    provider: &str,
) -> String {
    match thread_id {
        Some(thread) => format!("{account_id}:{chat_id}:{thread}:{provider}"),
        None => format!("{account_id}:{chat_id}:{provider}"),
    }
}

/// Outcome of a flow reservation attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowReservation {
    /// A code is already active for this chat — upstream refuses a second.
    AlreadyActive,
    /// Flow reserved; a new device code was issued.
    Reserved { code: String, expires_at_ms: u64 },
}

/// The message shown when a flow is already active (upstream string).
pub const LOGIN_FLOW_ALREADY_ACTIVE_TEXT: &str =
    "A login code is already active for this chat. Complete it, or wait for it to \
     expire before requesting a new one.";

/// Seam for the provider device-login exchange.
pub trait LoginFlowRunner: Send + Sync {
    /// Runs (or begins) the device login for `provider`, returning the
    /// user-visible pairing instructions.
    fn run(&self, provider: &str, code: &str) -> String;
}

/// Default runner: reports the local pairing code for out-of-band completion.
pub struct LocalCodeLoginRunner;

impl LoginFlowRunner for LocalCodeLoginRunner {
    fn run(&self, provider: &str, code: &str) -> String {
        format!(
            "Pairing code for {provider}: {code}\nEnter this code in the {provider} \
             device-login prompt within 10 minutes."
        )
    }
}

/// In-memory store of active pairing flows with expiry.
#[derive(Default)]
pub struct PairingFlowStore {
    flows: Mutex<HashMap<String, u64>>, // flow_key -> expires_at_ms
}

impl PairingFlowStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn generate_code() -> String {
        let mut rng = rand::thread_rng();
        let letters: String = (0..4)
            .map(|_| (b'A' + rng.gen_range(0..26)) as char)
            .collect();
        let digits: String = (0..4).map(|_| rng.gen_range(0..10).to_string()).collect();
        format!("{letters}-{digits}")
    }

    /// Reserves a flow. Expired reservations are reclaimed; an unexpired one
    /// refuses a second code (upstream behavior).
    pub fn reserve(&self, flow_key: &str, now_ms: u64) -> FlowReservation {
        let mut flows = self.flows.lock().unwrap();
        flows.retain(|_, &mut expires| expires > now_ms);
        if flows.contains_key(flow_key) {
            return FlowReservation::AlreadyActive;
        }
        let expires_at_ms = now_ms + TELEGRAM_LOGIN_CODE_TTL_MS;
        flows.insert(flow_key.to_string(), expires_at_ms);
        FlowReservation::Reserved {
            code: Self::generate_code(),
            expires_at_ms,
        }
    }

    /// Releases a flow (completed or aborted).
    pub fn release(&self, flow_key: &str) {
        self.flows.lock().unwrap().remove(flow_key);
    }

    pub fn active_count(&self, now_ms: u64) -> usize {
        let mut flows = self.flows.lock().unwrap();
        flows.retain(|_, &mut expires| expires > now_ms);
        flows.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_resolution() {
        assert_eq!(resolve_login_provider(None), Some("codex"));
        assert_eq!(resolve_login_provider(Some("codex")), Some("codex"));
        assert_eq!(resolve_login_provider(Some(" CODEX ")), Some("codex"));
        assert_eq!(resolve_login_provider(Some("")), Some("codex"));
        assert_eq!(resolve_login_provider(Some("gpt")), None);
    }

    #[test]
    fn login_gate_owner_and_dm_only() {
        // Not owner-configured → rejected even for the sender claiming owner.
        assert_eq!(
            evaluate_login_gate(LoginGateParams {
                owner_allowlist_configured: false,
                sender_is_owner: true,
                ..Default::default()
            }),
            LoginGateDecision::NotOwner
        );
        // Group chat → DM-only message.
        assert_eq!(
            evaluate_login_gate(LoginGateParams {
                owner_allowlist_configured: true,
                sender_is_owner: true,
                is_group: true,
                ..Default::default()
            }),
            LoginGateDecision::GroupChat
        );
        // Bad provider.
        assert_eq!(
            evaluate_login_gate(LoginGateParams {
                owner_allowlist_configured: true,
                sender_is_owner: true,
                provider_arg: Some("claude"),
                ..Default::default()
            }),
            LoginGateDecision::UnsupportedProvider
        );
        // Happy path.
        let allowed = evaluate_login_gate(LoginGateParams {
            owner_allowlist_configured: true,
            sender_is_owner: true,
            provider_arg: Some("codex"),
            ..Default::default()
        });
        assert_eq!(allowed, LoginGateDecision::Allowed);
        assert!(allowed.rejection_text().is_none());
    }

    #[test]
    fn flow_reservation_refuses_second_active_code() {
        let store = PairingFlowStore::new();
        let key = build_login_flow_key("default", "123", None, "codex");
        let first = store.reserve(&key, 1_000);
        assert!(matches!(first, FlowReservation::Reserved { .. }));
        assert_eq!(store.reserve(&key, 2_000), FlowReservation::AlreadyActive);
        // Different thread → separate flow.
        let topic_key = build_login_flow_key("default", "123", Some(9), "codex");
        assert!(matches!(
            store.reserve(&topic_key, 2_000),
            FlowReservation::Reserved { .. }
        ));
    }

    #[test]
    fn expired_flow_reclaimed() {
        let store = PairingFlowStore::new();
        let key = build_login_flow_key("default", "123", None, "codex");
        store.reserve(&key, 0);
        // After TTL the reservation expires and a new code can be issued.
        let later = TELEGRAM_LOGIN_CODE_TTL_MS + 1;
        assert!(matches!(
            store.reserve(&key, later),
            FlowReservation::Reserved { .. }
        ));
    }

    #[test]
    fn release_frees_flow() {
        let store = PairingFlowStore::new();
        let key = build_login_flow_key("default", "5", None, "codex");
        store.reserve(&key, 0);
        store.release(&key);
        assert!(matches!(store.reserve(&key, 1), FlowReservation::Reserved { .. }));
    }

    #[test]
    fn code_shape() {
        let store = PairingFlowStore::new();
        let key = build_login_flow_key("a", "b", None, "codex");
        if let FlowReservation::Reserved { code, .. } = store.reserve(&key, 0) {
            assert_eq!(code.len(), 9);
            assert_eq!(code.chars().nth(4), Some('-'));
        } else {
            panic!("expected reservation");
        }
    }
}
