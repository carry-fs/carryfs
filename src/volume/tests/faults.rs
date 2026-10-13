use super::*;

fn put(volume: &mut Volume<FaultDevice>, file: &str, byte: u8) -> Result<()> {
    let handle = volume.create_file(ROOT, name(file))?;
    volume.write(&handle, 0, &vec![byte; UNIT_BYTES])?;
    volume.close_handle(handle)
}

fn read(volume: &mut Volume<FaultDevice>, file: &str) -> Vec<u8> {
    let handle = volume.open_file(ROOT, &name(file), Access::Read).unwrap();
    let mut bytes = vec![0; UNIT_BYTES];
    assert_eq!(volume.read(&handle, 0, &mut bytes).unwrap(), UNIT_BYTES);
    volume.close_handle(handle).unwrap();
    bytes
}

fn baseline() -> BTreeMap<u64, Page> {
    let mut volume = opened();
    put(&mut volume, "anchor", 0x11).unwrap();
    put(&mut volume, "edited", 0x22).unwrap();
    put(&mut volume, "removed", 0x33).unwrap();
    volume.sync().unwrap();
    volume.into_model_image()
}

fn change(volume: &mut Volume<FaultDevice>) -> Result<()> {
    volume.set_deferred_checkpoints(true)?;
    for index in 0..4 {
        put(volume, &format!("batch{index}"), index + 0x70)?;
    }
    let handle = volume.open_file(ROOT, &name("edited"), Access::ReadWrite)?;
    volume.write(&handle, 0, &vec![0x44; UNIT_BYTES])?;
    volume.close_handle(handle)?;
    volume.remove(ROOT, &name("removed"), RemoveKind::File)?;
    volume.sync_publication()
}

fn assert_state(volume: &mut Volume<FaultDevice>, new: bool) {
    assert_eq!(read(volume, "anchor"), vec![0x11; UNIT_BYTES]);
    assert_eq!(
        read(volume, "edited"),
        vec![if new { 0x44 } else { 0x22 }; UNIT_BYTES]
    );
    if new {
        assert!(matches!(
            volume.lookup(ROOT, &name("removed")),
            Err(Error::NotFound)
        ));
    } else {
        assert_eq!(read(volume, "removed"), vec![0x33; UNIT_BYTES]);
    }
    for index in 0..4 {
        let file = format!("batch{index}");
        if new {
            assert_eq!(read(volume, &file), vec![index + 0x70; UNIT_BYTES]);
        } else {
            assert!(matches!(
                volume.lookup(ROOT, &name(&file)),
                Err(Error::NotFound)
            ));
        }
    }
}

fn verify(image: BTreeMap<u64, Page>, required_new: bool) -> Volume<FaultDevice> {
    let mut reader = Volume::open(FaultDevice::new(image.clone(), MountMode::ReadOnly)).unwrap();
    reader.check().unwrap();
    let summary = reader.scrub(&mut |finding| panic!("{finding:?}")).unwrap();
    assert_eq!(summary.unreadable_ranges, 0);
    assert_eq!(durable(&reader), image);
    let new = reader.lookup(ROOT, &name("batch0")).is_ok();
    assert!(!required_new || new, "lost the successful publication");
    assert_state(&mut reader, new);
    let mut writer = Volume::open(FaultDevice::new(image, MountMode::ReadWrite)).unwrap();
    assert_state(&mut writer, new);
    writer
}

fn pending_counts(events: &[Event]) -> Vec<usize> {
    let mut pending = 0;
    events
        .iter()
        .map(|event| match event {
            Event::Write(_) => {
                pending += 1;
                pending
            }
            Event::Flush => {
                let count = pending;
                pending = 0;
                count
            }
        })
        .collect()
}

#[test]
fn deferred_batch_output_cuts_and_two_page_tears_recover_old_or_new() {
    let base = baseline();
    let mut complete = Volume::open(FaultDevice::new(base.clone(), MountMode::ReadWrite)).unwrap();
    let media = complete.device.as_ref().unwrap().media.clone();
    media.borrow_mut().events.clear();
    change(&mut complete).unwrap();
    let events = media.borrow().events.clone();
    assert!(
        events
            .iter()
            .filter(|event| **event == Event::Flush)
            .count()
            >= 3
    );
    assert_state(&mut verify(durable(&complete), true), true);
    for (event, pending) in pending_counts(&events).into_iter().enumerate() {
        for whole_mask in [0, u64::MAX, 0x5555_5555_5555_5555, 0xAAAA_AAAA_AAAA_AAAA] {
            let mut volume =
                Volume::open(FaultDevice::new(base.clone(), MountMode::ReadWrite)).unwrap();
            let media = volume.device.as_ref().unwrap().media.clone();
            media.borrow_mut().events.clear();
            media.borrow_mut().failure = Some(Failure {
                event,
                whole_mask,
                tear: Some(Tear::Prefix {
                    write: 0,
                    bytes: 512,
                }),
            });
            if pending > 1 {
                media.borrow_mut().also_torn.push(Tear::Sectors {
                    write: pending - 1,
                    mask: 0xAA,
                });
            }
            assert!(
                change(&mut volume).is_err(),
                "event {event} mask {whole_mask:#x}"
            );
            assert!(matches!(volume.sync(), Err(Error::Faulted)));
            drop(volume);
            media.borrow_mut().crash();
            verify(media.borrow().durable.clone(), false);
        }
    }
    eprintln!(
        "batch output events={}, interruption cases={}",
        events.len(),
        events.len() * 4
    );
}

