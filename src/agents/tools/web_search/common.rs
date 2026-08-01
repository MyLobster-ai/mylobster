//! Shared web-search provider helpers (v2026.7.1 parity).
//!
//! Ports OpenClaw's `web-search-provider-common.ts`: count clamping, timeout /
//! cache-TTL resolution, freshness normalization, and ISO date-range parsing.

use serde_json::json;

/// Default number of results a provider returns when the caller does not ask.
pub const DEFAULT_SEARCH_COUNT: u32 = 5;
/// Hard cap for provider result counts.
pub const MAX_SEARCH_COUNT: u32 = 10;
/// Default HTTP timeout for search calls in seconds.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 30;
/// Default cache TTL in minutes for search payloads.
pub const DEFAULT_CACHE_TTL_MINUTES: u64 = 15;

pub const WEB_TOOLS_DOCS_URL: &str = "https://docs.openclaw.ai/tools/web";

/// Clamp a requested result count into `1..=MAX_SEARCH_COUNT`.
pub fn resolve_search_count(value: Option<u64>, fallback: u32) -> u32 {
    let parsed = value.map(|v| v as u32).unwrap_or(fallback);
    parsed.clamp(1, MAX_SEARCH_COUNT)
}

/// Resolve the provider HTTP timeout from search config.
pub fn resolve_search_timeout_seconds(configured: Option<u64>) -> u64 {
    configured.filter(|v| *v >= 1).unwrap_or(DEFAULT_TIMEOUT_SECONDS)
}

/// Resolve the payload cache TTL in milliseconds from search config.
pub fn resolve_search_cache_ttl_ms(cache_ttl_minutes: Option<u64>) -> u64 {
    cache_ttl_minutes.unwrap_or(DEFAULT_CACHE_TTL_MINUTES) * 60_000
}

/// Hostname of a result URL, used as `siteName` metadata.
pub fn resolve_site_name(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Structured provider error payload matching upstream's `{error, message, docs}`.
pub fn search_error_payload(error: &str, message: &str) -> serde_json::Value {
    json!({
        "error": error,
        "message": message,
        "docs": WEB_TOOLS_DOCS_URL,
    })
}

// ============================================================================
// Freshness / date filters
// ============================================================================

/// Providers differ in which literal freshness values their API accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshnessProvider {
    /// Accepts `pd/pw/pm/py` shortcuts and explicit ISO ranges.
    Brave,
    /// Accepts `day/week/month/year` recency buckets.
    Perplexity,
}

fn shortcut_to_recency(value: &str) -> Option<&'static str> {
    match value {
        "pd" => Some("day"),
        "pw" => Some("week"),
        "pm" => Some("month"),
        "py" => Some("year"),
        _ => None,
    }
}

fn recency_to_shortcut(value: &str) -> Option<&'static str> {
    match value {
        "day" => Some("pd"),
        "week" => Some("pw"),
        "month" => Some("pm"),
        "year" => Some("py"),
        _ => None,
    }
}

pub fn is_valid_iso_date(value: &str) -> bool {
    if value.len() != 10 {
        return false;
    }
    let bytes = value.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let (y, m, d) = (&value[0..4], &value[5..7], &value[8..10]);
    if !y.chars().all(|c| c.is_ascii_digit())
        || !m.chars().all(|c| c.is_ascii_digit())
        || !d.chars().all(|c| c.is_ascii_digit())
    {
        return false;
    }
    let (year, month, day): (i32, u32, u32) = match (y.parse(), m.parse(), d.parse()) {
        (Ok(y), Ok(m), Ok(d)) => (y, m, d),
        _ => return false,
    };
    chrono::NaiveDate::from_ymd_opt(year, month, day).is_some()
}

/// Accepts ISO dates plus Perplexity `M/D/YYYY` dates, canonicalized to ISO.
pub fn normalize_to_iso_date(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if is_valid_iso_date(trimmed) {
        return Some(trimmed.to_string());
    }
    // M/D/YYYY
    let parts: Vec<&str> = trimmed.split('/').collect();
    if parts.len() == 3
        && parts[0].len() <= 2
        && parts[1].len() <= 2
        && parts[2].len() == 4
        && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    {
        let iso = format!("{}-{:0>2}-{:0>2}", parts[2], parts[0], parts[1]);
        if is_valid_iso_date(&iso) {
            return Some(iso);
        }
    }
    None
}

