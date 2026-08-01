use crate::agents::tool_loop::{blocked_tool_result, ToolLoopDecision, ToolLoopGuard};
use crate::config::Config;
use crate::gateway::protocol::*;
use crate::hooks::{HookEvent, HookResult, SharedHookRegistry};
use crate::providers::{ProviderMessage, ProviderRequest, StreamEvent, ThinkingConfig};
use crate::sessions::{SessionHandle, SessionStore};

use anyhow::Result;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Maximum number of tool loop iterations before stopping.
const MAX_TOOL_ITERATIONS: usize = 25;

// ============================================================================
// Active-run queueing — steer mode (OpenClaw v2026.4.29)
// ============================================================================

/// Default followup debounce for steer mode (v2026.4.29: 500ms).
pub const STEER_FOLLOWUP_DEBOUNCE: Duration = Duration::from_millis(500);

/// Followups queued behind an active run for one session.
///
/// Steer semantics: messages arriving while a run is active are held; once
/// the newest entry is older than the debounce window, the whole batch is
/// folded into the active run as additional user turns.
#[derive(Debug)]
pub struct SteerQueue {
    pending: Vec<(String, Instant)>,
    debounce: Duration,
}

impl SteerQueue {
    pub fn new(debounce: Duration) -> Self {
        Self {
            pending: Vec::new(),
            debounce,
        }
    }

    pub fn enqueue(&mut self, message: impl Into<String>, now: Instant) {
        self.pending.push((message.into(), now));
    }

