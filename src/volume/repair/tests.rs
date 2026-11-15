use super::*;
use crate::Access;
use crate::volume::model::{Failure, FaultDevice, GEOMETRY, Tear, formatted_volume};

fn fixture() -> (BTreeMap<u64, Page>, u64, Vec<u8>) {
    let mut volume = formatted_volume();
    let directory = volume
        .create_directory(super::super::ROOT, Name::parse("docs").unwrap())
        .unwrap();
    let file = volume
        .create_file(directory, Name::parse("notes").unwrap())
        .unwrap();
    let id = volume
        .lookup(directory, &Name::parse("notes").unwrap())
        .unwrap()
        .get();
    let bytes: Vec<_> = (0..150_001).map(|i| (i % 251) as u8).collect();
    volume.write(&file, 0, &bytes).unwrap();
    volume.close_handle(file).unwrap();
    volume
        .create_symlink(directory, Name::parse("link").unwrap(), b"notes")
        .unwrap();
    volume.sync().unwrap();
    (volume.into_model_image(), id, bytes)
}

fn root(image: &BTreeMap<u64, Page>) -> Root {
    Root::decode(&image[&ROOT_A].0[..], ROOT_A, GEOMETRY).unwrap()
}

fn edit_catalog(image: &mut BTreeMap<u64, Page>, mut edit: impl FnMut(&mut Vec<Record>)) {
    let root = root(image);
    let mut device = FaultDevice::new(image.clone(), MountMode::ReadOnly);
    let scan = Scan::read(
        &mut device,
        &root,
        root.catalog,
        TreeKind::Catalog,
        &mut RepairPlan::default(),
    )
    .unwrap();
    for block in scan.pages {
        let page = &image[&block];
        let reference = TreeRef {
            block,
            generation: u64_le(&page.0[24..32]).unwrap(),
        };
        let decoded = decode_page(
            &page.0[..],
            reference,
            &root.uuid,
            TreeKind::Catalog,
            root.generation,
            None,
            root.blocks,
        )
        .unwrap();
        if decoded.level != 0 {
            continue;
        }
        let mut records: Vec<_> = decoded
            .records
            .iter()
            .map(|(k, v)| (k.to_vec(), v.to_vec()))
            .collect();
        edit(&mut records);
        let borrowed: Vec<_> = records
            .iter()
            .map(|(k, v)| (k.as_slice(), v.as_slice()))
            .collect();
        image.insert(
            block,
            crate::format::encode_leaf(
                reference,
                &root.uuid,
                TreeKind::Catalog,
                &borrowed,
                root.blocks,
            )
            .unwrap(),
        );
    }
}

fn assert_file(device: FaultDevice, path: &[&str], expected: &[u8]) {
    let mut volume = Volume::open(device).unwrap();
    volume.check().unwrap();
    let mut parent = super::super::ROOT;
    for name in &path[..path.len() - 1] {
        parent = volume.lookup(parent, &Name::parse(name).unwrap()).unwrap();
    }
    let file = volume
        .open_file(
            parent,
            &Name::parse(path.last().unwrap()).unwrap(),
            Access::Read,
        )
        .unwrap();
    let mut bytes = vec![0; expected.len()];
    assert_eq!(volume.read(&file, 0, &mut bytes).unwrap(), expected.len());
    assert_eq!(bytes, expected);
    volume.close_handle(file).unwrap();
}

#[test]
fn diagnosis_is_readonly_repair_rebuilds_allocation_and_is_idempotent() {
    let (mut image, _, bytes) = fixture();
    let root = root(&image);
    image.get_mut(&root.allocation.block).unwrap().0[0] ^= 1;
    for mode in [MountMode::ReadOnly, MountMode::ReadWrite] {
        let device = FaultDevice::new(image.clone(), mode);
        let media = device.media.clone();
        let repair = Repair::diagnose(device).unwrap();
        assert!(!repair.plan().clean);
        assert!(!repair.plan().data_loss);
        assert!(repair.plan().blocked.is_none(), "{:?}", repair.plan());
        assert!(media.borrow().events.is_empty());
        assert_eq!(media.borrow().durable, image);
        if mode == MountMode::ReadOnly {
            assert!(matches!(repair.apply(false), Err(Error::ReadOnly)));
        } else {
            assert!(repair.apply(false).unwrap());
            let repaired = media.borrow().durable.clone();
            let device = FaultDevice::new(repaired.clone(), MountMode::ReadWrite);
            let events = device.media.clone();
            let repair = Repair::diagnose(device).unwrap();
            assert!(repair.plan().clean);
            assert!(!repair.apply(false).unwrap());
            assert!(events.borrow().events.is_empty());
            assert_file(
                FaultDevice::new(repaired, MountMode::ReadOnly),
                &["docs", "notes"],
                &bytes,
            );
        }
    }
}

