//! Model backends: the E3 inference boundary.
//!
//! The engine no longer pulls canned lines from a script object; it
//! builds a bounded prompt ([`PromptCtx`] rendered by
//! [`build_prompt`]) and hands it to a [`ModelBackend`]. The prompt is
//! prose for a tiny model: a `TOOLS` block with the six compact
//! `esper_protocol::render_signature` tool signatures, a `STATE` line,
//! an optional `REPAIR` line (attempt, variant, and the §4.3 grammar
//! one-liner), an optional `LAST` observation line, and an `EMIT`
//! trailer. Only the `LAST` observation tail may truncate; the six
//! signature lines never do. Two backends ship:
//!
//! - [`ScriptedBackend`]: the host test double. It emits canned lines
//!   in script order and ignores the prompt contents, so every E0/E1
//!   trajectory keeps its meaning unchanged.
//! - [`TinyBackend`]: the deterministic distilled model. It answers
//!   each prompt from a caller-owned [`DistillEntry`] table keyed by
//!   [`fingerprint_prompt`]; an unknown prompt is
//!   [`BackendError::UnknownPrompt`], never a guess.
//!
//! Every inference is measured ([`TokenUsage`]) and every backend
//! carries a [`BundleId`] — the FNV-1a 64-bit identity of its model
//! bundle — so a run's journal binds it to the exact model that
//! produced it (see `RunSeed::model_bundle`).
//!
//! The prompt and output buffers are fixed ([`PROMPT_CAP`],
//! [`OUTPUT_CAP`]); the per-run policy is [`InferenceSettings`].
//! All items here are allocation-free except [`ScriptedBackend`],
//! which owns its script and is therefore host-only.

use thiserror::Error;

/// Maximum bytes of one inference prompt (E3).
pub const PROMPT_CAP: usize = 1024;

/// Maximum bytes of one model output line (E3).
pub const OUTPUT_CAP: usize = 256;

/// The tiny backend's parameter budget, in bytes (E3 release gate).
///
/// The distilled prompt→line table is the tiny model's parameters;
/// the reference table distills to 2156 bytes. The resource-gate
/// suite pins this value so parameter growth is always deliberate,
/// and separately asserts it stays under the 4096-byte target-board
/// allowance.
pub const TINY_PARAMS_BYTES: usize = 2156;

/// `PROMPT_CAP` as `u16` for the [`InferenceSettings`] fields: 1024
/// fits in `u16`. The assertion ties the literal to the cap, so the
/// two can never drift apart — a narrowing `as` cast here would trip
/// `cast_possible_truncation`.
const PROMPT_CAP_U16: u16 = {
    assert!(PROMPT_CAP == 1024);
    1024
};

/// `OUTPUT_CAP` as `u16` for the [`InferenceSettings`] fields: 256
/// fits in `u16`. The assertion ties the literal to the cap, so the
/// two can never drift apart.
const OUTPUT_CAP_U16: u16 = {
    assert!(OUTPUT_CAP == 256);
    256
};

/// The inference buffer policy: how much prompt the engine may hand
/// the model, and how much output the engine will read back.
///
/// Both bounds are validated against the caps; a zero or over-cap
/// bound fails closed at seed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InferenceSettings {
    /// Maximum prompt bytes per inference, `1..=1024`.
    pub max_prompt_bytes: u16,
    /// Maximum output bytes per inference, `1..=256`.
    pub max_output_bytes: u16,
}

impl InferenceSettings {
    /// The E3 defaults: the full prompt and output caps.
    #[must_use]
    pub const fn default_settings() -> Self {
        Self {
            max_prompt_bytes: PROMPT_CAP_U16,
            max_output_bytes: OUTPUT_CAP_U16,
        }
    }

    /// Validate the bounds: nonzero and within the caps.
    ///
    /// # Errors
    ///
    /// Returns a static reason when a bound is zero or exceeds its
    /// cap.
    pub const fn validate(&self) -> Result<(), &'static str> {
        if self.max_prompt_bytes == 0 || self.max_prompt_bytes > PROMPT_CAP_U16 {
            return Err("max_prompt_bytes must be within 1..=1024");
        }
        if self.max_output_bytes == 0 || self.max_output_bytes > OUTPUT_CAP_U16 {
            return Err("max_output_bytes must be within 1..=256");
        }
        Ok(())
    }

    /// The canonical 4-byte encoding, little-endian: `max_prompt_bytes`
    /// then `max_output_bytes`. Bound into the seed snapshot so a
    /// journal can never replay under a wider policy.
    #[must_use]
    pub const fn bytes(&self) -> [u8; 4] {
        let prompt = self.max_prompt_bytes.to_le_bytes();
        let output = self.max_output_bytes.to_le_bytes();
        [prompt[0], prompt[1], output[0], output[1]]
    }
}

/// Per-inference token accounting, measured not metered (SPEC §18).
///
/// Backends report what they saw; the engine records the numbers in
/// the journal but never spends budget against them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    /// Prompt bytes the backend read.
    pub input_tokens: u32,
    /// Output bytes the backend wrote.
    pub output_tokens: u32,
}

