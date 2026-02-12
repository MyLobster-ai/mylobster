use serde::{Deserialize, Serialize};

/// A normalized inbound message from any channel.
///
/// Channel implementations convert their platform-specific message format into
/// this common representation before handing it to the gateway session system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedMessage {
    /// Unique message id assigned by the originating platform.
    pub id: String,
    /// Channel type that produced this message (e.g. "telegram", "discord").
    pub channel: String,
    /// Account id within the channel (for multi-account setups).
    pub account_id: String,
    /// Conversation / chat identifier (channel-specific).
    pub chat_id: String,
    /// Display name of the conversation, if available.
    pub chat_name: Option<String>,
    /// The type of chat: "dm", "group", or "thread".
    pub chat_type: ChatType,
    /// Sender information.
    pub sender: NormalizedSender,
    /// Text content of the message (may be empty for media-only messages).
    pub text: String,
    /// Optional media attachments.
    #[serde(default)]
    pub attachments: Vec<NormalizedAttachment>,
    /// If this message is a reply, the id of the message it replies to.
    pub reply_to_id: Option<String>,
    /// ISO 8601 timestamp of when the message was sent.
    pub timestamp: String,
    /// Raw platform-specific payload, preserved for channel-specific tooling.
    pub raw: Option<serde_json::Value>,
}

/// Type of chat the message originated from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatType {
    Dm,
    Group,
    Thread,
}

/// Sender information, normalised across platforms.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedSender {
    /// Platform-specific user ID.
    pub id: String,
    /// Display name / username.
    pub name: String,
    /// Whether this sender is a bot.
    #[serde(default)]
    pub is_bot: bool,
}

/// A media attachment (image, file, audio, video, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedAttachment {
    /// MIME type of the attachment, if known (e.g. "image/png").
    pub mime_type: Option<String>,
    /// URL where the attachment can be downloaded.
    pub url: Option<String>,
    /// Raw bytes of the attachment (populated for small inline media).
    #[serde(skip)]
    pub data: Option<Vec<u8>>,
    /// Original file name, if available.
    pub filename: Option<String>,
    /// File size in bytes, if known.
    pub size: Option<u64>,
}

/// A normalized outbound message to be sent through a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedOutbound {
    /// Target chat id (channel-specific).
    pub chat_id: String,
    /// Text content.
    pub text: String,
    /// Optional reply-to message id.
    pub reply_to_id: Option<String>,
    /// Optional media attachments to include.
    #[serde(default)]
    pub attachments: Vec<NormalizedAttachment>,
}

/// Strip markdown formatting that is not supported by a target platform.
///
/// This is a simple pass that removes backtick code fences, bold/italic
/// markers, and other markdown constructs that render poorly on platforms
/// without rich-text support.
pub fn strip_markdown(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            // Skip triple backtick code fences (``` ... ```)
            '`' if chars.peek() == Some(&'`') => {
                chars.next(); // second `
                if chars.peek() == Some(&'`') {
                    chars.next(); // third `
                                  // Skip until closing ```
                    let mut fence_count = 0;
                    for c in chars.by_ref() {
                        if c == '`' {
                            fence_count += 1;
                            if fence_count == 3 {
                                break;
                            }
                        } else {
                            fence_count = 0;
                            result.push(c);
                        }
                    }
                } else {
                    // Inline code with double backtick — just skip the backticks
                }
            }
            // Single backtick inline code — skip the backtick itself
            '`' => {}
            // Bold / italic markers
            '*' | '_' => {}
            // Strikethrough
            '~' => {}
            _ => result.push(ch),
        }
    }

    result
}

/// Convert markdown to a simplified representation suitable for platforms that
/// support basic formatting (Telegram MarkdownV2, Slack mrkdwn, etc.).
///
/// This is a placeholder — a full implementation would parse the markdown AST
/// and emit the platform-specific markup.
pub fn markdown_to_platform(text: &str, _platform: &str) -> String {
    // For now, return the text unchanged.  Individual channel implementations
    // can override with platform-specific conversion.
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_markdown_removes_bold_and_italic() {
        assert_eq!(strip_markdown("**bold** and *italic*"), "bold and italic");
    }

    #[test]
    fn strip_markdown_removes_inline_code() {
        assert_eq!(strip_markdown("use `foo` here"), "use foo here");
    }

    #[test]
    fn strip_markdown_plain_text_unchanged() {
        let plain = "Hello, world!";
        assert_eq!(strip_markdown(plain), plain);
    }
}
