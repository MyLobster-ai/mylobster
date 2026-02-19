use crate::config::Config;
use crate::gateway::protocol::*;
use crate::providers::{ModelProvider, ProviderMessage, ProviderRequest, StreamEvent};
use crate::sessions::SessionStore;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info};
use uuid::Uuid;

/// Handle a chat request and stream events back.
pub async fn process_chat(
    config: &Config,
    sessions: &SessionStore,
    params: &ChatSendParams,
    event_tx: mpsc::Sender<ChatEvent>,
) -> Result<()> {
    let run_id = Uuid::new_v4().to_string();
    let session_key = &params.session_key;

    // Get or create session
    let session = sessions.get_or_create_session(session_key, config);

    // Build messages from session history + new message
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

    // Create request
    let request = ProviderRequest {
        model: model.clone(),
        messages,
        max_tokens: None,
        temperature: None,
        stream: true,
        tools: None,
        tool_choice: None,
    };

    // Stream response
    let mut seq = 0u64;
    let mut full_content = String::new();

    match provider.stream_chat(request).await {
        Ok(mut stream) => {
            while let Some(event) = stream.recv().await {
                match event {
                    StreamEvent::Delta(text) => {
                        full_content.push_str(&text);
                        let chat_event = ChatEvent {
                            run_id: run_id.clone(),
                            session_key: session_key.clone(),
                            seq,
                            state: ChatEventState::Delta,
                            message: Some(serde_json::json!({
                                "role": "assistant",
                                "content": text
                            })),
                            error_message: None,
                            usage: None,
                            stop_reason: None,
                        };
                        seq += 1;
                        let _ = event_tx.send(chat_event).await;
                    }
                    StreamEvent::ToolCall(tool_call) => {
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
                        // Add assistant message to session
                        session.add_message(ProviderMessage {
                            role: "assistant".to_string(),
                            content: serde_json::Value::String(full_content.clone()),
                            name: None,
                            tool_call_id: None,
                            tool_calls: None,
                        });

                        let chat_event = ChatEvent {
                            run_id: run_id.clone(),
                            session_key: session_key.clone(),
                            seq,
                            state: ChatEventState::Final,
                            message: Some(serde_json::json!({
                                "role": "assistant",
                                "content": full_content
                            })),
                            error_message: None,
                            usage: Some(usage),
                            stop_reason: Some("end_turn".to_string()),
                        };
                        let _ = event_tx.send(chat_event).await;
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
                        break;
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
        }
    }

    Ok(())
}
