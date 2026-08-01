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

// ============================================================================
// Cross-channel media (v2026.4.27 "non-image attachments via chat.send" +
// v2026.6.x–7.1 "Cross-channel media" row)
// ============================================================================

/// Kind of an outbound/inbound attachment, inferred from MIME + filename.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentKind {
    Image,
    Audio,
    Video,
    Document,
}

/// Classify an attachment so channel senders can pick the native send API
/// (photo vs voice-note vs video vs document). Non-image attachments flow
/// through `chat.send` as documents unless a richer native path exists
/// (OpenClaw v2026.4.27; telegram/discord senders: noted handoff).
pub fn classify_attachment(mime_type: Option<&str>, filename: Option<&str>) -> AttachmentKind {
    if let Some(mime) = mime_type {
        let mime = mime.trim().to_lowercase();
        if mime.starts_with("image/") {
            return AttachmentKind::Image;
        }
        if mime.starts_with("audio/") {
            return AttachmentKind::Audio;
        }
        if mime.starts_with("video/") {
            return AttachmentKind::Video;
        }
        if !mime.is_empty() && mime != "application/octet-stream" {
            return AttachmentKind::Document;
        }
    }
    if let Some(name) = filename {
        let lower = name.trim().to_lowercase();
        let ext = lower.rsplit('.').next().unwrap_or("");
        return match ext {
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "heif" | "bmp" => {
                AttachmentKind::Image
            }
            "mp3" | "ogg" | "oga" | "opus" | "m4a" | "wav" | "flac" | "aac" => {
                AttachmentKind::Audio
            }
            "mp4" | "mov" | "webm" | "mkv" | "avi" => AttachmentKind::Video,
            _ => AttachmentKind::Document,
        };
    }
    AttachmentKind::Document
}

/// A `MEDIA:` directive extracted from reply text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaDirective {
    /// Path or URL as written (after `MEDIA:`), `~/` left intact.
    pub raw: String,
}

/// Extract `MEDIA:<path-or-url>` directive lines from reply text.
///
/// Returns the cleaned text (directive lines removed) and the directives in
/// order. `MEDIA:` directives become real attachments rather than being
/// echoed as text (v2026.6.x "MEDIA directives as attachments").
pub fn extract_media_directives(text: &str) -> (String, Vec<MediaDirective>) {
    let mut directives = Vec::new();
    let mut kept: Vec<&str> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("MEDIA:") {
            let raw = rest.trim();
            if !raw.is_empty() {
                directives.push(MediaDirective {
                    raw: raw.to_string(),
                });
                continue;
            }
        }
        kept.push(line);
    }
    (kept.join("\n").trim().to_string(), directives)
}

/// Resolve a `MEDIA:` path: `~/x` is home-relative (v2026.7.1 "home-relative
/// `MEDIA:~/` paths"); URLs and absolute paths pass through.
pub fn resolve_media_path(raw: &str, home: Option<&std::path::Path>) -> String {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("~/") {
        if let Some(home) = home {
            return home.join(rest).to_string_lossy().to_string();
        }
    }
    trimmed.to_string()
}

/// Explicit unavailable-attachment notice (v2026.6.x: no phantom
/// placeholders — callers append this text instead of inventing a fake
/// attachment).
pub fn unavailable_attachment_notice(name: Option<&str>) -> String {
    match name {
        Some(n) if !n.trim().is_empty() => {
            format!("[attachment unavailable: {}]", n.trim())
        }
        _ => "[attachment unavailable]".to_string(),
    }
}

/// One attachment send operation in a channel send plan.
#[derive(Debug, Clone)]
pub struct AttachmentSend {
    pub kind: AttachmentKind,
    pub attachment: NormalizedAttachment,
    /// Caption carried with this attachment (preserved, never dropped).
    pub caption: Option<String>,
}

/// A channel-agnostic outbound send plan.
#[derive(Debug, Clone, Default)]
pub struct SendPlan {
    /// Standalone text to send (empty when fully carried as a caption).
    pub text: String,
    /// Attachment sends, in order.
    pub attachments: Vec<AttachmentSend>,
    /// Notices for attachments that could not be resolved.
    pub notices: Vec<String>,
}

/// Build a send plan from outbound text + attachments.
///
/// Caption preservation (v2026.6.x): when the channel supports captions and
/// the text fits `caption_limit`, the text rides as the first attachment's
/// caption; otherwise the text is sent standalone and attachments follow.
/// Attachments without url/data produce explicit unavailable notices instead
/// of phantom placeholder sends.
pub fn build_send_plan(
    text: &str,
    attachments: &[NormalizedAttachment],
    supports_captions: bool,
    caption_limit: usize,
) -> SendPlan {
    let mut plan = SendPlan::default();
    let mut remaining_text = text.trim().to_string();

    for (idx, attachment) in attachments.iter().enumerate() {
        let available = attachment.url.is_some() || attachment.data.is_some();
        if !available {
            plan.notices
                .push(unavailable_attachment_notice(attachment.filename.as_deref()));
            continue;
        }
        let kind = classify_attachment(
            attachment.mime_type.as_deref(),
            attachment.filename.as_deref(),
        );
        let caption = if idx == 0
            && supports_captions
            && !remaining_text.is_empty()
            && remaining_text.chars().count() <= caption_limit
        {
            let c = remaining_text.clone();
            remaining_text.clear();
            Some(c)
        } else {
            None
        };
        plan.attachments.push(AttachmentSend {
            kind,
            attachment: attachment.clone(),
            caption,
        });
    }

    plan.text = remaining_text;
    plan
}

