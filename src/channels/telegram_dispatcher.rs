//! Telegram polling dispatcher: durable-ingress polling loop over the update
//! spool, per-topic/DM drain lanes, turn adoption, watchdog keyed to
//! getUpdates liveness, 409 conflict recovery, outbound drain after
//! reconnect, native command routing (`/steer`, `/tell`, `/login`,
//! `/command@TargetBot`), mention → assistant identity binding, reply-chain
//! hydration, and account-scoped topic → agent routing.
//!
//! Ports the observable behavior of OpenClaw v2026.7.1
//! `polling-session.ts`, `telegram-ingress-worker.ts`,
//! `bot-message-dispatch.ts`, `bot-native-commands.ts` (steer/tell/login
//! surface), and `conversation-route.ts` topic-agent scoping.
//!
//! Seams (documented, telegram side fully implemented):
//! - agent turns run through `crate::agents::run_single_message` (the port
//!   has no streaming-run registry yet); `/steer` and `/tell` deliver through
//!   [`ActiveRunSink`], whose default queues guidance as the next turn.
//! - `/login` provider exchange runs through
//!   [`super::telegram_pairing::LoginFlowRunner`].

use super::telegram::{send_transcript_echo, TelegramApi};
use super::telegram_commands::{should_allow_group_command, GroupCommandGate};
use super::telegram_net::{
    is_get_updates_conflict_error, resolve_polling_stall_threshold_ms, TelegramApiError,
    TelegramPollingLivenessTracker,
};
use super::telegram_pairing::{
    build_login_flow_key, evaluate_login_gate, FlowReservation, LocalCodeLoginRunner,
    LoginFlowRunner, LoginGateDecision, LoginGateParams, PairingFlowStore,
    LOGIN_FLOW_ALREADY_ACTIVE_TEXT,
};
use super::telegram_spool::{SpoolStatus, SpooledUpdate, TelegramUpdateSpool};

use crate::config::{Config, TelegramAccountConfig};

use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

// ============================================================================
// Lane keying: per-topic / per-DM sequential, cross-lane concurrent
// ============================================================================

/// Processing lane: one per (chat, forum topic). Updates in a lane drain
/// sequentially; distinct lanes drain concurrently.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LaneKey {
    pub chat_id: String,
    pub thread_id: Option<i64>,
}

fn update_message<'a>(update: &'a serde_json::Value) -> Option<&'a serde_json::Value> {
    update
        .get("message")
        .or_else(|| update.get("edited_message"))
        .or_else(|| update.get("channel_post"))
        .or_else(|| update.get("edited_channel_post"))
}

/// Lane for an update: chat id + forum topic thread id (topic lanes only for
/// forum topic messages, mirroring upstream's topic-lane gating).
pub fn lane_key_for_update(update: &serde_json::Value) -> LaneKey {
    let Some(msg) = update_message(update) else {
        return LaneKey {
            chat_id: "__control".to_string(),
            thread_id: None,
        };
    };
    let chat_id = msg
        .pointer("/chat/id")
        .and_then(|v| v.as_i64())
        .map(|id| id.to_string())
        .unwrap_or_else(|| "__unknown".to_string());
    let is_topic = msg
        .get("is_topic_message")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let thread_id = if is_topic {
        msg.get("message_thread_id").and_then(|v| v.as_i64())
    } else {
        None
    };
    LaneKey { chat_id, thread_id }
}

/// Session key for a lane (matches the channel target syntax).
pub fn lane_session_key(lane: &LaneKey) -> String {
    match lane.thread_id {
        Some(thread) => format!("telegram:{}:topic:{}", lane.chat_id, thread),
        None => format!("telegram:{}", lane.chat_id),
    }
}

// ============================================================================
// Native command parsing: /command@TargetBot
// ============================================================================

static COMMAND_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/([A-Za-z0-9_]+)(?:@([A-Za-z0-9_]+))?(?:\s+([\s\S]*))?$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    pub name: String,
    pub target_bot: Option<String>,
    pub args: String,
}

/// Parses a native `/command[@TargetBot] [args]` message.
pub fn parse_native_command(text: &str) -> Option<ParsedCommand> {
    let caps = COMMAND_RE.captures(text.trim())?;
    Some(ParsedCommand {
        name: caps[1].to_lowercase(),
        target_bot: caps.get(2).map(|m| m.as_str().to_string()),
        args: caps.get(3).map(|m| m.as_str().trim().to_string()).unwrap_or_default(),
    })
}