/// A model bundle's identity: the FNV-1a 64-bit digest of the bundle
/// bytes (script lines plus the [`InferenceSettings`] bytes for
/// [`ScriptedBackend`], the distill table for [`TinyBackend`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BundleId(pub u64);

/// FNV-1a over 64 bits: the bundle and fingerprint hash.
#[must_use]
pub const fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    hash
}

/// Fold one more byte into a running FNV-1a 64-bit digest.
const fn mix_byte(mut digest: u64, byte: u8) -> u64 {
    digest ^= byte as u64;
    digest.wrapping_mul(0x0000_0100_0000_01b3)
}

/// What can go wrong at the inference boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum BackendError {
    /// The backend has no more output to give. For the scripted
    /// backend this is the exhausted script: a harness bug, not a run
    /// outcome.
    #[error("the model backend is exhausted")]
    Exhausted,
    /// The backend's line does not fit the output buffer. The line is
    /// dropped, not truncated: silent truncation would hand the
    /// decoder a prefix it never emitted.
    #[error("the model output is longer than the output buffer")]
    OutputTooLong,
    /// The tiny backend has no distill entry for this prompt. The
    /// model never guesses; an unrecorded prompt is a hard failure.
    #[error("the tiny backend has no line for this prompt")]
    UnknownPrompt,
    /// A distill entry failed its integrity check: the stored line
    /// hash does not match the line bytes. The table was tampered
    /// with or corrupted.
    #[error("a distill entry failed its integrity check")]
    PolicyMismatch,
}

/// The model backend: the engine's only window into the model.
///
/// Object-safe so the engine holds `&mut dyn ModelBackend` and the
/// harness can wrap backends (recording, fault injection) without
/// touching the driver.
///
/// `Send` is a supertrait so the Tokio async driver (`drive_run_async`)
/// can move the backend across worker threads: a `&mut dyn ModelBackend`
/// must be `Send` for the driver's future to be `Send` (clippy
/// `future_not_send`). Every shipped backend is `Send`
/// (`ScriptedBackend` owns `Vec`s of bytes; `TinyBackend` only borrows
/// `&[u8]` and `u64`s, which are `Sync`).
pub trait ModelBackend: Send {
    /// Run one inference: read `prompt`, write the output line into
    /// `out`, return the bytes written.
    ///
    /// # Errors
    ///
    /// Returns [`BackendError`] when the backend cannot produce a
    /// line for this prompt.
    fn infer(&mut self, prompt: &[u8], out: &mut [u8]) -> Result<usize, BackendError>;

    /// The identity of the model bundle behind this backend.
    fn bundle_id(&self) -> BundleId;

    /// The measured usage of the last inference.
    fn last_usage(&self) -> TokenUsage;

    /// Rewind one emission: the last line was taken but never
    /// committed (crash before the decision commit), so the next boot
    /// must see it again. Backends without an emission cursor (the
    /// tiny backend's lookup is pure) implement this as a no-op.
    fn unemit(&mut self);
}

/// The scripted model backend: emits canned lines, one per turn.
///
/// Repair lines are just later script lines; the script already
/// contains what the model would emit after a repair hint. The prompt
/// is measured for token accounting but otherwise ignored, so every
/// E0/E1 trajectory keeps its meaning unchanged under the E3 engine.
// HOST-ONLY (E3)
#[cfg(feature = "host")]
#[derive(Debug, Clone)]
pub struct ScriptedBackend {
    /// The canned lines, in emission order.
    // HOST-ONLY (E3)
    lines: Vec<Vec<u8>>,
    /// How many lines have been emitted.
    index: usize,
    /// The buffer policy this backend was built with.
    settings: InferenceSettings,
    /// The measured usage of the last inference.
    usage: TokenUsage,
    /// The bundle identity, fixed at construction.
    bundle: BundleId,
}

// HOST-ONLY (E3)
#[cfg(feature = "host")]
impl ScriptedBackend {
    /// Build a backend from canned lines, in emission order, under
    /// this buffer policy. The bundle digest binds the lines and the
    /// policy together: the same script under a different
    /// [`InferenceSettings`] is a different bundle.
    #[must_use]
    pub fn new(lines: Vec<Vec<u8>>, settings: InferenceSettings) -> Self {
        let bundle = BundleId(script_bundle(&lines, settings));
        Self {
            // HOST-ONLY (E3)
            lines,
            index: 0,
            settings,
            usage: TokenUsage::default(),
            bundle,
        }
    }

    /// How many lines have been emitted so far.
    #[must_use]
    pub const fn lines_consumed(&self) -> usize {
        self.index
    }

    /// The buffer policy this backend was built with.
    #[must_use]
    pub const fn settings(&self) -> InferenceSettings {
        self.settings
    }
}

