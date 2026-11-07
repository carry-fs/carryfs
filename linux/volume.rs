// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Mounting: device geometry, root selection, and super block operations.

use core::ffi::c_void;
use core::ptr;

use kernel::bindings;
use kernel::ffi::c_int;
use kernel::prelude::*;

use crate::glue;
use crate::ondisk::format::{
    self, BLOCK_BYTES, MAINTENANCE_RESERVE, RESERVED_TOTAL_BLOCKS, Root, TreeKind,
};
use crate::tree::{Block, Tree};
use crate::{corrupt, errno, inode, names};

/// `f_type` reported by statfs: ASCII "CFSX".
pub(crate) const MAGIC: u64 = 0x4346_5358;
const ROOT_A: u64 = 256;
// `SB_*` are macros the bindings omit; their values match the stable
// `MS_RDONLY` and `MS_NOATIME` mount flags.
const SB_RDONLY: usize = 1;
const SB_NOATIME: usize = 1 << 10;
const MIN_DEVICE_BYTES: u64 = 256 << 20;
const MAX_DEVICE_BYTES: u64 = 32 << 40;
/// Timestamp range of FORMAT.md section 5: 1601 through 9999.
const TIME_MIN: i64 = -11_644_473_600;
const TIME_MAX: i64 = 253_402_300_799;

/// A mounted volume, in `s_fs_info`. Read-only mounts never change it.
pub(crate) struct Volume {
    pub(crate) root: Root,
    pub(crate) uid: bindings::kuid_t,
    pub(crate) gid: bindings::kgid_t,
}

/// The volume of a mounted super block.
pub(crate) fn of<'a>(sb: *mut bindings::super_block) -> &'a Volume {
    // SAFETY: `fill_super` sets `s_fs_info` before the super block is
    // reachable, and `carryfs_free_volume` frees it only after
    // `kill_block_super` has shut every user down.
    unsafe { &*((*sb).s_fs_info as *const Volume) }
}

/// Fill a new super block; called by `get_tree_bdev` through glue.c.
#[no_mangle]
unsafe extern "C" fn carryfs_fill_super(
    sb: *mut bindings::super_block,
    uid: u32,
    gid: u32,
    silent: c_int,
) -> c_int {
    // SAFETY: The VFS passes a new, locked super block on the claimed device.
    match unsafe { fill_super(sb, uid, gid, silent != 0) } {
        Ok(()) => 0,
        Err(error) => error.to_errno(),
    }
}

/// Free the volume after `kill_block_super`, whether or not mounting
/// finished; glue.c's `kill_sb` calls it.
#[no_mangle]
unsafe extern "C" fn carryfs_free_volume(volume: *mut c_void) {
    if !volume.is_null() {
        // SAFETY: `fill_super` made `volume` with `KBox::into_raw`, and the
        // super block is gone.
        drop(unsafe { KBox::from_raw(volume as *mut Volume) });
    }
}

fn geometry(sb: *mut bindings::super_block, silent: bool) -> Result<u64> {
    // SAFETY: `sb` holds an open block device.
    let (bytes, logical, physical) = unsafe {
        (
            glue::carryfs_bdev_bytes(sb),
            glue::carryfs_bdev_logical_block_size(sb),
            glue::carryfs_bdev_physical_block_size(sb),
        )
    };
    let sectors = matches!((logical, physical), (512, 512 | 4096) | (4096, 4096));
    if !(MIN_DEVICE_BYTES..=MAX_DEVICE_BYTES).contains(&bytes)
        || bytes % BLOCK_BYTES as u64 != 0
        || !sectors
    {
        if !silent {
            pr_err!("unsupported geometry: {bytes} bytes, {logical}/{physical}-byte sectors\n");
        }
        return Err(EINVAL);
    }
    Ok(bytes / BLOCK_BYTES as u64)
}

/// The decoded root in `slot`, or `None` when its checksum or contents are
/// invalid. A read error fails the mount: that slot may hold the newer root.
fn read_root(sb: *mut bindings::super_block, slot: u64, blocks: u64) -> Result<Option<Root>> {
    let block = Block::read(sb, slot)?;
    format::supported_root(Root::decode(block.data(), slot, blocks)).map_err(errno)
}