/// Converts shared freshness names into provider-specific values.
///
/// Brave keeps `pd/pw/pm/py` plus explicit `YYYY-MM-DDtoYYYY-MM-DD` ranges;
/// Perplexity-style providers get `day/week/month/year`.
pub fn normalize_freshness(value: &str, provider: FreshnessProvider) -> Option<String> {
    let lower = value.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    if let Some(recency) = shortcut_to_recency(&lower) {
        return Some(match provider {
            FreshnessProvider::Brave => lower,
            FreshnessProvider::Perplexity => recency.to_string(),
        });
    }
    if recency_to_shortcut(&lower).is_some() {
        return Some(match provider {
            FreshnessProvider::Perplexity => lower.clone(),
            FreshnessProvider::Brave => recency_to_shortcut(&lower).unwrap().to_string(),
        });
    }
    if provider == FreshnessProvider::Brave {
        // Explicit ISO range: YYYY-MM-DDtoYYYY-MM-DD
        let original = value.trim();
        if let Some((start, end)) = original.split_once("to") {
            if is_valid_iso_date(start) && is_valid_iso_date(end) && start <= end {
                return Some(format!("{start}to{end}"));
            }
        }
    }
    None
}

/// Parsed time filters or a structured provider error payload.
#[derive(Debug, Default, PartialEq)]
pub struct ParsedTimeFilters {
    pub freshness: Option<String>,
    pub date_after: Option<String>,
    pub date_before: Option<String>,
}

/// Parses freshness/date filters, rejecting combinations providers cannot
/// express safely. Errors are the upstream `{error, message, docs}` payloads.
pub fn parse_web_search_time_filters(
    raw_freshness: Option<&str>,
    raw_date_after: Option<&str>,
    raw_date_before: Option<&str>,
    provider: FreshnessProvider,
    invalid_freshness_message: &str,
) -> Result<ParsedTimeFilters, serde_json::Value> {
    let freshness = raw_freshness
        .filter(|s| !s.trim().is_empty())
        .map(|s| normalize_freshness(s, provider));

    if let Some(None) = freshness {
        return Err(search_error_payload("invalid_freshness", invalid_freshness_message));
    }
    let freshness = freshness.flatten();

    if freshness.is_some() && (raw_date_after.is_some() || raw_date_before.is_some()) {
        return Err(search_error_payload(
            "conflicting_time_filters",
            "freshness and date_after/date_before cannot be used together. Use either freshness (day/week/month/year) or a date range (date_after/date_before), not both.",
        ));
    }

    let (date_after, date_before) = parse_iso_date_range(raw_date_after, raw_date_before)?;

    Ok(ParsedTimeFilters { freshness, date_after, date_before })
}

/// Parses optional date-range filters into canonical ISO dates.
pub fn parse_iso_date_range(
    raw_date_after: Option<&str>,
    raw_date_before: Option<&str>,
) -> Result<(Option<String>, Option<String>), serde_json::Value> {
    let date_after = match raw_date_after.filter(|s| !s.trim().is_empty()) {
        Some(raw) => match normalize_to_iso_date(raw) {
            Some(d) => Some(d),
            None => {
                return Err(search_error_payload(
                    "invalid_date",
                    "date_after must be YYYY-MM-DD format.",
                ))
            }
        },
        None => None,
    };
    let date_before = match raw_date_before.filter(|s| !s.trim().is_empty()) {
        Some(raw) => match normalize_to_iso_date(raw) {
            Some(d) => Some(d),
            None => {
                return Err(search_error_payload(
                    "invalid_date",
                    "date_before must be YYYY-MM-DD format.",
                ))
            }
        },
        None => None,
    };
    if let (Some(a), Some(b)) = (&date_after, &date_before) {
        if a > b {
            return Err(search_error_payload(
                "invalid_date_range",
                "date_after must be before date_before.",
            ));
        }
    }
    Ok((date_after, date_before))
}

