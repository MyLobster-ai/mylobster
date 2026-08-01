//! `sessions_send` target validation (v2026.5.2 parity).
//!
//! Upstream rejects `sessions_send` targets that resolve to thread-scoped
//! chat sessions: those sessions are bound to an external thread/topic lane
//! and injecting agent-to-agent sends into them would surface in the middle
//! of an unrelated human thread (and confuse thread routing). Callers should
//! address the canonical session instead.

/// Why a `sessions_send` target was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SendTargetError {
    #[error(
        "sessions_send target '{session_key}' resolves to a thread-scoped chat session; \
         send to the canonical session instead"
    )]
    ThreadScopedChatSession { session_key: String },
}

/// Whether a resolved session key denotes a thread-scoped chat session
/// (channel thread / forum-topic lane), as opposed to a canonical main,
/// group, DM, or subagent session.
pub fn is_thread_scoped_chat_session(session_key: &str) -> bool {
    let key = session_key.to_ascii_lowercase();
    // Thread/topic lane markers used by channel routing (Telegram forum
    // topics, Discord/Slack threads).
    const MARKERS: [&str; 2] = [":thread:", ":topic:"];
    if MARKERS.iter().any(|m| key.contains(m)) {
        return true;
    }
    // Topic-suffixed transcript identities (`<session>.topic-<id>` /
    // `<session>-topic-<id>`) resolve to per-topic lanes as well.
    const SUFFIX_MARKERS: [&str; 2] = [".topic-", "-topic-"];
    SUFFIX_MARKERS
        .iter()
        .any(|m| key.rfind(m).is_some_and(|idx| idx + m.len() < key.len()))
}

/// Validate a resolved `sessions_send` target key.
pub fn validate_sessions_send_target(resolved_key: &str) -> Result<(), SendTargetError> {
    if is_thread_scoped_chat_session(resolved_key) {
        return Err(SendTargetError::ThreadScopedChatSession {
            session_key: resolved_key.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_scoped_keys_are_rejected() {
        for key in [
            "telegram:group:1:topic:42",
            "discord:guild:9:thread:12345",
            "slack:T1:C2:thread:167.89",
            "sess-1.topic-42",
            "sess-1-topic-42",
            "TELEGRAM:G:TOPIC:7",
        ] {
            assert!(is_thread_scoped_chat_session(key), "{key}");
            let err = validate_sessions_send_target(key).unwrap_err();
            assert!(matches!(
                err,
                SendTargetError::ThreadScopedChatSession { .. }
            ));
        }
    }

    #[test]
    fn canonical_sessions_are_accepted() {
        for key in [
            "default",
            "telegram:group:1",
            "telegram:dm:123",
            "subagent:task-1",
            "discord:guild:9:channel:12",
            "my-topical-session", // contains "topic" but not a lane marker
        ] {
            assert!(!is_thread_scoped_chat_session(key), "{key}");
            assert!(validate_sessions_send_target(key).is_ok(), "{key}");
        }
    }

    #[test]
    fn bare_trailing_topic_marker_is_not_a_lane() {
        // Marker with nothing after it is not a topic lane identity.
        assert!(!is_thread_scoped_chat_session("sess-1.topic-"));
        assert!(!is_thread_scoped_chat_session("sess-1-topic-"));
    }
}