#[test]
fn cleanup_output_cuts_and_two_page_tears_preserve_sync_across_reopens() {
    let mut volume = Volume::open(FaultDevice::new(baseline(), MountMode::ReadWrite)).unwrap();
    change(&mut volume).unwrap();
    let base = durable(&volume);
    let media = volume.device.as_ref().unwrap().media.clone();
    assert!(volume.has_cleanup_work().unwrap());
    media.borrow_mut().events.clear();
    volume.sync().unwrap();
    let events = media.borrow().events.clone();
    assert!(!events.is_empty());
    for (event, pending) in pending_counts(&events).into_iter().enumerate() {
        for whole_mask in [0, u64::MAX] {
            let mut volume =
                Volume::open(FaultDevice::new(base.clone(), MountMode::ReadWrite)).unwrap();
            let media = volume.device.as_ref().unwrap().media.clone();
            media.borrow_mut().events.clear();
            media.borrow_mut().failure = Some(Failure {
                event,
                whole_mask,
                tear: Some(Tear::Sectors {
                    write: 0,
                    mask: 0x55,
                }),
            });
            if pending > 1 {
                media.borrow_mut().also_torn.push(Tear::Prefix {
                    write: pending - 1,
                    bytes: 512,
                });
            }
            assert!(volume.sync().is_err());
            drop(volume);
            media.borrow_mut().crash();
            let mut volume = verify(media.borrow().durable.clone(), true);
            // Interrupt two more maintenance attempts, each after a fresh open.
            for mask in [0, u64::MAX] {
                let media = volume.device.as_ref().unwrap().media.clone();
                media.borrow_mut().events.clear();
                if volume.has_cleanup_work().unwrap() {
                    media.borrow_mut().failure = Some(Failure {
                        event: 0,
                        whole_mask: mask,
                        tear: None,
                    });
                    assert!(volume.sync().is_err());
                }
                drop(volume);
                media.borrow_mut().crash();
                volume = verify(media.borrow().durable.clone(), true);
            }
            let mut steps = 0;
            while volume.has_cleanup_work().unwrap() {
                volume.sync().unwrap();
                steps += 1;
                assert!(steps <= 128, "fixture cleanup did not converge");
            }
            verify(durable(&volume), true);
        }
    }
    eprintln!(
        "cleanup output events={}, interruption cases={} with two further reopen cycles",
        events.len(),
        events.len() * 2
    );
}

#[test]
fn persistent_bad_sector_survives_crash_and_extraction_reports_only_the_gap() {
    let mut volume = opened();
    let (_, blocks) = backed_file(
        &mut volume,
        "damaged",
        (2 * UNIT_BYTES) as u64,
        &[(0, vec![0x51; UNIT_BYTES]), (1, vec![0x52; UNIT_BYTES])],
    );
    backed_file(
        &mut volume,
        "intact",
        UNIT_BYTES as u64,
        &[(0, vec![0x61; UNIT_BYTES])],
    );
    let device = volume
        .device
        .as_ref()
        .unwrap()
        .with_mode(MountMode::ReadOnly);
    let media = device.media.clone();
    let image = media.borrow().durable.clone();
    media
        .borrow_mut()
        .persistent_unreadable
        .insert(blocks[0] + 3);
    drop(volume);
    for _ in 0..3 {
        media.borrow_mut().crash();
        assert!(
            media
                .borrow()
                .persistent_unreadable
                .contains(&(blocks[0] + 3))
        );
        let mut reader = Volume::open(device.with_mode(MountMode::ReadOnly)).unwrap();
        let mut findings = Vec::new();
        let summary = reader
            .scrub(&mut |finding| findings.push((finding.object, finding.offset)))
            .unwrap();
        assert_eq!((summary.verified_units, summary.unreadable_ranges), (2, 1));
        assert_eq!(findings.len(), 1);
        let mut output = RecoveryOutput::new();
        let result = reader.extract_to(ROOT, &mut output, &mut |_| {}).unwrap();
        assert_eq!(
            (result.verified_bytes, result.unreadable_ranges),
            ((2 * UNIT_BYTES) as u64, 1)
        );
        let recovered = std::fs::read(output.root.join("damaged")).unwrap();
        assert_eq!(&recovered[..UNIT_BYTES], vec![0; UNIT_BYTES]);
        assert_eq!(&recovered[UNIT_BYTES..], vec![0x52; UNIT_BYTES]);
        assert_eq!(
            std::fs::read(output.root.join("intact")).unwrap(),
            vec![0x61; UNIT_BYTES]
        );
        assert_eq!(media.borrow().durable, image);
        assert!(media.borrow().events.is_empty());
    }
}

