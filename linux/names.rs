// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Case-insensitive names: comparison keys and dentry operations.
//!
//! Names match when their filename-profile-2 comparison keys match
//! (FORMAT.md section 7), so `d_hash` and `d_compare` use those keys rather
//! than the kernel's utf8 casefolding, which differs from the profile (NFD
//! output, other Unicode version). ASCII names, the common case, fold
//! without the shared scratch buffer.

use kernel::bindings;
use kernel::ffi::c_int;
use kernel::prelude::*;

use crate::glue::qstr_bytes;
use crate::inode::Ops;
use crate::ondisk::name::{self, Scratch, MAX_FOLDED_BYTES, MAX_STORED_BYTES};
use crate::tree;

/// Scratch for non-ASCII names and the first key of a comparison.
struct Fold {
    scratch: Scratch,
    first: [u8; MAX_FOLDED_BYTES],
}

kernel::sync::global_lock! {
    // SAFETY: `init` runs at module load, before any mount can use it.
    unsafe(uninit) static FOLD: SpinLock<Fold> = Fold {
        scratch: Scratch::new(),
        first: [0; MAX_FOLDED_BYTES],
    };
}

// one global lock serializes non-ASCII folding; switch to per-CPU
// scratch if Unicode-heavy lookups contend on it.

pub(crate) fn init() {
    // SAFETY: Called once from module init.
    unsafe { FOLD.init() };
}

pub(crate) fn validate_disk(stored: &[u8], folded: &[u8]) -> Result {
    name::validate_disk_name(stored, folded, &mut FOLD.lock().scratch).map_err(crate::errno)
}

/// The comparison key of `name`, written to `out`; `None` when the name is
/// not a valid stored name and so cannot exist on disk.
fn fold_into(name: &[u8], out: &mut KVec<u8>) -> Result<Option<()>> {
    let Ok(text) = core::str::from_utf8(name) else {
        return Ok(None);
    };
    // Capacity first: the copy below must not allocate under the lock.
    out.reserve(MAX_FOLDED_BYTES, GFP_KERNEL)?;
    if text.is_ascii() {
        let mut folded = [0; MAX_STORED_BYTES];
        let Some(buffer) = folded.get_mut(..text.len()) else {
            return Ok(None);
        };
        return Ok(match name::parse_ascii(text, buffer) {
            Ok(parsed) => Some(out.extend_from_slice(parsed.folded, GFP_NOWAIT)?),
            Err(_) => None,
        });
    }
    let mut fold = FOLD.lock();
    Ok(match name::parse_unicode(text, &mut fold.scratch) {
        Ok(parsed) => Some(out.extend_from_slice(parsed.folded, GFP_NOWAIT)?),
        Err(_) => None,
    })
}

/// `name`'s stored spelling and comparison key, for creating it.
pub(crate) fn parse(
    name: &[u8],
) -> core::result::Result<(KVec<u8>, KVec<u8>), crate::ondisk::error::Error> {
    use crate::ondisk::error::Error as E;
    let text = core::str::from_utf8(name).map_err(|_| E::InvalidName("name is not UTF-8"))?;
    let copy = |parsed: name::ParsedName<'_>| -> core::result::Result<(KVec<u8>, KVec<u8>), E> {
        let mut stored = KVec::with_capacity(parsed.stored.len(), GFP_NOWAIT)
            .map_err(|_| E::TooLong("out of memory"))?;
        stored
            .extend_from_slice(parsed.stored, GFP_NOWAIT)
            .map_err(|_| E::TooLong("out of memory"))?;
        let mut folded = KVec::with_capacity(parsed.folded.len(), GFP_NOWAIT)
            .map_err(|_| E::TooLong("out of memory"))?;
        folded
            .extend_from_slice(parsed.folded, GFP_NOWAIT)
            .map_err(|_| E::TooLong("out of memory"))?;
        Ok((stored, folded))
    };
    if text.is_ascii() {
        let mut folded = [0; MAX_STORED_BYTES];
        return copy(name::parse_ascii(text, &mut folded)?);
    }
    copy(name::parse_unicode(text, &mut FOLD.lock().scratch)?)
}

/// The catalog key of `name`'s directory entry in `dir`, or `None` when no
/// entry can have that name.
pub(crate) fn entry_key(dir: u64, name: &[u8]) -> Result<Option<KVec<u8>>> {
    let mut key = KVec::with_capacity(9 + MAX_FOLDED_BYTES, GFP_KERNEL)?;
    key.extend_from_slice(&tree::prefix(dir, tree::ENTRY), GFP_KERNEL)?;
    Ok(fold_into(name, &mut key)?.map(|()| key))
}

/// Calls `f` with the comparison key of `name`, or with `name` itself when
/// it cannot be stored, which then matches only identical bytes.
fn with_key<T>(name: &[u8], scratch: &mut Scratch, f: impl FnOnce(&[u8]) -> T) -> T {
    match core::str::from_utf8(name)
        .ok()
        .map(|text| name::parse_unicode(text, scratch))
    {
        Some(Ok(parsed)) => f(parsed.folded),
        _ => f(name),
    }
}

/// A dcache hash of a comparison key: FNV-1a seeded with the parent. Any
/// hash works if equal keys hash equally; this one streams, so an ASCII
/// name hashes without a lowercase copy.
fn hash(parent: *const bindings::dentry, key: impl Iterator<Item = u8>) -> u32 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ parent as usize as u64;
    for byte in key {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    (hash ^ hash >> 32) as u32
}

unsafe extern "C" fn d_hash(dentry: *const bindings::dentry, name: *mut bindings::qstr) -> c_int {
    // SAFETY: The VFS passes a valid name for the duration of the call.
    let bytes = unsafe { qstr_bytes(name) };
    let value = if bytes.is_ascii() && bytes.len() <= MAX_STORED_BYTES {
        // An ASCII name's comparison key is its lowercase form.
        hash(dentry, bytes.iter().map(u8::to_ascii_lowercase))
    } else {
        with_key(bytes, &mut FOLD.lock().scratch, |key| {
            hash(dentry, key.iter().copied())
        })
    };
    // SAFETY: `d_hash` may set the hash of the name it was given.
    unsafe { (*name).__bindgen_anon_1.__bindgen_anon_1.hash = value };
    0
}

unsafe extern "C" fn d_compare(
    _dentry: *const bindings::dentry,
    len: u32,
    stored: *const u8,
    name: *const bindings::qstr,
) -> c_int {
    // SAFETY: The VFS passes `len` readable bytes at `stored`, which RCU
    // keeps alive, and a valid `name`.
    let (first, second) = unsafe {
        (
            core::slice::from_raw_parts(stored, len as usize),
            qstr_bytes(name),
        )
    };
    let same = if first.is_ascii() && second.is_ascii() {
        first.eq_ignore_ascii_case(second)
    } else {
        let mut fold = FOLD.lock();
        let Fold {
            scratch,
            first: copy,
        } = &mut *fold;
        let len = with_key(first, scratch, |key| {
            let len = key.len().min(copy.len());
            copy[..len].copy_from_slice(&key[..len]);
            len
        });
        with_key(second, scratch, |key| key == &copy[..len])
    };
    c_int::from(!same)
}

pub(crate) static DENTRY_OPERATIONS: Ops<bindings::dentry_operations> =
    Ops(bindings::dentry_operations {
        d_revalidate: Some(crate::namei::d_revalidate),
        d_hash: Some(d_hash),
        d_compare: Some(d_compare),
        // SAFETY: All-zero entries are absent operations.
        ..unsafe { core::mem::zeroed() }
    });
