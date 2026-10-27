//! The item log: where `STORE`/`LOAD` and `SNAP_STORE`/`SNAP_LOAD` keep
//! their page-sized items on flash.
//!
//! A checkpoint (SPEC §21.4) and a rollover snapshot (SPEC §20.4) are
//! written item by item under a digest, and read back by the next
//! segment — after a `continue_as_new` installed a new journal, so the
//! old journal cannot hold them. They are the world's to keep, exactly
//! as a pin's level is: the activity that stores one is an effect, and
//! its recorded answer says it happened.
//!
//! # Layout
//!
//! A log is two equal areas. Each holds the items of **one** digest:
//!
//! ```text
//! area: [ header: magic, generation, digest, check ][ item ][ item ] … erased
//! item: [ index: u16, len: u16, check: u32 ][ bytes, padded to 4 ]
//! ```
//!
//! The first item of a new digest erases the area holding the *older*
//! of the two digests and opens it under the next generation; the other
//! area keeps the previous digest, so the newest complete checkpoint is
//! always on media while the next one is being written. An item is
//! appended, never rewritten; [`ItemLog::load`] returns the last whole
//! copy of an index, so a redelivered store is harmless.
//!
//! A torn item (a reset mid-program) fails its check and is skipped; a
//! torn header leaves an area no load trusts, which the next new digest
//! erases. Every check is FNV-1a over the bytes it covers.

use waymaker_flash::storage::StableStorage;

/// The largest item: one Waymaker page record.
pub const ITEM_MAX: usize = 480;

const MAGIC: u32 = 0x4553_494C; // "ESIL"
const HEADER_BYTES: u32 = 4 + 4 + 8 + 4;
const ITEM_HEADER_BYTES: u32 = 2 + 2 + 4;
const ERASED: u32 = 0xFFFF_FFFF;

/// Why the item log refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ItemFault<E> {
    /// No whole copy of that item is on media.
    #[error("no such item")]
    Missing,
    /// The item does not fit, or its area is full.
    #[error("the item log is full")]
    Full,
    /// The media refused.
    #[error("the media refused")]
    Media(E),
}

/// One area's header, as read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Area {
    generation: u32,
    digest: u64,
}

/// A two-area item log over a partition of `S`.
pub struct ItemLog<S: StableStorage> {
    storage: S,
}

impl<S: StableStorage> ItemLog<S> {
    /// A log over `storage`, whose capacity is split into two areas of
    /// whole erase blocks.
    pub const fn new(storage: S) -> Self {
        Self { storage }
    }

    /// The partition underneath, for a test or the board's diagnostics.
    pub const fn storage(&mut self) -> &mut S {
        &mut self.storage
    }

    fn area_bytes(&self) -> u32 {
        let geometry = self.storage.geometry();
        let blocks = geometry.capacity() / geometry.erase_size() / 2;
        blocks * geometry.erase_size()
    }

    fn area_base(&self, area: usize) -> u32 {
        if area == 0 { 0 } else { self.area_bytes() }
    }

    fn read_word<const N: usize>(&mut self, offset: u32) -> Result<[u8; N], ItemFault<S::Error>> {
        let mut bytes = [0u8; N];
        self.storage
            .read(offset, &mut bytes)
            .map_err(ItemFault::Media)?;
        Ok(bytes)
    }

    fn header(&mut self, area: usize) -> Result<Option<Area>, ItemFault<S::Error>> {
        let bytes: [u8; HEADER_BYTES as usize] = self.read_word(self.area_base(area))?;
        let [
            m0,
            m1,
            m2,
            m3,
            g0,
            g1,
            g2,
            g3,
            d0,
            d1,
            d2,
            d3,
            d4,
            d5,
            d6,
            d7,
            c0,
            c1,
            c2,
            c3,
        ] = bytes;
        let magic = u32::from_le_bytes([m0, m1, m2, m3]);
        let check = u32::from_le_bytes([c0, c1, c2, c3]);
        let covered = bytes.get(..16).unwrap_or_default();
        if magic != MAGIC || check != fold(covered) {
            return Ok(None);
        }
        Ok(Some(Area {
            generation: u32::from_le_bytes([g0, g1, g2, g3]),
            digest: u64::from_le_bytes([d0, d1, d2, d3, d4, d5, d6, d7]),
        }))
    }

