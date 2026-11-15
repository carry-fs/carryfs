//! Offline repair: build and check a replacement forest before issuing output.

use std::collections::{BTreeMap, BTreeSet};

use crate::format::{
    BLOCK_BYTES, EXTENT_UNWRITTEN, Page, Root, TreeKind, TreeRef, UNIT_BYTES, checksum_slot,
    compact_group, decode_page, encode_node, expand_group, leaf_data_blocks, reserved_group_blocks,
    set_checksum_slot, validate_inode_features,
};
use crate::tree::{Record, tree_ref, u64_be, u64_le};
use crate::{BlockDevice, Error, Geometry, MountMode, Name, Result};

use super::namespace::{DirentValue, Inode, dirent_key, inode_key};
use super::{
    Attributes, DirectoryCursor, DirectoryEntry, ExtractionSink, ExtractionSummary, Finding,
    Handle, ObjectId, ObjectKind, ROOT, ROOT_A, Volume,
};

mod manifest;
use manifest::{Damage, FileRecord, Manifest, MissingRange};

/// A rebuilt image and the exact JSON stored in its embedded manifest.
pub struct ImageRecovery {
    /// Recovered object and source-data totals, excluding the generated manifest.
    pub summary: ExtractionSummary,
    /// Collision-safe root filename of the embedded manifest.
    pub manifest_name: Name,
    /// Manifest contents, suitable for a JSON sidecar.
    pub manifest_json: String,
    /// Whether requested destination verification detected damaged output data.
    pub verification_failed: bool,
}

/// An offline repair proposal. Diagnosis and planning never write to the source.
#[derive(Debug, Default)]
pub struct RepairPlan {
    /// Whether the original forest and both root mirrors passed diagnosis.
    pub clean: bool,
    /// Human-readable findings and proposed changes, in deterministic order.
    pub actions: Vec<String>,
    /// Reachable inodes retained, including those reconnected under lost+found.
    pub objects: u64,
    /// Whether inaccessible metadata or incomplete file mappings limit recovery.
    /// Applying such a plan requires explicit acceptance of data loss.
    pub data_loss: bool,
    /// Why in-place repair cannot safely proceed. Recovery may still be possible.
    pub blocked: Option<String>,
    /// Whether the reconstructed forest passed validation for read-only export.
    pub recovery_available: bool,
}

/// A diagnosed volume and its checked replacement metadata, held under one lease.
///
/// The caller must exclude mounts and other writers for this object's lifetime.
/// Repair writes only to the selected root's verified metadata workspace and the
/// inactive workspace bank. It preserves file data, UUIDs, IDs, and timestamps.
/// No historical pages are scanned and no older root is silently selected.
/// The complete metadata proposal is kept in memory.
pub struct Repair<D> {
    device: D,
    geometry: Geometry,
    original: Root,
    proposed: Root,
    mirrors: Vec<u64>,
    pages: BTreeMap<u64, Page>,
    plan: RepairPlan,
    missing: Vec<MissingRange>,
}

#[derive(Clone, Copy)]
struct Boundary<'a> {
    level: u8,
    low: &'a [u8],
    high: Option<&'a [u8]>,
}

struct Scan {
    records: BTreeMap<Vec<u8>, Vec<u8>>,
    pages: BTreeSet<u64>,
    missing: bool,
}

impl Scan {
    fn read(
        device: &mut impl BlockDevice,
        root: &Root,
        reference: TreeRef,
        kind: TreeKind,
        plan: &mut RepairPlan,
    ) -> Result<Self> {
        let mut scan = Self {
            records: BTreeMap::new(),
            pages: BTreeSet::new(),
            missing: false,
        };
        scan.walk(device, root, reference, kind, None, plan)?;
        Ok(scan)
    }