/// Whether a parsed command addresses this bot. A foreign `/stop@otherbot`
/// stays on its topic lane but is never executed here.
pub fn command_is_for_bot(command: &ParsedCommand, bot_username: &str) -> bool {
    match &command.target_bot {
        None => true,
        Some(target) => target.eq_ignore_ascii_case(bot_username),
    }
}

// ============================================================================
// Mention → assistant identity binding
// ============================================================================

/// Mention patterns bound to the bot identity (from cached getMe), so mention
/// gating always tracks the actual assistant account.
pub fn mention_regexes(bot_username: &str) -> Vec<Regex> {
    let escaped = regex::escape(bot_username);
    vec![Regex::new(&format!(r"(?i)(^|\W)@{escaped}(\W|$)")).expect("valid mention regex")]
}

pub fn text_mentions_bot(text: &str, bot_username: &str) -> bool {
    mention_regexes(bot_username)
        .iter()
        .any(|re| re.is_match(text))
}

// ============================================================================
// Account-scoped topic → agent routing
// ============================================================================

/// Resolves the agent for a group/topic: topic `agentId` wins over the group
/// `agentId`; `None` = default agent (upstream conversation-route scoping).
pub fn resolve_topic_agent(
    account: &TelegramAccountConfig,
    chat_id: &str,
    topic_id: Option<i64>,
) -> Option<String> {
    let groups = account.groups.as_ref()?;
    let group = groups.get(chat_id).or_else(|| groups.get("*"))?;
    if let Some(topic_id) = topic_id {
        if let Some(topics) = &group.topics {
            let topic = topics
                .get(&topic_id.to_string())
                .or_else(|| topics.get("*"));
            if let Some(agent_id) = topic.and_then(|t| t.agent_id.clone()) {
                return Some(agent_id);
            }
        }
    }
    group.agent_id.clone()
}

// ============================================================================
// Topic-name propagation
// ============================================================================

/// Bounded cache of forum topic names, fed by `forum_topic_created` /
/// `forum_topic_edited` service messages and propagated into the update
/// context so agent turns see the human topic name (upstream
/// topic-name-cache behavior).
#[derive(Debug, Default)]
pub struct TopicNameCache {
    names: Mutex<HashMap<(String, i64), String>>,
}

const TOPIC_NAME_CACHE_CAP: usize = 1024;

impl TopicNameCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records topic names from service messages; returns the name recorded.
    pub fn observe_update(&self, update: &serde_json::Value) -> Option<String> {
        let msg = update_message(update)?;
        let chat_id = msg.pointer("/chat/id").and_then(|v| v.as_i64())?.to_string();
        let thread_id = msg.get("message_thread_id").and_then(|v| v.as_i64())?;
        let name = msg
            .pointer("/forum_topic_created/name")
            .or_else(|| msg.pointer("/forum_topic_edited/name"))
            .and_then(|v| v.as_str())?
            .to_string();
        let mut names = self.names.lock().unwrap();
        if names.len() >= TOPIC_NAME_CACHE_CAP && !names.contains_key(&(chat_id.clone(), thread_id))
        {
            names.clear(); // simple bounded reset
        }
        names.insert((chat_id, thread_id), name.clone());
        Some(name)
    }

    pub fn get(&self, chat_id: &str, thread_id: i64) -> Option<String> {
        self.names
            .lock()
            .unwrap()
            .get(&(chat_id.to_string(), thread_id))
            .cloned()
    }
}

// ============================================================================
// Steer / tell seam
// ============================================================================

/// Delivery sink for `/steer` and `/tell` guidance aimed at the active run of
/// a session. The port has no streaming-run registry yet, so the default sink
/// queues the guidance as the session's next turn and says so — the command
/// surface, parsing, and admin gating are fully implemented here.
#[async_trait]
pub trait ActiveRunSink: Send + Sync {
    /// `/steer`: interrupt-adjacent guidance for the active run.
    async fn steer(&self, session_key: &str, message: &str) -> String;
    /// `/tell`: queued guidance that must not interrupt the run.
    async fn tell(&self, session_key: &str, message: &str) -> String;
}

