//! Namespace and metadata operations: directories, inode attributes, rename,
//! remove, timestamps, and directory enumeration. File content is in `file`.

use super::Fault;
use crate::allocation::Transaction;
use crate::format::{TreeKind, Validation, validate_symlink_target};
use crate::name::{Name, validate_disk_name};
use crate::tree::{Forest, u64_le};
use crate::{BlockDevice, Error, MountMode, Result};

use super::{
    Access, Attributes, Handle, ObjectId, ObjectKind, ROOT, RemoveKind, Replace, Timestamp, Volume,
};

pub(super) const INODE_BYTES: usize = 128;

type RemovalGuard<'a> = Option<&'a mut dyn FnMut(&[u8]) -> Result<()>>;

pub(super) fn inode_key(object: u64) -> Vec<u8> {
    let mut key = object.to_be_bytes().to_vec();
    key.push(1);
    key
}

pub(super) fn dirent_key(parent: u64, folded: &[u8]) -> Vec<u8> {
    let mut key = parent.to_be_bytes().to_vec();
    key.push(2);
    key.extend_from_slice(folded);
    key
}

fn decode_timestamp(bytes: &[u8]) -> Result<Timestamp> {
    let seconds = i64::from_le_bytes(
        bytes[0..8]
            .try_into()
            .map_err(|_| Error::Corrupt("timestamp seconds"))?,
    );
    let nanos = u32::from_le_bytes(
        bytes[8..12]
            .try_into()
            .map_err(|_| Error::Corrupt("timestamp nanos"))?,
    );
    Timestamp::new(seconds, nanos).map_err(|_| Error::Corrupt("timestamp out of range"))
}

fn encode_timestamp(bytes: &mut [u8], timestamp: Timestamp) {
    bytes[0..8].copy_from_slice(&timestamp.seconds().to_le_bytes());
    bytes[8..12].copy_from_slice(&timestamp.nanos().to_le_bytes());
    bytes[12..16].fill(0);
}

pub(super) struct Inode {
    pub(super) kind: ObjectKind,
    pub(super) orphan: bool,
    /// Flag bits 1 and 2: whether a symlink Windows made is a directory link.
    pub(super) directory_link: Option<bool>,
    pub(super) parent: u64,
    pub(super) size: u64,
    pub(super) allocated_blocks: u64,
    pub(super) child_count: u64,
    pub(super) creation: Timestamp,
    pub(super) modification: Timestamp,
    pub(super) metadata_change: Timestamp,
    /// A symlink's target, stored after the fixed fields; empty otherwise.
    pub(super) target: Vec<u8>,
}

fn kind_code(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::File => 1,
        ObjectKind::Directory => 2,
        ObjectKind::Symlink => 3,
    }
}

fn kind_from_code(code: u8) -> Option<ObjectKind> {
    match code {
        1 => Some(ObjectKind::File),
        2 => Some(ObjectKind::Directory),
        3 => Some(ObjectKind::Symlink),
        _ => None,
    }
}

impl Inode {
    pub(super) fn decode(value: &[u8]) -> Result<Self> {
        if value.len() < INODE_BYTES {
            return Err(Error::Corrupt("invalid inode length"));
        }
        let kind = kind_from_code(value[0]).ok_or(Error::Corrupt("invalid inode type"))?;
        if (kind == ObjectKind::Symlink) != (value.len() > INODE_BYTES) {
            return Err(Error::Corrupt("invalid inode length"));
        }
        Ok(Self {
            kind,
            orphan: value[1] & 1 != 0,
            directory_link: (value[1] & 2 != 0).then_some(value[1] & 4 != 0),
            parent: u64_le(&value[8..16])?,
            size: u64_le(&value[16..24])?,
            allocated_blocks: u64_le(&value[24..32])?,
            child_count: u64_le(&value[32..40])?,
            creation: decode_timestamp(&value[40..56])?,
            modification: decode_timestamp(&value[56..72])?,
            metadata_change: decode_timestamp(&value[72..88])?,
            target: value[INODE_BYTES..].to_vec(),
        })
    }

    pub(super) fn encode(&self) -> Vec<u8> {
        let mut value = vec![0; INODE_BYTES + self.target.len()];
        value[0] = kind_code(self.kind);
        value[1] = u8::from(self.orphan)
            | match self.directory_link {
                None => 0,
                Some(false) => 2,
                Some(true) => 6,
            };
        value[8..16].copy_from_slice(&self.parent.to_le_bytes());
        value[16..24].copy_from_slice(&self.size.to_le_bytes());
        value[24..32].copy_from_slice(&self.allocated_blocks.to_le_bytes());
        value[32..40].copy_from_slice(&self.child_count.to_le_bytes());
        encode_timestamp(&mut value[40..56], self.creation);
        encode_timestamp(&mut value[56..72], self.modification);
        encode_timestamp(&mut value[72..88], self.metadata_change);
        value[INODE_BYTES..].copy_from_slice(&self.target);
        value
    }
}

struct DirentValue<'a> {
    target: u64,
    kind: ObjectKind,
    stored_name: &'a [u8],
}

impl<'a> DirentValue<'a> {
    fn decode(value: &'a [u8]) -> Result<Self> {
        if value.len() < 12 {
            return Err(Error::Corrupt("invalid directory entry length"));
        }
        let target = u64_le(&value[0..8])?;
        let kind =
            kind_from_code(value[8]).ok_or(Error::Corrupt("invalid directory entry type"))?;
        let name_len = usize::from(u16::from_le_bytes(
            value[10..12]
                .try_into()
                .map_err(|_| Error::Corrupt("directory entry name length"))?,
        ));
        if value.len() != 12 + name_len {
            return Err(Error::Corrupt("directory entry length mismatch"));
        }
        Ok(Self {
            target,
            kind,
            stored_name: &value[12..],
        })
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let name_len = u16::try_from(self.stored_name.len())
            .map_err(|_| Error::InvalidInput("name is too long"))?;
        let mut value = vec![0; 12 + self.stored_name.len()];
        value[0..8].copy_from_slice(&self.target.to_le_bytes());
        value[8] = kind_code(self.kind);
        value[10..12].copy_from_slice(&name_len.to_le_bytes());
        value[12..].copy_from_slice(self.stored_name);
        Ok(value)
    }
}