    /// Drain the batch once the debounce window has elapsed since the last
    /// enqueue; returns empty while messages are still arriving.
    pub fn drain_ready(&mut self, now: Instant) -> Vec<String> {
        match self.pending.last() {
            Some((_, last)) if now.duration_since(*last) >= self.debounce => {
                self.pending.drain(..).map(|(m, _)| m).collect()
            }
            _ => Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

static STEER_QUEUES: Lazy<DashMap<String, SteerQueue>> = Lazy::new(DashMap::new);

/// Whether active-run queueing uses steer mode. `messages.queue.mode`
/// defaults to `"steer"` (v2026.4.29).
fn steer_mode_enabled(config: &Config) -> bool {
    config
        .messages
        .queue
        .as_ref()
        .and_then(|q| q.mode.as_deref())
        .map(|m| m.trim().eq_ignore_ascii_case("steer"))
        .unwrap_or(true)
}

fn steer_debounce(config: &Config) -> Duration {
    config
        .messages
        .queue
        .as_ref()
        .and_then(|q| q.debounce_ms)
        .map(Duration::from_millis)
        .unwrap_or(STEER_FOLLOWUP_DEBOUNCE)
}

fn steer_enqueue(config: &Config, session_key: &str, message: &str) {
    let debounce = steer_debounce(config);
    STEER_QUEUES
        .entry(session_key.to_string())
        .or_insert_with(|| SteerQueue::new(debounce))
        .enqueue(message, Instant::now());
}

fn steer_drain_ready(session_key: &str) -> Vec<String> {
    match STEER_QUEUES.get_mut(session_key) {
        Some(mut q) => q.drain_ready(Instant::now()),
        None => Vec::new(),
    }
}

/// RAII busy flag for the session while a run is active.
struct BusyGuard {
    handle: SessionHandle,
}

impl BusyGuard {
    fn new(handle: SessionHandle) -> Self {
        handle.set_busy(true);
        Self { handle }
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.handle.set_busy(false);
    }
}

// ============================================================================
// Blank visible user prompts (OpenClaw v2026.5.2)
// ============================================================================

/// Skip blank visible user prompts at the embedded-runner boundary.
/// Internal runtime-event markers and media-only turns still run.
fn should_skip_visible_user_prompt(message: &str, attachments: Option<&[serde_json::Value]>) -> bool {
    if !message.trim().is_empty() {
        return false;
    }
    // Media-only turns are allowed.
    if attachments.map(|a| !a.is_empty()).unwrap_or(false) {
        return false;
    }
    true
}

/// Run a compaction pass for a session (hook-bracketed).
async fn run_compaction_pass(
    sessions: &SessionStore,
    session_key: &str,
    hooks: &Option<Arc<SharedHookRegistry>>,
) {
    if let Some(h) = hooks {
        h.emit(HookEvent::BeforeCompaction {
            session_key: session_key.to_string(),
        })
        .await;
    }
    let compacted = sessions.compact_session(session_key);
    if let Some(h) = hooks {
        h.emit(HookEvent::AfterCompaction {
            session_key: session_key.to_string(),
        })
        .await;
    }
    debug!(session_key, compacted, "compaction pass completed");
}

/// Handle a chat request and stream events back.
///
/// Events are emitted in OC format:
/// - Chat events: `{runId, state:"delta"|"final"|"error", message:{content:[{type:"text",text:"..."}]}}`
/// - Agent events: `{runId, stream:"tool"|"assistant", data:{...}}`
///
/// Content is always emitted as an array of content blocks `[{type:"text", text:"..."}]`,
/// because the bridge reads `content[0].text`.
pub async fn process_chat(
    config: &Config,
    sessions: &SessionStore,
    params: &ChatSendParams,
    event_tx: mpsc::Sender<ChatEvent>,
    cancel: CancellationToken,
) -> Result<()> {
    process_chat_with_hooks(config, sessions, params, event_tx, cancel, None).await
}

/// Process a chat request with optional hook registry for lifecycle events.
pub async fn process_chat_with_hooks(
    config: &Config,
    sessions: &SessionStore,
    params: &ChatSendParams,
    event_tx: mpsc::Sender<ChatEvent>,
    cancel: CancellationToken,
    hooks: Option<Arc<SharedHookRegistry>>,
) -> Result<()> {
    let run_id = params
        .idempotency_key
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let session_key = &params.session_key;

    // Fire BeforeAgentStart hook
    if let Some(ref h) = hooks {
        h.emit(HookEvent::BeforeAgentStart {
            session_key: session_key.clone(),
        })
        .await;
    }

    // Get or create session
    let session = sessions.get_or_create_session(session_key, config);

    // v2026.5.2: skip blank visible user prompts at the embedded-runner
    // boundary (media-only and runtime-event turns still run).
    if should_skip_visible_user_prompt(&params.message, params.attachments.as_deref()) {
        debug!(session_key, "skipping blank visible user prompt");
        let final_event = ChatEvent {
            run_id: run_id.clone(),
            session_key: session_key.clone(),
            seq: 0,
            state: ChatEventState::Final,
            message: Some(serde_json::json!({
                "role": "assistant",
                "content": [{ "type": "text", "text": "" }]
            })),
            error_message: None,
            usage: None,
            stop_reason: Some("skipped_blank_prompt".to_string()),
        };
        let _ = event_tx.send(final_event).await;
        return Ok(());
    }

    // v2026.4.29: active-run queueing — default steer mode. Followups
    // hitting a busy session are debounced (500ms) and folded into the
    // active run instead of spawning a competing run.
    if session.is_busy() && steer_mode_enabled(config) {
        debug!(session_key, "session busy — steering followup into active run");
        steer_enqueue(config, session_key, &params.message);
        return Ok(());
    }
    let _busy = BusyGuard::new(session.clone());

    // Fire MessageReceived hook
    if let Some(ref h) = hooks {
        h.emit(HookEvent::MessageReceived {
            from: session_key.clone(),
            content: params.message.clone(),
            timestamp: Some(chrono::Utc::now().timestamp_millis() as u64),
        })
        .await;
    }

    // Build messages from session history + new user message
    let mut messages = session.get_history();

    // v2026.2.26: Inject message timestamp context for time-aware responses.
    let timestamp = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();
    let message_with_time = if params.message.len() < 10_000 {
        format!("[{}] {}", timestamp, params.message)
    } else {
        params.message.clone() // Don't prepend to very long messages
    };

    messages.push(ProviderMessage {
        role: "user".to_string(),
        content: serde_json::Value::String(message_with_time),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    });

    // Resolve model provider
    let mut model = config
        .agent
        .model
        .primary_model()
        .unwrap_or_else(|| "claude-sonnet-4-6".to_string());

    // Fire BeforeModelResolve hook (modifying — can override model)
    if let Some(ref h) = hooks {
        let result = h
            .emit_modifying(HookEvent::BeforeModelResolve {
                prompt: params.message.clone(),
            })
            .await;
        if let HookResult::Override { data } = result {
            if let Some(m) = data.as_str() {
                info!(original = %model, override_to = m, "model overridden by hook");
                model = m.to_string();
            }
        }
    }

    let provider = crate::providers::resolve_provider(config, &model)?;

    // Build tool definitions for the provider
    let tools = build_tool_definitions(config);

    // v2026.4.26: maxActiveTranscriptBytes preflight compaction trigger.
    {
        let transcript_bytes =
            crate::agents::compaction::estimate_transcript_bytes(&messages);
        if crate::agents::compaction::should_preflight_compact(
            &config.agent.compaction,
            transcript_bytes,
        ) {
            info!(session_key, transcript_bytes, "preflight compaction triggered");
            run_compaction_pass(sessions, session_key, &hooks).await;
        }
    }

    // v2026.5.2: tool-loop circuit breaker — critical stops surface as
    // blocked tool results, not thrown failures.
    let mut loop_guard = ToolLoopGuard::new();

    // Agentic loop: call provider, execute tools, repeat
    let mut iteration = 0;
    let mut seq = 0u64;

    loop {
        // v2026.4.29 steer mode: fold debounced followups into this run.
        for followup in steer_drain_ready(session_key) {
            debug!(session_key, "steering queued followup into active run");
            messages.push(ProviderMessage {
                role: "user".to_string(),
                content: serde_json::Value::String(followup),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            });
        }

        // v2026.5.2: mid-turn compaction precheck between iterations.
        if iteration > 0 {
            let transcript_bytes =
                crate::agents::compaction::estimate_transcript_bytes(&messages);
            let estimated_tokens = transcript_bytes / 4;
            if crate::agents::compaction::should_compact_mid_turn(
                &config.agent.compaction,
                config.agent.context_tokens,
                estimated_tokens,
                transcript_bytes,
            ) {
                info!(session_key, transcript_bytes, "mid-turn compaction precheck triggered");
                run_compaction_pass(sessions, session_key, &hooks).await;
            }
        }

        if cancel.is_cancelled() {
            let abort_event = ChatEvent {
                run_id: run_id.clone(),
                session_key: session_key.clone(),
                seq,
                state: ChatEventState::Aborted,
                message: None,
                error_message: Some("cancelled".to_string()),
                usage: None,
                stop_reason: None,
            };
            let _ = event_tx.send(abort_event).await;
            break;
        }

        iteration += 1;
        if iteration > MAX_TOOL_ITERATIONS {
            warn!("Hit max tool iterations ({}) for run {}", MAX_TOOL_ITERATIONS, run_id);
            let error_event = ChatEvent {
                run_id: run_id.clone(),
                session_key: session_key.clone(),
                seq,
                state: ChatEventState::Error,
                message: None,
                error_message: Some(format!(
                    "Maximum tool iterations ({}) exceeded",
                    MAX_TOOL_ITERATIONS
                )),
                usage: None,
                stop_reason: None,
            };
            let _ = event_tx.send(error_event).await;
            break;
        }

        // Enable extended thinking for Claude models (makes reasoning visible)
        let thinking = if model.contains("claude") {
            Some(ThinkingConfig { budget_tokens: 10000 })
        } else {
            None
        };

        // Create request with tools
        let request = ProviderRequest {
            model: model.clone(),
            messages: messages.clone(),
            max_tokens: None,
            temperature: None,
            stream: true,
            tools: if tools.is_empty() {
                None
            } else {
                Some(tools.clone())
            },
            tool_choice: None,
            thinking,
        };

        // Fire LlmInput hook
        if let Some(ref h) = hooks {
            let msgs_json: Vec<serde_json::Value> = messages
                .iter()
                .map(|m| serde_json::json!({"role": m.role, "content": m.content}))
                .collect();
            h.emit(HookEvent::LlmInput {
                model: model.clone(),
                messages: msgs_json,
            })
            .await;
        }

        // Stream response
        let mut full_content = String::new();
        let mut tool_calls: Vec<serde_json::Value> = Vec::new();
        let mut final_usage = None;

        match provider.stream_chat(request).await {
            Ok(mut stream) => {
                while let Some(event) = stream.recv().await {
                    if cancel.is_cancelled() {
                        let abort_event = ChatEvent {
                            run_id: run_id.clone(),
                            session_key: session_key.clone(),
                            seq,
                            state: ChatEventState::Aborted,
                            message: None,
                            error_message: Some("cancelled".to_string()),
                            usage: None,
                            stop_reason: None,
                        };
                        let _ = event_tx.send(abort_event).await;
                        return Ok(());
                    }

                    match event {
                        StreamEvent::Delta(text) => {
                            full_content.push_str(&text);
                            // Emit delta with content as array of content blocks
                            let chat_event = ChatEvent {
                                run_id: run_id.clone(),
                                session_key: session_key.clone(),
                                seq,
                                state: ChatEventState::Delta,
                                message: Some(serde_json::json!({
                                    "role": "assistant",
                                    "content": [{
                                        "type": "text",
                                        "text": full_content
                                    }]
                                })),
                                error_message: None,
                                usage: None,
                                stop_reason: None,
                            };
                            seq += 1;
                            let _ = event_tx.send(chat_event).await;
                        }
                        StreamEvent::Thinking(text) => {
                            // Emit thinking delta so the user can see reasoning
                            let chat_event = ChatEvent {
                                run_id: run_id.clone(),
                                session_key: session_key.clone(),
                                seq,
                                state: ChatEventState::Delta,
                                message: Some(serde_json::json!({
                                    "thinking": text
                                })),
                                error_message: None,
                                usage: None,
                                stop_reason: None,
                            };
                            seq += 1;
                            let _ = event_tx.send(chat_event).await;
                        }
                        StreamEvent::ToolCall(tool_call) => {
                            tool_calls.push(tool_call.clone());

                            let chat_event = ChatEvent {
                                run_id: run_id.clone(),
                                session_key: session_key.clone(),
                                seq,
                                state: ChatEventState::Delta,
                                message: Some(serde_json::json!({
                                    "role": "assistant",
                                    "tool_calls": [tool_call]
                                })),
                                error_message: None,
                                usage: None,
                                stop_reason: None,
                            };
                            seq += 1;
                            let _ = event_tx.send(chat_event).await;
                        }
                        StreamEvent::Done(usage) => {
                            final_usage = Some(usage);
                            break;
                        }
                        StreamEvent::Error(e) => {
                            let chat_event = ChatEvent {
                                run_id: run_id.clone(),
                                session_key: session_key.clone(),
                                seq,
                                state: ChatEventState::Error,
                                message: None,
                                // v2026.4.1: sanitize before sending to client
                                error_message: Some(sanitize_chat_error(&e)),
                                usage: None,
                                stop_reason: None,
                            };
                            let _ = event_tx.send(chat_event).await;
                            return Ok(());
                        }
                        // Replay hook events are forwarded as metadata deltas (v2026.4.1).
                        StreamEvent::Replay(_) => {}
                    }
                }
            }
            Err(e) => {
                let raw = format!("Provider error: {}", e);
                let chat_event = ChatEvent {
                    run_id: run_id.clone(),
                    session_key: session_key.clone(),
                    seq: 0,
                    state: ChatEventState::Error,
                    message: None,
                    // v2026.4.1: sanitize before sending to client
                    error_message: Some(sanitize_chat_error(&raw)),
                    usage: None,
                    stop_reason: None,
                };
                let _ = event_tx.send(chat_event).await;
                return Ok(());
            }
        }

        // If there are tool calls, execute them and loop
        if !tool_calls.is_empty() {
            // Add assistant message with tool calls to history
            messages.push(ProviderMessage {
                role: "assistant".to_string(),
                content: if full_content.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::String(full_content.clone())
                },
                name: None,
                tool_call_id: None,
                tool_calls: Some(tool_calls.clone()),
            });

            // Execute each tool call
            let mut critical_stop: Option<String> = None;
            for tool_call in &tool_calls {
                let tool_name = tool_call
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let tool_call_id = tool_call
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let tool_input = tool_call
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

                // v2026.5.2: tool-loop circuit breaker. Blocked calls feed
                // the model a blocked tool result instead of executing;
                // critical stops end the run without a thrown failure.
                match loop_guard.check(tool_name, &tool_input) {
                    ToolLoopDecision::Proceed => {}
                    ToolLoopDecision::Blocked { reason } => {
                        warn!(tool = tool_name, "tool loop detected — blocking call");
                        let blocked = blocked_tool_result(&reason);
                        messages.push(ProviderMessage {
                            role: "tool".to_string(),
                            content: serde_json::Value::String(
                                blocked.text.unwrap_or_default(),
                            ),
                            name: Some(tool_name.to_string()),
                            tool_call_id: Some(tool_call_id.to_string()),
                            tool_calls: None,
                        });
                        continue;
                    }
                    ToolLoopDecision::CriticalStop { reason } => {
                        warn!(tool = tool_name, "tool loop critical stop");
                        let blocked = blocked_tool_result(&reason);
                        messages.push(ProviderMessage {
                            role: "tool".to_string(),
                            content: serde_json::Value::String(
                                blocked.text.unwrap_or_default(),
                            ),
                            name: Some(tool_name.to_string()),
                            tool_call_id: Some(tool_call_id.to_string()),
                            tool_calls: None,
                        });
                        critical_stop = Some(reason);
                        break;
                    }
                }

                debug!("Executing tool: {} (id={})", tool_name, tool_call_id);

                // Fire BeforeToolCall hook (modifying — can cancel)
                if let Some(ref h) = hooks {
                    let result = h
                        .emit_modifying(HookEvent::BeforeToolCall {
                            tool: tool_name.to_string(),
                            params: tool_input.clone(),
                        })
                        .await;
                    if let HookResult::Cancel { reason } = result {
                        info!(tool = tool_name, %reason, "tool call cancelled by hook");
                        messages.push(ProviderMessage {
                            role: "tool".to_string(),
                            content: serde_json::Value::String(format!(
                                "Tool call cancelled: {}",
                                reason
                            )),
                            name: Some(tool_name.to_string()),
                            tool_call_id: Some(tool_call_id.to_string()),
                            tool_calls: None,
                        });
                        continue;
                    }
                }

                // Execute tool
                let tool_result = execute_tool(config, session_key, tool_name, &tool_input).await;

                let result_text = match &tool_result {
                    Ok(result) => {
                        if let Some(ref text) = result.text {
                            text.clone()
                        } else if let Some(ref json) = result.json {
                            serde_json::to_string(json).unwrap_or_default()
                        } else {
                            "OK".to_string()
                        }
                    }
                    Err(e) => format!("Error: {}", e),
                };

                // Fire AfterToolCall hook
                if let Some(ref h) = hooks {
                    h.emit(HookEvent::AfterToolCall {
                        tool: tool_name.to_string(),
                        result: serde_json::json!({"text": result_text}),
                    })
                    .await;
                }

                // Add tool result to messages
                messages.push(ProviderMessage {
                    role: "tool".to_string(),
                    content: serde_json::Value::String(result_text),
                    name: Some(tool_name.to_string()),
                    tool_call_id: Some(tool_call_id.to_string()),
                    tool_calls: None,
                });
            }

            // v2026.5.2: a critical tool-loop stop ends the run as a normal
            // final (the blocked tool result is already in the transcript).
            if let Some(reason) = critical_stop {
                let text = crate::agents::reply_sanitize::sanitize_user_facing_reply(&reason);
                let final_event = ChatEvent {
                    run_id: run_id.clone(),
                    session_key: session_key.clone(),
                    seq,
                    state: ChatEventState::Final,
                    message: Some(serde_json::json!({
                        "role": "assistant",
                        "content": [{ "type": "text", "text": text }]
                    })),
                    error_message: None,
                    usage: None,
                    stop_reason: Some("tool_loop".to_string()),
                };
                let _ = event_tx.send(final_event).await;
                break;
            }

            // Clear tool_calls for next iteration
            tool_calls.clear();
            continue;
        }

        // Fire LlmOutput hook
        if let Some(ref h) = hooks {
            h.emit(HookEvent::LlmOutput {
                model: model.clone(),
                response: serde_json::json!({
                    "content": full_content
                }),
            })
            .await;
        }

        // No tool calls — this is the final response
        // Add assistant message to session
        session.add_message(ProviderMessage {
            role: "assistant".to_string(),
            content: serde_json::Value::String(full_content.clone()),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        });

        // v2026.5.2 / v2026.7.1: shared user-facing reply sanitization —
        // strip legacy [TOOL_CALL]/[TOOL_RESULT] blocks, MiniMax/XML tool
        // scaffolding, and runtime sentinels before the text reaches a user.
        let visible_content =
            crate::agents::reply_sanitize::sanitize_user_facing_reply(&full_content);

        // Emit final event with content as array of content blocks
        // Extract token counts before moving final_usage
        let hook_input_tokens = final_usage.as_ref().and_then(|u| u.input_tokens);
        let hook_output_tokens = final_usage.as_ref().and_then(|u| u.output_tokens);

        let final_event = ChatEvent {
            run_id: run_id.clone(),
            session_key: session_key.clone(),
            seq,
            state: ChatEventState::Final,
            message: Some(serde_json::json!({
                "role": "assistant",
                "content": [{
                    "type": "text",
                    "text": visible_content
                }]
            })),
            error_message: None,
            usage: final_usage,
            stop_reason: Some("end_turn".to_string()),
        };
        let _ = event_tx.send(final_event).await;

        // Fire AgentEnd hook
        if let Some(ref h) = hooks {
            h.emit(HookEvent::AgentEnd {
                session_key: session_key.clone(),
                input_tokens: hook_input_tokens,
                output_tokens: hook_output_tokens,
            })
            .await;
        }

        break;
    }

    Ok(())
}

// ============================================================================
// Chat error sanitization (v2026.4.1)
// ============================================================================

/// Sanitize error messages before sending to chat channels (v2026.4.1).
/// Prevents leaking raw provider/runtime failures to clients.
fn sanitize_chat_error(err: &str) -> String {
    // Don't expose internal error details to users
    if err.contains("API key") || err.contains("authentication") || err.contains("credentials") {
        return "Authentication error. Please check your configuration.".to_string();
    }
    if err.contains("rate limit") || err.contains("429") {
        return "Rate limited. Please try again in a moment.".to_string();
    }
    if err.contains("timeout") || err.contains("timed out") {
        return "Request timed out. Please try again.".to_string();
    }
    if err.contains("500") || err.contains("internal server error") {
        return "Provider error. Please try again.".to_string();
    }
    // Generic fallback — don't leak raw error
    "Something went wrong. Please try again.".to_string()
}

/// Build tool definitions in the format expected by providers.
fn build_tool_definitions(config: &Config) -> Vec<serde_json::Value> {
    let tools = crate::agents::tools::list_available_tools(config);
    tools
        .into_iter()
        .filter(|t| !t.hidden)
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
            })
        })
        .collect()
}

