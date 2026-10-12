//! Symlinks: the target lives in the inode record, so every inode rewrite
//! must carry it through.

use super::*;

fn target(volume: &mut Volume<FaultDevice>, parent: ObjectId, link: &str) -> Vec<u8> {
    let object = volume.lookup(parent, &name(link)).unwrap();
    volume.read_symlink(object).unwrap()
}

#[test]
fn symlinks_round_trip_through_rename_times_and_reopen() {
    let mut volume = opened();
    let dir = volume.create_directory(ROOT, name("dir")).unwrap();
    let link = volume
        .create_symlink(ROOT, name("link"), b"dir/../notes.txt")
        .unwrap();
    let attributes = volume.attributes(link).unwrap();
    assert_eq!(attributes.kind, ObjectKind::Symlink);
    assert_eq!(attributes.size, 16);
    assert_eq!(attributes.allocated_bytes, 0);
    assert_eq!(attributes.directory_link, None);
    assert_eq!(
        volume.lookup_entry(ROOT, &name("LINK")).unwrap().kind,
        ObjectKind::Symlink
    );

    volume
        .rename(ROOT, &name("link"), dir, name("Moved"), Replace::Forbidden)
        .unwrap();
    let stamp = Timestamp::new(1_000_000_000, 5).unwrap();
    let pin = volume.retain_object(link).unwrap();
    volume.set_times(&pin, Some(stamp), Some(stamp)).unwrap();
    volume.close_handle(pin).unwrap();
    assert_eq!(target(&mut volume, dir, "moved"), b"dir/../notes.txt");

    let listing = volume.open_directory(dir).unwrap();
    let page = volume.read_directory(&listing, None).unwrap();
    assert_eq!(page.entries[0].kind, ObjectKind::Symlink);
    volume.close_handle(listing).unwrap();

    volume.sync().unwrap();
    checked(&volume);
    let image = volume.into_model_image();
    let mut reopened = Volume::open(FaultDevice::new(image, MountMode::ReadWrite)).unwrap();
    assert_eq!(target(&mut reopened, dir, "Moved"), b"dir/../notes.txt");
    assert_eq!(reopened.attributes(link).unwrap().modification, stamp);
}

#[test]
fn symlinks_reject_bad_targets_and_file_or_directory_use() {
    let mut volume = opened();
    for bad in [&b""[..], b"a\0b", &[0xff]] {
        assert!(matches!(
            volume.create_symlink(ROOT, name("bad"), bad),
            Err(Error::InvalidInput("invalid symlink target"))
        ));
    }
    assert!(matches!(
        volume.create_symlink(ROOT, name("bad"), &[b'x'; 1024]),
        Err(Error::TooLong("symlink target is too long"))
    ));
    let longest = vec![b'x'; 1023];
    let link = volume.create_symlink(ROOT, name("link"), &longest).unwrap();
    assert_eq!(volume.read_symlink(link).unwrap(), longest);
    assert!(matches!(
        volume.create_symlink(ROOT, name("LINK"), b"x"),
        Err(Error::NameCollision)
    ));

    assert!(matches!(
        volume.open_file(ROOT, &name("link"), Access::Read),
        Err(Error::WrongType)
    ));
    assert!(matches!(
        volume.open_object(link, Access::ReadWrite),
        Err(Error::WrongType)
    ));
    assert!(matches!(volume.open_directory(link), Err(Error::WrongType)));
    assert!(matches!(volume.read_symlink(ROOT), Err(Error::WrongType)));
    assert!(matches!(
        volume.create_directory(link, name("child")),
        Err(Error::WrongType)
    ));
    assert!(matches!(
        volume.remove(ROOT, &name("link"), RemoveKind::Directory),
        Err(Error::WrongType)
    ));
    checked(&volume);
}

