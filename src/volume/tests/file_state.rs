use super::*;

#[test]
fn file_operations_match_their_oracle() {
    // Partial tails, sparse growth, replacement with an open destination,
    // unlink with a pin, writes through the pin, sync and crash.
    let actions = [
        [0, 0, 0, 0],
        [1, 0, 3, 2],
        [3, 0, 3, 0],
        [3, 0, 5, 0],
        [0, 1, 0, 0],
        [1, 1, 0, 1],
        [6, 1, 0, 0],
        [4, 0, 1, 0],
        [7, 0, 5, 0],
        [5, 1, 0, 0],
        [9, 0, 0, 0],
        [8, 0, 0, 0],
        [10, 0, 0, 0],
    ];
    for deferred in [false, true] {
        // A no-op or empty crash selects the mode without changing files.
        let mut input = vec![if deferred { 11 } else { 10 }, 0, 0, 0];
        input.extend_from_slice(&actions.concat());
        model::fuzz_files(&input);
    }
    let mut state = 0x42f0_9ab1_0ec3_2715_u64;
    for _ in 0..100 {
        let input: Vec<u8> = (0..256)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                (state >> 56) as u8
            })
            .collect();
        model::fuzz_files(&input);
    }
}

fn slots(volume: &mut Volume<FaultDevice>) -> usize {
    let root = volume.root.clone().unwrap();
    crate::workspace::read(volume.device.as_mut().unwrap(), &root)
        .unwrap()
        .len()
}

fn drain(volume: &mut Volume<FaultDevice>) {
    for _ in 0..10_000 {
        let before = volume.info().unwrap().generation;
        volume.sync().unwrap();
        if volume.info().unwrap().generation == before {
            assert_eq!(full_disk::counts(volume), [0; 3]);
            return;
        }
    }
    panic!("maintenance did not finish");
}

/// Each fixture is reached through file operations. No allocation bitmaps,
/// workspace slots, or cleanup records are manufactured by the test.
#[test]
fn interrupted_foreground_cleanup_preserves_files_and_refills() {
    for kind in ["refill", "retirement", "detach"] {
        let mut volume = opened();
        let file = volume.create_file(ROOT, name("keep")).unwrap();
        volume.write(&file, 0, &vec![0x59; 1 << 20]).unwrap();
        drain(&mut volume);
        let expected = if kind == "detach" {
            volume.truncate(&file, 100).unwrap();
            assert!(full_disk::counts(&mut volume)[1] > 0);
            vec![0x59; 100]
        } else {
            volume.write(&file, 0, &[0x73; 4096]).unwrap();
            let mut bytes = vec![0x59; 1 << 20];
            bytes[..4096].fill(0x73);
            assert!(full_disk::counts(&mut volume)[0] > 0);
            bytes
        };
        if kind == "refill" {
            for index in 0..10_000 {
                if slots(&mut volume) < 8192 - 128 {
                    break;
                }
                volume
                    .create_directory(ROOT, name(&format!("d{index:05}")))
                    .unwrap();
            }
            assert!(slots(&mut volume) < 8192 - 128, "did not reach refill");
        }
        volume.close_handle(file).unwrap();
        let base = durable(&volume);
        let before_slots = slots(&mut volume);
        let generation = volume.info().unwrap().generation;
        let media = volume.device.as_ref().unwrap().media.clone();
        media.borrow_mut().events.clear();
        volume.sync().unwrap();
        let events = media.borrow().events.clone();
        assert!(events.len() > 5, "fixture did not publish");
        if kind == "refill" {
            assert!(slots(&mut volume) > before_slots);
        } else {
            assert!(
                volume.info().unwrap().free_blocks
                    > Root::decode(&base[&ROOT_A].0[..], ROOT_A, GEOMETRY)
                        .unwrap()
                        .free_blocks
            );
        }
        // Root tears and replacement tears accompany every output failure.
        // Whole masks select none, all, and non-prefix pending writes.
        let mut cases = 0;
        for event in 0..events.len() {
            for (whole_mask, tear) in [
                (0, None),
                (u64::MAX, None),
                (0xaaaa_aaaa_aaaa_aaaa, None),
                (
                    0,
                    Some(Tear::Prefix {
                        write: 0,
                        bytes: 2049,
                    }),
                ),
                (
                    0,
                    Some(Tear::Sectors {
                        write: 0,
                        mask: 0x55,
                    }),
                ),
            ] {
                let mut volume =
                    Volume::open(FaultDevice::new(base.clone(), MountMode::ReadWrite)).unwrap();
                let media = volume.device.as_ref().unwrap().media.clone();
                media.borrow_mut().failure = Some(Failure {
                    event,
                    whole_mask,
                    tear,
                });
                assert!(volume.sync().is_err(), "{kind} event {event}");
                let count = media.borrow().events.len();
                assert!(matches!(volume.sync(), Err(Error::Faulted)));
                assert!(matches!(
                    volume.create_directory(ROOT, name("faulted")),
                    Err(Error::Faulted)
                ));
                assert_eq!(media.borrow().events.len(), count);
                drop(volume);
                media.borrow_mut().crash();
                let recovered = media.borrow().durable.clone();
                let mut reader =
                    Volume::open(FaultDevice::new(recovered.clone(), MountMode::ReadOnly)).unwrap();
                reader.check().unwrap();
                assert!((generation..=generation + 1).contains(&reader.info().unwrap().generation));
                let handle = reader.open_file(ROOT, &name("keep"), Access::Read).unwrap();
                assert_eq!(read_all(&mut reader, &handle, expected.len()), expected);
                assert!(
                    reader
                        .device
                        .as_ref()
                        .unwrap()
                        .media
                        .borrow()
                        .events
                        .is_empty()
                );
                let mut writer =
                    Volume::open(FaultDevice::new(recovered, MountMode::ReadWrite)).unwrap();
                drain(&mut writer);
                let file = writer
                    .open_file(ROOT, &name("keep"), Access::ReadWrite)
                    .unwrap();
                writer
                    .write(&file, expected.len() as u64 + 123, &[0x39; 4096])
                    .unwrap();
                let root = writer.root.clone().unwrap();
                let summary =
                    crate::check::validate_ownership(writer.device.as_mut().unwrap(), &root)
                        .unwrap();
                assert!(
                    summary.free_blocks - slots(&mut writer) as u64 >= crate::allocation::RESERVE
                );
                assert_eq!(read_all(&mut writer, &file, expected.len()), expected);
                checked(&writer);
                cases += 1;
            }
        }
        eprintln!(
            "{kind}: {} events, {cases} interrupted recoveries",
            events.len()
        );
    }
}