/// Up to 63 directory entries, with an optional continuation cursor.
#[derive(Clone, Debug)]
pub struct DirectoryPage {
    /// Entries in this page, in comparison-key order.
    pub entries: Vec<DirectoryEntry>,
    /// Present when more entries remain; pass to the next `read_directory` call.
    pub cursor: Option<DirectoryCursor>,
}

/// One directory entry: a child's name, object ID, and type.
#[derive(Clone, Debug)]
pub struct DirectoryEntry {
    /// The child's validated name.
    pub name: Name,
    /// The child's object ID.
    pub object: ObjectId,
    /// Whether the child is a file or a directory.
    pub kind: ObjectKind,
}

/// An opaque continuation cursor for `read_directory`.
///
/// Bound to the mount instance, the directory, and the namespace epoch at
/// issue time. Any namespace mutation anywhere in the volume invalidates
/// every open cursor, not only the affected directory's.
#[derive(Clone, Debug)]
pub struct DirectoryCursor {
    mount: u64,
    directory: ObjectId,
    epoch: u64,
    key: Vec<u8>,
}

fn dirent_prefix_match(object: u64, key: &[u8]) -> bool {
    key.len() >= 9 && key[..8] == object.to_be_bytes() && key[8] == 2
}

impl<D: BlockDevice> Volume<D> {
    /// Run one edit. An eager volume publishes it as its own plan. With
    /// deferred checkpoints it joins the open batch while the batch stays
    /// sealable (`Transaction::fits_batch`); otherwise the batch is sealed
    /// without it and it runs as its own plan. A failed or refused edit leaves
    /// the batch as it found it.
    pub(super) fn edit<T>(
        &mut self,
        mut edit: impl FnMut(&mut Transaction<'_, D>) -> Result<T>,
    ) -> Result<T> {
        self.writable()?;
        if self.chain.is_some() {
            if let Some(result) = self.edit_batched(&mut edit)? {
                return Ok(result);
            }
            self.seal_batch()?;
            // Between checkpoints no maintenance trims the workspace and
            // superseded pages stay in it, so a long stream of edits, such as
            // an unbatched `rm -rf`, can overflow its 12,500 slots. A
            // failed plan writes nothing: `sync` trims and checkpoints, then
            // the edit runs once more.
            match self.edit_alone(&mut edit) {
                Err(Error::LimitExceeded | Error::NoSpace) if self.has_unsynced_changes() => {
                    self.sync()?;
                }
                result => return result,
            }
        }
        self.edit_alone(&mut edit)
    }

    /// Apply `edit` to the open batch. `Ok(None)` means the batch refused it:
    /// it broke a batch bound, or hit a limit the batch may have caused, and
    /// has been undone.
    fn edit_batched<T>(
        &mut self,
        edit: &mut impl FnMut(&mut Transaction<'_, D>) -> Result<T>,
    ) -> Result<Option<T>> {
        let pending = self.batch.take();
        let fresh = pending.is_none();
        let outcome = (|| {
            let device = self.device.as_mut().ok_or(Error::Ejected)?;
            let root = self
                .root
                .as_ref()
                .ok_or(Error::InvalidInput("volume is not published"))?;
            let chain = self
                .chain
                .as_ref()
                .ok_or(Error::InvalidInput("batch without deferred checkpoints"))?;
            let context = crate::allocation::Context {
                cache: &mut self.cache,
                durable: Some((&chain.durable, &chain.excluded)),
            };
            let mut transaction = match pending {
                Some(pending) => Transaction::resume(device, root, context, pending)?,
                None => Transaction::cached(device, root, Some(context))?,
            };
            let undo = transaction.mark();
            let kept = match edit(&mut transaction) {
                Ok(value) if transaction.fits_batch() => Some(Ok(value)),
                Ok(_) | Err(Error::LimitExceeded | Error::NoSpace) => None,
                Err(error) => Some(Err(error)),
            };
            if !matches!(kept, Some(Ok(_))) {
                transaction.rewind(undo);
            }
            // An accepted operation's data leaves now; a rewound one held none.
            let sent = transaction.forest.send_data();
            Ok((kept, sent, transaction.suspend()))
        })();
        let (kept, pending) = match outcome {
            // Some pages may have reached blocks the batch no longer
            // tracks, so fault, as a failed publication does.
            Ok((_, Err(error), _)) => {
                self.set_fault(Fault::Publication);
                return self.record_corruption(Err(error));
            }
            Ok((kept, Ok(()), pending)) => (kept, pending),
            // Resuming failed, so the batch is gone with acknowledged
            // operations in it: fault, as a failed publication does.
            Err(error) if !fresh => {
                self.set_fault(Fault::Publication);
                return self.record_corruption(Err(error));
            }
            Err(error) => return self.record_corruption(Err(error)),
        };
        let keep = matches!(kept, Some(Ok(_))) || !fresh;
        if keep {
            self.batch = Some(pending);
        }
        match kept {
            Some(Ok(value)) => Ok(Some(value)),
            Some(Err(error)) => self.record_corruption(Err(error)),
            None => Ok(None),
        }
    }

    /// Run one edit as its own mirrored transaction.
    fn edit_alone<T>(
        &mut self,
        edit: &mut impl FnMut(&mut Transaction<'_, D>) -> Result<T>,
    ) -> Result<T> {
        let prepared = (|| {
            let device = self.device.as_mut().ok_or(Error::Ejected)?;
            let root = self
                .root
                .as_ref()
                .ok_or(Error::InvalidInput("volume is not published"))?;
            let context = crate::allocation::Context {
                cache: &mut self.cache,
                durable: self
                    .chain
                    .as_ref()
                    .map(|chain| (&chain.durable, &chain.excluded)),
            };
            let mut transaction = Transaction::cached(device, root, Some(context))?;
            let result = edit(&mut transaction)?;
            let plan = transaction.seal(false)?;
            Ok((result, plan.into_parts(root)?))
        })();
        let (result, (next, writes, returned, slots, built, data_pages)) =
            self.record_corruption(prepared)?;
        self.publish(
            next,
            super::block_writes(writes),
            &returned,
            slots,
            built,
            data_pages,
        )?;
        Ok(result)
    }

    /// Seal the open batch as one plan and publish it. The batch holds
    /// operations only while `fits_batch` says its seal cannot fail, so this
    /// fails only on a device error or a bug. Either faults the volume, which
    /// loses the batch as a crash would.
    pub(super) fn seal_batch(&mut self) -> Result<()> {
        let Some(pending) = self.batch.take() else {
            return Ok(());
        };
        let prepared = (|| {
            let device = self.device.as_mut().ok_or(Error::Ejected)?;
            let root = self
                .root
                .as_ref()
                .ok_or(Error::InvalidInput("volume is not published"))?;
            let chain = self
                .chain
                .as_ref()
                .ok_or(Error::InvalidInput("batch without deferred checkpoints"))?;
            let context = crate::allocation::Context {
                cache: &mut self.cache,
                durable: Some((&chain.durable, &chain.excluded)),
            };
            Transaction::resume(device, root, context, pending)?
                .seal(false)?
                .into_parts(root)
        })();
        let (next, writes, returned, slots, built, data_pages) = match prepared {
            Ok(parts) => parts,
            Err(error) => {
                // Space and budget are what `fits_batch` bounds; failing on
                // either would mean a bound is wrong, so tests stop here.
                debug_assert!(
                    !matches!(error, Error::NoSpace | Error::LimitExceeded),
                    "a batch that fit failed to seal: {error:?}"
                );
                self.set_fault(Fault::Publication);
                return self.record_corruption(Err(error));
            }
        };
        self.publish(
            next,
            super::block_writes(writes),
            &returned,
            slots,
            built,
            data_pages,
        )
    }

    pub(super) fn read_catalog<T>(
        &mut self,
        read: impl FnOnce(&mut Forest<'_, D>, crate::tree::Edge) -> Result<T>,
    ) -> Result<T> {
        self.read_catalog_with_cache(true, read)
    }

    pub(super) fn read_catalog_fresh<T>(
        &mut self,
        read: impl FnOnce(&mut Forest<'_, D>, crate::tree::Edge) -> Result<T>,
    ) -> Result<T> {
        self.read_catalog_with_cache(false, read)
    }

    fn read_catalog_with_cache<T>(
        &mut self,
        reuse_readonly_cache: bool,
        read: impl FnOnce(&mut Forest<'_, D>, crate::tree::Edge) -> Result<T>,
    ) -> Result<T> {
        self.readable()?;
        let result = (|| {
            let device = self.device.as_mut().ok_or(Error::Ejected)?;
            let root = self
                .root
                .as_ref()
                .ok_or(Error::InvalidInput("volume is not published"))?;
            // An open batch holds edits no published root has yet.
            if let Some(batch) = self.batch.as_mut() {
                return batch.read(device, root, &mut self.cache, read);
            }
            // Inspection opts into fresh reads; mounted read-only calls may
            // retain the same bounded, checksummed nodes as writable calls.
            let mut local;
            let cache = if device.mode() == MountMode::ReadWrite
                || self.scrubbing
                || (reuse_readonly_cache && self.readonly_metadata_cache)
            {
                &mut self.cache
            } else {
                local = crate::tree::Cache::default();
                &mut local
            };
            let mut forest = Forest::cached(device, root, Some(cache))?;
            let catalog = forest.root(root.catalog, TreeKind::Catalog)?;
            read(&mut forest, catalog)
        })();
        self.record_corruption(result)
    }

    /// Resolve a child's object ID by parent and name.
    pub fn lookup(&mut self, parent: ObjectId, name: &Name) -> Result<ObjectId> {
        let key = dirent_key(parent.0, &name.folded);
        self.read_catalog(|forest, catalog| {
            let value = forest
                .lookup(&catalog, TreeKind::Catalog, &key)?
                .ok_or(Error::NotFound)?;
            Ok(ObjectId(DirentValue::decode(&value)?.target))
        })
    }

    /// Resolve a child and return its stored spelling and type with its ID.
    ///
    /// Lookup is case-insensitive, so the stored spelling can differ from the
    /// requested one. Adapters that report a normalized name, such as Windows
    /// `FileNameInformation`, return the stored spelling.
    pub fn lookup_entry(&mut self, parent: ObjectId, name: &Name) -> Result<DirectoryEntry> {
        let key = dirent_key(parent.0, &name.folded);
        self.read_catalog(|forest, catalog| {
            let value = forest
                .lookup(&catalog, TreeKind::Catalog, &key)?
                .ok_or(Error::NotFound)?;
            let dirent = DirentValue::decode(&value)?;
            validate_disk_name(dirent.stored_name, &key[9..])?;
            Ok(DirectoryEntry {
                name: Name {
                    stored: dirent.stored_name.to_vec(),
                    folded: name.folded.clone(),
                },
                object: ObjectId(dirent.target),
                kind: dirent.kind,
            })
        })
    }

    /// Read an object's type, size, and stored timestamps.
    pub fn attributes(&mut self, object: ObjectId) -> Result<Attributes> {
        self.attributes_with_cache(object, true)
    }

    pub(super) fn attributes_fresh(&mut self, object: ObjectId) -> Result<Attributes> {
        self.attributes_with_cache(object, false)
    }

    fn attributes_with_cache(&mut self, object: ObjectId, reuse: bool) -> Result<Attributes> {
        let key = inode_key(object.0);
        let read = |forest: &mut Forest<'_, D>, catalog| {
            let value = forest
                .lookup(&catalog, TreeKind::Catalog, &key)?
                .ok_or(Error::NotFound)?;
            let inode = Inode::decode(&value)?;
            Ok(Attributes {
                kind: inode.kind,
                size: inode.size,
                allocated_bytes: inode
                    .allocated_blocks
                    .checked_mul(4096)
                    .ok_or(Error::Corrupt("allocated bytes overflow"))?,
                parent: ObjectId(inode.parent),
                creation: inode.creation,
                modification: inode.modification,
                metadata_change: inode.metadata_change,
                orphan: inode.orphan,
                directory_link: inode.directory_link,
            })
        };
        if reuse {
            self.read_catalog(read)
        } else {
            self.read_catalog_fresh(read)
        }
    }

    /// Read attributes for a handle's object.
    pub fn handle_attributes(&mut self, handle: &Handle) -> Result<Attributes> {
        let object = self.handles.validate(handle)?;
        self.attributes(object)
    }

    /// Create a new empty directory under `parent`.
    pub fn create_directory(&mut self, parent: ObjectId, name: Name) -> Result<ObjectId> {
        self.create(parent, name, ObjectKind::Directory)
    }

    /// Create a symlink to `target` under `parent`. The target is 1 through
    /// 1,023 bytes of UTF-8 without NUL, stored verbatim and never resolved.
    pub fn create_symlink(
        &mut self,
        parent: ObjectId,
        name: Name,
        target: &[u8],
    ) -> Result<ObjectId> {
        validate_symlink_target(target, Validation::Input)?;
        self.create_with(parent, name, ObjectKind::Symlink, target)
    }

    /// Read a symlink's target.
    pub fn read_symlink(&mut self, object: ObjectId) -> Result<Vec<u8>> {
        self.read_symlink_with_cache(object, true)
    }

    pub(super) fn read_symlink_fresh(&mut self, object: ObjectId) -> Result<Vec<u8>> {
        self.read_symlink_with_cache(object, false)
    }

    fn read_symlink_with_cache(&mut self, object: ObjectId, reuse: bool) -> Result<Vec<u8>> {
        let key = inode_key(object.0);
        let read = |forest: &mut Forest<'_, D>, catalog| {
            let value = forest
                .lookup(&catalog, TreeKind::Catalog, &key)?
                .ok_or(Error::NotFound)?;
            let inode = Inode::decode(&value)?;
            if inode.kind != ObjectKind::Symlink {
                return Err(Error::WrongType);
            }
            Ok(inode.target)
        };
        if reuse {
            self.read_catalog(read)
        } else {
            self.read_catalog_fresh(read)
        }
    }

    /// Turn the empty regular file or empty directory `name` under `parent`
    /// into a symlink to `target`, keeping its ID and creation time. Windows
    /// creates a link this way: it creates an object, then sets its reparse point.
    pub fn make_symlink(&mut self, parent: ObjectId, name: &Name, target: &[u8]) -> Result<()> {
        validate_symlink_target(target, Validation::Input)?;
        let now = Timestamp::now()?;
        self.edit(|transaction| make_symlink_in(transaction, parent.0, name, target, now))?;
        self.namespace_epoch = self.namespace_epoch.wrapping_add(1);
        Ok(())
    }

    /// Create an empty object of `kind` and link it under `parent`.
    pub(super) fn create(
        &mut self,
        parent: ObjectId,
        name: Name,
        kind: ObjectKind,
    ) -> Result<ObjectId> {
        self.create_with(parent, name, kind, &[])
    }

    fn create_with(
        &mut self,
        parent: ObjectId,
        name: Name,
        kind: ObjectKind,
        target: &[u8],
    ) -> Result<ObjectId> {
        let now = Timestamp::now()?;
        let object = self
            .edit(|transaction| create_in(transaction, parent, name.clone(), kind, target, now))?;
        self.namespace_epoch = self.namespace_epoch.wrapping_add(1);
        Ok(object)
    }

    /// Remove a file or an empty directory from its parent.
    ///
    /// Sets the object's orphan flag and records a cleanup marker; the actual
    /// catalog-interval detach happens later, in bounded maintenance.
    pub fn remove(&mut self, parent: ObjectId, name: &Name, kind: RemoveKind) -> Result<()> {
        self.remove_checked(parent, name, kind, None)
    }

    /// Remove an entry after checking its validated stored UTF-8 spelling.
    ///
    /// `before` runs at most once, before any namespace change. An error leaves the
    /// entry unchanged. Adapters can invalidate a cached spelling here without
    /// resolving the same entry twice. The edit retains [`Self::remove`]'s
    /// type checks, orphan handling and publication behavior.
    pub fn remove_with(
        &mut self,
        parent: ObjectId,
        name: &Name,
        kind: RemoveKind,
        before: impl FnOnce(&[u8]) -> Result<()>,
    ) -> Result<()> {
        let mut before = Some(before);
        let mut refused = None;
        let mut guard = |stored: &[u8]| {
            if let Some(before) = before.take()
                && let Err(error) = before(stored)
            {
                refused = Some(error);
                // A guard's space/budget error must not trigger an edit retry.
                return Err(Error::InvalidInput("removal guard refused"));
            }
            Ok(())
        };
        let result = self.remove_checked(parent, name, kind, Some(&mut guard));
        match refused {
            Some(error) => Err(error),
            None => result,
        }
    }

    fn remove_checked(
        &mut self,
        parent: ObjectId,
        name: &Name,
        kind: RemoveKind,
        mut before: RemovalGuard<'_>,
    ) -> Result<()> {
        let now = Timestamp::now()?;
        let pinned = self.handles.pinned();
        self.edit(|transaction| {
            remove_entry(transaction, parent.0, name, kind, now, &pinned, &mut before)
        })?;
        self.namespace_epoch = self.namespace_epoch.wrapping_add(1);
        Ok(())
    }

    /// Rename or move an object, optionally replacing an existing destination.
    pub fn rename(
        &mut self,
        source_parent: ObjectId,
        source: &Name,
        destination_parent: ObjectId,
        destination: Name,
        replace: Replace,
    ) -> Result<()> {
        if source_parent == destination_parent && source.folded == destination.folded {
            self.writable()?;
            let stored = self.read_catalog(|forest, catalog| {
                let key = dirent_key(source_parent.0, &source.folded);
                let value = forest
                    .lookup(&catalog, TreeKind::Catalog, &key)?
                    .ok_or(Error::NotFound)?;
                let dirent = DirentValue::decode(&value)?;
                validate_disk_name(dirent.stored_name, &key[9..])?;
                Ok(dirent.stored_name.to_vec())
            })?;
            if stored == destination.stored {
                return Ok(());
            }
            let now = Timestamp::now()?;
            self.edit(|transaction| {
                rename_spelling(transaction, source_parent.0, &destination, now)
            })?;
            self.namespace_epoch = self.namespace_epoch.wrapping_add(1);
            return Ok(());
        }
        let now = Timestamp::now()?;
        self.edit(|transaction| {
            rename_move(
                transaction,
                source_parent.0,
                source,
                destination_parent.0,
                &destination,
                replace,
                now,
            )
        })?;
        self.namespace_epoch = self.namespace_epoch.wrapping_add(1);
        Ok(())
    }

    /// Read one bounded page of a directory's entries.
    pub fn read_directory(
        &mut self,
        directory: &Handle,
        cursor: Option<&DirectoryCursor>,
    ) -> Result<DirectoryPage> {
        let object = self.handles.validate(directory)?;
        if let Some(cursor) = cursor
            && (cursor.mount != self.mount
                || cursor.directory != object
                || cursor.epoch != self.namespace_epoch)
        {
            return Err(Error::DirectoryChanged);
        }
        let start = match cursor {
            Some(cursor) => [cursor.key.as_slice(), &[0]].concat(),
            None => dirent_key(object.0, &[]),
        };
        self.directory_page(object, start, true)
    }

    pub(super) fn read_directory_fresh(
        &mut self,
        directory: &Handle,
        cursor: Option<&DirectoryCursor>,
    ) -> Result<DirectoryPage> {
        let object = self.handles.validate(directory)?;
        if let Some(cursor) = cursor
            && (cursor.mount != self.mount
                || cursor.directory != object
                || cursor.epoch != self.namespace_epoch)
        {
            return Err(Error::DirectoryChanged);
        }
        let start = match cursor {
            Some(cursor) => [cursor.key.as_slice(), &[0]].concat(),
            None => dirent_key(object.0, &[]),
        };
        self.directory_page(object, start, false)
    }

    /// Read one bounded page of entries whose comparison keys follow `after`.
    ///
    /// Unlike a cursor, this survives namespace changes, as POSIX readdir and
    /// name-marker enumeration require: entries unchanged since the previous
    /// page appear exactly once; entries added or removed meanwhile may or may not.
    /// `after` need not still exist.
    pub fn read_directory_after(
        &mut self,
        directory: &Handle,
        after: &Name,
    ) -> Result<DirectoryPage> {
        let object = self.handles.validate(directory)?;
        let mut start = dirent_key(object.0, &after.folded);
        start.push(0);
        self.directory_page(object, start, true)
    }

    /// Scan dirents of `object` from the inclusive catalog key `start`.
    fn directory_page(
        &mut self,
        object: ObjectId,
        start: Vec<u8>,
        reuse: bool,
    ) -> Result<DirectoryPage> {
        let mount = self.mount;
        let epoch = self.namespace_epoch;
        let read = |forest: &mut Forest<'_, D>, catalog| {
            let inode_value = forest
                .lookup(&catalog, TreeKind::Catalog, &inode_key(object.0))?
                .ok_or(Error::Corrupt("handle target inode is missing"))?;
            if Inode::decode(&inode_value)?.kind != ObjectKind::Directory {
                return Err(Error::WrongType);
            }
            // Entries end where the directory's next record kind begins.
            let mut end = object.0.to_be_bytes().to_vec();
            end.push(3);
            let raw = forest.scan_before(&catalog, TreeKind::Catalog, &start, Some(&end), 64)?;
            let mut matched = Vec::with_capacity(raw.len());
            for (key, value) in raw {
                if !dirent_prefix_match(object.0, &key) {
                    break;
                }
                matched.push((key, value));
            }
            // Keep one lookahead record so a full page needs no second tree walk.
            let needs_cursor = matched.len() == 64;
            if needs_cursor {
                matched.pop();
            }
            let cursor = match matched.last() {
                Some((key, _)) if needs_cursor => Some(DirectoryCursor {
                    mount,
                    directory: object,
                    epoch,
                    key: key.clone(),
                }),
                _ => None,
            };
            let mut entries = Vec::with_capacity(matched.len());
            for (mut key, mut value) in matched {
                let dirent = DirentValue::decode(&value)?;
                validate_disk_name(dirent.stored_name, &key[9..])?;
                let (target, kind) = (dirent.target, dirent.kind);
                // The scan's copies become the entry's name: drop the record
                // headers in place instead of copying both tails again.
                value.drain(..12);
                key.drain(..9);
                entries.push(DirectoryEntry {
                    name: Name {
                        stored: value,
                        folded: key,
                    },
                    object: ObjectId(target),
                    kind,
                });
            }
            Ok(DirectoryPage { entries, cursor })
        };
        if reuse {
            self.read_catalog(read)
        } else {
            self.read_catalog_fresh(read)
        }
    }

    /// Overwrite explicit creation/modification timestamps. Always bumps the
    /// metadata-change time.
    pub fn set_times(
        &mut self,
        handle: &Handle,
        creation: Option<Timestamp>,
        modification: Option<Timestamp>,
    ) -> Result<()> {
        let object = self.handles.validate(handle)?;
        let now = Timestamp::now()?;
        self.edit(|transaction| {
            let key = inode_key(object.0);
            let value = transaction
                .forest
                .lookup(&transaction.catalog, TreeKind::Catalog, &key)?
                .ok_or(Error::Corrupt("handle target inode is missing"))?;
            let mut inode = Inode::decode(&value)?;
            if let Some(creation) = creation {
                inode.creation = creation;
            }
            if let Some(modification) = modification {
                inode.modification = modification;
            }
            inode.metadata_change = now;
            let catalog = transaction.forest.put(
                transaction.catalog.clone(),
                TreeKind::Catalog,
                &key,
                &inode.encode(),
            )?;
            transaction.catalog = catalog;
            Ok(())
        })
    }

    /// Open a handle to a directory, pinning it against maintenance reclamation.
    pub fn open_directory(&mut self, object: ObjectId) -> Result<Handle> {
        let key = inode_key(object.0);
        let kind = self.read_catalog(|forest, catalog| {
            let value = forest
                .lookup(&catalog, TreeKind::Catalog, &key)?
                .ok_or(Error::NotFound)?;
            Ok(Inode::decode(&value)?.kind)
        })?;
        if kind != ObjectKind::Directory {
            return Err(Error::WrongType);
        }
        self.handles.open(object, Access::Read)
    }

    /// Open an object by ID, including an orphan that still exists. Only
    /// regular files open for writing.
    pub fn open_object(&mut self, object: ObjectId, access: Access) -> Result<Handle> {
        if access == Access::ReadWrite {
            self.writable()?;
        }
        if self.attributes(object)?.kind != ObjectKind::File && access == Access::ReadWrite {
            return Err(Error::WrongType);
        }
        self.handles.open(object, access)
    }

    /// Pin a catalog object by ID without asserting its type.
    pub fn retain_object(&mut self, object: ObjectId) -> Result<Handle> {
        self.retain_object_with_cache(object, true)
    }

    pub(super) fn retain_object_fresh(&mut self, object: ObjectId) -> Result<Handle> {
        self.retain_object_with_cache(object, false)
    }

    fn retain_object_with_cache(&mut self, object: ObjectId, reuse: bool) -> Result<Handle> {
        let key = inode_key(object.0);
        let read = |forest: &mut Forest<'_, D>, catalog| {
            forest
                .lookup(&catalog, TreeKind::Catalog, &key)?
                .ok_or(Error::NotFound)
        };
        if reuse {
            self.read_catalog(read)?;
        } else {
            self.read_catalog_fresh(read)?;
        }
        self.handles.open(object, Access::Read)
    }

    /// Release a handle. Never publishes; the object may still be pinned by
    /// other open handles.
    pub fn close_handle(&mut self, handle: Handle) -> Result<()> {
        self.handles.close(handle)
    }
}

fn remove_entry<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    parent: u64,
    name: &Name,
    kind: RemoveKind,
    now: Timestamp,
    pinned: &std::collections::BTreeSet<u64>,
    before: &mut RemovalGuard<'_>,
) -> Result<()> {
    let dirent_key_bytes = dirent_key(parent, &name.folded);
    let dirent_value = transaction
        .forest
        .lookup(&transaction.catalog, TreeKind::Catalog, &dirent_key_bytes)?
        .ok_or(Error::NotFound)?;
    let dirent = DirentValue::decode(&dirent_value)?;
    if let Some(before) = before.as_mut() {
        validate_disk_name(dirent.stored_name, &dirent_key_bytes[9..])?;
        before(dirent.stored_name)?;
    }
    let target = dirent.target;
    if target == ROOT.0 {
        return Err(Error::Corrupt("root cannot be removed"));
    }
    let target_key = inode_key(target);
    let target_value = transaction
        .forest
        .lookup(&transaction.catalog, TreeKind::Catalog, &target_key)?
        .ok_or(Error::Corrupt("directory entry has no inode"))?;
    let mut target_inode = Inode::decode(&target_value)?;
    if (kind == RemoveKind::Directory) != (target_inode.kind == ObjectKind::Directory) {
        return Err(Error::WrongType);
    }
    if target_inode.kind == ObjectKind::Directory && target_inode.child_count != 0 {
        return Err(Error::DirectoryNotEmpty);
    }
    let parent_key = inode_key(parent);
    let parent_value = transaction
        .forest
        .lookup(&transaction.catalog, TreeKind::Catalog, &parent_key)?
        .ok_or(Error::Corrupt("parent inode is missing"))?;
    let mut parent_inode = Inode::decode(&parent_value)?;
    parent_inode.child_count = parent_inode
        .child_count
        .checked_sub(1)
        .ok_or(Error::Corrupt("directory child count underflow"))?;
    parent_inode.modification = now;
    parent_inode.metadata_change = now;
    target_inode.orphan = true;
    target_inode.metadata_change = now;

    let mut catalog = transaction.catalog.clone();
    catalog = transaction
        .forest
        .remove(catalog, TreeKind::Catalog, &dirent_key_bytes)?;
    catalog = transaction.forest.put(
        catalog,
        TreeKind::Catalog,
        &parent_key,
        &parent_inode.encode(),
    )?;
    transaction.catalog = catalog;
    // Nothing holds an unpinned small object open, so it goes now, in this
    // transaction, instead of as an orphan that maintenance removes later.
    if !pinned.contains(&target) && transaction.remove_small_object(target)? {
        return Ok(());
    }
    let catalog = transaction.catalog.clone();
    transaction.catalog = transaction.forest.put(
        catalog,
        TreeKind::Catalog,
        &target_key,
        &target_inode.encode(),
    )?;
    transaction.mark_orphan(target)?;
    Ok(())
}

fn rename_spelling<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    parent: u64,
    destination: &Name,
    now: Timestamp,
) -> Result<()> {
    let key = dirent_key(parent, &destination.folded);
    let value = transaction
        .forest
        .lookup(&transaction.catalog, TreeKind::Catalog, &key)?
        .ok_or(Error::NotFound)?;
    let mut dirent = DirentValue::decode(&value)?;
    dirent.stored_name = &destination.stored;
    let target_key = inode_key(dirent.target);
    let target_value = transaction
        .forest
        .lookup(&transaction.catalog, TreeKind::Catalog, &target_key)?
        .ok_or(Error::Corrupt("directory entry has no inode"))?;
    let mut target_inode = Inode::decode(&target_value)?;
    target_inode.metadata_change = now;

    let parent_key = inode_key(parent);
    let parent_value = transaction
        .forest
        .lookup(&transaction.catalog, TreeKind::Catalog, &parent_key)?
        .ok_or(Error::Corrupt("parent inode is missing"))?;
    let mut parent_inode = Inode::decode(&parent_value)?;
    parent_inode.modification = now;
    parent_inode.metadata_change = now;

    let mut catalog = transaction.catalog.clone();
    catalog = transaction
        .forest
        .put(catalog, TreeKind::Catalog, &key, &dirent.encode()?)?;
    catalog = transaction.forest.put(
        catalog,
        TreeKind::Catalog,
        &target_key,
        &target_inode.encode(),
    )?;
    catalog = transaction.forest.put(
        catalog,
        TreeKind::Catalog,
        &parent_key,
        &parent_inode.encode(),
    )?;
    transaction.catalog = catalog;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn rename_move<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    source_parent: u64,
    source: &Name,
    destination_parent: u64,
    destination: &Name,
    replace: Replace,
    now: Timestamp,
) -> Result<()> {
    let source_key = dirent_key(source_parent, &source.folded);
    let source_value = transaction
        .forest
        .lookup(&transaction.catalog, TreeKind::Catalog, &source_key)?
        .ok_or(Error::NotFound)?;
    let source_dirent = DirentValue::decode(&source_value)?;
    let moved = source_dirent.target;
    let moved_kind = source_dirent.kind;

    let destination_parent_key = inode_key(destination_parent);
    let destination_parent_value = transaction
        .forest
        .lookup(
            &transaction.catalog,
            TreeKind::Catalog,
            &destination_parent_key,
        )?
        .ok_or(Error::NotFound)?;
    let mut destination_parent_inode = Inode::decode(&destination_parent_value)?;
    if destination_parent_inode.kind != ObjectKind::Directory {
        return Err(Error::WrongType);
    }
    if destination_parent_inode.orphan {
        return Err(Error::InvalidInput(
            "cannot create a child in an orphaned directory",
        ));
    }

    // A live directory has live ancestors, so a missing one is corruption. The
    // catalog is unedited here, so every step reads a disk page and the
    // forest's read budget bounds the walk.
    if moved_kind == ObjectKind::Directory {
        let mut current = destination_parent;
        while current != ROOT.0 {
            if current == moved {
                return Err(Error::InvalidInput(
                    "cannot move a directory into itself or a descendant",
                ));
            }
            let value = transaction
                .forest
                .lookup(&transaction.catalog, TreeKind::Catalog, &inode_key(current))?
                .ok_or(Error::Corrupt("directory ancestor is missing"))?;
            current = Inode::decode(&value)?.parent;
        }
    }

    let destination_key = dirent_key(destination_parent, &destination.folded);
    let existing_destination =
        transaction
            .forest
            .lookup(&transaction.catalog, TreeKind::Catalog, &destination_key)?;

    let mut replaced_target = None;
    if let Some(existing) = &existing_destination {
        if replace == Replace::Forbidden {
            return Err(Error::NameCollision);
        }
        let existing_dirent = DirentValue::decode(existing)?;
        // Any non-directory replaces any non-directory, as POSIX rename allows.
        if (existing_dirent.kind == ObjectKind::Directory) != (moved_kind == ObjectKind::Directory)
        {
            return Err(Error::WrongType);
        }
        if existing_dirent.kind == ObjectKind::Directory {
            let existing_value = transaction
                .forest
                .lookup(
                    &transaction.catalog,
                    TreeKind::Catalog,
                    &inode_key(existing_dirent.target),
                )?
                .ok_or(Error::Corrupt("directory entry has no inode"))?;
            if Inode::decode(&existing_value)?.child_count != 0 {
                return Err(Error::DirectoryNotEmpty);
            }
        }
        replaced_target = Some(existing_dirent.target);
    }

    let source_parent_key = inode_key(source_parent);
    let source_parent_value = transaction
        .forest
        .lookup(&transaction.catalog, TreeKind::Catalog, &source_parent_key)?
        .ok_or(Error::Corrupt("parent inode is missing"))?;
    let mut source_parent_inode = Inode::decode(&source_parent_value)?;

    let moved_value = transaction
        .forest
        .lookup(&transaction.catalog, TreeKind::Catalog, &inode_key(moved))?
        .ok_or(Error::Corrupt("directory entry has no inode"))?;
    let mut moved_inode = Inode::decode(&moved_value)?;
    moved_inode.parent = destination_parent;
    moved_inode.metadata_change = now;

    let destination_parent_update = if source_parent == destination_parent {
        None
    } else {
        if existing_destination.is_none() {
            destination_parent_inode.child_count = destination_parent_inode
                .child_count
                .checked_add(1)
                .ok_or(Error::LimitExceeded)?;
        }
        destination_parent_inode.modification = now;
        destination_parent_inode.metadata_change = now;
        Some(destination_parent_inode)
    };

    source_parent_inode.child_count = source_parent_inode
        .child_count
        .checked_sub(1)
        .ok_or(Error::Corrupt("directory child count underflow"))?;
    if destination_parent_update.is_none() && existing_destination.is_none() {
        source_parent_inode.child_count = source_parent_inode
            .child_count
            .checked_add(1)
            .ok_or(Error::LimitExceeded)?;
    }
    source_parent_inode.modification = now;
    source_parent_inode.metadata_change = now;

    let mut catalog = transaction.catalog.clone();
    catalog = transaction
        .forest
        .remove(catalog, TreeKind::Catalog, &source_key)?;
    catalog = transaction.forest.put(
        catalog,
        TreeKind::Catalog,
        &inode_key(moved),
        &moved_inode.encode(),
    )?;
    let destination_dirent = DirentValue {
        target: moved,
        kind: moved_kind,
        stored_name: &destination.stored,
    }
    .encode()?;
    catalog = transaction.forest.put(
        catalog,
        TreeKind::Catalog,
        &destination_key,
        &destination_dirent,
    )?;
    catalog = transaction.forest.put(
        catalog,
        TreeKind::Catalog,
        &source_parent_key,
        &source_parent_inode.encode(),
    )?;
    if let Some(inode) = &destination_parent_update {
        catalog = transaction.forest.put(
            catalog,
            TreeKind::Catalog,
            &destination_parent_key,
            &inode.encode(),
        )?;
    }
    if let Some(target) = replaced_target {
        let target_key = inode_key(target);
        let target_value = transaction
            .forest
            .lookup(&transaction.catalog, TreeKind::Catalog, &target_key)?
            .ok_or(Error::Corrupt("directory entry has no inode"))?;
        let mut target_inode = Inode::decode(&target_value)?;
        target_inode.orphan = true;
        target_inode.metadata_change = now;
        catalog = transaction.forest.put(
            catalog,
            TreeKind::Catalog,
            &target_key,
            &target_inode.encode(),
        )?;
        transaction.catalog = catalog;
        transaction.mark_orphan(target)?;
    } else {
        transaction.catalog = catalog;
    }
    Ok(())
}

