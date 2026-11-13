//! Modes and the hidden attribute: stored only after `upgrade`, carried
//! through every inode rewrite, and never written without the root feature.

use super::*;

fn upgraded() -> Volume<FaultDevice> {
    let mut volume = opened();
    assert!(volume.upgrade().unwrap());
    volume
}

fn new(mode: u32, hidden: bool) -> NewAttributes {
    NewAttributes {
        mode: Some(Mode::from_host(mode)),
        hidden,
    }
}

fn reopened(volume: &Volume<FaultDevice>) -> Volume<FaultDevice> {
    Volume::open(FaultDevice::new(durable(volume), MountMode::ReadWrite)).unwrap()
}

#[test]
fn mode_keeps_only_the_nine_permission_bits() {
    assert_eq!(Mode::from_host(0o104_755).bits(), 0o755);
    assert!(Mode::from_host(0o200).owner_writable());
    assert_eq!(Mode::from_host(0o666).with_owner_write(false).bits(), 0o444);
    assert_eq!(Mode::from_host(0o444).with_owner_write(true).bits(), 0o644);
}

#[test]
fn a_volume_without_the_feature_reads_defaults_and_stores_nothing() {
    let mut volume = opened();
    assert!(!volume.stores_attributes());
    let file = volume
        .create_file_with(ROOT, name("f"), new(0o600, true))
        .unwrap();
    let dir = volume
        .create_directory_with(ROOT, name("d"), new(0o700, true))
        .unwrap();
    let object = volume.lookup(ROOT, &name("f")).unwrap();
    let attributes = volume.attributes(object).unwrap();
    assert_eq!((attributes.mode, attributes.hidden), (Mode::FILE, false));
    assert_eq!(volume.attributes(dir).unwrap().mode, Mode::DIRECTORY);

    for change in [
        AttributeChange {
            mode: Some(Mode::FILE),
            ..Default::default()
        },
        AttributeChange {
            hidden: Some(false),
            ..Default::default()
        },
    ] {
        assert!(matches!(
            volume.set_attributes(&file, change),
            Err(Error::InvalidInput(_))
        ));
    }
    let stamp = Timestamp::new(7, 0).unwrap();
    volume
        .set_attributes(
            &file,
            AttributeChange {
                modification: Some(stamp),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(volume.attributes(object).unwrap().modification, stamp);
    volume.close_handle(file).unwrap();
    volume.sync().unwrap();
    checked(&volume);
}

#[test]
fn the_core_refuses_an_inode_with_a_mode_on_a_volume_without_the_feature() {
    let mut volume = opened();
    let now = Timestamp::now().unwrap();
    let refused = volume.edit(|transaction| {
        super::super::namespace::create_in(
            transaction,
            ROOT,
            name("f"),
            ObjectKind::File,
            &[],
            new(0o600, false),
            now,
        )
    });
    assert!(matches!(refused, Err(Error::InvalidInput(_))));
}

#[test]
fn upgrade_is_idempotent_and_publishes_only_a_root() {
    let device = FaultDevice::new(formatted(), MountMode::ReadWrite);
    let media = device.media.clone();
    let mut volume = Volume::open(device).unwrap();
    let before = volume.info().unwrap();
    assert!(volume.upgrade().unwrap());
    assert_eq!(
        media.borrow().events,
        vec![
            Event::Flush,
            Event::Write(ROOT_A),
            Event::Flush,
            Event::Write(GEOMETRY.blocks() - 257),
            Event::Flush,
        ]
    );
    let events = media.borrow().events.clone();
    assert!(!volume.upgrade().unwrap());
    assert_eq!(media.borrow().events, events);
    assert_eq!(volume.info().unwrap().free_blocks, before.free_blocks);
    assert!(reopened(&volume).stores_attributes());
    checked(&volume);
}

#[test]
fn a_crash_during_upgrade_leaves_the_old_or_the_new_root() {
    for event in 0..5 {
        let device = FaultDevice::new(formatted(), MountMode::ReadWrite);
        let media = device.media.clone();
        let mut volume = Volume::open(device).unwrap();
        media.borrow_mut().failure = Some(Failure {
            event,
            whole_mask: 0,
            tear: None,
        });
        let _ = volume.upgrade();
        let durable = media.borrow().durable.clone();
        let mut volume = Volume::open(FaultDevice::new(durable, MountMode::ReadOnly)).unwrap();
        volume.check().unwrap();
    }
}

#[test]
fn mode_and_hidden_survive_writes_renames_truncation_orphaning_and_reopen() {
    let mut volume = upgraded();
    let file = volume
        .create_file_with(ROOT, name("script"), new(0o750, true))
        .unwrap();
    let object = volume.lookup(ROOT, &name("script")).unwrap();
    let dir = volume
        .create_directory_with(ROOT, name("private"), new(0o700, false))
        .unwrap();
    volume.write(&file, 0, b"#!/bin/sh\n").unwrap();
    volume.truncate(&file, 4).unwrap();
    volume
        .rename(ROOT, &name("script"), dir, name("run"), Replace::Forbidden)
        .unwrap();
    volume.sync().unwrap();
    checked(&volume);

    let mut volume = reopened(&volume);
    let attributes = volume.attributes(object).unwrap();
    assert_eq!((attributes.mode.bits(), attributes.hidden), (0o750, true));
    assert_eq!(volume.attributes(dir).unwrap().mode.bits(), 0o700);

    let pin = volume.retain_object(object).unwrap();
    volume.remove(dir, &name("run"), RemoveKind::File).unwrap();
    let attributes = volume.attributes(object).unwrap();
    assert!(attributes.orphan);
    assert_eq!((attributes.mode.bits(), attributes.hidden), (0o750, true));
    volume.close_handle(pin).unwrap();
}

#[test]
fn a_mode_change_bumps_only_the_metadata_change_time() {
    let mut volume = upgraded();
    let file = volume.create_file(ROOT, name("f")).unwrap();
    let object = volume.lookup(ROOT, &name("f")).unwrap();
    let before = volume.attributes(object).unwrap();
    assert_eq!((before.mode, before.hidden), (Mode::FILE, false));
    volume
        .set_attributes(
            &file,
            AttributeChange {
                mode: Some(Mode::from_host(0o4755)),
                hidden: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    let after = volume.attributes(object).unwrap();
    assert_eq!((after.mode.bits(), after.hidden), (0o755, true));
    assert_eq!(
        (after.creation, after.modification),
        (before.creation, before.modification)
    );
    assert!(
        (
            after.metadata_change.seconds(),
            after.metadata_change.nanos()
        ) >= (
            before.metadata_change.seconds(),
            before.metadata_change.nanos()
        )
    );

    // Hidden alone leaves the mode.
    volume
        .set_attributes(
            &file,
            AttributeChange {
                hidden: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
    let after = volume.attributes(object).unwrap();
    assert_eq!((after.mode.bits(), after.hidden), (0o755, false));
    volume.close_handle(file).unwrap();
    volume.sync().unwrap();
    checked(&volume);
}

#[test]
fn a_symlink_stores_no_mode_but_keeps_hidden() {
    let mut volume = upgraded();
    let link = volume.create_symlink(ROOT, name("l"), b"target").unwrap();
    let pin = volume.retain_object(link).unwrap();
    volume
        .set_attributes(
            &pin,
            AttributeChange {
                mode: Some(Mode::from_host(0o600)),
                hidden: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    volume.close_handle(pin).unwrap();
    let attributes = volume.attributes(link).unwrap();
    assert_eq!((attributes.mode, attributes.hidden), (Mode::SYMLINK, true));

    // Converting a file in place clears its mode and keeps hidden.
    let file = volume
        .create_file_with(ROOT, name("w"), new(0o600, true))
        .unwrap();
    volume.close_handle(file).unwrap();
    volume.make_symlink(ROOT, &name("w"), b"target").unwrap();
    let object = volume.lookup(ROOT, &name("w")).unwrap();
    let attributes = volume.attributes(object).unwrap();
    assert_eq!((attributes.mode, attributes.hidden), (Mode::SYMLINK, true));
    volume.sync().unwrap();
    checked(&volume);
}

#[test]
fn mode_zero_is_stored_and_distinct_from_the_default() {
    let mut volume = upgraded();
    let file = volume
        .create_file_with(ROOT, name("locked"), new(0, false))
        .unwrap();
    volume.close_handle(file).unwrap();
    volume.sync().unwrap();
    let mut volume = reopened(&volume);
    let object = volume.lookup(ROOT, &name("locked")).unwrap();
    assert_eq!(volume.attributes(object).unwrap().mode.bits(), 0);
}