#[test]
fn reconnects_orphans_and_missing_entries_without_changing_data() {
    let (mut image, id, bytes) = fixture();
    edit_catalog(&mut image, |records| {
        records.retain(|(key, value)| !(key[8] == 2 && u64_le(&value[..8]).unwrap() == id));
        for (key, value) in records {
            if key == &inode_key(id) {
                value[1] |= 1;
            }
        }
    });
    let device = FaultDevice::new(image, MountMode::ReadWrite);
    let media = device.media.clone();
    let repair = Repair::diagnose(device).unwrap();
    assert!(!repair.plan().data_loss);
    assert!(
        repair
            .plan()
            .actions
            .iter()
            .any(|action| action.contains(&format!("/lost+found/{id}")))
    );
    assert!(repair.plan().blocked.is_none(), "{:?}", repair.plan());
    repair.apply(false).unwrap();
    assert_file(
        FaultDevice::new(media.borrow().durable.clone(), MountMode::ReadOnly),
        &["lost+found", &id.to_string()],
        &bytes,
    );
}

#[test]
fn missing_checksums_require_explicit_loss_acceptance() {
    let (mut image, id, _) = fixture();
    edit_catalog(&mut image, |records| {
        records.retain(|(key, _)| !(key[8] == 4 && u64_be(&key[..8]).unwrap() == id))
    });
    let device = FaultDevice::new(image.clone(), MountMode::ReadWrite);
    let media = device.media.clone();
    let repair = Repair::diagnose(device).unwrap();
    assert!(repair.plan().data_loss);
    assert!(repair.plan().blocked.is_none(), "{:?}", repair.plan());
    assert!(repair.apply(false).is_err());
    assert!(media.borrow().events.is_empty());
    assert_eq!(media.borrow().durable, image);
    let repair = Repair::diagnose(FaultDevice::new(image, MountMode::ReadWrite)).unwrap();
    let media = repair.device.media.clone();
    repair.apply(true).unwrap();
    Volume::open(FaultDevice::new(
        media.borrow().durable.clone(),
        MountMode::ReadOnly,
    ))
    .unwrap()
    .check()
    .unwrap();
}

#[test]
fn broken_catalog_leaf_preserves_independent_siblings() {
    let mut volume = formatted_volume();
    volume.set_deferred_checkpoints(true).unwrap();
    for i in 0..120 {
        let handle = volume
            .create_file(
                super::super::ROOT,
                Name::parse(&format!("f{i:03}")).unwrap(),
            )
            .unwrap();
        volume.close_handle(handle).unwrap();
    }
    volume.sync().unwrap();
    let mut image = volume.into_model_image();
    let root = root(&image);
    let page = &image[&root.catalog.block];
    let decoded = decode_page(
        &page.0[..],
        root.catalog,
        &root.uuid,
        TreeKind::Catalog,
        root.generation,
        None,
        root.blocks,
    )
    .unwrap();
    assert!(decoded.level > 0);
    let bad = tree_ref(decoded.records.last().unwrap().1).unwrap().block;
    image.get_mut(&bad).unwrap().0[0] ^= 1;
    let device = FaultDevice::new(image, MountMode::ReadWrite);
    let media = device.media.clone();
    let repair = Repair::diagnose(device).unwrap();
    assert!(repair.plan().data_loss);
    assert!(repair.plan().objects > 1 && repair.plan().objects < 121);
    assert!(repair.plan().blocked.is_none(), "{:?}", repair.plan());
    repair.apply(true).unwrap();
    Volume::open(FaultDevice::new(
        media.borrow().durable.clone(),
        MountMode::ReadOnly,
    ))
    .unwrap()
    .check()
    .unwrap();
}

#[test]
fn damaged_workspace_blocks_writes_but_keeps_export_available() {
    let (mut image, _, _) = fixture();
    let root = root(&image);
    image.get_mut(&root.workspace.block).unwrap().0[0] ^= 1;
    let device = FaultDevice::new(image.clone(), MountMode::ReadWrite);
    let media = device.media.clone();
    let repair = Repair::diagnose(device).unwrap();
    assert!(repair.plan().blocked.is_some());
    assert!(repair.plan().recovery_available, "{:?}", repair.plan());
    assert!(repair.apply(true).is_err());
    assert!(media.borrow().events.is_empty());
    assert_eq!(media.borrow().durable, image);
}

