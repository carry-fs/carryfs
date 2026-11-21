//! Mounted volumes and open files, addressed by plain integers so the JNI
//! layer can pass them to Kotlin.

use std::collections::{BTreeMap, HashMap};

use carryfs_core::{
    Access, BlockDevice, DirectoryEntry, Error, Handle, MountMode, Name, ObjectId, ObjectKind,
    ROOT, RemoveKind, Replace, Result, Timestamp, Volume,
};

use crate::disk::{Partition, SharedDisk, volumes};

/// A file or directory as the documents provider shows it.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: i64,
    /// The directory holding it; the root is its own parent.
    pub parent: i64,
    pub name: String,
    pub directory: bool,
    pub size: u64,
    pub modified_ms: i64,
}

struct Mounted {
    drive: i64,
    read_only: bool,
    volume: Volume<Partition>,
    files: HashMap<i64, Open>,
    /// The writable range of a CarryFS 0.1.0 volume, mounted read-only until upgraded.
    legacy: Option<Partition>,
}

impl Mounted {
    fn open(drive: i64, partition: Partition) -> Result<Self> {
        let write_protected = partition.mode() == MountMode::ReadOnly;
        let (legacy, mut volume) = match Volume::open(partition.clone()) {
            Err(Error::NeedsUpgrade) => (
                Some(partition.clone()),
                Volume::open(partition.read_only())?,
            ),
            volume => (None, volume?),
        };
        let read_only = write_protected || legacy.is_some();
        if read_only {
            volume.enable_readonly_metadata_cache()?;
        } else {
            volume.set_deferred_checkpoints(true)?;
        }
        Ok(Self {
            drive,
            read_only,
            volume,
            files: HashMap::new(),
            legacy,
        })
    }
}

struct Open {
    handle: Handle,
    written: bool,
}

/// Every mounted volume. Checkpoints are deferred: changes publish when a
/// written file closes, on fsync, on eject, and on the maintenance tick.
#[derive(Default)]
pub struct Session {
    volumes: BTreeMap<i64, Mounted>,
    next: i64,
    mount_warnings: BTreeMap<i64, String>,
}

impl Session {
    /// Mount every CarryFS volume on a disk. `drive` is the caller's key for
    /// the disk, used to eject or detach it.
    pub fn mount(&mut self, drive: i64, disk: SharedDisk) -> Result<Vec<i64>> {
        if self.volumes.values().any(|mounted| mounted.drive == drive) {
            return Err(Error::Busy);
        }
        let partitions = volumes(&disk).map_err(|source| Error::Io {
            operation: "read partition table",
            source,
        })?;
        let mut opened = Vec::new();
        let mut warnings = Vec::new();
        if partitions.iter().any(|partition| partition.degraded_gpt) {
            warnings.push("One GPT copy is damaged. Volumes are mounted read-only.".to_owned());
        }
        for (index, partition) in partitions.into_iter().enumerate() {
            match Mounted::open(drive, partition) {
                Ok(mounted) => opened.push(mounted),
                Err(error) => warnings.push(format!(
                    "CarryFS volume {} could not be opened: {error}",
                    index + 1
                )),
            }
        }
        if opened.is_empty() && !warnings.is_empty() {
            return Err(Error::Io {
                operation: "mount CarryFS volumes",
                source: std::io::Error::other(warnings.join(" ")),
            });
        }
        if !warnings.is_empty() {
            self.mount_warnings.insert(drive, warnings.join(" "));
        }
        Ok(opened
            .into_iter()
            .map(|mounted| {
                self.next += 1;
                self.volumes.insert(self.next, mounted);
                self.next
            })
            .collect())
    }

    pub fn take_mount_warning(&mut self, drive: i64) -> String {
        self.mount_warnings.remove(&drive).unwrap_or_default()
    }

