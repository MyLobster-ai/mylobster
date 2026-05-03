//! Log redaction patterns and helpers (v2026.5.2 parity).
//!
//! Mirrors the OpenClaw v2026.5.2 fix: extend default log/tool-payload
//! redaction patterns so cloud provider API keys, payment credentials, and
//! credential-bearing URL/header fragments don't leak through log sinks.
//!
//! Upstream changes captured here:
//! - Tencent Cloud, Alibaba Cloud, HuggingFace, Replicate API key keywords
//!   (#58162).
//! - Payment credential field names: card number, CVC/CVV, shared payment
//!   token, payment credential.
//! - `sanitizeForLog`: redact `?password=…` / `?token=…` query params and
//!   `Authorization:` headers (CWE-532, BlueBubbles patch).

use regex::Regex;
use std::sync::OnceLock;

/// Replacement string used when a redactable substring is found.
pub const REDACTED: &str = "[REDACTED]";

/// Field/keyword names whose `=` / `:` values should be redacted. Each
/// keyword is matched case-insensitively as a whole token (allowing `_` and
/// `-` separators). Listed longest-first inside the regex so that
/// `tencent_cloud_secret_key` wins over a bare `secret_key` match.
const SECRET_KEYWORDS: &[&str] = &[
    // Cloud providers (v2026.5.2: #58162) — listed first because they are
    // the longest and most specific.
    "tencent_cloud_secret_key",
    "tencent_cloud_api_key",
    "tencent_secret_key",
    "tencent_api_key",
    "alibaba_cloud_access_key",
    "alibaba_cloud_secret_key",
    "aliyun_access_key",
    "aliyun_secret_key",
    "huggingface_api_token",
    "huggingface_token",
    "replicate_api_token",
    "hf_token",
    // Payment credentials (v2026.5.2)
    "shared_payment_token",
    "payment_credential",
    "card_number",
    "card_cvv",
    "cvc",
    "cvv",
    // Generic credentials
    "refresh_token",
    "bearer_token",
    "access_token",
    "secret_key",
    "api_key",
    "authorization",
    "password",
    "passwd",
];

/// Build the field-assignment regex. Matches each keyword followed by
/// `:`/`=` and captures the value up to the next quote, comma, whitespace,
/// or `}`. Underscores in keywords are normalized into a `[_-]` class so
/// `api_key`, `api-key`, and `apiKey` (via case-insensitive flag) all match.
fn keyword_assignment_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Sort longest-first; replace `_` with `[_-]?` so dashed/no-separator
        // forms also match. Anchor each keyword between a non-word char (or
        // start) and the value separator.
        let mut alts: Vec<String> = SECRET_KEYWORDS
            .iter()
            .map(|kw| kw.replace('_', "[_-]?"))
            .collect();
        alts.sort_by_key(|s| std::cmp::Reverse(s.len()));
        let alternation = alts.join("|");

        // sep allows an optional closing quote on the keyword (JSON-style
        // `"key":`), then `:` or `=`, then an optional opening quote on the
        // value. The value class excludes `&` so URL query rewrites earlier
        // in the pipeline are not clobbered by a keyword pass that would
        // greedily eat the rest of the query string.
        let pattern = format!(
            r#"(?i)(?P<lead>(?:^|[^A-Za-z0-9_]))(?P<kw>{kw})(?P<sep>"?\s*[:=]\s*"?)(?P<val>[^"\s,}}&]+)"#,
            kw = alternation
        );
        Regex::new(&pattern).expect("redact: keyword assignment regex valid")
    })
}

fn url_credential_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            "(?i)([?&](?:password|token|api[_-]?key|access[_-]?token)=)([^&\\s\"]+)",
        )
        .expect("redact: url credential regex valid")
    })
}

fn authorization_header_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)(authorization\s*:\s*)([^\r\n]+)"#)
            .expect("redact: authorization header regex valid")
    })
}

