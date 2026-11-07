// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Regular-file reads.
//!
//! Verify each 64 KiB checksum unit before copying it into the page cache.
//! Readahead aligns to unit boundaries and overlaps batches; a workqueue
//! verifies completed reads and unlocks their folios. `small_read` uses the
//! buffer cache to read ahead across nearby small files. See FORMAT.md section 5.

use kernel::bindings;
use kernel::ffi::c_int;
use kernel::page::Page;
use kernel::prelude::*;
use kernel::sync::atomic::Relaxed;

use crate::glue;
use crate::inode::Ops;
use crate::ondisk::crc::{DataUnitCrc, UNIT_BYTES};
use crate::ondisk::error::Error as Encoding;
use crate::ondisk::format::{checksum_slot, BLOCK_BYTES};
use crate::tree::{self, le_u64, Tree};
use crate::vfs::{Folio, Inode};
use crate::volume::{self, State, Volume};
use crate::{corrupt, errno, namei};

pub(crate) const UNIT_BLOCKS: u64 = (UNIT_BYTES / BLOCK_BYTES) as u64;
const SECTORS_PER_BLOCK: u64 = (BLOCK_BYTES / 512) as u64;

/// Copy page `block` of a unit with `valid` bytes in `pages` into
/// `destination`, zero past the valid bytes.
fn copy_block(pages: &[Page], valid: usize, block: usize, destination: &mut [u8]) {
    let start = block * BLOCK_BYTES;
    let len = valid.saturating_sub(start).min(BLOCK_BYTES);
    if let Some(page) = pages.get(block).filter(|_| len > 0) {
        // SAFETY: The bounce page holds `BLOCK_BYTES` bytes.
        let source = unsafe {
            core::slice::from_raw_parts(glue::carryfs_page_address(page.as_ptr()).cast::<u8>(), len)
        };
        destination[..len].copy_from_slice(source);
    }
    destination[len..BLOCK_BYTES].fill(0);
}

/// A verified unit: its valid bytes in up to 16 pages, or a hole.
pub(crate) struct Unit {
    index: u64,
    pages: KVec<Page>,
    valid: usize,
}

impl Unit {
    /// Copy page `block` of this unit into `destination`, zero past the
    /// valid bytes; a hole reads as zeros.
    pub(crate) fn copy_into(&self, block: usize, destination: &mut [u8]) {
        copy_block(&self.pages, self.valid, block, destination);
    }

    /// Copy this unit's blocks into `folio`, which lies within it, zero past
    /// the valid bytes; a hole reads as zeros.
    fn copy_to(&self, folio: &Folio) {
        let first = (folio.first_block() % UNIT_BLOCKS) as usize;
        for index in 0..folio.blocks() {
            folio.with_block(index, |destination| self.copy_into(first + index, destination));
        }
    }
}

/// The file being read: its object, and its size in the page cache, which
/// bounds readahead.
struct Source<'a> {
    sb: *mut bindings::super_block,
    volume: &'a Volume,
    object: u64,
    size: u64,
}

impl<'a> Source<'a> {
    fn new(inode: &'a Inode) -> Self {
        let sb = inode.super_block();
        Self {
            sb: sb.as_ptr(),
            volume: sb.volume(),
            object: inode.object(),
            size: inode.size(),
        }
    }

    /// The stored checksum of `unit`, or `None` for a hole.
    fn checksum(&self, state: &State, unit: u64) -> Result<Option<u32>> {
        let key = tree::numbered_key(self.object, tree::CHECKSUMS, unit / 256);
        let slot = (unit % 256) as usize;
        Ok(Tree::catalog(self.sb, state)
            .get(&key, |group| checksum_slot(group, slot).map_err(errno))?
            .flatten())
    }