#[test]
fn every_repair_write_and_barrier_can_be_interrupted_and_retried() {
    let (mut image, id, bytes) = fixture();
    edit_catalog(&mut image, |records| {
        records.retain(|(key, value)| !(key[8] == 2 && u64_le(&value[..8]).unwrap() == id))
    });
    let root = root(&image);
    // Only A is current: mirroring B must precede all workspace reuse.
    image.get_mut(&(root.blocks - 257)).unwrap().0[0] ^= 1;
    let device = FaultDevice::new(image.clone(), MountMode::ReadWrite);
    let media = device.media.clone();
    Repair::diagnose(device).unwrap().apply(false).unwrap();
    let events = media.borrow().events.clone();
    let mut pending = 0usize;
    for (event, kind) in events.iter().enumerate() {
        if matches!(kind, crate::volume::model::Event::Write(_)) {
            pending += 1;
        }
        for whole_mask in [0, u64::MAX, 0x5555_5555_5555_5555] {
            for tear in [
                None,
                Some(Tear::Prefix {
                    write: pending.saturating_sub(1),
                    bytes: 64,
                }),
                Some(Tear::Sectors {
                    write: pending.saturating_sub(1),
                    mask: 0x55,
                }),
            ] {
                let device = FaultDevice::new(image.clone(), MountMode::ReadWrite);
                let media = device.media.clone();
                let repair = Repair::diagnose(device).unwrap();
                media.borrow_mut().failure = Some(Failure {
                    event,
                    whole_mask,
                    tear,
                });
                assert!(repair.apply(false).is_err());
                media.borrow_mut().crash();
                let recovered = media.borrow().durable.clone();
                let device = FaultDevice::new(recovered, MountMode::ReadWrite);
                let media = device.media.clone();
                let repair = Repair::diagnose(device).unwrap();
                assert!(
                    repair.plan().blocked.is_none(),
                    "event {event}: {:?}",
                    repair.plan()
                );
                repair.apply(false).unwrap();
                assert_file(
                    FaultDevice::new(media.borrow().durable.clone(), MountMode::ReadOnly),
                    &["lost+found", &id.to_string()],
                    &bytes,
                );
            }
        }
        if matches!(kind, crate::volume::model::Event::Flush) {
            pending = 0;
        }
    }
}

#[test]
fn rejects_conflicting_roots_without_output() {
    let (mut image, _, _) = fixture();
    let mut other = root(&image);
    other.next_job += 1;
    let block = other.blocks - 257;
    image.insert(block, other.encode(block, GEOMETRY).unwrap());
    let device = FaultDevice::new(image.clone(), MountMode::ReadWrite);
    let media = device.media.clone();
    assert!(Repair::diagnose(device).is_err());
    assert!(media.borrow().events.is_empty());
    assert_eq!(media.borrow().durable, image);
}

#[test]
fn repairs_free_counts_and_preserves_preallocation() {
    let mut volume = formatted_volume();
    let handle = volume
        .create_file(super::super::ROOT, Name::parse("preallocated").unwrap())
        .unwrap();
    volume.preallocate(&handle, 0, 3 * 65_536, true).unwrap();
    volume.write(&handle, 65_536, b"tail").unwrap();
    volume.close_handle(handle).unwrap();
    volume.sync().unwrap();
    let mut image = volume.into_model_image();
    let mut root = root(&image);
    root.free_blocks = 0;
    for block in [ROOT_A, root.blocks - 257] {
        image.insert(block, root.encode(block, GEOMETRY).unwrap());
    }
    let device = FaultDevice::new(image, MountMode::ReadWrite);
    let media = device.media.clone();
    let repair = Repair::diagnose(device).unwrap();
    assert!(!repair.plan().data_loss);
    assert!(repair.plan().blocked.is_none(), "{:?}", repair.plan());
    repair.apply(false).unwrap();
    let mut volume = Volume::open(FaultDevice::new(
        media.borrow().durable.clone(),
        MountMode::ReadOnly,
    ))
    .unwrap();
    volume.check().unwrap();
    let handle = volume
        .open_file(
            super::super::ROOT,
            &Name::parse("preallocated").unwrap(),
            Access::Read,
        )
        .unwrap();
    assert_eq!(volume.allocation_end(&handle).unwrap(), 3 * 65_536);
    let mut bytes = vec![0; 65_540];
    assert_eq!(volume.read(&handle, 0, &mut bytes).unwrap(), bytes.len());
    assert!(bytes[..65_536].iter().all(|b| *b == 0));
    assert_eq!(&bytes[65_536..], b"tail");
}

