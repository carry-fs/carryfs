//! CarryFS image storage, structural checking, and offline
//! directory and checksummed file operations, scrub, extraction, and offline repair.
//! FORMAT.md specifies the on-disk format.
//!
//! ```no_run
//! use carryfs_core::{ImageDevice, MountMode, Volume};
//! # fn main() -> carryfs_core::Result<()> {
//! let device = ImageDevice::open("backup.cfs", MountMode::ReadOnly)?;
//! let mut volume = Volume::open(device)?;
//! println!("generation {}", volume.info()?.generation);
//! volume.eject()?;
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs)]

mod allocation;
mod check;
mod device;
mod error;
mod format;
#[cfg(target_os = "freebsd")]
pub mod freebsd;
mod name;
#[cfg(target_os = "netbsd")]
pub mod netbsd;
mod tree;
mod volume;
mod workspace;

pub use carryfs_ondisk::format::{LEGACY_REVISION, REVISION};
pub use check::CheckSummary;

/// The product version every crate and app shares (VERSIONING.md).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What `--version` prints: the product version and the format revisions
/// this build writes and reads (VERSIONING.md).
pub fn version_line(program: &str) -> String {
    format!(
        "{program} {VERSION} (format revision {REVISION}; reads revisions {LEGACY_REVISION}-{REVISION})"
    )
}
pub use device::{BlockDevice, Geometry, ImageDevice, MountMode};
pub use error::{Error, Result};
pub use name::{Name, windows_symlink_target};
pub use volume::{
    Access, AttributeChange, Attributes, DirectoryCursor, DirectoryEntry, DirectoryPage,
    ExtractionSink, ExtractionSummary, Finding, Handle, ImageRecovery, LeavesRead, Mode,
    NewAttributes, ObjectId, ObjectKind, ROOT, ReadAhead, RemoveKind, Repair, RepairPlan, Replace,
    Space, Timestamp, Volume, VolumeInfo, VolumeLabel, erase_reserved_bands, has_root_magic,
};

#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzzing;