/// Default sink: queues guidance as the next agent turn.
pub struct QueueNextTurnSink {
    pub config: Config,
}

#[async_trait]
impl ActiveRunSink for QueueNextTurnSink {
    async fn steer(&self, session_key: &str, message: &str) -> String {
        match crate::agents::run_single_message(&self.config, message, Some(session_key)).await {
            Ok(()) => "Steering delivered to the session.".to_string(),
            Err(err) => format!("Steering failed: {err}"),
        }
    }

    async fn tell(&self, session_key: &str, message: &str) -> String {
        match crate::agents::run_single_message(&self.config, message, Some(session_key)).await {
            Ok(()) => "Message queued for the session.".to_string(),
            Err(err) => format!("Tell failed: {err}"),
        }
    }
}

// ============================================================================
// Update handler
// ============================================================================

/// Context passed to update handlers.
pub struct UpdateContext {
    pub lane: LaneKey,
    pub session_key: String,
    pub bot_username: String,
    /// Agent scoped to this topic/group, when configured.
    pub agent_id: Option<String>,
    /// Human-readable forum topic name, when known (topic-name propagation).
    pub topic_name: Option<String>,
}

/// Handles one spooled update. Returning `Ok` adopts the update (completes it
/// in the spool); `Err` records a failed attempt (tombstoned after 3).
#[async_trait]
pub trait TelegramUpdateHandler: Send + Sync {
    async fn handle(&self, update: &serde_json::Value, ctx: &UpdateContext) -> anyhow::Result<()>;
}

/// Default handler: command routing + agent turns + reply hydration +
/// transcript echo, against the nearest existing port APIs.
pub struct AgentUpdateHandler {
    pub config: Config,
    pub api: Arc<TelegramApi>,
    pub spool: Arc<TelegramUpdateSpool>,
    pub sink: Arc<dyn ActiveRunSink>,
    pub pairing: PairingFlowStore,
    pub login_runner: Box<dyn LoginFlowRunner>,
}

impl AgentUpdateHandler {
    pub fn new(config: Config, api: Arc<TelegramApi>, spool: Arc<TelegramUpdateSpool>) -> Self {
        let sink = Arc::new(QueueNextTurnSink {
            config: config.clone(),
        });
        Self {
            config,
            api,
            spool,
            sink,
            pairing: PairingFlowStore::new(),
            login_runner: Box::new(LocalCodeLoginRunner),
        }
    }

    async fn reply(&self, lane: &LaneKey, text: &str) {
        if let Err(err) = self
            .api
            .send_message_chunked(&lane.chat_id, text, lane.thread_id, None)
            .await
        {
            warn!("telegram dispatcher reply failed: {err}");
        }
    }

    async fn sender_is_group_admin(&self, chat_id: &str, sender_id: Option<i64>) -> bool {
        let Some(sender_id) = sender_id else {
            return false;
        };
        match self.api.get_chat_member_status(chat_id, sender_id).await {
            Ok(Some(status)) => super::telegram_commands::is_chat_admin_status(&status),
            _ => false,
        }
    }

