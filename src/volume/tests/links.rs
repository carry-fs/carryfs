//! Hard links: several directory entries for one regular file.

use super::*;
use crate::format::{TreeRef, decode_page, encode_leaf};

fn write_file(
    volume: &mut Volume<FaultDevice>,
    parent: ObjectId,
    file: &str,
    bytes: &[u8],
) -> ObjectId {
    let handle = volume.create_file(parent, name(file)).unwrap();
    volume.write(&handle, 0, bytes).unwrap();
    volume.close_handle(handle).unwrap();
    volume.lookup(parent, &name(file)).unwrap()
}

fn read_file(volume: &mut Volume<FaultDevice>, parent: ObjectId, file: &str) -> Vec<u8> {
    let handle = volume.open_file(parent, &name(file), Access::Read).unwrap();
    let mut bytes = vec![0; 64];
    let read = volume.read(&handle, 0, &mut bytes).unwrap();
    volume.close_handle(handle).unwrap();
    bytes.truncate(read);
    bytes
}

fn links(volume: &mut Volume<FaultDevice>, object: ObjectId) -> u32 {
    volume.attributes(object).unwrap().links
}

#[test]
fn links_share_contents_and_survive_until_the_last_name_goes() {
    let mut volume = opened();
    let dir = volume.create_directory(ROOT, name("dir")).unwrap();
    let file = write_file(&mut volume, ROOT, "a", b"shared");
    assert_eq!(links(&mut volume, file), 1);
    assert!(!volume.supports_links());

    volume.link(file, dir, name("b")).unwrap();
    assert!(volume.supports_links());
    assert_eq!(volume.lookup(dir, &name("B")).unwrap(), file);
    assert_eq!(links(&mut volume, file), 2);
    assert_eq!(volume.attributes(file).unwrap().parent, dir);
    assert_eq!(volume.attributes(dir).unwrap().size, 0);
    let handle = volume
        .open_file(dir, &name("b"), Access::ReadWrite)
        .unwrap();
    volume.write(&handle, 0, b"SHARED").unwrap();
    volume.close_handle(handle).unwrap();
    assert_eq!(read_file(&mut volume, ROOT, "a"), b"SHARED");
    volume.sync().unwrap();
    checked(&volume);

    // Removing a name the parent hint points at leaves the file linked.
    volume.remove(dir, &name("b"), RemoveKind::File).unwrap();
    assert_eq!(links(&mut volume, file), 1);
    volume
        .remove(ROOT, &name("dir"), RemoveKind::Directory)
        .unwrap();
    volume.sync().unwrap();
    checked(&volume);
    let image = volume.into_model_image();
    let mut reopened = Volume::open(FaultDevice::new(image, MountMode::ReadWrite)).unwrap();
    assert!(reopened.supports_links());
    assert_eq!(read_file(&mut reopened, ROOT, "a"), b"SHARED");

    reopened.remove(ROOT, &name("a"), RemoveKind::File).unwrap();
    assert!(matches!(reopened.attributes(file), Err(Error::NotFound)));
    reopened.sync().unwrap();
    checked(&reopened);
}

#[test]
fn an_open_file_keeps_its_other_names_when_one_is_removed() {
    let mut volume = opened();
    let file = write_file(&mut volume, ROOT, "a", b"open");
    volume.link(file, ROOT, name("b")).unwrap();
    let handle = volume.open_file(ROOT, &name("a"), Access::Read).unwrap();
    volume.remove(ROOT, &name("a"), RemoveKind::File).unwrap();
    let attributes = volume.attributes(file).unwrap();
    assert!(!attributes.orphan);
    assert_eq!(attributes.links, 1);
    volume.remove(ROOT, &name("b"), RemoveKind::File).unwrap();
    assert_eq!(volume.attributes(file).unwrap().links, 0);
    assert!(volume.attributes(file).unwrap().orphan);
    let mut bytes = [0; 4];
    assert_eq!(volume.read(&handle, 0, &mut bytes).unwrap(), 4);
    volume.close_handle(handle).unwrap();
    volume.sync().unwrap();
    checked(&volume);
}