#[test]
#[ignore = "foreground full-disk fill/delete followed by every refill interruption"]
fn interrupted_full_disk_refill() {
    let mut volume = opened();
    let keep = volume.create_file(ROOT, name("keep")).unwrap();
    volume.write(&keep, 0, b"p").unwrap();
    volume.close_handle(keep).unwrap();
    let file = volume.create_file(ROOT, name("fill")).unwrap();
    let chunk = vec![0x69; 1 << 20];
    let mut size = 0;
    loop {
        match volume.write(&file, size, &chunk) {
            Ok(written) => size += written as u64,
            Err(Error::NoSpace) => break,
            Err(error) => panic!("fill: {error:?}"),
        }
    }
    assert!(size > 100 << 20);
    let info = volume.info().unwrap();
    assert!(info.free_blocks - slots(&mut volume) as u64 >= crate::allocation::RESERVE);
    volume.close_handle(file).unwrap();
    volume
        .remove(ROOT, &name("fill"), RemoveKind::File)
        .unwrap();
    drain(&mut volume);
    let base = durable(&volume);
    drop(volume);
    namespace_crash_matrix(
        &base,
        |volume| {
            let file = volume.open_file(ROOT, &name("keep"), Access::ReadWrite)?;
            volume.write(&file, 65_535, &[0x39; 4096])?;
            volume.close_handle(file)
        },
        |volume| {
            let file = volume.open_file(ROOT, &name("keep"), Access::Read).unwrap();
            let size = volume.handle_attributes(&file).unwrap().size as usize;
            assert!([1, 69_631].contains(&size));
            let bytes = read_all(volume, &file, size);
            assert_eq!(bytes[0], b'p');
            if size > 1 {
                assert!(bytes[1..65_535].iter().all(|b| *b == 0));
                assert!(bytes[65_535..].iter().all(|b| *b == 0x39));
            }
            volume.close_handle(file).unwrap();
            size > 1
        },
    );
    eprintln!("filled {size} bytes, deleted, drained, and interrupted every refill output");
}