#[test]
fn wrong_address_reads_and_payload_replay_fail_checksums_without_verified_output() {
    let mut volume = opened();
    let (_, a) = backed_file(
        &mut volume,
        "a",
        UNIT_BYTES as u64,
        &[(0, vec![0x71; UNIT_BYTES])],
    );
    let (_, b) = backed_file(
        &mut volume,
        "b",
        UNIT_BYTES as u64,
        &[(0, vec![0x72; UNIT_BYTES])],
    );
    let image = volume.into_model_image();
    for redirect in [true, false] {
        let device = FaultDevice::new(image.clone(), MountMode::ReadOnly);
        let media = device.media.clone();
        if redirect {
            media.borrow_mut().redirected_read = Some((a[0], b[0]));
        } else {
            // A stale/wrong-address write leaves valid payload from another file.
            let stale = media.borrow().durable[&b[0]].clone();
            media.borrow_mut().durable.insert(a[0], stale);
        }
        let before = media.borrow().durable.clone();
        let mut reader = Volume::open(device).unwrap();
        reader.check().unwrap();
        let summary = reader.scrub(&mut |_| {}).unwrap();
        assert_eq!((summary.verified_units, summary.unreadable_ranges), (1, 1));
        let mut output = RecoveryOutput::new();
        let result = reader.extract_to(ROOT, &mut output, &mut |_| {}).unwrap();
        assert_eq!(
            (result.verified_bytes, result.unreadable_ranges),
            (UNIT_BYTES as u64, 1)
        );
        assert_eq!(
            std::fs::read(output.root.join("a")).unwrap(),
            vec![0; UNIT_BYTES]
        );
        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::MetadataExt::blocks(
                &std::fs::metadata(output.root.join("a")).unwrap()
            ),
            0
        );
        assert_eq!(
            std::fs::read(output.root.join("b")).unwrap(),
            vec![0x72; UNIT_BYTES]
        );
        assert_eq!(media.borrow().durable, before);
        assert!(media.borrow().events.is_empty());
    }
}

#[test]
fn stale_metadata_and_checksum_damage_reject_but_coherent_rollback_is_undetectable() {
    let mut volume = opened();
    let (_, blocks) = backed_file(
        &mut volume,
        "anchor",
        UNIT_BYTES as u64,
        &[(0, vec![0x81; UNIT_BYTES])],
    );
    volume.sync().unwrap();
    let old = durable(&volume);
    let old_catalog = volume.root.as_ref().unwrap().catalog.block;
    put(&mut volume, "later", 0x82).unwrap();
    volume.sync().unwrap();
    let current = durable(&volume);
    let catalog = volume.root.as_ref().unwrap().catalog.block;
    for replay in [true, false] {
        let mut damaged = current.clone();
        if replay {
            damaged.insert(catalog, old[&old_catalog].clone());
        } else {
            damaged.get_mut(&catalog).unwrap().0[64] ^= 1;
        }
        // Unrelated catalog damage leaves these payload bytes intact, but
        // ordinary extraction cannot trust a namespace whose check failed.
        for offset in 0..UNIT_BYTES / BLOCK_BYTES {
            assert_eq!(
                &damaged[&(blocks[0] + offset as u64)].0[..],
                &[0x81; BLOCK_BYTES]
            );
        }
        let mut output = RecoveryOutput::new();
        match Volume::open(FaultDevice::new(damaged.clone(), MountMode::ReadOnly)) {
            Ok(mut reader) => {
                assert!(matches!(
                    reader.check(),
                    Err(Error::Corrupt(_) | Error::Corruption { .. })
                ));
                let result = reader.extract_to(ROOT, &mut output, &mut |_| {}).unwrap();
                assert!(!result.complete);
                assert_eq!((result.objects, result.verified_bytes), (0, 0));
            }
            Err(error) => assert!(
                matches!(error, Error::Corrupt(_) | Error::Corruption { .. }),
                "{error:?}"
            ),
        }
        assert!(!output.root.exists());
        assert!(matches!(
            Volume::open(FaultDevice::new(damaged, MountMode::ReadWrite)),
            Err(Error::Corrupt(_) | Error::Corruption { .. })
        ));
    }
    let mut rollback = Volume::open(FaultDevice::new(old, MountMode::ReadOnly)).unwrap();
    rollback.check().unwrap();
    assert_eq!(rollback.scrub(&mut |_| {}).unwrap().unreadable_ranges, 0);
    assert_eq!(read(&mut rollback, "anchor"), vec![0x81; UNIT_BYTES]);
    assert!(matches!(
        rollback.lookup(ROOT, &name("later")),
        Err(Error::NotFound)
    ));
}

