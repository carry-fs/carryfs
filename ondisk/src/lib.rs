//! CarryFS on-disk encodings: CRC32C, root and page records, and filename
//! profile 2. FORMAT.md specifies the format.
//!
//! The crate allocates nothing and needs only `core`, so the Linux kernel
//! module compiles these same files. The kernel module includes the modules
//! below by path, so they refer to each other through `super::`, never
//! `crate::`.

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(missing_docs)]

pub mod crc;
pub mod error;
pub mod format;
pub mod name;
mod name_tables;