    /// The physical block of logical `block` and how many blocks after it
    /// stay contiguous in its extent.
    fn map(&self, state: &State, block: u64) -> Result<(u64, u64)> {
        let key = tree::numbered_key(self.object, tree::EXTENT, block);
        let found = Tree::catalog(self.sb, state).floor(&key, |found, value| {
            let owner = found.get(..9) == Some(&tree::prefix(self.object, tree::EXTENT)[..]);
            let start = found
                .get(9..17)
                .and_then(|bytes| bytes.try_into().ok())
                .map_or(0, u64::from_be_bytes);
            let count = le_u64(value, 8) & u64::from(u32::MAX);
            Ok((owner && block >= start && block - start < count)
                .then(|| (le_u64(value, 0) + (block - start), count - (block - start))))
        })?;
        found.flatten().ok_or_else(|| {
            corrupt(
                state.root.catalog.block,
                Encoding::Corrupt("backed checksum unit has unmapped blocks"),
            )
        })
    }

    /// Where `unit`'s blocks are, for a file whose stored size is `size`,
    /// with bounce pages to read them into. Called under the volume lock.
    fn locate(&self, state: &State, size: u64, unit: u64) -> Result<Located> {
        let start = unit * UNIT_BYTES as u64;
        let valid = size.saturating_sub(start).min(UNIT_BYTES as u64) as usize;
        let mut located = Located {
            index: unit,
            valid: 0,
            expected: 0,
            ok: false,
            runs: KVec::new(),
            pages: KVec::new(),
        };
        let expected = if valid == 0 {
            None
        } else {
            self.checksum(state, unit)?
        };
        let Some(expected) = expected else {
            return Ok(located);
        };
        let blocks = valid.div_ceil(BLOCK_BYTES);
        located.pages.reserve(blocks, GFP_KERNEL)?;
        for _ in 0..blocks {
            located
                .pages
                .push(Page::alloc_page(GFP_KERNEL)?, GFP_KERNEL)?;
        }
        let mut done = 0;
        while done < blocks {
            let (physical, run) = self.map(state, unit * UNIT_BLOCKS + done as u64)?;
            let run = (run as usize).min(blocks - done);
            located.runs.push((physical, run), GFP_KERNEL)?;
            done += run;
        }
        located.valid = valid;
        located.expected = expected;
        Ok(located)
    }

    /// The stored size of the file.
    fn stored_size(&self, state: &State) -> Result<u64> {
        Ok(le_u64(&namei::read_record(self.sb, state, self.object)?, 16))
    }

    /// Read and verify `unit`.
    ///
    /// Read size, checksum, and extents under one volume-lock hold. Use the
    /// stored size because the page cache may already be larger. Writable
    /// mounts hold the lock through I/O to prevent block reuse mid-read.
    fn read(&self, unit: u64) -> Result<Unit> {
        let state = self.volume.lock();
        let size = self.stored_size(&state)?;
        let located = self.locate(&state, size, unit)?;
        let uuid = state.root.uuid;
        let held = if self.volume.writable {
            Some(state)
        } else {
            drop(state);
            None
        };
        let mut bio = core::ptr::null_mut();
        located.queue(self.sb, &mut bio);
        if !bio.is_null() {
            // SAFETY: `bio` is the last of the chain, not yet submitted.
            let status = unsafe {
                let status = bindings::submit_bio_wait(bio);
                bindings::bio_put(bio);
                status
            };
            if status != 0 {
                pr_err!("read error in object {}\n", self.object);
                return Err(EIO);
            }
        }
        drop(held);
        located.verify(&uuid, self.object)
    }
}

/// A unit located for reading: its blocks and bounce pages, or a hole when
/// `valid` is 0.
struct Located {
    index: u64,
    valid: usize,
    expected: u32,
    /// Read and verified, for a readahead batch.
    ok: bool,
    runs: KVec<(u64, usize)>,
    pages: KVec<Page>,
}

impl Located {
    /// Queue reads of the unit's runs, one bio per run, chained after `bio`,
    /// which is submitted; `bio` becomes the new last, unsubmitted bio.
    fn queue(&self, sb: *mut bindings::super_block, bio: &mut *mut bindings::bio) {
        let mut done = 0;
        for (physical, run) in self.runs.iter().copied() {
            // SAFETY: `sb` holds its block device; GFP_NOIO bio allocation
            // does not fail.
            let next =
                unsafe { glue::carryfs_bio_alloc_read(sb, physical * SECTORS_PER_BLOCK, run as u32) };
            for page in &self.pages[done..done + run] {
                // SAFETY: The bio was sized for `run` pages, so each fits.
                let added =
                    unsafe { bindings::bio_add_page(next, page.as_ptr(), BLOCK_BYTES as u32, 0) };
                debug_assert_eq!(added, BLOCK_BYTES as i32);
            }
            if !bio.is_null() {
                // SAFETY: `next` completes only after `*bio`, submitted here.
                unsafe {
                    bindings::bio_chain(*bio, next);
                    bindings::submit_bio(*bio);
                }
            }
            *bio = next;
            done += run;
        }
    }

