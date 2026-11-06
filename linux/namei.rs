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
use crate::glue;
use crate::ondisk::format::{TreeKind, MAINTENANCE_RESERVE, MAX_ID};
use crate::tree::{self, le_u64, Tree};
use crate::vfs::{Dentry, Inode};
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
fn changed(dir: &Inode) {
    dir.bump_version();
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
    edit(sb, &mut state, f)
}

/// Run `f` on the open transaction without committing first, as a data
/// write recording blocks it allocated must: the commit would wait for it.
/// Faults the volume if `f` fails after its first edit.
pub(crate) fn edit<T>(
    sb: *mut bindings::super_block,
    state: &mut State,
    f: impl FnOnce(&mut State, &Disk) -> Result<T>,
) -> Result<T> {
    if state.faulted || state.txn.is_none() {
        return Err(EIO);
    }
    let disk = Disk {
        sb,
        uuid: state.root.uuid,
        blocks: state.root.blocks,
        generation: state.root.generation,
    };
    let before = state.txn.as_ref().map_or(0, |txn| txn.edits);
    let result = f(state, &disk);
    if result.is_err() && state.txn.as_ref().map_or(0, |txn| txn.edits) != before {
        pr_err!("a change failed part-way; the volume is read-only until remounted\n");
        volume::fault(sb, state);
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
        .get(&key, |value| tree::append(&mut record, value))?
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
    dir: &Inode,
    delta: i64,
    time: (i64, u32),
) -> Result {
    let mut record = read_record(sb, state, dir.object())?;
    let children = le_u64(&record, CHILDREN).saturating_add_signed(delta);
    set_u64(&mut record, CHILDREN, children);
    set_time(&mut record, MODIFIED, time);
    set_time(&mut record, CHANGED, time);
    write_record(state, disk, dir.object(), &record)?;
    inode::refresh_times(dir, state)
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
fn parse_name(dentry: &Dentry) -> Result<(KVec<u8>, KVec<u8>)> {
    names::parse(dentry.name()).map_err(errno)
}

/// Create an object of `kind` named by `dentry` in `dir`; its new inode.
fn create_object(
    dir: &Inode,
    dentry: &Dentry,
    kind: u8,
    target: &[u8],
) -> Result<*mut bindings::inode> {
    let (stored, folded) = parse_name(dentry)?;
    let (sb, parent) = (dir.super_block().as_ptr(), dir.object());
    let mut key = KVec::with_capacity(9 + folded.len(), GFP_KERNEL)?;
    key.extend_from_slice(&tree::prefix(parent, tree::ENTRY), GFP_KERNEL)?;
    key.extend_from_slice(&folded, GFP_KERNEL)?;
    let object = mutate(sb, |state, disk| {
        if read_entry(sb, state, &key)?.is_some() {
            return Err(EEXIST);
        }
        if crate::reserve::available(state) < HEADROOM {
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
        touch_parent(sb, state, disk, dir, 1, time)?;
        Ok(object)
    })?;
    changed(dir);
    inode::get(sb, object)
}

unsafe extern "C" fn create(
    _idmap: *mut bindings::mnt_idmap,
    dir: *mut bindings::inode,
    dentry: *mut bindings::dentry,
    _mode: bindings::umode_t,
    _excl: bool,
) -> c_int {
    // SAFETY: The VFS holds `dir` locked and `dentry` referenced.
    let (dir_ref, dentry_ref) = unsafe { (Inode::from_raw(dir), Dentry::from_raw(dentry)) };
    match create_object(dir_ref, dentry_ref, FILE, &[]) {
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
    // SAFETY: As in `create`.
    let (dir_ref, dentry_ref) = unsafe { (Inode::from_raw(dir), Dentry::from_raw(dentry)) };
    match create_object(dir_ref, dentry_ref, DIRECTORY, &[]) {
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
    // SAFETY: As in `create`.
    let (dir_ref, dentry_ref) = unsafe { (Inode::from_raw(dir), Dentry::from_raw(dentry)) };
    match create_object(dir_ref, dentry_ref, SYMLINK, target) {
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
fn remove_entry(dir: &Inode, dentry: &Dentry, directory: bool) -> Result {
    let (_, folded) = parse_name(dentry)?;
    let (sb, parent) = (dir.super_block().as_ptr(), dir.object());
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
        touch_parent(sb, state, disk, dir, -1, time)?;
        orphan(sb, state, disk, entry.target, time)?;
        if let Some(victim) = dentry.inode() {
            inode::refresh_times(victim, state)?;
        }
        Ok(())
    })?;
    changed(dir);
    if let Some(victim) = dentry.inode() {
        victim.clear_nlink();
    }
    Ok(())
}

unsafe extern "C" fn unlink(dir: *mut bindings::inode, dentry: *mut bindings::dentry) -> c_int {
    // SAFETY: The VFS holds `dir` and the dentry's inode locked.
    let (dir, dentry) = unsafe { (Inode::from_raw(dir), Dentry::from_raw(dentry)) };
    match remove_entry(dir, dentry, false) {
        Ok(()) => 0,
        Err(error) => error.to_errno(),
    }
}

unsafe extern "C" fn rmdir(dir: *mut bindings::inode, dentry: *mut bindings::dentry) -> c_int {
    // SAFETY: As in `unlink`.
    let (dir, dentry) = unsafe { (Inode::from_raw(dir), Dentry::from_raw(dentry)) };
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
    // SAFETY: The VFS holds both directories and both dentries locked.
    let operands = unsafe {
        (
            Inode::from_raw(old_dir),
            Dentry::from_raw(old_dentry),
            Inode::from_raw(new_dir),
            Dentry::from_raw(new_dentry),
        )
    };
    match rename_entry(operands.0, operands.1, operands.2, operands.3, flags) {
        Ok(()) => 0,
        Err(error) => error.to_errno(),
    }
}

/// Rename `old_dentry` in `old_dir` to `new_dentry` in `new_dir`, all
/// locked by the VFS.
fn rename_entry(
    old_dir: &Inode,
    old_dentry: &Dentry,
    new_dir: &Inode,
    new_dentry: &Dentry,
    flags: u32,
) -> Result {
    let (_, old_folded) = parse_name(old_dentry)?;
    let (stored, new_folded) = parse_name(new_dentry)?;
    let (sb, from, to) = (
        old_dir.super_block().as_ptr(),
        old_dir.object(),
        new_dir.object(),
    );
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
            return touch_parent(sb, state, disk, old_dir, 0, time);
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
        if let Some(moved) = old_dentry.inode() {
            inode::refresh_times(moved, state)?;
        }
        if let Some(destination) = destination {
            orphan(sb, state, disk, destination.target, time)?;
            if let Some(replaced) = new_dentry.inode() {
                inode::refresh_times(replaced, state)?;
            }
            victim_unlinked = true;
            touch_parent(sb, state, disk, new_dir, 0, time)?;
        } else {
            touch_parent(sb, state, disk, new_dir, 1, time)?;
        }
        touch_parent(sb, state, disk, old_dir, -1, time)
    })?;
    changed(old_dir);
    changed(new_dir);
    if victim_unlinked {
        if let Some(replaced) = new_dentry.inode() {
            replaced.clear_nlink();
        }
    }
    Ok(())
}

/// Remove an evicted, unlinked object's records, retiring its data.
/// Called for each batch until it reports the object gone.
pub(crate) fn remove_object(sb: *mut bindings::super_block, object: u64) -> Result {
    loop {
        let done = mutate(sb, |state, disk| {
            let done = crate::write::remove_data_step(sb, state, disk, object)?;
            if done {
                txn(state).delete(disk, TreeKind::Catalog, &tree::prefix(object, tree::INODE))?;
                txn(state).delete(disk, TreeKind::Cleanup, &orphan_key(object))?;
            }
            Ok(done)
        })?;
        if done {
            return Ok(());
        }
    }
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