/// The bundle digest of a script: FNV-1a 64 over the domain tag, the
/// [`InferenceSettings`] bytes (little-endian, so the digest is stable
/// across targets), then each line's length (little-endian `u64`,
/// never platform-width `usize`, so the digest is stable across
/// targets) and bytes. Length-prefixing keeps `["ab", "c"]` and `["a",
/// "bc"]` distinct, and the settings bytes keep the same script under
/// a different buffer policy distinct.
// HOST-ONLY (E3)
#[cfg(feature = "host")]
fn script_bundle(lines: &[Vec<u8>], settings: InferenceSettings) -> u64 {
    // HOST-ONLY (E3)
    let mut digest = fnv1a64(b"esper-scripted-v1");
    for byte in settings.bytes() {
        digest = mix_byte(digest, byte);
    }
    for line in lines {
        // `u64`, never `usize`: the digest must not depend on the
        // target's pointer width.
        let len = u64::try_from(line.len()).unwrap_or(u64::MAX);
        for byte in len.to_le_bytes() {
            digest = mix_byte(digest, byte);
        }
        for byte in line {
            digest = mix_byte(digest, *byte);
        }
    }
    digest
}

// HOST-ONLY (E3)
#[cfg(feature = "host")]
impl ModelBackend for ScriptedBackend {
    fn infer(&mut self, prompt: &[u8], out: &mut [u8]) -> Result<usize, BackendError> {
        // HOST-ONLY (E3)
        let line = self.lines.get(self.index).ok_or(BackendError::Exhausted)?;
        if line.len() > out.len() {
            return Err(BackendError::OutputTooLong);
        }
        out[..line.len()].copy_from_slice(line);
        self.index += 1;
        self.usage = TokenUsage {
            input_tokens: u32::try_from(prompt.len()).unwrap_or(u32::MAX),
            output_tokens: u32::try_from(line.len()).unwrap_or(u32::MAX),
        };
        Ok(line.len())
    }

    fn bundle_id(&self) -> BundleId {
        self.bundle
    }

    fn last_usage(&self) -> TokenUsage {
        self.usage
    }

    fn unemit(&mut self) {
        self.index = self.index.saturating_sub(1);
    }
}

/// One distilled prompt→line mapping: the tiny model's parameters.
///
/// `line_hash` is `fnv1a64(line)`; the backend checks it on every
/// lookup so a tampered or corrupted table fails closed as
/// [`BackendError::PolicyMismatch`] instead of feeding the decoder
/// bytes nobody distilled.
#[derive(Debug, Clone, Copy)]
pub struct DistillEntry<'a> {
    /// `fingerprint_prompt` of the prompt this entry answers.
    pub prompt_fp: u64,
    /// The integrity hash of `line`.
    pub line_hash: u64,
    /// The distilled output line.
    pub line: &'a [u8],
}

/// The prompt fingerprint: `fnv1a64` over the exact prompt bytes.
///
/// The distill table keys on this, so record and replay must build
/// byte-identical prompts (the engine restores the prompt inputs from
/// the journal for exactly this reason).
#[must_use]
pub const fn fingerprint_prompt(prompt: &[u8]) -> u64 {
    fnv1a64(prompt)
}

/// The tiny model backend: a deterministic prompt→line lookup over a
/// caller-owned distill table.
///
/// No allocation, no guessing: a prompt with no entry is
/// [`BackendError::UnknownPrompt`], and an entry whose line fails its
/// integrity check is [`BackendError::PolicyMismatch`]. `unemit` is a
/// no-op by design — the lookup is pure, so a crash between inference
/// and commit just re-issues the same prompt on the next boot and gets
/// the same line; there is no cursor to rewind.
#[derive(Debug, Clone, Copy)]
pub struct TinyBackend<'a> {
    /// The distilled prompt→line table, borrowed.
    table: &'a [DistillEntry<'a>],
    /// The measured usage of the last inference.
    usage: TokenUsage,
    /// The bundle identity, fixed at construction.
    bundle: BundleId,
}

impl<'a> TinyBackend<'a> {
    /// Build a backend over this distill table. The table is borrowed:
    /// it must outlive the run.
    #[must_use]
    pub fn new(table: &'a [DistillEntry<'a>]) -> Self {
        Self {
            table,
            usage: TokenUsage::default(),
            bundle: BundleId(tiny_bundle(table)),
        }
    }

    /// How many distill entries this backend holds.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.table.len()
    }

    /// Whether the distill table is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.table.is_empty()
    }
}

/// The bundle digest of a distill table: FNV-1a 64 over the domain tag,
/// then each entry's prompt fingerprint and line hash. Two tables that
/// answer the same prompts with the same lines share a bundle id.
fn tiny_bundle(table: &[DistillEntry<'_>]) -> u64 {
    let mut digest = fnv1a64(b"esper-tiny-v1");
    for entry in table {
        for byte in entry.prompt_fp.to_le_bytes() {
            digest = mix_byte(digest, byte);
        }
        for byte in entry.line_hash.to_le_bytes() {
            digest = mix_byte(digest, byte);
        }
    }
    digest
}

impl ModelBackend for TinyBackend<'_> {
    fn infer(&mut self, prompt: &[u8], out: &mut [u8]) -> Result<usize, BackendError> {
        let fingerprint = fingerprint_prompt(prompt);
        let entry = self
            .table
            .iter()
            .find(|entry| entry.prompt_fp == fingerprint)
            .ok_or(BackendError::UnknownPrompt)?;
        if fnv1a64(entry.line) != entry.line_hash {
            return Err(BackendError::PolicyMismatch);
        }
        if entry.line.len() > out.len() {
            return Err(BackendError::OutputTooLong);
        }
        out[..entry.line.len()].copy_from_slice(entry.line);
        self.usage = TokenUsage {
            input_tokens: u32::try_from(prompt.len()).unwrap_or(u32::MAX),
            output_tokens: u32::try_from(entry.line.len()).unwrap_or(u32::MAX),
        };
        Ok(entry.line.len())
    }

    fn bundle_id(&self) -> BundleId {
        self.bundle
    }

    fn last_usage(&self) -> TokenUsage {
        self.usage
    }

    fn unemit(&mut self) {
        // No-op by design: the lookup is pure, so re-issuing the same
        // prompt after a crash returns the same line. Documented on
        // the struct; the resource-gate suite pins the behavior.
    }
}