    /// Close every file, publish, and release the drive's volumes. All are
    /// released even if one fails; the first error is returned.
    pub fn eject(&mut self, drive: i64) -> Result<()> {
        let mut result = Ok(());
        for mut mounted in self.take(drive) {
            for (_, open) in mounted.files.drain() {
                result = result.and(mounted.volume.close_handle(open.handle));
            }
            result = result.and(mounted.volume.eject());
        }
        result
    }

    /// Forget a drive that is already gone. Unpublished changes are lost.
    pub fn detach(&mut self, drive: i64) {
        self.take(drive);
    }

    fn take(&mut self, drive: i64) -> Vec<Mounted> {
        let ids: Vec<i64> = self
            .volumes
            .iter()
            .filter(|(_, mounted)| mounted.drive == drive)
            .map(|(id, _)| *id)
            .collect();
        ids.iter()
            .filter_map(|id| self.volumes.remove(id))
            .collect()
    }

    pub fn label(&mut self, volume: i64) -> Result<String> {
        let info = self.volume(volume)?.info()?;
        Ok(info
            .label
            .map_or_else(|| "CarryFS".to_owned(), |label| label.as_str().to_owned()))
    }

    /// Available and total bytes.
    pub fn space(&mut self, volume: i64) -> Result<(u64, u64)> {
        let space = self.volume(volume)?.space()?;
        let block = u64::from(space.block_bytes);
        Ok((space.available_blocks * block, space.total_blocks * block))
    }

    pub fn read_only(&mut self, volume: i64) -> Result<bool> {
        Ok(self.mounted_mut(volume)?.read_only)
    }

    /// Whether CarryFS 0.1.0 formatted the volume, so it takes writes only after `upgrade`.
    pub fn needs_upgrade(&mut self, volume: i64) -> Result<bool> {
        Ok(self.mounted_mut(volume)?.legacy.is_some())
    }

    /// Upgrade a CarryFS 0.1.0 volume and remount it writable under the same id.
    /// CarryFS 0.1.0 can't mount it afterwards.
    pub fn upgrade(&mut self, volume: i64) -> Result<()> {
        let mounted = self.mounted_mut(volume)?;
        let partition = mounted
            .legacy
            .clone()
            .ok_or(Error::InvalidInput("the volume is already up to date"))?;
        if !mounted.files.is_empty() {
            return Err(Error::Busy);
        }
        Volume::upgrade(partition.clone())?;
        let mut writable = Volume::open(partition)?;
        writable.set_deferred_checkpoints(true)?;
        let mut old = std::mem::replace(&mut mounted.volume, writable);
        mounted.read_only = false;
        mounted.legacy = None;
        old.eject()
    }

    pub fn list(&mut self, volume: i64, directory: i64) -> Result<Vec<Entry>> {
        let volume = self.volume(volume)?;
        let mut listed = Vec::new();
        // Documents have no symlinks; they stay reachable from desktops.
        for child in entries(volume, object(directory)?)? {
            if child.kind != ObjectKind::Symlink {
                let mut listed_entry = entry(volume, child.object, &child.name)?;
                // A file's stored parent is a hint; here the parent is known.
                listed_entry.parent = directory;
                listed.push(listed_entry);
            }
        }
        Ok(listed)
    }

    pub fn stat(&mut self, volume: i64, id: i64) -> Result<Entry> {
        let object = object(id)?;
        if object == ROOT {
            let name = Name::parse(&self.label(volume)?)?;
            return entry(self.volume(volume)?, ROOT, &name);
        }
        let volume = self.volume(volume)?;
        let name = name_of(volume, object)?;
        entry(volume, object, &name)
    }

    /// Create a file or directory, adding " (n)" to the name if it is taken.
    pub fn create(&mut self, volume: i64, parent: i64, name: &str, directory: bool) -> Result<i64> {
        let volume = self.volume(volume)?;
        let parent = object(parent)?;
        for attempt in 0..100 {
            let name = Name::parse(&numbered(name, attempt))?;
            let created = if directory {
                with_space(volume, |v| v.create_directory(parent, name.clone()))
            } else {
                with_space(volume, |v| v.create_file(parent, name.clone()))
                    .and_then(|handle| volume.close_handle(handle))
                    .and_then(|()| volume.lookup(parent, &name))
            };
            match created {
                Err(Error::NameCollision) => continue,
                created => return created.map(|object| object.get() as i64),
            }
        }
        Err(Error::NameCollision)
    }

