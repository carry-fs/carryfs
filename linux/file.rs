// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Regular-file reads.
//!
//! Data is checksummed per 64 KiB unit (FORMAT.md section 5), so a unit is
//! read and verified whole before any of its bytes reach the page cache.
//! Readahead windows are widened to unit boundaries, so sequential reads
//! fetch each unit once.

use kernel::bindings;
use kernel::ffi::c_int;
use kernel::page::Page;
use kernel::prelude::*;

use crate::glue;
use crate::inode::Ops;
use crate::ondisk::crc::{DataUnitCrc, UNIT_BYTES};
use crate::ondisk::error::Error as Encoding;
use crate::ondisk::format::{BLOCK_BYTES, checksum_slot};
use crate::tree::{self, Tree, le_u64};
use crate::volume::{self, Volume};
use crate::{corrupt, errno};

pub(crate) const UNIT_BLOCKS: u64 = (UNIT_BYTES / BLOCK_BYTES) as u64;
const SECTORS_PER_BLOCK: u64 = (BLOCK_BYTES / 512) as u64;

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
        let start = block * BLOCK_BYTES;
        let len = self.valid.saturating_sub(start).min(BLOCK_BYTES);
        if let Some(page) = self.pages.get(block).filter(|_| len > 0) {
            // SAFETY: The bounce page holds `BLOCK_BYTES` bytes.
            let source = unsafe {
                core::slice::from_raw_parts(glue::carryfs_page_address(page.as_ptr()).cast::<u8>(), len)
            };
            destination[..len].copy_from_slice(source);
        }
        destination[len..BLOCK_BYTES].fill(0);
    }

    /// Copy page `block` of this unit into `folio`, zero past the valid
    /// bytes; a hole reads as zeros.
    fn copy_to(&self, block: usize, folio: *mut bindings::folio) {
        let start = block * BLOCK_BYTES;
        let len = self.valid.saturating_sub(start).min(BLOCK_BYTES);
        // SAFETY: A locked order-0 folio maps one page; the bounce page holds
        // `BLOCK_BYTES` bytes.
        unsafe {
            let destination = glue::carryfs_kmap_local_folio(folio, 0).cast::<u8>();
            if let Some(page) = self.pages.get(block).filter(|_| len > 0) {
                let source = glue::carryfs_page_address(page.as_ptr()).cast::<u8>();
                core::ptr::copy_nonoverlapping(source, destination, len);
            }
            core::ptr::write_bytes(destination.add(len), 0, BLOCK_BYTES - len);
            glue::carryfs_kunmap_local(destination.cast());
        }
    }
}

/// The file being read: its object and size.
struct Source<'a> {
    sb: *mut bindings::super_block,
    volume: &'a Volume,
    object: u64,
    size: u64,
}