#[test]
fn hole_punch_preserves_edges_size_and_survives_remount() {
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("punch")).unwrap();
    let mut expected = vec![0x5a; 3 * UNIT_BYTES + 19];
    volume.write(&file, 0, &expected).unwrap();
    let before = volume.handle_attributes(&file).unwrap();
    let offset = 17;
    let end = 3 * UNIT_BYTES + 7;
    volume
        .punch_hole(&file, offset, (end - offset as usize) as u64)
        .unwrap();
    expected[offset as usize..end].fill(0);
    let after = volume.handle_attributes(&file).unwrap();
    assert_eq!(after.size, before.size);
    assert_eq!(
        before.allocated_bytes - after.allocated_bytes,
        (2 * UNIT_BYTES) as u64
    );
    let mut actual = vec![0; expected.len()];
    volume.read(&file, 0, &mut actual).unwrap();
    assert_eq!(actual, expected);
    volume
        .punch_hole(&file, (3 * UNIT_BYTES) as u64, UNIT_BYTES as u64)
        .unwrap();
    expected[3 * UNIT_BYTES..].fill(0);
    volume.punch_hole(&file, u64::from(1u32 << 30), 1).unwrap();
    assert!(volume.punch_hole(&file, 0, 0).is_err());
    assert!(volume.punch_hole(&file, u64::MAX, 2).is_err());
    volume.close_handle(file).unwrap();
    drain(&mut volume);
    checked(&volume);
    let mut reader = Volume::open(FaultDevice::new(durable(&volume), MountMode::ReadOnly)).unwrap();
    let file = reader
        .open_file(ROOT, &name("punch"), Access::Read)
        .unwrap();
    reader.read(&file, 0, &mut actual).unwrap();
    assert_eq!(actual, expected);
    assert!(matches!(
        reader.punch_hole(&file, 0, 1),
        Err(Error::ReadOnly)
    ));
}

#[test]
fn preallocation_survives_remount_and_is_consumed_without_exposing_device_bytes() {
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("reserved")).unwrap();
    volume
        .preallocate(&file, 0, (3 * UNIT_BYTES) as u64, true)
        .unwrap();
    assert_eq!(volume.handle_attributes(&file).unwrap().size, 0);
    assert_eq!(
        volume.handle_attributes(&file).unwrap().allocated_bytes,
        (3 * UNIT_BYTES) as u64
    );
    volume.close_handle(file).unwrap();
    drain(&mut volume);
    checked(&volume);
    let mut volume =
        Volume::open(FaultDevice::new(durable(&volume), MountMode::ReadWrite)).unwrap();
    let file = volume
        .open_file(ROOT, &name("reserved"), Access::ReadWrite)
        .unwrap();
    volume.write(&file, 17, b"hello").unwrap();
    assert_eq!(
        volume.handle_attributes(&file).unwrap().allocated_bytes,
        (3 * UNIT_BYTES) as u64
    );
    volume.truncate(&file, (3 * UNIT_BYTES) as u64).unwrap();
    let mut bytes = vec![0; 3 * UNIT_BYTES];
    volume.read(&file, 0, &mut bytes).unwrap();
    let mut expected = bytes.clone();
    expected.fill(0);
    expected[17..22].copy_from_slice(b"hello");
    assert_eq!(bytes, expected);
    volume
        .write(&file, (UNIT_BYTES + 3) as u64, b"second")
        .unwrap();
    volume
        .punch_hole(&file, (2 * UNIT_BYTES) as u64, UNIT_BYTES as u64)
        .unwrap();
    assert_eq!(
        volume.handle_attributes(&file).unwrap().allocated_bytes,
        (2 * UNIT_BYTES) as u64
    );
    volume.close_handle(file).unwrap();
    drain(&mut volume);
    checked(&volume);
}

#[test]
fn preallocation_preserves_data_and_truncation_discards_unused_reservations() {
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("reserved")).unwrap();
    volume.write(&file, 0, b"keep me").unwrap();
    volume
        .preallocate(&file, 0, (3 * UNIT_BYTES) as u64, true)
        .unwrap();
    let mut bytes = [0; 7];
    volume.read(&file, 0, &mut bytes).unwrap();
    assert_eq!(&bytes, b"keep me");
    volume
        .punch_hole(&file, (2 * UNIT_BYTES) as u64, UNIT_BYTES as u64)
        .unwrap();
    assert_eq!(
        volume.handle_attributes(&file).unwrap().allocated_bytes,
        (2 * UNIT_BYTES) as u64
    );
    volume.truncate(&file, 7).unwrap();
    assert_eq!(
        volume.handle_attributes(&file).unwrap().allocated_bytes,
        BLOCK_BYTES as u64
    );
    volume
        .preallocate(&file, UNIT_BYTES as u64, UNIT_BYTES as u64, false)
        .unwrap();
    assert_eq!(
        volume.handle_attributes(&file).unwrap().size,
        (2 * UNIT_BYTES) as u64
    );
    volume.close_handle(file).unwrap();
    drain(&mut volume);
    checked(&volume);
}

