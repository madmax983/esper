//! Versioned, integrity-checked snapshot encoding for [`CompactState`](crate::compact::CompactState) (SPEC §20.3).
//!
//! Byte layout:
//!
//! ```text
//! [version: 1][payload][integrity: u64 LE]
//! ```
//!
//! `integrity` is FNV-1a-64 over `version || payload`. The payload is a
//! fixed-order little-endian encoding of the compact state:
//!
//! ```text
//! objective_digest: u64
//! budgets: model_turns u16, input_tokens u32, output_tokens u32,
//!          elapsed_ms u64, radio_bytes u32, mutations u16,
//!          consecutive_errors u8
//! versions: workflow u32, model u64, catalog u64, policy u64
//! completed: count u8, then per note: len u8, bytes[len]
//! open:      count u8, then per note: len u8, bytes[len]
//! accepted:  count u8, then per digest: u64
//! facts:     count u8, then per fact: len u8, bytes[len], source_seq u32
//! failed:    count u8, then per path: tool u8, args_digest u64, error u8
//! pending:   count u8, then per item: kind u8, seq u32, digest u64
//! fingerprints: count u8, then per fingerprint: u64
//! ```
//!
//! [`decode`] fails closed on truncated input
//! ([`Error::SnapshotTruncated`]), checksum mismatch
//! ([`Error::SnapshotChecksumMismatch`]), version mismatch
//! ([`Error::SnapshotVersionMismatch`]), and payload bytes that pass
//! the checksum but do not form a valid state
//! ([`Error::SnapshotCorrupt`]). [`verify`] performs the
//! length/version/integrity checks without building the state.

use crate::budget::ResourceBudget;
use crate::compact::{
    CompactState, Fact, FailedPath, Note, PendingItem, PendingKind, VersionSet, MAX_ACCEPTED_DECISIONS,
    MAX_COMPLETED, MAX_FACTS, MAX_FAILED_PATHS, MAX_FINGERPRINTS, MAX_OPEN, MAX_PENDING, NOTE_MAX,
};
use crate::error::{Error, ErrorCode};
use crate::ids::{Digest, ToolId};
use crate::mask::fnv1a64;

/// Snapshot format version. Bumped only with a migration path;
/// unknown versions fail closed (SPEC §20.6).
pub const SNAPSHOT_VERSION: u8 = 1;

/// Length of the trailing integrity field.
const CHECKSUM_LEN: usize = 8;

/// Exact bytes [`encode`] writes for `state` (version + payload +
/// integrity).
#[must_use]
pub fn encoded_len(state: &CompactState) -> usize {
    1 + payload_len(state) + CHECKSUM_LEN
}

/// Encode `state` into `out`, returning the bytes written.
///
/// # Errors
///
/// Returns [`Error::SnapshotBufferTooSmall`] when `out` holds fewer
/// than [`encoded_len`] bytes.
pub fn encode(state: &CompactState, out: &mut [u8]) -> Result<usize, Error> {
    let need = encoded_len(state);
    if out.len() < need {
        return Err(Error::SnapshotBufferTooSmall);
    }
    let mut pos: usize = 0;
    put_u8(out, &mut pos, SNAPSHOT_VERSION)?;
    put_u64(out, &mut pos, state.objective_digest.get())?;
    let b = state.budgets;
    put_u16(out, &mut pos, b.model_turns)?;
    put_u32(out, &mut pos, b.input_tokens)?;
    put_u32(out, &mut pos, b.output_tokens)?;
    put_u64(out, &mut pos, b.elapsed_ms)?;
    put_u32(out, &mut pos, b.radio_bytes)?;
    put_u16(out, &mut pos, b.mutations)?;
    put_u8(out, &mut pos, b.consecutive_errors)?;
    let v = state.versions;
    put_u32(out, &mut pos, v.workflow)?;
    put_u64(out, &mut pos, v.model.get())?;
    put_u64(out, &mut pos, v.catalog.get())?;
    put_u64(out, &mut pos, v.policy.get())?;
    put_list(out, &mut pos, state.completed(), |out, pos, note| {
        put_note(out, pos, note)
    })?;
    put_list(out, &mut pos, state.open(), |out, pos, note| {
        put_note(out, pos, note)
    })?;
    put_list(out, &mut pos, state.accepted_decisions(), |out, pos, digest| {
        put_u64(out, pos, digest.get())
    })?;
    put_list(out, &mut pos, state.facts(), |out, pos, fact| {
        put_note(out, pos, &fact.text)?;
        put_u32(out, pos, fact.source_seq)
    })?;
    put_list(out, &mut pos, state.failed_paths(), |out, pos, path| {
        put_u8(out, pos, path.tool.get())?;
        put_u64(out, pos, path.args_digest.get())?;
        put_u8(out, pos, path.error.code())
    })?;
    put_list(out, &mut pos, state.pending(), |out, pos, item| {
        put_u8(out, pos, item.kind.code())?;
        put_u32(out, pos, item.seq)?;
        put_u64(out, pos, item.digest.get())
    })?;
    put_list(out, &mut pos, state.fingerprints(), |out, pos, fp| {
        put_u64(out, pos, *fp)
    })?;
    let checksum = fnv1a64(out.get(..pos).ok_or(Error::SnapshotBufferTooSmall)?);
    put_u64(out, &mut pos, checksum)?;
    Ok(pos)
}