/// Create an empty object of `kind` under `parent` inside `transaction`. The
/// edit validates everything before it changes the catalog and assigns the
/// transaction's catalog root last, so a failure leaves the transaction as it
/// found it apart from unreachable drafts.
pub(super) fn create_in<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    parent: ObjectId,
    name: Name,
    kind: ObjectKind,
    target: &[u8],
    now: Timestamp,
) -> Result<ObjectId> {
    let parent_key = inode_key(parent.0);
    let parent_value = transaction
        .forest
        .lookup(&transaction.catalog, TreeKind::Catalog, &parent_key)?
        .ok_or(Error::NotFound)?;
    let mut parent_inode = Inode::decode(&parent_value)?;
    if parent_inode.kind != ObjectKind::Directory {
        return Err(Error::WrongType);
    }
    if parent_inode.orphan {
        return Err(Error::InvalidInput(
            "cannot create a child in an orphaned directory",
        ));
    }
    let dirent_key_bytes = dirent_key(parent.0, &name.folded);
    if transaction
        .forest
        .lookup(&transaction.catalog, TreeKind::Catalog, &dirent_key_bytes)?
        .is_some()
    {
        return Err(Error::NameCollision);
    }
    let child = transaction.new_object()?;
    let child_inode = Inode {
        kind,
        orphan: false,
        directory_link: None,
        parent: parent.0,
        size: target.len() as u64,
        allocated_blocks: 0,
        child_count: 0,
        creation: now,
        modification: now,
        metadata_change: now,
        target: target.to_vec(),
    };
    let dirent = DirentValue {
        target: child,
        kind,
        stored_name: &name.stored,
    }
    .encode()?;
    parent_inode.child_count = parent_inode
        .child_count
        .checked_add(1)
        .ok_or(Error::LimitExceeded)?;
    parent_inode.modification = now;
    parent_inode.metadata_change = now;

    let mut catalog = transaction.catalog.clone();
    catalog = transaction.forest.put(
        catalog,
        TreeKind::Catalog,
        &inode_key(child),
        &child_inode.encode(),
    )?;
    catalog = transaction
        .forest
        .put(catalog, TreeKind::Catalog, &dirent_key_bytes, &dirent)?;
    catalog = transaction.forest.put(
        catalog,
        TreeKind::Catalog,
        &parent_key,
        &parent_inode.encode(),
    )?;
    transaction.catalog = catalog;
    Ok(ObjectId(child))
}