    /// The area holding `digest`, if one does.
    fn find(&mut self, digest: u64) -> Result<Option<usize>, ItemFault<S::Error>> {
        for area in 0..2 {
            if self.header(area)?.is_some_and(|held| held.digest == digest) {
                return Ok(Some(area));
            }
        }
        Ok(None)
    }

    /// Walk `area`'s items, calling `visit(offset, index, len, whole)`
    /// for each; answers where the next item would go, or `None` when a
    /// damaged length ends the area.
    fn walk(
        &mut self,
        area: usize,
        digest: u64,
        mut visit: impl FnMut(u32, u16, u16, bool),
    ) -> Result<Option<u32>, ItemFault<S::Error>> {
        let base = self.area_base(area);
        let end = base + self.area_bytes();
        let mut at = base + HEADER_BYTES;
        while at + ITEM_HEADER_BYTES <= end {
            let head: [u8; ITEM_HEADER_BYTES as usize] = self.read_word(at)?;
            let [i0, i1, l0, l1, c0, c1, c2, c3] = head;
            if u32::from_le_bytes([i0, i1, l0, l1]) == ERASED {
                return Ok(Some(at));
            }
            let index = u16::from_le_bytes([i0, i1]);
            let len = u16::from_le_bytes([l0, l1]);
            if usize::from(len) > ITEM_MAX {
                return Ok(None);
            }
            let mut body = [0u8; ITEM_MAX];
            let Some(body) = body.get_mut(..usize::from(len)) else {
                return Ok(None);
            };
            let padded = pad(u32::from(len));
            if at + ITEM_HEADER_BYTES + padded > end {
                return Ok(None);
            }
            self.read_padded(at + ITEM_HEADER_BYTES, body)?;
            let whole = u32::from_le_bytes([c0, c1, c2, c3]) == item_check(digest, index, body);
            visit(at, index, len, whole);
            at += ITEM_HEADER_BYTES + padded;
        }
        Ok(None)
    }

    /// Read `dst.len()` bytes from `offset`, in whole read units.
    fn read_padded(&mut self, offset: u32, dst: &mut [u8]) -> Result<(), ItemFault<S::Error>> {
        let mut unit = [0u8; 4];
        for (chunk, from) in dst.chunks_mut(4).zip((offset..).step_by(4)) {
            self.storage
                .read(from, &mut unit)
                .map_err(ItemFault::Media)?;
            chunk.copy_from_slice(unit.get(..chunk.len()).unwrap_or_default());
        }
        Ok(())
    }

    /// Store `item` as `(digest, index)`.
    ///
    /// # Errors
    ///
    /// [`ItemFault::Full`] when the item is over [`ITEM_MAX`] or its area
    /// has no room; [`ItemFault::Media`] when the media refused.
    pub fn store(
        &mut self,
        digest: u64,
        index: u16,
        item: &[u8],
    ) -> Result<(), ItemFault<S::Error>> {
        let len = u16::try_from(item.len()).map_err(|_| ItemFault::Full)?;
        if item.len() > ITEM_MAX {
            return Err(ItemFault::Full);
        }
        let area = match self.find(digest)? {
            Some(area) => area,
            None => self.open(digest)?,
        };
        let Some(at) = self.walk(area, digest, |_, _, _, _| {})? else {
            return Err(ItemFault::Full);
        };
        let padded = pad(u32::from(len));
        if at + ITEM_HEADER_BYTES + padded > self.area_base(area) + self.area_bytes() {
            return Err(ItemFault::Full);
        }
        let mut head = [0u8; ITEM_HEADER_BYTES as usize];
        let check = item_check(digest, index, item);
        for (into, from) in head.iter_mut().zip(
            index
                .to_le_bytes()
                .into_iter()
                .chain(len.to_le_bytes())
                .chain(check.to_le_bytes()),
        ) {
            *into = from;
        }
        // The body first, then the header that makes it visible: a reset
        // between the two leaves erased bytes where the header goes, and
        // the next store reuses the slot only after the area is erased.
        let mut padded_body = [0xFF_u8; ITEM_MAX + 3];
        let body = padded_body
            .get_mut(..padded as usize)
            .ok_or(ItemFault::Full)?;
        body.get_mut(..item.len())
            .ok_or(ItemFault::Full)?
            .copy_from_slice(item);
        self.storage
            .program(at + ITEM_HEADER_BYTES, body)
            .and_then(|()| self.storage.barrier())
            .and_then(|()| self.storage.program(at, &head))
            .and_then(|()| self.storage.barrier())
            .map_err(ItemFault::Media)
    }