    async fn handle_command(
        &self,
        command: &ParsedCommand,
        msg: &serde_json::Value,
        ctx: &UpdateContext,
    ) -> anyhow::Result<()> {
        let lane = &ctx.lane;
        let chat_type = msg
            .pointer("/chat/type")
            .and_then(|v| v.as_str())
            .unwrap_or("private");
        let sender_id = msg.pointer("/from/id").and_then(|v| v.as_i64());
        let account = &self.config.channels.telegram.default_account;

        // Super-group admin gate for control commands.
        if super::telegram_commands::is_group_chat_type(chat_type) {
            let status_admin = self.sender_is_group_admin(&lane.chat_id, sender_id).await;
            let allowlisted = sender_id
                .map(|id| {
                    let id = id.to_string();
                    account
                        .allow_from
                        .iter()
                        .flatten()
                        .chain(account.group_allow_from.iter().flatten())
                        .any(|entry| entry == &id)
                })
                .unwrap_or(false);
            let group_cfg = account
                .groups
                .as_ref()
                .and_then(|g| g.get(&lane.chat_id).or_else(|| g.get("*")));
            let allowed = should_allow_group_command(GroupCommandGate {
                chat_type,
                sender_status: status_admin.then_some("administrator"),
                sender_allowlisted: allowlisted,
                group_admin_only_commands: group_cfg.and_then(|g| g.admin_only_commands),
                account_admin_only_commands: account.admin_only_commands,
            });
            if !allowed {
                debug!("telegram: dropping unauthorized group command /{}", command.name);
                return Ok(());
            }
        }

        match command.name.as_str() {
            "steer" => {
                if command.args.is_empty() {
                    self.reply(lane, "Usage: /steer <message>").await;
                    return Ok(());
                }
                let reply = self.sink.steer(&ctx.session_key, &command.args).await;
                self.reply(lane, &reply).await;
            }
            "tell" => {
                if command.args.is_empty() {
                    self.reply(lane, "Usage: /tell <message>").await;
                    return Ok(());
                }
                let reply = self.sink.tell(&ctx.session_key, &command.args).await;
                self.reply(lane, &reply).await;
            }
            "login" => {
                let sender = sender_id.map(|id| id.to_string()).unwrap_or_default();
                let owners: Vec<&String> = account.allow_from.iter().flatten().collect();
                let gate = evaluate_login_gate(LoginGateParams {
                    owner_allowlist_configured: !owners.is_empty(),
                    sender_is_owner: owners.iter().any(|o| **o == sender),
                    is_group: super::telegram_commands::is_group_chat_type(chat_type),
                    provider_arg: (!command.args.is_empty()).then_some(command.args.as_str()),
                });
                if let Some(rejection) = gate.rejection_text() {
                    self.reply(lane, rejection).await;
                    return Ok(());
                }
                debug_assert_eq!(gate, LoginGateDecision::Allowed);
                let provider = super::telegram_pairing::resolve_login_provider(
                    (!command.args.is_empty()).then_some(command.args.as_str()),
                )
                .unwrap_or("codex");
                let flow_key =
                    build_login_flow_key("default", &lane.chat_id, lane.thread_id, provider);
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                match self.pairing.reserve(&flow_key, now_ms) {
                    FlowReservation::AlreadyActive => {
                        self.reply(lane, LOGIN_FLOW_ALREADY_ACTIVE_TEXT).await;
                    }
                    FlowReservation::Reserved { code, .. } => {
                        let text = self.login_runner.run(provider, &code);
                        self.reply(lane, &text).await;
                    }
                }
            }
            other => {
                debug!("telegram: unhandled native command /{other}; routed as agent turn");
                let prompt = format!("/{} {}", command.name, command.args);
                crate::agents::run_single_message(
                    &self.config,
                    prompt.trim(),
                    Some(&ctx.session_key),
                )
                .await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl TelegramUpdateHandler for AgentUpdateHandler {
    async fn handle(&self, update: &serde_json::Value, ctx: &UpdateContext) -> anyhow::Result<()> {
        let Some(msg) = update_message(update) else {
            return Ok(()); // Non-message updates adopt as no-ops.
        };
        let text = msg
            .get("text")
            .or_else(|| msg.get("caption"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let message_id = msg.get("message_id").and_then(|v| v.as_i64());

        // Record inbound context for later reply-chain hydration.
        if let Some(message_id) = message_id {
            let _ = self.spool.record_reply_context(
                &ctx.lane.chat_id,
                message_id,
                (!text.is_empty()).then_some(text),
                &[],
            );
        }

        // Native commands (incl. /command@TargetBot).
        if let Some(command) = parse_native_command(text) {
            if !command_is_for_bot(&command, &ctx.bot_username) {
                // Foreign `/stop@otherbot`: stays on this topic lane, never
                // executed by this bot.
                debug!(
                    "telegram: ignoring /{}@{} (foreign bot)",
                    command.name,
                    command.target_bot.as_deref().unwrap_or("")
                );
                return Ok(());
            }
            return self.handle_command(&command, msg, ctx).await;
        }

        if text.is_empty() {
            // Voice-note-only messages: echo preflighted transcript when a
            // transcript is attached by the media pipeline (seam: the port's
            // transcription runner populates it on the update payload).
            if let Some(transcript) = update
                .pointer("/mylobster/preflight_transcript")
                .and_then(|v| v.as_str())
            {
                send_transcript_echo(
                    &self.api,
                    &self.config,
                    &ctx.lane.chat_id,
                    ctx.lane.thread_id,
                    transcript,
                )
                .await;
            }
            return Ok(());
        }

        // Reply-chain hydration: prefix quoted context recovered from the
        // persisted reply-context cache.
        let mut body = text.to_string();
        if let Some(reply_to_id) = msg.pointer("/reply_to_message/message_id").and_then(|v| v.as_i64())
        {
            if let Ok(Some((Some(quoted), _media))) =
                self.spool.hydrate_reply_context(&ctx.lane.chat_id, reply_to_id)
            {
                body = format!("[Replying to: {quoted:.200}]\n{body}");
            }
        }

        crate::agents::run_single_message(&self.config, &body, Some(&ctx.session_key)).await
    }
}

// ============================================================================
// Dispatcher
// ============================================================================

const OUTBOUND_DRAIN_CAP: usize = 256;
const CONFLICT_BACKOFF_MS: u64 = 3_000;

/// The Telegram polling dispatcher.
pub struct TelegramDispatcher {
    api: Arc<TelegramApi>,
    spool: Arc<TelegramUpdateSpool>,
    handler: Arc<dyn TelegramUpdateHandler>,
    account: TelegramAccountConfig,
    liveness: Mutex<TelegramPollingLivenessTracker>,
    stall_threshold_ms: u64,
    /// Failed-but-retryable outbound sends drained after reconnect.
    outbound_queue: Mutex<VecDeque<(String, serde_json::Value)>>,
    bot_username: Mutex<Option<String>>,
    had_transport_failure: Mutex<bool>,
    topic_names: Arc<TopicNameCache>,
}

impl TelegramDispatcher {
    pub fn new(
        api: Arc<TelegramApi>,
        spool: Arc<TelegramUpdateSpool>,
        handler: Arc<dyn TelegramUpdateHandler>,
        account: TelegramAccountConfig,
    ) -> Self {
        let now_ms = Self::now_ms();
        let stall_threshold_ms =
            resolve_polling_stall_threshold_ms(account.polling_stall_threshold_ms);
        Self {
            api,
            spool,
            handler,
            account,
            liveness: Mutex::new(TelegramPollingLivenessTracker::new(now_ms)),
            stall_threshold_ms,
            outbound_queue: Mutex::new(VecDeque::new()),
            bot_username: Mutex::new(None),
            had_transport_failure: Mutex::new(false),
            topic_names: Arc::new(TopicNameCache::new()),
        }
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    async fn bot_username(&self) -> String {
        if let Some(username) = self.bot_username.lock().unwrap().clone() {
            return username;
        }
        let username = match self.api.get_me_cached().await {
            Ok(me) => me
                .get("username")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            Err(err) => {
                debug!("telegram dispatcher getMe failed: {err}");
                String::new()
            }
        };
        *self.bot_username.lock().unwrap() = Some(username.clone());
        username
    }

    /// Queues a failed outbound send for the post-reconnect drain (bounded).
    pub fn enqueue_outbound(&self, method: &str, body: serde_json::Value) {
        let mut queue = self.outbound_queue.lock().unwrap();
        if queue.len() >= OUTBOUND_DRAIN_CAP {
            queue.pop_front();
        }
        queue.push_back((method.to_string(), body));
    }

    pub fn outbound_queue_len(&self) -> usize {
        self.outbound_queue.lock().unwrap().len()
    }

    /// Drains queued outbound sends after transport recovery.
    async fn drain_outbound(&self) {
        loop {
            let next = self.outbound_queue.lock().unwrap().pop_front();
            let Some((method, body)) = next else { break };
            if let Err(err) = self.api.call_with_send_retry(&method, &body).await {
                warn!("telegram outbound drain: {method} still failing: {err}");
                // Push back and stop draining; another recovery will retry.
                self.outbound_queue.lock().unwrap().push_front((method, body));
                break;
            }
        }
    }

    /// One poll cycle: getUpdates → spool → dispatch by lane → persist offset.
    /// Returns the number of updates spooled this cycle.
    pub async fn poll_once(&self) -> Result<usize, TelegramApiError> {
        let fingerprint = self.api.token_fingerprint().to_string();
        // Token rotation discards the offset by construction (fingerprint key).
        let offset = self.spool.load_offset(&fingerprint).ok().flatten();
        let started = Self::now_ms();
        self.liveness.lock().unwrap().note_started(started);

        let updates = match self.api.get_updates(offset).await {
            Ok(result) => {
                let mut liveness = self.liveness.lock().unwrap();
                let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                liveness.note_success(count, Self::now_ms());
                liveness.note_finished();
                drop(liveness);
                // Transport recovered → drain queued outbound sends.
                let recovered = {
                    let mut failed = self.had_transport_failure.lock().unwrap();
                    std::mem::replace(&mut *failed, false)
                };
                if recovered {
                    info!("telegram: transport recovered; draining outbound queue");
                    self.drain_outbound().await;
                }
                result
            }
            Err(err) => {
                {
                    let mut liveness = self.liveness.lock().unwrap();
                    liveness.note_error(Self::now_ms());
                    liveness.note_finished();
                }
                if err.transport {
                    *self.had_transport_failure.lock().unwrap() = true;
                }
                if is_get_updates_conflict_error(&err) {
                    // Duplicate poller (pid reuse / second instance): back off
                    // and re-enter — the other poller may be a stale self.
                    warn!(
                        "telegram getUpdates conflict (409): another poller holds the long poll; \
                         backing off {CONFLICT_BACKOFF_MS}ms"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(CONFLICT_BACKOFF_MS)).await;
                    return Ok(0);
                }
                return Err(err);
            }
        };

        let mut max_update_id: Option<i64> = None;
        let mut spooled = 0usize;
        for update in updates.as_array().map(|a| a.as_slice()).unwrap_or_default() {
            let Some(update_id) = update.get("update_id").and_then(|v| v.as_i64()) else {
                continue;
            };
            max_update_id = Some(max_update_id.map_or(update_id, |m: i64| m.max(update_id)));
            if let Ok(true) = self.spool.enqueue(update_id, &update.to_string()) {
                spooled += 1;
            }
        }

        self.dispatch_spooled().await;

        // Offset persisted AFTER dispatch (crash between poll and dispatch
        // re-polls; spool dedupe absorbs replays).
        if let Some(max_update_id) = max_update_id {
            let _ = self.spool.store_offset(&fingerprint, max_update_id + 1);
        }
        Ok(spooled)
    }

    /// Dispatches queued spool entries: sequential within a lane, concurrent
    /// across lanes. Adoption completes an entry; failures are re-attempted
    /// then tombstoned by the spool.
    pub async fn dispatch_spooled(&self) {
        let batch = match self.spool.next_batch(100) {
            Ok(batch) => batch,
            Err(err) => {
                warn!("telegram spool read failed: {err}");
                return;
            }
        };
        if batch.is_empty() {
            return;
        }
        let bot_username = self.bot_username().await;

        let mut lanes: HashMap<LaneKey, Vec<(SpooledUpdate, serde_json::Value)>> = HashMap::new();
        for entry in batch {
            let Ok(update) = serde_json::from_str::<serde_json::Value>(&entry.payload) else {
                // Unparseable payload: burn attempts straight to tombstone.
                while let Ok(SpoolStatus::Queued) = self.spool.record_failed_attempt(entry.rowid) {}
                continue;
            };
            // Topic-name propagation: service messages feed the cache.
            self.topic_names.observe_update(&update);
            lanes
                .entry(lane_key_for_update(&update))
                .or_default()
                .push((entry, update));
        }

        let lane_futures = lanes.into_iter().map(|(lane, entries)| {
            let handler = Arc::clone(&self.handler);
            let spool = Arc::clone(&self.spool);
            let account = self.account.clone();
            let bot_username = bot_username.clone();
            let topic_names = Arc::clone(&self.topic_names);
            async move {
                for (entry, update) in entries {
                    let ctx = UpdateContext {
                        session_key: lane_session_key(&lane),
                        agent_id: resolve_topic_agent(&account, &lane.chat_id, lane.thread_id),
                        bot_username: bot_username.clone(),
                        topic_name: lane
                            .thread_id
                            .and_then(|thread| topic_names.get(&lane.chat_id, thread)),
                        lane: lane.clone(),
                    };
                    match handler.handle(&update, &ctx).await {
                        Ok(()) => {
                            // Turn adoption: complete only after the handler
                            // durably adopted the update.
                            if let Err(err) = spool.mark_adopted(entry.rowid) {
                                warn!("telegram spool adoption write failed: {err}");
                            }
                        }
                        Err(err) => {
                            warn!(
                                "telegram update {} failed (attempt {}): {err}",
                                entry.update_id,
                                entry.attempts + 1
                            );
                            match spool.record_failed_attempt(entry.rowid) {
                                Ok(SpoolStatus::Tombstoned) => warn!(
                                    "telegram update {} tombstoned (dead-letter)",
                                    entry.update_id
                                ),
                                Ok(_) => {}
                                Err(err) => warn!("telegram spool attempt write failed: {err}"),
                            }
                        }
                    }
                }
            }
        });
        futures::future::join_all(lane_futures).await;
    }

    /// Runs the polling loop until `shutdown` resolves. The watchdog is keyed
    /// to getUpdates liveness: a stall past `pollingStallThresholdMs` forces
    /// a poll-loop restart (fresh getUpdates; the 45 s HTTP guard bounds each
    /// wire call so a restart is always reachable).
    pub async fn run(self: Arc<Self>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        info!(
            "telegram dispatcher started (stall threshold {}ms)",
            self.stall_threshold_ms
        );
        loop {
            if *shutdown.borrow() {
                break;
            }
            let poll = self.poll_once();
            tokio::select! {
                result = poll => {
                    if let Err(err) = result {
                        warn!("telegram poll failed: {err}");
                        tokio::time::sleep(std::time::Duration::from_millis(1_000)).await;
                    }
                }
                _ = shutdown.changed() => break,
            }
            // Watchdog: detect stalls keyed to getUpdates liveness.
            let stall = self
                .liveness
                .lock()
                .unwrap()
                .detect_stall(self.stall_threshold_ms, Self::now_ms());
            if let Some(stall) = stall {
                warn!("{}", stall.message);
                self.api.invalidate_bot_info();
            }
        }
        info!("telegram dispatcher stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- lane keying ----

    fn update_json(chat_id: i64, thread: Option<i64>, text: &str) -> serde_json::Value {
        let mut msg = serde_json::json!({
            "message_id": 10,
            "chat": { "id": chat_id, "type": if chat_id < 0 { "supergroup" } else { "private" } },
            "from": { "id": 555 },
            "text": text,
        });
        if let Some(thread) = thread {
            msg["is_topic_message"] = serde_json::json!(true);
            msg["message_thread_id"] = serde_json::json!(thread);
        }
        serde_json::json!({ "update_id": 1, "message": msg })
    }

    #[test]
    fn lane_key_separates_topics() {
        let dm = lane_key_for_update(&update_json(42, None, "hi"));
        assert_eq!(dm.chat_id, "42");
        assert_eq!(dm.thread_id, None);
        let topic = lane_key_for_update(&update_json(-100, Some(7), "hi"));
        assert_eq!(topic.chat_id, "-100");
        assert_eq!(topic.thread_id, Some(7));
        assert_ne!(
            lane_key_for_update(&update_json(-100, Some(7), "a")),
            lane_key_for_update(&update_json(-100, Some(8), "a"))
        );
    }

    #[test]
    fn non_forum_thread_id_ignored_for_lanes() {
        // Non-topic message with a message_thread_id (plain reply threads)
        // stays on the chat lane.
        let update = serde_json::json!({
            "update_id": 2,
            "message": {
                "chat": { "id": -5, "type": "group" },
                "message_thread_id": 33,
                "text": "x"
            }
        });
        let lane = lane_key_for_update(&update);
        assert_eq!(lane.thread_id, None);
    }

    #[test]
    fn session_keys_match_target_syntax() {
        assert_eq!(
            lane_session_key(&LaneKey {
                chat_id: "-100".to_string(),
                thread_id: Some(3)
            }),
            "telegram:-100:topic:3"
        );
        assert_eq!(
            lane_session_key(&LaneKey {
                chat_id: "9".to_string(),
                thread_id: None
            }),
            "telegram:9"
        );
    }

    // ---- command parsing ----

    #[test]
    fn command_parsing_with_bot_target() {
        let cmd = parse_native_command("/steer@MyBot fix the tests").unwrap();
        assert_eq!(cmd.name, "steer");
        assert_eq!(cmd.target_bot.as_deref(), Some("MyBot"));
        assert_eq!(cmd.args, "fix the tests");

        let bare = parse_native_command("/tell keep going").unwrap();
        assert_eq!(bare.name, "tell");
        assert_eq!(bare.target_bot, None);
        assert_eq!(bare.args, "keep going");

        assert!(parse_native_command("not a command").is_none());
        assert!(parse_native_command("/").is_none());
    }

    #[test]
    fn foreign_bot_commands_not_executed() {
        let cmd = parse_native_command("/stop@OtherBot").unwrap();
        assert!(!command_is_for_bot(&cmd, "MyBot"));
        assert!(command_is_for_bot(&cmd, "otherbot")); // case-insensitive
        let untargeted = parse_native_command("/stop").unwrap();
        assert!(command_is_for_bot(&untargeted, "MyBot"));
    }

    // ---- mention binding ----

    #[test]
    fn mention_binding_tracks_bot_identity() {
        assert!(text_mentions_bot("hey @MyBot help", "MyBot"));
        assert!(text_mentions_bot("@mybot: hi", "MyBot"));
        assert!(!text_mentions_bot("hey @MyBotOther help", "MyBot"));
        assert!(!text_mentions_bot("no mention here", "MyBot"));
    }

    // ---- topic → agent routing ----

    #[test]
    fn topic_agent_overrides_group_agent() {
        use crate::config::{TelegramGroupConfig, TelegramTopicConfig};
        use std::collections::HashMap;

        let mut topics = HashMap::new();
        topics.insert(
            "7".to_string(),
            TelegramTopicConfig {
                agent_id: Some("finance".to_string()),
                ..Default::default()
            },
        );
        let mut groups = HashMap::new();
        groups.insert(
            "-100".to_string(),
            TelegramGroupConfig {
                agent_id: Some("general".to_string()),
                topics: Some(topics),
                ..Default::default()
            },
        );
        let account = TelegramAccountConfig {
            groups: Some(groups),
            ..Default::default()
        };
        assert_eq!(
            resolve_topic_agent(&account, "-100", Some(7)).as_deref(),
            Some("finance")
        );
        assert_eq!(
            resolve_topic_agent(&account, "-100", Some(8)).as_deref(),
            Some("general")
        );
        assert_eq!(
            resolve_topic_agent(&account, "-100", None).as_deref(),
            Some("general")
        );
        assert_eq!(resolve_topic_agent(&account, "-999", None), None);
    }

    // ---- topic-name propagation ----

    #[test]
    fn topic_names_recorded_and_looked_up() {
        let cache = TopicNameCache::new();
        let created = serde_json::json!({
            "update_id": 3,
            "message": {
                "chat": { "id": -100, "type": "supergroup" },
                "message_thread_id": 7,
                "is_topic_message": true,
                "forum_topic_created": { "name": "Finance" }
            }
        });
        assert_eq!(cache.observe_update(&created).as_deref(), Some("Finance"));
        assert_eq!(cache.get("-100", 7).as_deref(), Some("Finance"));
        // Edited names replace.
        let edited = serde_json::json!({
            "update_id": 4,
            "message": {
                "chat": { "id": -100 },
                "message_thread_id": 7,
                "forum_topic_edited": { "name": "Money" }
            }
        });
        cache.observe_update(&edited);
        assert_eq!(cache.get("-100", 7).as_deref(), Some("Money"));
        // Plain messages don't record names.
        assert!(cache.observe_update(&update_json(-100, Some(7), "x")).is_none());
        assert_eq!(cache.get("-100", 8), None);
    }

    // ---- outbound drain queue ----

    #[test]
    fn outbound_queue_bounded() {
        let api = Arc::new(TelegramApi::new("1:X", None, None));
        let spool = Arc::new(TelegramUpdateSpool::open_in_memory().unwrap());
        let handler = Arc::new(AgentUpdateHandler::new(
            Config::default(),
            Arc::clone(&api),
            Arc::clone(&spool),
        ));
        let dispatcher = TelegramDispatcher::new(
            api,
            spool,
            handler,
            TelegramAccountConfig::default(),
        );
        for i in 0..(OUTBOUND_DRAIN_CAP + 10) {
            dispatcher.enqueue_outbound("sendMessage", serde_json::json!({ "i": i }));
        }
        assert_eq!(dispatcher.outbound_queue_len(), OUTBOUND_DRAIN_CAP);
    }
}