/// Today's UTC date as `YYYY-MM-DD`.
pub fn today_iso_date() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_clamps_into_bounds() {
        assert_eq!(resolve_search_count(None, DEFAULT_SEARCH_COUNT), 5);
        assert_eq!(resolve_search_count(Some(0), 5), 1);
        assert_eq!(resolve_search_count(Some(50), 5), MAX_SEARCH_COUNT);
        assert_eq!(resolve_search_count(Some(3), 5), 3);
    }

    #[test]
    fn iso_date_validation() {
        assert!(is_valid_iso_date("2026-02-28"));
        assert!(!is_valid_iso_date("2026-02-30"));
        assert!(!is_valid_iso_date("2026-13-01"));
        assert!(!is_valid_iso_date("26-01-01"));
        assert!(!is_valid_iso_date("2026/01/01"));
    }

    #[test]
    fn normalize_to_iso_accepts_us_dates() {
        assert_eq!(normalize_to_iso_date("3/7/2026").as_deref(), Some("2026-03-07"));
        assert_eq!(normalize_to_iso_date("2026-03-07").as_deref(), Some("2026-03-07"));
        assert_eq!(normalize_to_iso_date("13/40/2026"), None);
    }

    #[test]
    fn freshness_normalization_brave() {
        assert_eq!(normalize_freshness("pd", FreshnessProvider::Brave).as_deref(), Some("pd"));
        assert_eq!(normalize_freshness("week", FreshnessProvider::Brave).as_deref(), Some("pw"));
        assert_eq!(
            normalize_freshness("2026-01-01to2026-02-01", FreshnessProvider::Brave).as_deref(),
            Some("2026-01-01to2026-02-01")
        );
        // Reversed range is invalid.
        assert_eq!(normalize_freshness("2026-02-01to2026-01-01", FreshnessProvider::Brave), None);
        assert_eq!(normalize_freshness("bogus", FreshnessProvider::Brave), None);
    }

    #[test]
    fn freshness_normalization_perplexity() {
        assert_eq!(
            normalize_freshness("pd", FreshnessProvider::Perplexity).as_deref(),
            Some("day")
        );
        assert_eq!(
            normalize_freshness("month", FreshnessProvider::Perplexity).as_deref(),
            Some("month")
        );
        // Explicit ranges are Brave-only.
        assert_eq!(
            normalize_freshness("2026-01-01to2026-02-01", FreshnessProvider::Perplexity),
            None
        );
    }

    #[test]
    fn time_filters_reject_conflicts() {
        let err = parse_web_search_time_filters(
            Some("pw"),
            Some("2026-01-01"),
            None,
            FreshnessProvider::Brave,
            "bad freshness",
        )
        .unwrap_err();
        assert_eq!(err["error"], "conflicting_time_filters");
    }

    #[test]
    fn time_filters_reject_reversed_range() {
        let err = parse_web_search_time_filters(
            None,
            Some("2026-02-01"),
            Some("2026-01-01"),
            FreshnessProvider::Brave,
            "bad freshness",
        )
        .unwrap_err();
        assert_eq!(err["error"], "invalid_date_range");
    }

    #[test]
    fn time_filters_pass_valid_range() {
        let parsed = parse_web_search_time_filters(
            None,
            Some("2026-01-01"),
            Some("2026-02-01"),
            FreshnessProvider::Brave,
            "bad freshness",
        )
        .unwrap();
        assert_eq!(parsed.date_after.as_deref(), Some("2026-01-01"));
        assert_eq!(parsed.date_before.as_deref(), Some("2026-02-01"));
        assert_eq!(parsed.freshness, None);
    }

    #[test]
    fn site_name_strips_www() {
        assert_eq!(
            resolve_site_name("https://www.rust-lang.org/learn").as_deref(),
            Some("rust-lang.org")
        );
        assert_eq!(resolve_site_name("not a url"), None);
    }
}
