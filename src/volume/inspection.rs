//! Read-only inspection and recovery. Disk traversal never creates transactions.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};

use crate::format::{TreeKind, checksum_slot};
use crate::tree::{Forest, u64_be};
use crate::{BlockDevice, CheckSummary, Error, MountMode, Name, Result};

use super::file::{UnitError, group_key, inspect_unit};
use super::namespace::{Inode, inode_key};
use super::{Attributes, DirectoryCursor, DirectoryEntry, Handle, ObjectId, ObjectKind, Volume};

const UNIT_BYTES: u64 = 65_536;

/// One streamed inspection failure. References are valid during the callback.
#[derive(Debug)]
pub struct Finding<'a> {
    /// Object being inspected, when known.
    pub object: Option<ObjectId>,
    /// Relative extraction path; empty for the selected object or non-extraction checks.
    pub path: &'a [Name],
    /// First unreadable byte in a file, or None for a structural failure.
    pub offset: Option<u64>,
    /// Unreadable byte count; zero when no data range can be identified.
    pub length: u64,
    /// Original failure, including its block or operating-system cause when available.
    pub error: &'a Error,
}

/// Recovery totals. Missing ranges are reported separately from genuine sparse holes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtractionSummary {
    /// Objects created at the destination, including partial files.
    pub objects: u64,
    /// Files whose backed-unit enumeration finished, including files with damaged units.
    pub files: u64,
    /// Bytes copied after checksum verification; excludes holes and unreadable ranges.
    pub verified_bytes: u64,
    /// Data units that could not be read or verified.
    pub unreadable_ranges: u64,
    /// Whether enumeration finished. True does not imply every data unit was readable.
    pub complete: bool,
}

/// Destination owner for recovery. The empty path names the selected output object.
///
/// Implementations must reject existing entries and source aliases, follow no
/// destination symlinks, and hold directory handles to prevent path replacement
/// from redirecting writes. Returned files must be new, writable, and seekable.
/// The caller, which knows the source identity, is responsible for these checks.
/// No timestamp restoration or destination durability is implied by extraction.
pub trait ExtractionSink {
    /// Create a new directory at validated relative name components.
    fn create_directory(&mut self, path: &[Name], attributes: &Attributes) -> Result<()>;
    /// Create a new regular file at validated relative name components.
    fn create_file(&mut self, path: &[Name], attributes: &Attributes) -> Result<File>;
    /// Create a new symlink to the stored `target` bytes, without resolving it.
    fn create_symlink(
        &mut self,
        path: &[Name],
        target: &[u8],
        attributes: &Attributes,
    ) -> Result<()>;
}

#[derive(Default)]
struct UnitCounts {
    verified: u64,
    bytes: u64,
    unreadable: u64,
}

struct FileScan<'a> {
    path: &'a [Name],
    output: Option<&'a mut File>,
    report: &'a mut dyn FnMut(Finding<'_>),
    counts: &'a mut UnitCounts,
    reuse_cache: bool,
}

// Only source failures become incomplete recovery. Destination failures abort.
enum ExtractionError {
    Source(Error),
    Destination(Error),
}
impl From<Error> for ExtractionError {
    fn from(error: Error) -> Self {
        Self::Source(error)
    }
}
impl ExtractionError {
    fn into_error(self) -> Error {
        match self {
            Self::Source(error) | Self::Destination(error) => error,
        }
    }
}

