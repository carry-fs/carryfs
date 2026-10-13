// SPDX-License-Identifier: Apache-2.0 OR GPL-2.0-only
use super::*;

#[test]
fn volume_label_preserves_unicode_and_punctuation_with_bounded_input() {
    for text in [
        "Backup",
        " Andrei, \\ backup ",
        "東京 💾",
        "e\u{301}",
        "a:b?*",
    ] {
        assert_eq!(VolumeLabel::parse(text).unwrap().as_str(), text);
    }
    for text in ["", ".", "..", "a/b", "a\0b", "a\nb", "a\u{7f}b", "a\u{85}b"] {
        assert!(VolumeLabel::parse(text).is_err(), "{text:?}");
    }
    assert!(VolumeLabel::parse(&"a".repeat(255)).is_ok());
    assert!(VolumeLabel::parse(&"a".repeat(256)).is_err());
    assert!(VolumeLabel::parse(&"é".repeat(127)).is_ok());
    assert!(VolumeLabel::parse(&"é".repeat(128)).is_err());
}

#[test]
fn formatting_with_label_stores_it_in_the_first_generation() {
    let device = FaultDevice::new(BTreeMap::new(), MountMode::ReadWrite);
    let media = device.media.clone();
    let label = VolumeLabel::parse("Andrei, Backup \\ 東京").unwrap();
    let mut volume = Volume::format_with_label(device, label.clone()).unwrap();
    let info = volume.info().unwrap();
    assert_eq!(info.label, Some(label));
    assert_eq!(info.name(), "Andrei, Backup \\ 東京");
    assert_eq!((info.revision, info.generation), (1, 1));
    volume.eject().unwrap();
    let durable = media.borrow().durable.clone();
    let device = FaultDevice::new(durable, MountMode::ReadOnly);
    let reopened = Volume::open(device).unwrap();
    assert_eq!(reopened.info().unwrap(), info);
}

#[test]
fn naming_an_unnamed_volume_keeps_revision_identity_and_accounting() {
    let device = FaultDevice::new(formatted(), MountMode::ReadWrite);
    let media = device.media.clone();
    let mut volume = Volume::open(device).unwrap();
    let before = volume.info().unwrap();
    assert_eq!(before.revision, 1);
    assert_eq!(before.name(), "CarryFS-42424242");
    volume
        .set_label(VolumeLabel::parse("Backup").unwrap())
        .unwrap();
    let after = volume.info().unwrap();
    assert_eq!(after.revision, 1);
    assert_eq!(after.name(), "Backup");
    assert_eq!(after.uuid, before.uuid);
    assert_eq!(after.free_blocks, before.free_blocks);
    assert_eq!(after.generation, before.generation + 1);
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
    volume
        .set_label(VolumeLabel::parse("Backup").unwrap())
        .unwrap();
    assert_eq!(media.borrow().events, events);
}

#[test]
fn label_change_publishes_pending_deferred_file_changes() {
    let device = FaultDevice::new(formatted(), MountMode::ReadWrite);
    let media = device.media.clone();
    let mut volume = Volume::open(device).unwrap();
    volume.set_deferred_checkpoints(true).unwrap();
    let file = volume
        .create_file(ROOT, Name::parse("file").unwrap())
        .unwrap();
    volume.write(&file, 0, b"kept").unwrap();
    assert!(volume.has_unsynced_changes());
    volume
        .set_label(VolumeLabel::parse("Backup").unwrap())
        .unwrap();
    assert!(!volume.has_unsynced_changes());
    let same = volume
        .create_file(ROOT, Name::parse("same").unwrap())
        .unwrap();
    volume.write(&same, 0, b"same label").unwrap();
    volume
        .set_label(VolumeLabel::parse("Backup").unwrap())
        .unwrap();
    assert!(!volume.has_unsynced_changes());
    drop(volume);
    media.borrow_mut().crash();
    let device = FaultDevice::new(media.borrow().durable.clone(), MountMode::ReadOnly);
    let mut volume = Volume::open(device).unwrap();
    assert_eq!(volume.info().unwrap().name(), "Backup");
    let file = volume
        .open_file(ROOT, &Name::parse("file").unwrap(), Access::Read)
        .unwrap();
    let mut bytes = [0; 4];
    assert_eq!(volume.read(&file, 0, &mut bytes).unwrap(), 4);
    assert_eq!(&bytes, b"kept");
    let file = volume
        .open_file(ROOT, &Name::parse("same").unwrap(), Access::Read)
        .unwrap();
    let mut bytes = [0; 10];
    assert_eq!(volume.read(&file, 0, &mut bytes).unwrap(), 10);
    assert_eq!(&bytes, b"same label");
}

#[test]
fn read_only_label_change_does_not_write() {
    let device = FaultDevice::new(formatted(), MountMode::ReadOnly);
    let media = device.media.clone();
    let mut volume = Volume::open(device).unwrap();
    assert!(matches!(
        volume.set_label(VolumeLabel::parse("Backup").unwrap()),
        Err(Error::ReadOnly)
    ));
    assert!(media.borrow().events.is_empty());
}

#[test]
fn interrupted_label_publication_recovers_old_or_new_label_and_mirrors_it() {
    for initial in [None, Some("Original")] {
        let mut base = formatted();
        if let Some(text) = initial {
            let device = FaultDevice::new(base, MountMode::ReadWrite);
            let media = device.media.clone();
            let mut volume = Volume::open(device).unwrap();
            volume.set_label(VolumeLabel::parse(text).unwrap()).unwrap();
            base = media.borrow().durable.clone();
        }
        for event in 0..5 {
            for mask in [0, u64::MAX] {
                for prefix in [0, 185, 200, 512, 4096] {
                    let device = FaultDevice::new(base.clone(), MountMode::ReadWrite);
                    let media = device.media.clone();
                    let mut volume = Volume::open(device).unwrap();
                    media.borrow_mut().failure = Some(Failure {
                        event,
                        whole_mask: mask,
                        tear: Some(Tear::Prefix {
                            write: 0,
                            bytes: prefix,
                        }),
                    });
                    assert!(
                        volume
                            .set_label(VolumeLabel::parse("New name 東京").unwrap())
                            .is_err()
                    );
                    assert!(matches!(
                        volume.set_label(VolumeLabel::parse("again").unwrap()),
                        Err(Error::Faulted)
                    ));
                    drop(volume);
                    media.borrow_mut().crash();
                    let durable = media.borrow().durable.clone();
                    let device = FaultDevice::new(durable.clone(), MountMode::ReadOnly);
                    let readonly_media = device.media.clone();
                    let mut reader = Volume::open(device).unwrap();
                    reader.check().unwrap();
                    let info = reader.info().unwrap();
                    let old_name = initial.unwrap_or("CarryFS-42424242");
                    assert!(info.name() == old_name || info.name() == "New name 東京");
                    assert!(readonly_media.borrow().events.is_empty());
                    let device = FaultDevice::new(durable, MountMode::ReadWrite);
                    let writable_media = device.media.clone();
                    let reopened = Volume::open(device).unwrap();
                    assert_eq!(reopened.info().unwrap(), info);
                    let disk = writable_media.borrow();
                    let a = Root::decode(&disk.durable[&ROOT_A].0[..], ROOT_A, GEOMETRY).unwrap();
                    let b_slot = GEOMETRY.blocks() - 257;
                    let b = Root::decode(&disk.durable[&b_slot].0[..], b_slot, GEOMETRY).unwrap();
                    assert_eq!(a, b);
                }
            }
        }
    }
}
