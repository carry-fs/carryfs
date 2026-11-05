// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! CarryFS: a native Linux driver for CarryFS volumes.
//!
//! This revision mounts volumes read-write. The on-disk encodings, checksums,
//! and filename profile come from `ondisk/`, which the userspace tools build
//! as the `carryfs-ondisk` crate; the module compiles the same files by path,
//! as XFS shares `libxfs` with xfsprogs.

use kernel::prelude::*;

#[path = "../ondisk/src"]
#[allow(dead_code, unreachable_pub, missing_docs, clippy::all)]
mod ondisk {
    pub mod crc;
    pub mod error;
    pub mod format;
    pub mod name;
    mod name_tables;
}

mod commit;
mod check;
mod dir;
mod edit;
mod file;
mod glue;
mod inode;
mod namei;
mod names;
mod reserve;
mod space;
mod tree;
mod vfs;
mod volume;
mod write;

module! {
    type: CarryFs,
    name: "carryfs",
    authors: ["CarryFS contributors"],
    description: "CarryFS cross-platform copy-on-write filesystem",
    license: "GPL",
}

struct CarryFs;

impl kernel::Module for CarryFs {
    fn init(_module: &'static ThisModule) -> Result<Self> {
        if ondisk::crc::crc32c(b"123456789") != 0xE306_9283 {
            pr_err!("kernel CRC32C does not match the CarryFS reference vector\n");
            return Err(EINVAL);
        }
        names::init();
        // SAFETY: Called once at module load; `Drop` unregisters.
        kernel::error::to_result(unsafe { glue::carryfs_register() })?;
        Ok(CarryFs)
    }
}

impl Drop for CarryFs {
    fn drop(&mut self) {
        // SAFETY: `init` registered the filesystem type.
        unsafe { glue::carryfs_unregister() };
    }
}

/// The kernel error for a failed encoding check. Corruption is
/// `EFSCORRUPTED`, which Linux spells `EUCLEAN`.
pub(crate) fn errno(error: ondisk::error::Error) -> Error {
    use ondisk::error::Error as E;
    let code = match error {
        E::Corrupt(_) => kernel::bindings::EUCLEAN,
        E::UnsupportedFormat => kernel::bindings::EOPNOTSUPP,
        E::TooLong(_) => kernel::bindings::ENAMETOOLONG,
        E::InvalidGeometry | E::InvalidInput(_) | E::InvalidName(_) => kernel::bindings::EINVAL,
    };
    Error::from_errno(-(code as i32))
}

/// The kernel error for an errno the `kernel` crate does not name.
pub(crate) fn code(errno: u32) -> Error {
    Error::from_errno(-(errno as i32))
}

/// Log corruption found at `block` and return `EFSCORRUPTED`.
pub(crate) fn corrupt(block: u64, error: ondisk::error::Error) -> Error {
    if let ondisk::error::Error::Corrupt(reason) = error {
        pr_err!("corrupt metadata at block {block}: {reason}\n");
    }
    errno(error)
}