/// Execute a tool by name and return the result.
async fn execute_tool(
    config: &Config,
    session_key: &str,
    tool_name: &str,
    input: &serde_json::Value,
) -> Result<crate::agents::tools::ToolResult> {
    use crate::agents::tools::{AgentTool, ToolContext, ToolResult};

    let context = ToolContext {
        session_key: session_key.to_string(),
        agent_id: "default".to_string(),
        config: config.clone(),
    };

    // Resolve tool by name and execute
    use crate::agents::tools::{
        cron_tool, image_tool, media_tool, memory_tool, message_tool,
        pdf_tool, tts_tool,
        discord_actions, slack_actions, telegram_actions, whatsapp_actions,
        node_tools, canvas, subagents, agent_step, sessions_a2a,
    };

    let tool: Box<dyn AgentTool> = match tool_name {
        // Web tools
        "web_fetch" => Box::new(crate::agents::tools::web_fetch::WebFetchTool),
        "web_search" => Box::new(crate::agents::tools::web_search::WebSearchTool),

        // System tools
        "system_run" => Box::new(crate::agents::tools::bash::BashTool),

        // Memory tools
        "memory_store" => Box::new(memory_tool::MemoryStoreTool),
        "memory_search" => Box::new(memory_tool::MemorySearchTool),

        // Messaging tool
        "message_send" => Box::new(message_tool::MessageSendTool),

        // Cron tools
        "cron_schedule" => Box::new(cron_tool::CronScheduleTool),
        "cron_list" => Box::new(cron_tool::CronListTool),

        // Image generation
        "image_generate" => Box::new(image_tool::ImageGenerateTool),

        // TTS
        "tts_speak" => Box::new(tts_tool::TtsSpeakTool),

        // PDF extraction
        "pdf_extract" => Box::new(pdf_tool::PdfTool),

        // Media processing
        "media_process" => Box::new(media_tool::MediaTool),

        // Channel action tools
        "discord_actions" => Box::new(discord_actions::DiscordActionsTool),
        "telegram_actions" => Box::new(telegram_actions::TelegramActionsTool),
        "slack_actions" => Box::new(slack_actions::SlackActionsTool),
        "whatsapp_actions" => Box::new(whatsapp_actions::WhatsAppActionsTool),

        // Node/device tools
        "node_invoke" => Box::new(node_tools::NodeTool),

        // Canvas
        "canvas_render" => Box::new(canvas::CanvasTool),

        // Subagents
        "subagents" => Box::new(subagents::SubagentsTool),

        // Agent step (multi-step reasoning)
        "agent_step" => Box::new(agent_step::AgentStepTool),

        // A2A sessions
        "sessions_a2a" => Box::new(sessions_a2a::SessionsA2aTool),

        // Heartbeat structured response (v2026.5.2)
        "heartbeat_respond" => Box::new(crate::agents::heartbeat::HeartbeatRespondTool),

        _ => {
            // For tools that don't have full implementations yet,
            // return an error result rather than crashing
            warn!("Tool not implemented for execution: {}", tool_name);
            return Ok(ToolResult::error(format!(
                "Tool '{}' is not available for execution",
                tool_name
            )));
        }
    };

    tool.execute(input.clone(), &context).await
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // sanitize_chat_error — security-relevant: must NEVER leak raw provider
    // errors that could expose API keys, internal hostnames, etc.
    // ------------------------------------------------------------------------

    #[test]
    fn sanitize_masks_api_key_phrase() {
        let raw = "Anthropic API error (401): Invalid API key sk-ant-abc123def";
        let safe = sanitize_chat_error(raw);
        assert!(!safe.contains("sk-ant-"), "raw key must not appear: {}", safe);
        assert!(!safe.contains("API key"));
        assert!(safe.to_lowercase().contains("authentication"));
    }

    #[test]
    fn sanitize_masks_authentication_phrase() {
        let raw = "Provider authentication failed for user@host.internal";
        let safe = sanitize_chat_error(raw);
        assert!(!safe.contains("user@host.internal"));
        assert!(!safe.contains("authentication failed"));
    }

    #[test]
    fn sanitize_masks_credentials_phrase() {
        let raw = "Unable to load credentials from /home/user/.aws/credentials";
        let safe = sanitize_chat_error(raw);
        assert!(!safe.contains("/home/user"));
        assert!(!safe.contains("credentials"));
    }

    #[test]
    fn sanitize_maps_rate_limit_phrase() {
        assert_eq!(
            sanitize_chat_error("hit rate limit on tier 1"),
            "Rate limited. Please try again in a moment."
        );
    }

    #[test]
    fn sanitize_maps_429_status() {
        assert_eq!(
            sanitize_chat_error("got HTTP 429 from upstream"),
            "Rate limited. Please try again in a moment."
        );
    }

    #[test]
    fn sanitize_maps_timeout_phrase() {
        assert_eq!(
            sanitize_chat_error("operation timeout"),
            "Request timed out. Please try again."
        );
        assert_eq!(
            sanitize_chat_error("connection timed out after 30s"),
            "Request timed out. Please try again."
        );
    }

    #[test]
    fn sanitize_maps_500_status() {
        assert_eq!(
            sanitize_chat_error("HTTP 500 Bad Gateway"),
            "Provider error. Please try again."
        );
    }

    #[test]
    fn sanitize_maps_internal_server_error_phrase() {
        assert_eq!(
            sanitize_chat_error("internal server error from provider"),
            "Provider error. Please try again."
        );
    }

    #[test]
    fn sanitize_falls_back_to_generic_for_unknown_errors() {
        // Random raw error text — must not be echoed back to the user.
        let raw = "panic: thread 'tokio-1' panicked at 'unexpected None' src/foo.rs:42";
        let safe = sanitize_chat_error(raw);
        assert_eq!(safe, "Something went wrong. Please try again.");
        assert!(!safe.contains("panic"));
        assert!(!safe.contains("src/foo.rs"));
    }

    #[test]
    fn sanitize_first_match_wins_when_multiple_phrases_present() {
        // "API key" matched before "rate limit" in the implementation.
        let safe = sanitize_chat_error("API key invalid AND rate limit exceeded");
        assert!(safe.contains("Authentication"));
    }

    // ------------------------------------------------------------------------
    // build_tool_definitions
    // ------------------------------------------------------------------------

    #[test]
    fn build_tool_definitions_emits_provider_format() {
        let config = Config::default();
        let defs = build_tool_definitions(&config);
        // Each entry should be a JSON object with the keys providers expect.
        for d in &defs {
            assert!(d.is_object(), "tool def must be a JSON object");
            assert!(d.get("name").and_then(|v| v.as_str()).is_some(), "missing name");
            assert!(
                d.get("description").and_then(|v| v.as_str()).is_some(),
                "missing description"
            );
            assert!(
                d.get("input_schema").is_some(),
                "missing input_schema"
            );
        }
    }

    #[test]
    fn build_tool_definitions_filters_hidden_tools() {
        let config = Config::default();
        let defs = build_tool_definitions(&config);
        // Re-fetch raw catalog and confirm none of the returned definitions
        // correspond to hidden tools.
        let raw = crate::agents::tools::list_available_tools(&config);
        let hidden_names: std::collections::HashSet<&str> = raw
            .iter()
            .filter(|t| t.hidden)
            .map(|t| t.name.as_str())
            .collect();
        for d in &defs {
            let name = d["name"].as_str().unwrap();
            assert!(
                !hidden_names.contains(name),
                "build_tool_definitions returned hidden tool {}",
                name
            );
        }
    }

    #[test]
    fn build_tool_definitions_uses_input_schema_field_name() {
        // Anthropic and OpenAI both expect `input_schema` (not `parameters`)
        // when the gateway emits unified tool defs to providers. Regression
        // guard against accidentally renaming.
        let config = Config::default();
        let defs = build_tool_definitions(&config);
        if let Some(first) = defs.first() {
            assert!(first.get("input_schema").is_some());
            assert!(first.get("parameters").is_none());
        }
    }

    // ------------------------------------------------------------------------
    // MAX_TOOL_ITERATIONS
    // ------------------------------------------------------------------------

    #[test]
    fn max_tool_iterations_is_documented_constant() {
        // Bumping this changes user-visible behavior (longer agent runs);
        // pin it so the change requires a deliberate test edit.
        assert_eq!(MAX_TOOL_ITERATIONS, 25);
    }

    // ------------------------------------------------------------------
    // Steer mode (v2026.4.29) — 500ms followup debounce
    // ------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn steer_queue_holds_batch_until_debounce_elapses() {
        let mut q = SteerQueue::new(STEER_FOLLOWUP_DEBOUNCE);
        q.enqueue("first", Instant::now());
        tokio::time::advance(Duration::from_millis(200)).await;
        // 200ms after last enqueue — still inside the debounce window.
        assert!(q.drain_ready(Instant::now()).is_empty());
        assert_eq!(q.len(), 1);

        tokio::time::advance(Duration::from_millis(300)).await;
        // 500ms elapsed → batch is released.
        assert_eq!(q.drain_ready(Instant::now()), vec!["first".to_string()]);
        assert!(q.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn steer_queue_debounce_resets_on_new_followup() {
        let mut q = SteerQueue::new(STEER_FOLLOWUP_DEBOUNCE);
        q.enqueue("a", Instant::now());
        tokio::time::advance(Duration::from_millis(400)).await;
        // New followup 400ms in resets the debounce clock.
        q.enqueue("b", Instant::now());
        tokio::time::advance(Duration::from_millis(400)).await;
        assert!(
            q.drain_ready(Instant::now()).is_empty(),
            "batch must wait for quiet period after the newest followup"
        );
        tokio::time::advance(Duration::from_millis(100)).await;
        assert_eq!(
            q.drain_ready(Instant::now()),
            vec!["a".to_string(), "b".to_string()],
            "batch released in arrival order"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn steer_queue_empty_drain_is_empty() {
        let mut q = SteerQueue::new(STEER_FOLLOWUP_DEBOUNCE);
        assert!(q.drain_ready(Instant::now()).is_empty());
    }

    #[test]
    fn steer_mode_is_default_queue_mode() {
        let config = Config::default();
        assert!(steer_mode_enabled(&config), "steer is the v2026.4.29 default");
    }

    #[test]
    fn steer_mode_disabled_by_explicit_other_mode() {
        let mut config = Config::default();
        config.messages.queue = Some(crate::config::types::QueueConfig {
            mode: Some("queue".into()),
            ..Default::default()
        });
        assert!(!steer_mode_enabled(&config));

        config.messages.queue = Some(crate::config::types::QueueConfig {
            mode: Some("Steer".into()),
            ..Default::default()
        });
        assert!(steer_mode_enabled(&config));
    }

    #[test]
    fn steer_debounce_configurable_with_500ms_default() {
        let config = Config::default();
        assert_eq!(steer_debounce(&config), Duration::from_millis(500));

        let mut custom = Config::default();
        custom.messages.queue = Some(crate::config::types::QueueConfig {
            debounce_ms: Some(1200),
            ..Default::default()
        });
        assert_eq!(steer_debounce(&custom), Duration::from_millis(1200));
    }

    // ------------------------------------------------------------------
    // Blank visible user prompts (v2026.5.2)
    // ------------------------------------------------------------------

    #[test]
    fn blank_prompt_without_attachments_skipped() {
        assert!(should_skip_visible_user_prompt("", None));
        assert!(should_skip_visible_user_prompt("   \n\t", None));
        assert!(should_skip_visible_user_prompt("", Some(&[])));
    }

    #[test]
    fn media_only_prompt_not_skipped() {
        let attachments = vec![serde_json::json!({"type": "image", "url": "x"})];
        assert!(!should_skip_visible_user_prompt("", Some(&attachments)));
    }

    #[test]
    fn normal_prompt_not_skipped() {
        assert!(!should_skip_visible_user_prompt("hello", None));
    }

    #[test]
    fn runtime_event_marker_not_skipped() {
        // Internal runtime-only turns (compaction memory flush) must run.
        let marker = crate::agents::compaction::memory_flush_turn_marker();
        assert!(!should_skip_visible_user_prompt(&marker, None));
    }
}
