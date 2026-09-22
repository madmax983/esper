//! Secret and PII masking (SPEC §20.1).
//!
//! Masking runs **before** prompt construction, **before** frame
//! durability, and **before** logging (design §9: redaction happens
//! before durability). What the durable record keeps instead is the
//! opaque marker plus digests: [`MaskReport::secret_digest`] is
//! FNV-1a-64 over the concatenated redacted *secret* bytes, so a
//! later crew can correlate "the same secret appeared again" without
//! ever storing the secret.
//!
//! The scanners are hand-rolled, deterministic, ASCII-only state
//! machines — there is no regex crate on firmware. Non-ASCII bytes
//! pass through untouched (UTF-8 multibyte sequences never match an
//! ASCII class). Matching is shape-based, not semantic: anything
//! shaped like a secret or like PII is redacted, and the eval crew
//! measures the false-positive rate (SPEC §20.7).

use crate::error::Error;
use crate::ids::Digest;

/// Which class of sensitive data a redacted span belonged to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MaskClass {
    /// API keys, tokens, and other credential shapes.
    Secret,
    /// Personally identifying shapes (emails, phone-like digit runs).
    Pii,
}

/// The outcome of one [`mask_report`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaskReport {
    /// How many spans were redacted (1-based marker numbering).
    pub redacted_spans: u32,
    /// FNV-1a-64 over the concatenated redacted *secret* bytes, in
    /// redaction order; `0` when no secret span was redacted. PII bytes
    /// are excluded: this digest exists to correlate secret reuse
    /// without storing secrets.
    pub secret_digest: u64,
    /// Bytes written to the output buffer.
    pub output_len: usize,
}

/// FNV-1a-64 over `bytes`.
///
/// The algorithm lives in [`Digest::of_bytes`](crate::ids::Digest::of_bytes);
/// this is the stable free-function name the E4 contract promises,
/// delegating so the algorithm is not duplicated.
#[must_use]
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    Digest::of_bytes(bytes).get()
}

/// Longest marker this module can emit: `"[redacted:secret#"` (17) +
/// up to 10 decimal digits + `"]"`.
const MAX_MARKER_LEN: usize = 28;
/// Shortest span any matcher accepts: `"a@b.co"` (left 1 + `@` + right 4).
const MIN_SPAN_LEN: usize = 6;

/// Worst-case output length for masking `input_len` input bytes.
///
/// Every input byte is either copied (1 byte) or belongs to a redacted
/// span of at least 6 bytes (`a@b.co`, the shortest accepted shape)
/// replaced by a marker of at most 28 bytes (`[redacted:secret#` + up
/// to 10 decimal digits + `]`), so the bound is
/// `n + (MAX_MARKER_LEN - MIN_SPAN_LEN) * (n / MIN_SPAN_LEN)`,
/// saturating. The marker-length term assumes span indices stay under
/// ten decimal digits (inputs below ~60 GiB); [`mask_bytes`] still
/// fails closed with [`Error::MaskOutputTooSmall`] if the bound ever
/// proves insufficient.
#[must_use]
pub const fn mask_bound(input_len: usize) -> usize {
    input_len
        .saturating_add((MAX_MARKER_LEN - MIN_SPAN_LEN).saturating_mul(input_len / MIN_SPAN_LEN))
}

/// Mask `input` into `output`, returning the bytes written.
///
/// Markers are `[redacted:secret#N]` / `[redacted:pii#N]` with `N`
/// 1-based per call across both classes. The caller must size
/// `output` to at least [`mask_bound`] of the input length.
///
/// Addresses that abut without separators (e.g. `a@b.coa@b.co`) are
/// scanned greedily: the left address's domain scan consumes the next
/// address's local part into its own redacted span, so no local part
/// is ever left visible — only inert domain residue remains. Realistic
/// inputs carry separators and redact as one span per address.
///
/// # Errors
///
/// Returns [`Error::MaskOutputTooSmall`] when `output` is smaller than
/// [`mask_bound`] of the input length.
pub fn mask_bytes(input: &[u8], output: &mut [u8]) -> Result<usize, Error> {
    mask_report(input, output).map(|report| report.output_len)
}