/// The structured repair hint the prompt carries (E3).
///
/// This is the engine's hint — which repair turn just burned and why —
/// not the decoder's field-level hint (`esper_core::error::RepairHint`
/// names the broken field; this one names the repair attempt).
#[derive(Debug, Clone, Copy)]
pub struct RepairHint {
    /// The one-based repair index already committed.
    pub attempt: u8,
    /// The variant of the last invalid line: `"malformed"` or
    /// `"invalid_args"`.
    pub variant: &'static str,
}

/// The prompt inputs for one inference (E3).
///
/// Everything the prompt renders comes from durable or rebuilt state,
/// so a post-reboot inference builds the byte-identical prompt: the
/// tiny backend's prompt→line lookup depends on it.
#[derive(Debug)]
pub struct PromptCtx<'a> {
    /// Model turns still unspent.
    pub turns_left: u16,
    /// Mutations still unspent.
    pub mutations_left: u16,
    /// The last committed observation's outcome bytes (capped at 128
    /// by the engine), if any tool has reported yet.
    pub last_observation: Option<&'a [u8]>,
    /// The repair hint, when an invalid line already burned a turn.
    pub repair: Option<RepairHint>,
    /// The folded-history marker, when this run continues a
    /// compacted parent (E4, SPEC §20.5). `None` for root runs.
    pub prior: Option<PriorCtx>,
}

/// The folded-history marker for a continued run's prompt (E4,
/// SPEC §20.5).
///
/// When a run continues after a rollover, its prompt carries one
/// fixed line naming the compact epoch the parent's history was
/// folded into, plus the compact-state counts the child inherits.
/// The parent's raw bytes are gone by construction: any reference
/// to a compacted frame resolves to the stale marker, never to the
/// original payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriorCtx {
    /// The compact epoch (`continued_at_frame`) the parent folded at.
    pub epoch: u32,
    /// Ruled-out paths inherited from the parent fold.
    pub failed_paths: u8,
    /// Open obligations inherited from the parent fold.
    pub pending: u8,
    /// Retained facts inherited from the parent fold.
    pub facts: u8,
}

/// Render the stale-reference marker `[stale:folded@epoch=N]`
/// into `out`, returning the bytes written (E4, SPEC §20.5).
///
/// The marker is what any reference to a compacted frame's payload
/// resolves to: it names the compact epoch the frame was folded
/// into and carries no original bytes, so content is never
/// invented. The caller must provide at least 31 bytes.
pub(crate) fn write_stale_marker(epoch: u32, out: &mut [u8]) -> usize {
    use core::fmt::Write as _;
    let mut writer = PromptWriter::new(out);
    let _ = write!(writer, "[stale:folded@epoch={epoch}]");
    writer.pos()
}

/// The grammar one-liner the REPAIR line carries: the decision grammar
/// from `esper_core::decode_line` (SPEC §4.3), abbreviated to fit the
/// prompt budget — `CALL <tool> <json> | ASK <json> | FINISH <json>`.
/// Fixed text — never truncated.
const GRAMMAR_ONE_LINER: &str = "CALL <tool> <json> | ASK <json> | FINISH <json>";

/// The prompt trailer: the last line of every prompt, telling the
/// model what to emit. Fixed text — never truncated.
const EMIT_TRAILER: &str = "EMIT one decision line.\n";

/// The marker ending a truncated LAST observation line. Fixed text.
const TRUNCATED_MARKER: &str = "[truncated]\n";

