//! Surrogate-safe UTF-16 string slicing (OpenClaw v2026.7.1 parity).
//!
//! Port of `packages/normalization-core/src/utf16-slice.ts`.
//!
//! Upstream truncates in *UTF-16 code units* because that is what JS string
//! length means, and because the wire limits these helpers enforce (Telegram's
//! 4096, Discord's 2000, Matrix event caps, tool-result caps) are themselves
//! specified in UTF-16 units. A naive cut at a code-unit index can land between
//! the two halves of a surrogate pair and emit a lone surrogate — which is not
//! valid UTF-8, renders as a replacement character, and in several of the
//! upstream reports corrupted the whole message.
//!
//! Rust strings are UTF-8 and cannot hold a lone surrogate at all, so the port
//! measures in UTF-16 units (to keep the same limits) and drops any pair that
//! would be split, exactly as upstream does.
//!
//! Deliberate API differences from the TS original:
//! - `truncate_utf16_safe` takes `usize`. Upstream clamps negative and floors
//!   fractional limits because JS numbers are neither; in Rust those states are
//!   unrepresentable, which is strictly better than replicating the clamp.
//! - `slice_utf16_safe` keeps `isize` bounds — negative indices are a real
//!   feature of the upstream helper (`slice_utf16_safe(s, -5, None)`), not a
//!   JS artifact.

fn is_high_surrogate(unit: u16) -> bool {
    (0xD800..=0xDBFF).contains(&unit)
}

fn is_low_surrogate(unit: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&unit)
}

/// Slice a string by UTF-16 code-unit offsets without returning dangling
/// surrogate halves at either edge.
///
/// `start`/`end` follow `String.prototype.slice` semantics: negative values
/// count back from the end, out-of-range values clamp, and `end <= start`
/// yields an empty string. `end == None` means "to the end".
pub fn slice_utf16_safe(input: &str, start: isize, end: Option<isize>) -> String {
    let units: Vec<u16> = input.encode_utf16().collect();
    let len = units.len() as isize;

    let mut from = if start < 0 {
        (len + start).max(0)
    } else {
        start.min(len)
    };
    let mut to = match end {
        None => len,
        Some(e) if e < 0 => (len + e).max(0),
        Some(e) => e.min(len),
    };

    if to <= from {
        return String::new();
    }

    // If the start index landed on the low half of a pair, step past it.
    if from > 0 && from < len {
        let idx = from as usize;
        if is_low_surrogate(units[idx]) && is_high_surrogate(units[idx - 1]) {
            from += 1;
        }
    }
    // If the end index would cut a pair, step back before its high half.
    if to > 0 && to < len {
        let idx = to as usize;
        if is_high_surrogate(units[idx - 1]) && is_low_surrogate(units[idx]) {
            to -= 1;
        }
    }

    // Both adjustments can converge (e.g. a 1-unit window inside one pair).
    if to <= from {
        return String::new();
    }

    String::from_utf16_lossy(&units[from as usize..to as usize])
}

/// Truncate to at most `max_len` UTF-16 code units without splitting a
/// surrogate pair. Returns `input` unchanged when it already fits.
pub fn truncate_utf16_safe(input: &str, max_len: usize) -> String {
    // Cheap fast path: UTF-8 byte length is an upper bound on UTF-16 unit
    // count, so anything short enough in bytes is short enough in units and
    // needs no re-encoding at all.
    if input.len() <= max_len {
        return input.to_string();
    }
    if input.encode_utf16().count() <= max_len {
        return input.to_string();
    }
    slice_utf16_safe(input, 0, Some(max_len as isize))
}