/// Mask `input` into `output`, returning the full [`MaskReport`].
///
/// See [`mask_bytes`] for the marker format and buffer contract.
///
/// # Errors
///
/// Returns [`Error::MaskOutputTooSmall`] when `output` is smaller than
/// [`mask_bound`] of the input length.
pub fn mask_report(input: &[u8], output: &mut [u8]) -> Result<MaskReport, Error> {
    if output.len() < mask_bound(input.len()) {
        return Err(Error::MaskOutputTooSmall);
    }
    let mut out: usize = 0;
    let mut spans: u32 = 0;
    let mut digest: u64 = 0xcbf2_9ce4_8422_2325;
    let mut secret_seen = false;
    let mut i: usize = 0;
    while i < input.len() {
        let rest = input.get(i..).ok_or(Error::MaskOutputTooSmall)?;
        if let Some(span) = match_secret(rest) {
            let secret = rest.get(..span).ok_or(Error::MaskOutputTooSmall)?;
            digest = fnv_mix(digest, secret);
            secret_seen = true;
            spans = spans.saturating_add(1);
            out = emit_marker(output, out, MaskClass::Secret, spans)?;
            i += span;
            continue;
        }
        if rest.first() == Some(&b'@')
            && let Some((left, total)) = match_email(input, i)
        {
            // Rewind only over bytes provably copied literally: the
            // output tail must equal the input's left part. (A secret
            // match can end right before the `@`, in which case the
            // tail is a marker and the email match is declined.)
            let tail = output.get(out.saturating_sub(left)..out);
            let head = input.get(i.saturating_sub(left)..i);
            if tail == head {
                out = out.saturating_sub(left);
                spans = spans.saturating_add(1);
                out = emit_marker(output, out, MaskClass::Pii, spans)?;
                i += total - left;
                continue;
            }
        }
        let first = rest.first().ok_or(Error::MaskOutputTooSmall)?;
        if (*first == b'+' || first.is_ascii_digit())
            && let Some(span) = match_phone(rest)
        {
            spans = spans.saturating_add(1);
            out = emit_marker(output, out, MaskClass::Pii, spans)?;
            i += span;
            continue;
        }
        out = put_byte(output, out, *first)?;
        i += 1;
    }
    Ok(MaskReport {
        redacted_spans: spans,
        secret_digest: if secret_seen { digest } else { 0 },
        output_len: out,
    })
}

/// One FNV-1a-64 step block over already-hashed state.
fn fnv_mix(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Write one byte, failing closed when the cursor left the buffer.
fn put_byte(output: &mut [u8], out: usize, byte: u8) -> Result<usize, Error> {
    output
        .get_mut(out)
        .map_or(Err(Error::MaskOutputTooSmall), |slot| {
            *slot = byte;
            Ok(out.saturating_add(1))
        })
}

/// Write `n` in decimal (no `format!` on firmware).
fn put_u32(mut out: usize, output: &mut [u8], mut n: u32) -> Result<usize, Error> {
    if n == 0 {
        return put_byte(output, out, b'0');
    }
    let mut digits = [0u8; 10];
    let mut len: usize = 0;
    while n > 0 {
        let digit = u8::try_from(n % 10).map_err(|_| Error::MaskOutputTooSmall)?;
        if let Some(slot) = digits.get_mut(len) {
            *slot = b'0'.saturating_add(digit);
        }
        len += 1;
        n /= 10;
    }
    while len > 0 {
        len -= 1;
        if let Some(byte) = digits.get(len) {
            out = put_byte(output, out, *byte)?;
        }
    }
    Ok(out)
}

/// Emit `[redacted:secret#N]` or `[redacted:pii#N]`.
fn emit_marker(
    output: &mut [u8],
    mut out: usize,
    class: MaskClass,
    n: u32,
) -> Result<usize, Error> {
    let tag: &[u8] = match class {
        MaskClass::Secret => b"[redacted:secret#",
        MaskClass::Pii => b"[redacted:pii#",
    };
    for byte in tag {
        out = put_byte(output, out, *byte)?;
    }
    out = put_u32(out, output, n)?;
    put_byte(output, out, b']')
}

// ---------------------------------------------------------------------------
// Secret matchers: each returns the span length on a match.
// ---------------------------------------------------------------------------

/// Try every secret shape at the start of `rest`.
fn match_secret(rest: &[u8]) -> Option<usize> {
    if rest.starts_with(b"-----BEGIN ") {
        return match_pem(rest);
    }
    if rest.starts_with(b"sk_live_") {
        return match_token(rest, 8);
    }
    if rest.starts_with(b"sk_test_") {
        return match_token(rest, 8);
    }
    if rest.starts_with(b"github_pat_") {
        return match_token(rest, 11);
    }
    if rest.starts_with(b"sk-") {
        return match_token(rest, 3);
    }
    if rest.starts_with(b"AKIA") {
        return match_akia(rest);
    }
    if rest.starts_with(b"Bearer ") || rest.starts_with(b"bearer ") {
        return match_token(rest, 7);
    }
    None
}

/// A prefixed token: `anchor` bytes plus at least 16 token characters
/// (`[A-Za-z0-9._~+/\-=]`, up to 128). Short anchors without a
/// token-looking tail (prose mentions like "sk-") do not match.
fn match_token(rest: &[u8], anchor: usize) -> Option<usize> {
    let mut len = anchor;
    while len < rest.len() && len - anchor < 128 {
        let byte = rest.get(len)?;
        if !is_token_char(*byte) {
            break;
        }
        len += 1;
    }
    if len - anchor >= 16 { Some(len) } else { None }
}

/// `AKIA` plus exactly 16 alphanumerics (AWS access-key-ID shape).
fn match_akia(rest: &[u8]) -> Option<usize> {
    let tail = rest.get(4..20)?;
    if tail.iter().all(u8::is_ascii_alphanumeric) {
        Some(20)
    } else {
        None
    }
}

/// A PEM block: from `-----BEGIN ` through the end of the
/// `-----END ...` line. The closing marker must appear within a
/// bounded window, otherwise the opening line is ordinary text.
fn match_pem(rest: &[u8]) -> Option<usize> {
    const MAX_PEM_BYTES: usize = 2048;
    let window = rest.len().min(MAX_PEM_BYTES);
    let mut i = "-----BEGIN ".len();
    while i + "-----END ".len() <= window {
        if rest.get(i..)?.starts_with(b"-----END ") {
            let mut end = i + "-----END ".len();
            while end < rest.len() && rest.get(end) != Some(&b'\n') {
                end += 1;
            }
            if end < rest.len() {
                end += 1; // keep the newline
            }
            return Some(end);
        }
        i += 1;
    }
    None
}

const fn is_token_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'~' | b'+' | b'/' | b'-' | b'=')
}