/// Render the canonical E3 prompt into `out`, returning the bytes
/// written.
///
/// The prompt is prose for a tiny model, data for the fingerprint:
///
/// ```text
/// TOOLS
/// <render_signature of each of the 6 catalog tools>
/// STATE turns=<n> muts=<n>
/// PRIOR [stale:folded@epoch=<e>] failed=<n> pending=<n> facts=<n>
/// REPAIR <attempt>:<variant> <grammar one-liner>
/// LAST <observation bytes>
/// EMIT one decision line.
/// ```
///
/// The TOOLS block names every tool the model may call, rendered by
/// `esper_protocol::render_signature` from the single contract source
/// (SPEC §17.3) — the model sees the signatures, never a bare tool
/// list. The PRIOR line appears only in a continued run (after a
/// rollover): it carries the stale marker of §20.5 — the compact
/// epoch the parent's history was folded into — plus the inherited
/// failed-path, pending-obligation, and fact counts. The parent's
/// raw bytes never appear in a continued prompt: any reference to a
/// compacted frame resolves to the marker, never to the original
/// payload. The REPAIR line appears only after an invalid line burned a
/// turn, and carries the attempt, the §4.4 variant, and the grammar
/// one-liner again. The LAST line appears only after a tool reported,
/// and carries the observation bytes raw (already bounded machine
/// JSON, capped at 128 bytes by the engine).
///
/// Truncation rule: the TOOLS block, the STATE line, the PRIOR line,
/// the REPAIR line, and the EMIT trailer are fixed — they are never
/// truncated. Only the LAST observation tail may truncate, and then
/// it ends with `[truncated]`. The framing around the signatures is
/// abbreviated (`turns=`/`muts=`) so the fixed prompt — signatures plus
/// framing — always fits `PROMPT_CAP`; a unit test pins this budget, so
/// signature growth fails loudly instead of silently squeezing the
/// observation. (A narrowed `max_prompt_bytes` truncates
/// deterministically head-first, the same saturation discipline as
/// before.)
#[must_use]
pub fn build_prompt(ctx: &PromptCtx, out: &mut [u8]) -> usize {
    use core::fmt::Write as _;
    let mut writer = PromptWriter::new(out);
    // The fixed head: TOOLS block, STATE line, REPAIR line. Never
    // truncated by design (see the truncation rule above).
    let _ = writer.write_str("TOOLS\n");
    for contract in esper_protocol::CATALOG {
        let _ = esper_protocol::render_signature(&contract, &mut writer);
        let _ = writer.write_char('\n');
    }
    let _ = writeln!(
        writer,
        "STATE turns={} muts={}",
        ctx.turns_left, ctx.mutations_left
    );
    if let Some(prior) = ctx.prior {
        // The folded-history marker: fixed line, never truncated.
        let _ = writer.write_str("PRIOR ");
        let mut marker = [0u8; 32];
        let marker_len = write_stale_marker(prior.epoch, &mut marker);
        let _ = writer.write_bytes(marker.get(..marker_len).unwrap_or(&[]));
        let _ = writeln!(
            writer,
            " failed={} pending={} facts={}",
            prior.failed_paths, prior.pending, prior.facts
        );
    }
    if let Some(hint) = ctx.repair {
        let _ = writeln!(
            writer,
            "REPAIR {}:{} {GRAMMAR_ONE_LINER}",
            hint.attempt, hint.variant
        );
    }
    // The LAST line is the only truncatable part: reserve the EMIT
    // trailer, then fit what fits of the observation.
    if let Some(observation) = ctx.last_observation {
        let _ = writer.write_str("LAST ");
        let trailer_len = EMIT_TRAILER.len();
        let room = writer.room();
        // Room for the observation, its newline, and the trailer.
        let full_need = observation.len() + 1 + trailer_len;
        if full_need <= room {
            let _ = writer.write_bytes(observation);
            let _ = writer.write_char('\n');
        } else {
            let marker_len = TRUNCATED_MARKER.len();
            let keep = room.saturating_sub(trailer_len + marker_len);
            let _ = writer.write_bytes(observation.get(..keep).unwrap_or(&[]));
            let _ = writer.write_str(TRUNCATED_MARKER);
        }
    }
    let _ = writer.write_str(EMIT_TRAILER);
    writer.pos()
}

/// A [`core::fmt::Write`] over a byte slice: the prompt renderer.
///
/// `render_signature` writes through `core::fmt::Write`; this adapter
/// lets it render straight into the caller's prompt buffer with no
/// allocation (the render.rs `Sink` test pattern, productionized).
/// Writes that would overflow fail with [`core::fmt::Error`] instead
/// of truncating silently; `room` reports the bytes still free.
struct PromptWriter<'a> {
    /// The caller's buffer.
    buf: &'a mut [u8],
    /// Bytes written so far.
    pos: usize,
}

impl<'a> PromptWriter<'a> {
    /// Wrap the caller's buffer; nothing is written yet.
    const fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes written so far.
    const fn pos(&self) -> usize {
        self.pos
    }

    /// Bytes still free in the buffer.
    const fn room(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    /// Copy raw bytes (the observation is machine JSON, not text).
    fn write_bytes(&mut self, bytes: &[u8]) -> core::fmt::Result {
        let end = self.pos.saturating_add(bytes.len());
        if end > self.buf.len() {
            return Err(core::fmt::Error);
        }
        self.buf[self.pos..end].copy_from_slice(bytes);
        self.pos = end;
        Ok(())
    }
}

impl core::fmt::Write for PromptWriter<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        self.write_bytes(text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BackendError, DistillEntry, InferenceSettings, ModelBackend, OUTPUT_CAP, OUTPUT_CAP_U16,
        PROMPT_CAP, PROMPT_CAP_U16, PriorCtx, PromptCtx, RepairHint, TINY_PARAMS_BYTES, TinyBackend,
        build_prompt, fingerprint_prompt, fnv1a64, write_stale_marker,
    };

    // The 4096-byte target-board allowance for the distilled table,
    // as a const assertion rather than a runtime check
    // (`assertions_on_constants`).
    const _: () = assert!(TINY_PARAMS_BYTES <= 4096);