/// Redact secrets in arbitrary log/tool-payload text. Replaces:
/// - keyword `=`/`:` value pairs (see [`SECRET_KEYWORDS`])
/// - `?password=…` / `?token=…` / `?api_key=…` URL query params
/// - `Authorization: …` header values
///
/// The shape of the surrounding text is preserved — only the secret value
/// portion is replaced with [`REDACTED`]. Authorization headers and URL
/// credentials are processed first so that field-shaped keyword passes do
/// not double-redact already-replaced values.
pub fn redact_text(input: &str) -> String {
    let after_auth = authorization_header_regex().replace_all(input, |caps: &regex::Captures| {
        format!("{}{}", &caps[1], REDACTED)
    });

    let after_url = url_credential_regex().replace_all(&after_auth, |caps: &regex::Captures| {
        format!("{}{}", &caps[1], REDACTED)
    });

    let after_kw = keyword_assignment_regex().replace_all(&after_url, |caps: &regex::Captures| {
        format!(
            "{lead}{kw}{sep}{redacted}",
            lead = &caps["lead"],
            kw = &caps["kw"],
            sep = &caps["sep"],
            redacted = REDACTED
        )
    });

    after_kw.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_redacted(input: &str, leak: &str) {
        let out = redact_text(input);
        assert!(out.contains(REDACTED), "expected [REDACTED] in: {out}");
        assert!(
            !out.contains(leak),
            "leaked `{leak}` in: {out} (input: {input})"
        );
    }

    #[test]
    fn redacts_generic_api_key_assignment() {
        assert_redacted("api_key=sk-secret-abc123", "sk-secret-abc123");
    }

    #[test]
    fn redacts_huggingface_token() {
        assert_redacted(r#"{"hf_token": "hf_thisIsSecret"}"#, "hf_thisIsSecret");
    }

    #[test]
    fn redacts_tencent_cloud_secret_key() {
        assert_redacted(
            "tencent_cloud_secret_key: AKIDxyz_redact_me",
            "AKIDxyz_redact_me",
        );
    }

    #[test]
    fn redacts_alibaba_cloud_access_key() {
        assert_redacted("alibaba_cloud_access_key=LTAI5tBeefcafe", "LTAI5tBeefcafe");
    }

    #[test]
    fn redacts_replicate_api_token() {
        assert_redacted("replicate_api_token=r8_aaaaaaaaaaaa", "r8_aaaaaaaaaaaa");
    }

    #[test]
    fn redacts_card_number_field() {
        assert_redacted(
            r#"{"card_number": "4242424242424242"}"#,
            "4242424242424242",
        );
    }

    #[test]
    fn redacts_cvv_field() {
        assert_redacted("cvv=123", "=123");
    }

    #[test]
    fn redacts_shared_payment_token() {
        assert_redacted(r#"shared_payment_token: "spt_abcdef""#, "spt_abcdef");
    }

    #[test]
    fn redacts_url_password_query_param() {
        let s = redact_text("https://example.com/x?password=p4ssw0rd&id=42");
        assert!(!s.contains("p4ssw0rd"), "leaked: {s}");
        assert!(s.contains("id=42"), "preserved: {s}");
    }

    #[test]
    fn redacts_url_token_query_param() {
        let s = redact_text("https://api.example.com/x?token=eyJ.live.t0ken&page=1");
        assert!(!s.contains("eyJ.live.t0ken"), "leaked: {s}");
        assert!(s.contains("page=1"));
    }

    #[test]
    fn redacts_authorization_header() {
        let s = redact_text("Authorization: Bearer eyJhbGciOi.live.token\nOther: keep");
        assert!(!s.contains("eyJhbGciOi.live.token"), "leaked: {s}");
        assert!(s.contains("Other: keep"));
    }

    #[test]
    fn does_not_touch_unrelated_text() {
        let s = redact_text("nothing sensitive here");
        assert_eq!(s, "nothing sensitive here");
    }

    #[test]
    fn redacts_password_field() {
        assert_redacted(r#"password="hunter2""#, "hunter2");
    }

    #[test]
    fn longer_keyword_wins_over_shorter_substring() {
        // Verifies tencent_cloud_secret_key matches as a whole instead of
        // bare `secret_key` capturing only the tail.
        let out = redact_text("tencent_cloud_secret_key: AKID_long_specific_value");
        assert!(
            out.contains("tencent_cloud_secret_key"),
            "kept full keyword in output: {out}"
        );
        assert!(!out.contains("AKID_long_specific_value"), "leaked: {out}");
    }
}