/// Decode a snapshot, verifying version and integrity first.
///
/// # Errors
///
/// Returns [`Error::SnapshotTruncated`] for short input,
/// [`Error::SnapshotVersionMismatch`] for a foreign version byte,
/// [`Error::SnapshotChecksumMismatch`] when the integrity check fails,
/// and [`Error::SnapshotCorrupt`] when the payload passes the
/// checksum but does not decode to a valid state.
pub fn decode(bytes: &[u8]) -> Result<CompactState, Error> {
    verify(bytes)?;
    let mut r = Reader {
        bytes,
        pos: 1,
        end: bytes.len() - CHECKSUM_LEN,
    };
    let objective = Digest::new(r.take_u64()?);
    let budgets = ResourceBudget::new(
        r.take_u16()?,
        r.take_u32()?,
        r.take_u32()?,
        r.take_u64()?,
        r.take_u32()?,
        r.take_u16()?,
        r.take_u8()?,
    );
    let versions = VersionSet {
        workflow: r.take_u32()?,
        model: Digest::new(r.take_u64()?),
        catalog: Digest::new(r.take_u64()?),
        policy: Digest::new(r.take_u64()?),
    };
    let mut state = CompactState::new(objective, budgets, versions);
    let n = r.take_count(MAX_COMPLETED)?;
    for _ in 0..n {
        state.record_completed(take_note(&mut r)?);
    }
    let n = r.take_count(MAX_OPEN)?;
    for _ in 0..n {
        state.record_open(take_note(&mut r)?);
    }
    let n = r.take_count(MAX_ACCEPTED_DECISIONS)?;
    for _ in 0..n {
        state.record_decision(Digest::new(r.take_u64()?));
    }
    let n = r.take_count(MAX_FACTS)?;
    for _ in 0..n {
        let text = take_note(&mut r)?;
        let source_seq = r.take_u32()?;
        state.record_fact(Fact { text, source_seq });
    }
    let n = r.take_count(MAX_FAILED_PATHS)?;
    for _ in 0..n {
        let path = FailedPath {
            tool: ToolId::new(r.take_u8()?),
            args_digest: Digest::new(r.take_u64()?),
            error: ErrorCode::from_code(r.take_u8()?).ok_or(Error::SnapshotCorrupt)?,
        };
        state
            .record_failed_path(path)
            .map_err(|_| Error::SnapshotCorrupt)?;
    }
    let n = r.take_count(MAX_PENDING)?;
    for _ in 0..n {
        let item = PendingItem {
            kind: PendingKind::from_code(r.take_u8()?).ok_or(Error::SnapshotCorrupt)?,
            seq: r.take_u32()?,
            digest: Digest::new(r.take_u64()?),
        };
        state
            .record_pending(item)
            .map_err(|_| Error::SnapshotCorrupt)?;
    }
    let n = r.take_count(MAX_FINGERPRINTS)?;
    for _ in 0..n {
        state.record_fingerprint(r.take_u64()?);
    }
    if r.pos != r.end {
        return Err(Error::SnapshotCorrupt);
    }
    Ok(state)
}

/// Check a snapshot's length, version, and integrity without
/// decoding the payload.
///
/// # Errors
///
/// Returns [`Error::SnapshotTruncated`] for short input,
/// [`Error::SnapshotVersionMismatch`] for a foreign version byte, and
/// [`Error::SnapshotChecksumMismatch`] when the integrity check fails.
pub fn verify(bytes: &[u8]) -> Result<(), Error> {
    if bytes.len() < 1 + CHECKSUM_LEN {
        return Err(Error::SnapshotTruncated);
    }
    let version = bytes.first().ok_or(Error::SnapshotTruncated)?;
    if *version != SNAPSHOT_VERSION {
        return Err(Error::SnapshotVersionMismatch { found: *version });
    }
    let end = bytes.len() - CHECKSUM_LEN;
    let stored = bytes
        .get(end..end + CHECKSUM_LEN)
        .ok_or(Error::SnapshotTruncated)?;
    let mut arr = [0u8; CHECKSUM_LEN];
    arr.copy_from_slice(stored);
    if u64::from_le_bytes(arr) != fnv1a64(bytes.get(..end).ok_or(Error::SnapshotTruncated)?) {
        return Err(Error::SnapshotChecksumMismatch);
    }
    Ok(())
}