    pub fn rename(&mut self, volume: i64, id: i64, name: &str) -> Result<()> {
        let volume = self.volume(volume)?;
        let object = object(id)?;
        let parent = volume.attributes(object)?.parent;
        let old = name_of(volume, object)?;
        let new = Name::parse(name)?;
        with_space(volume, |v| {
            v.rename(parent, &old, parent, new.clone(), Replace::Forbidden)
        })
    }

    pub fn move_to(&mut self, volume: i64, id: i64, from: i64, to: i64) -> Result<()> {
        let volume = self.volume(volume)?;
        let (object, from, to) = (object(id)?, object(from)?, object(to)?);
        let name = find(volume, from, object)?;
        with_space(volume, |v| {
            v.rename(from, &name, to, name.clone(), Replace::Forbidden)
        })
    }

    /// Delete a file, or a directory and everything in it.
    pub fn delete(&mut self, volume: i64, id: i64) -> Result<()> {
        let volume = self.volume(volume)?;
        let object = object(id)?;
        let attributes = volume.attributes(object)?;
        let name = name_of(volume, object)?;
        remove_tree(volume, attributes.parent, &name, object, attributes.kind)
    }

    pub fn open(&mut self, volume: i64, id: i64, write: bool, truncate: bool) -> Result<i64> {
        let access = if write {
            Access::ReadWrite
        } else {
            Access::Read
        };
        let mounted = self.mounted_mut(volume)?;
        let handle = mounted.volume.open_object(object(id)?, access)?;
        if truncate && let Err(error) = with_space(&mut mounted.volume, |v| v.truncate(&handle, 0))
        {
            mounted.volume.close_handle(handle)?;
            return Err(error);
        }
        self.next += 1;
        let file = self.next;
        self.mounted_mut(volume)?.files.insert(
            file,
            Open {
                handle,
                written: truncate,
            },
        );
        Ok(file)
    }

    pub fn read(&mut self, volume: i64, file: i64, offset: u64, out: &mut [u8]) -> Result<usize> {
        let mounted = self.mounted_mut(volume)?;
        let open = mounted.files.get(&file).ok_or(Error::NotFound)?;
        let mut done = 0;
        while done < out.len() {
            let read = mounted
                .volume
                .read(&open.handle, offset + done as u64, &mut out[done..])?;
            if read == 0 {
                break;
            }
            done += read;
        }
        Ok(done)
    }

    pub fn write(&mut self, volume: i64, file: i64, offset: u64, data: &[u8]) -> Result<()> {
        let mounted = self.mounted_mut(volume)?;
        let open = mounted.files.get_mut(&file).ok_or(Error::NotFound)?;
        open.written = true;
        let mut done = 0;
        while done < data.len() {
            done += with_space(&mut mounted.volume, |v| {
                v.write(&open.handle, offset + done as u64, &data[done..])
            })?;
        }
        Ok(())
    }

    pub fn size(&mut self, volume: i64, file: i64) -> Result<u64> {
        let mounted = self.mounted_mut(volume)?;
        let open = mounted.files.get(&file).ok_or(Error::NotFound)?;
        Ok(mounted.volume.handle_attributes(&open.handle)?.size)
    }

    pub fn sync(&mut self, volume: i64) -> Result<()> {
        self.volume(volume)?.sync()
    }