const REPLACEMENT: u8 = 0x53;

/// Overwrite `target` and publish while `redirect` sends one write to the wrong
/// block. Every device call succeeds, so the volume sees no failure.
fn overwrite_target(
    base: &BTreeMap<u64, Page>,
    redirect: Option<(u64, u64)>,
) -> Volume<FaultDevice> {
    let mut volume = Volume::open(FaultDevice::new(base.clone(), MountMode::ReadWrite)).unwrap();
    volume
        .device
        .as_ref()
        .unwrap()
        .media
        .borrow_mut()
        .redirected_write = redirect;
    volume.set_deferred_checkpoints(true).unwrap();
    let handle = volume
        .open_file(ROOT, &name("target"), Access::ReadWrite)
        .unwrap();
    volume
        .write(&handle, 0, &vec![REPLACEMENT; UNIT_BYTES])
        .unwrap();
    volume.close_handle(handle).unwrap();
    volume.sync_publication().unwrap();
    volume
}

#[test]
fn wrong_address_writes_are_reported_by_scrub_or_rejected_but_never_repaired() {
    let mut volume = opened();
    let (_, victim) = backed_file(
        &mut volume,
        "victim",
        UNIT_BYTES as u64,
        &[(0, vec![0x51; UNIT_BYTES])],
    );
    backed_file(
        &mut volume,
        "target",
        UNIT_BYTES as u64,
        &[(0, vec![0x52; UNIT_BYTES])],
    );
    volume.sync().unwrap();
    let base = durable(&volume);
    // A clean run names the replacement blocks the misdirected runs aim at.
    let clean = overwrite_target(&base, None);
    let catalog = clean.root.as_ref().unwrap().catalog.block;
    let data = *durable(&clean)
        .iter()
        .find(|(_, page)| page.0[..] == [REPLACEMENT; BLOCK_BYTES])
        .unwrap()
        .0;
    // The writer sees success in every case; only a later open, check, or
    // scrub can tell. Nothing detects the write at the moment it goes wrong.
    {
        let image = durable(&overwrite_target(&base, Some((data, victim[0]))));
        let mut reader =
            Volume::open(FaultDevice::new(image.clone(), MountMode::ReadOnly)).unwrap();
        reader.check().unwrap();
        // The victim's block now holds another file's bytes, and the target's
        // new block was never written. Each file reports its own gap.
        let summary = reader.scrub(&mut |_| {}).unwrap();
        assert_eq!((summary.verified_units, summary.unreadable_ranges), (0, 2));
        for file in ["victim", "target"] {
            let handle = reader.open_file(ROOT, &name(file), Access::Read).unwrap();
            let mut bytes = vec![0; UNIT_BYTES];
            let error = reader.read(&handle, 0, &mut bytes).unwrap_err();
            assert!(
                matches!(error, Error::Corruption { .. }),
                "{file}: {error:?}"
            );
            reader.close_handle(handle).unwrap();
        }
        let mut output = RecoveryOutput::new();
        let result = reader.extract_to(ROOT, &mut output, &mut |_| {}).unwrap();
        assert_eq!((result.verified_bytes, result.unreadable_ranges), (0, 2));
        // The damage stays: a writable reopen neither repairs nor hides it.
        let mut writer = Volume::open(FaultDevice::new(image, MountMode::ReadWrite)).unwrap();
        writer.sync().unwrap();
        let mut again =
            Volume::open(FaultDevice::new(durable(&writer), MountMode::ReadOnly)).unwrap();
        again.check().unwrap();
        assert_eq!(again.scrub(&mut |_| {}).unwrap().unreadable_ranges, 2);
    }
    // A tree page that lands elsewhere leaves the new root pointing at a block
    // that is not that page, and a data unit that lands on a live tree page
    // destroys it. Either way the only root generation left is unusable.
    for (case, requested, landed) in [
        ("data over the live catalog page", data, catalog),
        ("catalog page over another file's data", catalog, victim[0]),
    ] {
        let image = durable(&overwrite_target(&base, Some((requested, landed))));
        for mode in [MountMode::ReadOnly, MountMode::ReadWrite] {
            let opened = Volume::open(FaultDevice::new(image.clone(), mode));
            assert!(
                matches!(opened.as_ref().map(|_| ()), Err(Error::Corrupt(_))),
                "{case}: {:?}",
                opened.map(|_| ())
            );
        }
    }
}