#[test]
fn preallocated_writes_and_overwrites_work_after_other_files_fill_the_volume() {
    let geometry = GEOMETRY;
    let device = FaultDevice::with_geometry(BTreeMap::new(), MountMode::ReadWrite, geometry);
    let media = device.media.clone();
    Volume::format_at(device, UUID, 1, 0).unwrap();
    let mut volume = Volume::open(FaultDevice::with_geometry(
        media.borrow().durable.clone(),
        MountMode::ReadWrite,
        geometry,
    ))
    .unwrap();
    let reserved = volume.create_file(ROOT, name("reserved")).unwrap();
    volume
        .preallocate(&reserved, 0, (1 << 20) as u64, true)
        .unwrap();
    let filler = volume.create_file(ROOT, name("filler")).unwrap();
    let bytes = vec![0x53; 1 << 20];
    let mut size = 0;
    loop {
        match volume.write(&filler, size, &bytes) {
            Ok(done) => size += done as u64,
            Err(Error::NoSpace) => break,
            Err(error) => panic!("fill failed: {error}"),
        }
    }
    assert!(size > 0);
    assert_eq!(volume.write(&reserved, 0, &bytes).unwrap(), bytes.len());
    for byte in 0..8 {
        assert_eq!(
            volume.write(&reserved, 0, &vec![byte; UNIT_BYTES]).unwrap(),
            UNIT_BYTES
        );
        volume.sync().unwrap();
    }
    let mut read = vec![0; UNIT_BYTES];
    volume.read(&reserved, 0, &mut read).unwrap();
    assert_eq!(read, vec![7; UNIT_BYTES]);
    volume.close_handle(filler).unwrap();
    volume.close_handle(reserved).unwrap();
    volume.sync().unwrap();
    checked(&volume);
}

#[test]
fn interrupted_unwritten_conversion_recovers_zeros_or_complete_data() {
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("reserved")).unwrap();
    volume
        .preallocate(&file, 0, UNIT_BYTES as u64, false)
        .unwrap();
    volume.close_handle(file).unwrap();
    drain(&mut volume);
    let base = durable(&volume);
    let mut complete = Volume::open(FaultDevice::new(base.clone(), MountMode::ReadWrite)).unwrap();
    let file = complete
        .open_file(ROOT, &name("reserved"), Access::ReadWrite)
        .unwrap();
    let media = complete.device.as_ref().unwrap().media.clone();
    media.borrow_mut().events.clear();
    complete.write(&file, 0, &vec![0x67; UNIT_BYTES]).unwrap();
    let count = media.borrow().events.len();
    for event in 0..count {
        for whole_mask in [0, u64::MAX] {
            let mut volume =
                Volume::open(FaultDevice::new(base.clone(), MountMode::ReadWrite)).unwrap();
            let file = volume
                .open_file(ROOT, &name("reserved"), Access::ReadWrite)
                .unwrap();
            let media = volume.device.as_ref().unwrap().media.clone();
            media.borrow_mut().events.clear();
            media.borrow_mut().failure = Some(Failure {
                event,
                whole_mask,
                tear: None,
            });
            let _ = volume.write(&file, 0, &vec![0x67; UNIT_BYTES]);
            media.borrow_mut().crash();
            let mut reader = Volume::open(FaultDevice::new(
                media.borrow().durable.clone(),
                MountMode::ReadOnly,
            ))
            .unwrap();
            reader.check().unwrap();
            let file = reader
                .open_file(ROOT, &name("reserved"), Access::Read)
                .unwrap();
            let mut bytes = vec![0; UNIT_BYTES];
            reader.read(&file, 0, &mut bytes).unwrap();
            assert!(bytes.iter().all(|byte| *byte == 0) || bytes.iter().all(|byte| *byte == 0x67));
            reader.close_handle(file).unwrap();
        }
    }
    complete.close_handle(file).unwrap();
}
