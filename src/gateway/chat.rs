use crate::config::Config;
use crate::gateway::protocol::*;
use crate::hooks::{HookEvent, HookResult, SharedHookRegistry};
use crate::providers::{ProviderMessage, ProviderRequest, StreamEvent, ThinkingConfig};
use crate::sessions::SessionStore;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Maximum number of tool loop iterations before stopping.
const MAX_TOOL_ITERATIONS: usize = 25;

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

    // Agentic loop: call provider, execute tools, repeat
    let mut iteration = 0;
    let mut seq = 0u64;

    loop {
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
                                error_message: Some(e),
                                usage: None,
                                stop_reason: None,
                            };
                            let _ = event_tx.send(chat_event).await;
                            return Ok(());
                        }
                    }
                }
            }
            Err(e) => {
                let chat_event = ChatEvent {
                    run_id: run_id.clone(),
                    session_key: session_key.clone(),
                    seq: 0,
                    state: ChatEventState::Error,
                    message: None,
                    error_message: Some(format!("Provider error: {}", e)),
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
                    "text": full_content
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