#[test]
fn rename_replaces_non_directories_with_each_other_only() {
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("file")).unwrap();
    volume.write(&file, 0, b"data").unwrap();
    volume.close_handle(file).unwrap();
    volume.create_symlink(ROOT, name("link"), b"file").unwrap();
    volume.create_directory(ROOT, name("dir")).unwrap();

    // A link replaces a file, and a file replaces that link.
    volume
        .rename(ROOT, &name("link"), ROOT, name("file"), Replace::Allowed)
        .unwrap();
    assert_eq!(target(&mut volume, ROOT, "file"), b"file");
    let other = volume.create_file(ROOT, name("other")).unwrap();
    volume.close_handle(other).unwrap();
    volume
        .rename(ROOT, &name("other"), ROOT, name("file"), Replace::Allowed)
        .unwrap();
    assert_eq!(
        volume.lookup_entry(ROOT, &name("file")).unwrap().kind,
        ObjectKind::File
    );

    volume.create_symlink(ROOT, name("link"), b"dir").unwrap();
    assert!(matches!(
        volume.rename(ROOT, &name("link"), ROOT, name("dir"), Replace::Allowed),
        Err(Error::WrongType)
    ));
    assert!(matches!(
        volume.rename(ROOT, &name("dir"), ROOT, name("link"), Replace::Allowed),
        Err(Error::WrongType)
    ));
    volume
        .remove(ROOT, &name("link"), RemoveKind::File)
        .unwrap();
    checked(&volume);
}

#[test]
fn a_pinned_unlinked_symlink_keeps_its_target_until_close() {
    let mut volume = opened();
    let link = volume.create_symlink(ROOT, name("link"), b"away").unwrap();
    let pin = volume.retain_object(link).unwrap();
    volume
        .remove(ROOT, &name("link"), RemoveKind::File)
        .unwrap();
    assert!(volume.attributes(link).unwrap().orphan);
    assert_eq!(volume.read_symlink(link).unwrap(), b"away");
    checked(&volume);
    volume.close_handle(pin).unwrap();
    drain_cleanup(&mut volume);
    assert!(matches!(volume.attributes(link), Err(Error::NotFound)));
    checked(&volume);
}

#[test]
fn make_symlink_converts_only_empty_files_and_directories() {
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("file")).unwrap();
    volume.close_handle(file).unwrap();
    let dir = volume.create_directory(ROOT, name("dir")).unwrap();
    let full = volume.create_file(ROOT, name("full")).unwrap();
    volume.write(&full, 0, b"x").unwrap();
    volume.close_handle(full).unwrap();
    let parent = volume.create_directory(ROOT, name("parent")).unwrap();
    volume.create_directory(parent, name("child")).unwrap();

    volume.make_symlink(ROOT, &name("FILE"), b"target").unwrap();
    volume
        .make_symlink(ROOT, &name("dir"), b"elsewhere")
        .unwrap();
    assert_eq!(target(&mut volume, ROOT, "file"), b"target");
    assert_eq!(volume.read_symlink(dir).unwrap(), b"elsewhere");
    assert_eq!(volume.attributes(dir).unwrap().directory_link, Some(true));
    let file = volume.lookup(ROOT, &name("file")).unwrap();
    assert_eq!(volume.attributes(file).unwrap().directory_link, Some(false));
    assert_eq!(
        volume.lookup_entry(ROOT, &name("dir")).unwrap().kind,
        ObjectKind::Symlink
    );
    for (name_text, reason) in [("full", "data"), ("parent", "children"), ("file", "link")] {
        assert!(
            matches!(
                volume.make_symlink(ROOT, &name(name_text), b"x"),
                Err(Error::WrongType)
            ),
            "{reason}"
        );
    }
    assert!(matches!(
        volume.make_symlink(ROOT, &name("missing"), b"x"),
        Err(Error::NotFound)
    ));
    checked(&volume);

    // An orphaned directory link carries both inode flags until its last close.
    let pin = volume.retain_object(dir).unwrap();
    volume.remove(ROOT, &name("dir"), RemoveKind::File).unwrap();
    let attributes = volume.attributes(dir).unwrap();
    assert!(attributes.orphan && attributes.directory_link == Some(true));
    checked(&volume);
    volume.close_handle(pin).unwrap();
    drain_cleanup(&mut volume);
    assert!(matches!(volume.attributes(dir), Err(Error::NotFound)));
    checked(&volume);
}

