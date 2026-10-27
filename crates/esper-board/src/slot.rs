//! A slot: one Waymaker bank pair on flash, holding one workflow's runs.
//!
//! `waymaker_drive::Driver::at_bank` boots whatever run a bank pair's
//! authoritative bank names, and follows `continue_as_new` from bank to
//! bank — but it cannot install a pair's *first* run: a swap needs an
//! authoritative bank to retire. [`commission`] is that first install,
//! the same five steps Waymaker's own rig performs on a fresh part
//! (`waymaker-rig`'s `install`): erase, barrier, header, barrier, seal,
//! barrier. A reset anywhere in them leaves a pair with no authoritative
//! bank, which the next boot commissions again; a reset after the last
//! barrier leaves a run the driver boots.
//!
//! [`reinstall`] is the board's answer to a journal Waymaker refuses. A
//! reset between a frame and its seal, or inside a frame, leaves a tail
//! with no append point, and Waymaker refuses to guess at it (its ADR
//! 0018): recycling such a bank is §10's `continue_as_new`. The board
//! does exactly that with the one input it has durably — the segment's
//! own, in the authoritative header — installing it into the *other*
//! bank at the next generation. Until that seal lands the refused bank
//! stays authoritative, so a reset in the middle reinstalls again; after
//! it, the segment replays from its start. Effects its discarded history
//! performed may be performed again, which is Waymaker's at-least-once
//! and every board tool is safe under.
//!
//! [`authority`] and [`terminal`] only *read*, through Waymaker's own
//! decoders — the header's seal, the bank selection, the recovery scan.
//! Nothing here decides what a record means.

use waymaker_core::{RecordRef, RunId};
use waymaker_flash::bank::{self, Authority, BankHeader, BankId, BankLayout, Generation};
use waymaker_flash::recovery::{JournalRegion, Recovery};
use waymaker_flash::storage::StableStorage;

/// The widest header page a slot reads: a header with a full run
/// input, rounded to the read unit.
pub const HEADER_PAGE: usize = 1024;

/// The authoritative bank's header, as a slot reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Held {
    /// The bank it is in.
    pub bank: BankId,
    /// The generation its seal carries.
    pub generation: Generation,
    /// The Waymaker run it names.
    pub run: RunId,
    /// The workflow kind.
    pub kind: u16,
    /// The run input's length (the bytes are in the caller's buffer).
    pub input_len: usize,
}

/// Why a slot could not be read or written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SlotFault<E> {
    /// The media refused.
    #[error("the media refused")]
    Media(E),
    /// The header or a buffer does not fit, or the layout refused it.
    #[error("the slot's header does not fit")]
    Layout,
    /// Both banks are sealed at one generation: a state no protocol
    /// produces, reported rather than resolved.
    #[error("both banks claim one generation")]
    Ambiguous,
}

/// Read one bank's header frame into `page` and its sealed generation.
fn sealed<S: StableStorage>(
    storage: &mut S,
    layout: BankLayout,
    id: BankId,
    page: &mut [u8],
) -> Result<Option<(Generation, usize)>, SlotFault<S::Error>> {
    let region = layout.bank(id);
    let mut seal = [0u8; 64];
    let seal = seal
        .get_mut(..region.seal_bytes() as usize)
        .ok_or(SlotFault::Layout)?;
    storage
        .read(region.seal_offset(), seal)
        .map_err(SlotFault::Media)?;
    let read = storage.geometry().read_size().max(1);
    let wanted = page.len().min(region.payload_bytes() as usize);
    let wanted = wanted - wanted % read as usize;
    let frame = page.get_mut(..wanted).ok_or(SlotFault::Layout)?;
    storage
        .read(region.base(), frame)
        .map_err(SlotFault::Media)?;
    let Ok(len) = bank::header_len_of(frame) else {
        return Ok(None);
    };
    let Some(header) = frame.get(..len) else {
        return Ok(None);
    };
    Ok(bank::sealed_generation(header, seal).map(|generation| (generation, len)))
}

/// The authoritative bank's header, with its run input copied into
/// `input`; `None` for a pair no run was ever installed in.
///
/// # Errors
///
/// [`SlotFault`] when the media refused, the input does not fit
/// `input`, or both banks claim one generation.
pub fn authority<S: StableStorage>(
    storage: &mut S,
    layout: BankLayout,
    input: &mut [u8],
) -> Result<Option<Held>, SlotFault<S::Error>> {
    let mut page = [0u8; HEADER_PAGE];
    let a = sealed(storage, layout, BankId::A, &mut page)?.map(|(generation, _)| generation);
    let b = sealed(storage, layout, BankId::B, &mut page)?.map(|(generation, _)| generation);
    let (bank, generation) = match bank::select([a, b]) {
        Authority::Unsealed => return Ok(None),
        Authority::Ambiguous { .. } => return Err(SlotFault::Ambiguous),
        Authority::Bank { id, generation } => (id, generation),
    };
    let Some((_, len)) = sealed(storage, layout, bank, &mut page)? else {
        return Err(SlotFault::Layout);
    };
    let frame = page.get(..len).ok_or(SlotFault::Layout)?;
    let header = bank::decode_header(frame).map_err(|_| SlotFault::Layout)?;
    input
        .get_mut(..header.input.len())
        .ok_or(SlotFault::Layout)?
        .copy_from_slice(header.input);
    Ok(Some(Held {
        bank,
        generation,
        run: header.run,
        kind: header.workflow_kind,
        input_len: header.input.len(),
    }))
}