    fn walk(
        &mut self,
        device: &mut impl BlockDevice,
        root: &Root,
        reference: TreeRef,
        kind: TreeKind,
        boundary: Option<Boundary<'_>>,
        plan: &mut RepairPlan,
    ) -> Result<()> {
        // References and a decreasing level bound recursion even on a hostile image.
        let result = (|| {
            if reference.block < 512
                || reference.block >= root.blocks - 512
                || reference.generation > root.generation
                || !self.pages.insert(reference.block)
            {
                return Err(Error::Corrupt("invalid or repeated recovery page"));
            }
            let mut page = Page(Box::new([0; BLOCK_BYTES]));
            device.read_exact_at(reference.block * BLOCK_BYTES as u64, &mut page.0[..])?;
            let decoded = decode_page(
                &page.0[..],
                reference,
                &root.uuid,
                kind,
                root.generation,
                boundary.map(|boundary| boundary.level),
                root.blocks,
            )?;
            if let Some(Boundary { low, high, .. }) = boundary {
                if decoded
                    .records
                    .iter()
                    .any(|(key, _)| *key < low || high.is_some_and(|high| *key >= high))
                {
                    return Err(Error::Corrupt("recovery page leaves its parent key range"));
                }
                if decoded.records.first().map(|(key, _)| *key) != Some(low) {
                    plan.actions.push(format!(
                        "Rebuild child minimum at block {}",
                        reference.block
                    ));
                }
            }
            if decoded.level == 0 {
                for (key, value) in decoded.records {
                    if kind == TreeKind::Catalog {
                        validate_inode_features(key, value, root.features)?;
                    }
                    self.records.insert(key.to_vec(), value.to_vec());
                }
            } else {
                for (index, (key, value)) in decoded.records.iter().enumerate() {
                    let high = decoded
                        .records
                        .get(index + 1)
                        .map(|(key, _)| *key)
                        .or_else(|| boundary.and_then(|boundary| boundary.high));
                    self.walk(
                        device,
                        root,
                        tree_ref(value)?,
                        kind,
                        Some(Boundary {
                            level: decoded.level - 1,
                            low: key,
                            high,
                        }),
                        plan,
                    )?;
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(()),
            Err(Error::UnsupportedFormat) => Err(Error::UnsupportedFormat),
            Err(error) => {
                self.missing = true;
                plan.actions.push(format!(
                    "Inaccessible {kind:?} page {}: {error}",
                    reference.block
                ));
                Ok(())
            }
        }
    }
}

impl<D: BlockDevice> Repair<D> {
    /// Diagnose and prepare replacement metadata without writing a byte.
    ///
    /// Supports read-only devices. Corrupt or unreadable subtrees are reported
    /// while independently validated siblings are retained. Unsupported formats
    /// and ambiguous root identities stop recovery.
    pub fn diagnose(device: D) -> Result<Self> {
        Self::diagnose_inner(device, false)
    }

    fn diagnose_inner(mut device: D, force_virtual: bool) -> Result<Self> {
        let geometry = device.geometry()?;
        geometry.validate()?;
        let mut plan = RepairPlan::default();
        let mut roots = Vec::new();
        for block in [ROOT_A, geometry.blocks() - 257] {
            let mut page = Page(Box::new([0; BLOCK_BYTES]));
            let result = device
                .read_exact_at(geometry.block_offset(block)?, &mut page.0[..])
                .and_then(|()| Root::decode(&page.0[..], block, geometry));
            match result {
                Ok(root) => roots.push(Some(root)),
                Err(Error::UnsupportedFormat) => return Err(Error::UnsupportedFormat),
                Err(error) => {
                    plan.actions
                        .push(format!("Restore root mirror at block {block}: {error}"));
                    roots.push(None);
                }
            }
        }
        let original = super::select_root(roots[0].as_ref(), roots[1].as_ref())?.clone();
        let mirrors: Vec<_> = [ROOT_A, geometry.blocks() - 257]
            .into_iter()
            .zip(&roots)
            .filter(|(_, root)| root.as_ref() != Some(&original))
            .map(|(block, _)| block)
            .collect();
        for block in &mirrors {
            plan.actions.push(format!(
                "Mirror selected generation {} to root at block {block}",
                original.generation
            ));
        }
        match crate::check::validate(&mut device, &original) {
            Ok(summary) => {
                plan.objects = summary.objects;
                plan.clean = mirrors.is_empty();
                plan.recovery_available = true;
            }
            Err(Error::UnsupportedFormat) => return Err(Error::UnsupportedFormat),
            Err(error) => plan.actions.push(format!("Structural diagnosis: {error}")),
        }
        if plan.recovery_available {
            return Ok(Self {
                device,
                geometry,
                proposed: original.clone(),
                original,
                mirrors,
                pages: BTreeMap::new(),
                plan,
                missing: Vec::new(),
            });
        }
        plan.recovery_available = false;
        let mut catalog = Scan::read(
            &mut device,
            &original,
            original.catalog,
            TreeKind::Catalog,
            &mut plan,
        )?;
        let cleanup = Scan::read(
            &mut device,
            &original,
            original.cleanup,
            TreeKind::Cleanup,
            &mut plan,
        )?;
        let allocation = Scan::read(
            &mut device,
            &original,
            original.allocation,
            TreeKind::Allocation,
            &mut plan,
        )?;
        plan.data_loss = catalog.missing;
        // Cleanup holds retired pages and detached deleted data. Keep those
        // allocations until publication, even though the repaired root drops them.
        let mut protected = BTreeMap::new();
        for block in catalog
            .pages
            .iter()
            .chain(&cleanup.pages)
            .chain(&allocation.pages)
        {
            mark(&mut protected, *block, 1);
        }
        protect_extents(&catalog.records, &mut protected)?;
        for (key, value) in &cleanup.records {
            match key[0] {
                1 => {
                    let first = u64_be(&key[9..])?;
                    let count = u32::from_le_bytes(value[..4].try_into().unwrap()) as u64;
                    mark(&mut protected, first, count);
                }
                2 => {
                    let detached = Scan::read(
                        &mut device,
                        &original,
                        tree_ref(&value[16..])?,
                        TreeKind::Catalog,
                        &mut plan,
                    )?;
                    for block in detached.pages {
                        mark(&mut protected, block, 1);
                    }
                    protect_extents(&detached.records, &mut protected)?;
                    if detached.missing {
                        plan.blocked = Some(
                            "a detached tree is unreadable; its ownership cannot be established"
                                .into(),
                        );
                    }
                }
                _ => {}
            }
        }
        // Rebuild the free count rather than trusting it to validate the bank.
        let mut workspace_root = original.clone();
        workspace_root.free_blocks = original.blocks;
        let mut workspace = match crate::workspace::read(&mut device, &workspace_root) {
            Ok(slots) => slots,
            Err(error) => {
                plan.blocked = Some(format!(
                    "cannot establish a safe metadata workspace: {error}"
                ));
                Vec::new()
            }
        };
        // If a tree is missing, its checksummed allocation map must independently
        // confirm workspace ownership. Otherwise an unseen extent could alias it.
        if (catalog.missing || cleanup.missing)
            && (allocation.missing
                || workspace
                    .iter()
                    .any(|block| !allocated(&allocation.records, *block)))
        {
            plan.blocked = Some(
                "incomplete ownership and allocation metadata; no safe write locations".into(),
            );
        }
        if workspace.iter().any(|block| allocated(&protected, *block)) {
            plan.blocked = Some("metadata workspace overlaps an existing owner".into());
        }
        // A matching orphan marker records a committed unlink. Finish those
        // deletions; reconnecting them would undo the user's last operation.
        let targets: BTreeSet<_> = catalog
            .records
            .iter()
            .filter(|(key, _)| key[8] == 2)
            .map(|(_, value)| u64_le(&value[..8]))
            .collect::<Result<_>>()?;
        let deleted: BTreeSet<_> = cleanup
            .records
            .keys()
            .filter(|key| key[0] == 3)
            .map(|key| u64_be(&key[1..]))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|id| {
                *id != 1
                    && !targets.contains(id)
                    && catalog
                        .records
                        .get(&inode_key(*id)[..])
                        .is_some_and(|inode| inode[1] & 1 != 0)
            })
            .collect();
        for id in &deleted {
            plan.actions
                .push(format!("Reclaim intentionally unlinked object {id}"));
        }
        catalog.records.retain(|key, _| {
            !deleted.contains(&u64_be(&key[..8]).expect("validated catalog owner"))
        });
        let mut missing = Vec::new();
        normalize_catalog(&mut catalog.records, &original, &mut plan, &mut missing)?;
        if force_virtual && plan.blocked.is_none() {
            plan.blocked = Some("replacement metadata exceeds the safe workspace".into());
        }
        let virtual_workspace =
            plan.blocked.is_some() || workspace.len() < crate::allocation::MAX_PAGES;
        if virtual_workspace {
            if plan.blocked.is_none() {
                plan.blocked = Some("insufficient metadata workspace".into());
            }
            // These are virtual addresses only. They enable export when the
            // real workspace is damaged; a blocked plan can never write them.
            let mut data = BTreeMap::new();
            protect_extents(&catalog.records, &mut data)?;
            workspace = (512..original.blocks - 512)
                .filter(|block| !allocated(&data, *block))
                .take(
                    catalog
                        .records
                        .len()
                        .saturating_mul(2)
                        .saturating_add(data.len().saturating_mul(2))
                        .saturating_add(2 * crate::allocation::MAX_PAGES + 128),
                )
                .collect();
        }
        let mut proposed = original.clone();
        proposed.generation = proposed
            .generation
            .checked_add(1)
            .ok_or(Error::LimitExceeded)?;
        proposed.next_object = catalog
            .records
            .keys()
            .map(|key| u64_be(&key[..8]))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .max()
            .unwrap_or(1)
            .checked_add(1)
            .ok_or(Error::LimitExceeded)?
            .max(original.next_object);
        proposed.workspace = crate::workspace::next_reference(&original)?;
        proposed.workspace_cursor = 0;
        let mut builder = Builder {
            root: &proposed,
            slots: &workspace,
            used: 0,
            pages: BTreeMap::new(),
        };
        let built = (|| {
            let catalog_ref = builder.tree(TreeKind::Catalog, &catalog.records)?;
            let cleanup_ref = builder.tree(TreeKind::Cleanup, &BTreeMap::new())?;
            let (allocation_ref, owned, available_end) =
                builder.allocation(&catalog.records, virtual_workspace)?;
            Ok((
                catalog_ref,
                cleanup_ref,
                allocation_ref,
                owned,
                available_end,
            ))
        })();
        let used = builder.used;
        let mut pages = builder.pages;
        match built {
            Ok((catalog_ref, cleanup_ref, allocation_ref, owned, available_end)) => {
                proposed.catalog = catalog_ref;
                proposed.cleanup = cleanup_ref;
                proposed.allocation = allocation_ref;
                proposed.free_blocks =
                    proposed.blocks - 1024 - owned + (available_end - used) as u64;
                pages.extend(crate::workspace::encode(
                    &proposed,
                    &workspace[used..available_end],
                )?);
                add_roots(&mut pages, &proposed, geometry)?;
                match crate::check::validate(
                    &mut Overlay {
                        device: &mut device,
                        pages: &pages,
                    },
                    &proposed,
                ) {
                    Ok(_) => {
                        plan.recovery_available = true;
                        plan.actions.push(format!("Publish generation {}: rebuild catalog, allocation and cleanup; write {used} metadata pages", proposed.generation));
                    }
                    Err(error) => {
                        plan.blocked =
                            Some(format!("replacement forest failed validation: {error}"))
                    }
                }
            }
            Err(Error::NoSpace) if !virtual_workspace => {
                // Export can reuse any non-data address in its private overlay.
                // Replan once without the physical workspace's capacity limit.
                return Self::diagnose_inner(device, true);
            }
            Err(error) => {
                plan.blocked = Some(format!("cannot build a safe replacement forest: {error}"))
            }
        }
        Ok(Self {
            device,
            geometry,
            original,
            proposed,
            mirrors,
            pages,
            plan,
            missing,
        })
    }

    /// Inspect the deterministic proposal before applying it.
    pub fn plan(&self) -> &RepairPlan {
        &self.plan
    }

    /// Apply this proposal offline. `accept_data_loss` permits reported missing
    /// catalog pages or file mappings to be discarded. File bytes are never rewritten.
    ///
    /// The caller must persist the plan before calling this method. On I/O
    /// failure close the device and diagnose again; no further writes are made.
    /// Returns false for a clean filesystem. Data checksums require a separate scrub.
    pub fn apply(mut self, accept_data_loss: bool) -> Result<bool> {
        if self.device.mode() != MountMode::ReadWrite {
            return Err(Error::ReadOnly);
        }
        if self.plan.clean {
            return Ok(false);
        }
        if self.plan.blocked.is_some() {
            return Err(Error::InvalidInput("repair plan is blocked"));
        }
        if self.plan.data_loss && !accept_data_loss {
            return Err(Error::InvalidInput(
                "repair would discard inaccessible metadata or mappings; recover files first, or explicitly accept data loss",
            ));
        }
        // Finish mirroring the selected root before reusing its workspace:
        // an older surviving mirror might still own some of those pages.
        for block in &self.mirrors {
            let block = *block;
            let page = self.original.encode(block, self.geometry)?;
            self.device
                .write_all_at(block * BLOCK_BYTES as u64, &page.0[..])?;
            self.device.flush_durable()?;
        }
        if self.original != self.proposed {
            for (block, page) in &self.pages {
                if *block != ROOT_A && *block != self.geometry.blocks() - 257 {
                    self.device
                        .write_all_at(*block * BLOCK_BYTES as u64, &page.0[..])?;
                }
            }
            self.device.flush_durable()?;
            for block in [ROOT_A, self.geometry.blocks() - 257] {
                let page = self.proposed.encode(block, self.geometry)?;
                self.device
                    .write_all_at(block * BLOCK_BYTES as u64, &page.0[..])?;
                self.device.flush_durable()?;
            }
        }
        for block in [ROOT_A, self.geometry.blocks() - 257] {
            let mut page = Page(Box::new([0; BLOCK_BYTES]));
            self.device
                .read_exact_at(block * BLOCK_BYTES as u64, &mut page.0[..])?;
            if Root::decode(&page.0[..], block, self.geometry)? != self.proposed {
                return Err(Error::Corrupt("repair root did not persist"));
            }
        }
        crate::check::validate(&mut self.device, &self.proposed)?;
        Ok(true)
    }

    /// Export the reconstructed namespace without modifying the damaged source.
    /// Uses the normal checksummed extraction path and reports partial recovery.
    pub fn recover_to(
        &mut self,
        sink: &mut dyn ExtractionSink,
        report: &mut dyn FnMut(Finding<'_>),
    ) -> Result<ExtractionSummary> {
        if !self.plan.recovery_available {
            return Err(Error::InvalidInput(
                "recovery forest could not be validated",
            ));
        }
        let mut volume = Volume::open(Overlay {
            device: &mut self.device,
            pages: &self.pages,
        })?;
        let mut summary = volume.extract_to(super::ROOT, sink, report)?;
        summary.complete &= !self.plan.data_loss;
        volume.eject()?;
        Ok(summary)
    }

    /// Rebuild recovered files into a fresh, distinct destination device.
    ///
    /// The caller must supply new zeroed media and exclude source aliases.
    /// The source is never written. The destination receives a new UUID and
    /// object IDs, retaining the label, names, timestamps, modes, and symlinks.
    /// Sparse holes and reported unreadable ranges remain unwritten. Unwritten
    /// preallocation is not copied. Check `complete` and `unreadable_ranges`:
    /// successful output may still be partial. Errors can leave a partial image.
    pub fn recover_image<E: BlockDevice>(
        &mut self,
        destination: E,
        report: &mut dyn FnMut(Finding<'_>),
    ) -> Result<ExtractionSummary> {
        self.recover_image_with_verification(destination, false, report)
            .map(|recovery| recovery.summary)
    }

    /// Rebuild an image with an embedded JSON manifest and optional data read-back.
    /// Source failures produce partial results. Destination verification failures
    /// are recorded separately and set `verification_failed`; they are not success.
    pub fn recover_image_with_verification<E: BlockDevice>(
        &mut self,
        destination: E,
        verify_data: bool,
        report: &mut dyn FnMut(Finding<'_>),
    ) -> Result<ImageRecovery> {
        if !self.plan.recovery_available {
            return Err(Error::InvalidInput(
                "recovery forest could not be validated",
            ));
        }
        let mut source = Volume::open(Overlay {
            device: &mut self.device,
            pages: &self.pages,
        })?;
        // Rebuilt metadata lives entirely in the immutable overlay. Reuse its
        // bounded cache; physical source metadata still gets fresh reads.
        source.inspection_metadata_cache = !self.pages.is_empty();
        let mut target = match self.original.label.clone() {
            Some(label) => Volume::format_with_label(destination, label)?,
            None => Volume::format(destination)?,
        };
        if source.stores_attributes() {
            target.upgrade()?;
        }
        let mut summary = ExtractionSummary {
            complete: !self.plan.data_loss,
            ..Default::default()
        };
        let mut suffix = 0u64;
        let (manifest_name, working_name) = loop {
            let name = Name::parse(&if suffix == 0 {
                "RECOVERY-REPORT.json".into()
            } else {
                format!("RECOVERY-REPORT.{suffix}.json")
            })?;
            let working = Name::parse(&format!(
                "{}.working",
                std::str::from_utf8(name.stored()).expect("manifest name")
            ))?;
            let mut available = true;
            for candidate in [&name, &working] {
                match source.lookup(ROOT, candidate) {
                    Err(Error::NotFound) => {}
                    Ok(_) => available = false,
                    Err(error) => return Err(error),
                }
            }
            if available {
                break (name, working);
            }
            suffix += 1;
        };
        let mut manifest = Manifest {
            source_uuid: self.original.uuid,
            source_generation: self.original.generation,
            source_bytes: self.geometry.byte_len,
            destination_uuid: target.info()?.uuid,
            metadata_complete: !self.plan.data_loss,
            actions: self.plan.actions.clone(),
            files: Vec::new(),
            verification: if verify_data {
                "pending"
            } else {
                "not_requested"
            },
        };
        write_manifest(
            &mut target,
            &manifest_name,
            &working_name,
            &manifest.json("in_progress", &summary),
        )?;
        target.sync_publication()?;
        // The durable marker survives interrupted copying. Batch the remaining
        // writes through the normal bounded checkpoint path until publication.
        target.set_deferred_checkpoints(true)?;
        let mut directories: Vec<ImageDirectory> = Vec::new();
        let mut path = Vec::new();
        let mut pending = Some((ROOT, ROOT));
        loop {
            if let Some((object, destination)) = pending.take() {
                let attributes = source.attributes_fresh(object)?;
                let handle = source.open_directory(object)?;
                let page = source.read_directory_fresh(&handle, None)?;
                directories.push(ImageDirectory {
                    handle,
                    destination,
                    attributes,
                    entries: page.entries.into_iter(),
                    cursor: page.cursor,
                });
                summary.objects += 1;
            }
            let Some(directory) = directories.last_mut() else {
                break;
            };
            if let Some(entry) = directory.entries.next() {
                path.push(entry.name.clone());
                let attributes = source.attributes_fresh(entry.object)?;
                let destination = match entry.kind {
                    ObjectKind::Directory => {
                        let object = target.create_directory(directory.destination, entry.name)?;
                        pending = Some((entry.object, object));
                        continue;
                    }
                    ObjectKind::File => {
                        let file = target.create_file(directory.destination, entry.name.clone())?;
                        let object = target.lookup(directory.destination, &entry.name)?;
                        target.truncate(&file, attributes.size)?;
                        let mut source_damage: Vec<_> = self
                            .missing
                            .iter()
                            .filter(|range| range.object == entry.object.0)
                            .map(|range| range.damage.clone())
                            .collect();
                        let copied = source.copy_verified_file(
                            entry.object,
                            attributes.size,
                            &path,
                            &mut |offset, bytes| {
                                let count = target.write(&file, offset, bytes)?;
                                if count != bytes.len() {
                                    return Err(Error::io(
                                        "write recovered image",
                                        std::io::Error::from(std::io::ErrorKind::WriteZero),
                                    ));
                                }
                                Ok(())
                            },
                            &mut |finding| {
                                source_damage.push(Damage::finding(&finding));
                                report(finding);
                            },
                        );
                        let closed = target.close_handle(file);
                        let (bytes, unreadable) = copied?;
                        closed?;
                        manifest.files.push(FileRecord {
                            path: path.clone(),
                            source: entry.object,
                            destination: object,
                            size: attributes.size,
                            copied_bytes: bytes,
                            verified_bytes: 0,
                            source_damage,
                            destination_damage: Vec::new(),
                        });
                        summary.files += 1;
                        summary.verified_bytes += bytes;
                        summary.unreadable_ranges += unreadable;
                        object
                    }
                    ObjectKind::Symlink => target.create_symlink(
                        directory.destination,
                        entry.name,
                        &source.read_symlink_fresh(entry.object)?,
                    )?,
                };
                restore_image_attributes(&mut target, destination, attributes)?;
                summary.objects += 1;
                path.pop();
            } else if let Some(cursor) = directory.cursor.take() {
                let page = source.read_directory_fresh(&directory.handle, Some(&cursor))?;
                directory.entries = page.entries.into_iter();
                directory.cursor = page.cursor;
            } else {
                let directory = directories.pop().ok_or(Error::LimitExceeded)?;
                source.close_handle(directory.handle)?;
                // Child creation changes a directory's timestamps; restore them last.
                restore_image_attributes(&mut target, directory.destination, directory.attributes)?;
                path.pop();
            }
        }
        let state = if summary.complete && summary.unreadable_ranges == 0 {
            "complete"
        } else {
            "partial"
        };
        let mut verification_failed = false;
        if verify_data {
            write_manifest(
                &mut target,
                &manifest_name,
                &working_name,
                &manifest.json("unverified", &summary),
            )?;
            target.sync()?;
            let pages = BTreeMap::new();
            let mut verified = Volume::open(Overlay {
                device: target.device.as_mut().ok_or(Error::Ejected)?,
                pages: &pages,
            })?;
            verified.check()?;
            // As in scrub, verify physical metadata once before the data pass,
            // then reuse its bounded cache on this exclusively held device.
            verified.inspection_metadata_cache = true;
            for file in &mut manifest.files {
                let (bytes, unreadable) = verified.copy_verified_file(
                    file.destination,
                    file.size,
                    &file.path,
                    &mut |_, _| Ok(()),
                    &mut |finding| {
                        file.destination_damage.push(Damage::finding(&finding));
                        report(finding);
                    },
                )?;
                file.verified_bytes = bytes;
                verification_failed |= unreadable != 0;
            }
            verified.eject()?;
            manifest.verification = if verification_failed {
                "failed"
            } else {
                "passed"
            };
        }
        let manifest_json = manifest.json(
            if verification_failed {
                "verification_failed"
            } else {
                state
            },
            &summary,
        );
        write_manifest(&mut target, &manifest_name, &working_name, &manifest_json)?;
        restore_image_attributes(&mut target, ROOT, source.attributes_fresh(ROOT)?)?;
        target.sync()?;
        let mut device = target.device.take().ok_or(Error::Ejected)?;
        {
            // Keep verification tied to the held device, not a replaceable path.
            let pages = BTreeMap::new();
            let mut verified = Volume::open(Overlay {
                device: &mut device,
                pages: &pages,
            })?;
            verified.check()?;
            if verify_data {
                let file = verified.open_file(ROOT, &manifest_name, super::Access::Read)?;
                let mut buffer = vec![0; UNIT_BYTES];
                let mut offset = 0;
                for expected in manifest_json.as_bytes().chunks(UNIT_BYTES) {
                    let count = verified.read(&file, offset, &mut buffer[..expected.len()])?;
                    if count != expected.len() || &buffer[..count] != expected {
                        return Err(Error::io(
                            "verify recovery manifest",
                            std::io::Error::from(std::io::ErrorKind::InvalidData),
                        ));
                    }
                    offset += count as u64;
                }
                verified.close_handle(file)?;
            }
            verified.eject()?;
        }
        source.eject()?;
        Ok(ImageRecovery {
            summary,
            manifest_name,
            manifest_json,
            verification_failed,
        })
    }
}

fn write_manifest<D: BlockDevice>(
    volume: &mut Volume<D>,
    name: &Name,
    working: &Name,
    json: &str,
) -> Result<()> {
    let file = volume.create_file(ROOT, working.clone())?;
    let result = (|| {
        let mut offset = 0;
        while offset < json.len() {
            let count = volume.write(&file, offset as u64, &json.as_bytes()[offset..])?;
            if count == 0 {
                return Err(Error::io(
                    "write recovery manifest",
                    std::io::Error::from(std::io::ErrorKind::WriteZero),
                ));
            }
            offset += count;
        }
        volume.sync_publication()
    })();
    let closed = volume.close_handle(file);
    result?;
    closed?;
    // Publish the complete JSON atomically. A crash retains the older marker
    // rather than a truncated report claiming that recovery finished.
    volume.rename(ROOT, working, ROOT, name.clone(), super::Replace::Allowed)
}

struct ImageDirectory {
    handle: Handle,
    destination: ObjectId,
    attributes: Attributes,
    entries: std::vec::IntoIter<DirectoryEntry>,
    cursor: Option<DirectoryCursor>,
}

fn restore_image_attributes<D: BlockDevice>(
    volume: &mut Volume<D>,
    object: ObjectId,
    attributes: Attributes,
) -> Result<()> {
    let stores_attributes = volume.stores_attributes();
    volume.edit(|transaction| {
        let key = inode_key(object.0);
        let mut inode = transaction
            .forest
            .lookup_with(&transaction.catalog, TreeKind::Catalog, &key, Inode::decode)?
            .ok_or(Error::Corrupt("recovered inode missing"))?;
        inode.creation = attributes.creation;
        inode.modification = attributes.modification;
        inode.metadata_change = attributes.metadata_change;
        inode.directory_link = attributes.directory_link;
        if stores_attributes {
            if inode.kind != ObjectKind::Symlink {
                inode.mode = attributes.mode.encode();
            }
            inode.attributes = if attributes.hidden {
                crate::format::ATTRIBUTE_HIDDEN
            } else {
                0
            };
        }
        transaction.catalog = transaction.forest.put(
            transaction.catalog.clone(),
            TreeKind::Catalog,
            &key,
            &inode.encode(),
        )?;
        Ok(())
    })
}

fn allocated(records: &BTreeMap<Vec<u8>, Vec<u8>>, block: u64) -> bool {
    records
        .get(&(block / 16_384).to_be_bytes()[..])
        .is_some_and(|bitmap| bitmap[(block % 16_384 / 8) as usize] & (1 << (block % 8)) != 0)
}

fn protect_extents(
    records: &BTreeMap<Vec<u8>, Vec<u8>>,
    owned: &mut BTreeMap<Vec<u8>, Vec<u8>>,
) -> Result<()> {
    for (key, value) in records {
        if key.len() == 17 && key[8] == 3 {
            let first = u64_le(&value[..8])?;
            let count = u32::from_le_bytes(value[8..12].try_into().unwrap()) as u64;
            mark(owned, first, count);
        }
    }
    Ok(())
}

fn add_roots(pages: &mut BTreeMap<u64, Page>, root: &Root, geometry: Geometry) -> Result<()> {
    for block in [ROOT_A, geometry.blocks() - 257] {
        pages.insert(block, root.encode(block, geometry)?);
    }
    Ok(())
}

/// A packed bottom-up tree; no changes to the normal incremental edit engine.
// ponytail: repair fits in the existing workspace while leaving 4096 slots;
// add a separate verified free-space allocator if larger repairs need it.
struct Builder<'a> {
    root: &'a Root,
    slots: &'a [u64],
    used: usize,
    pages: BTreeMap<u64, Page>,
}

impl Builder<'_> {
    fn allocation(
        &mut self,
        records: &BTreeMap<Vec<u8>, Vec<u8>>,
        virtual_workspace: bool,
    ) -> Result<(TreeRef, u64, usize)> {
        let start = self.used;
        let mut expected = 1;
        loop {
            let available_end = if virtual_workspace {
                start + expected + crate::allocation::MAX_PAGES
            } else {
                self.slots.len()
            };
            let slots = self.slots.get(..available_end).ok_or(Error::NoSpace)?;
            let mut bitmaps = BTreeMap::new();
            protect_extents(records, &mut bitmaps)?;
            if slots.iter().any(|block| allocated(&bitmaps, *block)) {
                return Err(Error::Corrupt("recovered extents overlap the workspace"));
            }
            for block in slots {
                mark(&mut bitmaps, *block, 1);
            }
            let owned: u64 = bitmaps
                .values()
                .flat_map(|bitmap| bitmap.iter())
                .map(|byte| byte.count_ones() as u64)
                .sum();
            mark(&mut bitmaps, 0, 512);
            mark(&mut bitmaps, self.root.blocks - 512, 512);
            for (key, bitmap) in &mut bitmaps {
                for block in reserved_group_blocks(u64_be(key)?, self.root.blocks) {
                    bitmap[(block % 16_384 / 8) as usize] |= 1 << (block % 8);
                }
            }
            let mut tree = Builder {
                root: self.root,
                slots: self.slots,
                used: start,
                pages: BTreeMap::new(),
            };
            let reference = tree.tree(TreeKind::Allocation, &bitmaps)?;
            let actual = tree.used - start;
            if virtual_workspace && actual != expected {
                // Including the bitmap's own pages can add a bitmap group.
                // Increasing its prefix only adds groups, so this converges.
                expected = actual;
                continue;
            }
            if available_end.saturating_sub(tree.used) < crate::allocation::MAX_PAGES {
                return Err(Error::NoSpace);
            }
            self.used = tree.used;
            self.pages.extend(tree.pages);
            return Ok((reference, owned, available_end));
        }
    }

    fn tree(&mut self, kind: TreeKind, records: &BTreeMap<Vec<u8>, Vec<u8>>) -> Result<TreeRef> {
        let mut records: Vec<Record> = records
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut level = 0;
        let mut totals = BTreeMap::new();
        loop {
            let mut children = Vec::new();
            let mut start = 0;
            loop {
                let mut end = start;
                let mut bytes = 0;
                while end < records.len()
                    && bytes + 4 + records[end].0.len() + records[end].1.len() <= BLOCK_BYTES - 80
                {
                    bytes += 4 + records[end].0.len() + records[end].1.len();
                    end += 1;
                }
                // Internal pages need two children, including the final page.
                if level > 0 && records.len() - end == 1 && end - start > 2 {
                    end -= 1;
                }
                let block = *self.slots.get(self.used).ok_or(Error::NoSpace)?;
                self.used += 1;
                let reference = TreeRef {
                    block,
                    generation: self.root.generation,
                };
                let borrowed: Vec<_> = records[start..end]
                    .iter()
                    .map(|(k, v)| (k.as_slice(), v.as_slice()))
                    .collect();
                let data = if level == 0 {
                    leaf_data_blocks(
                        reference,
                        &self.root.uuid,
                        kind,
                        &borrowed,
                        self.root.blocks,
                    )?
                } else {
                    records[start..end]
                        .iter()
                        .map(|(_, v)| totals[&tree_ref(v).unwrap().block])
                        .sum()
                };
                let page = encode_node(
                    reference,
                    &self.root.uuid,
                    kind,
                    &borrowed,
                    self.root.blocks,
                    level,
                    data,
                )?;
                self.pages.insert(block, page);
                totals.insert(block, data);
                let mut value = block.to_le_bytes().to_vec();
                value.extend_from_slice(&reference.generation.to_le_bytes());
                children.push((
                    records.get(start).map_or_else(Vec::new, |(k, _)| k.clone()),
                    value,
                ));
                start = end;
                if start == records.len() {
                    break;
                }
            }
            if children.len() == 1 {
                return tree_ref(&children[0].1);
            }
            records = children;
            level += 1;
            if level > 7 {
                return Err(Error::LimitExceeded);
            }
        }
    }
}

struct Overlay<'a, D> {
    device: &'a mut D,
    pages: &'a BTreeMap<u64, Page>,
}
impl<D: BlockDevice> BlockDevice for Overlay<'_, D> {
    fn mode(&self) -> MountMode {
        MountMode::ReadOnly
    }
    fn geometry(&mut self) -> Result<Geometry> {
        self.device.geometry()
    }
    fn read_exact_at(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
        for (index, chunk) in out.chunks_mut(BLOCK_BYTES).enumerate() {
            let at = offset + (index * BLOCK_BYTES) as u64;
            if let Some(page) = self.pages.get(&(at / BLOCK_BYTES as u64)) {
                chunk.copy_from_slice(&page.0[..chunk.len()]);
            } else {
                self.device.read_exact_at(at, chunk)?;
            }
        }
        Ok(())
    }
    fn write_all_at(&mut self, _: u64, _: &[u8]) -> Result<()> {
        Err(Error::ReadOnly)
    }
    fn flush_durable(&mut self) -> Result<()> {
        Err(Error::ReadOnly)
    }
}

fn normalize_catalog(
    records: &mut BTreeMap<Vec<u8>, Vec<u8>>,
    root: &Root,
    plan: &mut RepairPlan,
    missing: &mut Vec<MissingRange>,
) -> Result<()> {
    let mut inodes = BTreeMap::new();
    for (key, value) in records.iter().filter(|(key, _)| key[8] == 1) {
        inodes.insert(u64_be(&key[..8])?, Inode::decode(value)?);
    }
    if let std::collections::btree_map::Entry::Vacant(entry) = inodes.entry(1) {
        let now = super::Timestamp::new(0, 0)?;
        entry.insert(Inode {
            kind: ObjectKind::Directory,
            orphan: false,
            directory_link: None,
            parent: 1,
            size: 0,
            allocated_blocks: 0,
            child_count: 0,
            creation: now,
            modification: now,
            metadata_change: now,
            mode: 0,
            attributes: 0,
            target: Vec::new(),
        });
        plan.actions.push("Reconstruct the root directory".into());
        plan.data_loss = true;
    }
    if inodes[&1].kind != ObjectKind::Directory {
        return Err(Error::Corrupt("root inode is not a directory"));
    }
    let mut links = BTreeMap::<u64, (u64, Name)>::new();
    let mut ambiguous = BTreeSet::new();
    for (key, value) in records.iter().filter(|(key, _)| key[8] == 2) {
        let parent = u64_be(&key[..8])?;
        let entry = DirentValue::decode(value)?;
        let valid = inodes
            .get(&parent)
            .is_some_and(|inode| inode.kind == ObjectKind::Directory)
            && entry.target != 1
            && inodes.get(&entry.target).is_some_and(|inode| {
                inode.kind == entry.kind && inode.parent == parent && !inode.orphan
            })
            && crate::name::validate_disk_name(entry.stored_name, &key[9..]).is_ok();
        if !valid {
            if !inodes.contains_key(&entry.target) {
                plan.data_loss = true;
            }
            plan.actions.push(format!(
                "Remove invalid directory entry from object {parent} to {}",
                entry.target
            ));
            continue;
        }
        let name = Name {
            stored: entry.stored_name.to_vec(),
            folded: key[9..].to_vec(),
        };
        if links.insert(entry.target, (parent, name)).is_some() {
            ambiguous.insert(entry.target);
        }
    }
    for object in ambiguous {
        links.remove(&object);
    }
    // Every retained edge names an existing directory. Break each cycle once;
    // memoized paths keep a deep namespace linear in the number of inodes.
    let mut rooted = BTreeSet::from([1]);
    for object in inodes.keys().copied() {
        let mut path = BTreeSet::new();
        let mut current = object;
        while !rooted.contains(&current) {
            if !path.insert(current) {
                links.remove(&current);
                plan.actions
                    .push(format!("Break directory cycle at object {current}"));
                break;
            }
            let Some((parent, _)) = links.get(&current) else {
                break;
            };
            current = *parent;
        }
        rooted.extend(path);
    }
    let disconnected: Vec<_> = inodes
        .keys()
        .copied()
        .filter(|id| *id != 1 && !links.contains_key(id))
        .collect();
    if !disconnected.is_empty() {
        let mut suffix = 0;
        let (lost, lost_name) = loop {
            let name = Name::parse(&if suffix == 0 {
                "lost+found".to_owned()
            } else {
                format!("lost+found.{suffix}")
            })?;
            let existing = links
                .iter()
                .find(|(_, (parent, candidate))| *parent == 1 && candidate.folded == name.folded)
                .map(|(id, _)| *id);
            match existing {
                Some(id) if inodes[&id].kind == ObjectKind::Directory => break (id, name),
                Some(_) => suffix += 1,
                None => {
                    let id = root
                        .next_object
                        .max(inodes.last_key_value().map_or(2, |(id, _)| id + 1));
                    ObjectId::new(id)?;
                    let source = &inodes[&1];
                    let inode = Inode {
                        kind: ObjectKind::Directory,
                        orphan: false,
                        directory_link: None,
                        parent: 1,
                        size: 0,
                        allocated_blocks: 0,
                        child_count: 0,
                        creation: source.creation,
                        modification: source.modification,
                        metadata_change: source.metadata_change,
                        mode: 0,
                        attributes: 0,
                        target: Vec::new(),
                    };
                    inodes.insert(id, inode);
                    links.insert(id, (1, name.clone()));
                    break (id, name);
                }
            }
        };
        for id in disconnected {
            let mut suffix = 0;
            let name = loop {
                let name = Name::parse(&if suffix == 0 {
                    id.to_string()
                } else {
                    format!("{id}.{suffix}")
                })?;
                // Generated names for different object IDs cannot collide.
                // Check only retained source entries using the catalog index.
                let occupied = match records.get(&dirent_key(lost, &name.folded)) {
                    Some(value) => links.get(&DirentValue::decode(value)?.target).is_some_and(
                        |(parent, candidate)| *parent == lost && candidate.folded == name.folded,
                    ),
                    None => false,
                };
                if !occupied {
                    break name;
                }
                suffix += 1;
            };
            plan.actions.push(format!(
                "Reconnect object {id} at /{}/{}",
                String::from_utf8_lossy(lost_name.stored()),
                String::from_utf8_lossy(name.stored())
            ));
            links.insert(id, (lost, name));
        }
    }
    let old = std::mem::take(records);
    // Keep only regular-file mappings whose inode survived. Detached records
    // cannot be assigned a size or type reliably and are explicit recovery loss.
    for (key, value) in &old {
        if key[8] >= 3 {
            let owner = u64_be(&key[..8])?;
            if inodes
                .get(&owner)
                .is_some_and(|inode| inode.kind == ObjectKind::File)
            {
                records.insert(key.clone(), value.clone());
            } else {
                plan.data_loss = true;
                plan.actions.push(format!(
                    "Unrecoverable mapping for missing or non-file inode {owner}"
                ));
            }
        }
    }
    for (id, inode) in &mut inodes {
        if inode.kind == ObjectKind::File {
            normalize_file(*id, inode, records, plan, missing)?;
        }
        inode.orphan = false;
        inode.parent = links.get(id).map_or(1, |(parent, _)| *parent);
        inode.child_count = 0;
    }
    for (id, (parent, name)) in &links {
        let entry = DirentValue {
            target: *id,
            kind: inodes[id].kind,
            stored_name: name.stored(),
        };
        records.insert(dirent_key(*parent, &name.folded), entry.encode()?);
        inodes
            .get_mut(parent)
            .ok_or(Error::Corrupt("recovery parent missing"))?
            .child_count += 1;
    }
    plan.objects = inodes.len() as u64;
    for (id, inode) in inodes {
        records.insert(inode_key(id).to_vec(), inode.encode());
    }
    Ok(())
}

fn normalize_file(
    id: u64,
    inode: &mut Inode,
    records: &mut BTreeMap<Vec<u8>, Vec<u8>>,
    plan: &mut RepairPlan,
    missing: &mut Vec<MissingRange>,
) -> Result<()> {
    use super::file::{extent_key, group_key};
    let low = extent_key(id, 0).to_vec();
    let high = inode_key(id + 1).to_vec();
    let source: BTreeMap<_, _> = records
        .range(low..high)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let extents: Vec<_> = source
        .iter()
        .filter(|(k, _)| k[8] == 3)
        .map(|(k, v)| {
            Ok((
                u64_be(&k[9..])?,
                u64_le(&v[..8])?,
                u32::from_le_bytes(v[8..12].try_into().unwrap()) as u64,
                u32::from_le_bytes(v[12..16].try_into().unwrap()),
            ))
        })
        .collect::<Result<_>>()?;
    let mut valid = BTreeMap::new();
    let mut accepted: Vec<(u64, u64)> = Vec::new();
    for (key, value) in source.iter().filter(|(k, _)| k[8] == 4) {
        let group = u64_be(&key[9..])?;
        let mut checksums = expand_group(value);
        for slot in 0..256 {
            if checksum_slot(value, slot)?.is_none() {
                continue;
            }
            let unit = group * 256 + slot as u64;
            let offset = unit * UNIT_BYTES as u64;
            let first = unit * 16;
            let end = (first + 16).min(inode.size.div_ceil(BLOCK_BYTES as u64));
            let mut at = first;
            while offset < inode.size && at < end {
                let index = extents.partition_point(|(logical, _, _, _)| *logical <= at);
                let Some(index) = index.checked_sub(1) else {
                    break;
                };
                let (logical, _, count, _) = extents[index];
                if logical + count <= at {
                    break;
                }
                let next = (logical + count).min(end);
                if extents
                    .get(index + 1)
                    .is_some_and(|(other, _, _, _)| *other < next)
                    || (index > 0 && extents[index - 1].0 + extents[index - 1].2 > at)
                {
                    break;
                }
                at = next;
            }
            if offset >= inode.size || at != end {
                set_checksum_slot(&mut checksums, slot, None);
                missing.push(MissingRange {
                    object: id,
                    damage: Damage {
                        offset: Some(offset.min(inode.size)),
                        length: inode.size.saturating_sub(offset).min(UNIT_BYTES as u64),
                        error: "checksum mapping incomplete".into(),
                    },
                });
                plan.data_loss = true;
                plan.actions.push(format!(
                    "Unrecoverable object {id} bytes {offset}..{}: checksum mapping incomplete",
                    inode.size.min(offset.saturating_add(UNIT_BYTES as u64))
                ));
            } else if let Some((_, last)) = accepted.last_mut().filter(|(_, last)| *last == first) {
                *last = end;
            } else {
                accepted.push((first, end));
            }
        }
        if checksums[..32].iter().any(|b| *b != 0) {
            valid.insert(
                group_key(id, group).to_vec(),
                compact_group(&checksums).to_vec(),
            );
        }
    }
    let mut blocks = 0;
    for (logical, physical, count, flags) in extents {
        if flags & EXTENT_UNWRITTEN != 0 {
            let mut value = physical.to_le_bytes().to_vec();
            value.extend_from_slice(&(count as u32).to_le_bytes());
            value.extend_from_slice(&flags.to_le_bytes());
            valid.insert(extent_key(id, logical).to_vec(), value);
            blocks += count;
            continue;
        }
        let end = logical + count;
        let mut covered = logical;
        let index = accepted.partition_point(|(_, end)| *end <= logical);
        for (first, until) in accepted[index..]
            .iter()
            .take_while(|(first, _)| *first < end)
        {
            let start = logical.max(*first);
            let until = end.min(*until);
            if covered < start && flags & EXTENT_UNWRITTEN == 0 {
                lost_mapping(plan, id, covered, start, inode.size, missing);
            }
            let mut value = (physical + start - logical).to_le_bytes().to_vec();
            value.extend_from_slice(&((until - start) as u32).to_le_bytes());
            value.extend_from_slice(&flags.to_le_bytes());
            valid.insert(extent_key(id, start).to_vec(), value);
            blocks += until - start;
            covered = until;
        }
        if covered < end && flags & EXTENT_UNWRITTEN == 0 {
            lost_mapping(plan, id, covered, end, inode.size, missing);
        }
    }
    for key in source.keys() {
        records.remove(key);
    }
    records.extend(valid);
    inode.allocated_blocks = blocks;
    Ok(())
}

fn lost_mapping(
    plan: &mut RepairPlan,
    id: u64,
    first: u64,
    end: u64,
    size: u64,
    missing: &mut Vec<MissingRange>,
) {
    let offset = (first * BLOCK_BYTES as u64).min(size);
    missing.push(MissingRange {
        object: id,
        damage: Damage {
            offset: Some(offset),
            length: (end * BLOCK_BYTES as u64).min(size).saturating_sub(offset),
            error: "no complete checksum mapping".into(),
        },
    });
    plan.data_loss = true;
    plan.actions.push(format!(
        "Unrecoverable object {id} bytes {}..{}: no complete checksum mapping",
        first * BLOCK_BYTES as u64,
        end * BLOCK_BYTES as u64
    ));
}

// Compact ownership bitmaps avoid one allocation per data block on large files.
fn mark(bitmaps: &mut BTreeMap<Vec<u8>, Vec<u8>>, mut start: u64, count: u64) {
    let end = start + count;
    while start < end {
        let next = end.min((start / 16_384 + 1) * 16_384);
        let bitmap = bitmaps
            .entry((start / 16_384).to_be_bytes().to_vec())
            .or_insert_with(|| vec![0; 2048]);
        let mut at = (start % 16_384) as usize;
        let until = at + (next - start) as usize;
        while at < until && !at.is_multiple_of(8) {
            bitmap[at / 8] |= 1 << (at % 8);
            at += 1;
        }
        bitmap[at / 8..until / 8].fill(0xff);
        at = at.max(until / 8 * 8);
        while at < until {
            bitmap[at / 8] |= 1 << (at % 8);
            at += 1;
        }
        start = next;
    }
}

#[cfg(test)]
mod tests;