    /// Close a file, publishing it if it was written. Apps release files
    /// asynchronously, so a close can arrive after its volume was ejected,
    /// which already closed and published it, or detached; that is no error.
    pub fn close(&mut self, volume: i64, file: i64) -> Result<()> {
        let Some(mounted) = self.volumes.get_mut(&volume) else {
            return Ok(());
        };
        let open = mounted.files.remove(&file).ok_or(Error::NotFound)?;
        mounted.volume.close_handle(open.handle)?;
        if open.written {
            mounted.volume.sync()?;
        }
        Ok(())
    }

    /// Publish pending changes and run bounded cleanup on every volume.
    /// Returns the failures, which also fault the volume they came from.
    pub fn maintain(&mut self) -> Vec<Error> {
        let mut failures = Vec::new();
        for mounted in self.volumes.values_mut() {
            let volume = &mut mounted.volume;
            if mounted.read_only || volume.is_faulted() {
                continue;
            }
            let step = if volume.has_unsynced_changes() {
                volume.sync()
            } else {
                volume
                    .has_cleanup_work()
                    .and_then(|work| if work { volume.sync() } else { Ok(()) })
            };
            if let Err(error) = step {
                failures.push(error);
            }
        }
        failures
    }

    fn mounted_mut(&mut self, volume: i64) -> Result<&mut Mounted> {
        self.volumes.get_mut(&volume).ok_or(Error::Ejected)
    }

    fn volume(&mut self, volume: i64) -> Result<&mut Volume<Partition>> {
        Ok(&mut self.mounted_mut(volume)?.volume)
    }
}

fn object(id: i64) -> Result<ObjectId> {
    ObjectId::new(u64::try_from(id).map_err(|_| Error::InvalidInput("object ID is negative"))?)
}

/// `name` for the first attempt, then "name (n).ext".
fn numbered(name: &str, attempt: u32) -> String {
    if attempt == 0 {
        return name.to_owned();
    }
    match name.rfind('.').filter(|dot| *dot > 0) {
        Some(dot) => format!("{} ({attempt}){}", &name[..dot], &name[dot..]),
        None => format!("{name} ({attempt})"),
    }
}

fn entry(volume: &mut Volume<Partition>, object: ObjectId, name: &Name) -> Result<Entry> {
    let attributes = volume.attributes(object)?;
    Ok(Entry {
        id: object.get() as i64,
        parent: attributes.parent.get() as i64,
        name: String::from_utf8_lossy(name.stored()).into_owned(),
        directory: attributes.kind == ObjectKind::Directory,
        size: attributes.size,
        modified_ms: millis(attributes.modification),
    })
}

fn millis(time: Timestamp) -> i64 {
    time.seconds()
        .saturating_mul(1000)
        .saturating_add(i64::from(time.nanos() / 1_000_000))
}

fn entries(volume: &mut Volume<Partition>, directory: ObjectId) -> Result<Vec<DirectoryEntry>> {
    let handle = volume.open_directory(directory)?;
    let mut listed = Vec::new();
    let mut cursor = None;
    let result = loop {
        match volume.read_directory(&handle, cursor.as_ref()) {
            Ok(page) => {
                listed.extend(page.entries);
                cursor = page.cursor;
                if cursor.is_none() {
                    break Ok(listed);
                }
            }
            Err(error) => break Err(error),
        }
    };
    volume.close_handle(handle)?;
    result
}

/// The name of `object` in its parent directory.
fn name_of(volume: &mut Volume<Partition>, object: ObjectId) -> Result<Name> {
    let parent = volume.attributes(object)?.parent;
    find(volume, parent, object)
}

// ponytail: scans the whole parent; cache names if huge folders get slow.
fn find(volume: &mut Volume<Partition>, parent: ObjectId, object: ObjectId) -> Result<Name> {
    entries(volume, parent)?
        .into_iter()
        .find(|entry| entry.object == object)
        .map(|entry| entry.name)
        .ok_or(Error::NotFound)
}