impl Source<'_> {
    /// # Safety
    ///
    /// `inode` must be a referenced regular-file inode of this filesystem.
    unsafe fn new(inode: *mut bindings::inode) -> Self {
        // SAFETY: The caller guarantees the inode is valid; a read-only
        // inode's size never changes.
        unsafe {
            let sb = (*inode).i_sb;
            Self {
                sb,
                volume: volume::of(sb),
                object: (*inode).i_ino as u64,
                size: (*inode).i_size as u64,
            }
        }
    }

    /// The stored checksum of `unit`, or `None` for a hole.
    fn checksum(&self, unit: u64) -> Result<Option<u32>> {
        let key = tree::numbered_key(self.object, tree::CHECKSUMS, unit / 256);
        let slot = (unit % 256) as usize;
        let state = self.volume.lock();
        Ok(Tree::catalog(self.sb, &state)
            .get(&key, |group| checksum_slot(group, slot).map_err(errno))?
            .flatten())
    }

    /// The physical block of logical `block` and how many blocks after it
    /// stay contiguous in its extent.
    fn map(&self, block: u64) -> Result<(u64, u64)> {
        let key = tree::numbered_key(self.object, tree::EXTENT, block);
        let state = self.volume.lock();
        let catalog = state.root.catalog.block;
        let found = Tree::catalog(self.sb, &state).floor(&key, |found, value| {
            let owner = found.get(..9) == Some(&tree::prefix(self.object, tree::EXTENT)[..]);
            let start = found
                .get(9..17)
                .and_then(|bytes| bytes.try_into().ok())
                .map_or(0, u64::from_be_bytes);
            let count = le_u64(value, 8) & u64::from(u32::MAX);
            Ok((owner && block >= start && block - start < count).then(|| {
                (le_u64(value, 0) + (block - start), count - (block - start))
            }))
        })?;
        found.flatten().ok_or_else(|| {
            corrupt(
                catalog,
                Encoding::Corrupt("backed checksum unit has unmapped blocks"),
            )
        })
    }

    /// Read and verify `unit`.
    fn read(&self, unit: u64) -> Result<Unit> {
        let start = unit * UNIT_BYTES as u64;
        let valid = self.size.saturating_sub(start).min(UNIT_BYTES as u64) as usize;
        let mut pages = KVec::new();
        let Some(expected) = (if valid == 0 { None } else { self.checksum(unit)? }) else {
            return Ok(Unit {
                index: unit,
                pages,
                valid: 0,
            });
        };
        let blocks = valid.div_ceil(BLOCK_BYTES);
        pages.reserve(blocks, GFP_KERNEL)?;
        for _ in 0..blocks {
            pages.push(Page::alloc_page(GFP_KERNEL)?, GFP_KERNEL)?;
        }
        self.read_blocks(unit * UNIT_BLOCKS, &pages)?;

        let uuid = self.volume.lock().root.uuid;
        let mut crc = DataUnitCrc::new(&uuid, self.object, unit, valid)
            .map_err(errno)?;
        for (index, page) in pages.iter().enumerate() {
            let len = (valid - index * BLOCK_BYTES).min(BLOCK_BYTES);
            // SAFETY: The page is ours and holds `BLOCK_BYTES` bytes.
            let bytes = unsafe {
                core::slice::from_raw_parts(glue::carryfs_page_address(page.as_ptr()).cast::<u8>(), len)
            };
            crc.update(bytes);
        }
        if crc.finish().map_err(errno)? != expected {
            pr_err!(
                "data checksum mismatch in object {} unit {unit}\n",
                self.object
            );
            return Err(EIO);
        }
        Ok(Unit {
            index: unit,
            pages,
            valid,
        })
    }

    /// Read logical blocks from `first` into `pages`, one bio per
    /// contiguous run, chained so that one wait covers them all.
    fn read_blocks(&self, first: u64, pages: &[Page]) -> Result {
        let mut bio: *mut bindings::bio = core::ptr::null_mut();
        let mut done = 0;
        while done < pages.len() {
            let (physical, run) = self.map(first + done as u64)?;
            let run = (run as usize).min(pages.len() - done);
            // SAFETY: `sb` holds its block device; GFP_NOIO bio allocation
            // does not fail.
            let next = unsafe {
                glue::carryfs_bio_alloc_read(self.sb, physical * SECTORS_PER_BLOCK, run as u32)
            };
            for page in &pages[done..done + run] {
                // SAFETY: The bio was sized for `run` pages, so each fits.
                let added =
                    unsafe { bindings::bio_add_page(next, page.as_ptr(), BLOCK_BYTES as u32, 0) };
                debug_assert_eq!(added, BLOCK_BYTES as i32);
            }
            bio = if bio.is_null() {
                next
            } else {
                // SAFETY: `next` completes only after `bio`, which is
                // submitted here.
                unsafe {
                    bindings::bio_chain(bio, next);
                    bindings::submit_bio(bio);
                }
                next
            };
            done += run;
        }
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
        Ok(())
    }
}

