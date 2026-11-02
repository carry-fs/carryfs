// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Namespace changes: create, mkdir, symlink, unlink, rmdir, rename, and
//! timestamp updates (FORMAT.md section 5).
//!
//! Each change edits the open transaction under the volume lock. Checks run
//! before the first edit, so a refused change leaves no trace; an edit that
//! fails part-way faults the volume.

use kernel::bindings;
use kernel::ffi::c_int;
use kernel::prelude::*;

use crate::commit::{self, COMMIT_DRAFTS};
use crate::edit::{Disk, Txn};
use crate::glue::{self, qstr_bytes};
use crate::ondisk::format::{TreeKind, MAINTENANCE_RESERVE, MAX_ID};
use crate::tree::{self, le_u64, Tree};
use crate::volume::{self, State};
use crate::{errno, inode, names};

/// Kinds of the catalog's inode records.
pub(crate) const FILE: u8 = 1;
pub(crate) const DIRECTORY: u8 = 2;
pub(crate) const SYMLINK: u8 = 3;
/// Inode flag bit 0: the object is unlinked but pinned.
const ORPHAN: u8 = 1;
/// Free blocks a foreground change must leave: the maintenance reserve
/// plus the largest publication.
const HEADROOM: u64 = MAINTENANCE_RESERVE + 4_096;

/// Inode record field offsets.
const PARENT: usize = 8;
const SIZE: usize = 16;
pub(crate) const ALLOCATED: usize = 24;
const CHILDREN: usize = 32;
const CREATED: usize = 40;
const MODIFIED: usize = 56;
const CHANGED: usize = 72;