    /// Open an area for `digest`: the one holding the older generation
    /// (or no header at all) is erased and given the next generation.
    fn open(&mut self, digest: u64) -> Result<usize, ItemFault<S::Error>> {
        let (a, b) = (self.header(0)?, self.header(1)?);
        let (area, generation) = match (a, b) {
            (None, None) => (0, 1),
            (None, Some(held)) => (0, held.generation.wrapping_add(1)),
            (Some(held), None) => (1, held.generation.wrapping_add(1)),
            (Some(first), Some(second)) => {
                if first.generation <= second.generation {
                    (0, second.generation.wrapping_add(1))
                } else {
                    (1, first.generation.wrapping_add(1))
                }
            }
        };
        let base = self.area_base(area);
        let bytes = self.area_bytes();
        let mut header = [0u8; HEADER_BYTES as usize];
        for (into, from) in header.iter_mut().zip(
            MAGIC
                .to_le_bytes()
                .into_iter()
                .chain(generation.to_le_bytes())
                .chain(digest.to_le_bytes()),
        ) {
            *into = from;
        }
        let check = fold(header.get(..16).unwrap_or_default());
        if let Some(tail) = header.get_mut(16..) {
            tail.copy_from_slice(&check.to_le_bytes());
        }
        self.storage
            .erase(base, bytes)
            .and_then(|()| self.storage.barrier())
            .and_then(|()| self.storage.program(base, &header))
            .and_then(|()| self.storage.barrier())
            .map_err(ItemFault::Media)?;
        Ok(area)
    }

    /// Load the last whole copy of `(digest, index)` into `out`.
    ///
    /// # Errors
    ///
    /// [`ItemFault::Missing`] when there is none, [`ItemFault::Full`]
    /// when `out` is short, [`ItemFault::Media`] when the media refused.
    pub fn load(
        &mut self,
        digest: u64,
        index: u16,
        out: &mut [u8],
    ) -> Result<usize, ItemFault<S::Error>> {
        let Some(area) = self.find(digest)? else {
            return Err(ItemFault::Missing);
        };
        let mut found = None;
        self.walk(area, digest, |at, held, len, whole| {
            if whole && held == index {
                found = Some((at, len));
            }
        })?;
        let (at, len) = found.ok_or(ItemFault::Missing)?;
        let into = out.get_mut(..usize::from(len)).ok_or(ItemFault::Full)?;
        self.read_padded(at + ITEM_HEADER_BYTES, into)?;
        Ok(usize::from(len))
    }
}

/// `len` rounded up to the 4-byte program unit.
const fn pad(len: u32) -> u32 {
    (len + 3) & !3
}

