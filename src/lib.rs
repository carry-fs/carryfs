//! CarryFS image storage, structural checking, and offline
//! directory and checksummed file operations, scrub, and extraction.
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
mod name;
mod name_tables;
mod tree;
mod volume;
mod workspace;

pub use check::CheckSummary;
pub use device::{BlockDevice, Geometry, ImageDevice, MountMode};
pub use error::{Error, Result};
pub use name::Name;
pub use volume::{
    Access, Attributes, DirectoryCursor, DirectoryEntry, DirectoryPage, ExtractionSink,
    ExtractionSummary, Finding, Handle, LeavesRead, ObjectId, ObjectKind, ROOT, ReadAhead,
    RemoveKind, Replace, Space, Timestamp, Volume, VolumeInfo, VolumeLabel, has_root_magic,
};

#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzzing;