// ---------------------------------------------------------------------------
// PII matchers
// ---------------------------------------------------------------------------

/// An email around the `@` at `at`: returns `(left_len, total_len)`.
/// The caller rewinds the output over `left_len` literal bytes.
fn match_email(input: &[u8], at: usize) -> Option<(usize, usize)> {
    let mut left_start = at;
    while left_start > 0 && input.get(left_start - 1).is_some_and(|b| is_local_char(*b)) {
        left_start -= 1;
    }
    let left = at - left_start;
    if left == 0 {
        return None;
    }
    let mut right_end = at + 1;
    while right_end < input.len() && input.get(right_end).is_some_and(|b| is_domain_char(*b)) {
        right_end += 1;
    }
    // Trailing dots are sentence punctuation, not the domain.
    while right_end > at + 1 && input.get(right_end - 1) == Some(&b'.') {
        right_end -= 1;
    }
    let right = input.get(at + 1..right_end)?;
    if right.len() < 4 || !right.contains(&b'.') {
        return None;
    }
    Some((left, right_end - left_start))
}

/// A phone-like digit run at the start of `rest`: an optional `+`
/// followed by digits with ` .-()` separators, 7–15 digits total.
/// Bare digit runs longer than 11 digits are left alone (timestamps);
/// anything separator-shaped in the 7–15 range is redacted.
fn match_phone(rest: &[u8]) -> Option<usize> {
    let mut i: usize = 0;
    if rest.first() == Some(&b'+') {
        i = 1;
        if rest.get(i).is_none_or(|byte| !byte.is_ascii_digit()) {
            return None;
        }
    }
    let mut digits: u32 = 0;
    while i < rest.len() {
        let byte = rest.get(i)?;
        if byte.is_ascii_digit() {
            digits = digits.saturating_add(1);
        } else if !is_phone_sep(*byte) {
            break;
        }
        i += 1;
    }
    while i > 0 && rest.get(i - 1).is_some_and(|byte| is_phone_sep(*byte)) {
        i -= 1;
    }
    if !(7..=15).contains(&digits) {
        return None;
    }
    // A span with no separators must be short (timestamps are not phone
    // numbers); anything separator-shaped in range is redacted.
    let plus = u64::from(rest.starts_with(b"+"));
    let has_sep = (i as u64) > u64::from(digits) + plus;
    if !has_sep && digits > 11 {
        return None;
    }
    Some(i)
}

const fn is_local_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'%' | b'+' | b'-')
}

const fn is_domain_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')
}

const fn is_phone_sep(byte: u8) -> bool {
    matches!(byte, b' ' | b'-' | b'.' | b'(' | b')')
}
