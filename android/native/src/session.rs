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
    // Object IDs are positive. Give each hard-link name its own negative ID.
    links: HashMap<i64, LinkName>,
    link_ids: BTreeMap<(ObjectId, Vec<u8>), i64>,
    next_link: i64,
    /// The writable range of a CarryFS 0.1.0 volume, mounted read-only until upgraded.
    legacy: Option<Partition>,
}

#[derive(Clone)]
struct LinkName {
    object: ObjectId,
    parent: ObjectId,
    name: Name,
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
            links: HashMap::new(),
            link_ids: BTreeMap::new(),
            next_link: 0,
            legacy,
        })
    }
}

struct Open {
    handle: Handle,
    written: bool,
    writable: bool,
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

    /// Keep all volumes and handles registered until the drive is safe to remove.
    pub fn eject(&mut self, drive: i64) -> Result<()> {
        let volumes = || {
            self.volumes
                .values()
                .filter(|mounted| mounted.drive == drive)
        };
        if volumes().any(|mounted| mounted.files.values().any(|file| file.writable)) {
            return Err(Error::Busy);
        }
        for mounted in self
            .volumes
            .values_mut()
            .filter(|mounted| mounted.drive == drive)
        {
            mounted.volume.sync()?;
        }
        self.detach(drive);
        Ok(())
    }