    /// Check the read unit against its stored checksum.
    fn verify(self, uuid: &[u8; 16], object: u64) -> Result<Unit> {
        self.check(uuid, object)?;
        Ok(Unit {
            index: self.index,
            pages: self.pages,
            valid: self.valid,
        })
    }

    fn check(&self, uuid: &[u8; 16], object: u64) -> Result {
        if self.valid > 0 {
            let mut crc = DataUnitCrc::new(uuid, object, self.index, self.valid).map_err(errno)?;
            for (index, page) in self.pages.iter().enumerate() {
                let len = (self.valid - index * BLOCK_BYTES).min(BLOCK_BYTES);
                // SAFETY: The page is ours and holds `BLOCK_BYTES` bytes.
                let bytes = unsafe {
                    core::slice::from_raw_parts(
                        glue::carryfs_page_address(page.as_ptr()).cast::<u8>(),
                        len,
                    )
                };
                crc.update(bytes);
            }
            if crc.finish().map_err(errno)? != self.expected {
                pr_err!(
                    "data checksum mismatch in object {object} unit {}\n",
                    self.index
                );
                return Err(EIO);
            }
        }
        Ok(())
    }
}

/// Units one readahead read submits together: 2 MiB.
const BATCH_UNITS: u64 = 32;

/// A readahead read in flight: its units and their locked folios, which
/// `carryfs_read_done` verifies and unlocks.
struct Batch {
    sb: *mut bindings::super_block,
    object: u64,
    uuid: [u8; 16],
    units: KVec<Located>,
    folios: KVec<*mut bindings::folio>,
}

impl Batch {
    /// Verify the units, fill and unlock the folios. `status` is the reads'
    /// errno; a counted read ends before the folios are unlocked, as an
    /// unmount waits only for them.
    fn finish(mut self, status: c_int, counted: bool) {
        if status != 0 && counted {
            pr_err!("read error in object {}\n", self.object);
        }
        for unit in self.units.iter_mut() {
            unit.ok = status == 0 && unit.check(&self.uuid, self.object).is_ok();
        }
        let unit_of = |folio: &Folio| {
            let block = folio.first_block();
            let unit = self
                .units
                .iter()
                .find(|unit| unit.ok && unit.index == block / UNIT_BLOCKS);
            (unit, (block % UNIT_BLOCKS) as usize)
        };
        for folio in self.folios.iter() {
            // SAFETY: The batch holds its folios locked.
            let folio = unsafe { Folio::from_raw(*folio) };
            if let (Some(unit), first) = unit_of(folio) {
                for index in 0..folio.blocks() {
                    folio.with_block(index, |bytes| {
                        copy_block(&unit.pages, unit.valid, first + index, bytes)
                    });
                }
            }
        }
        if counted {
            volume::of(self.sb).lock().end_read();
        }
        for folio in self.folios.iter() {
            // SAFETY: As above; `read_folio` retries and reports failures.
            let folio = unsafe { Folio::from_raw(*folio) };
            folio.end_read(unit_of(folio).0.is_some());
        }
    }
}

/// Completion of a batch submitted by `read_batch`, on the read workqueue.
#[no_mangle]
unsafe extern "C" fn carryfs_read_done(batch: *mut core::ffi::c_void, status: c_int) {
    // SAFETY: `read_batch` passed a batch it leaked for this call.
    let batch = unsafe { KBox::from_raw(batch.cast::<Batch>()) };
    KBox::into_inner(batch).finish(status, true);
}