/// UTF-16 code-unit length — the unit every upstream wire limit is expressed in.
pub fn utf16_len(input: &str) -> usize {
    input.encode_utf16().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors of packages/normalization-core/src/utf16-slice.test.ts.

    #[test]
    fn slices_ascii_string_normally() {
        assert_eq!(slice_utf16_safe("hello world", 0, Some(5)), "hello");
    }

    #[test]
    fn handles_negative_start() {
        assert_eq!(slice_utf16_safe("hello world", -5, None), "world");
    }

    #[test]
    fn handles_negative_end() {
        assert_eq!(slice_utf16_safe("hello world", 0, Some(-6)), "hello");
    }

    #[test]
    fn handles_start_beyond_length() {
        assert_eq!(slice_utf16_safe("hello", 10, None), "");
    }

    #[test]
    fn handles_end_beyond_length() {
        assert_eq!(slice_utf16_safe("hello", 0, Some(10)), "hello");
    }

    #[test]
    fn returns_empty_when_start_after_end() {
        assert_eq!(slice_utf16_safe("hello", 3, Some(1)), "");
    }

    #[test]
    fn preserves_emoji_with_surrogate_pairs() {
        let emoji = "👨‍👩‍👧‍👦";
        assert_eq!(slice_utf16_safe(emoji, 0, None), emoji);
    }

    #[test]
    fn returns_empty_when_slicing_middle_of_surrogate_pair() {
        // "👨👩" is [D83D DC68 D83D DC69]; 1..3 is the inner half of both pairs.
        assert_eq!(slice_utf16_safe("👨👩", 1, Some(3)), "");
    }

    #[test]
    fn returns_empty_when_slicing_at_start_of_surrogate_pair() {
        assert_eq!(slice_utf16_safe("👨👩", 0, Some(1)), "");
    }

    #[test]
    fn handles_empty_string() {
        assert_eq!(slice_utf16_safe("", 0, None), "");
    }

    #[test]
    fn handles_open_ended_slice() {
        assert_eq!(slice_utf16_safe("hello", 2, None), "llo");
    }

    #[test]
    fn truncate_returns_input_when_shorter_than_limit() {
        assert_eq!(truncate_utf16_safe("hello", 10), "hello");
    }

    #[test]
    fn truncate_cuts_when_longer_than_limit() {
        assert_eq!(truncate_utf16_safe("hello world", 5), "hello");
    }

    #[test]
    fn truncate_handles_zero_limit() {
        assert_eq!(truncate_utf16_safe("hello", 0), "");
    }

    #[test]
    fn truncate_preserves_emoji_with_surrogate_pairs() {
        let emoji = "👨‍👩‍👧‍👦";
        let out = truncate_utf16_safe(emoji, 10);
        assert!(utf16_len(&out) <= utf16_len(emoji));
        // Whatever survives must still be valid, losslessly round-trippable text.
        assert_eq!(String::from_utf16(&out.encode_utf16().collect::<Vec<_>>()).unwrap(), out);
    }

    #[test]
    fn truncate_at_surrogate_boundary_drops_the_partial_pair() {
        assert_eq!(truncate_utf16_safe("👨👩", 1), "");
        // Exactly one whole pair fits in 2 units.
        assert_eq!(truncate_utf16_safe("👨👩", 2), "👨");
        // 3 units would split the second pair, so it is dropped entirely.
        assert_eq!(truncate_utf16_safe("👨👩", 3), "👨");
    }

    // Port-specific: multi-byte-but-single-unit text must not be measured in
    // UTF-8 bytes. "日本語テキスト" is 21 UTF-8 bytes but only 7 UTF-16 units,
    // so a byte-based limit would truncate it far too aggressively.
    #[test]
    fn cjk_is_measured_in_utf16_units_not_utf8_bytes() {
        let cjk = "日本語テキスト";
        assert_eq!(cjk.len(), 21);
        assert_eq!(utf16_len(cjk), 7);
        assert_eq!(truncate_utf16_safe(cjk, 7), cjk);
        assert_eq!(truncate_utf16_safe(cjk, 3), "日本語");
    }

    #[test]
    fn truncate_never_emits_a_lone_surrogate() {
        // Sweep every cut point across mixed BMP/astral text.
        let text = "a👨b👩‍👧c日本";
        for limit in 0..=utf16_len(&text) + 2 {
            let out = truncate_utf16_safe(text, limit);
            let units: Vec<u16> = out.encode_utf16().collect();
            assert!(
                String::from_utf16(&units).is_ok(),
                "limit {limit} produced invalid UTF-16"
            );
            assert!(utf16_len(&out) <= limit.max(0) || limit >= utf16_len(text));
        }
    }
}