#[test]
fn packed_rebuild_never_leaves_an_internal_page_with_one_child() {
    let (image, _, _) = fixture();
    let mut root = root(&image);
    root.blocks = 16_384 * 200;
    let mut records = BTreeMap::new();
    // One allocation record per leaf, 143 child references per internal page.
    // 144 leaves used to produce a final internal page with one child.
    for group in 0u64..144 {
        let mut bitmap = vec![0; 2048];
        for block in reserved_group_blocks(group, root.blocks) {
            bitmap[(block % 16_384 / 8) as usize] |= 1 << (block % 8);
        }
        records.insert(group.to_be_bytes().to_vec(), bitmap);
    }
    let slots: Vec<_> = (8192..10_000).collect();
    let mut builder = Builder {
        root: &root,
        slots: &slots,
        used: 0,
        pages: BTreeMap::new(),
    };
    builder.tree(TreeKind::Allocation, &records).unwrap();
    for (block, page) in builder.pages {
        let reference = TreeRef {
            block,
            generation: root.generation,
        };
        let decoded = decode_page(
            &page.0[..],
            reference,
            &root.uuid,
            TreeKind::Allocation,
            root.generation,
            None,
            root.blocks,
        )
        .unwrap();
        assert!(decoded.level == 0 || decoded.records.len() >= 2);
    }
}

#[test]
fn repair_completes_committed_unlinks_with_valid_orphan_markers() {
    let mut volume = formatted_volume();
    let name = Name::parse("deleted").unwrap();
    let handle = volume
        .create_file(super::super::ROOT, name.clone())
        .unwrap();
    volume.write(&handle, 0, b"deliberately deleted").unwrap();
    volume
        .remove(super::super::ROOT, &name, crate::RemoveKind::File)
        .unwrap();
    // Keep the handle pinned so ordinary maintenance has not reclaimed it.
    let mut image = volume.into_model_image();
    let root = root(&image);
    image.get_mut(&root.allocation.block).unwrap().0[0] ^= 1;
    let device = FaultDevice::new(image, MountMode::ReadWrite);
    let media = device.media.clone();
    let repair = Repair::diagnose(device).unwrap();
    assert!(!repair.plan().data_loss);
    assert_eq!(repair.plan().objects, 1);
    assert!(
        repair
            .plan()
            .actions
            .iter()
            .any(|action| action.starts_with("Reclaim intentionally unlinked"))
    );
    repair.apply(false).unwrap();
    let mut volume = Volume::open(FaultDevice::new(
        media.borrow().durable.clone(),
        MountMode::ReadOnly,
    ))
    .unwrap();
    assert_eq!(volume.check().unwrap().objects, 1);
    assert!(matches!(
        volume.lookup(super::super::ROOT, &Name::parse("lost+found").unwrap()),
        Err(Error::NotFound)
    ));
    drop(handle);
}