#[test]
fn rename_and_swap_between_names_of_one_file_change_nothing() {
    let mut volume = opened();
    let file = write_file(&mut volume, ROOT, "a", b"one");
    let other = write_file(&mut volume, ROOT, "other", b"two");
    volume.link(file, ROOT, name("b")).unwrap();
    let before = volume.attributes(ROOT).unwrap();

    volume
        .rename(ROOT, &name("a"), ROOT, name("b"), Replace::Allowed)
        .unwrap();
    volume.swap(ROOT, &name("a"), ROOT, &name("b")).unwrap();
    assert_eq!(volume.attributes(ROOT).unwrap(), before);
    assert_eq!(volume.lookup(ROOT, &name("a")).unwrap(), file);
    assert_eq!(volume.lookup(ROOT, &name("b")).unwrap(), file);
    assert_eq!(links(&mut volume, file), 2);
    assert!(matches!(
        volume.rename(ROOT, &name("a"), ROOT, name("b"), Replace::Forbidden),
        Err(Error::NameCollision)
    ));

    // Replacing one name of a linked file drops that name, not the file.
    volume
        .rename(ROOT, &name("other"), ROOT, name("b"), Replace::Allowed)
        .unwrap();
    assert_eq!(volume.lookup(ROOT, &name("b")).unwrap(), other);
    let attributes = volume.attributes(file).unwrap();
    assert!(!attributes.orphan);
    assert_eq!(attributes.links, 1);
    assert_eq!(read_file(&mut volume, ROOT, "a"), b"one");
    volume.sync().unwrap();
    checked(&volume);
}

#[test]
fn directories_never_link_and_failed_links_leave_the_volume_unchanged() {
    let mut volume = opened();
    let dir = volume.create_directory(ROOT, name("dir")).unwrap();
    let file = volume.create_file(ROOT, name("f")).unwrap();
    volume.close_handle(file).unwrap();
    let file = volume.lookup(ROOT, &name("f")).unwrap();
    volume.sync().unwrap();
    let before = durable(&volume);

    assert!(matches!(
        volume.link(dir, ROOT, name("x")),
        Err(Error::WrongType)
    ));
    assert!(matches!(
        volume.link(file, ROOT, name("DIR")),
        Err(Error::NameCollision)
    ));
    assert!(matches!(
        volume.link(file, file, name("x")),
        Err(Error::WrongType)
    ));
    volume.sync().unwrap();
    assert_eq!(durable(&volume), before);
    assert!(!volume.supports_links());

    // A file with another name cannot become a symlink.
    volume.link(file, ROOT, name("g")).unwrap();
    assert!(matches!(
        volume.make_symlink(ROOT, &name("f"), b"dir"),
        Err(Error::WrongType)
    ));
    volume.sync().unwrap();
    checked(&volume);
}

#[test]
fn symlinks_link_like_files() {
    let mut volume = opened();
    let dir = volume.create_directory(ROOT, name("dir")).unwrap();
    let symlink = volume.create_symlink(ROOT, name("s"), b"../t").unwrap();
    volume.link(symlink, dir, name("s2")).unwrap();
    assert!(volume.supports_links());
    let entry = volume.lookup_entry(dir, &name("s2")).unwrap();
    assert_eq!((entry.object, entry.kind), (symlink, ObjectKind::Symlink));
    assert_eq!(links(&mut volume, symlink), 2);
    assert_eq!(volume.read_symlink(symlink).unwrap(), b"../t");
    volume.sync().unwrap();
    checked(&volume);

    // The directory its parent hint names can go once its name there does.
    volume.remove(dir, &name("s2"), RemoveKind::File).unwrap();
    volume
        .remove(ROOT, &name("dir"), RemoveKind::Directory)
        .unwrap();
    assert_eq!(links(&mut volume, symlink), 1);
    volume.sync().unwrap();
    checked(&volume);
    volume.remove(ROOT, &name("s"), RemoveKind::File).unwrap();
    assert!(matches!(volume.attributes(symlink), Err(Error::NotFound)));
    volume.sync().unwrap();
    checked(&volume);
}