fn metadata_finding(
    report: &mut dyn FnMut(Finding<'_>),
    object: Option<ObjectId>,
    path: &[Name],
    error: &Error,
) {
    report(Finding {
        object,
        path,
        offset: None,
        length: 0,
        error,
    });
}

struct Directory {
    object: ObjectId,
    handle: Handle,
    cursor: Option<DirectoryCursor>,
    entries: std::vec::IntoIter<DirectoryEntry>,
}

impl<D: BlockDevice> Volume<D> {
    fn inspection_mode(&self) -> Result<()> {
        self.readable()?;
        if self.device.as_ref().ok_or(Error::Ejected)?.mode() != MountMode::ReadOnly {
            return Err(Error::InvalidInput(
                "inspection requires a read-only device",
            ));
        }
        Ok(())
    }

    /// Run the structural check, reporting the first error before returning it.
    /// Requires read-only access. No file data is read and no writes are issued.
    pub fn check_with_report(
        &mut self,
        report: &mut dyn FnMut(Finding<'_>),
    ) -> Result<CheckSummary> {
        self.inspection_mode()?;
        self.check().inspect_err(|error| {
            report(Finding {
                object: None,
                path: &[],
                offset: None,
                length: 0,
                error,
            });
        })
    }

    /// Check structure, then verify all catalog file data, including orphan files.
    ///
    /// Structural errors stop the scan and return Err. Data errors are reported
    /// and counted while scanning continues; inspect `unreadable_ranges` even
    /// when this returns Ok. Detached retired data and sparse holes are skipped.
    ///
    /// The structural check reads every metadata page. The data pass then walks
    /// the catalog in key order and keeps pages it has verified in the bounded
    /// tree cache for the length of the run, since a fresh descent per file
    /// re-read every sibling of every branch on the path. The device
    /// is read-only and held exclusively, so a cached page cannot change under
    /// the scan.
    pub fn scrub(&mut self, report: &mut dyn FnMut(Finding<'_>)) -> Result<CheckSummary> {
        self.cache = crate::tree::Cache::default();
        let summary = self.check_with_report(report)?;
        self.scrubbing = true;
        let result = self.scrub_data(summary, report);
        self.scrubbing = false;
        self.cache = crate::tree::Cache::default();
        result
    }

    fn scrub_data(
        &mut self,
        mut summary: CheckSummary,
        report: &mut dyn FnMut(Finding<'_>),
    ) -> Result<CheckSummary> {
        let mut start = inode_key(1);
        loop {
            let record = self
                .read_catalog(|forest, catalog| {
                    let Some((key, value)) =
                        forest.scan(&catalog, TreeKind::Catalog, &start, 1)?.pop()
                    else {
                        return Ok(None);
                    };
                    if key.len() != 9 || key[8] != 1 {
                        return Err(Error::Corrupt("catalog object has no inode"));
                    }
                    Ok(Some((ObjectId(u64_be(&key[..8])?), Inode::decode(&value)?)))
                })
                .inspect_err(|error| metadata_finding(report, None, &[], error))?;
            let Some((object, inode)) = record else { break };
            if inode.kind == ObjectKind::File {
                let mut counts = UnitCounts::default();
                self.inspect_file(
                    object,
                    inode.size,
                    FileScan {
                        path: &[],
                        output: None,
                        report,
                        counts: &mut counts,
                        reuse_cache: true,
                    },
                )
                .map_err(ExtractionError::into_error)
                .inspect_err(|error| metadata_finding(report, Some(object), &[], error))?;
                summary.inspected_files += 1;
                summary.verified_units += counts.verified;
                summary.unreadable_ranges += counts.unreadable;
            }
            let Some(next) = object.0.checked_add(1) else {
                break;
            };
            start = inode_key(next);
        }
        Ok(summary)
    }

    // Enumerate checksum records, not logical file offsets, so a multi-terabyte
    // hole takes no reads. A fresh forest per checksum group bounds cached work
    // and its read budget, while the group's 256 units share one catalog reader.
    fn inspect_file(
        &mut self,
        object: ObjectId,
        size: u64,
        mut scan: FileScan<'_>,
    ) -> std::result::Result<(), ExtractionError> {
        let mut start = group_key(object.0, 0);
        loop {
            let read = |forest: &mut Forest<'_, D>, catalog| {
                let Some((key, value)) = forest.scan(&catalog, TreeKind::Catalog, &start, 1)?.pop()
                else {
                    return Ok(None);
                };
                if key.len() != 17 || key[..9] != start[..9] {
                    return Ok(None);
                }
                let group = u64_be(&key[9..])?;
                // Extents ahead of the units of this group, carried between them.
                // The walk covers the whole file, so they have no end.
                let mut held = Vec::new();
                let mut destination = Ok(());
                for slot in 0..256 {
                    if checksum_slot(&value, slot)?.is_none() {
                        continue;
                    }
                    let unit = group
                        .checked_mul(256)
                        .and_then(|n| n.checked_add(slot as u64))
                        .ok_or(Error::Corrupt("checksum unit overflow"))?;
                    let offset = unit
                        .checked_mul(UNIT_BYTES)
                        .filter(|offset| *offset < size)
                        .ok_or(Error::Corrupt("checksum past EOF"))?;
                    match inspect_unit(
                        forest,
                        &catalog,
                        object.0,
                        size,
                        unit,
                        Some(&value),
                        Some((&mut held, u64::MAX)),
                    ) {
                        Ok(Some(bytes)) => {
                            if let Some(file) = scan.output.as_mut() {
                                destination = file
                                    .seek(SeekFrom::Start(offset))
                                    .and_then(|_| file.write_all(&bytes))
                                    .map_err(|e| {
                                        ExtractionError::Destination(Error::io(
                                            "write extraction output",
                                            e,
                                        ))
                                    });
                                if destination.is_err() {
                                    break;
                                }
                            }
                            scan.counts.verified += 1;
                            scan.counts.bytes += bytes.len() as u64;
                        }
                        Ok(None) => {
                            return Err(Error::Corrupt("enumerated checksum disappeared"));
                        }
                        Err(UnitError::Metadata(error)) => return Err(error),
                        Err(UnitError::Data(error)) => {
                            (scan.report)(Finding {
                                object: Some(object),
                                path: scan.path,
                                offset: Some(offset),
                                length: (size - offset).min(UNIT_BYTES),
                                error: &error,
                            });
                            scan.counts.unreadable += 1;
                        }
                    }
                }
                Ok(Some((key, destination)))
            };
            let group = if scan.reuse_cache {
                self.read_catalog(read)?
            } else {
                self.read_catalog_fresh(read)?
            };
            let Some((key, destination)) = group else {
                break;
            };
            destination?;
            start = key;
            start.push(0);
        }
        Ok(())
    }

    /// Extract a file, symlink, or directory subtree to a source-preserving
    /// destination.
    ///
    /// Requires read-only access. A full structural preflight runs before any
    /// destination creation; structural damage returns an incomplete summary
    /// and a finding. This version does not salvage structurally damaged trees.
    /// Data failures leave explicitly reported unwritten ranges and recovery
    /// continues. Check both `complete` and `unreadable_ranges` on success.
    /// Destination failures return Err and may leave partial output.
    pub fn extract_to(
        &mut self,
        object: ObjectId,
        destination: &mut dyn ExtractionSink,
        report: &mut dyn FnMut(Finding<'_>),
    ) -> Result<ExtractionSummary> {
        self.inspection_mode()?;
        let mut summary = ExtractionSummary::default();
        if self.check_with_report(report).is_err() {
            return Ok(summary);
        }
        self.attributes_fresh(object)?;
        let mut directories: Vec<Directory> = Vec::new();
        let mut path = Vec::new();
        let mut pending = Some(object);
        let mut active = object;
        let result = (|| -> std::result::Result<(), ExtractionError> {
            loop {
                if let Some(object) = pending.take() {
                    active = object;
                    let attributes = self.attributes_fresh(object)?;
                    match attributes.kind {
                        ObjectKind::File => {
                            let mut file = destination
                                .create_file(&path, &attributes)
                                .map_err(ExtractionError::Destination)?;
                            summary.objects += 1;
                            file.set_len(attributes.size).map_err(|e| {
                                ExtractionError::Destination(Error::io("size extraction output", e))
                            })?;
                            let mut counts = UnitCounts::default();
                            let result = self.inspect_file(
                                object,
                                attributes.size,
                                FileScan {
                                    path: &path,
                                    output: Some(&mut file),
                                    report,
                                    counts: &mut counts,
                                    reuse_cache: false,
                                },
                            );
                            summary.verified_bytes += counts.bytes;
                            summary.unreadable_ranges += counts.unreadable;
                            result?;
                            summary.files += 1;
                            path.pop();
                        }
                        ObjectKind::Symlink => {
                            let target = self.read_symlink_fresh(object)?;
                            destination
                                .create_symlink(&path, &target, &attributes)
                                .map_err(ExtractionError::Destination)?;
                            summary.objects += 1;
                            path.pop();
                        }
                        ObjectKind::Directory => {
                            let handle = self.retain_object_fresh(object)?;
                            // Register the handle before fallible work so every exit closes it.
                            directories.push(Directory {
                                object,
                                handle,
                                cursor: None,
                                entries: Vec::new().into_iter(),
                            });
                            destination
                                .create_directory(&path, &attributes)
                                .map_err(ExtractionError::Destination)?;
                            summary.objects += 1;
                            let dir = directories.last_mut().ok_or(Error::LimitExceeded)?;
                            let page = self.read_directory_fresh(&dir.handle, None)?;
                            dir.entries = page.entries.into_iter();
                            dir.cursor = page.cursor;
                        }
                    }
                }
                let Some(dir) = directories.last_mut() else {
                    break;
                };
                if let Some(entry) = dir.entries.next() {
                    path.push(entry.name);
                    pending = Some(entry.object);
                } else if let Some(cursor) = dir.cursor.take() {
                    active = dir.object;
                    let page = self.read_directory_fresh(&dir.handle, Some(&cursor))?;
                    dir.entries = page.entries.into_iter();
                    dir.cursor = page.cursor;
                } else {
                    let dir = directories.pop().ok_or(Error::LimitExceeded)?;
                    self.close_handle(dir.handle)?;
                    path.pop();
                }
            }
            Ok(())
        })();
        let mut close_result = Ok(());
        for dir in directories {
            if let Err(error) = self.close_handle(dir.handle) {
                close_result = Err(error);
            }
        }
        close_result?;
        match result {
            Ok(()) => summary.complete = true,
            Err(ExtractionError::Source(error)) => {
                metadata_finding(report, Some(active), &path, &error)
            }
            Err(ExtractionError::Destination(error)) => return Err(error),
        }
        Ok(summary)
    }
}