#[test]
fn recovery_overlay_can_exceed_the_physical_metadata_workspace() {
    use super::super::file::{extent_key, group_key};
    use super::super::model::UUID;
    use crate::format::{CHECKSUM_GROUP_BYTES, data_unit_crc};
    let geometry = Geometry {
        byte_len: 6000 * 16_384 * BLOCK_BYTES as u64,
        ..GEOMETRY
    };
    let device = FaultDevice::with_geometry(BTreeMap::new(), MountMode::ReadWrite, geometry);
    let mut image = Volume::format_at(device, UUID, 0, 0)
        .unwrap()
        .into_model_image();
    let mut root = Root::decode(&image[&ROOT_A].0[..], ROOT_A, geometry).unwrap();
    let decoded = decode_page(
        &image[&root.catalog.block].0[..],
        root.catalog,
        &root.uuid,
        TreeKind::Catalog,
        root.generation,
        None,
        root.blocks,
    )
    .unwrap();
    let mut directory = Inode::decode(decoded.records[0].1).unwrap();
    directory.child_count = 1;
    let mut file = Inode::decode(decoded.records[0].1).unwrap();
    file.kind = ObjectKind::File;
    file.size = 5000 * UNIT_BYTES as u64;
    file.allocated_blocks = 5000 * 16;
    let mut records = BTreeMap::from([
        (inode_key(1).to_vec(), directory.encode()),
        (inode_key(2).to_vec(), file.encode()),
    ]);
    let name = Name::parse("scattered").unwrap();
    records.insert(
        dirent_key(1, &name.folded),
        DirentValue {
            target: 2,
            kind: ObjectKind::File,
            stored_name: name.stored(),
        }
        .encode()
        .unwrap(),
    );
    let mut groups = BTreeMap::<u64, [u8; CHECKSUM_GROUP_BYTES]>::new();
    let zeros = vec![0; UNIT_BYTES];
    for unit in 0u64..5000 {
        // One unit per allocation group requires over 5000 bitmap pages,
        // while the real bank can fund at most 4096 replacement pages.
        let mut extent = ((unit + 1) * 16_384 + 4096).to_le_bytes().to_vec();
        extent.extend_from_slice(&16u32.to_le_bytes());
        extent.extend_from_slice(&0u32.to_le_bytes());
        records.insert(extent_key(2, unit * 16).to_vec(), extent);
        let crc = data_unit_crc(&root.uuid, 2, unit, &zeros).unwrap();
        set_checksum_slot(
            groups
                .entry(unit / 256)
                .or_insert([0; CHECKSUM_GROUP_BYTES]),
            (unit % 256) as usize,
            Some(crc),
        );
    }
    for (group, checksums) in groups {
        records.insert(
            group_key(2, group).to_vec(),
            compact_group(&checksums).to_vec(),
        );
    }
    let source_slots: Vec<_> = (20_000..30_000).collect();
    let mut builder = Builder {
        root: &root,
        slots: &source_slots,
        used: 0,
        pages: BTreeMap::new(),
    };
    let catalog = builder.tree(TreeKind::Catalog, &records).unwrap();
    image.extend(builder.pages);
    root.catalog = catalog;
    root.next_object = 3;
    for block in [ROOT_A, root.blocks - 257] {
        image.insert(block, root.encode(block, geometry).unwrap());
    }
    image.get_mut(&root.allocation.block).unwrap().0[0] ^= 1;
    let device = FaultDevice::with_geometry(image.clone(), MountMode::ReadWrite, geometry);
    let media = device.media.clone();
    let mut repair = Repair::diagnose(device).unwrap();
    assert!(repair.plan().blocked.is_some());
    assert!(repair.plan().recovery_available, "{:?}", repair.plan());
    assert!(repair.pages.len() > crate::allocation::MAX_PAGES);
    Volume::open(Overlay {
        device: &mut repair.device,
        pages: &repair.pages,
    })
    .unwrap()
    .check()
    .unwrap();
    assert!(repair.apply(false).is_err());
    assert!(media.borrow().events.is_empty());
    assert_eq!(media.borrow().durable, image);
}

#[test]
fn new_image_rebuilds_a_blocked_source_and_preserves_metadata_and_sparse_files() {
    use crate::{AttributeChange, Mode, Timestamp, VolumeLabel};
    let (image, _, bytes) = fixture();
    let mut volume = Volume::open(FaultDevice::new(image, MountMode::ReadWrite)).unwrap();
    volume.upgrade().unwrap();
    volume
        .set_label(VolumeLabel::parse("Recovered archive").unwrap())
        .unwrap();
    let docs = volume.lookup(ROOT, &Name::parse("docs").unwrap()).unwrap();
    let handle = volume.retain_object(docs).unwrap();
    volume
        .set_attributes(
            &handle,
            AttributeChange {
                creation: Some(Timestamp::new(123, 456).unwrap()),
                modification: Some(Timestamp::new(789, 123).unwrap()),
                mode: Some(Mode::from_host(0o500)),
                hidden: Some(true),
            },
        )
        .unwrap();
    volume.close_handle(handle).unwrap();
    let expected = volume.attributes(docs).unwrap();
    let sparse = volume
        .create_file(ROOT, Name::parse("sparse").unwrap())
        .unwrap();
    let tail = 1u64 << 40;
    volume.write(&sparse, tail, b"tail").unwrap();
    volume.close_handle(sparse).unwrap();
    let reserved = volume
        .create_file(ROOT, Name::parse("reserved").unwrap())
        .unwrap();
    volume.preallocate(&reserved, 0, 65_536, true).unwrap();
    volume.close_handle(reserved).unwrap();
    volume.sync().unwrap();
    let mut image = volume.into_model_image();
    let original = root(&image);
    image.get_mut(&original.workspace.block).unwrap().0[0] ^= 1;
    let source = FaultDevice::new(image.clone(), MountMode::ReadOnly);
    let source_media = source.media.clone();
    let mut repair = Repair::diagnose(source).unwrap();
    assert!(repair.plan().blocked.is_some());
    let target = FaultDevice::new(BTreeMap::new(), MountMode::ReadWrite);
    let target_media = target.media.clone();
    let summary = repair
        .recover_image(target, &mut |finding| panic!("{finding:?}"))
        .unwrap();
    assert!(summary.complete);
    assert_eq!(summary.unreadable_ranges, 0);
    assert_eq!(summary.files, 3);
    assert_eq!(source_media.borrow().durable, image);
    assert!(source_media.borrow().events.is_empty());
    let recovered = target_media.borrow().durable.clone();
    assert_file(
        FaultDevice::new(recovered.clone(), MountMode::ReadOnly),
        &["docs", "notes"],
        &bytes,
    );
    let mut target = Volume::open(FaultDevice::new(recovered, MountMode::ReadOnly)).unwrap();
    let info = target.info().unwrap();
    assert_ne!(info.uuid, original.uuid);
    assert_eq!(info.label.unwrap().as_str(), "Recovered archive");
    let docs = target.lookup(ROOT, &Name::parse("docs").unwrap()).unwrap();
    let attributes = target.attributes(docs).unwrap();
    assert_eq!(attributes.creation, expected.creation);
    assert_eq!(attributes.modification, expected.modification);
    assert_eq!(attributes.metadata_change, expected.metadata_change);
    assert_eq!(attributes.mode, expected.mode);
    assert_eq!(attributes.hidden, expected.hidden);
    let link = target.lookup(docs, &Name::parse("link").unwrap()).unwrap();
    assert_eq!(target.read_symlink(link).unwrap(), b"notes");
    let sparse = target
        .open_file(ROOT, &Name::parse("sparse").unwrap(), Access::Read)
        .unwrap();
    assert_eq!(target.handle_attributes(&sparse).unwrap().size, tail + 4);
    assert_eq!(
        target.handle_attributes(&sparse).unwrap().allocated_bytes,
        4096
    );
    let mut buffer = [0xff; 4];
    target.read(&sparse, 0, &mut buffer).unwrap();
    assert_eq!(buffer, [0; 4]);
    target.read(&sparse, tail, &mut buffer).unwrap();
    assert_eq!(&buffer, b"tail");
    target.close_handle(sparse).unwrap();
    let reserved = target
        .lookup(ROOT, &Name::parse("reserved").unwrap())
        .unwrap();
    assert_eq!(target.attributes(reserved).unwrap().allocated_bytes, 0);
    let summary = target.scrub(&mut |finding| panic!("{finding:?}")).unwrap();
    assert_eq!(summary.unreadable_ranges, 0);
}