/// Install `header` as the pair's first run, at generation 1 in bank A,
/// over whatever the pair held before.
///
/// # Errors
///
/// [`SlotFault`] when the media refused or the header does not fit.
pub fn commission<S: StableStorage>(
    storage: &mut S,
    layout: BankLayout,
    header: &BankHeader<'_>,
) -> Result<(), SlotFault<S::Error>> {
    let capacity = storage.geometry().capacity();
    storage
        .erase(0, capacity)
        .and_then(|()| storage.barrier())
        .map_err(SlotFault::Media)?;
    install(storage, layout, BankId::A, Generation(1), header)
}

/// Reinstall the authoritative segment's own input into the other bank,
/// one generation up, under the run's successor id — after Waymaker
/// refused the authoritative bank's journal. See the module docs.
///
/// # Errors
///
/// [`SlotFault`] when the media refused, nothing is installed, or the
/// generation or run id is exhausted.
pub fn reinstall<S: StableStorage>(
    storage: &mut S,
    layout: BankLayout,
) -> Result<RunId, SlotFault<S::Error>> {
    let mut input = [0u8; HEADER_PAGE];
    let Some(held) = authority(storage, layout, &mut input)? else {
        return Err(SlotFault::Layout);
    };
    let input = input.get(..held.input_len).ok_or(SlotFault::Layout)?;
    let run = held.run.successor().ok_or(SlotFault::Layout)?;
    let generation = held.generation.successor().ok_or(SlotFault::Layout)?;
    let mut page = [0u8; HEADER_PAGE];
    let Some((_, len)) = sealed(storage, layout, held.bank, &mut page)? else {
        return Err(SlotFault::Layout);
    };
    let old = bank::decode_header(page.get(..len).ok_or(SlotFault::Layout)?)
        .map_err(|_| SlotFault::Layout)?;
    let header = BankHeader {
        run,
        align: layout.align(),
        workflow_kind: old.workflow_kind,
        workflow_version: old.workflow_version,
        input_schema: old.input_schema,
        input,
    };
    let target = held.bank.other();
    let region = layout.bank(target);
    storage
        .erase(region.base(), region.bytes())
        .and_then(|()| storage.barrier())
        .map_err(SlotFault::Media)?;
    install(storage, layout, target, generation, &header)?;
    Ok(run)
}

/// Header, barrier, seal, barrier: one erased bank becomes authoritative
/// at `generation` the moment its seal is durable.
fn install<S: StableStorage>(
    storage: &mut S,
    layout: BankLayout,
    id: BankId,
    generation: Generation,
    header: &BankHeader<'_>,
) -> Result<(), SlotFault<S::Error>> {
    let mut page = [0xFF_u8; HEADER_PAGE];
    let len = bank::encode_header(header, &mut page).map_err(|_| SlotFault::Layout)?;
    let padded = layout.align().round_up(len).ok_or(SlotFault::Layout)?;
    // Padding past the frame stays erased: the journal starts at the
    // header's `journal_offset`, and erased bytes there read as no record.
    if let Some(tail) = page.get_mut(len..padded) {
        tail.fill(0xFF);
    }
    let region = layout.bank(id);
    let frame = page.get(..padded).ok_or(SlotFault::Layout)?;
    storage
        .program(region.base(), frame)
        .and_then(|()| storage.barrier())
        .map_err(SlotFault::Media)?;
    let header_frame = page.get(..len).ok_or(SlotFault::Layout)?;
    let seal = bank::seal_for(header_frame, generation).map_err(|_| SlotFault::Layout)?;
    let mut sealed = [0xFF_u8; 64];
    let n = bank::encode_seal(&seal, layout.align(), &mut sealed).map_err(|_| SlotFault::Layout)?;
    let sealed = sealed.get(..n).ok_or(SlotFault::Layout)?;
    storage
        .program(region.seal_offset(), sealed)
        .and_then(|()| storage.barrier())
        .map_err(SlotFault::Media)
}

/// The authoritative run's terminal record, if its journal holds one:
/// completed (else failed), with its bytes copied into `out`.
///
/// # Errors
///
/// [`SlotFault`] when the media refused or the record does not fit.
pub fn terminal<S: StableStorage>(
    storage: &mut S,
    layout: BankLayout,
    out: &mut [u8],
) -> Result<Option<(bool, usize)>, SlotFault<S::Error>> {
    let mut page = [0u8; HEADER_PAGE];
    let mut input = [0u8; HEADER_PAGE];
    let Some(held) = authority(storage, layout, &mut input)? else {
        return Ok(None);
    };
    let Some((_, len)) = sealed(storage, layout, held.bank, &mut page)? else {
        return Err(SlotFault::Layout);
    };
    let frame = page.get(..len).ok_or(SlotFault::Layout)?;
    let header = bank::decode_header(frame).map_err(|_| SlotFault::Layout)?;
    let region = JournalRegion::of(layout, held.bank, &header).map_err(|_| SlotFault::Layout)?;
    let mut recovery = Recovery::new(region, storage);
    let mut record_page = [0u8; HEADER_PAGE];
    let mut found = None;
    while let Some(Ok(record)) = recovery.next(&mut record_page) {
        let (completed, bytes) = match record {
            RecordRef::RunCompleted { result } => (true, result),
            RecordRef::RunFailed { error } => (false, error),
            _ => continue,
        };
        let into = out.get_mut(..bytes.len()).ok_or(SlotFault::Layout)?;
        into.copy_from_slice(bytes);
        found = Some((completed, bytes.len()));
    }
    Ok(found)
}