/// The page index of a locked folio.
fn folio_block(folio: *mut bindings::folio) -> u64 {
    // SAFETY: The caller passes a locked page-cache folio.
    unsafe { glue::carryfs_folio_pos(folio) as u64 / BLOCK_BYTES as u64 }
}

/// Fill `folio` from `unit`, reading it first unless it is the one given.
fn fill(source: &Source<'_>, unit: &mut Option<Unit>, folio: *mut bindings::folio) -> Result {
    let block = folio_block(folio);
    let index = block / UNIT_BLOCKS;
    if unit.as_ref().is_none_or(|unit| unit.index != index) {
        *unit = None;
        *unit = Some(source.read(index)?);
    }
    if let Some(unit) = unit {
        unit.copy_to((block % UNIT_BLOCKS) as usize, folio);
    }
    Ok(())
}

unsafe extern "C" fn read_folio(_file: *mut bindings::file, folio: *mut bindings::folio) -> c_int {
    // SAFETY: The VFS passes a locked folio of a mapping of this filesystem.
    let source = unsafe { Source::new(glue::carryfs_folio_inode(folio)) };
    let result = fill(&source, &mut None, folio);
    // SAFETY: Unlocks the folio, marking it uptodate on success.
    unsafe { bindings::folio_end_read(folio, result.is_ok()) };
    match result {
        Ok(()) => 0,
        Err(error) => error.to_errno(),
    }
}

unsafe extern "C" fn readahead(rac: *mut bindings::readahead_control) {
    // SAFETY: The VFS passes a readahead request on a mapping of this
    // filesystem.
    let source = unsafe { Source::new(glue::carryfs_readahead_inode(rac)) };
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
    let mut unit = None;
    let mut failed = false;
    loop {
        // SAFETY: Returns the next locked folio of the request, or null.
        let folio = unsafe { glue::carryfs_readahead_folio(rac) };
        if folio.is_null() {
            break;
        }
        // After a failure, leave the rest to `read_folio`, which reports it.
        let ok = !failed && fill(&source, &mut unit, folio).is_ok();
        failed |= !ok;
        // SAFETY: Unlocks the folio, marking it uptodate on success.
        unsafe { bindings::folio_end_read(folio, ok) };
    }
}

/// Read and verify `unit` of `object`, a file of `size` bytes.
pub(crate) fn read_unit(sb: *mut bindings::super_block, object: u64, size: u64, unit: u64) -> Result<Unit> {
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
pub(crate) fn fill_folio(inode: *mut bindings::inode, folio: *mut bindings::folio) -> Result {
    // SAFETY: The VFS holds the inode and the folio.
    let source = unsafe { Source::new(inode) };
    fill(&source, &mut None, folio)
}

pub(crate) static ADDRESS_SPACE_OPERATIONS: Ops<bindings::address_space_operations> =
    Ops(bindings::address_space_operations {
        read_folio: Some(read_folio),
        readahead: Some(readahead),
        writepages: Some(crate::write::writepages),
        dirty_folio: Some(bindings::filemap_dirty_folio),
        write_begin: Some(crate::write::write_begin),
        write_end: Some(crate::write::write_end),
        // SAFETY: All-zero entries are absent operations.
        ..unsafe { core::mem::zeroed() }
    });

pub(crate) static FILE_OPERATIONS: Ops<bindings::file_operations> =
    Ops(bindings::file_operations {
        llseek: Some(bindings::generic_file_llseek),
        read_iter: Some(bindings::generic_file_read_iter),
        write_iter: Some(bindings::generic_file_write_iter),
        fsync: Some(crate::write::fsync),
        mmap_prepare: Some(bindings::generic_file_mmap_prepare),
        splice_read: Some(bindings::filemap_splice_read),
        // SAFETY: All-zero entries are absent operations.
        ..unsafe { core::mem::zeroed() }
    });