#[test]
fn new_image_recovers_disconnected_files_without_source_writes() {
    let (mut image, id, bytes) = fixture();
    edit_catalog(&mut image, |records| {
        records.retain(|(key, value)| !(key[8] == 2 && u64_le(&value[..8]).unwrap() == id))
    });
    let source = FaultDevice::new(image.clone(), MountMode::ReadOnly);
    let source_media = source.media.clone();
    let mut repair = Repair::diagnose(source).unwrap();
    let destination = FaultDevice::new(BTreeMap::new(), MountMode::ReadWrite);
    let destination_media = destination.media.clone();
    let summary = repair
        .recover_image(destination, &mut |finding| panic!("{finding:?}"))
        .unwrap();
    assert!(summary.complete);
    assert_file(
        FaultDevice::new(
            destination_media.borrow().durable.clone(),
            MountMode::ReadOnly,
        ),
        &["lost+found", &id.to_string()],
        &bytes,
    );
    assert_eq!(source_media.borrow().durable, image);
    assert!(source_media.borrow().events.is_empty());
}

#[test]
fn new_image_reports_missing_metadata() {
    let (mut image, id, _) = fixture();
    edit_catalog(&mut image, |records| {
        records.retain(|(key, _)| !(key[8] == 4 && u64_be(&key[..8]).unwrap() == id))
    });
    let source = FaultDevice::new(image.clone(), MountMode::ReadOnly);
    let source_media = source.media.clone();
    let mut repair = Repair::diagnose(source).unwrap();
    assert!(repair.plan().data_loss);
    let destination = FaultDevice::new(BTreeMap::new(), MountMode::ReadWrite);
    let destination_media = destination.media.clone();
    let summary = repair
        .recover_image(destination, &mut |finding| panic!("{finding:?}"))
        .unwrap();
    assert!(!summary.complete);
    assert_eq!(source_media.borrow().durable, image);
    let mut recovered = Volume::open(FaultDevice::new(
        destination_media.borrow().durable.clone(),
        MountMode::ReadOnly,
    ))
    .unwrap();
    recovered.check().unwrap();
    assert_eq!(
        recovered
            .scrub(&mut |finding| panic!("{finding:?}"))
            .unwrap()
            .unreadable_ranges,
        0
    );
    assert!(source_media.borrow().events.is_empty());
}