/// The single catalog leaf of a small synced volume, decoded, edited and
/// re-encoded in place.
fn forge(
    volume: &Volume<FaultDevice>,
    edit: impl FnOnce(&mut Vec<(Vec<u8>, Vec<u8>)>),
) -> BTreeMap<u64, Page> {
    let mut image = durable(volume);
    let root = volume.root.clone().unwrap();
    let decoded = decode_page(
        &image[&root.catalog.block].0[..],
        root.catalog,
        &root.uuid,
        TreeKind::Catalog,
        root.generation,
        Some(0),
        root.blocks,
    )
    .expect("a small catalog is one leaf");
    let mut records: Vec<_> = decoded
        .records
        .iter()
        .map(|(key, value)| (key.to_vec(), value.to_vec()))
        .collect();
    edit(&mut records);
    records.sort();
    let borrowed: Vec<_> = records
        .iter()
        .map(|(key, value)| (key.as_slice(), value.as_slice()))
        .collect();
    let reference = TreeRef {
        block: root.catalog.block,
        generation: root.catalog.generation,
    };
    let page = encode_leaf(
        reference,
        &root.uuid,
        TreeKind::Catalog,
        &borrowed,
        root.blocks,
    );
    image.insert(root.catalog.block, page.unwrap());
    image
}

fn set_links(records: &mut [(Vec<u8>, Vec<u8>)], object: ObjectId, extra: u32) {
    let key = super::super::namespace::inode_key(object.0);
    let (_, inode) = records.iter_mut().find(|(k, _)| k[..] == key[..]).unwrap();
    inode[88..92].copy_from_slice(&extra.to_le_bytes());
}

/// Whether the checker rejects `image` and names the page at fault.
fn rejected(image: BTreeMap<u64, Page>) -> bool {
    let mut volume = Volume::open(FaultDevice::new(image, MountMode::ReadOnly)).unwrap();
    matches!(volume.check(), Err(Error::Corruption { .. }))
}

#[test]
fn check_rejects_link_counts_that_miss_their_entries() {
    let mut volume = opened();
    let two = write_file(&mut volume, ROOT, "two", b"2");
    volume.link(two, ROOT, name("two-b")).unwrap();
    let three = write_file(&mut volume, ROOT, "three", b"3");
    volume.link(three, ROOT, name("three-b")).unwrap();
    volume.link(three, ROOT, name("three-c")).unwrap();
    let single = write_file(&mut volume, ROOT, "single", b"1");
    volume.sync().unwrap();
    checked(&volume);

    assert!(rejected(forge(&volume, |r| set_links(r, two, 2))));
    assert!(rejected(forge(&volume, |r| set_links(r, two, 0))));
    assert!(rejected(forge(&volume, |r| set_links(r, single, 1))));
    // Moving one link between objects keeps every total equal.
    assert!(rejected(forge(&volume, |r| {
        set_links(r, two, 2);
        set_links(r, three, 1);
    })));
}

#[test]
fn check_rejects_entries_in_an_orphaned_directory() {
    let mut volume = opened();
    let file = write_file(&mut volume, ROOT, "f", b"x");
    volume.link(file, ROOT, name("g")).unwrap();
    let dir = volume.create_directory(ROOT, name("gone")).unwrap();
    let pin = volume.open_directory(dir).unwrap();
    volume
        .remove(ROOT, &name("gone"), RemoveKind::Directory)
        .unwrap();
    volume.sync().unwrap();
    checked(&volume);
    // Move entry "g" into the orphaned directory: totals still balance.
    let image = forge(&volume, |records| {
        let from = super::super::namespace::dirent_key(ROOT.0, &name("g").folded);
        let at = records.iter().position(|(k, _)| *k == from).unwrap();
        let (_, value) = records.remove(at);
        records.push((
            super::super::namespace::dirent_key(dir.0, &name("g").folded),
            value,
        ));
        for (owner, delta) in [(ROOT.0, -1i64), (dir.0, 1)] {
            let key = super::super::namespace::inode_key(owner);
            let (_, inode) = records.iter_mut().find(|(k, _)| k[..] == key[..]).unwrap();
            let count = u64::from_le_bytes(inode[32..40].try_into().unwrap());
            inode[32..40].copy_from_slice(&count.saturating_add_signed(delta).to_le_bytes());
        }
    });
    assert!(rejected(image));
    volume.close_handle(pin).unwrap();
}

#[test]
fn a_link_past_the_cap_is_refused() {
    let mut volume = opened();
    volume.set_deferred_checkpoints(true).unwrap();
    let dir = volume.create_directory(ROOT, name("names")).unwrap();
    let file = write_file(&mut volume, dir, "0", b"x");
    for index in 1..=crate::format::MAX_EXTRA_LINKS {
        volume.link(file, dir, name(&index.to_string())).unwrap();
    }
    assert_eq!(links(&mut volume, file), 65_535);
    assert!(matches!(
        volume.link(file, ROOT, name("over")),
        Err(Error::LimitExceeded)
    ));
    volume.sync().unwrap();
    checked(&volume);
}