/// # Safety
///
/// `sb` must be a new super block that `get_tree_bdev` is filling.
unsafe fn fill_super(sb: *mut bindings::super_block, uid: u32, gid: u32, silent: bool) -> Result {
    // SAFETY: `sb` is a new super block on a block device.
    if unsafe { bindings::sb_set_blocksize(sb, BLOCK_BYTES as c_int) } == 0 {
        if !silent {
            pr_err!("device sectors are larger than {BLOCK_BYTES} bytes\n");
        }
        return Err(EINVAL);
    }
    let blocks = geometry(sb, silent)?;
    let a = read_root(sb, ROOT_A, blocks)?;
    let b = read_root(sb, blocks - 257, blocks)?;
    let root = *format::select_root(a.as_ref(), b.as_ref()).map_err(|error| {
        if !silent {
            if let crate::ondisk::error::Error::Corrupt(reason) = error {
                pr_err!("no usable root: {reason}\n");
            }
        }
        errno(error)
    })?;
    let volume = KBox::new(
        Volume {
            root,
            uid: bindings::kuid_t { val: uid },
            gid: bindings::kgid_t { val: gid },
        },
        GFP_KERNEL,
    )?;
    // SAFETY: `sb` is not yet visible to anyone else.
    unsafe {
        (*sb).s_fs_info = KBox::into_raw(volume) as *mut c_void;
        (*sb).s_magic = MAGIC as _;
        (*sb).s_op = &SUPER_OPERATIONS;
        (*sb).s_maxbytes = i64::MAX;
        (*sb).s_time_gran = 1;
        (*sb).s_time_min = TIME_MIN;
        (*sb).s_time_max = TIME_MAX;
        (*sb).s_flags |= SB_RDONLY | SB_NOATIME;
        glue::carryfs_set_uuid(sb, root.uuid.as_ptr());
        bindings::set_default_d_op(sb, &names::DENTRY_OPERATIONS.0);
    }
    let volume = of(sb);
    for (kind, reference) in [
        (TreeKind::Catalog, root.catalog),
        (TreeKind::Allocation, root.allocation),
        (TreeKind::Cleanup, root.cleanup),
    ] {
        Tree::new(sb, volume, kind, reference).check_root()?;
    }

    let root_inode = inode::get(sb, inode::ROOT)?;
    // SAFETY: `get` returned a referenced inode.
    if unsafe { (*root_inode).i_mode } as u32 & bindings::S_IFMT != bindings::S_IFDIR {
        // SAFETY: Drop the reference `get` took.
        unsafe { bindings::iput(root_inode) };
        return Err(corrupt(root.catalog.block, crate::ondisk::error::Error::Corrupt("root is not a directory")));
    }
    // SAFETY: `d_make_root` consumes the inode reference, even on failure.
    let dentry = unsafe { bindings::d_make_root(root_inode) };
    if dentry.is_null() {
        return Err(ENOMEM);
    }
    // SAFETY: `sb` is still private to this mount.
    unsafe { (*sb).s_root = dentry };
    Ok(())
}

unsafe extern "C" fn statfs(dentry: *mut bindings::dentry, buf: *mut bindings::kstatfs) -> c_int {
    // SAFETY: The VFS passes a live dentry of this filesystem.
    let root = of(unsafe { (*dentry).d_sb }).root;
    let fsid = root.uuid;
    let word = |offset: usize| {
        u32::from_le_bytes([fsid[offset], fsid[offset + 1], fsid[offset + 2], fsid[offset + 3]])
    };
    // SAFETY: `buf` is the caller's statfs buffer.
    unsafe {
        glue::carryfs_statfs_fill(
            buf,
            MAGIC,
            BLOCK_BYTES as u64,
            root.blocks.saturating_sub(RESERVED_TOTAL_BLOCKS),
            root.free_blocks,
            root.free_blocks.saturating_sub(MAINTENANCE_RESERVE),
            255,
            word(0) ^ word(8),
            word(4) ^ word(12),
        )
    };
    0
}

unsafe extern "C" fn show_options(seq: *mut bindings::seq_file, root: *mut bindings::dentry) -> c_int {
    // SAFETY: The VFS passes the mount's root dentry.
    let volume = of(unsafe { (*root).d_sb });
    // SAFETY: Translating IDs into the initial namespace is always valid, and
    // the format string takes two unsigned ints.
    unsafe {
        let uid = bindings::from_kuid_munged(ptr::addr_of_mut!(bindings::init_user_ns), volume.uid);
        let gid = bindings::from_kgid_munged(ptr::addr_of_mut!(bindings::init_user_ns), volume.gid);
        bindings::seq_printf(seq, c",uid=%u,gid=%u".as_ptr().cast(), uid, gid);
    }
    0
}

static SUPER_OPERATIONS: bindings::super_operations = bindings::super_operations {
    free_inode: Some(inode::free_inode),
    statfs: Some(statfs),
    show_options: Some(show_options),
    // SAFETY: All-zero is a valid `super_operations`: every other entry is
    // an absent operation.
    ..unsafe { core::mem::zeroed() }
};