fn remove_tree(
    volume: &mut Volume<Partition>,
    parent: ObjectId,
    name: &Name,
    object: ObjectId,
    kind: ObjectKind,
) -> Result<()> {
    if kind != ObjectKind::Directory {
        return volume.remove(parent, name, RemoveKind::File);
    }
    for child in entries(volume, object)? {
        remove_tree(volume, object, &child.name, child.object, child.kind)?;
    }
    volume.remove(parent, name, RemoveKind::Directory)
}

/// Retry once after publishing and draining cleanup, which frees space.
fn with_space<T>(
    volume: &mut Volume<Partition>,
    mut operation: impl FnMut(&mut Volume<Partition>) -> Result<T>,
) -> Result<T> {
    match operation(volume) {
        Err(Error::NoSpace) => {
            // The cap only guards against a core that never quiesces.
            for _ in 0..1_000_000 {
                volume.sync()?;
                if !volume.has_cleanup_work()? {
                    break;
                }
            }
            operation(volume)
        }
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::tests::{MIB, format, usb_disk, write_gpt};

    fn mounted() -> (Session, i64, SharedDisk) {
        let disk = usb_disk(400 * MIB);
        write_gpt(&disk, &[(MIB, 300 * MIB)]);
        format(&disk, MIB, 300 * MIB);
        let mut session = Session::default();
        let volumes = session.mount(7, disk.clone()).unwrap();
        assert_eq!(volumes.len(), 1);
        (session, volumes[0], disk)
    }

    /// Rewrite both roots as CarryFS 0.1.0 wrote them.
    fn make_legacy(disk: &SharedDisk, start: u64, len: u64) {
        use carryfs_ondisk::format::{LEGACY_REVISION, Root};
        let blocks = len / 4096;
        let mut disk = disk.lock().unwrap();
        for slot in [256, blocks - 257] {
            let offset = start + slot * 4096;
            let mut page = [0; 4096];
            disk.read_at(offset, &mut page).unwrap();
            let mut root = Root::decode(&page, slot, blocks).unwrap();
            root.revision = LEGACY_REVISION;
            root.encode(slot, blocks, &mut page).unwrap();
            disk.write_at(offset, &page).unwrap();
        }
    }

    #[test]
    fn a_legacy_volume_mounts_read_only_until_upgraded() {
        let disk = usb_disk(400 * MIB);
        write_gpt(&disk, &[(MIB, 300 * MIB)]);
        format(&disk, MIB, 300 * MIB);
        make_legacy(&disk, MIB, 300 * MIB);
        let mut session = Session::default();
        let root = ROOT.get() as i64;
        let volume = session.mount(7, disk.clone()).unwrap()[0];
        assert!(session.read_only(volume).unwrap());
        assert!(session.needs_upgrade(volume).unwrap());
        assert!(session.create(volume, root, "a.txt", false).is_err());

        session.upgrade(volume).unwrap();
        assert!(!session.read_only(volume).unwrap());
        assert!(!session.needs_upgrade(volume).unwrap());
        session.create(volume, root, "a.txt", false).unwrap();
        session.eject(7).unwrap();

        let volume = session.mount(7, disk).unwrap()[0];
        assert!(!session.needs_upgrade(volume).unwrap());
        assert_eq!(names(&mut session, volume, root), ["a.txt"]);
    }

    fn names(session: &mut Session, volume: i64, directory: i64) -> Vec<String> {
        let mut names: Vec<_> = session
            .list(volume, directory)
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        names.sort();
        names
    }

    #[test]
    fn files_survive_eject_and_remount() {
        let (mut session, volume, disk) = mounted();
        let root = ROOT.get() as i64;
        let photos = session.create(volume, root, "Photos", true).unwrap();
        let file = session.create(volume, photos, "a.jpg", false).unwrap();
        let open = session.open(volume, file, true, false).unwrap();
        let data: Vec<u8> = (0..3 * MIB as usize).map(|i| (i % 251) as u8).collect();
        session.write(volume, open, 0, &data).unwrap();
        assert_eq!(session.size(volume, open).unwrap(), data.len() as u64);
        let late = session.open(volume, file, false, false).unwrap();
        session.close(volume, open).unwrap();
        session.eject(7).unwrap();
        assert!(session.list(volume, root).is_err());
        session.close(volume, late).unwrap();

        let volume = session.mount(8, disk).unwrap()[0];
        assert_eq!(names(&mut session, volume, root), ["Photos"]);
        let found = session.stat(volume, file).unwrap();
        assert_eq!(
            (found.name.as_str(), found.size),
            ("a.jpg", data.len() as u64)
        );
        let open = session.open(volume, file, false, false).unwrap();
        let mut back = vec![0; data.len() + 10];
        assert_eq!(
            session.read(volume, open, 0, &mut back).unwrap(),
            data.len()
        );
        assert_eq!(&back[..data.len()], &data[..]);
    }

    #[test]
    fn a_damaged_partition_does_not_hide_a_healthy_volume() {
        let disk = usb_disk(650 * MIB);
        let size = 300 * MIB;
        let damaged = 301 * MIB;
        write_gpt(&disk, &[(MIB, size), (damaged, size)]);
        format(&disk, MIB, size);
        format(&disk, damaged, size);
        {
            let mut disk = disk.lock().unwrap();
            for offset in [damaged + MIB, damaged + size - 257 * 4096] {
                let mut page = [0; 4096];
                disk.read_at(offset, &mut page).unwrap();
                page[24] ^= 1; // Retain the volume magic, invalidate the root checksum.
                disk.write_at(offset, &page).unwrap();
            }
        }
        let mut session = Session::default();
        let volumes = session.mount(7, disk).unwrap();
        assert_eq!(volumes.len(), 1);
        assert!(
            session
                .take_mount_warning(7)
                .contains("volume 2 could not be opened")
        );
        assert!(session.label(volumes[0]).is_ok());
    }

    #[test]
    fn create_numbers_taken_names() {
        let (mut session, volume, _) = mounted();
        let root = ROOT.get() as i64;
        for _ in 0..3 {
            session.create(volume, root, "notes.txt", false).unwrap();
        }
        session
            .create(volume, root, "Notes.TXT (x)", false)
            .unwrap();
        assert_eq!(
            names(&mut session, volume, root),
            [
                "Notes.TXT (x)",
                "notes (1).txt",
                "notes (2).txt",
                "notes.txt"
            ]
        );
    }

    #[test]
    fn rename_move_and_recursive_delete() {
        let (mut session, volume, _) = mounted();
        let root = ROOT.get() as i64;
        let a = session.create(volume, root, "a", true).unwrap();
        let b = session.create(volume, a, "b", true).unwrap();
        let file = session.create(volume, b, "f.txt", false).unwrap();

        session.rename(volume, file, "g.txt").unwrap();
        session.move_to(volume, file, b, root).unwrap();
        assert_eq!(names(&mut session, volume, root), ["a", "g.txt"]);
        assert_eq!(session.stat(volume, file).unwrap().name, "g.txt");

        session.delete(volume, a).unwrap();
        assert_eq!(names(&mut session, volume, root), ["g.txt"]);
        assert!(matches!(session.stat(volume, b), Err(Error::NotFound)));
    }

    #[test]
    fn truncating_open_and_maintenance_publish() {
        let (mut session, volume, _) = mounted();
        let root = ROOT.get() as i64;
        let file = session.create(volume, root, "f", false).unwrap();
        let open = session.open(volume, file, true, false).unwrap();
        session.write(volume, open, 0, b"hello world").unwrap();
        session.close(volume, open).unwrap();
        let open = session.open(volume, file, true, true).unwrap();
        session.write(volume, open, 0, b"bye").unwrap();
        assert!(session.maintain().is_empty());
        assert_eq!(session.size(volume, open).unwrap(), 3);
        session.close(volume, open).unwrap();
        assert_eq!(session.label(volume).unwrap(), "CarryFS");
        let (available, total) = session.space(volume).unwrap();
        assert!(available > 0 && available < total);
    }
}