    /// Forget a drive that is already gone. Unpublished changes are lost.
    pub fn detach(&mut self, drive: i64) {
        self.volumes.retain(|_, mounted| mounted.drive != drive);
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

    #[cfg(test)]
    fn list(&mut self, volume: i64, directory: i64) -> Result<Vec<Entry>> {
        let mounted = self.mounted_mut(volume)?;
        let parent = object(directory)?;
        let children = entries(&mut mounted.volume, parent)?;
        Self::describe_children(mounted, parent, children)
    }

    pub fn list_page(
        &mut self,
        volume: i64,
        directory: i64,
        after: Option<Name>,
    ) -> Result<(Vec<Entry>, Option<Name>)> {
        let mounted = self.mounted_mut(volume)?;
        let parent = object(directory)?;
        let handle = mounted.volume.open_directory(parent)?;
        let page = if let Some(after) = &after {
            mounted.volume.read_directory_after(&handle, after)
        } else {
            mounted.volume.read_directory(&handle, None)
        };
        mounted.volume.close_handle(handle)?;
        let page = page?;
        let next = page
            .cursor
            .and_then(|_| page.entries.last().map(|entry| entry.name.clone()));
        let entries = Self::describe_children(mounted, parent, page.entries)?;
        Ok((entries, next))
    }

    fn describe_children(
        mounted: &mut Mounted,
        parent: ObjectId,
        children: Vec<DirectoryEntry>,
    ) -> Result<Vec<Entry>> {
        let mut listed = Vec::new();
        // Documents have no symlinks; they stay reachable from desktops.
        for child in children {
            if child.kind != ObjectKind::Symlink {
                let (mut entry, links) = entry(&mut mounted.volume, child.object, &child.name)?;
                entry.parent = parent.get() as i64;
                let key = (parent, child.name.stored().to_vec());
                if let Some(&id) = mounted.link_ids.get(&key) {
                    entry.id = id;
                } else if links > 1 && child.kind == ObjectKind::File {
                    mounted.next_link -= 1;
                    let id = mounted.next_link;
                    mounted.link_ids.insert(key, id);
                    mounted.links.insert(
                        id,
                        LinkName {
                            object: child.object,
                            parent,
                            name: child.name,
                        },
                    );
                    entry.id = id;
                }
                listed.push(entry);
            }
        }
        Ok(listed)
    }

    pub fn stat(&mut self, volume: i64, id: i64) -> Result<Entry> {
        if id == ROOT.get() as i64 {
            let name = Name::parse(&self.label(volume)?)?;
            return entry(self.volume(volume)?, ROOT, &name).map(|(entry, _)| entry);
        }
        let mounted = self.mounted_mut(volume)?;
        let link = mounted.document_name(id)?;
        let (mut entry, _) = entry(&mut mounted.volume, link.object, &link.name)?;
        entry.id = id;
        entry.parent = link.parent.get() as i64;
        Ok(entry)
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
        let mounted = self.mounted_mut(volume)?;
        let link = mounted.document_name(id)?;
        let new = Name::parse(name)?;
        mounted.rename(id, &link, link.parent, new)
    }

    pub fn move_to(&mut self, volume: i64, id: i64, from: i64, to: i64) -> Result<()> {
        let mounted = self.mounted_mut(volume)?;
        let link = mounted.document_name(id)?;
        if link.parent != object(from)? {
            return Err(Error::NotFound);
        }
        mounted.rename(id, &link, object(to)?, link.name.clone())
    }

    /// Delete a file, or a directory and everything in it.
    pub fn delete(&mut self, volume: i64, id: i64) -> Result<()> {
        let mounted = self.mounted_mut(volume)?;
        let link = mounted.document_name(id)?;
        let kind = mounted.volume.attributes(link.object)?.kind;
        mounted.remove_tree(link.parent, &link.name, link.object, kind)
    }

    pub fn open(&mut self, volume: i64, id: i64, write: bool, truncate: bool) -> Result<i64> {
        let access = if write {
            Access::ReadWrite
        } else {
            Access::Read
        };
        let mounted = self.mounted_mut(volume)?;
        let object = if id < 0 {
            mounted.document_name(id)?.object
        } else {
            object(id)?
        };
        let handle = mounted.volume.open_object(object, access)?;
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
                writable: write,
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

impl Mounted {
    fn document_name(&mut self, id: i64) -> Result<LinkName> {
        if id < 0 {
            let link = self.links.get(&id).cloned().ok_or(Error::NotFound)?;
            if self.volume.lookup(link.parent, &link.name)? != link.object {
                return Err(Error::NotFound);
            }
            return Ok(link);
        }
        let object = object(id)?;
        let attributes = self.volume.attributes(object)?;
        if attributes.kind != ObjectKind::Directory && attributes.links > 1 {
            return Err(Error::NotFound);
        }
        let name = find(&mut self.volume, attributes.parent, object)?;
        Ok(LinkName {
            object,
            parent: attributes.parent,
            name,
        })
    }

    fn rename(&mut self, id: i64, link: &LinkName, parent: ObjectId, name: Name) -> Result<()> {
        with_space(&mut self.volume, |v| {
            v.rename(
                link.parent,
                &link.name,
                parent,
                name.clone(),
                Replace::Forbidden,
            )
        })?;
        if id < 0 {
            self.link_ids
                .remove(&(link.parent, link.name.stored().to_vec()));
            self.link_ids.insert((parent, name.stored().to_vec()), id);
            self.links.insert(
                id,
                LinkName {
                    object: link.object,
                    parent,
                    name,
                },
            );
        }
        Ok(())
    }

    fn remove_tree(
        &mut self,
        parent: ObjectId,
        name: &Name,
        object: ObjectId,
        kind: ObjectKind,
    ) -> Result<()> {
        if kind == ObjectKind::Directory {
            for child in entries(&mut self.volume, object)? {
                self.remove_tree(object, &child.name, child.object, child.kind)?;
            }
            return self.volume.remove(parent, name, RemoveKind::Directory);
        }
        self.volume.remove(parent, name, RemoveKind::File)?;
        if let Some(id) = self.link_ids.remove(&(parent, name.stored().to_vec())) {
            self.links.remove(&id);
        }
        Ok(())
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

fn entry(volume: &mut Volume<Partition>, object: ObjectId, name: &Name) -> Result<(Entry, u32)> {
    let attributes = volume.attributes(object)?;
    Ok((
        Entry {
            id: object.get() as i64,
            parent: attributes.parent.get() as i64,
            name: String::from_utf8_lossy(name.stored()).into_owned(),
            directory: attributes.kind == ObjectKind::Directory,
            size: attributes.size,
            modified_ms: millis(attributes.modification),
        },
        attributes.links,
    ))
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

fn find(volume: &mut Volume<Partition>, parent: ObjectId, object: ObjectId) -> Result<Name> {
    let handle = volume.open_directory(parent)?;
    let result = (|| {
        let mut cursor = None;
        loop {
            let page = volume.read_directory(&handle, cursor.as_ref())?;
            if let Some(entry) = page
                .entries
                .into_iter()
                .find(|entry| entry.object == object)
            {
                return Ok(entry.name);
            }
            cursor = page.cursor;
            if cursor.is_none() {
                return Err(Error::NotFound);
            }
        }
    })();
    volume.close_handle(handle)?;
    result
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
    fn refuses_eject_while_writers_are_open() {
        let (mut session, volume, _) = mounted();
        let doc = session.create(volume, 1, "busy", false).unwrap();
        let handle = session.open(volume, doc, true, false).unwrap();
        let drive = session.mounted_mut(volume).unwrap().drive;
        assert!(matches!(session.eject(drive), Err(Error::Busy)));
        session.write(volume, handle, 0, b"still open").unwrap();
        session.close(volume, handle).unwrap();
        session.eject(drive).unwrap();
    }

    #[test]
    fn failed_eject_keeps_the_drive_and_its_volumes_mounted() {
        use crate::scsi::{Disk, fake::FakeUsb};
        use std::sync::{Arc, Mutex};

        let transport = FakeUsb::new(512, 650 * MIB);
        let media = transport.media.clone();
        let disk: SharedDisk = Arc::new(Mutex::new(Disk::open(transport).unwrap()));
        write_gpt(&disk, &[(MIB, 300 * MIB), (301 * MIB, 300 * MIB)]);
        format(&disk, MIB, 300 * MIB);
        format(&disk, 301 * MIB, 300 * MIB);
        let mut session = Session::default();
        let volumes = session.mount(7, disk.clone()).unwrap();
        assert_eq!(volumes.len(), 2);
        session
            .create(volumes[0], ROOT.get() as i64, "pending", false)
            .unwrap();

        media.lock().unwrap().fail_flush = true;
        assert!(matches!(session.eject(7), Err(Error::Io { .. })));
        assert!(matches!(session.mount(7, disk.clone()), Err(Error::Busy)));
        assert!(session.volume(volumes[0]).unwrap().is_faulted());
        assert!(session.read_only(volumes[1]).is_ok());

        session.detach(7);
        media.lock().unwrap().fail_flush = false;
        assert_eq!(session.mount(7, disk).unwrap().len(), 2);
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
    fn directory_pages_preserve_entries_at_page_boundaries() {
        let (mut session, volume, _) = mounted();
        for index in 0..70 {
            session
                .create(volume, 1, &format!("file-{index:02}"), false)
                .unwrap();
        }
        let (first, marker) = session.list_page(volume, 1, None).unwrap();
        assert_eq!(first.len(), 63);
        let (second, marker) = session.list_page(volume, 1, marker).unwrap();
        assert_eq!(second.len(), 7);
        assert!(marker.is_none());
        let names: Vec<_> = first
            .into_iter()
            .chain(second)
            .map(|entry| entry.name)
            .collect();
        assert_eq!(
            names,
            (0..70)
                .map(|index| format!("file-{index:02}"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn hard_link_documents_keep_their_own_names_and_parents() {
        let (mut session, volume, _) = mounted();
        let root = ROOT.get() as i64;
        let a = session.create(volume, root, "a", true).unwrap();
        let b = session.create(volume, root, "b", true).unwrap();
        let file = session.create(volume, a, "original", false).unwrap();
        let disk = session.volume(volume).unwrap();
        for (parent, name) in [(a, "sibling"), (b, "other")] {
            disk.link(
                object(file).unwrap(),
                object(parent).unwrap(),
                Name::parse(name).unwrap(),
            )
            .unwrap();
        }
        let id_for = |session: &mut Session, parent, name| {
            session
                .list(volume, parent)
                .unwrap()
                .into_iter()
                .find(|entry| entry.name == name)
                .unwrap()
                .id
        };
        let original = id_for(&mut session, a, "original");
        let sibling = id_for(&mut session, a, "sibling");
        let other = id_for(&mut session, b, "other");
        assert_ne!(original, sibling);
        assert_ne!(sibling, other);
        assert_eq!(session.stat(volume, original).unwrap().parent, a);
        assert_eq!(session.stat(volume, other).unwrap().name, "other");
        let open = session.open(volume, sibling, true, false).unwrap();
        session.write(volume, open, 0, b"shared").unwrap();
        session.close(volume, open).unwrap();
        assert_eq!(session.stat(volume, other).unwrap().size, 6);

        session.rename(volume, sibling, "renamed").unwrap();
        session.move_to(volume, sibling, a, b).unwrap();
        assert_eq!(session.stat(volume, sibling).unwrap().parent, b);
        assert_eq!(id_for(&mut session, b, "renamed"), sibling);
        session.delete(volume, sibling).unwrap();
        session.delete(volume, other).unwrap();
        assert_eq!(names(&mut session, volume, a), ["original"]);
        assert!(names(&mut session, volume, b).is_empty());
        assert_eq!(id_for(&mut session, a, "original"), original);
        assert!(matches!(
            session.stat(volume, sibling),
            Err(Error::NotFound)
        ));
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