fn make_symlink_in<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    parent: u64,
    name: &Name,
    target: &[u8],
    now: Timestamp,
) -> Result<()> {
    let entry_key = dirent_key(parent, &name.folded);
    let entry_value = transaction
        .forest
        .lookup(&transaction.catalog, TreeKind::Catalog, &entry_key)?
        .ok_or(Error::NotFound)?;
    let mut entry = DirentValue::decode(&entry_value)?;
    let key = inode_key(entry.target);
    let value = transaction
        .forest
        .lookup(&transaction.catalog, TreeKind::Catalog, &key)?
        .ok_or(Error::NotFound)?;
    let mut inode = Inode::decode(&value)?;
    // An empty file has no extents or checksums, so only the inode changes.
    let empty = match inode.kind {
        ObjectKind::File => inode.size == 0 && inode.allocated_blocks == 0,
        ObjectKind::Directory => inode.child_count == 0,
        ObjectKind::Symlink => false,
    };
    if !empty {
        return Err(Error::WrongType);
    }
    entry.kind = ObjectKind::Symlink;
    // Windows keeps directory and file links apart and asks for the one it created.
    inode.directory_link = Some(inode.kind == ObjectKind::Directory);
    inode.kind = ObjectKind::Symlink;
    inode.size = target.len() as u64;
    inode.target = target.to_vec();
    inode.modification = now;
    inode.metadata_change = now;

    let mut catalog = transaction.catalog.clone();
    catalog = transaction
        .forest
        .put(catalog, TreeKind::Catalog, &key, &inode.encode())?;
    catalog = transaction
        .forest
        .put(catalog, TreeKind::Catalog, &entry_key, &entry.encode()?)?;
    transaction.catalog = catalog;
    Ok(())
}
