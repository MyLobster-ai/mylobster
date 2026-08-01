//! Auto-reply delivery policy (OpenClaw v2026.5.2 Routing/Auth rows +
//! v2026.6.1 visible-replies groundwork).
//!
//! Decides how a finished agent turn reaches the user:
//! - **Group-chat tool policy precedes the fallback-delivery decision**: when
//!   the group's tool policy denies the `message` tool, the turn must fall
//!   back to automatic source delivery *before* any message-tool-only
//!   suppression logic runs — otherwise replies silently vanish.
//! - **Message-tool unavailability falls back to automatic source delivery**
//!   for precomputed message-tool-only replies.
//! - `messages.visibleReplies` resolution: legacy boolean config maps onto
//!   the v2026.6.1 `automatic` / `message_tool` modes.
//! - `NO_REPLY` is only honored in automatic group/channel contexts.

// ============================================================================
// visibleReplies resolution (v2026.6.1 groundwork)
// ============================================================================

/// How agent output reaches the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibleRepliesMode {
    /// Final text is delivered automatically to the source conversation.
    Automatic,
    /// Output reaches the user only via explicit `message` tool sends.
    MessageTool,
}

/// Resolve the legacy `messages.visibleReplies` boolean into a mode.
/// `Some(true)` → message-tool-only; anything else → automatic (the
/// v2026.6.1 default).
pub fn resolve_visible_replies(configured: Option<bool>) -> VisibleRepliesMode {
    match configured {
        Some(true) => VisibleRepliesMode::MessageTool,
        _ => VisibleRepliesMode::Automatic,
    }
}

// ============================================================================
// Auto-reply delivery decision (v2026.5.2)
// ============================================================================

/// Context for an auto-reply delivery decision.
#[derive(Debug, Clone, Copy)]
pub struct ReplyDeliveryContext {
    /// Resolved visible-replies mode for this conversation.
    pub visible_replies: VisibleRepliesMode,
    /// Whether this is a group/channel chat (vs a DM/direct chat).
    pub is_group_chat: bool,
    /// Whether the `message` tool is available for this run (present in the
    /// run's tool list and not policy-hidden).
    pub message_tool_available: bool,
    /// Whether the group-chat tool policy allows the `message` tool for this
    /// sender/room. Ignored for non-group chats.
    pub group_tool_policy_allows_message_tool: bool,
}

/// How the final reply is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyDelivery {
    /// Deliver automatically to the source conversation.
    AutomaticSource,
    /// Expect the model's explicit `message` tool send(s); bare final text
    /// is suppressed.
    MessageToolOnly,
}

/// Decide the delivery route for an auto-reply.
///
/// Order matters (v2026.5.2): the group-chat tool policy is consulted
/// **before** the fallback-delivery decision, and message-tool
/// unavailability always falls back to automatic source delivery instead of
/// suppressing the reply.
pub fn decide_auto_reply_delivery(ctx: ReplyDeliveryContext) -> ReplyDelivery {
    match ctx.visible_replies {
        VisibleRepliesMode::Automatic => ReplyDelivery::AutomaticSource,
        VisibleRepliesMode::MessageTool => {
            // 1. Group-chat tool policy precedes everything: if the policy
            //    denies the message tool in this room, message-tool-only
            //    delivery is impossible → automatic source delivery.
            if ctx.is_group_chat && !ctx.group_tool_policy_allows_message_tool {
                return ReplyDelivery::AutomaticSource;
            }
            // 2. Message tool unavailable → fall back to automatic source
            //    delivery (never drop the reply).
            if !ctx.message_tool_available {
                return ReplyDelivery::AutomaticSource;
            }
            ReplyDelivery::MessageToolOnly
        }
    }
}

// ============================================================================
// NO_REPLY policy (v2026.6.1, partial)
// ============================================================================

/// The literal token models emit to decline replying.
pub const NO_REPLY_TOKEN: &str = "NO_REPLY";

/// Whether a final text is a bare `NO_REPLY` decline.
pub fn is_no_reply(text: &str) -> bool {
    text.trim() == NO_REPLY_TOKEN
}