    #[test]
    fn fnv1a64_matches_the_standard_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn default_settings_use_the_full_caps() {
        let settings = InferenceSettings::default_settings();
        assert_eq!(settings.max_prompt_bytes, PROMPT_CAP_U16);
        assert_eq!(settings.max_output_bytes, OUTPUT_CAP_U16);
        assert!(settings.validate().is_ok());
        assert_eq!(settings.bytes(), [0, 4, 0, 1]);
    }

    #[test]
    fn invalid_settings_fail_closed() {
        assert!(
            InferenceSettings {
                max_prompt_bytes: 0,
                ..InferenceSettings::default_settings()
            }
            .validate()
            .is_err()
        );
        assert!(
            InferenceSettings {
                max_output_bytes: 0,
                ..InferenceSettings::default_settings()
            }
            .validate()
            .is_err()
        );
        assert!(
            InferenceSettings {
                max_prompt_bytes: 1025,
                ..InferenceSettings::default_settings()
            }
            .validate()
            .is_err()
        );
        assert!(
            InferenceSettings {
                max_output_bytes: 257,
                ..InferenceSettings::default_settings()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn build_prompt_renders_the_tools_block_format() {
        let ctx = PromptCtx {
            turns_left: 10,
            mutations_left: 4,
            last_observation: None,
            repair: None,
            prior: None,
        };
        // HOST-ONLY (E3)
        let mut out = [0u8; PROMPT_CAP];
        let n = build_prompt(&ctx, &mut out);
        let text = core::str::from_utf8(&out[..n]).expect("the prompt is ASCII");
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("TOOLS"));
        // Exactly the six catalog signatures, in catalog order, each
        // byte-identical to `render_signature`.
        for contract in esper_protocol::CATALOG {
            let line = lines.next().expect("one signature line per tool");
            assert!(
                line.starts_with(contract.name),
                "signature line names its tool"
            );
            assert!(line.contains("--"), "signature carries its description");
        }
        assert_eq!(lines.next(), Some("STATE turns=10 muts=4"));
        // No observation and no repair: no LAST or REPAIR lines.
        assert_eq!(lines.next(), Some("EMIT one decision line."));
        assert_eq!(lines.next(), None);
    }

    #[test]
    fn build_prompt_renders_repair_and_last_lines() {
        // The REPAIR line carries attempt, variant, and the grammar
        // one-liner (derived from `decode_line`, SPEC §4.3).
        let ctx = PromptCtx {
            turns_left: 9,
            mutations_left: 3,
            last_observation: None,
            repair: Some(RepairHint {
                attempt: 1,
                variant: "malformed",
            }),
            prior: None,
        };
        // HOST-ONLY (E3)
        let mut out = [0u8; PROMPT_CAP];
        let n = build_prompt(&ctx, &mut out);
        let text = core::str::from_utf8(&out[..n]).expect("the prompt is ASCII");
        assert!(
            text.contains("REPAIR 1:malformed CALL <tool> <json> | ASK <json> | FINISH <json>"),
            "the REPAIR line carries attempt, variant, and the grammar one-liner"
        );
        assert!(text.ends_with("EMIT one decision line.\n"));

        // The LAST line carries the observation bytes raw (no repair
        // line here, so the observation fits whole).
        let ctx = PromptCtx {
            turns_left: 9,
            mutations_left: 3,
            last_observation: Some(b"{\"pin\":4,\"level\":\"low\"}"),
            repair: None,
            prior: None,
        };
        let n = build_prompt(&ctx, &mut out);
        let text = core::str::from_utf8(&out[..n]).expect("the prompt is ASCII");
        assert!(
            text.contains("LAST {\"pin\":4,\"level\":\"low\"}\n"),
            "the LAST line carries the observation bytes raw"
        );
        assert!(text.ends_with("EMIT one decision line.\n"));
    }

    #[test]
    fn build_prompt_renders_the_prior_line_for_continued_runs() {
        // HOST-ONLY (E3)
        let mut out = [0u8; PROMPT_CAP];
        // A root run carries no PRIOR line.
        let root = PromptCtx {
            turns_left: 9,
            mutations_left: 3,
            last_observation: None,
            repair: None,
            prior: None,
        };
        let n = build_prompt(&root, &mut out);
        let text = core::str::from_utf8(&out[..n]).expect("the prompt is ASCII");
        assert!(!text.contains("PRIOR"), "root runs have no PRIOR line");
        // A continued run names the compact epoch as a stale marker
        // and carries the inherited counts; the parent's raw bytes
        // never appear.
        let child = PromptCtx {
            turns_left: 9,
            mutations_left: 3,
            last_observation: None,
            repair: None,
            prior: Some(PriorCtx {
                epoch: 12,
                failed_paths: 1,
                pending: 0,
                facts: 2,
            }),
        };
        let n = build_prompt(&child, &mut out);
        let text = core::str::from_utf8(&out[..n]).expect("the prompt is ASCII");
        assert!(
            text.contains("PRIOR [stale:folded@epoch=12] failed=1 pending=0 facts=2\n"),
            "the PRIOR line names the epoch and the inherited counts, got: {text}"
        );
        assert!(text.ends_with("EMIT one decision line.\n"));
    }

    #[test]
    fn stale_marker_names_the_epoch_and_carries_no_payload() {
        let mut out = [0u8; 32];
        let n = write_stale_marker(0, &mut out);
        assert_eq!(&out[..n], b"[stale:folded@epoch=0]");
        let n = write_stale_marker(4_294_967_295, &mut out);
        assert_eq!(&out[..n], b"[stale:folded@epoch=4294967295]");
    }

    /// The hard rule: the six signature lines are never truncated.
    /// The signature block fits `PROMPT_CAP` with margin for the fixed
    /// framing, and the longest fixed prompt (maximal counters, longest
    /// repair line, no observation) still fits `PROMPT_CAP` — so under
    /// the default [`InferenceSettings`] only the LAST observation tail
    /// can ever truncate. The budget is pinned: signature growth fails
    /// loudly here instead of silently squeezing the observation.
    #[test]
    fn prompt_fixed_part_fits_with_margin() {
        // The signature block: the six `render_signature` lines with
        // their newlines. Never truncated, by test.
        let ctx = PromptCtx {
            turns_left: 10,
            mutations_left: 4,
            last_observation: None,
            repair: None,
            prior: None,
        };
        // HOST-ONLY (E3)
        let mut out = [0u8; PROMPT_CAP];
        let n = build_prompt(&ctx, &mut out);
        let text = core::str::from_utf8(&out[..n]).expect("the prompt is ASCII");
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("TOOLS"));
        let mut sig_bytes = 0usize;
        for contract in esper_protocol::CATALOG {
            let line = lines.next().expect("one signature line per tool");
            assert!(line.starts_with(contract.name));
            sig_bytes += line.len() + 1; // the line plus its newline
        }
        // Margin for the fixed framing around the signatures
        // ("TOOLS\n", the STATE line, the REPAIR line, the EMIT
        // trailer): 128 bytes, well above the ~120 they take at most.
        assert!(
            sig_bytes + 128 <= PROMPT_CAP,
            "the {sig_bytes}-byte signature block must leave margin in {PROMPT_CAP}"
        );
        // The longest fixed rendering: maximal counters, the longest
        // repair variant, no observation. It must fit whole — if the
        // framing ever outgrows the cap, this fails instead of
        // silently cutting a signature.
        let ctx = PromptCtx {
            turns_left: u16::MAX,
            mutations_left: u16::MAX,
            last_observation: None,
            repair: Some(RepairHint {
                attempt: u8::MAX,
                variant: "invalid_args",
            }),
            prior: None,
        };
        let n = build_prompt(&ctx, &mut out);
        assert!(
            n <= PROMPT_CAP,
            "the longest fixed prompt is {n} bytes, over {PROMPT_CAP}"
        );
        let text = core::str::from_utf8(&out[..n]).expect("the prompt is ASCII");
        assert!(text.ends_with("EMIT one decision line.\n"));
        for contract in esper_protocol::CATALOG {
            assert!(
                text.lines().any(|line| line.starts_with(contract.name)),
                "signature for {} is whole",
                contract.name
            );
        }
    }

    #[test]
    fn build_prompt_truncates_only_the_last_observation() {
        // An observation far longer than the buffer: the LAST tail
        // truncates with the marker, and the EMIT trailer survives.
        let observation = [b'x'; PROMPT_CAP];
        let ctx = PromptCtx {
            turns_left: 10,
            mutations_left: 4,
            last_observation: Some(&observation),
            repair: None,
            prior: None,
        };
        // HOST-ONLY (E3)
        let mut out = [0u8; PROMPT_CAP];
        let n = build_prompt(&ctx, &mut out);
        assert_eq!(n, PROMPT_CAP, "the buffer is filled exactly");
        let text = core::str::from_utf8(&out).expect("the prompt is ASCII");
        assert!(
            text.contains("[truncated]\n"),
            "a truncated observation ends with the marker"
        );
        assert!(
            text.ends_with("EMIT one decision line.\n"),
            "the EMIT trailer is never truncated"
        );
        // The TOOLS block is intact: all six signatures present.
        for contract in esper_protocol::CATALOG {
            assert!(
                text.lines().any(|line| line.starts_with(contract.name)),
                "signature for {} survives truncation elsewhere",
                contract.name
            );
        }
        // Same inputs, same truncation: the fingerprint is stable.
        let mut out2 = [0u8; PROMPT_CAP];
        let n2 = build_prompt(&ctx, &mut out2);
        assert_eq!(n2, n);
        assert_eq!(
            fingerprint_prompt(&out[..n]),
            fingerprint_prompt(&out2[..n2])
        );
    }

    #[test]
    fn tiny_backend_answers_its_table() {
        let line = b"FINISH {\"status\": \"completed\", \"summary\": \"done\"}";
        let prompt = b"TOOLS\nSTATE turns_left=10 mutations_left=4\nEMIT one decision line.\n";
        // HOST-ONLY (E3)
        let table = [DistillEntry {
            prompt_fp: fingerprint_prompt(prompt),
            line_hash: fnv1a64(line),
            line: line.as_slice(),
        }];
        let mut tiny = super::TinyBackend::new(&table);
        // HOST-ONLY (E3)
        let mut out = [0u8; OUTPUT_CAP];
        let n = tiny.infer(prompt, &mut out).expect("known prompt");
        assert_eq!(&out[..n], line);
        let usage = tiny.last_usage();
        assert_eq!(
            usage.input_tokens,
            u32::try_from(prompt.len()).expect("the prompt fits in u32")
        );
        assert_eq!(
            usage.output_tokens,
            u32::try_from(line.len()).expect("the line fits in u32")
        );
    }

    #[test]
    fn tiny_backend_rejects_unknown_prompts_and_tampered_lines() {
        let line = b"CALL gpio_pin_read {\"pin\": 4}";
        // HOST-ONLY (E3)
        let table = [DistillEntry {
            prompt_fp: fingerprint_prompt(b"known"),
            line_hash: fnv1a64(line),
            line: line.as_slice(),
        }];
        let mut tiny = super::TinyBackend::new(&table);
        // HOST-ONLY (E3)
        let mut out = [0u8; OUTPUT_CAP];
        assert_eq!(
            tiny.infer(b"unknown", &mut out),
            Err(BackendError::UnknownPrompt)
        );
        // A tampered line hash fails closed even for a known prompt.
        let tampered = [DistillEntry {
            prompt_fp: fingerprint_prompt(b"known"),
            line_hash: 0xdead_beef,
            line: line.as_slice(),
        }];
        let mut tampered_tiny = super::TinyBackend::new(&tampered);
        assert_eq!(
            tampered_tiny.infer(b"known", &mut out),
            Err(BackendError::PolicyMismatch)
        );
        // A line longer than the output buffer is dropped, not cut.
        let big = [DistillEntry {
            prompt_fp: fingerprint_prompt(b"known"),
            line_hash: fnv1a64(line),
            line: line.as_slice(),
        }];
        let mut big_tiny = super::TinyBackend::new(&big);
        // HOST-ONLY (E3)
        let mut small = [0u8; 4];
        assert_eq!(
            big_tiny.infer(b"known", &mut small),
            Err(BackendError::OutputTooLong)
        );
    }

    #[test]
    fn tiny_unemit_is_a_noop() {
        let line = b"FINISH {\"status\": \"completed\", \"summary\": \"done\"}";
        let prompt = b"TOOLS\nSTATE turns_left=1 mutations_left=0\nEMIT one decision line.\n";
        // HOST-ONLY (E3)
        let table = [DistillEntry {
            prompt_fp: fingerprint_prompt(prompt),
            line_hash: fnv1a64(line),
            line: line.as_slice(),
        }];
        let mut tiny = super::TinyBackend::new(&table);
        // HOST-ONLY (E3)
        let mut out = [0u8; OUTPUT_CAP];
        let first = tiny.infer(prompt, &mut out).expect("known prompt");
        tiny.unemit();
        let second = tiny.infer(prompt, &mut out).expect("known prompt");
        assert_eq!(first, second);
        assert_eq!(&out[..second], line);
    }

    #[test]
    fn tiny_params_budget_is_pinned() {
        assert_eq!(TINY_PARAMS_BYTES, 2156);
    }

    #[test]
    fn backend_errors_render() {
        assert_eq!(
            BackendError::UnknownPrompt.to_string(),
            "the tiny backend has no line for this prompt"
        );
    }

    /// Correction (E3 steering): the scripted bundle digest binds the
    /// [`InferenceSettings`] bytes too — the same script under a
    /// different buffer policy is a different bundle (defense in
    /// depth; the seed snapshot already binds them).
    #[test]
    #[cfg(feature = "host")]
    fn scripted_bundle_binds_the_settings() {
        let lines = vec![b"FINISH {\"status\": \"ok\"}".to_vec()];
        let full =
            super::ScriptedBackend::new(lines.clone(), InferenceSettings::default_settings());
        let narrowed = super::ScriptedBackend::new(
            lines,
            InferenceSettings {
                max_prompt_bytes: 512,
                ..InferenceSettings::default_settings()
            },
        );
        assert_ne!(
            full.bundle_id(),
            narrowed.bundle_id(),
            "the same script under different settings must not share a bundle id"
        );
        // And the digest is stable: rebuilding gives the same id.
        let rebuilt = super::ScriptedBackend::new(
            vec![b"FINISH {\"status\": \"ok\"}".to_vec()],
            InferenceSettings::default_settings(),
        );
        assert_eq!(full.bundle_id(), rebuilt.bundle_id());
    }

    /// Correction (E3 steering): every backend is `Send` — the
    /// `ModelBackend: Send` supertrait lets the Tokio async driver move
    /// `&mut dyn ModelBackend` across workers (clippy
    /// `future_not_send`). `TinyBackend` only borrows `&[u8]` and
    /// `u64`s, which are `Sync`, so the borrow is `Send`.
    #[test]
    fn backends_are_send() {
        fn assert_send<T: Send>() {}
        // The supertrait carries it: any `ModelBackend` is `Send`.
        fn assert_backend_send<T: ModelBackend>() {}
        assert_send::<TinyBackend<'static>>();
        #[cfg(feature = "host")]
        assert_send::<super::ScriptedBackend>();
        assert_backend_send::<TinyBackend<'static>>();
    }
}