#[test]
fn new_image_aborts_failed_data_writes_and_flushes_without_changing_the_source() {
    let (image, _, bytes) = fixture();
    let source = FaultDevice::new(image.clone(), MountMode::ReadOnly);
    let source_media = source.media.clone();
    let mut repair = Repair::diagnose(source).unwrap();
    let destination = FaultDevice::new(BTreeMap::new(), MountMode::ReadWrite);
    let completed = destination.media.clone();
    repair
        .recover_image(destination, &mut |finding| panic!("{finding:?}"))
        .unwrap();
    let failure = {
        let completed = completed.borrow();
        completed
            .events
            .iter()
            .rposition(|event| match event {
                crate::volume::model::Event::Write(block) => completed
                    .durable
                    .get(block)
                    .is_some_and(|page| page.0[..] == bytes[..BLOCK_BYTES]),
                _ => false,
            })
            .expect("the copy writes file data")
    };
    let final_flush = completed.borrow().events.len() - 1;
    for failure in [failure, final_flush] {
        let destination = FaultDevice::new(BTreeMap::new(), MountMode::ReadWrite);
        let partial = destination.media.clone();
        partial.borrow_mut().failure = Some(Failure {
            event: failure,
            whole_mask: u64::MAX,
            tear: Some(Tear::Prefix {
                write: 0,
                bytes: 64,
            }),
        });
        let error = repair
            .recover_image(destination, &mut |finding| panic!("{finding:?}"))
            .unwrap_err();
        assert!(matches!(error, Error::Io { .. }));
        assert_eq!(source_media.borrow().durable, image);
        assert!(source_media.borrow().events.is_empty());
        partial.borrow_mut().crash();
        let mut volume = Volume::open(FaultDevice::new(
            partial.borrow().durable.clone(),
            MountMode::ReadOnly,
        ))
        .unwrap();
        volume.check().unwrap();
        if failure != final_flush {
            assert!(
                embedded_manifest(&mut volume, &Name::parse("RECOVERY-REPORT.json").unwrap())
                    .contains("\"state\":\"in_progress\"")
            );
        }
    }
}

#[test]
fn new_image_copies_every_directory_page_and_keeps_parent_paths() {
    let mut source = formatted_volume();
    let directory = source
        .create_directory(ROOT, Name::parse("directory").unwrap())
        .unwrap();
    let child = source
        .create_file(directory, Name::parse("nested").unwrap())
        .unwrap();
    source.write(&child, 0, b"nested").unwrap();
    source.close_handle(child).unwrap();
    for index in 0..80 {
        let file = source
            .create_file(ROOT, Name::parse(&format!("f{index:03}")).unwrap())
            .unwrap();
        if index == 79 {
            source.write(&file, 0, b"last").unwrap();
        }
        source.close_handle(file).unwrap();
    }
    source.sync().unwrap();
    let image = source.into_model_image();
    let source = FaultDevice::new(image.clone(), MountMode::ReadOnly);
    let media = source.media.clone();
    let mut repair = Repair::diagnose(source).unwrap();
    let destination = FaultDevice::new(BTreeMap::new(), MountMode::ReadWrite);
    let recovered = destination.media.clone();
    let summary = repair
        .recover_image(destination, &mut |finding| panic!("{finding:?}"))
        .unwrap();
    assert_eq!(summary.objects, 83);
    assert_eq!(summary.files, 81);
    assert!(summary.complete);
    assert_file(
        FaultDevice::new(recovered.borrow().durable.clone(), MountMode::ReadOnly),
        &["directory", "nested"],
        b"nested",
    );
    assert_file(
        FaultDevice::new(recovered.borrow().durable.clone(), MountMode::ReadOnly),
        &["f079"],
        b"last",
    );
    assert_eq!(media.borrow().durable, image);
    assert!(media.borrow().events.is_empty());
}