/// Read the units of `folios`, locked readahead folios of one file within
/// `BATCH_UNITS` units, and unlock them once verified, without waiting.
fn read_batch(source: &Source<'_>, folios: KVec<*mut bindings::folio>) {
    let batch = Batch {
        sb: source.sb,
        object: source.object,
        uuid: [0; 16],
        units: KVec::new(),
        folios: KVec::new(),
    };
    let Ok(mut batch) = KBox::new(batch, GFP_KERNEL) else {
        for folio in folios.iter() {
            // SAFETY: Locked readahead folios; `read_folio` retries.
            unsafe { Folio::from_raw(*folio) }.end_read(false);
        }
        return;
    };
    batch.folios = folios;
    let located = (|| -> Result {
        let mut state = source.volume.lock();
        // Counted before locating, so no commit reclaims what is located.
        state.start_read();
        let found = (|| -> Result {
            let size = source.stored_size(&state)?;
            batch.uuid = state.root.uuid;
            let mut last = None;
            for position in 0..batch.folios.len() {
                // SAFETY: Readahead folios are locked page-cache folios.
                let folio = unsafe { Folio::from_raw(batch.folios[position]) };
                let unit = folio.pos() / UNIT_BYTES as u64;
                if last != Some(unit) {
                    batch.units.push(source.locate(&state, size, unit)?, GFP_KERNEL)?;
                    last = Some(unit);
                }
            }
            Ok(())
        })();
        if found.is_err() {
            state.end_read();
        }
        found
    })();
    if located.is_err() {
        KBox::into_inner(batch).finish(EIO.to_errno(), false);
        return;
    }
    if small_read(source, &mut batch).is_some() {
        KBox::into_inner(batch).finish(0, true);
        return;
    }
    let mut bio = core::ptr::null_mut();
    for unit in batch.units.iter() {
        unit.queue(source.sb, &mut bio);
    }
    if bio.is_null() {
        // Holes only.
        KBox::into_inner(batch).finish(0, true);
        return;
    }
    let raw = KBox::into_raw(batch);
    // SAFETY: `bio` is the last of the chain, not yet submitted; the batch
    // lives until `carryfs_read_done` takes it back.
    if unsafe { glue::carryfs_submit_read(bio, raw.cast()) } != 0 {
        // SAFETY: Not submitted: the batch is still ours.
        let batch = KBox::into_inner(unsafe { KBox::from_raw(raw) });
        // SAFETY: As above.
        let status = unsafe {
            let status = bindings::submit_bio_wait(bio);
            bindings::bio_put(bio);
            status
        };
        batch.finish(status, true);
    }
}

/// Largest readahead batch `small_read` takes: small files.
const SMALL_BLOCKS: usize = 16;
/// Blocks `small_read` reads ahead at a time: 256 KiB.
const CHUNK_BLOCKS: u64 = 64;

/// Read nearby small files through the buffer cache to merge device requests.
/// Start prefetch on the third consecutive nearby read, covering two aligned chunks.
/// `write::forget_cached` invalidates reused blocks before readers can find them.
///
/// Return `None` for other batches or cache-read failures; the caller falls
/// back to bios, which report I/O errors.
fn small_read(source: &Source<'_>, batch: &mut Batch) -> Option<()> {
    let blocks: usize = batch.units.iter().map(|unit| unit.pages.len()).sum();
    if blocks == 0 || blocks > SMALL_BLOCKS {
        return None;
    }
    let (first, _) = batch.units.iter().find_map(|unit| unit.runs.first().copied())?;
    let (last, run) = batch.units.iter().rev().find_map(|unit| unit.runs.last().copied())?;
    let volume = source.volume;
    let previous = volume.next_small.load(Relaxed);
    volume.next_small.store(last + run as u64, Relaxed);
    let follows = first.wrapping_sub(previous) < CHUNK_BLOCKS;
    let streak = if follows { volume.small_run.load(Relaxed).saturating_add(1) } else { 0 };
    volume.small_run.store(streak, Relaxed);
    // Two in a row: a random read follows on by chance often enough that
    // one would cost random reads 8% more device time.
    if streak < 2 {
        return None;
    }
    let sb = source.sb;
    // SAFETY: `sb` is mounted.
    let device = unsafe { glue::carryfs_bdev_bytes(sb) } / BLOCK_BYTES as u64;
    let from = first / CHUNK_BLOCKS * CHUNK_BLOCKS;
    let to = (from + 2 * CHUNK_BLOCKS).min(device);
    let mut ahead = KVec::with_capacity(2 * CHUNK_BLOCKS as usize, GFP_KERNEL).ok()?;
    for block in from..to {
        ahead.push(block, GFP_KERNEL).ok()?;
    }
    // SAFETY: `sb` is mounted; the blocks are inside the device.
    unsafe { glue::carryfs_readahead_blocks(sb, ahead.as_ptr(), ahead.len() as u32) };
    for unit in batch.units.iter() {
        let mut done = 0;
        for (physical, run) in unit.runs.iter().copied() {
            for offset in 0..run {
                let block = tree::Block::read(sb, physical + offset as u64).ok()?;
                // SAFETY: The bounce page holds `BLOCK_BYTES` bytes.
                unsafe {
                    core::slice::from_raw_parts_mut(
                        glue::carryfs_page_address(unit.pages[done + offset].as_ptr()).cast::<u8>(),
                        BLOCK_BYTES,
                    )
                }
                .copy_from_slice(block.data());
            }
            done += run;
        }
    }
    Some(())
}

