//! Volumes CarryFS 0.1.0 formatted: readable as they are, writable only after
//! `upgrade` publishes a revision 2 root.

use super::*;

fn root_writes(media: &std::rc::Rc<std::cell::RefCell<super::super::model::Media>>) -> Vec<u64> {
    media
        .borrow()
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Write(block) => Some(*block),
            Event::Flush => None,
        })
        .collect()
}

#[test]
fn a_legacy_volume_reads_but_refuses_a_writable_open_without_writing() {
    let image = formatted_legacy();
    let mut reader = Volume::open(FaultDevice::new(image.clone(), MountMode::ReadOnly)).unwrap();
    assert_eq!(reader.info().unwrap().revision, 1);
    reader.check().unwrap();

    let device = FaultDevice::new(image, MountMode::ReadWrite);
    let media = device.media.clone();
    assert!(matches!(Volume::open(device), Err(Error::NeedsUpgrade)));
    assert!(media.borrow().events.is_empty());
}

#[test]
fn upgrade_publishes_only_a_root_and_is_idempotent() {
    let image = formatted_legacy();
    let before = Volume::open(FaultDevice::new(image.clone(), MountMode::ReadOnly))
        .unwrap()
        .info()
        .unwrap();
    let device = FaultDevice::new(image, MountMode::ReadWrite);
    let media = device.media.clone();
    assert!(Volume::upgrade(device).unwrap());
    assert_eq!(root_writes(&media), vec![ROOT_A, GEOMETRY.blocks() - 257]);

    let upgraded = media.borrow().durable.clone();
    let mut volume =
        Volume::open(FaultDevice::new(upgraded.clone(), MountMode::ReadWrite)).unwrap();
    let after = volume.info().unwrap();
    assert_eq!(after.revision, 2);
    assert_eq!(after.free_blocks, before.free_blocks);
    let file = volume
        .create_file_with(
            ROOT,
            name("f"),
            NewAttributes {
                mode: Some(Mode::from_host(0o600)),
                hidden: true,
            },
        )
        .unwrap();
    volume.close_handle(file).unwrap();
    checked(&volume);

    let device = FaultDevice::new(upgraded, MountMode::ReadWrite);
    let media = device.media.clone();
    assert!(!Volume::upgrade(device).unwrap());
    assert!(root_writes(&media).is_empty());
}

#[test]
fn a_new_volume_needs_no_upgrade() {
    let device = FaultDevice::new(formatted(), MountMode::ReadWrite);
    assert!(!Volume::upgrade(device).unwrap());
}

#[test]
fn a_crash_during_upgrade_leaves_the_old_or_the_new_root() {
    for event in 0..5 {
        let device = FaultDevice::new(formatted_legacy(), MountMode::ReadWrite);
        let media = device.media.clone();
        media.borrow_mut().failure = Some(Failure {
            event,
            whole_mask: 0,
            tear: None,
        });
        let _ = Volume::upgrade(device);
        let durable = media.borrow().durable.clone();
        let mut volume = Volume::open(FaultDevice::new(durable, MountMode::ReadOnly)).unwrap();
        volume.check().unwrap();
    }
}