#[test]
fn extraction_recreates_symlinks_without_following_them() {
    let mut volume = opened();
    let dir = volume.create_directory(ROOT, name("dir")).unwrap();
    volume
        .create_symlink(dir, name("up"), b"../outside/secret")
        .unwrap();
    volume
        .create_symlink(dir, name("abs"), b"/etc/passwd")
        .unwrap();
    let image = volume.into_model_image();
    let mut volume = Volume::open(FaultDevice::new(image, MountMode::ReadOnly)).unwrap();
    let mut output = RecoveryOutput::new();
    let summary = volume
        .extract_to(ROOT, &mut output, &mut |_| panic!("valid tree"))
        .unwrap();
    assert!(summary.complete);
    assert_eq!(summary.objects, 4);
    assert_eq!(summary.files, 0);
    let up = output.root.join("dir/up");
    assert!(std::fs::symlink_metadata(&up).unwrap().is_symlink());
    assert_eq!(
        std::fs::read_link(&up).unwrap(),
        std::path::Path::new("../outside/secret")
    );
    assert_eq!(
        std::fs::read_link(output.root.join("dir/abs")).unwrap(),
        std::path::Path::new("/etc/passwd")
    );
}

#[test]
fn check_rejects_extents_under_a_symlink() {
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("file")).unwrap();
    volume.write(&file, 0, b"data").unwrap();
    volume.close_handle(file).unwrap();
    let object = volume.lookup(ROOT, &name("file")).unwrap();
    // A valid symlink inode over a file whose extents stay allocated.
    volume
        .edit(|transaction| {
            let key = namespace::inode_key(object.0);
            let value = transaction
                .forest
                .lookup(&transaction.catalog, crate::format::TreeKind::Catalog, &key)?
                .unwrap();
            let mut inode = namespace::Inode::decode(&value)?;
            inode.kind = ObjectKind::Symlink;
            inode.target = b"x".to_vec();
            inode.size = 1;
            inode.allocated_blocks = 0;
            let entry_key = namespace::dirent_key(ROOT.0, &name("file").folded);
            let mut entry = transaction
                .forest
                .lookup(
                    &transaction.catalog,
                    crate::format::TreeKind::Catalog,
                    &entry_key,
                )?
                .unwrap();
            entry[8] = 3;
            let mut catalog = transaction.catalog.clone();
            for (key, value) in [(key, inode.encode()), (entry_key, entry)] {
                catalog = transaction.forest.put(
                    catalog,
                    crate::format::TreeKind::Catalog,
                    &key,
                    &value,
                )?;
            }
            transaction.catalog = catalog;
            Ok(())
        })
        .unwrap();
    let error = Volume::open(FaultDevice::new(durable(&volume), MountMode::ReadOnly))
        .unwrap()
        .check()
        .unwrap_err();
    assert!(
        matches!(error, Error::Corruption { reason, .. } if reason == "non-file has extents"),
        "{error:?}"
    );
}

#[test]
fn interrupted_symlink_publications_recover_old_or_new_state() {
    let base = formatted();
    namespace_crash_matrix(
        &base,
        |volume| {
            volume
                .create_symlink(ROOT, name("link"), b"target")
                .map(|_| ())
        },
        |volume| {
            volume
                .lookup(ROOT, &name("link"))
                .is_ok_and(|link| volume.read_symlink(link).unwrap() == b"target")
        },
    );

    let mut volume = opened();
    let file = volume.create_file(ROOT, name("empty")).unwrap();
    volume.close_handle(file).unwrap();
    let populated = volume.into_model_image();
    namespace_crash_matrix(
        &populated,
        |volume| volume.make_symlink(ROOT, &name("empty"), b"target"),
        |volume| volume.lookup_entry(ROOT, &name("empty")).unwrap().kind == ObjectKind::Symlink,
    );
}