// ============================================================================
// Typed presentation actions + generic polls (v2026.6.x–7.1)
// ============================================================================

/// Typed presentation action attached to an outbound message (generic
/// replacement for per-channel button/keyboard payloads).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PresentationAction {
    /// Approve/deny pair for exec approvals.
    Approval { id: String, label: String },
    /// Run a chat command when pressed.
    Command { command: String, label: String },
    /// Open a URL.
    Url { url: String, label: String },
    /// Launch an embedded web app (Telegram Mini App etc.).
    WebApp { url: String, label: String },
    /// Single-select from options.
    Select { id: String, options: Vec<String> },
}

/// Generic poll payload for channels with native polls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PollPayload {
    pub question: String,
    pub options: Vec<String>,
    #[serde(default)]
    pub multi_select: bool,
}

impl PollPayload {
    /// Clamp options to a channel's cap (e.g. Telegram's 10), dropping
    /// extras rather than failing the send.
    pub fn clamped(mut self, max_options: usize) -> Self {
        if max_options > 0 && self.options.len() > max_options {
            self.options.truncate(max_options);
        }
        self
    }
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

    // ====================================================================
    // Cross-channel media
    // ====================================================================

    #[test]
    fn attachment_classification() {
        assert_eq!(
            classify_attachment(Some("image/png"), None),
            AttachmentKind::Image
        );
        assert_eq!(
            classify_attachment(Some("audio/ogg"), None),
            AttachmentKind::Audio
        );
        assert_eq!(
            classify_attachment(Some("video/mp4"), None),
            AttachmentKind::Video
        );
        assert_eq!(
            classify_attachment(Some("application/pdf"), None),
            AttachmentKind::Document
        );
        // Octet-stream falls back to filename sniffing.
        assert_eq!(
            classify_attachment(Some("application/octet-stream"), Some("x.jpg")),
            AttachmentKind::Image
        );
        assert_eq!(
            classify_attachment(None, Some("voice.opus")),
            AttachmentKind::Audio
        );
        assert_eq!(
            classify_attachment(None, Some("report.docx")),
            AttachmentKind::Document
        );
        assert_eq!(classify_attachment(None, None), AttachmentKind::Document);
    }

    #[test]
    fn media_directive_extraction() {
        let text = "Here you go\nMEDIA:~/pics/cat.png\nMEDIA: https://x.test/a.pdf\nBye";
        let (clean, directives) = extract_media_directives(text);
        assert_eq!(clean, "Here you go\nBye");
        assert_eq!(directives.len(), 2);
        assert_eq!(directives[0].raw, "~/pics/cat.png");
        assert_eq!(directives[1].raw, "https://x.test/a.pdf");
        // Empty directive lines are kept as text.
        let (clean, directives) = extract_media_directives("MEDIA:\nhello");
        assert_eq!(clean, "MEDIA:\nhello");
        assert!(directives.is_empty());
    }

    #[test]
    fn media_path_resolution() {
        let home = std::path::Path::new("/home/lobster");
        assert_eq!(
            resolve_media_path("~/pics/cat.png", Some(home)),
            "/home/lobster/pics/cat.png"
        );
        assert_eq!(
            resolve_media_path("https://x.test/a.pdf", Some(home)),
            "https://x.test/a.pdf"
        );
        assert_eq!(resolve_media_path("/abs/p.txt", Some(home)), "/abs/p.txt");
        // No home dir: raw preserved.
        assert_eq!(resolve_media_path("~/x", None), "~/x");
    }

    #[test]
    fn send_plan_caption_preservation_and_notices() {
        let attachments = vec![
            NormalizedAttachment {
                mime_type: Some("image/png".into()),
                url: Some("https://x.test/a.png".into()),
                data: None,
                filename: Some("a.png".into()),
                size: None,
            },
            // Unavailable attachment: no url, no data.
            NormalizedAttachment {
                mime_type: Some("application/pdf".into()),
                url: None,
                data: None,
                filename: Some("gone.pdf".into()),
                size: None,
            },
        ];
        let plan = build_send_plan("caption text", &attachments, true, 1024);
        assert_eq!(plan.text, "");
        assert_eq!(plan.attachments.len(), 1);
        assert_eq!(plan.attachments[0].kind, AttachmentKind::Image);
        assert_eq!(plan.attachments[0].caption.as_deref(), Some("caption text"));
        assert_eq!(plan.notices, vec!["[attachment unavailable: gone.pdf]"]);

        // No caption support: text stays standalone.
        let plan = build_send_plan("caption text", &attachments[..1], false, 1024);
        assert_eq!(plan.text, "caption text");
        assert_eq!(plan.attachments[0].caption, None);

        // Over-limit caption stays standalone.
        let plan = build_send_plan("caption text", &attachments[..1], true, 3);
        assert_eq!(plan.text, "caption text");
        assert_eq!(plan.attachments[0].caption, None);
    }

    #[test]
    fn poll_clamping() {
        let poll = PollPayload {
            question: "q".into(),
            options: (0..12).map(|i| format!("o{i}")).collect(),
            multi_select: false,
        }
        .clamped(10);
        assert_eq!(poll.options.len(), 10);
    }
}
