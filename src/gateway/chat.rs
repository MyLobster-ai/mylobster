use crate::config::Config;
use crate::gateway::protocol::*;
use crate::providers::{ProviderMessage, ProviderRequest, StreamEvent};
use crate::sessions::SessionStore;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
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
    let run_id = params
        .idempotency_key
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let session_key = &params.session_key;

    // Get or create session
    let session = sessions.get_or_create_session(session_key, config);

    // Build messages from session history + new user message
    let mut messages = session.get_history();
    messages.push(ProviderMessage {
        role: "user".to_string(),
        content: serde_json::Value::String(params.message.clone()),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    });

    // Resolve model provider
    let model = config
        .agent
        .model
        .primary_model()
        .unwrap_or_else(|| "claude-sonnet-4-6".to_string());

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
        };

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
    let tool: Box<dyn AgentTool> = match tool_name {
        "web.fetch" => Box::new(crate::agents::tools::web_fetch::WebFetchTool),
        "web.search" => Box::new(crate::agents::tools::web_search::WebSearchTool),
        "system.run" => Box::new(crate::agents::tools::bash::BashTool),
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