fn embedded_manifest(volume: &mut Volume<FaultDevice>, name: &Name) -> String {
    let file = volume.open_file(ROOT, name, Access::Read).unwrap();
    let size = volume.handle_attributes(&file).unwrap().size as usize;
    let mut bytes = vec![0; size];
    let mut offset = 0;
    while offset < size {
        offset += volume
            .read(&file, offset as u64, &mut bytes[offset..])
            .unwrap();
    }
    volume.close_handle(file).unwrap();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn manifest_preserves_colliding_names_and_records_verified_source_provenance() {
    let (image, _, _) = fixture();
    let mut source = Volume::open(FaultDevice::new(image, MountMode::ReadWrite)).unwrap();
    for name in ["recovery-report.JSON", "RECOVERY-REPORT.1.json.working"] {
        let file = source
            .create_file(ROOT, Name::parse(name).unwrap())
            .unwrap();
        source.write(&file, 0, b"original report").unwrap();
        source.close_handle(file).unwrap();
    }
    source.sync().unwrap();
    let image = source.into_model_image();
    let original = root(&image);
    let mut repair = Repair::diagnose(FaultDevice::new(image, MountMode::ReadOnly)).unwrap();
    let destination = FaultDevice::new(BTreeMap::new(), MountMode::ReadWrite);
    let media = destination.media.clone();
    let result = repair
        .recover_image_with_verification(destination, true, &mut |finding| panic!("{finding:?}"))
        .unwrap();
    assert!(!result.verification_failed);
    assert_eq!(result.manifest_name.stored(), b"RECOVERY-REPORT.2.json");
    assert!(result.manifest_json.contains("\"verification\":\"passed\""));
    assert!(
        result
            .manifest_json
            .contains(&format!("\"generation\":{}", original.generation))
    );
    let mut volume = Volume::open(FaultDevice::new(
        media.borrow().durable.clone(),
        MountMode::ReadOnly,
    ))
    .unwrap();
    assert_eq!(
        embedded_manifest(&mut volume, &result.manifest_name),
        result.manifest_json
    );
    for name in ["recovery-report.JSON", "RECOVERY-REPORT.1.json.working"] {
        assert_file(
            FaultDevice::new(media.borrow().durable.clone(), MountMode::ReadOnly),
            &[name],
            b"original report",
        );
    }
}

#[test]
fn manifest_identifies_missing_mappings_even_when_rebuilt_data_scrubs_clean() {
    let (mut image, id, _) = fixture();
    edit_catalog(&mut image, |records| {
        records.retain(|(key, _)| !(key[8] == 4 && u64_be(&key[..8]).unwrap() == id))
    });
    let mut repair = Repair::diagnose(FaultDevice::new(image, MountMode::ReadOnly)).unwrap();
    let destination = FaultDevice::new(BTreeMap::new(), MountMode::ReadWrite);
    let media = destination.media.clone();
    let result = repair
        .recover_image_with_verification(destination, true, &mut |finding| panic!("{finding:?}"))
        .unwrap();
    assert!(!result.summary.complete);
    assert!(!result.verification_failed);
    assert!(result.manifest_json.contains("\"state\":\"partial\""));
    assert!(
        result
            .manifest_json
            .contains("\"offset\":0,\"length\":150001")
    );
    assert!(result.manifest_json.contains("\"status\":\"partial\""));
    let mut volume = Volume::open(FaultDevice::new(
        media.borrow().durable.clone(),
        MountMode::ReadOnly,
    ))
    .unwrap();
    assert_eq!(
        volume
            .scrub(&mut |finding| panic!("{finding:?}"))
            .unwrap()
            .unreadable_ranges,
        0
    );
    assert_eq!(
        embedded_manifest(&mut volume, &result.manifest_name),
        result.manifest_json
    );
}

#[test]
fn destination_verification_records_output_damage_without_blessing_it() {
    let (image, _, bytes) = fixture();
    let source = FaultDevice::new(image.clone(), MountMode::ReadOnly);
    let source_media = source.media.clone();
    let mut repair = Repair::diagnose(source).unwrap();
    let baseline = FaultDevice::new(BTreeMap::new(), MountMode::ReadWrite);
    let baseline_media = baseline.media.clone();
    repair
        .recover_image_with_verification(baseline, true, &mut |finding| panic!("{finding:?}"))
        .unwrap();
    let block = *baseline_media
        .borrow()
        .durable
        .iter()
        .find(|(_, page)| page.0[..] == bytes[..BLOCK_BYTES])
        .unwrap()
        .0;
    let destination = FaultDevice::new(BTreeMap::new(), MountMode::ReadWrite);
    let media = destination.media.clone();
    media.borrow_mut().persistent_unreadable.insert(block);
    let mut findings = Vec::new();
    let result = repair
        .recover_image_with_verification(destination, true, &mut |finding| {
            findings.push((finding.offset, finding.length));
        })
        .unwrap();
    assert!(result.verification_failed);
    assert_eq!(findings, [(Some(0), 65_536)]);
    assert!(
        result
            .manifest_json
            .contains("\"state\":\"verification_failed\"")
    );
    assert!(result.manifest_json.contains("\"verification\":\"failed\""));
    assert!(result.manifest_json.contains("\"status\":\"damaged\""));
    let mut volume = Volume::open(FaultDevice::new(
        media.borrow().durable.clone(),
        MountMode::ReadOnly,
    ))
    .unwrap();
    assert_eq!(
        embedded_manifest(&mut volume, &result.manifest_name),
        result.manifest_json
    );
    assert_eq!(source_media.borrow().durable, image);
    assert!(source_media.borrow().events.is_empty());
}
