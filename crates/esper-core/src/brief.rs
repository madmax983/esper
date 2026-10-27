//! The brief: a goal's objective and recalled memory, pre-rendered
//! for a spawned run's prompt (SPEC §21.4).
//!
//! ```text
//! GOAL <objective>
//! MEM <label> = <value>
//! ```
//!
//! One `GOAL` line, then one `MEM` line per recalled entry while the
//! block fits [`BRIEF_CAP`]; the first line that does not fit and
//! every later one are dropped, so composition is deterministic. The
//! inputs are [`Note`]s that [`crate::memory::ingest`] already masked
//! and stripped of control bytes, so no brief line can forge another
//! prompt line.
//!
//! The brief is run identity: the runtime binds [`Brief::digest`] into
//! the seed snapshot, so a reboot can never swap it under a committed
//! history.

use crate::compact::Note;
use crate::error::Error;
use crate::ids::Digest;
use crate::memory::Recall;

/// Maximum bytes of one rendered brief.
pub const BRIEF_CAP: usize = 256;

/// The `GOAL` line prefix.
const GOAL_PREFIX: &[u8] = b"GOAL ";
/// The `MEM` line prefix.
const MEM_PREFIX: &[u8] = b"MEM ";
/// The `MEM` label/value separator.
const MEM_SEPARATOR: &[u8] = b" = ";

/// The rendered brief: at most [`BRIEF_CAP`] bytes of newline-ended
/// lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Brief {
    bytes: [u8; BRIEF_CAP],
    len: u16,
    lines: u8,
}

impl Brief {
    /// Compose the brief for `objective` and its `recall` (SPEC §21.4).
    #[must_use]
    pub fn compose(objective: &Note, recall: &Recall<'_>) -> Self {
        let mut brief = Self {
            bytes: [0; BRIEF_CAP],
            len: 0,
            lines: 0,
        };
        // The GOAL line always fits: 5 + NOTE_MAX + 1 <= BRIEF_CAP.
        brief.push_line(&[GOAL_PREFIX, objective.as_bytes()]);
        for entry in recall.entries() {
            if !brief.push_line(&[
                MEM_PREFIX,
                entry.label.as_bytes(),
                MEM_SEPARATOR,
                entry.value.as_bytes(),
            ]) {
                break;
            }
        }
        brief
    }

    /// Rebuild a brief from its rendered bytes — the receiving side of a
    /// spawn request, which must not trust what it was handed.
    ///
    /// # Errors
    ///
    /// [`Error::BriefInvalid`] unless `bytes` is at most [`BRIEF_CAP`],
    /// opens with `GOAL `, every line after it opens with `MEM `, every
    /// line ends with a newline, and no other control byte appears.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > BRIEF_CAP || !bytes.starts_with(GOAL_PREFIX) || !bytes.ends_with(b"\n") {
            return Err(Error::BriefInvalid);
        }
        let mut lines: u8 = 0;
        for (index, line) in bytes
            .split(|b| *b == b'\n')
            .take_while(|line| !line.is_empty())
            .enumerate()
        {
            let prefix = if index == 0 { GOAL_PREFIX } else { MEM_PREFIX };
            if !line.starts_with(prefix) || line.iter().any(|b| *b < 0x20 || *b == 0x7f) {
                return Err(Error::BriefInvalid);
            }
            lines = lines.saturating_add(1);
        }
        // Every byte must belong to a counted line: no empty line in
        // the middle (`take_while` stops at one).
        let counted: usize = bytes
            .split(|b| *b == b'\n')
            .take(usize::from(lines))
            .map(|line| line.len() + 1)
            .sum();
        if counted != bytes.len() {
            return Err(Error::BriefInvalid);
        }
        let mut brief = Self {
            bytes: [0; BRIEF_CAP],
            len: u16::try_from(bytes.len()).map_err(|_| Error::BriefInvalid)?,
            lines,
        };
        brief
            .bytes
            .get_mut(..bytes.len())
            .ok_or(Error::BriefInvalid)?
            .copy_from_slice(bytes);
        Ok(brief)
    }

    /// Append one line made of `parts` plus a newline, or nothing when
    /// it does not fit. Returns whether it was appended.
    fn push_line(&mut self, parts: &[&[u8]]) -> bool {
        let need = parts.iter().map(|part| part.len()).sum::<usize>() + 1;
        let start = usize::from(self.len);
        let end = start + need;
        let Some(dst) = self.bytes.get_mut(start..end) else {
            return false;
        };
        let mut at = 0;
        for part in parts {
            if let Some(chunk) = dst.get_mut(at..at + part.len()) {
                chunk.copy_from_slice(part);
            }
            at += part.len();
        }
        if let Some(newline) = dst.get_mut(at) {
            *newline = b'\n';
        }
        // `end <= BRIEF_CAP = 256` fits u16.
        self.len = u16::try_from(end).unwrap_or(self.len);
        self.lines = self.lines.saturating_add(1);
        true
    }

    /// The rendered bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..usize::from(self.len)).unwrap_or(&[])
    }

    /// Lines rendered (the `GOAL` line plus the `MEM` lines kept).
    #[must_use]
    pub const fn lines(&self) -> u8 {
        self.lines
    }

    /// FNV-1a-64 over the rendered bytes: the seed binding.
    #[must_use]
    pub fn digest(&self) -> u64 {
        Digest::of_bytes(self.as_bytes()).get()
    }
}