/// Payload length without the version byte and integrity field.
fn payload_len(state: &CompactState) -> usize {
    let mut len: usize = 8 + (2 + 4 + 4 + 8 + 4 + 2 + 1) + (4 + 8 + 8 + 8);
    len = len.saturating_add(
        1 + state
            .completed()
            .iter()
            .fold(0usize, |a, n| a.saturating_add(1 + n.as_bytes().len())),
    );
    len = len.saturating_add(
        1 + state
            .open()
            .iter()
            .fold(0usize, |a, n| a.saturating_add(1 + n.as_bytes().len())),
    );
    len = len.saturating_add(1 + state.accepted_decisions().len().saturating_mul(8));
    len = len.saturating_add(
        1 + state.facts().iter().fold(0usize, |a, f| {
            a.saturating_add(1 + f.text.as_bytes().len() + 4)
        }),
    );
    len = len.saturating_add(1 + state.failed_paths().len().saturating_mul(1 + 8 + 1));
    len = len.saturating_add(1 + state.pending().len().saturating_mul(1 + 4 + 8));
    len = len.saturating_add(1 + state.fingerprints().len().saturating_mul(8));
    len
}

/// Bounded cursor writer; every write fails closed.
fn put_u8(out: &mut [u8], pos: &mut usize, v: u8) -> Result<(), Error> {
    out.get_mut(*pos).map_or(Err(Error::SnapshotBufferTooSmall), |slot| {
        *slot = v;
        *pos = pos.saturating_add(1);
        Ok(())
    })
}

fn put_u16(out: &mut [u8], pos: &mut usize, v: u16) -> Result<(), Error> {
    for byte in v.to_le_bytes() {
        put_u8(out, pos, byte)?;
    }
    Ok(())
}

fn put_u32(out: &mut [u8], pos: &mut usize, v: u32) -> Result<(), Error> {
    for byte in v.to_le_bytes() {
        put_u8(out, pos, byte)?;
    }
    Ok(())
}

fn put_u64(out: &mut [u8], pos: &mut usize, v: u64) -> Result<(), Error> {
    for byte in v.to_le_bytes() {
        put_u8(out, pos, byte)?;
    }
    Ok(())
}

fn put_note(out: &mut [u8], pos: &mut usize, note: &Note) -> Result<(), Error> {
    let bytes = note.as_bytes();
    let len = u8::try_from(bytes.len()).map_err(|_| Error::SnapshotCorrupt)?;
    put_u8(out, pos, len)?;
    for byte in bytes {
        put_u8(out, pos, *byte)?;
    }
    Ok(())
}

/// Write a counted list: `count u8` then one `item` call per element.
fn put_list<T>(
    out: &mut [u8],
    pos: &mut usize,
    items: &[T],
    mut put_item: impl FnMut(&mut [u8], &mut usize, &T) -> Result<(), Error>,
) -> Result<(), Error> {
    let count = u8::try_from(items.len()).map_err(|_| Error::SnapshotCorrupt)?;
    put_u8(out, pos, count)?;
    for item in items {
        put_item(out, pos, item)?;
    }
    Ok(())
}

/// Bounded cursor reader over the payload region.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let next = self.pos.saturating_add(n);
        if next > self.end {
            return Err(Error::SnapshotTruncated);
        }
        let slice = self
            .bytes
            .get(self.pos..next)
            .ok_or(Error::SnapshotTruncated)?;
        self.pos = next;
        Ok(slice)
    }

    fn take_u8(&mut self) -> Result<u8, Error> {
        self.take(1).map(|b| b[0])
    }

    fn take_u16(&mut self) -> Result<u16, Error> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn take_u32(&mut self) -> Result<u32, Error> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn take_u64(&mut self) -> Result<u64, Error> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// A list count, failing closed when it exceeds the list's capacity.
    fn take_count(&mut self, capacity: usize) -> Result<u8, Error> {
        let n = self.take_u8()?;
        if usize::from(n) > capacity {
            return Err(Error::SnapshotCorrupt);
        }
        Ok(n)
    }
}

/// Read one note, failing closed on overlong or truncated text.
fn take_note(r: &mut Reader) -> Result<Note, Error> {
    let len = r.take_u8()?;
    if usize::from(len) > NOTE_MAX {
        return Err(Error::SnapshotCorrupt);
    }
    let bytes = r.take(usize::from(len))?;
    Note::from_bytes(bytes).map_err(|_| Error::SnapshotCorrupt)
}