/// Fill `folio` from `unit`, reading it first unless it is the one given.
fn fill(source: &Source<'_>, unit: &mut Option<Unit>, folio: &Folio) -> Result {
    let index = folio.first_block() / UNIT_BLOCKS;
    if unit.as_ref().is_none_or(|unit| unit.index != index) {
        *unit = None;
        *unit = Some(source.read(index)?);
    }
    if let Some(unit) = unit {
        unit.copy_to(folio);
    }
    Ok(())
}

unsafe extern "C" fn read_folio(_file: *mut bindings::file, folio: *mut bindings::folio) -> c_int {
    // SAFETY: The VFS passes a locked folio of a mapping of this filesystem.
    let folio = unsafe { Folio::from_raw(folio) };
    let result = fill(&Source::new(folio.inode()), &mut None, folio);
    folio.end_read(result.is_ok());
    match result {
        Ok(()) => 0,
        Err(error) => error.to_errno(),
    }
}

unsafe extern "C" fn readahead(rac: *mut bindings::readahead_control) {
    // A FUSE read-ahead runs in the daemon, clamped to full clock; this one
    // runs in the reader, and a cached reread right after inherits its clock.
    let _boost = glue::CallerBoost::new();
    // SAFETY: The VFS passes a readahead request on a mapping of this
    // filesystem, whose host stays referenced while it has pages.
    let inode = unsafe { Inode::from_raw(glue::carryfs_readahead_inode(rac)) };
    let source = Source::new(inode);
    // Widen the window to whole units, so no unit is read twice.
    // SAFETY: As above.
    let (start, len) = unsafe {
        (
            glue::carryfs_readahead_index(rac),
            glue::carryfs_readahead_count(rac),
        )
    };
    let unit_pages = UNIT_BLOCKS;
    let first = start / unit_pages * unit_pages;
    let end = (start + len)
        .div_ceil(unit_pages)
        .saturating_mul(unit_pages)
        .min(source.size.div_ceil(BLOCK_BYTES as u64));
    if end > first {
        // SAFETY: As above.
        unsafe {
            bindings::readahead_expand(
                rac,
                (first * BLOCK_BYTES as u64) as i64,
                ((end - first) * BLOCK_BYTES as u64) as usize,
            )
        };
    }
    // Read the window in batches of whole units, each verified and
    // unlocked by the read workqueue, so the next batch's I/O overlaps.
    let mut folios: KVec<*mut bindings::folio> = KVec::new();
    let mut first_unit = 0;
    loop {
        // SAFETY: Returns the next locked folio of the request, or null.
        let folio = unsafe { glue::carryfs_readahead_folio(rac) };
        let unit = (!folio.is_null())
            // SAFETY: Readahead folios are locked page-cache folios.
            .then(|| unsafe { Folio::from_raw(folio) }.pos() / UNIT_BYTES as u64);
        if !folios.is_empty() && unit.is_none_or(|unit| unit >= first_unit + BATCH_UNITS) {
            read_batch(&source, core::mem::take(&mut folios));
        }
        let Some(unit) = unit else {
            break;
        };
        if folios.is_empty() {
            first_unit = unit;
        }
        if folios.push(folio, GFP_KERNEL).is_err() {
            // SAFETY: As above; `read_folio` retries.
            unsafe { Folio::from_raw(folio) }.end_read(false);
        }
    }
}