/// Every cut of a link's output leaves the old state or the new one, with the
/// feature bit published together with the second entry.
#[test]
fn link_and_unlink_output_cuts_recover_old_or_new() {
    let mut volume = opened();
    let file = write_file(&mut volume, ROOT, "a", b"atomic");
    volume.sync().unwrap();
    let base = volume.into_model_image();
    let link = |volume: &mut Volume<FaultDevice>| -> Result<()> {
        volume.link(file, ROOT, name("b"))?;
        volume.sync_publication()
    };
    let unlink = |volume: &mut Volume<FaultDevice>| -> Result<()> {
        volume.remove(ROOT, &name("a"), RemoveKind::File)?;
        volume.sync_publication()
    };
    let mut complete = Volume::open(FaultDevice::new(base.clone(), MountMode::ReadWrite)).unwrap();
    link(&mut complete).unwrap();
    let linked = complete.into_model_image();
    for (base, operation, linked_before) in [
        (
            base,
            &link as &dyn Fn(&mut Volume<FaultDevice>) -> Result<()>,
            false,
        ),
        (linked, &unlink, true),
    ] {
        let mut probe = Volume::open(FaultDevice::new(base.clone(), MountMode::ReadWrite)).unwrap();
        let media = probe.device.as_ref().unwrap().media.clone();
        media.borrow_mut().events.clear();
        operation(&mut probe).unwrap();
        let events = media.borrow().events.len();
        for event in 0..events {
            for whole_mask in [0, u64::MAX] {
                let mut volume =
                    Volume::open(FaultDevice::new(base.clone(), MountMode::ReadWrite)).unwrap();
                let media = volume.device.as_ref().unwrap().media.clone();
                media.borrow_mut().events.clear();
                media.borrow_mut().failure = Some(Failure {
                    event,
                    whole_mask,
                    tear: None,
                });
                assert!(operation(&mut volume).is_err());
                drop(volume);
                media.borrow_mut().crash();
                let image = media.borrow().durable.clone();
                let mut reader =
                    Volume::open(FaultDevice::new(image, MountMode::ReadOnly)).unwrap();
                reader.check().unwrap();
                let a = reader.lookup(ROOT, &name("a")).ok();
                let b = reader.lookup(ROOT, &name("b")).ok();
                let names = usize::from(a.is_some()) + usize::from(b.is_some());
                assert_eq!(reader.attributes(file).unwrap().links as usize, names);
                assert_eq!(reader.supports_links(), linked_before || names == 2);
                assert_eq!(
                    read_file(&mut reader, ROOT, if a.is_some() { "a" } else { "b" }),
                    b"atomic"
                );
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn extraction_links_later_names_to_the_first() {
    use std::os::unix::fs::MetadataExt;
    let mut volume = opened();
    let dir = volume.create_directory(ROOT, name("dir")).unwrap();
    let file = write_file(&mut volume, ROOT, "a", b"shared");
    volume.link(file, dir, name("b")).unwrap();
    let symlink = volume.create_symlink(ROOT, name("s"), b"a").unwrap();
    volume.link(symlink, dir, name("s2")).unwrap();
    volume.sync().unwrap();
    let image = volume.into_model_image();
    let mut reader = Volume::open(FaultDevice::new(image, MountMode::ReadOnly)).unwrap();
    let mut output = RecoveryOutput::new();
    let summary = reader
        .extract_to(ROOT, &mut output, &mut |_| panic!("valid tree"))
        .unwrap();
    assert!(summary.complete);
    assert_eq!(summary.files, 1, "the second name is a link, not a copy");
    let inode = |path: &str| std::fs::symlink_metadata(output.root.join(path)).unwrap();
    for (first, second) in [("a", "dir/b"), ("s", "dir/s2")] {
        assert_eq!(inode(first).ino(), inode(second).ino());
        assert_eq!(inode(first).nlink(), 2);
    }
    assert_eq!(std::fs::read(output.root.join("dir/b")).unwrap(), b"shared");
}