/// FNV-1a-32 over `bytes`.
fn fold(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// An item's check: binds the bytes to their digest and index.
fn item_check(digest: u64, index: u16, bytes: &[u8]) -> u32 {
    let mut key = [0u8; 10];
    for (into, from) in key
        .iter_mut()
        .zip(digest.to_le_bytes().into_iter().chain(index.to_le_bytes()))
    {
        *into = from;
    }
    let mut hash = fold(&key);
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use core::cell::RefCell;

    use waymaker_flash::storage::Geometry;

    use super::*;
    use crate::flash::Partition;
    use crate::ram::{Cut, RamNor};

    /// Two areas of two erase blocks each.
    const BYTES: usize = 4 * 4096;

    fn geometry() -> Geometry {
        Geometry::new(4 * 4096, 4096, 4, 4).expect("a legal geometry")
    }

    #[test]
    fn an_item_reads_back_as_stored_and_the_last_copy_wins() {
        let chip = RefCell::new(RamNor::<BYTES>::new());
        let mut log = ItemLog::new(Partition::new(&chip, 0, geometry()).expect("fits"));
        log.store(7, 0, b"first").expect("stored");
        log.store(7, 1, b"second item").expect("stored");
        log.store(7, 0, b"FIRST")
            .expect("a redelivered store appends");
        let mut out = [0u8; ITEM_MAX];
        assert_eq!(log.load(7, 0, &mut out), Ok(5));
        assert_eq!(&out[..5], b"FIRST");
        assert_eq!(log.load(7, 1, &mut out), Ok(11));
        assert_eq!(&out[..11], b"second item");
        assert_eq!(log.load(7, 2, &mut out), Err(ItemFault::Missing));
        assert_eq!(log.load(8, 0, &mut out), Err(ItemFault::Missing));
    }

    #[test]
    fn a_new_digest_keeps_the_previous_one_and_a_third_retires_the_oldest() {
        let chip = RefCell::new(RamNor::<BYTES>::new());
        let mut log = ItemLog::new(Partition::new(&chip, 0, geometry()).expect("fits"));
        let mut out = [0u8; ITEM_MAX];
        log.store(1, 0, b"one").expect("stored");
        log.store(2, 0, b"two").expect("stored");
        assert_eq!(
            log.load(1, 0, &mut out),
            Ok(3),
            "the previous checkpoint survives"
        );
        log.store(3, 0, b"three").expect("stored");
        assert_eq!(
            log.load(1, 0, &mut out),
            Err(ItemFault::Missing),
            "the oldest is retired"
        );
        assert_eq!(log.load(2, 0, &mut out), Ok(3));
        assert_eq!(log.load(3, 0, &mut out), Ok(5));
    }

    #[test]
    fn an_item_over_a_page_and_an_area_out_of_room_are_refused() {
        let chip = RefCell::new(RamNor::<BYTES>::new());
        let mut log = ItemLog::new(Partition::new(&chip, 0, geometry()).expect("fits"));
        assert_eq!(log.store(1, 0, &[0; ITEM_MAX + 1]), Err(ItemFault::Full));
        let item = [0x5A; ITEM_MAX];
        let mut stored = 0;
        while log.store(1, stored, &item).is_ok() {
            stored += 1;
        }
        // Two blocks hold 16 whole pages after the area header, leaving
        // 364 bytes: a small item still fits, a 400-byte one does not.
        assert_eq!(stored, 16);
        assert_eq!(log.store(1, stored, &[0; 400]), Err(ItemFault::Full));
        assert_eq!(log.store(1, stored, b"x"), Ok(()));
    }

    #[test]
    fn a_torn_item_is_never_read_and_its_redelivery_lands_whole() {
        for cut in [Cut::Before, Cut::Torn] {
            let chip = RefCell::new(RamNor::<BYTES>::new());
            let mut log = ItemLog::new(Partition::new(&chip, 0, geometry()).expect("fits"));
            log.store(9, 0, b"kept").expect("stored");
            // The next item's body, then its header: cut each.
            for at in 1..=2 {
                chip.borrow_mut().cut_after(at, cut);
                assert!(log.store(9, 1, b"lost in the cut").is_err());
                chip.borrow_mut().power_on();
                let mut out = [0u8; ITEM_MAX];
                assert_eq!(log.load(9, 0, &mut out), Ok(4), "{cut:?} at {at}");
                // The redelivered store lands whole.
                log.store(9, 1, b"lost in the cut").expect("stored");
                assert_eq!(log.load(9, 1, &mut out), Ok(15), "{cut:?} at {at}");
            }
        }
    }
}