/// Read and verify `unit` of `object`, a file of `size` bytes.
pub(crate) fn read_unit(
    sb: *mut bindings::super_block,
    object: u64,
    size: u64,
    unit: u64,
) -> Result<Unit> {
    Source {
        sb,
        volume: volume::of(sb),
        object,
        size,
    }
    .read(unit)
}

/// Fill a locked, not uptodate folio of `inode` from disk, for a partial
/// write into it.
pub(crate) fn fill_folio(inode: &Inode, folio: &Folio) -> Result {
    fill(&Source::new(inode), &mut None, folio)
}

pub(crate) static ADDRESS_SPACE_OPERATIONS: Ops<bindings::address_space_operations> =
    Ops(bindings::address_space_operations {
        read_folio: Some(read_folio),
        readahead: Some(readahead),
        writepages: Some(crate::write::writepages),
        dirty_folio: Some(bindings::filemap_dirty_folio),
        // Folios carry no private data, so memory compaction may move them.
        migrate_folio: Some(bindings::filemap_migrate_folio),
        write_begin: Some(crate::write::write_begin),
        write_end: Some(crate::write::write_end),
        // SAFETY: All-zero entries are absent operations.
        ..unsafe { core::mem::zeroed() }
    });

/// Open a regular file, accepting `O_DIRECT`: `read_iter` and `write_iter`
/// serve direct I/O through the page cache, since a direct write smaller
/// than a 64 KiB checksum unit would rewrite the unit anyway.
unsafe extern "C" fn open(inode: *mut bindings::inode, file: *mut bindings::file) -> c_int {
    // SAFETY: The VFS passes the inode and the file it is opening.
    unsafe {
        glue::carryfs_file_allow_direct(file);
        bindings::generic_file_open(inode, file)
    }
}

unsafe extern "C" fn read_iter(iocb: *mut bindings::kiocb, to: *mut bindings::iov_iter) -> isize {
    // SAFETY: The VFS passes a request on an open file; a direct read is
    // served from the page cache, which is coherent with it.
    unsafe {
        (*iocb).ki_flags &= !(bindings::IOCB_DIRECT as c_int);
        bindings::generic_file_read_iter(iocb, to)
    }
}

unsafe extern "C" fn write_iter(iocb: *mut bindings::kiocb, from: *mut bindings::iov_iter) -> isize {
    // SAFETY: The VFS passes a request on an open file of this filesystem.
    unsafe {
        let direct = (*iocb).ki_flags & bindings::IOCB_DIRECT as c_int != 0;
        (*iocb).ki_flags &= !(bindings::IOCB_DIRECT as c_int);
        let written = bindings::generic_file_write_iter(iocb, from);
        if direct && written > 0 {
            // A direct write is on the device when it returns, and leaves
            // no cached copy behind.
            let mapping = (*(*(*iocb).ki_filp).f_inode).i_mapping;
            let start = (*iocb).ki_pos - written as i64;
            let end = (*iocb).ki_pos - 1;
            let status = bindings::filemap_write_and_wait_range(mapping, start, end);
            if status != 0 {
                return status as isize;
            }
            bindings::invalidate_mapping_pages(
                mapping,
                (start / BLOCK_BYTES as i64) as _,
                (end / BLOCK_BYTES as i64) as _,
            );
        }
        written
    }
}

pub(crate) static FILE_OPERATIONS: Ops<bindings::file_operations> =
    Ops(bindings::file_operations {
        llseek: Some(bindings::generic_file_llseek),
        open: Some(open),
        release: Some(glue::carryfs_file_release),
        read_iter: Some(read_iter),
        write_iter: Some(write_iter),
        fsync: Some(crate::write::fsync),
        mmap_prepare: Some(glue::carryfs_file_mmap_prepare),
        splice_read: Some(bindings::filemap_splice_read),
        // copy_file_range and splice write through the page cache.
        splice_write: Some(bindings::iter_file_splice_write),
        // SAFETY: All-zero entries are absent operations.
        ..unsafe { core::mem::zeroed() }
    });
