use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::{bail, Result};
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

// ============================================================================
// Nostr Channel Implementation
// ============================================================================

/// Nostr relay channel integration.
///
/// Connects to one or more Nostr relays via WebSocket and publishes/receives
/// NIP-01 text note events (kind 1) and NIP-04 encrypted DMs (kind 4).
///
/// This is a non-REST channel — it requires persistent WebSocket connections
/// to relays. `send_message` will return an error if not connected.
///
/// Protocol reference: <https://github.com/nostr-protocol/nips>
pub struct NostrChannel {
    /// Nostr private key (hex-encoded, 32 bytes / 64 hex chars).
    /// Used to sign events and derive the public key (npub).
    private_key: Option<String>,
    /// List of relay WebSocket URLs (e.g. `["wss://relay.damus.io"]`).
    relays: Option<Vec<String>>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// Whether relay connections are currently active.
    connected: Arc<AtomicBool>,
}

impl NostrChannel {
    pub fn new() -> Self {
        Self {
            private_key: None,
            relays: None,
            enabled: None,
            connected: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Create a configured Nostr channel.
    pub fn with_config(private_key: String, relays: Vec<String>) -> Self {
        Self {
            private_key: Some(private_key),
            relays: Some(relays),
            enabled: Some(true),
            connected: Arc::new(AtomicBool::new(false)),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }
}

#[async_trait]
impl ChannelPlugin for NostrChannel {
    fn id(&self) -> &str {
        "nostr"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Nostr".to_string(),
            description: "Nostr protocol channel via relay WebSocket connections".to_string(),
            enabled: self.is_enabled(),
            multi_account: false,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
            ChannelCapability::Groups,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        if self.private_key.is_none() {
            warn!("Nostr channel enabled but no private_key configured");
            return Ok(());
        }

        let relays = self.relays.as_deref().unwrap_or(&[]);
        if relays.is_empty() {
            warn!("Nostr channel enabled but no relays configured");
            return Ok(());
        }

        info!(
            relay_count = relays.len(),
            relays = ?relays,
            "Nostr channel starting — would connect to relays"
        );

        // TODO: For each relay:
        // 1. Open a WebSocket connection (tokio-tungstenite).
        // 2. Subscribe to events mentioning our pubkey (REQ filter).
        // 3. Handle incoming EVENT, EOSE, NOTICE messages.
        // 4. Implement NIP-04 decryption for encrypted DMs.
        //
        // Event signing uses secp256k1 Schnorr signatures (BIP-340).
        // The private key is used to derive the public key and sign events.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("Nostr channel stopping");
            self.connected.store(false, Ordering::Relaxed);
            // TODO: Send CLOSE to all subscriptions and disconnect from relays.
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        if !self.connected.load(Ordering::Relaxed) {
            anyhow::bail!("Nostr: not connected to any relay — cannot send message");
        }

        info!(to = %to, "Nostr: publishing event");

        // `to` interpretation depends on the event kind:
        // - For kind 1 (text note): `to` is ignored (broadcast to relays)
        // - For kind 4 (encrypted DM): `to` is the recipient's pubkey (hex)
        //
        // TODO: Construct a NIP-01 event, sign it with the private key,
        // and publish via ["EVENT", <event>] to all connected relays.
        let _ = message;

        Ok(())
    }
}

// ============================================================================
// npub / bech32 pubkey decoding + allowlist matching
//
// Port of OpenClaw `extensions/nostr/src/nostr-key-utils.ts` (v2026.7.1):
// `npub` allowlist entries are decoded to hex pubkeys for matching, and the
// allowlist matcher accepts either `npub1...` (NIP-19 bech32) or 64-char hex
// entries, case-insensitively. Upstream delegates the bech32 step to
// nostr-tools `nip19.decode`; here we carry a small self-contained bech32
// (BIP-173, original constant — NIP-19 uses bech32, not bech32m) decoder
// because no bech32 crate is available.
// ============================================================================

/// Bech32 character set (BIP-173).
const BECH32_CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";

/// BIP-173 polymod over 5-bit values.
fn bech32_polymod(values: &[u8]) -> u32 {
    const GEN: [u32; 5] = [0x3b6a_57b2, 0x2650_8e6d, 0x1ea1_19fa, 0x3d42_33dd, 0x2a14_62b3];
    let mut chk: u32 = 1;
    for &v in values {
        let b = chk >> 25;
        chk = ((chk & 0x01ff_ffff) << 5) ^ u32::from(v);
        for (i, g) in GEN.iter().enumerate() {
            if (b >> i) & 1 == 1 {
                chk ^= g;
            }
        }
    }
    chk
}

/// Expand the human-readable part for checksum computation (BIP-173).
fn bech32_hrp_expand(hrp: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(hrp.len() * 2 + 1);
    for c in hrp.bytes() {
        out.push(c >> 5);
    }
    out.push(0);
    for c in hrp.bytes() {
        out.push(c & 31);
    }
    out
}

/// Decode a bech32 string into `(hrp, 5-bit data values)` (checksum stripped).
///
/// Follows BIP-173: rejects mixed case, out-of-range characters, missing
/// separator, short checksums, and checksum mismatches. NIP-19 entities
/// (`npub`, `nsec`, `note`) use the original bech32 constant (1).
pub fn decode_bech32(input: &str) -> Result<(String, Vec<u8>)> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("bech32: empty string");
    }
    let has_lower = trimmed.chars().any(|c| c.is_ascii_lowercase());
    let has_upper = trimmed.chars().any(|c| c.is_ascii_uppercase());
    if has_lower && has_upper {
        bail!("bech32: mixed-case string");
    }
    if !trimmed.chars().all(|c| ('\x21'..='\x7e').contains(&c)) {
        bail!("bech32: character outside US-ASCII printable range");
    }
    let lowered = trimmed.to_ascii_lowercase();
    let sep = match lowered.rfind('1') {
        Some(pos) => pos,
        None => bail!("bech32: missing separator"),
    };
    if sep == 0 {
        bail!("bech32: empty human-readable part");
    }
    let data_part = &lowered[sep + 1..];
    if data_part.len() < 6 {
        bail!("bech32: data part too short for checksum");
    }
    let hrp = &lowered[..sep];
    let mut values = Vec::with_capacity(data_part.len());
    for c in data_part.bytes() {
        match BECH32_CHARSET.iter().position(|&b| b == c) {
            Some(v) => values.push(v as u8),
            None => bail!("bech32: invalid data character '{}'", c as char),
        }
    }
    let mut check = bech32_hrp_expand(hrp);
    check.extend_from_slice(&values);
    if bech32_polymod(&check) != 1 {
        bail!("bech32: checksum mismatch");
    }
    values.truncate(values.len() - 6);
    Ok((hrp.to_string(), values))
}

/// Convert 5-bit bech32 groups back to 8-bit bytes (no padding allowed on
/// decode: leftover bits must be < 5 and zero, per BIP-173).
fn bech32_convert_5_to_8(data: &[u8]) -> Result<Vec<u8>> {
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    let mut out = Vec::with_capacity(data.len() * 5 / 8);
    for &v in data {
        if v >= 32 {
            bail!("bech32: 5-bit value out of range");
        }
        acc = (acc << 5) | u32::from(v);
        bits += 5;
        while bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    if bits >= 5 || (acc & ((1 << bits) - 1)) != 0 {
        bail!("bech32: invalid padding");
    }
    Ok(out)
}

fn bytes_to_lower_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Decode an `npub1...` NIP-19 entity to its 32-byte hex pubkey (lowercase).
pub fn npub_to_hex(npub: &str) -> Result<String> {
    let (hrp, data) = decode_bech32(npub)?;
    if hrp != "npub" {
        bail!("Invalid npub key: wrong type ({})", hrp);
    }
    let bytes = bech32_convert_5_to_8(&data)?;
    if bytes.len() != 32 {
        bail!("Invalid npub key: expected 32 bytes, got {}", bytes.len());
    }
    Ok(bytes_to_lower_hex(&bytes))
}

fn is_hex_pubkey(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Normalize a pubkey to lowercase hex. Accepts `npub1...` (case-insensitive)
/// or 64-char hex (upstream `normalizePubkey`).
pub fn normalize_pubkey(input: &str) -> Result<String> {
    let trimmed = input.trim();
    let lowered = trimmed.to_ascii_lowercase();
    if lowered.starts_with("npub1") {
        return npub_to_hex(trimmed);
    }
    if !is_hex_pubkey(trimmed) {
        bail!("Pubkey must be 64 hex characters or npub format");
    }
    Ok(lowered)
}

/// Check whether a string looks like a valid Nostr pubkey (hex or npub)
/// (upstream `isValidPubkey`).
pub fn is_valid_pubkey(input: &str) -> bool {
    normalize_pubkey(input).is_ok()
}

/// Match a sender pubkey against a DM allowlist whose entries may be `npub`
/// or hex, in any case, optionally with a `nostr:` URI prefix. Entries that
/// fail to decode are ignored (they can never match) rather than failing the
/// whole allowlist.
pub fn allowlist_matches(allowlist: &[String], sender_pubkey: &str) -> bool {
    let sender = match normalize_pubkey(sender_pubkey) {
        Ok(hex) => hex,
        Err(_) => return false,
    };
    allowlist.iter().any(|entry| {
        let e = entry.trim();
        let e = e.strip_prefix("nostr:").unwrap_or(e);
        matches!(normalize_pubkey(e), Ok(hex) if hex == sender)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Known NIP-19 vectors (verified against the BIP-173 reference
    // implementation): jack's and fiatjaf's published pubkeys.
    const JACK_NPUB: &str = "npub1sg6plzptd64u62a878hep2kev88swjh3tw00gjsfl8f237lmu63q0uf63m";
    const JACK_HEX: &str = "82341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2";
    const FIATJAF_NPUB: &str = "npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6";
    const FIATJAF_HEX: &str = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";

    #[test]
    fn npub_decodes_to_known_hex() {
        assert_eq!(npub_to_hex(JACK_NPUB).unwrap(), JACK_HEX);
        assert_eq!(npub_to_hex(FIATJAF_NPUB).unwrap(), FIATJAF_HEX);
    }

    #[test]
    fn uppercase_npub_is_accepted() {
        assert_eq!(
            normalize_pubkey(&JACK_NPUB.to_ascii_uppercase()).unwrap(),
            JACK_HEX
        );
    }

    #[test]
    fn mixed_case_npub_is_rejected() {
        let mut mixed = JACK_NPUB.to_string();
        mixed.replace_range(0..1, "N");
        assert!(npub_to_hex(&mixed).is_err());
    }

    #[test]
    fn corrupted_checksum_is_rejected() {
        let mut bad = JACK_NPUB.to_string();
        // Flip the last character to a different charset member.
        let last = bad.pop().unwrap();
        bad.push(if last == 'q' { 'p' } else { 'q' });
        assert!(npub_to_hex(&bad).is_err());
    }

    #[test]
    fn wrong_hrp_is_rejected() {
        // nsec-style HRP must not be accepted as a pubkey.
        assert!(npub_to_hex("nsec1vl029mgpspedva04g90vltkh6fvh240zqtv9k0t9af8935ke9laqsnlfe5").is_err());
    }

    #[test]
    fn hex_pubkeys_normalize_case_insensitively() {
        assert_eq!(
            normalize_pubkey(&JACK_HEX.to_ascii_uppercase()).unwrap(),
            JACK_HEX
        );
        assert!(normalize_pubkey("deadbeef").is_err());
        assert!(normalize_pubkey(&"g".repeat(64)).is_err());
    }

    #[test]
    fn is_valid_pubkey_accepts_both_forms() {
        assert!(is_valid_pubkey(JACK_NPUB));
        assert!(is_valid_pubkey(JACK_HEX));
        assert!(!is_valid_pubkey("npub1notarealkey"));
        assert!(!is_valid_pubkey(""));
    }

    #[test]
    fn allowlist_matches_npub_and_hex_entries() {
        let allow = vec![JACK_NPUB.to_string()];
        assert!(allowlist_matches(&allow, JACK_HEX));
        assert!(allowlist_matches(&allow, &JACK_HEX.to_ascii_uppercase()));
        assert!(allowlist_matches(&allow, JACK_NPUB));
        assert!(!allowlist_matches(&allow, FIATJAF_HEX));

        let allow_hex = vec![FIATJAF_HEX.to_ascii_uppercase()];
        assert!(allowlist_matches(&allow_hex, FIATJAF_NPUB));
    }

    #[test]
    fn allowlist_accepts_nostr_uri_prefix_and_skips_bad_entries() {
        let allow = vec![
            "not-a-key".to_string(),
            format!("nostr:{}", JACK_NPUB),
        ];
        assert!(allowlist_matches(&allow, JACK_HEX));
        assert!(!allowlist_matches(&["not-a-key".to_string()], JACK_HEX));
    }
}