pub(crate) fn set_u64(record: &mut [u8], offset: usize, value: u64) {
    record[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

/// The current time, clamped to the format's timestamp range.
pub(crate) fn now() -> (i64, u32) {
    let mut ts = bindings::timespec64 {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: Fills the caller's timespec.
    unsafe { bindings::ktime_get_real_ts64(&mut ts) };
    (
        ts.tv_sec.clamp(-11_644_473_600, 253_402_300_799),
        ts.tv_nsec as u32,
    )
}

pub(crate) fn set_time(record: &mut [u8], offset: usize, (seconds, nanos): (i64, u32)) {
    record[offset..offset + 8].copy_from_slice(&seconds.to_le_bytes());
    record[offset + 8..offset + 12].copy_from_slice(&nanos.to_le_bytes());
    record[offset + 12..offset + 16].fill(0);
}

/// The key of `object`'s orphan marker in the cleanup tree.
fn orphan_key(object: u64) -> [u8; 9] {
    let mut key = [3; 9];
    key[1..].copy_from_slice(&object.to_be_bytes());
    key
}

/// Note a change to `dir`'s entries, under its exclusive inode lock: bump
/// its version, which invalidates negative dentries and listings being
/// built, and drop its kept listing.
fn changed(dir: *mut bindings::inode) {
    // SAFETY: The caller holds `dir` locked exclusively.
    unsafe { glue::carryfs_inode_inc_iversion(dir) };
    crate::dir::forget(dir);
}

/// Run `f` on the open transaction of a writable volume, opening one, and
/// committing a large one first. Faults the volume if `f` fails after its
/// first edit.
pub(crate) fn mutate<T>(
    sb: *mut bindings::super_block,
    f: impl FnOnce(&mut State, &Disk) -> Result<T>,
) -> Result<T> {
    let volume = volume::of(sb);
    if !volume.writable {
        return Err(EROFS);
    }
    let mut state = volume.lock();
    if state.faulted {
        return Err(EIO);
    }
    if state
        .txn
        .as_ref()
        .is_some_and(|txn| txn.drafts.len() >= COMMIT_DRAFTS)
    {
        commit::commit(sb, &mut state)?;
    }
    if state.txn.is_none() {
        let txn = Txn::new(&state.root, state.hint)?;
        state.txn = Some(txn);
    }
    let disk = Disk {
        sb,
        uuid: state.root.uuid,
        blocks: state.root.blocks,
        generation: state.root.generation,
    };
    let before = state.txn.as_ref().map_or(0, |txn| txn.edits);
    let result = f(&mut state, &disk);
    if result.is_err() && state.txn.as_ref().map_or(0, |txn| txn.edits) != before {
        pr_err!("a change failed part-way; the volume is read-only until remounted\n");
        state.faulted = true;
    }
    result
}

/// The open transaction; `mutate` opened it.
pub(crate) fn txn(state: &mut State) -> &mut Txn {
    state.txn.as_mut().expect("mutate opens a transaction")
}

/// `object`'s inode record, as the open transaction sees it.
pub(crate) fn read_record(
    sb: *mut bindings::super_block,
    state: &State,
    object: u64,
) -> Result<KVec<u8>> {
    let key = tree::prefix(object, tree::INODE);
    let mut record = KVec::new();
    Tree::catalog(sb, state)
        .get(&key, |value| {
            Ok(record.extend_from_slice(value, GFP_KERNEL)?)
        })?
        .ok_or(ENOENT)?;
    Ok(record)
}

/// Store `object`'s inode record.
pub(crate) fn write_record(state: &mut State, disk: &Disk, object: u64, record: &[u8]) -> Result {
    txn(state).put(
        disk,
        TreeKind::Catalog,
        &tree::prefix(object, tree::INODE),
        record,
    )
}

/// A directory entry: its target and type.
struct Entry {
    target: u64,
    kind: u8,
}

fn read_entry(sb: *mut bindings::super_block, state: &State, key: &[u8]) -> Result<Option<Entry>> {
    Tree::catalog(sb, state).get(key, |value| {
        Ok(Entry {
            target: le_u64(value, 0),
            kind: value.get(8).copied().unwrap_or(0),
        })
    })
}

fn entry_value(target: u64, kind: u8, stored: &[u8]) -> Result<KVec<u8>> {
    let mut value = KVec::with_capacity(12 + stored.len(), GFP_KERNEL)?;
    value.extend_from_slice(&target.to_le_bytes(), GFP_KERNEL)?;
    value.extend_from_slice(&[kind, 0], GFP_KERNEL)?;
    value.extend_from_slice(&(stored.len() as u16).to_le_bytes(), GFP_KERNEL)?;
    value.extend_from_slice(stored, GFP_KERNEL)?;
    Ok(value)
}

/// Add `delta` children to directory `dir` and stamp its times.
fn touch_parent(
    sb: *mut bindings::super_block,
    state: &mut State,
    disk: &Disk,
    dir: u64,
    delta: i64,
    time: (i64, u32),
) -> Result {
    let mut record = read_record(sb, state, dir)?;
    let children = le_u64(&record, CHILDREN).saturating_add_signed(delta);
    set_u64(&mut record, CHILDREN, children);
    set_time(&mut record, MODIFIED, time);
    set_time(&mut record, CHANGED, time);
    write_record(state, disk, dir, &record)
}

/// Mark `object` unlinked: set its orphan flag and add its marker. Its
/// eviction removes it.
fn orphan(
    sb: *mut bindings::super_block,
    state: &mut State,
    disk: &Disk,
    object: u64,
    time: (i64, u32),
) -> Result {
    let mut record = read_record(sb, state, object)?;
    record[1] |= ORPHAN;
    set_time(&mut record, CHANGED, time);
    write_record(state, disk, object, &record)?;
    txn(state).put(disk, TreeKind::Cleanup, &orphan_key(object), &[])
}

/// The dentry's name parsed: stored spelling and comparison key.
fn parse_name(dentry: *mut bindings::dentry) -> Result<(KVec<u8>, KVec<u8>)> {
    // SAFETY: The VFS passes a referenced dentry.
    let name = unsafe { qstr_bytes(glue::carryfs_dentry_name(dentry)) };
    names::parse(name).map_err(errno)
}

/// Create an object of `kind` named by `dentry` in `dir`; its new inode.
fn create_object(
    dir: *mut bindings::inode,
    dentry: *mut bindings::dentry,
    kind: u8,
    target: &[u8],
) -> Result<*mut bindings::inode> {
    let (stored, folded) = parse_name(dentry)?;
    // SAFETY: The VFS holds `dir` locked.
    let (sb, parent) = unsafe { ((*dir).i_sb, (*dir).i_ino as u64) };
    let mut key = KVec::with_capacity(9 + folded.len(), GFP_KERNEL)?;
    key.extend_from_slice(&tree::prefix(parent, tree::ENTRY), GFP_KERNEL)?;
    key.extend_from_slice(&folded, GFP_KERNEL)?;
    let object = mutate(sb, |state, disk| {
        if read_entry(sb, state, &key)?.is_some() {
            return Err(EEXIST);
        }
        if state.free_blocks() < HEADROOM {
            return Err(ENOSPC);
        }
        let object = txn(state).next_object;
        if object >= MAX_ID - 1 {
            return Err(ENOSPC);
        }
        let time = now();
        let mut record = KVec::with_capacity(128 + target.len(), GFP_KERNEL)?;
        record.resize(128, 0, GFP_KERNEL)?;
        record[0] = kind;
        set_u64(&mut record, PARENT, parent);
        set_u64(&mut record, SIZE, target.len() as u64);
        set_time(&mut record, CREATED, time);
        set_time(&mut record, MODIFIED, time);
        set_time(&mut record, CHANGED, time);
        record.extend_from_slice(target, GFP_KERNEL)?;
        let value = entry_value(object, kind, &stored)?;
        txn(state).next_object = object + 1;
        write_record(state, disk, object, &record)?;
        txn(state).put(disk, TreeKind::Catalog, &key, &value)?;
        touch_parent(sb, state, disk, parent, 1, time)?;
        Ok(object)
    })?;
    changed(dir);
    let inode = inode::get(sb, object)?;
    // SAFETY: Both are referenced; the times changed.
    unsafe { inode::refresh_times(dir) };
    Ok(inode)
}

unsafe extern "C" fn create(
    _idmap: *mut bindings::mnt_idmap,
    dir: *mut bindings::inode,
    dentry: *mut bindings::dentry,
    _mode: bindings::umode_t,
    _excl: bool,
) -> c_int {
    match create_object(dir, dentry, FILE, &[]) {
        // SAFETY: `d_instantiate` takes the new inode's reference.
        Ok(inode) => unsafe {
            bindings::d_instantiate(dentry, inode);
            0
        },
        Err(error) => error.to_errno(),
    }
}

unsafe extern "C" fn mkdir(
    _idmap: *mut bindings::mnt_idmap,
    dir: *mut bindings::inode,
    dentry: *mut bindings::dentry,
    _mode: bindings::umode_t,
) -> *mut bindings::dentry {
    match create_object(dir, dentry, DIRECTORY, &[]) {
        // SAFETY: As in `create`.
        Ok(inode) => unsafe {
            bindings::d_instantiate(dentry, inode);
            core::ptr::null_mut()
        },
        Err(error) => error.to_ptr(),
    }
}

unsafe extern "C" fn symlink(
    _idmap: *mut bindings::mnt_idmap,
    dir: *mut bindings::inode,
    dentry: *mut bindings::dentry,
    target: *const kernel::ffi::c_char,
) -> c_int {
    // SAFETY: The VFS passes a NUL-terminated target.
    let target = unsafe { core::ffi::CStr::from_ptr(target.cast()) }.to_bytes();
    if let Err(error) = crate::ondisk::format::validate_symlink_target(
        target,
        crate::ondisk::format::Validation::Input,
    ) {
        return errno(error).to_errno();
    }
    match create_object(dir, dentry, SYMLINK, target) {
        // SAFETY: As in `create`.
        Ok(inode) => unsafe {
            bindings::d_instantiate(dentry, inode);
            0
        },
        Err(error) => error.to_errno(),
    }
}

/// Remove the entry `dentry` names in `dir`, which must name a directory
/// exactly when `directory` is set.
fn remove_entry(
    dir: *mut bindings::inode,
    dentry: *mut bindings::dentry,
    directory: bool,
) -> Result {
    let (_, folded) = parse_name(dentry)?;
    // SAFETY: The VFS holds `dir` and the dentry's inode locked.
    let (sb, parent, victim) = unsafe { ((*dir).i_sb, (*dir).i_ino as u64, (*dentry).d_inode) };
    let mut key = KVec::with_capacity(9 + folded.len(), GFP_KERNEL)?;
    key.extend_from_slice(&tree::prefix(parent, tree::ENTRY), GFP_KERNEL)?;
    key.extend_from_slice(&folded, GFP_KERNEL)?;
    mutate(sb, |state, disk| {
        let entry = read_entry(sb, state, &key)?.ok_or(ENOENT)?;
        match (entry.kind == DIRECTORY, directory) {
            (true, false) => return Err(EISDIR),
            (false, true) => return Err(ENOTDIR),
            _ => {}
        }
        if directory && le_u64(&read_record(sb, state, entry.target)?, CHILDREN) != 0 {
            return Err(crate::code(bindings::ENOTEMPTY));
        }
        let time = now();
        txn(state).delete(disk, TreeKind::Catalog, &key)?;
        touch_parent(sb, state, disk, parent, -1, time)?;
        orphan(sb, state, disk, entry.target, time)
    })?;
    changed(dir);
    // SAFETY: The VFS holds both inodes; the entry is gone.
    unsafe {
        bindings::clear_nlink(victim);
        inode::refresh_times(dir);
    }
    Ok(())
}

unsafe extern "C" fn unlink(dir: *mut bindings::inode, dentry: *mut bindings::dentry) -> c_int {
    match remove_entry(dir, dentry, false) {
        Ok(()) => 0,
        Err(error) => error.to_errno(),
    }
}

unsafe extern "C" fn rmdir(dir: *mut bindings::inode, dentry: *mut bindings::dentry) -> c_int {
    match remove_entry(dir, dentry, true) {
        Ok(()) => 0,
        Err(error) => error.to_errno(),
    }
}

/// `rename(2)` flags the format supports.
const RENAME_NOREPLACE: u32 = 1;

unsafe extern "C" fn rename(
    _idmap: *mut bindings::mnt_idmap,
    old_dir: *mut bindings::inode,
    old_dentry: *mut bindings::dentry,
    new_dir: *mut bindings::inode,
    new_dentry: *mut bindings::dentry,
    flags: u32,
) -> c_int {
    if flags & !RENAME_NOREPLACE != 0 {
        return EINVAL.to_errno();
    }
    // SAFETY: The VFS holds both directories and both dentries.
    match unsafe { rename_entry(old_dir, old_dentry, new_dir, new_dentry, flags) } {
        Ok(()) => 0,
        Err(error) => error.to_errno(),
    }
}

/// # Safety
///
/// The arguments must be the VFS's locked rename operands.
unsafe fn rename_entry(
    old_dir: *mut bindings::inode,
    old_dentry: *mut bindings::dentry,
    new_dir: *mut bindings::inode,
    new_dentry: *mut bindings::dentry,
    flags: u32,
) -> Result {
    let (_, old_folded) = parse_name(old_dentry)?;
    let (stored, new_folded) = parse_name(new_dentry)?;
    // SAFETY: The caller guarantees these are valid and locked.
    let (sb, from, to, replaced) = unsafe {
        (
            (*old_dir).i_sb,
            (*old_dir).i_ino as u64,
            (*new_dir).i_ino as u64,
            (*new_dentry).d_inode,
        )
    };
    let mut old_key = KVec::new();
    old_key.extend_from_slice(&tree::prefix(from, tree::ENTRY), GFP_KERNEL)?;
    old_key.extend_from_slice(&old_folded, GFP_KERNEL)?;
    let mut new_key = KVec::new();
    new_key.extend_from_slice(&tree::prefix(to, tree::ENTRY), GFP_KERNEL)?;
    new_key.extend_from_slice(&new_folded, GFP_KERNEL)?;
    let mut victim_unlinked = false;
    mutate(sb, |state, disk| {
        let source = read_entry(sb, state, &old_key)?.ok_or(ENOENT)?;
        let time = now();
        if old_key == new_key {
            // A case-only rename updates the stored spelling in place.
            let value = entry_value(source.target, source.kind, &stored)?;
            txn(state).put(disk, TreeKind::Catalog, &new_key, &value)?;
            return touch_parent(sb, state, disk, from, 0, time);
        }
        let destination = read_entry(sb, state, &new_key)?;
        if let Some(destination) = &destination {
            if flags & RENAME_NOREPLACE != 0 {
                return Err(EEXIST);
            }
            match (source.kind == DIRECTORY, destination.kind == DIRECTORY) {
                (true, false) => return Err(ENOTDIR),
                (false, true) => return Err(EISDIR),
                (true, true) => {
                    if le_u64(&read_record(sb, state, destination.target)?, CHILDREN) != 0 {
                        return Err(crate::code(bindings::ENOTEMPTY));
                    }
                }
                _ => {}
            }
        }
        let mut moved = read_record(sb, state, source.target)?;
        set_u64(&mut moved, PARENT, to);
        set_time(&mut moved, CHANGED, time);
        let value = entry_value(source.target, source.kind, &stored)?;
        txn(state).delete(disk, TreeKind::Catalog, &old_key)?;
        txn(state).put(disk, TreeKind::Catalog, &new_key, &value)?;
        write_record(state, disk, source.target, &moved)?;
        if let Some(destination) = destination {
            orphan(sb, state, disk, destination.target, time)?;
            victim_unlinked = true;
            touch_parent(sb, state, disk, to, 0, time)?;
        } else {
            touch_parent(sb, state, disk, to, 1, time)?;
        }
        touch_parent(sb, state, disk, from, -1, time)
    })?;
    // SAFETY: As above.
    changed(old_dir);
    changed(new_dir);
    // SAFETY: As above.
    unsafe {
        if victim_unlinked && !replaced.is_null() {
            bindings::clear_nlink(replaced);
        }
        inode::refresh_times(old_dir);
        inode::refresh_times(new_dir);
    }
    Ok(())
}

/// Remove an evicted, unlinked object's records, retiring its data.
/// Called for each batch until it reports the object gone.
pub(crate) fn remove_object(sb: *mut bindings::super_block, object: u64) -> Result {
    loop {
        let done = mutate(sb, |state, disk| {
            // Collect a bounded batch of the object's keys, then delete them.
            let prefix = &tree::prefix(object, 0)[..8];
            let mut keys: KVec<KVec<u8>> = KVec::new();
            let mut extents: KVec<(u64, u64)> = KVec::new();
            Tree::catalog(sb, state).scan(prefix, &mut |key, value| {
                if !key.starts_with(prefix) || keys.len() >= 256 {
                    return Ok(false);
                }
                if key.get(8) == Some(&tree::EXTENT) {
                    extents.push(
                        (le_u64(value, 0), le_u64(value, 8) & u64::from(u32::MAX)),
                        GFP_KERNEL,
                    )?;
                }
                let mut copy = KVec::new();
                copy.extend_from_slice(key, GFP_KERNEL)?;
                keys.push(copy, GFP_KERNEL)?;
                Ok(true)
            })?;
            let done = keys.len() < 256;
            // The inode record goes last, with the marker.
            for key in keys.iter().filter(|key| key.get(8) != Some(&tree::INODE)) {
                txn(state).delete(disk, TreeKind::Catalog, key)?;
            }
            for (first, count) in extents.iter() {
                txn(state).retire(disk, *first, *count)?;
            }
            if done {
                txn(state).delete(disk, TreeKind::Catalog, &tree::prefix(object, tree::INODE))?;
                txn(state).delete(disk, TreeKind::Cleanup, &orphan_key(object))?;
            } else {
                // Keep the record's block count equal to its extents'.
                let mut record = read_record(sb, state, object)?;
                let removed: u64 = extents.iter().map(|(_, count)| count).sum();
                let allocated = le_u64(&record, ALLOCATED).saturating_sub(removed);
                set_u64(&mut record, ALLOCATED, allocated);
                write_record(state, disk, object, &record)?;
            }
            Ok(done)
        })?;
        if done {
            return Ok(());
        }
    }
}

/// Store new modification or change times from `setattr`.
pub(crate) fn set_times(
    sb: *mut bindings::super_block,
    object: u64,
    modified: Option<(i64, u32)>,
) -> Result {
    mutate(sb, |state, disk| {
        let mut record = read_record(sb, state, object)?;
        if let Some(time) = modified {
            set_time(&mut record, MODIFIED, time);
        }
        set_time(&mut record, CHANGED, now());
        write_record(state, disk, object, &record)
    })
}

pub(crate) unsafe extern "C" fn d_revalidate(
    dir: *mut bindings::inode,
    _name: *const bindings::qstr,
    dentry: *mut bindings::dentry,
    flags: u32,
) -> c_int {
    // SAFETY: The VFS passes a dentry and its parent's inode.
    unsafe { glue::carryfs_revalidate_negative(dir, dentry, flags) }
}

pub(crate) static DIRECTORY_INODE_OPERATIONS: inode::Ops<bindings::inode_operations> =
    inode::Ops(bindings::inode_operations {
        lookup: Some(inode::lookup),
        create: Some(create),
        mkdir: Some(mkdir),
        symlink: Some(symlink),
        unlink: Some(unlink),
        rmdir: Some(rmdir),
        rename: Some(rename),
        setattr: Some(inode::setattr),
        // SAFETY: All-zero entries are absent operations.
        ..unsafe { core::mem::zeroed() }
    });