/// `NO_REPLY` is honored only for automatic group/channel delivery; in DMs
/// (and message-tool-only rooms) it is treated as an empty reply problem
/// rather than a legitimate decline.
pub fn no_reply_allowed(ctx: ReplyDeliveryContext) -> bool {
    ctx.is_group_chat && matches!(ctx.visible_replies, VisibleRepliesMode::Automatic)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(
        visible: VisibleRepliesMode,
        group: bool,
        tool_avail: bool,
        policy_allows: bool,
    ) -> ReplyDeliveryContext {
        ReplyDeliveryContext {
            visible_replies: visible,
            is_group_chat: group,
            message_tool_available: tool_avail,
            group_tool_policy_allows_message_tool: policy_allows,
        }
    }

    // ------------------------------------------------------------------
    // visibleReplies resolution
    // ------------------------------------------------------------------

    #[test]
    fn visible_replies_default_is_automatic() {
        assert_eq!(resolve_visible_replies(None), VisibleRepliesMode::Automatic);
        assert_eq!(resolve_visible_replies(Some(false)), VisibleRepliesMode::Automatic);
    }

    #[test]
    fn visible_replies_true_is_message_tool() {
        assert_eq!(resolve_visible_replies(Some(true)), VisibleRepliesMode::MessageTool);
    }

    // ------------------------------------------------------------------
    // delivery decision
    // ------------------------------------------------------------------

    #[test]
    fn automatic_mode_always_automatic() {
        for group in [false, true] {
            for avail in [false, true] {
                assert_eq!(
                    decide_auto_reply_delivery(ctx(VisibleRepliesMode::Automatic, group, avail, true)),
                    ReplyDelivery::AutomaticSource
                );
            }
        }
    }

    #[test]
    fn message_tool_mode_uses_message_tool_when_available_and_allowed() {
        assert_eq!(
            decide_auto_reply_delivery(ctx(VisibleRepliesMode::MessageTool, true, true, true)),
            ReplyDelivery::MessageToolOnly
        );
        assert_eq!(
            decide_auto_reply_delivery(ctx(VisibleRepliesMode::MessageTool, false, true, true)),
            ReplyDelivery::MessageToolOnly
        );
    }

    #[test]
    fn group_tool_policy_denial_precedes_fallback_decision() {
        // Policy denies the message tool in this group → automatic source
        // delivery even though the tool is nominally available.
        assert_eq!(
            decide_auto_reply_delivery(ctx(VisibleRepliesMode::MessageTool, true, true, false)),
            ReplyDelivery::AutomaticSource
        );
    }

    #[test]
    fn group_policy_ignored_for_direct_chats() {
        // A DM is not subject to the *group* tool policy.
        assert_eq!(
            decide_auto_reply_delivery(ctx(VisibleRepliesMode::MessageTool, false, true, false)),
            ReplyDelivery::MessageToolOnly
        );
    }

    #[test]
    fn message_tool_unavailable_falls_back_to_automatic() {
        assert_eq!(
            decide_auto_reply_delivery(ctx(VisibleRepliesMode::MessageTool, false, false, true)),
            ReplyDelivery::AutomaticSource
        );
        assert_eq!(
            decide_auto_reply_delivery(ctx(VisibleRepliesMode::MessageTool, true, false, true)),
            ReplyDelivery::AutomaticSource
        );
    }

    // ------------------------------------------------------------------
    // NO_REPLY policy
    // ------------------------------------------------------------------

    #[test]
    fn no_reply_token_detection() {
        assert!(is_no_reply("NO_REPLY"));
        assert!(is_no_reply("  NO_REPLY \n"));
        assert!(!is_no_reply("NO_REPLY needed here"));
        assert!(!is_no_reply("no_reply"));
    }

    #[test]
    fn no_reply_only_in_automatic_group_context() {
        assert!(no_reply_allowed(ctx(VisibleRepliesMode::Automatic, true, true, true)));
        assert!(!no_reply_allowed(ctx(VisibleRepliesMode::Automatic, false, true, true)));
        assert!(!no_reply_allowed(ctx(VisibleRepliesMode::MessageTool, true, true, true)));
    }
}
