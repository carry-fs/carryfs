use super::model::{Event, Failure, FaultDevice, GEOMETRY, Tear, UUID, advance};
use super::*;
use crate::Name;
use crate::format::{BLOCK_BYTES, UNIT_BYTES};
use std::collections::{BTreeMap, BTreeSet};

mod deferred;
mod faults;
mod file_state;
mod full_disk;
mod labels;
mod salvage;
mod symlinks;

fn formatted() -> BTreeMap<u64, Page> {
    let device = FaultDevice::new(BTreeMap::new(), MountMode::ReadWrite);
    let media = device.media.clone();
    Volume::format_at(device, UUID, 1, 0).unwrap();
    media.borrow().durable.clone()
}

fn recovered(durable: BTreeMap<u64, Page>, minimum: u64, maximum: u64) {
    let read_only = FaultDevice::new(durable.clone(), MountMode::ReadOnly);
    let read_media = read_only.media.clone();
    let mut reader = Volume::open(read_only).unwrap();
    let info = reader.info().unwrap();
    assert!((minimum..=maximum).contains(&info.generation));
    assert_eq!(
        info.free_blocks,
        GEOMETRY.blocks() - 1024 - info.generation * 3
    );
    reader.sync().unwrap();
    reader.eject().unwrap();
    assert!(read_media.borrow().events.is_empty());
    assert_eq!(read_media.borrow().durable, durable);

    let device = FaultDevice::new(durable, MountMode::ReadWrite);
    let media = device.media.clone();
    let volume = Volume::open(device).unwrap();
    assert_eq!(volume.info().unwrap(), info);
    let disk = media.borrow();
    let a = Root::decode(&disk.durable[&ROOT_A].0[..], ROOT_A, GEOMETRY).unwrap();
    let b = Root::decode(
        &disk.durable[&(GEOMETRY.blocks() - 257)].0[..],
        GEOMETRY.blocks() - 257,
        GEOMETRY,
    )
    .unwrap();
    assert_eq!(a, b);
}

fn interrupted(base: &BTreeMap<u64, Page>, failure: Failure) {
    interrupted_with(base, failure, &[]);
}

/// `interrupted`, with `also_torn` pages torn by the same failure.
fn interrupted_with(base: &BTreeMap<u64, Page>, failure: Failure, also_torn: &[Tear]) {
    let device = FaultDevice::new(base.clone(), MountMode::ReadWrite);
    let media = device.media.clone();
    let mut volume = Volume::open(device).unwrap();
    media.borrow_mut().failure = Some(failure);
    media.borrow_mut().also_torn = also_torn.to_vec();
    assert!(advance(&mut volume).is_err(), "{failure:?}");
    let events = media.borrow().events.clone();
    assert!(matches!(advance(&mut volume), Err(Error::Faulted)));
    assert!(matches!(volume.sync(), Err(Error::Faulted)));
    assert!(matches!(volume.eject(), Err(Error::Faulted)));
    assert_eq!(media.borrow().events, events);
    drop(volume);
    media.borrow_mut().crash();
    recovered(
        media.borrow().durable.clone(),
        if failure.event >= 6 { 2 } else { 1 },
        2,
    );
}

// Two pages of one flush interval torn at once, each with sector subsets that
// keep the head, the tail, or alternate sectors. The interval writes three
// replacement pages, so every pair is covered.
#[test]
fn two_torn_pages_in_one_interval_recover() {
    let base = formatted();
    let masks = [0x00, 0x01, 0x0F, 0xF0, 0x55, 0xAA, 0x80, 0xFE];
    for first in 0..3 {
        for second in first + 1..3 {
            for a in masks {
                for b in masks {
                    interrupted_with(
                        &base,
                        Failure {
                            event: 3,
                            whole_mask: 0,
                            tear: Some(Tear::Sectors {
                                write: first,
                                mask: a,
                            }),
                        },
                        &[Tear::Sectors {
                            write: second,
                            mask: b,
                        }],
                    );
                }
            }
        }
    }
}

#[test]
fn publication_orders_replacement_flush_a_flush_b_flush() {
    let device = FaultDevice::new(formatted(), MountMode::ReadWrite);
    let media = device.media.clone();
    let mut volume = Volume::open(device).unwrap();
    assert!(media.borrow().events.is_empty());
    advance(&mut volume).unwrap();
    assert_eq!(
        media.borrow().events,
        vec![
            Event::Write(515),
            Event::Write(516),
            Event::Write(517),
            Event::Flush,
            Event::Write(ROOT_A),
            Event::Flush,
            Event::Write(GEOMETRY.blocks() - 257),
            Event::Flush
        ]
    );
    drop(volume);
    media.borrow_mut().crash();
    recovered(media.borrow().durable.clone(), 2, 2);
}

#[test]
fn every_output_interruption_and_pending_write_subset_recovers() {
    let base = formatted();
    for (event, pending) in [1, 2, 3, 3, 1, 1, 1, 1].into_iter().enumerate() {
        for whole_mask in 0..1 << pending {
            interrupted(
                &base,
                Failure {
                    event,
                    whole_mask,
                    tear: None,
                },
            );
        }
    }
}

// A device that acknowledges a flush but keeps the old bytes for some pages is
// outside CarryFS's contract. The test states what still must hold: a writable
// open either rejects the image or shows a checked generation, never damage it
// would write over. Rollback to an older coherent generation is accepted.
#[test]
fn a_flush_that_drops_published_pages_is_rejected_or_recovers_a_checked_generation() {
    let base = formatted();
    let device = FaultDevice::new(base.clone(), MountMode::ReadWrite);
    let media = device.media.clone();
    let mut volume = Volume::open(device).unwrap();
    advance(&mut volume).unwrap();
    let written: Vec<u64> = media
        .borrow()
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Write(block) => Some(*block),
            Event::Flush => None,
        })
        .collect();
    let published = media.borrow().durable.clone();
    let (mut rejected, mut recovered_images) = (0, 0);
    for mask in 0..1u32 << written.len() {
        let mut image = published.clone();
        for (index, block) in written.iter().enumerate() {
            if mask & (1 << index) != 0 {
                match base.get(block) {
                    Some(old) => image.insert(*block, old.clone()),
                    None => image.remove(block),
                };
            }
        }
        let Ok(reopened) = Volume::open(FaultDevice::new(image.clone(), MountMode::ReadWrite))
        else {
            rejected += 1;
            continue;
        };
        recovered_images += 1;
        assert!(
            (1..=2).contains(&reopened.info().unwrap().generation),
            "{mask:b}"
        );
        Volume::open(FaultDevice::new(image, MountMode::ReadOnly))
            .unwrap()
            .check()
            .unwrap();
    }
    assert!(
        rejected > 0 && recovered_images > 0,
        "{rejected} {recovered_images}"
    );
}

#[test]
fn every_sector_subset_of_each_pending_page_recovers() {
    let base = formatted();
    for (event, pending) in [1, 2, 3, 3, 1, 1, 1, 1].into_iter().enumerate() {
        for write in 0..pending {
            for whole_mask in 0..1 << pending {
                if whole_mask & (1 << write) != 0 {
                    continue;
                }
                for mask in 0..=255 {
                    interrupted(
                        &base,
                        Failure {
                            event,
                            whole_mask,
                            tear: Some(Tear::Sectors { write, mask }),
                        },
                    );
                }
            }
        }
    }
}

#[test]
fn short_writes_at_header_checksum_and_sector_boundaries_recover() {
    let base = formatted();
    for (event, pending) in [1, 2, 3, 3, 1, 1, 1, 1].into_iter().enumerate() {
        for write in 0..pending {
            for bytes in [
                0, 1, 7, 8, 24, 40, 48, 56, 64, 65, 67, 68, 79, 80, 159, 160, 511, 512, 513, 1023,
                2048, 4091, 4092, 4093, 4094, 4095, 4096,
            ] {
                interrupted(
                    &base,
                    Failure {
                        event,
                        whole_mask: 0,
                        tear: Some(Tear::Prefix { write, bytes }),
                    },
                );
            }
        }
    }
}

#[test]
fn failed_format_recovers_only_if_a_valid_first_root_reached_disk() {
    let writes = bootstrap(GEOMETRY, UUID, 1, 0).unwrap().1.len();
    for event in 0..writes + 5 {
        let pending = if event < writes {
            event + 1
        } else if event == writes {
            writes
        } else {
            1
        };
        // Project each page as present/absent as well as the empty/full batch.
        let masks = [0, u64::MAX]
            .into_iter()
            .chain((0..pending).flat_map(|index| [1 << index, !(1 << index)]));
        for whole_mask in masks {
            let device = FaultDevice::new(BTreeMap::new(), MountMode::ReadWrite);
            let media = device.media.clone();
            media.borrow_mut().failure = Some(Failure {
                event,
                whole_mask,
                tear: None,
            });
            assert!(Volume::format_at(device, UUID, 1, 0).is_err());
            media.borrow_mut().crash();
            let durable = media.borrow().durable.clone();
            if event >= writes + 3 || (event > writes && whole_mask & 1 != 0) {
                recovered(durable, 1, 1);
            } else {
                assert!(matches!(
                    Volume::open(FaultDevice::new(durable, MountMode::ReadWrite)),
                    Err(Error::Corrupt(_))
                ));
            }
        }
    }
}

#[test]
fn torn_bootstrap_roots_recover_only_after_a_valid_first_publication() {
    let expected_a = formatted()[&ROOT_A].clone();
    let writes = bootstrap(GEOMETRY, UUID, 1, 0).unwrap().1.len();
    for event in writes + 1..writes + 5 {
        for tear in (0..=255)
            .map(|mask| Tear::Sectors { write: 0, mask })
            .chain(
                [0, 1, 48, 64, 512, 4092, 4093, 4094, 4095, 4096]
                    .map(|bytes| Tear::Prefix { write: 0, bytes }),
            )
        {
            let device = FaultDevice::new(BTreeMap::new(), MountMode::ReadWrite);
            let media = device.media.clone();
            media.borrow_mut().failure = Some(Failure {
                event,
                whole_mask: 0,
                tear: Some(tear),
            });
            assert!(Volume::format_at(device, UUID, 1, 0).is_err());
            media.borrow_mut().crash();
            let durable = media.borrow().durable.clone();
            // If B was reached, A's successful flush already made the forest durable.
            // A tear can omit zero bytes and still exactly match the complete root.
            let valid_a = durable.get(&ROOT_A) == Some(&expected_a);
            if event >= writes + 3 || valid_a {
                recovered(durable, 1, 1);
            } else {
                assert!(matches!(
                    Volume::open(FaultDevice::new(durable, MountMode::ReadWrite)),
                    Err(Error::Corrupt(_))
                ));
            }
        }
    }
}

#[test]
fn valid_b_repairs_a_and_unreadable_selected_tree_pages_do_not_mirror() {
    let mut durable = formatted();
    durable.get_mut(&ROOT_A).unwrap().0[0] ^= 1;
    let device = FaultDevice::new(durable.clone(), MountMode::ReadWrite);
    let media = device.media.clone();
    let volume = Volume::open(device).unwrap();
    assert_eq!(volume.info().unwrap().generation, 1);
    assert_eq!(media.borrow().events, [Event::Write(ROOT_A), Event::Flush]);
    drop(volume);
    recovered(media.borrow().durable.clone(), 1, 1);

    for block in [512, 513, 514] {
        let device = FaultDevice::new(durable.clone(), MountMode::ReadWrite);
        let media = device.media.clone();
        media.borrow_mut().unreadable = Some(block);
        assert!(matches!(Volume::open(device), Err(Error::Io { .. })));
        assert!(media.borrow().events.is_empty());
        assert_eq!(media.borrow().durable, durable);
    }
}

#[test]
fn reopen_mirroring_is_idempotent_across_repeated_failures() {
    let device = FaultDevice::new(formatted(), MountMode::ReadWrite);
    let media = device.media.clone();
    let mut volume = Volume::open(device).unwrap();
    media.borrow_mut().failure = Some(Failure {
        event: 6,
        whole_mask: 0,
        tear: None,
    });
    assert!(advance(&mut volume).is_err());
    drop(volume);
    media.borrow_mut().crash();
    let mut durable = media.borrow().durable.clone();
    for attempt in 0..16 {
        let device = FaultDevice::new(durable, MountMode::ReadWrite);
        let media = device.media.clone();
        media.borrow_mut().failure = Some(Failure {
            event: attempt % 2,
            whole_mask: 0,
            tear: Some(Tear::Prefix {
                write: 0,
                bytes: 48 + attempt,
            }),
        });
        assert!(Volume::open(device).is_err());
        media.borrow_mut().crash();
        durable = media.borrow().durable.clone();
    }
    recovered(durable, 2, 2);
}

#[test]
fn repeated_commits_preserve_the_last_acknowledged_generation() {
    let mut durable = formatted();
    let mut acknowledged = 1;
    for attempt in 0..24 {
        let device = FaultDevice::new(durable, MountMode::ReadWrite);
        let media = device.media.clone();
        let mut volume = Volume::open(device).unwrap();
        media.borrow_mut().events.clear();
        let before = volume.info().unwrap().generation;
        if attempt % 3 != 0 {
            media.borrow_mut().failure = Some(Failure {
                event: attempt % 8,
                whole_mask: 7,
                tear: Some(Tear::Sectors {
                    write: 0,
                    mask: 0x55,
                }),
            });
        }
        if advance(&mut volume).is_ok() {
            acknowledged = before + 1;
        }
        drop(volume);
        media.borrow_mut().crash();
        durable = media.borrow().durable.clone();
        recovered(durable.clone(), acknowledged, before + 1);
    }
}

#[test]
fn selected_tree_damage_never_rolls_back_or_mirrors() {
    let device = FaultDevice::new(formatted(), MountMode::ReadWrite);
    let media = device.media.clone();
    let mut volume = Volume::open(device).unwrap();
    media.borrow_mut().failure = Some(Failure {
        event: 6,
        whole_mask: 0,
        tear: None,
    });
    assert!(advance(&mut volume).is_err());
    drop(volume);
    media.borrow_mut().crash();
    media.borrow_mut().durable.get_mut(&515).unwrap().0[100] ^= 1;
    let durable = media.borrow().durable.clone();
    let device = FaultDevice::new(durable.clone(), MountMode::ReadWrite);
    let media = device.media.clone();
    assert!(matches!(Volume::open(device), Err(Error::Corrupt(_))));
    assert!(media.borrow().events.is_empty());
    assert_eq!(media.borrow().durable, durable);
}

#[test]
fn unreadable_root_cannot_be_treated_as_an_invalid_crc() {
    for block in [ROOT_A, GEOMETRY.blocks() - 257] {
        let device = FaultDevice::new(formatted(), MountMode::ReadWrite);
        let media = device.media.clone();
        media.borrow_mut().unreadable = Some(block);
        assert!(matches!(Volume::open(device), Err(Error::Io { .. })));
        assert!(media.borrow().events.is_empty());
    }
}

#[test]
fn conflicting_roots_and_unsupported_format_never_fall_back() {
    let base = formatted();
    for kind in 0..4 {
        let mut durable = base.clone();
        let a = durable.get_mut(&ROOT_A).unwrap();
        match kind {
            0 => a.0[120..128].copy_from_slice(&60000_u64.to_le_bytes()),
            1 => a.0[24] ^= 1,
            2 => a.0[8..12].copy_from_slice(&5_u32.to_le_bytes()),
            _ => a.0[128] = 1,
        }
        a.0[4092..].fill(0);
        let crc = crate::format::crc32c(&a.0[..]);
        a.0[4092..].copy_from_slice(&crc.to_le_bytes());
        let device = FaultDevice::new(durable, MountMode::ReadWrite);
        let media = device.media.clone();
        let result = Volume::open(device);
        if kind < 2 {
            assert!(matches!(result, Err(Error::Corrupt(_))));
        } else {
            assert!(matches!(result, Err(Error::UnsupportedFormat)));
        }
        assert!(media.borrow().events.is_empty());
    }
}

#[test]
fn failed_sync_and_eject_retain_the_device_and_reject_further_io() {
    let device = FaultDevice::new(formatted(), MountMode::ReadWrite);
    let media = device.media.clone();
    let mut volume = Volume::open(device).unwrap();
    media.borrow_mut().failure = Some(Failure {
        event: 0,
        whole_mask: 0,
        tear: None,
    });
    assert!(!volume.is_faulted());
    assert!(volume.eject().is_err());
    assert!(volume.is_faulted());
    assert!(volume.device.is_some());
    assert!(matches!(volume.sync(), Err(Error::Faulted)));
    assert!(matches!(volume.eject(), Err(Error::Faulted)));
    assert_eq!(media.borrow().events, [Event::Flush]);
}

#[test]
fn ejected_volumes_reject_requests_and_drop_does_not_sync() {
    let device = FaultDevice::new(formatted(), MountMode::ReadWrite);
    let media = device.media.clone();
    let mut volume = Volume::open(device).unwrap();
    volume.eject().unwrap();
    assert!(matches!(volume.info(), Err(Error::Ejected)));
    assert!(matches!(volume.sync(), Err(Error::Ejected)));
    assert!(matches!(volume.eject(), Err(Error::Ejected)));
    let calls = media.borrow().events.clone();
    drop(volume);
    assert_eq!(media.borrow().events, calls);

    let device = FaultDevice::new(formatted(), MountMode::ReadWrite);
    let media = device.media.clone();
    drop(Volume::open(device).unwrap());
    assert!(media.borrow().events.is_empty());
}

#[test]
fn invalid_prepared_commit_fails_before_output_without_faulting() {
    let device = FaultDevice::new(formatted(), MountMode::ReadWrite);
    let media = device.media.clone();
    let mut volume = Volume::open(device).unwrap();
    let (mut root, writes) = bootstrap(GEOMETRY, UUID, 1, 0).unwrap();
    assert!(
        volume
            .publish(root.clone(), writes.clone(), &[], None, Vec::new(), 0)
            .is_err()
    );
    root.generation = 2;
    let duplicate = [
        BlockWrite {
            block: 515,
            page: Page(Box::new([0; BLOCK_BYTES])),
        },
        BlockWrite {
            block: 515,
            page: Page(Box::new([0; BLOCK_BYTES])),
        },
    ];
    assert!(
        volume
            .publish(root, duplicate.to_vec(), &[], None, Vec::new(), 0)
            .is_err()
    );
    assert!(media.borrow().events.is_empty());
    assert_eq!(volume.info().unwrap().generation, 1);
    advance(&mut volume).unwrap();
}

#[test]
fn generation_exhaustion_and_existing_media_are_rejected_without_writes() {
    let base = formatted();
    let device = FaultDevice::new(base.clone(), MountMode::ReadWrite);
    let media = device.media.clone();
    assert!(Volume::format_at(device, UUID, 1, 0).is_err());
    assert!(media.borrow().events.is_empty());

    let device = FaultDevice::new(base, MountMode::ReadWrite);
    let media = device.media.clone();
    let mut volume = Volume::open(device).unwrap();
    volume.root.as_mut().unwrap().generation = u64::MAX;
    let (root, writes) = bootstrap(GEOMETRY, UUID, 1, 0).unwrap();
    assert!(
        volume
            .publish(root, writes.clone(), &[], None, Vec::new(), 0)
            .is_err()
    );
    assert!(media.borrow().events.is_empty());
}

#[test]
fn formatting_rejects_dirty_reserved_bands_before_any_write() {
    for block in [0, 511, GEOMETRY.blocks() - 512, GEOMETRY.blocks() - 1] {
        let mut durable = BTreeMap::new();
        let mut page = Page(Box::new([0; BLOCK_BYTES]));
        page.0[70] = 1;
        durable.insert(block, page);
        let device = FaultDevice::new(durable.clone(), MountMode::ReadWrite);
        let media = device.media.clone();
        assert!(Volume::format_at(device, UUID, 1, 0).is_err());
        assert!(media.borrow().events.is_empty());
        assert_eq!(media.borrow().durable, durable);
    }
}

#[test]
fn damaged_selected_workspace_never_rolls_back_or_mirrors() {
    let base = model::orphan_image(16);
    let mut volume = Volume::open(FaultDevice::new(base.clone(), MountMode::ReadWrite)).unwrap();
    volume.sync().unwrap();
    let committed = volume.into_model_image();
    let root = Root::decode(&committed[&256].0[..], 256, GEOMETRY).unwrap();
    // A contiguous slot list is one run, so this bank is a single leaf root.
    let mut damaged = committed.clone();
    damaged.insert(root.blocks - 257, base[&(root.blocks - 257)].clone());
    damaged.get_mut(&root.workspace.block).unwrap().0[100] ^= 1;
    for mode in [MountMode::ReadOnly, MountMode::ReadWrite] {
        let device = FaultDevice::new(damaged.clone(), mode);
        let media = device.media.clone();
        assert!(matches!(
            Volume::open(device),
            Err(Error::Corruption { .. })
        ));
        assert!(media.borrow().events.is_empty());
    }
}

#[test]
fn workspace_cannot_own_a_live_page_or_an_unallocated_block() {
    for alias in [false, true] {
        let mut image = formatted();
        let root = Root::decode(&image[&256].0[..], 256, GEOMETRY).unwrap();
        let mut slots: Vec<_> = crate::workspace::INITIAL.collect();
        // 512 already owns the catalog; 8191 is free in the allocation bitmap.
        slots[0] = if alias { 512 } else { 8191 };
        image.extend(crate::workspace::encode(&root, &slots).unwrap());
        let mut readonly =
            Volume::open(FaultDevice::new(image.clone(), MountMode::ReadOnly)).unwrap();
        assert!(matches!(
            readonly.check(),
            Err(Error::Corrupt(_) | Error::Corruption { .. })
        ));
        let device = FaultDevice::new(image, MountMode::ReadWrite);
        let media = device.media.clone();
        assert!(matches!(
            Volume::open(device),
            Err(Error::Corrupt(_) | Error::Corruption { .. })
        ));
        assert!(media.borrow().events.is_empty());
    }
}

#[test]
fn bank_selection_survives_cursor_only_commits_and_rejects_active_bank_writes() {
    let device = FaultDevice::new(formatted(), MountMode::ReadWrite);
    let media = device.media.clone();
    let mut volume = Volume::open(device).unwrap();
    let mut cursor = volume.root.clone().unwrap();
    cursor.generation += 1;
    cursor.workspace_cursor = 1;
    volume
        .publish(cursor.clone(), Vec::new(), &[], None, Vec::new(), 0)
        .unwrap();
    assert_eq!(cursor.workspace.block, 32);
    let mut next = cursor.clone();
    next.generation += 1;
    next.workspace = crate::workspace::next_reference(&cursor).unwrap();
    let slots: Vec<_> = crate::workspace::INITIAL.collect();
    let writes = |root: &Root| -> Vec<BlockWrite> {
        crate::workspace::encode(root, &slots)
            .unwrap()
            .into_iter()
            .map(|(block, page)| BlockWrite { block, page })
            .collect()
    };
    assert_eq!(next.workspace.block, next.blocks - 96);
    volume
        .publish(next.clone(), writes(&next), &[], None, Vec::new(), 0)
        .unwrap();
    media.borrow_mut().events.clear();
    let mut wrong = next.clone();
    wrong.generation += 1;
    wrong.workspace.generation = wrong.generation;
    assert!(matches!(
        volume.publish(wrong.clone(), writes(&wrong), &[], None, Vec::new(), 0),
        Err(Error::InvalidInput(_))
    ));
    assert!(media.borrow().events.is_empty());
    let mut incomplete = wrong;
    incomplete.workspace = crate::workspace::next_reference(&next).unwrap();
    let mut partial = writes(&incomplete);
    partial.remove(0);
    assert!(matches!(
        volume.publish(incomplete, partial, &[], None, Vec::new(), 0),
        Err(Error::InvalidInput(_))
    ));
    assert!(media.borrow().events.is_empty());
    assert_eq!(volume.info().unwrap().generation, 3);
    Volume::open(FaultDevice::new(
        volume.into_model_image(),
        MountMode::ReadOnly,
    ))
    .unwrap()
    .check()
    .unwrap();
}

fn opened() -> Volume<FaultDevice> {
    Volume::open(FaultDevice::new(formatted(), MountMode::ReadWrite)).unwrap()
}

fn durable(volume: &Volume<FaultDevice>) -> BTreeMap<u64, Page> {
    volume
        .device
        .as_ref()
        .unwrap()
        .media
        .borrow()
        .durable
        .clone()
}

fn checked(volume: &Volume<FaultDevice>) {
    Volume::open(FaultDevice::new(durable(volume), MountMode::ReadOnly))
        .unwrap()
        .check()
        .unwrap();
}

fn name(text: &str) -> Name {
    Name::parse(text).unwrap()
}

#[test]
fn create_rename_remove_and_enumerate_round_trip_through_reopen() {
    let mut volume = opened();
    let docs = volume
        .create_directory(ROOT, Name::parse("docs").unwrap())
        .unwrap();
    let notes = volume
        .create_directory(docs, Name::parse("notes").unwrap())
        .unwrap();
    volume
        .rename(
            docs,
            &Name::parse("notes").unwrap(),
            ROOT,
            Name::parse("notes-renamed").unwrap(),
            Replace::Forbidden,
        )
        .unwrap();
    assert_eq!(
        volume
            .lookup(ROOT, &Name::parse("notes-renamed").unwrap())
            .unwrap(),
        notes
    );
    assert!(volume.lookup(docs, &Name::parse("notes").unwrap()).is_err());
    assert_eq!(volume.attributes(notes).unwrap().parent, ROOT);

    volume
        .remove(
            ROOT,
            &Name::parse("notes-renamed").unwrap(),
            RemoveKind::Directory,
        )
        .unwrap();
    assert!(matches!(
        volume.lookup(ROOT, &Name::parse("notes-renamed").unwrap()),
        Err(Error::NotFound)
    ));

    let handle = volume.open_directory(ROOT).unwrap();
    let page = volume.read_directory(&handle, None).unwrap();
    assert!(page.cursor.is_none());
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].object, docs);
    volume.close_handle(handle).unwrap();

    volume.sync().unwrap();
    let image = volume.into_model_image();
    let mut reopened = Volume::open(FaultDevice::new(image.clone(), MountMode::ReadWrite)).unwrap();
    assert_eq!(
        reopened
            .lookup(ROOT, &Name::parse("docs").unwrap())
            .unwrap(),
        docs
    );
    Volume::open(FaultDevice::new(image, MountMode::ReadOnly))
        .unwrap()
        .check()
        .unwrap();
}

#[test]
fn open_directory_survives_removal_and_still_reports_attributes() {
    let mut volume = opened();
    let name = Name::parse("child").unwrap();
    let child = volume.create_directory(ROOT, name.clone()).unwrap();
    let handle = volume.open_directory(child).unwrap();
    volume.remove(ROOT, &name, RemoveKind::Directory).unwrap();

    let attributes = volume.handle_attributes(&handle).unwrap();
    assert_eq!(attributes.kind, ObjectKind::Directory);
    assert!(volume.eject().is_err());

    volume.close_handle(handle).unwrap();
    volume.sync().unwrap();
    assert!(matches!(volume.attributes(child), Err(Error::NotFound)));
    volume.eject().unwrap();
}

#[test]
fn directory_pages_preserve_lookahead_at_boundaries() {
    let mut volume = opened();
    volume.set_deferred_checkpoints(true).unwrap();
    let mut created = 0;
    for count in [0, 1, 62, 63, 64, 65, 126, 127, 128] {
        for index in created..count {
            volume
                .create_directory(ROOT, name(&format!("d{index:03}")))
                .unwrap();
        }
        created = count;
        let handle = volume.open_directory(ROOT).unwrap();
        let mut names = Vec::new();
        let mut cursor = None;
        loop {
            let page = volume.read_directory(&handle, cursor.as_ref()).unwrap();
            assert!(page.entries.len() <= 63);
            names.extend(
                page.entries
                    .iter()
                    .map(|entry| String::from_utf8(entry.name.stored().to_vec()).unwrap()),
            );
            assert_eq!(page.cursor.is_some(), names.len() < count);
            cursor = page.cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(
            names,
            (0..count)
                .map(|index| format!("d{index:03}"))
                .collect::<Vec<_>>()
        );
        volume.close_handle(handle).unwrap();
    }
}

#[test]
fn directory_cursor_invalidates_on_unrelated_namespace_change() {
    let mut volume = opened();
    for index in 0..65 {
        volume
            .create_directory(ROOT, Name::parse(&format!("d{index:02}")).unwrap())
            .unwrap();
    }
    let handle = volume.open_directory(ROOT).unwrap();
    let page = volume.read_directory(&handle, None).unwrap();
    assert_eq!(page.entries.len(), 63);
    let cursor = page.cursor.expect("more entries remain");

    volume
        .create_directory(ROOT, Name::parse("unrelated").unwrap())
        .unwrap();
    assert!(matches!(
        volume.read_directory(&handle, Some(&cursor)),
        Err(Error::DirectoryChanged)
    ));
    volume.close_handle(handle).unwrap();
}

#[test]
fn rename_rejects_moving_a_directory_into_its_own_descendant() {
    let mut volume = opened();
    let a = volume
        .create_directory(ROOT, Name::parse("a").unwrap())
        .unwrap();
    let b = volume
        .create_directory(a, Name::parse("b").unwrap())
        .unwrap();
    assert!(matches!(
        volume.rename(
            ROOT,
            &Name::parse("a").unwrap(),
            b,
            Name::parse("a2").unwrap(),
            Replace::Forbidden,
        ),
        Err(Error::InvalidInput(_))
    ));
}

#[test]
fn rename_replace_orphans_the_old_destination_instead_of_deleting_it_inline() {
    let mut volume = opened();
    let source = volume
        .create_directory(ROOT, Name::parse("source").unwrap())
        .unwrap();
    let destination = volume
        .create_directory(ROOT, Name::parse("destination").unwrap())
        .unwrap();
    let handle = volume.open_directory(destination).unwrap();
    volume
        .rename(
            ROOT,
            &Name::parse("source").unwrap(),
            ROOT,
            Name::parse("destination").unwrap(),
            Replace::Allowed,
        )
        .unwrap();
    assert_eq!(
        volume
            .lookup(ROOT, &Name::parse("destination").unwrap())
            .unwrap(),
        source
    );
    // The old destination is orphaned, not deleted: its handle still resolves.
    assert_eq!(
        volume.handle_attributes(&handle).unwrap().kind,
        ObjectKind::Directory
    );
    assert_eq!(volume.attributes(ROOT).unwrap().kind, ObjectKind::Directory);
    checked(&volume);
    volume.close_handle(handle).unwrap();
    volume.sync().unwrap();
    assert!(matches!(
        volume.attributes(destination),
        Err(Error::NotFound)
    ));
    checked(&volume);
}

#[test]
fn guarded_removal_checks_the_stored_spelling_once_and_preserves_a_veto() {
    for deferred in [false, true] {
        let mut volume = opened();
        volume.set_deferred_checkpoints(deferred).unwrap();
        let object = volume.create_directory(ROOT, name("éCho")).unwrap();
        let alias = name("ÉCHO");
        let before = volume.info().unwrap();
        let media = volume.device.as_ref().unwrap().media.clone();
        let events = media.borrow().events.len();
        let mut calls = 0;
        assert!(matches!(
            volume.remove_with(ROOT, &alias, RemoveKind::Directory, |stored| {
                assert_eq!(stored, "éCho".as_bytes());
                calls += 1;
                Err(Error::NoSpace)
            }),
            Err(Error::NoSpace)
        ));
        assert_eq!(calls, 1);
        assert_eq!(volume.info().unwrap(), before);
        assert_eq!(volume.lookup(ROOT, &alias).unwrap(), object);
        assert_eq!(media.borrow().events.len(), events);
        volume
            .remove_with(ROOT, &alias, RemoveKind::Directory, |stored| {
                assert_eq!(stored, "éCho".as_bytes());
                calls += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(calls, 2);
        assert!(matches!(volume.lookup(ROOT, &alias), Err(Error::NotFound)));
        volume.sync().unwrap();
        checked(&volume);
    }
}

#[test]
fn guarded_removal_does_not_call_back_for_missing_or_readonly_entries() {
    let mut volume = opened();
    assert!(matches!(
        volume.remove_with(ROOT, &name("missing"), RemoveKind::File, |_| {
            panic!("missing entry reached guard")
        }),
        Err(Error::NotFound)
    ));
    volume.create_directory(ROOT, name("present")).unwrap();
    let mut reader = Volume::open(FaultDevice::new(durable(&volume), MountMode::ReadOnly)).unwrap();
    assert!(matches!(
        reader.remove_with(ROOT, &name("present"), RemoveKind::Directory, |_| {
            panic!("read-only entry reached guard")
        }),
        Err(Error::ReadOnly)
    ));
    assert!(reader.lookup(ROOT, &name("present")).is_ok());
}

#[test]
fn remove_rejects_nonempty_directory_and_wrong_kind() {
    let mut volume = opened();
    let parent = volume
        .create_directory(ROOT, Name::parse("parent").unwrap())
        .unwrap();
    volume
        .create_directory(parent, Name::parse("child").unwrap())
        .unwrap();
    assert!(matches!(
        volume.remove(ROOT, &Name::parse("parent").unwrap(), RemoveKind::Directory),
        Err(Error::DirectoryNotEmpty)
    ));
    assert!(matches!(
        volume.remove(ROOT, &Name::parse("parent").unwrap(), RemoveKind::File),
        Err(Error::WrongType)
    ));
}

#[test]
fn handle_registry_is_bounded_and_eject_requires_no_outstanding_handles() {
    let mut volume = opened();
    let mut handles = Vec::new();
    for _ in 0..4096 {
        handles.push(volume.retain_object(ROOT).unwrap());
    }
    assert!(matches!(
        volume.retain_object(ROOT),
        Err(Error::LimitExceeded)
    ));
    assert!(volume.eject().is_err());
    for handle in handles {
        volume.close_handle(handle).unwrap();
    }
    volume.eject().unwrap();
}

#[test]
fn rename_rejects_missing_file_and_orphaned_destination_parents() {
    let mut volume = opened();
    let moved = volume.create_directory(ROOT, name("moved")).unwrap();
    let orphan = volume.create_directory(ROOT, name("orphan")).unwrap();
    let handle = volume.open_directory(orphan).unwrap();
    volume
        .remove(ROOT, &name("orphan"), RemoveKind::Directory)
        .unwrap();
    let generation = volume.info().unwrap().generation;
    for (parent, expected) in [
        (ObjectId(999), "missing"),
        (orphan, "orphaned"),
        (moved, "itself"),
    ] {
        let result = volume.rename(ROOT, &name("moved"), parent, name("x"), Replace::Forbidden);
        match (expected, result) {
            ("missing", Err(Error::NotFound)) => {}
            ("orphaned" | "itself", Err(Error::InvalidInput(_))) => {}
            (expected, result) => panic!("{expected}: {result:?}"),
        }
    }
    assert_eq!(volume.info().unwrap().generation, generation);
    volume.close_handle(handle).unwrap();
    checked(&volume);
}

#[test]
fn maintenance_skips_a_pinned_orphan_and_reclaims_the_next() {
    let mut volume = opened();
    let pinned = volume.create_directory(ROOT, name("pinned")).unwrap();
    let released = volume.create_directory(ROOT, name("released")).unwrap();
    let handle = volume.open_directory(pinned).unwrap();
    for text in ["pinned", "released"] {
        volume
            .remove(ROOT, &name(text), RemoveKind::Directory)
            .unwrap();
    }
    volume.sync().unwrap();
    assert!(matches!(volume.attributes(released), Err(Error::NotFound)));
    assert!(volume.attributes(pinned).unwrap().kind == ObjectKind::Directory);
    checked(&volume);
    volume.close_handle(handle).unwrap();
    // The released directory went at removal, since nothing held it open, so
    // no job waits: the newly unpinned orphan goes at the next step.
    let steps = (1..=4)
        .find(|_| {
            volume.sync().unwrap();
            volume.attributes(pinned).is_err()
        })
        .expect("unpinned orphan is reclaimed within four steps");
    assert_eq!(steps, 1, "the unpinned orphan waited behind other work");
    assert!(matches!(volume.attributes(pinned), Err(Error::NotFound)));
    checked(&volume);
}

type Operation = fn(&mut Volume<FaultDevice>) -> Result<()>;
type Applied = fn(&mut Volume<FaultDevice>) -> bool;

/// Interrupt every output event of one namespace publication, with no, all,
/// or single pending pages persisted, and require recovery to exactly the old
/// or new namespace with a passing structural check.
fn namespace_crash_matrix(base: &BTreeMap<u64, Page>, operation: Operation, applied: Applied) {
    let base_generation = Volume::open(FaultDevice::new(base.clone(), MountMode::ReadOnly))
        .unwrap()
        .info()
        .unwrap()
        .generation;
    let mut event = 0;
    loop {
        let mut completed = true;
        for whole_mask in [0, u64::MAX, 1, 2, !1] {
            let device = FaultDevice::new(base.clone(), MountMode::ReadWrite);
            let media = device.media.clone();
            let mut volume = Volume::open(device).unwrap();
            media.borrow_mut().failure = Some(Failure {
                event,
                whole_mask,
                tear: None,
            });
            if operation(&mut volume).is_ok() {
                continue;
            }
            completed = false;
            assert!(matches!(volume.sync(), Err(Error::Faulted)));
            assert!(matches!(operation(&mut volume), Err(Error::Faulted)));
            drop(volume);
            media.borrow_mut().crash();
            let image = media.borrow().durable.clone();

            let mut reader =
                Volume::open(FaultDevice::new(image.clone(), MountMode::ReadOnly)).unwrap();
            reader.check().unwrap();
            let generation = reader.info().unwrap().generation;
            assert!(
                [base_generation, base_generation + 1].contains(&generation),
                "event {event} mask {whole_mask:#x}"
            );
            assert_eq!(
                applied(&mut reader),
                generation == base_generation + 1,
                "event {event} mask {whole_mask:#x}"
            );

            let mut writer = Volume::open(FaultDevice::new(image, MountMode::ReadWrite)).unwrap();
            writer.sync().unwrap();
            checked(&writer);
        }
        if completed {
            assert!(
                event > 4,
                "operation finished before its publication events"
            );
            return;
        }
        event += 1;
    }
}

#[test]
fn interrupted_namespace_publications_recover_old_or_new_state() {
    let base = formatted();
    namespace_crash_matrix(
        &base,
        |volume| volume.create_directory(ROOT, name("created")).map(|_| ()),
        |volume| volume.lookup(ROOT, &name("created")).is_ok(),
    );

    let mut volume = opened();
    volume.create_directory(ROOT, name("victim")).unwrap();
    volume.create_directory(ROOT, name("source")).unwrap();
    volume.create_directory(ROOT, name("target")).unwrap();
    let populated = volume.into_model_image();
    namespace_crash_matrix(
        &populated,
        |volume| volume.remove(ROOT, &name("victim"), RemoveKind::Directory),
        |volume| volume.lookup(ROOT, &name("victim")).is_err(),
    );
    namespace_crash_matrix(
        &populated,
        |volume| {
            volume.rename(
                ROOT,
                &name("source"),
                ROOT,
                name("target"),
                Replace::Allowed,
            )
        },
        |volume| volume.lookup(ROOT, &name("source")).is_err(),
    );
}

#[test]
fn file_objects_round_trip_and_reject_wrong_types() {
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("a.txt")).unwrap();
    let object = volume.lookup(ROOT, &name("a.txt")).unwrap();
    let attributes = volume.handle_attributes(&file).unwrap();
    assert_eq!(
        (attributes.kind, attributes.size, attributes.allocated_bytes),
        (ObjectKind::File, 0, 0)
    );
    assert_eq!(volume.read(&file, 0, &mut [0; 8]).unwrap(), 0);
    assert!(matches!(
        volume.create_file(ROOT, name("A.TXT")),
        Err(Error::NameCollision)
    ));
    assert!(matches!(
        volume.open_directory(object),
        Err(Error::WrongType)
    ));
    volume.create_directory(ROOT, name("dir")).unwrap();
    assert!(matches!(
        volume.open_file(ROOT, &name("dir"), Access::Read),
        Err(Error::WrongType)
    ));
    let directory = volume.open_directory(ROOT).unwrap();
    assert!(matches!(
        volume.read(&directory, 0, &mut [0; 8]),
        Err(Error::WrongType)
    ));
    let reader = volume
        .open_file(ROOT, &name("a.txt"), Access::Read)
        .unwrap();
    for handle in [file, directory, reader] {
        volume.close_handle(handle).unwrap();
    }
    checked(&volume);

    let image = volume.into_model_image();
    let mut read_only = Volume::open(FaultDevice::new(image.clone(), MountMode::ReadOnly)).unwrap();
    assert!(matches!(
        read_only.open_file(ROOT, &name("a.txt"), Access::ReadWrite),
        Err(Error::ReadOnly)
    ));
    assert!(matches!(
        read_only.create_file(ROOT, name("b.txt")),
        Err(Error::ReadOnly)
    ));
    let reader = read_only
        .open_file(ROOT, &name("a.txt"), Access::Read)
        .unwrap();
    assert_eq!(read_only.read(&reader, 0, &mut [0; 8]).unwrap(), 0);
    read_only.close_handle(reader).unwrap();

    let mut reopened = Volume::open(FaultDevice::new(image, MountMode::ReadWrite)).unwrap();
    assert_eq!(reopened.lookup(ROOT, &name("a.txt")).unwrap(), object);
}

/// Encode a file straight from FORMAT.md records, independent of the write
/// path: each `(unit, bytes)` becomes a backed unit; other units are holes.
/// Returns the object and the physical block of each unit's first block.
fn backed_file(
    volume: &mut Volume<FaultDevice>,
    file: &str,
    size: u64,
    units: &[(u64, Vec<u8>)],
) -> (ObjectId, Vec<u64>) {
    use crate::allocation::Transaction;
    use crate::format::{BLOCK_BYTES, CHECKSUM_GROUP_BYTES, data_unit_crc, set_checksum_slot};
    use crate::volume::file::{extent_key, group_key};
    use crate::volume::namespace::{Inode, inode_key};

    let handle = volume.create_file(ROOT, name(file)).unwrap();
    let object = volume.handles.validate(&handle).unwrap();
    volume.close_handle(handle).unwrap();
    let root = volume.root.clone().unwrap();
    let device = volume.device.as_mut().unwrap();
    let mut transaction = Transaction::new(device, &root).unwrap();
    let mut groups = BTreeMap::new();
    let mut extents: Vec<(u64, u64, u64)> = Vec::new();
    let mut firsts = Vec::new();
    let mut total = 0;
    for (unit, bytes) in units {
        let count = bytes.len().div_ceil(BLOCK_BYTES) as u64;
        let mut logical = unit * 16;
        for (start, run) in transaction.allocate_data(count, 0).unwrap() {
            for index in 0..run {
                let at = ((logical - unit * 16 + index) as usize) * BLOCK_BYTES;
                let mut page = Page(Box::new([0; BLOCK_BYTES]));
                let end = bytes.len().min(at + BLOCK_BYTES);
                page.0[..end - at].copy_from_slice(&bytes[at..end]);
                transaction.write_data(start + index, page).unwrap();
            }
            if logical == unit * 16 {
                firsts.push(start);
            }
            match extents.last_mut() {
                Some((l, p, c)) if *l + *c == logical && *p + *c == start => *c += run,
                _ => extents.push((logical, start, run)),
            }
            logical += run;
        }
        total += count;
        let crc = data_unit_crc(&root.uuid, object.0, *unit, bytes).unwrap();
        let group = groups
            .entry(unit / 256)
            .or_insert([0u8; CHECKSUM_GROUP_BYTES]);
        set_checksum_slot(group, (unit % 256) as usize, Some(crc));
    }
    let mut catalog = transaction.catalog.clone();
    for (logical, physical, count) in extents {
        let mut value = physical.to_le_bytes().to_vec();
        value.extend_from_slice(&(count as u32).to_le_bytes());
        value.extend_from_slice(&[0; 4]);
        catalog = transaction
            .forest
            .put(
                catalog,
                TreeKind::Catalog,
                &extent_key(object.0, logical),
                &value,
            )
            .unwrap();
    }
    for (index, group) in groups {
        catalog = transaction
            .forest
            .put(
                catalog,
                TreeKind::Catalog,
                &group_key(object.0, index),
                crate::format::compact_group(&crate::format::expand_group(&group)),
            )
            .unwrap();
    }
    let key = inode_key(object.0);
    let mut inode = Inode::decode(
        &transaction
            .forest
            .lookup(&catalog, TreeKind::Catalog, &key)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    inode.size = size;
    inode.allocated_blocks = total;
    catalog = transaction
        .forest
        .put(catalog, TreeKind::Catalog, &key, &inode.encode())
        .unwrap();
    transaction.catalog = catalog;
    let (next, writes, ..) = transaction.seal(false).unwrap().into_parts(&root).unwrap();
    let writes: Vec<_> = writes
        .into_iter()
        .map(|(block, page)| BlockWrite { block, page })
        .collect();
    volume
        .publish(next, writes.clone(), &[], None, Vec::new(), 0)
        .unwrap();
    (object, firsts)
}

#[test]
fn read_verifies_backed_units_holes_and_padding() {
    let mut volume = opened();
    let unit = |seed: u8, len: usize| -> Vec<u8> {
        (0..len)
            .map(|index| seed.wrapping_add(index as u8))
            .collect()
    };
    let size = 3 * 65_536 + 100;
    let (object, firsts) = backed_file(
        &mut volume,
        "data.bin",
        size,
        &[
            (0, unit(1, 65_536)),
            (1, unit(7, 65_536)),
            (3, unit(9, 100)),
        ],
    );
    assert_eq!(firsts[1], firsts[0] + 16, "units 0 and 1 share one extent");
    checked(&volume);
    assert_eq!(
        volume.attributes(object).unwrap().allocated_bytes,
        33 * 4096
    );

    let mut expected = unit(1, 65_536);
    expected.extend(unit(7, 65_536));
    expected.extend(vec![0; 65_536]);
    expected.extend(unit(9, 100));
    let file = volume
        .open_file(ROOT, &name("data.bin"), Access::Read)
        .unwrap();
    let mut out = vec![0xFF; 400_000];
    assert_eq!(volume.read(&file, 0, &mut out).unwrap(), size as usize);
    assert_eq!(&out[..size as usize], &expected[..]);
    let mut across = [0; 20];
    assert_eq!(volume.read(&file, 65_530, &mut across).unwrap(), 20);
    assert_eq!(&across, &expected[65_530..65_550]);
    assert_eq!(volume.read(&file, size, &mut across).unwrap(), 0);
    assert_eq!(volume.read(&file, size + 1, &mut across).unwrap(), 0);

    let flip = |volume: &mut Volume<FaultDevice>, block: u64, at: usize| {
        let media = volume.device.as_ref().unwrap().media.clone();
        media.borrow_mut().durable.get_mut(&block).unwrap().0[at] ^= 0x40;
    };
    flip(&mut volume, firsts[1] + 3, 17);
    assert!(matches!(
        volume.read(&file, 65_536, &mut across),
        Err(Error::Corruption {
            reason: "file data checksum mismatch",
            ..
        })
    ));
    assert_eq!(volume.read(&file, 0, &mut across).unwrap(), 20);
    flip(&mut volume, firsts[1] + 3, 17);

    flip(&mut volume, firsts[2], 100);
    assert!(matches!(
        volume.read(&file, 3 * 65_536, &mut across),
        Err(Error::Corruption {
            reason: "nonzero data past EOF",
            ..
        })
    ));
    volume.close_handle(file).unwrap();
}

/// A file-data read names the device offset where its extent ends, on the
/// contiguous path and on the unit-by-unit one, and a contiguous read near
/// that end names where the file's next extent starts.
#[test]
fn data_reads_name_the_end_of_their_extent_and_the_next_one() {
    let mut volume = opened();
    let two = 2 * UNIT_BYTES;
    let first = volume.create_file(ROOT, name("first")).unwrap();
    let object = volume.handles.validate(&first).unwrap();
    volume.write(&first, 0, &vec![3; two]).unwrap();
    // Another file's data lands after the first run, so the append cannot
    // continue it.
    let other = volume.create_file(ROOT, name("other")).unwrap();
    volume.write(&other, 0, &vec![5; UNIT_BYTES]).unwrap();
    volume
        .write(&first, two as u64, &vec![7; two + 17])
        .unwrap();
    volume.sync().unwrap();
    let runs = extents(&mut volume, object);
    assert_eq!(runs.len(), 2, "{runs:?}");
    let offset = |block: u64| block * BLOCK_BYTES as u64;
    let ends = [offset(runs[0].1 + runs[0].2), offset(runs[1].1 + runs[1].2)];

    let media = volume.device.as_ref().unwrap().media.clone();
    media.borrow_mut().data_runs.clear();
    let mut out = vec![0; two];
    assert_eq!(volume.read(&first, 0, &mut out).unwrap(), two);
    assert_eq!(volume.read(&first, two as u64, &mut out).unwrap(), two);
    // The tail is not whole units, so it is read unit by unit.
    assert_eq!(volume.read(&first, 2 * two as u64, &mut out).unwrap(), 17);
    assert_eq!(
        media.borrow().data_runs,
        [
            (offset(runs[0].1), ends[0], Some(offset(runs[1].1))),
            (offset(runs[1].1), ends[1], None),
            (offset(runs[1].1) + two as u64, ends[1], None)
        ]
    );
}

#[test]
fn contiguous_reads_verify_group_boundaries_partial_units_and_device_errors() {
    let mut volume = opened();
    let offset = 254 * UNIT_BYTES;
    let bytes = [vec![3; UNIT_BYTES], vec![7; UNIT_BYTES], vec![9; 17]].concat();
    let (_, blocks) = backed_file(
        &mut volume,
        "group-boundary",
        (offset + bytes.len()) as u64,
        &[
            (254, vec![3; UNIT_BYTES]),
            (255, vec![7; UNIT_BYTES]),
            (256, vec![9; 17]),
        ],
    );
    assert_eq!(blocks[1], blocks[0] + 16);
    assert_eq!(blocks[2], blocks[1] + 16);
    let file = volume
        .open_file(ROOT, &name("group-boundary"), Access::Read)
        .unwrap();
    let mut out = vec![0; bytes.len()];
    assert_eq!(
        volume.read(&file, offset as u64, &mut out).unwrap(),
        bytes.len()
    );
    assert_eq!(out, bytes);
    let media = volume.device.as_ref().unwrap().media.clone();
    // Verify bytes outside the caller's prefix of the last checksum unit too.
    media.borrow_mut().durable.get_mut(&blocks[2]).unwrap().0[16] ^= 1;
    out.fill(0xff);
    assert!(matches!(
        volume.read(&file, offset as u64, &mut out[..2 * UNIT_BYTES + 1]),
        Err(Error::Corruption {
            reason: "file data checksum mismatch",
            ..
        })
    ));
    media.borrow_mut().durable.get_mut(&blocks[2]).unwrap().0[16] ^= 1;
    media.borrow_mut().durable.get_mut(&blocks[2]).unwrap().0[17] = 1;
    assert!(matches!(
        volume.read(&file, offset as u64, &mut out),
        Err(Error::Corruption {
            reason: "nonzero data past EOF",
            ..
        })
    ));
    media.borrow_mut().durable.get_mut(&blocks[2]).unwrap().0[17] = 0;
    media.borrow_mut().unreadable = Some(blocks[1]);
    assert!(matches!(
        volume.read(&file, offset as u64, &mut out),
        Err(Error::Io { .. })
    ));
    volume.close_handle(file).unwrap();
}

#[test]
fn contiguous_full_units_cross_checksum_groups_and_reject_damage() {
    let mut volume = opened();
    let offset = 255 * UNIT_BYTES;
    let expected = [vec![3; UNIT_BYTES], vec![7; UNIT_BYTES]].concat();
    let (_, blocks) = backed_file(
        &mut volume,
        "full-group-boundary",
        (offset + expected.len()) as u64,
        &[(255, vec![3; UNIT_BYTES]), (256, vec![7; UNIT_BYTES])],
    );
    assert_eq!(blocks[1], blocks[0] + 16);
    let file = volume
        .open_file(ROOT, &name("full-group-boundary"), Access::Read)
        .unwrap();
    let mut out = vec![0; expected.len()];
    assert_eq!(
        volume.read(&file, offset as u64, &mut out).unwrap(),
        expected.len()
    );
    assert_eq!(out, expected);
    let media = volume.device.as_ref().unwrap().media.clone();
    media.borrow_mut().durable.get_mut(&blocks[1]).unwrap().0[20] ^= 1;
    assert!(matches!(
        volume.read(&file, offset as u64, &mut out),
        Err(Error::Corruption {
            reason: "file data checksum mismatch",
            ..
        })
    ));
    media.borrow_mut().durable.get_mut(&blocks[1]).unwrap().0[20] ^= 1;
    media.borrow_mut().unreadable = Some(blocks[1]);
    assert!(matches!(
        volume.read(&file, offset as u64, &mut out),
        Err(Error::Io { .. })
    ));
    volume.close_handle(file).unwrap();
}

fn retirement_records(volume: &mut Volume<FaultDevice>) -> usize {
    let root = volume.root.clone().unwrap();
    let device = volume.device.as_mut().unwrap();
    let mut forest = crate::tree::Forest::new(device, &root).unwrap();
    let cleanup = forest.root(root.cleanup, TreeKind::Cleanup).unwrap();
    forest
        .scan(&cleanup, TreeKind::Cleanup, &[1], 64)
        .unwrap()
        .iter()
        .filter(|(key, _)| key[0] == 1)
        .count()
}

#[test]
fn writes_match_an_in_memory_model() {
    const UNIT: usize = 65_536;
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("model.bin")).unwrap();
    let mut model: Vec<u8> = Vec::new();
    let mut backed = BTreeSet::new();
    let mut random = 0x5eed_u64;
    let mut next = |bound: u64| {
        random = random
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (random >> 33) % bound
    };
    for step in 0..40 {
        let offset = if step == 17 { 3 << 20 } else { next(600_000) } as usize;
        let len = 1 + next(150_000) as usize;
        let seed = step as u8;
        let bytes: Vec<u8> = (0..len)
            .map(|index| seed.wrapping_mul(31).wrapping_add(index as u8))
            .collect();
        assert_eq!(volume.write(&file, offset as u64, &bytes).unwrap(), len);
        if model.len() < offset + len {
            model.resize(offset + len, 0);
        }
        model[offset..offset + len].copy_from_slice(&bytes);
        backed.extend(offset / UNIT..=(offset + len - 1) / UNIT);

        let attributes = volume.handle_attributes(&file).unwrap();
        assert_eq!(attributes.size, model.len() as u64, "step {step}");
        let expected_blocks: usize = backed
            .iter()
            .map(|unit| (model.len() - unit * UNIT).min(UNIT).div_ceil(4096))
            .sum();
        assert_eq!(attributes.allocated_bytes, expected_blocks as u64 * 4096);
        let mut out = vec![0; model.len()];
        let mut at = 0;
        while at < model.len() {
            at += volume.read(&file, at as u64, &mut out[at..]).unwrap();
        }
        assert!(out == model, "step {step} content differs");
        if step % 10 == 9 {
            checked(&volume);
        }
    }
    volume.close_handle(file).unwrap();
}

#[test]
fn grouped_write_preserves_checksums_across_groups_and_sparse_growth() {
    let boundary = 256 * UNIT_BYTES as u64;
    let offset = boundary - UNIT_BYTES as u64 - 7;
    let bytes: Vec<u8> = (0..3 * UNIT_BYTES).map(|i| (i % 251) as u8).collect();
    for deferred in [false, true] {
        let mut volume = opened();
        volume.set_deferred_checkpoints(deferred).unwrap();
        let file = volume.create_file(ROOT, name("groups.bin")).unwrap();
        volume.write(&file, 0, b"tail").unwrap();
        // Preserve a slot beyond the units replaced by the grouped write.
        let sentinel = boundary + 4 * UNIT_BYTES as u64;
        volume.write(&file, sentinel, b"keep").unwrap();
        volume.write(&file, offset, &bytes).unwrap();
        // Partial overwrites must still verify the checksum from that write.
        volume.write(&file, boundary - 1, b"patch").unwrap();
        let mut expected = bytes.clone();
        let patch = (boundary - 1 - offset) as usize;
        expected[patch..patch + 5].copy_from_slice(b"patch");
        volume.sync().unwrap();
        checked(&volume);
        let mut reader =
            Volume::open(FaultDevice::new(durable(&volume), MountMode::ReadOnly)).unwrap();
        let handle = reader
            .open_file(ROOT, &name("groups.bin"), Access::Read)
            .unwrap();
        let mut out = vec![0; expected.len()];
        assert_eq!(
            reader.read(&handle, offset, &mut out).unwrap(),
            expected.len()
        );
        assert_eq!(out, expected);
        let mut tail = [0; 8];
        reader.read(&handle, 0, &mut tail).unwrap();
        assert_eq!(&tail, b"tail\0\0\0\0");
        reader.read(&handle, sentinel, &mut tail[..4]).unwrap();
        assert_eq!(&tail[..4], b"keep");
        let mut findings = 0;
        reader.scrub(&mut |_| findings += 1).unwrap();
        assert_eq!(findings, 0);
    }
}

/// Extent records of `object` as (logical, physical, count).
fn extents(volume: &mut Volume<FaultDevice>, object: ObjectId) -> Vec<(u64, u64, u64)> {
    use crate::volume::file::extent_key;
    volume
        .read_catalog(|forest, catalog| {
            forest.scan(&catalog, TreeKind::Catalog, &extent_key(object.0, 0), 64)
        })
        .unwrap()
        .into_iter()
        .take_while(|(key, _)| key[..9] == extent_key(object.0, 0)[..9])
        .map(|(key, value)| {
            (
                u64::from_be_bytes(key[9..].try_into().unwrap()),
                u64::from_le_bytes(value[..8].try_into().unwrap()),
                u64::from(u32::from_le_bytes(value[8..12].try_into().unwrap())),
            )
        })
        .collect()
}

#[test]
fn sequential_writes_coalesce_extents_and_overwrites_rejoin_them() {
    let bytes: Vec<u8> = (0..1 << 20).map(|i| (i % 253) as u8).collect();
    for deferred in [false, true] {
        let mut volume = opened();
        volume.set_deferred_checkpoints(deferred).unwrap();
        let file = volume.create_file(ROOT, name("seq.bin")).unwrap();
        let object = volume.handles.validate(&file).unwrap();
        let mut expected = Vec::new();
        for _ in 0..3 {
            volume.write(&file, expected.len() as u64, &bytes).unwrap();
            expected.extend_from_slice(&bytes);
        }
        // Fresh sequential allocation yields one physical run.
        let before = extents(&mut volume, object);
        assert_eq!(before.len(), 1, "{before:?}");
        assert_eq!(before[0].2, 768);
        // One replaced block in the middle splits the run in three, not five.
        volume.write(&file, (1 << 20) + 8192, b"middle").unwrap();
        expected[(1 << 20) + 8192..(1 << 20) + 8198].copy_from_slice(b"middle");
        let after = extents(&mut volume, object);
        assert_eq!(after.len(), 3, "{after:?}");
        assert_eq!(after[0], (0, before[0].1, 258));
        assert_eq!(after[1].2, 1);
        assert_eq!(after[2], (259, before[0].1 + 259, 768 - 259));
        // A shrink into the middle of a long extent keeps its prefix only.
        volume.truncate(&file, (2 << 20) + 100).unwrap();
        expected.truncate((2 << 20) + 100);
        volume.sync().unwrap();
        checked(&volume);
        let mut reader =
            Volume::open(FaultDevice::new(durable(&volume), MountMode::ReadOnly)).unwrap();
        let handle = reader
            .open_file(ROOT, &name("seq.bin"), Access::Read)
            .unwrap();
        let mut out = vec![0; expected.len()];
        let mut read = 0;
        while read < out.len() {
            read += reader.read(&handle, read as u64, &mut out[read..]).unwrap();
        }
        assert_eq!(out, expected);
        let mut findings = 0;
        reader.scrub(&mut |_| findings += 1).unwrap();
        assert_eq!(findings, 0);
    }
}

#[test]
fn batched_data_is_read_back_from_unflushed_device_writes() {
    let mut volume = opened();
    volume.set_deferred_checkpoints(true).unwrap();
    let media = volume.device.as_ref().unwrap().media.clone();
    let file = volume.create_file(ROOT, name("queued.bin")).unwrap();
    let object = volume.handles.validate(&file).unwrap();
    let writes = |media: &std::rc::Rc<std::cell::RefCell<crate::volume::model::Media>>| {
        media
            .borrow()
            .events
            .iter()
            .filter(|event| matches!(event, Event::Write(_)))
            .count()
    };
    let before = writes(&media);
    volume.write(&file, 0, &[7; 8192]).unwrap();
    // The accepted write's two pages already went to the device, unflushed,
    // and the patch reads its unit back from there.
    assert_eq!(writes(&media), before + 2);
    volume.write(&file, 100, b"patch").unwrap();
    let mut out = [0; 8192];
    assert_eq!(volume.read(&file, 0, &mut out).unwrap(), 8192);
    assert_eq!(&out[100..105], b"patch");
    assert!(out[..100].iter().chain(&out[105..]).all(|byte| *byte == 7));
    // Once published, the same range must come from the device and fail.
    volume.sync().unwrap();
    let block = extents(&mut volume, object)[0].1;
    media.borrow_mut().unreadable = Some(block);
    assert!(volume.read(&file, 0, &mut out).is_err());
}

#[test]
fn ownership_windows_agree_across_group_and_window_boundaries() {
    let mut volume = opened();
    let bytes = vec![5; 1 << 20];
    for index in 0..6 {
        let file = volume
            .create_file(ROOT, name(&format!("f{index}")))
            .unwrap();
        volume.write(&file, 0, &bytes).unwrap();
        volume.close_handle(file).unwrap();
    }
    // Leave retired blocks and a detached tree behind for the check to own.
    volume.remove(ROOT, &name("f1"), RemoveKind::File).unwrap();
    volume.remove(ROOT, &name("f4"), RemoveKind::File).unwrap();
    volume.sync_publication().unwrap();
    let root = volume.root.clone().unwrap();
    let device = volume.device.as_mut().unwrap();
    let whole = crate::check::validate_ownership(device, &root).unwrap();
    assert!(whole.data_blocks > 0);
    for window in [4096, 12_288, 16_384, 24_576] {
        let windowed = crate::check::ownership_in_windows(device, &root, window).unwrap();
        assert_eq!(windowed, whole, "window {window}");
    }
    // A free count that disagrees with ownership fails at any window size.
    let mut damaged = root.clone();
    damaged.free_blocks += 1;
    for window in [4096, 1 << 28] {
        assert!(crate::check::ownership_in_windows(device, &damaged, window).is_err());
    }
}

#[test]
fn write_accepts_a_one_mib_prefix_and_requires_a_writable_handle() {
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("big.bin")).unwrap();
    let generation = volume.info().unwrap().generation;
    assert_eq!(volume.write(&file, 5, &[]).unwrap(), 0);
    assert_eq!(volume.info().unwrap().generation, generation);
    assert_eq!(volume.handle_attributes(&file).unwrap().size, 0);

    let bytes = vec![0xA5; (1 << 20) + 10];
    assert_eq!(volume.write(&file, 0, &bytes).unwrap(), 1 << 20);
    assert_eq!(volume.handle_attributes(&file).unwrap().size, 1 << 20);
    let reader = volume
        .open_file(ROOT, &name("big.bin"), Access::Read)
        .unwrap();
    assert!(matches!(
        volume.write(&reader, 0, b"x"),
        Err(Error::ReadOnly)
    ));
    assert!(matches!(
        volume.write(&file, i64::MAX as u64, b"x"),
        Err(Error::LimitExceeded)
    ));
    volume.close_handle(reader).unwrap();
    volume.close_handle(file).unwrap();
    checked(&volume);
}

#[test]
fn damaged_units_accept_only_complete_overwrites() {
    let mut volume = opened();
    let pattern = |seed: u8| -> Vec<u8> { (0..65_536).map(|i| seed ^ (i as u8)).collect() };
    let (_, firsts) = backed_file(
        &mut volume,
        "damaged.bin",
        2 * 65_536,
        &[(0, pattern(3)), (1, pattern(5))],
    );
    let media = volume.device.as_ref().unwrap().media.clone();
    media
        .borrow_mut()
        .durable
        .get_mut(&(firsts[0] + 2))
        .unwrap()
        .0[9] ^= 1;
    let file = volume
        .open_file(ROOT, &name("damaged.bin"), Access::ReadWrite)
        .unwrap();
    let generation = volume.info().unwrap().generation;
    assert!(matches!(
        volume.write(&file, 100, b"partial"),
        Err(Error::Corruption { .. })
    ));
    assert_eq!(volume.info().unwrap().generation, generation);

    assert!(matches!(
        volume.write(&file, 0, &pattern(9)),
        Err(Error::Faulted)
    ));
    volume.close_handle(file).unwrap();
    let mut volume =
        Volume::open(FaultDevice::new(durable(&volume), MountMode::ReadWrite)).unwrap();
    let file = volume
        .open_file(ROOT, &name("damaged.bin"), Access::ReadWrite)
        .unwrap();
    let replacement = pattern(9);
    assert_eq!(volume.write(&file, 0, &replacement).unwrap(), 65_536);
    let mut out = vec![0; 2 * 65_536];
    assert_eq!(volume.read(&file, 0, &mut out).unwrap(), 2 * 65_536);
    assert_eq!(&out[..65_536], &replacement[..]);
    assert_eq!(&out[65_536..], &pattern(5)[..]);
    volume.close_handle(file).unwrap();
    checked(&volume);
}

#[test]
fn overwritten_data_is_retired_then_reclaimed() {
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("churn.bin")).unwrap();
    for round in 0..6u8 {
        volume.write(&file, 0, &vec![round; 200_000]).unwrap();
    }
    assert!(retirement_records(&mut volume) > 0);
    checked(&volume);
    for _ in 0..64 {
        volume.sync().unwrap();
        if retirement_records(&mut volume) == 0 {
            break;
        }
    }
    assert_eq!(retirement_records(&mut volume), 0);
    checked(&volume);
    let mut out = vec![0; 200_000];
    assert_eq!(volume.read(&file, 0, &mut out).unwrap(), 200_000);
    assert!(out.iter().all(|byte| *byte == 5));
    volume.close_handle(file).unwrap();
}

#[test]
fn interrupted_writes_recover_old_or_new_contents() {
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("f")).unwrap();
    volume.write(&file, 0, &vec![0x11; 100_000]).unwrap();
    volume.close_handle(file).unwrap();
    let base = volume.into_model_image();
    namespace_crash_matrix(
        &base,
        |volume| {
            let file = volume.open_file(ROOT, &name("f"), Access::ReadWrite)?;
            volume.write(&file, 60_000, &vec![0x5A; 70_000]).map(|_| ())
        },
        |volume| {
            let file = volume.open_file(ROOT, &name("f"), Access::Read).unwrap();
            let size = volume.handle_attributes(&file).unwrap().size;
            let mut out = vec![0; size as usize];
            let mut at = 0;
            while at < out.len() {
                at += volume.read(&file, at as u64, &mut out[at..]).unwrap();
            }
            volume.close_handle(file).unwrap();
            assert!(out[..60_000].iter().all(|byte| *byte == 0x11));
            match size {
                100_000 => {
                    assert!(out[60_000..].iter().all(|byte| *byte == 0x11));
                    false
                }
                130_000 => {
                    assert!(out[60_000..].iter().all(|byte| *byte == 0x5A));
                    true
                }
                other => panic!("unexpected size {other}"),
            }
        },
    );
}

fn read_all(volume: &mut Volume<FaultDevice>, file: &Handle, size: usize) -> Vec<u8> {
    let mut out = vec![0; size];
    let mut at = 0;
    while at < size {
        let read = volume.read(file, at as u64, &mut out[at..]).unwrap();
        assert!(read > 0, "short read at {at} of {size}");
        at += read;
    }
    out
}

fn cleanup_jobs(volume: &mut Volume<FaultDevice>) -> usize {
    let root = volume.root.clone().unwrap();
    let device = volume.device.as_mut().unwrap();
    let mut forest = crate::tree::Forest::new(device, &root).unwrap();
    let cleanup = forest.root(root.cleanup, TreeKind::Cleanup).unwrap();
    forest
        .scan(&cleanup, TreeKind::Cleanup, &[2], 64)
        .unwrap()
        .iter()
        .filter(|(key, _)| key[0] == 2)
        .count()
}

#[test]
fn writes_and_truncates_match_an_in_memory_model() {
    const UNIT: usize = 65_536;
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("sized.bin")).unwrap();
    let mut model: Vec<u8> = Vec::new();
    let mut backed = BTreeSet::new();
    let mut random = 0xfeed_u64;
    let mut next = |bound: u64| {
        random = random
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (random >> 33) % bound
    };
    for step in 0..60 {
        if next(5) < 3 {
            let offset = next(500_000) as usize;
            let len = 1 + next(120_000) as usize;
            let bytes: Vec<u8> = (0..len)
                .map(|index| (step as u8).wrapping_mul(13) ^ index as u8)
                .collect();
            assert_eq!(volume.write(&file, offset as u64, &bytes).unwrap(), len);
            if model.len() < offset + len {
                model.resize(offset + len, 0);
            }
            model[offset..offset + len].copy_from_slice(&bytes);
            backed.extend(offset / UNIT..=(offset + len - 1) / UNIT);
        } else {
            let size = next(700_000) as usize;
            volume.truncate(&file, size as u64).unwrap();
            model.resize(size, 0);
            backed.retain(|unit| unit * UNIT < size);
        }
        let attributes = volume.handle_attributes(&file).unwrap();
        assert_eq!(attributes.size, model.len() as u64, "step {step}");
        let blocks: usize = backed
            .iter()
            .map(|unit| (model.len() - unit * UNIT).min(UNIT).div_ceil(4096))
            .sum();
        assert_eq!(
            attributes.allocated_bytes,
            blocks as u64 * 4096,
            "step {step}"
        );
        let out = read_all(&mut volume, &file, model.len());
        assert!(out == model, "step {step} content differs");
        if step % 10 == 9 {
            checked(&volume);
        }
    }
    for _ in 0..256 {
        volume.sync().unwrap();
        if cleanup_jobs(&mut volume) == 0 && retirement_records(&mut volume) == 0 {
            break;
        }
    }
    assert_eq!(
        (cleanup_jobs(&mut volume), retirement_records(&mut volume)),
        (0, 0)
    );
    checked(&volume);
    let out = read_all(&mut volume, &file, model.len());
    assert!(out == model);
    volume.close_handle(file).unwrap();
}

#[test]
fn sparse_files_round_trip_past_four_gib() {
    const GIB: u64 = 1 << 30;
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("sparse.bin")).unwrap();
    volume.write(&file, 0, b"head").unwrap();
    volume.write(&file, 5 * GIB, b"tail past five GiB").unwrap();
    let size = 5 * GIB + 18;
    let attributes = volume.handle_attributes(&file).unwrap();
    assert_eq!(attributes.size, size);
    // Growing past the 4-byte tail rewrote unit 0 as a full 16-block unit.
    assert_eq!(attributes.allocated_bytes, 17 * 4096);
    let mut out = [0xFF; 32];
    assert_eq!(volume.read(&file, 0, &mut out[..4]).unwrap(), 4);
    assert_eq!(&out[..4], b"head");
    assert_eq!(volume.read(&file, 4 * GIB - 8, &mut out[..16]).unwrap(), 16);
    assert!(out[..16].iter().all(|byte| *byte == 0));
    assert_eq!(volume.read(&file, 5 * GIB, &mut out).unwrap(), 18);
    assert_eq!(&out[..18], b"tail past five GiB");
    checked(&volume);

    volume.truncate(&file, 4 * GIB + 3).unwrap();
    assert_eq!(volume.read(&file, 4 * GIB, &mut out).unwrap(), 3);
    assert_eq!(&out[..3], &[0, 0, 0]);
    volume.truncate(&file, 6 * GIB).unwrap();
    assert_eq!(volume.read(&file, 5 * GIB, &mut out).unwrap(), 32);
    assert!(
        out.iter().all(|byte| *byte == 0),
        "old tail bytes reappeared"
    );
    assert_eq!(
        volume.handle_attributes(&file).unwrap().allocated_bytes,
        16 * 4096
    );
    checked(&volume);
    volume.close_handle(file).unwrap();
}

#[test]
fn truncate_detaches_discarded_units_into_cleanup_jobs() {
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("shrink.bin")).unwrap();
    volume.write(&file, 0, &vec![0x77; 1 << 20]).unwrap();
    let reader = volume
        .open_file(ROOT, &name("shrink.bin"), Access::Read)
        .unwrap();
    assert!(matches!(volume.truncate(&reader, 0), Err(Error::ReadOnly)));
    volume.close_handle(reader).unwrap();

    volume.truncate(&file, 100).unwrap();
    assert!(
        cleanup_jobs(&mut volume) > 0,
        "discarded units were not detached"
    );
    assert_eq!(
        volume.handle_attributes(&file).unwrap().allocated_bytes,
        4096
    );
    checked(&volume);
    for _ in 0..256 {
        volume.sync().unwrap();
        if cleanup_jobs(&mut volume) == 0 && retirement_records(&mut volume) == 0 {
            break;
        }
    }
    assert_eq!(
        (cleanup_jobs(&mut volume), retirement_records(&mut volume)),
        (0, 0)
    );
    checked(&volume);
    assert_eq!(read_all(&mut volume, &file, 100), vec![0x77; 100]);
    volume.close_handle(file).unwrap();
}

#[test]
fn interrupted_truncates_recover_old_or_new_size() {
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("t")).unwrap();
    volume.write(&file, 0, &vec![0x33; 300_000]).unwrap();
    volume.close_handle(file).unwrap();
    let base = volume.into_model_image();
    namespace_crash_matrix(
        &base,
        |volume| {
            let file = volume.open_file(ROOT, &name("t"), Access::ReadWrite)?;
            volume.truncate(&file, 70_000)
        },
        |volume| {
            let file = volume.open_file(ROOT, &name("t"), Access::Read).unwrap();
            let size = volume.handle_attributes(&file).unwrap().size;
            let out = read_all(volume, &file, size as usize);
            volume.close_handle(file).unwrap();
            assert!(out.iter().all(|byte| *byte == 0x33));
            match size {
                300_000 => false,
                70_000 => true,
                other => panic!("unexpected size {other}"),
            }
        },
    );
}

fn cleanup_records(volume: &mut Volume<FaultDevice>) -> usize {
    let root = volume.root.clone().unwrap();
    let device = volume.device.as_mut().unwrap();
    let mut forest = crate::tree::Forest::new(device, &root).unwrap();
    let cleanup = forest.root(root.cleanup, TreeKind::Cleanup).unwrap();
    forest
        .scan(&cleanup, TreeKind::Cleanup, &[], 64)
        .unwrap()
        .len()
}

/// Run maintenance until the cleanup tree is empty; returns the step count.
fn drain_cleanup(volume: &mut Volume<FaultDevice>) -> usize {
    for step in 0..512 {
        if cleanup_records(volume) == 0 {
            return step;
        }
        volume.sync().unwrap();
    }
    panic!("cleanup did not drain");
}

/// A small orphan's records go in the plan that finds it, with no cleanup
/// job; one larger than a drain step is still detached into a job. Both end
/// with no data owned and a volume that passes the full check.
#[test]
fn small_orphans_are_removed_directly_and_large_ones_detached() {
    let mut volume = opened();
    for (file, len) in [("small", 5_000), ("big", 3 << 20)] {
        let handle = volume.create_file(ROOT, name(file)).unwrap();
        let bytes = vec![7u8; len];
        let mut done = 0;
        while done < len {
            done += volume.write(&handle, done as u64, &bytes[done..]).unwrap();
        }
        volume.close_handle(handle).unwrap();
    }
    volume.sync().unwrap();
    volume
        .remove(ROOT, &name("small"), RemoveKind::File)
        .unwrap();
    volume.sync().unwrap();
    assert_eq!(cleanup_jobs(&mut volume), 0, "a small orphan became a job");
    volume.remove(ROOT, &name("big"), RemoveKind::File).unwrap();
    volume.sync().unwrap();
    assert_eq!(
        cleanup_jobs(&mut volume),
        1,
        "a large orphan was not detached"
    );
    drain_cleanup(&mut volume);
    assert_eq!(data_blocks(&volume), 0);
}

fn data_blocks(volume: &Volume<FaultDevice>) -> u64 {
    Volume::open(FaultDevice::new(durable(volume), MountMode::ReadOnly))
        .unwrap()
        .check()
        .unwrap()
        .data_blocks
}

/// An aligned 4 KiB overwrite replaces one block of its 64 KiB unit and keeps
/// the other fifteen, so it writes far less than a whole-unit overwrite does.
#[test]
fn an_aligned_small_overwrite_replaces_one_block() {
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("big.bin")).unwrap();
    let original: Vec<u8> = (0..4 * UNIT_BYTES as u32)
        .map(|index| (index.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    volume.write(&file, 0, &original).unwrap();
    volume.sync().unwrap();

    let overwrite = |volume: &mut Volume<FaultDevice>, offset: u64, len: usize| {
        let writes = |volume: &Volume<FaultDevice>| {
            volume
                .device
                .as_ref()
                .unwrap()
                .media
                .borrow()
                .events
                .iter()
                .filter(|event| matches!(event, Event::Write(_)))
                .count()
        };
        let before = writes(volume);
        volume.write(&file, offset, &vec![0xa5; len]).unwrap();
        volume.sync().unwrap();
        writes(volume) - before
    };
    let unit = UNIT_BYTES as u64;
    let small = overwrite(&mut volume, unit + 2 * BLOCK_BYTES as u64, BLOCK_BYTES);
    let whole = overwrite(&mut volume, 2 * unit, UNIT_BYTES);
    // Fifteen retained blocks, less whatever extra mapping the split costs.
    assert!(
        small + 10 < whole,
        "4 KiB overwrite wrote {small} blocks, 64 KiB wrote {whole}"
    );

    let mut readback = vec![0; original.len()];
    volume.read(&file, 0, &mut readback).unwrap();
    let mut expected = original.clone();
    let patched = (unit + 2 * BLOCK_BYTES as u64) as usize;
    expected[patched..patched + BLOCK_BYTES].fill(0xa5);
    expected[2 * UNIT_BYTES..3 * UNIT_BYTES].fill(0xa5);
    assert_eq!(readback, expected);
    volume.close_handle(file).unwrap();

    // The retained blocks stay reachable and verify after a reopen.
    let mut reopened =
        Volume::open(FaultDevice::new(durable(&volume), MountMode::ReadOnly)).unwrap();
    let mut findings = 0;
    let scrubbed = reopened.scrub(&mut |_| findings += 1).unwrap();
    assert_eq!(
        (findings, scrubbed.unreadable_ranges, scrubbed.data_blocks),
        (0, 0, 4 * (UNIT_BYTES / BLOCK_BYTES) as u64)
    );
}

#[test]
fn unlinked_open_files_stay_usable_then_reclaim_all_data() {
    let mut volume = opened();
    let writer = volume.create_file(ROOT, name("gone.bin")).unwrap();
    volume.write(&writer, 0, &vec![0x42; 300_000]).unwrap();
    let object = volume.lookup(ROOT, &name("gone.bin")).unwrap();
    let reader = volume
        .open_file(ROOT, &name("gone.bin"), Access::Read)
        .unwrap();
    assert!(matches!(
        volume.remove(ROOT, &name("gone.bin"), RemoveKind::Directory),
        Err(Error::WrongType)
    ));
    volume
        .remove(ROOT, &name("gone.bin"), RemoveKind::File)
        .unwrap();
    assert!(matches!(
        volume.lookup(ROOT, &name("gone.bin")),
        Err(Error::NotFound)
    ));
    assert!(matches!(
        volume.open_file(ROOT, &name("gone.bin"), Access::Read),
        Err(Error::NotFound)
    ));

    assert_eq!(read_all(&mut volume, &reader, 300_000), vec![0x42; 300_000]);
    volume.write(&writer, 299_990, &[0x43; 20]).unwrap();
    assert_eq!(volume.handle_attributes(&reader).unwrap().size, 300_010);
    checked(&volume);
    // While pinned, maintenance drains retired data but keeps the orphan marker.
    for _ in 0..64 {
        if cleanup_records(&mut volume) == 1 {
            break;
        }
        volume.sync().unwrap();
    }
    assert_eq!(cleanup_records(&mut volume), 1);
    volume.sync().unwrap();
    assert_eq!(cleanup_records(&mut volume), 1);
    assert_eq!(volume.attributes(object).unwrap().kind, ObjectKind::File);
    assert!(data_blocks(&volume) > 0);

    volume.close_handle(writer).unwrap();
    assert_eq!(read_all(&mut volume, &reader, 10)[..], [0x42; 10]);
    volume.close_handle(reader).unwrap();
    drain_cleanup(&mut volume);
    assert!(matches!(volume.attributes(object), Err(Error::NotFound)));
    assert_eq!(data_blocks(&volume), 0);
    checked(&volume);
}

#[test]
fn rename_replaces_a_file_and_keeps_the_open_destination_readable() {
    let mut volume = opened();
    for (file, byte) in [("a", 0xAA_u8), ("b", 0xBB)] {
        let handle = volume.create_file(ROOT, name(file)).unwrap();
        volume.write(&handle, 0, &vec![byte; 100_000]).unwrap();
        volume.close_handle(handle).unwrap();
    }
    let source = volume.lookup(ROOT, &name("a")).unwrap();
    let old = volume.open_file(ROOT, &name("b"), Access::Read).unwrap();
    volume.create_directory(ROOT, name("dir")).unwrap();
    assert!(matches!(
        volume.rename(ROOT, &name("a"), ROOT, name("dir"), Replace::Allowed),
        Err(Error::WrongType)
    ));
    volume
        .rename(ROOT, &name("a"), ROOT, name("b"), Replace::Allowed)
        .unwrap();
    assert_eq!(volume.lookup(ROOT, &name("b")).unwrap(), source);
    assert_eq!(read_all(&mut volume, &old, 100_000), vec![0xBB; 100_000]);
    let new = volume.open_file(ROOT, &name("b"), Access::Read).unwrap();
    assert_eq!(read_all(&mut volume, &new, 100_000), vec![0xAA; 100_000]);
    checked(&volume);

    let both = data_blocks(&volume);
    volume.close_handle(old).unwrap();
    drain_cleanup(&mut volume);
    assert_eq!(data_blocks(&volume), both / 2);
    assert_eq!(read_all(&mut volume, &new, 100_000), vec![0xAA; 100_000]);
    volume.close_handle(new).unwrap();
    checked(&volume);
}

#[test]
fn interrupted_file_unlink_recovers_old_or_new_namespace() {
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("victim.bin")).unwrap();
    volume.write(&file, 0, &vec![0x99; 200_000]).unwrap();
    volume.close_handle(file).unwrap();
    let base = volume.into_model_image();
    namespace_crash_matrix(
        &base,
        |volume| volume.remove(ROOT, &name("victim.bin"), RemoveKind::File),
        |volume| {
            let Ok(file) = volume.open_file(ROOT, &name("victim.bin"), Access::Read) else {
                return true;
            };
            assert_eq!(read_all(volume, &file, 200_000), vec![0x99; 200_000]);
            volume.close_handle(file).unwrap();
            false
        },
    );
}

struct RecoveryOutput {
    root: std::path::PathBuf,
    fail_create: bool,
    read_only_file: bool,
}

impl RecoveryOutput {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self {
            root: std::env::temp_dir().join(format!(
                "carryfs-recovery-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )),
            fail_create: false,
            read_only_file: false,
        }
    }
    fn path(&self, names: &[Name]) -> std::path::PathBuf {
        names.iter().fold(self.root.clone(), |path, name| {
            path.join(std::str::from_utf8(name.stored()).unwrap())
        })
    }
}
impl Drop for RecoveryOutput {
    fn drop(&mut self) {
        if self.root.is_dir() {
            std::fs::remove_dir_all(&self.root).unwrap();
        } else if self.root.exists() {
            std::fs::remove_file(&self.root).unwrap();
        }
    }
}
impl ExtractionSink for RecoveryOutput {
    fn create_directory(&mut self, path: &[Name], _: &Attributes) -> Result<()> {
        if self.fail_create {
            return Err(Error::Io {
                operation: "test sink",
                source: std::io::Error::other("destination failed"),
            });
        }
        std::fs::create_dir(self.path(path)).map_err(|e| Error::io("test mkdir", e))
    }
    fn create_file(&mut self, path: &[Name], _: &Attributes) -> Result<std::fs::File> {
        if self.fail_create {
            return Err(Error::Io {
                operation: "test sink",
                source: std::io::Error::other("destination failed"),
            });
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.path(path))
            .map_err(|e| Error::io("test create", e))?;
        if self.read_only_file {
            drop(file);
            std::fs::File::open(self.path(path)).map_err(|e| Error::io("test reopen", e))
        } else {
            Ok(file)
        }
    }
    fn create_symlink(&mut self, path: &[Name], target: &[u8], _: &Attributes) -> Result<()> {
        let target = std::str::from_utf8(target).unwrap();
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(target, self.path(path));
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(target, self.path(path));
        made.map_err(|e| Error::io("test symlink", e))
    }
}

#[test]
fn inspection_reports_bad_units_recovers_sparse_data_and_preserves_source() {
    use std::io::{Read, Seek, SeekFrom};
    let mut volume = opened();
    let far = 5 * 1024 * 1024 * 1024_u64;
    let (object, blocks) = backed_file(
        &mut volume,
        "data.bin",
        far + 7,
        &[
            (0, vec![0x11; 65_536]),
            (1, vec![0x22; 65_536]),
            (2, vec![0x33; 65_536]),
            (far / 65_536, vec![0x44; 7]),
        ],
    );
    let mut image = volume.into_model_image();
    image.get_mut(&blocks[0]).unwrap().0[100] ^= 1;
    image.get_mut(&blocks[1]).unwrap().0[200] ^= 1;
    let device = FaultDevice::new(image.clone(), MountMode::ReadOnly);
    let media = device.media.clone();
    media.borrow_mut().unreadable = Some(blocks[2]);
    let mut volume = Volume::open(device).unwrap();
    let mut ranges = Vec::new();
    let summary = volume
        .scrub(&mut |f| {
            assert_eq!(f.object, Some(object));
            ranges.push((f.offset.unwrap(), f.length));
            if f.offset == Some(2 * 65_536) {
                assert!(std::error::Error::source(f.error).is_some());
            }
        })
        .unwrap();
    assert_eq!(summary.inspected_files, 1);
    assert_eq!(summary.verified_units, 1);
    assert_eq!(summary.unreadable_ranges, 3);
    assert_eq!(
        ranges,
        vec![(0, 65_536), (65_536, 65_536), (2 * 65_536, 65_536)]
    );

    let mut output = RecoveryOutput::new();
    let mut findings = 0;
    let summary = volume
        .extract_to(object, &mut output, &mut |_| findings += 1)
        .unwrap();
    assert!(summary.complete);
    assert_eq!(summary.objects, 1);
    assert_eq!(summary.files, 1);
    assert_eq!(summary.verified_bytes, 7);
    assert_eq!(summary.unreadable_ranges, 3);
    assert_eq!(findings, 3);
    let mut recovered = std::fs::File::open(&output.root).unwrap();
    assert_eq!(recovered.metadata().unwrap().len(), far + 7);
    let mut start = vec![1; 3 * 65_536];
    recovered.read_exact(&mut start).unwrap();
    assert!(start.iter().all(|byte| *byte == 0));
    recovered.seek(SeekFrom::Start(far)).unwrap();
    let mut tail = [0; 7];
    recovered.read_exact(&mut tail).unwrap();
    assert_eq!(tail, [0x44; 7]);
    volume.eject().unwrap();
    assert!(media.borrow().events.is_empty());
    assert_eq!(media.borrow().durable, image);
}

#[test]
fn inspection_covers_orphans_and_skips_retired_data() {
    let mut volume = opened();
    let (object, blocks) = backed_file(&mut volume, "orphan", 65_536, &[(0, vec![0x51; 65_536])]);
    let pin = volume.retain_object(object).unwrap();
    volume
        .remove(ROOT, &name("orphan"), RemoveKind::File)
        .unwrap();
    // Leave the handle pinned until teardown so the orphan survives reopening.
    let _pin = pin;
    let mut image = volume.into_model_image();
    image.get_mut(&blocks[0]).unwrap().0[0] ^= 1;
    let mut volume = Volume::open(FaultDevice::new(image, MountMode::ReadOnly)).unwrap();
    let summary = volume
        .scrub(&mut |f| assert_eq!(f.object, Some(object)))
        .unwrap();
    assert_eq!(summary.inspected_files, 1);
    assert_eq!(summary.unreadable_ranges, 1);

    let mut volume = opened();
    let (_, blocks) = backed_file(&mut volume, "old", 65_536, &[(0, vec![0x62; 65_536])]);
    let file = volume
        .open_file(ROOT, &name("old"), Access::ReadWrite)
        .unwrap();
    volume.write(&file, 0, &vec![0x63; 65_536]).unwrap();
    volume.close_handle(file).unwrap();
    let mut image = volume.into_model_image();
    image.get_mut(&blocks[0]).unwrap().0[0] ^= 1;
    let mut volume = Volume::open(FaultDevice::new(image, MountMode::ReadOnly)).unwrap();
    let summary = volume
        .scrub(&mut |_| panic!("retired data must not be scrubbed"))
        .unwrap();
    assert_eq!(summary.verified_units, 1);
    assert_eq!(summary.unreadable_ranges, 0);
}

#[test]
fn inspection_rejects_writable_devices_and_reports_structural_failure_before_output() {
    let mut volume = opened();
    assert!(
        volume
            .check_with_report(&mut |_| panic!("precondition is not corruption"))
            .is_err()
    );
    assert!(
        volume
            .scrub(&mut |_| panic!("precondition is not corruption"))
            .is_err()
    );
    let mut output = RecoveryOutput::new();
    assert!(
        volume
            .extract_to(ROOT, &mut output, &mut |_| panic!("precondition"))
            .is_err()
    );
    assert!(!output.root.exists());
    let image = volume.into_model_image();
    let device = FaultDevice::new(image.clone(), MountMode::ReadOnly);
    let media = device.media.clone();
    let mut volume = Volume::open(device).unwrap();
    media.borrow_mut().unreadable = Some(volume.root.as_ref().unwrap().allocation.block);
    let mut count = 0;
    assert!(
        volume
            .check_with_report(&mut |f| {
                count += 1;
                assert!(f.offset.is_none());
            })
            .is_err()
    );
    assert_eq!(count, 1);
    let summary = volume
        .extract_to(ROOT, &mut output, &mut |_| count += 1)
        .unwrap();
    assert!(!summary.complete);
    assert_eq!(summary.objects, 0);
    assert!(!output.root.exists());
    assert_eq!(count, 2);
    assert_eq!(media.borrow().durable, image);
    assert!(media.borrow().events.is_empty());
}

#[test]
fn inspection_extracts_paged_directories_and_closes_handles_on_sink_failure() {
    let mut volume = opened();
    let dir = volume.create_directory(ROOT, name("dir")).unwrap();
    for index in 0..66 {
        let file = volume
            .create_file(dir, name(&format!("file{index:03}")))
            .unwrap();
        if index == 65 {
            volume.write(&file, 0, b"last").unwrap();
        }
        volume.close_handle(file).unwrap();
    }
    volume.create_directory(ROOT, name("empty")).unwrap();
    let image = volume.into_model_image();
    let mut volume = Volume::open(FaultDevice::new(image.clone(), MountMode::ReadOnly)).unwrap();
    let mut output = RecoveryOutput::new();
    let summary = volume
        .extract_to(ROOT, &mut output, &mut |_| panic!("valid tree"))
        .unwrap();
    assert!(summary.complete);
    assert_eq!(summary.objects, 69);
    assert_eq!(summary.files, 66);
    assert_eq!(
        std::fs::read(output.root.join("dir/file065")).unwrap(),
        b"last"
    );
    assert!(output.root.join("empty").is_dir());
    volume.eject().unwrap();

    let mut volume = Volume::open(FaultDevice::new(image, MountMode::ReadOnly)).unwrap();
    let mut output = RecoveryOutput::new();
    output.fail_create = true;
    assert!(
        volume
            .extract_to(ROOT, &mut output, &mut |_| panic!(
                "sink failure is not disk damage"
            ))
            .is_err()
    );
    volume.eject().unwrap();
}

#[test]
fn inspection_reports_incomplete_traversal_after_a_later_metadata_read_failure() {
    struct FailingSink {
        output: RecoveryOutput,
        media: std::rc::Rc<std::cell::RefCell<model::Media>>,
        block: u64,
    }
    impl ExtractionSink for FailingSink {
        fn create_directory(&mut self, path: &[Name], attributes: &Attributes) -> Result<()> {
            self.output.create_directory(path, attributes)?;
            self.media.borrow_mut().unreadable = Some(self.block);
            Ok(())
        }
        fn create_file(&mut self, _: &[Name], _: &Attributes) -> Result<std::fs::File> {
            panic!("enumeration must stop before files");
        }
        fn create_symlink(&mut self, _: &[Name], _: &[u8], _: &Attributes) -> Result<()> {
            panic!("enumeration must stop before symlinks");
        }
    }
    let volume = opened();
    let image = volume.into_model_image();
    for reuse in [false, true] {
        let device = FaultDevice::new(image.clone(), MountMode::ReadOnly);
        let media = device.media.clone();
        let mut volume = Volume::open(device).unwrap();
        if reuse {
            volume.enable_readonly_metadata_cache().unwrap();
        }
        let mut sink = FailingSink {
            output: RecoveryOutput::new(),
            media: media.clone(),
            block: volume.root.as_ref().unwrap().catalog.block,
        };
        let mut findings = 0;
        let summary = volume
            .extract_to(ROOT, &mut sink, &mut |f| {
                findings += 1;
                assert!(f.offset.is_none());
                assert!(std::error::Error::source(f.error).is_some());
            })
            .unwrap();
        assert_eq!(findings, 1);
        assert!(!summary.complete);
        assert_eq!(summary.objects, 1);
        volume.eject().unwrap();
        assert_eq!(media.borrow().durable, image);
        assert!(media.borrow().events.is_empty());
    }
}

#[test]
fn inspection_returns_destination_io_errors_without_reporting_source_damage() {
    let mut volume = opened();
    let handle = volume.create_file(ROOT, name("data")).unwrap();
    volume.write(&handle, 0, b"verified data").unwrap();
    volume.close_handle(handle).unwrap();
    let mut volume = Volume::open(FaultDevice::new(
        volume.into_model_image(),
        MountMode::ReadOnly,
    ))
    .unwrap();
    let mut sink = RecoveryOutput::new();
    sink.read_only_file = true;
    let error = volume
        .extract_to(ROOT, &mut sink, &mut |_| {
            panic!("destination I/O is not source damage")
        })
        .unwrap_err();
    assert!(matches!(
        error,
        Error::Io {
            operation: "size extraction output",
            ..
        }
    ));
    volume.eject().unwrap();
}

#[test]
fn regression_corruption_must_fault_mutations() {
    for detect_by_write in [false, true] {
        let mut volume = opened();
        let (_, firsts) = backed_file(
            &mut volume,
            "bad",
            131072,
            &[(0, vec![7; 65536]), (1, vec![9; 65536])],
        );
        let file = volume
            .open_file(ROOT, &name("bad"), Access::ReadWrite)
            .unwrap();
        let media = volume.device.as_ref().unwrap().media.clone();
        media.borrow_mut().durable.get_mut(&firsts[0]).unwrap().0[17] ^= 1;
        let failure = if detect_by_write {
            volume.write(&file, 17, b"x")
        } else {
            volume.read(&file, 0, &mut [0; 1])
        };
        assert!(matches!(failure, Err(Error::Corruption { .. })));
        let events = media.borrow().events.len();
        assert!(matches!(
            volume.write(&file, 65536, &vec![5; 65536]),
            Err(Error::Faulted)
        ));
        assert!(matches!(volume.write(&file, 0, b""), Err(Error::Faulted)));
        assert!(matches!(volume.truncate(&file, 0), Err(Error::Faulted)));
        assert!(matches!(
            volume.set_times(&file, None, None),
            Err(Error::Faulted)
        ));
        assert!(matches!(
            volume.create_file(ROOT, name("new")),
            Err(Error::Faulted)
        ));
        assert!(matches!(
            volume.remove(ROOT, &name("bad"), RemoveKind::File),
            Err(Error::Faulted)
        ));
        assert!(matches!(
            volume.rename(ROOT, &name("bad"), ROOT, name("bad"), Replace::Allowed),
            Err(Error::Faulted)
        ));
        assert!(matches!(volume.sync(), Err(Error::Faulted)));
        let mut good = [0; 1];
        assert_eq!(volume.read(&file, 65536, &mut good).unwrap(), 1);
        assert_eq!(good, [9]);
        volume.close_handle(file).unwrap();
        let reader = volume.open_file(ROOT, &name("bad"), Access::Read).unwrap();
        assert_eq!(volume.read(&reader, 65536, &mut good).unwrap(), 1);
        volume.close_handle(reader).unwrap();
        assert!(matches!(volume.eject(), Err(Error::Faulted)));
        assert_eq!(media.borrow().events.len(), events);
    }
}

#[test]
fn regression_identical_rename_validates_source_and_state() {
    let mut volume = opened();
    let missing = volume.rename(
        ROOT,
        &name("missing"),
        ROOT,
        name("missing"),
        Replace::Allowed,
    );
    volume.eject().unwrap();
    let ejected = volume.rename(
        ROOT,
        &name("missing"),
        ROOT,
        name("missing"),
        Replace::Allowed,
    );
    let mut read_only = Volume::open(FaultDevice::new(formatted(), MountMode::ReadOnly)).unwrap();
    let readonly = read_only.rename(
        ROOT,
        &name("missing"),
        ROOT,
        name("missing"),
        Replace::Allowed,
    );
    assert!(matches!(missing, Err(Error::NotFound)));
    assert!(matches!(ejected, Err(Error::Ejected)));
    assert!(matches!(readonly, Err(Error::ReadOnly)));
}

#[test]
fn lookup_entry_reports_the_stored_spelling_and_kind() {
    let mut volume = opened();
    let object = volume.create_directory(ROOT, name("Docs")).unwrap();
    let file = volume.create_file(object, name("Readme.TXT")).unwrap();
    volume.close_handle(file).unwrap();

    let entry = volume.lookup_entry(ROOT, &name("DOCS")).unwrap();
    assert_eq!(entry.name.stored(), b"Docs");
    assert_eq!((entry.object, entry.kind), (object, ObjectKind::Directory));

    let entry = volume.lookup_entry(object, &name("readme.txt")).unwrap();
    assert_eq!(entry.name.stored(), b"Readme.TXT");
    assert_eq!(entry.kind, ObjectKind::File);
    assert_eq!(
        entry.object,
        volume.lookup(object, &name("README.txt")).unwrap()
    );

    assert!(matches!(
        volume.lookup_entry(ROOT, &name("missing")),
        Err(Error::NotFound)
    ));
}

#[test]
fn regression_rename_alias_preserves_requested_spelling() {
    let mut volume = opened();
    volume.create_directory(ROOT, name("child")).unwrap();
    volume
        .rename(ROOT, &name("CHILD"), ROOT, name("CHILD"), Replace::Allowed)
        .unwrap();
    let directory = volume.open_directory(ROOT).unwrap();
    let entries = volume.read_directory(&directory, None).unwrap().entries;
    let actual = entries[0].name.stored();
    assert_eq!(actual, b"CHILD");
}

#[test]
fn regression_case_rename_updates_parent_times() {
    let mut volume = opened();
    let object = volume.create_directory(ROOT, name("child")).unwrap();
    let parent = volume.open_directory(ROOT).unwrap();
    volume
        .set_times(&parent, None, Some(Timestamp::new(1, 0).unwrap()))
        .unwrap();
    let before = volume.attributes(ROOT).unwrap();
    volume
        .rename(
            ROOT,
            &name("child"),
            ROOT,
            name("CHILD"),
            Replace::Forbidden,
        )
        .unwrap();
    let after = volume.attributes(ROOT).unwrap();
    let child = volume.attributes(object).unwrap();
    assert_eq!(volume.lookup(ROOT, &name("CHILD")).unwrap(), object);
    checked(&volume);
    assert_ne!(after.modification, before.modification);
    assert_eq!(after.modification, child.metadata_change);
    assert_eq!(after.metadata_change, child.metadata_change);
}

fn replace_disk_spelling(volume: &mut Volume<FaultDevice>, spelling: &[u8]) {
    // The damage appears behind verified metadata; only an uncached read sees it.
    volume.cache = crate::tree::Cache::default();
    let root = volume.root.as_ref().unwrap();
    let mut media = volume.device.as_ref().unwrap().media.borrow_mut();
    let decoded = decode_page(
        &media.durable[&root.catalog.block].0[..],
        root.catalog,
        &root.uuid,
        TreeKind::Catalog,
        root.generation,
        Some(0),
        root.blocks,
    )
    .unwrap();
    let mut records: Vec<_> = decoded
        .records
        .iter()
        .map(|(key, value)| (key.to_vec(), value.to_vec()))
        .collect();
    let entry = records.iter_mut().find(|(key, _)| key[8] == 2).unwrap();
    entry.1[12..].copy_from_slice(spelling);
    let records: Vec<_> = records
        .iter()
        .map(|(key, value)| (key.as_slice(), value.as_slice()))
        .collect();
    media.durable.insert(
        root.catalog.block,
        encode_leaf(
            root.catalog,
            &root.uuid,
            TreeKind::Catalog,
            &records,
            root.blocks,
        )
        .unwrap(),
    );
}

/// Point directory `a`'s parent at its child `b`, rewriting the catalog leaf
/// with a valid checksum, so the parent chain of `b` loops.
#[test]
fn check_rejects_a_directory_parent_cycle() {
    let mut volume = opened();
    let a = volume.create_directory(ROOT, name("a")).unwrap();
    let b = volume.create_directory(a, name("b")).unwrap();
    volume.sync().unwrap();
    let root = volume.root.as_ref().unwrap();
    let mut media = volume.device.as_ref().unwrap().media.borrow_mut();
    let decoded = decode_page(
        &media.durable[&root.catalog.block].0[..],
        root.catalog,
        &root.uuid,
        TreeKind::Catalog,
        root.generation,
        Some(0),
        root.blocks,
    )
    .unwrap();
    let mut records: Vec<_> = decoded
        .records
        .iter()
        .map(|(key, value)| (key.to_vec(), value.to_vec()))
        .collect();
    let inode = records
        .iter_mut()
        .find(|(key, _)| key[..8] == a.0.to_be_bytes() && key[8] == 1)
        .unwrap();
    inode.1[8..16].copy_from_slice(&b.0.to_le_bytes());
    // Move a's directory entry from the root into b, keeping keys sorted, so
    // the entry and the parent field agree and only the loop is wrong.
    let entry = records
        .iter_mut()
        .find(|(key, value)| {
            key[..8] == ROOT.0.to_be_bytes() && key[8] == 2 && value[..8] == a.0.to_le_bytes()
        })
        .unwrap();
    entry.0[..8].copy_from_slice(&b.0.to_be_bytes());
    // The root loses its only child and b gains a.
    for (owner, count) in [(ROOT, 0u64), (b, 1)] {
        let inode = records
            .iter_mut()
            .find(|(key, _)| key[..8] == owner.0.to_be_bytes() && key[8] == 1)
            .unwrap();
        inode.1[32..40].copy_from_slice(&count.to_le_bytes());
    }
    records.sort();
    let records: Vec<_> = records
        .iter()
        .map(|(key, value)| (key.as_slice(), value.as_slice()))
        .collect();
    media.durable.insert(
        root.catalog.block,
        encode_leaf(
            root.catalog,
            &root.uuid,
            TreeKind::Catalog,
            &records,
            root.blocks,
        )
        .unwrap(),
    );
    drop(media);
    let result = Volume::open(FaultDevice::new(durable(&volume), MountMode::ReadOnly))
        .unwrap()
        .check();
    assert!(
        matches!(
            result,
            Err(Error::Corruption {
                reason: "directory parent cycle",
                ..
            })
        ),
        "{result:?}"
    );
}

#[test]
fn regression_enumeration_rejects_invalid_disk_names() {
    // Both spellings fit the envelope and retain its CRC. The second is a
    // portable name whose comparison key disagrees with the stored "ok" key.
    for spelling in [b"..", b"no"] {
        for mode in [MountMode::ReadOnly, MountMode::ReadWrite] {
            let mut volume = opened();
            volume.create_directory(ROOT, name("ok")).unwrap();
            let mut volume = Volume::open(FaultDevice::new(durable(&volume), mode)).unwrap();
            let directory = volume.open_directory(ROOT).unwrap();
            replace_disk_spelling(&mut volume, spelling);
            let media = volume.device.as_ref().unwrap().media.clone();
            assert!(matches!(
                volume.read_directory(&directory, None),
                Err(Error::Corrupt(_))
            ));
            if mode == MountMode::ReadWrite {
                assert!(matches!(
                    volume.create_file(ROOT, name("new")),
                    Err(Error::Faulted)
                ));
                assert!(matches!(volume.sync(), Err(Error::Faulted)));
            }
            volume.attributes(ROOT).unwrap();
            volume.close_handle(directory).unwrap();
            assert!(media.borrow().events.is_empty());
        }
    }
}

#[test]
fn regression_valid_enumerated_names_transfer_between_volumes() {
    let mut source = opened();
    for spelling in ["Résumé.TXT", "Straße"] {
        let file = source.create_file(ROOT, name(spelling)).unwrap();
        source.close_handle(file).unwrap();
    }
    let mut source = Volume::open(FaultDevice::new(durable(&source), MountMode::ReadOnly)).unwrap();
    let directory = source.open_directory(ROOT).unwrap();
    let mut destination = opened();
    for entry in source.read_directory(&directory, None).unwrap().entries {
        let spelling = entry.name.clone();
        let file = destination.create_file(ROOT, entry.name).unwrap();
        destination.lookup(ROOT, &spelling).unwrap();
        destination.close_handle(file).unwrap();
    }
    checked(&destination);
}

#[test]
fn writable_volumes_reuse_verified_metadata_and_inspection_rereads_it() {
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("a")).unwrap();
    volume.close_handle(file).unwrap();
    let image = durable(&volume);
    for mode in [MountMode::ReadWrite, MountMode::ReadOnly] {
        let mut volume = Volume::open(FaultDevice::new(image.clone(), mode)).unwrap();
        let object = volume.lookup(ROOT, &name("a")).unwrap();
        let root = volume.root.clone().unwrap();
        let media = volume.device.as_ref().unwrap().media.clone();
        media.borrow_mut().unreadable = Some(root.catalog.block);
        let again = volume.lookup(ROOT, &name("a"));
        if mode == MountMode::ReadOnly {
            assert!(again.is_err());
            continue;
        }
        assert_eq!(again.unwrap(), object);
        media.borrow_mut().unreadable = None;
        // After one edit, the next reads the published workspace bank from
        // memory, and later reads see the pages it published.
        volume.create_directory(ROOT, name("b")).unwrap();
        let workspace = volume.root.as_ref().unwrap().workspace.block;
        media.borrow_mut().unreadable = Some(workspace);
        let file = volume.create_file(ROOT, name("c")).unwrap();
        volume.close_handle(file).unwrap();
        media.borrow_mut().unreadable = None;
        assert_ne!(volume.lookup(ROOT, &name("c")).unwrap(), object);
        checked(&volume);
    }
}

/// Reads shorter than a unit return the right bytes when single blocks of
/// that unit were overwritten after it was written: a block that moved, one
/// that stayed, and ranges that cross from one to the other.
#[test]
fn short_reads_in_a_partly_overwritten_unit_return_its_bytes() {
    let mut writer = opened();
    let file = writer.create_file(ROOT, name("split")).unwrap();
    let mut expected: Vec<u8> = (0..UNIT_BYTES)
        .map(|index| (index % 251) as u8 + 1)
        .collect();
    writer.write(&file, 0, &expected).unwrap();
    for block in [3, 7, 11] {
        let at = block * BLOCK_BYTES;
        writer
            .write(&file, at as u64, &[0xA5; BLOCK_BYTES])
            .unwrap();
        expected[at..at + BLOCK_BYTES].fill(0xA5);
    }
    writer.close_handle(file).unwrap();
    let mut volume = Volume::open(FaultDevice::new(durable(&writer), MountMode::ReadOnly)).unwrap();
    let file = volume
        .open_file(ROOT, &name("split"), Access::Read)
        .unwrap();
    for (offset, len) in [
        (0, BLOCK_BYTES),
        (3 * BLOCK_BYTES, BLOCK_BYTES),
        (3 * BLOCK_BYTES - 100, 200),
        (7 * BLOCK_BYTES - 1, BLOCK_BYTES + 2),
        (10 * BLOCK_BYTES, 3 * BLOCK_BYTES),
        (UNIT_BYTES - 5, 5),
    ] {
        let mut out = vec![0; len];
        assert_eq!(volume.read(&file, offset as u64, &mut out).unwrap(), len);
        assert_eq!(
            out,
            expected[offset..offset + len],
            "{len} bytes at {offset}"
        );
    }
}

#[test]
fn readonly_metadata_cache_reuses_calls_but_inspection_reads_media_fresh() {
    let mut writer = opened();
    let directory = writer.create_directory(ROOT, name("cached")).unwrap();
    let file = writer.create_file(directory, name("data")).unwrap();
    writer.write(&file, 0, b"verified").unwrap();
    writer.close_handle(file).unwrap();
    let image = durable(&writer);
    let device = FaultDevice::new(image, MountMode::ReadOnly);
    let media = device.media.clone();
    let mut volume = Volume::open(device).unwrap();
    assert!(matches!(
        writer.enable_readonly_metadata_cache(),
        Err(Error::InvalidInput(_))
    ));
    volume.enable_readonly_metadata_cache().unwrap();
    let directory = volume.lookup(ROOT, &name("cached")).unwrap();
    let file = volume.lookup(directory, &name("data")).unwrap();
    volume.attributes(file).unwrap();
    media.borrow_mut().reads = 0;
    assert_eq!(volume.lookup(directory, &name("data")).unwrap(), file);
    volume.attributes(file).unwrap();
    assert_eq!(media.borrow().reads, 0);

    assert!(volume.check().is_ok());
    assert!(media.borrow().reads > 0);
    let catalog = volume.root.as_ref().unwrap().catalog.block;
    media.borrow_mut().durable.get_mut(&catalog).unwrap().0[100] ^= 1;
    assert!(volume.attributes(ROOT).is_ok());
    assert!(volume.check().is_err());
    assert!(volume.scrub(&mut |_| {}).is_err());
    let mut destination = RecoveryOutput::new();
    let mut findings = 0;
    let summary = volume
        .extract_to(ROOT, &mut destination, &mut |_| findings += 1)
        .unwrap();
    assert!(!summary.complete);
    assert_eq!(findings, 1);
    assert!(!destination.root.exists());
    assert!(media.borrow().events.is_empty());
}

#[test]
fn regression_metadata_corruption_faults_reads_edits_and_maintenance() {
    for operation in 0..3 {
        let mut volume = opened();
        let file = volume.create_file(ROOT, name("good")).unwrap();
        volume.write(&file, 0, b"verified").unwrap();
        let root = volume.root.as_ref().unwrap();
        let block = if operation == 0 {
            root.catalog.block
        } else {
            root.workspace.block
        };
        let media = volume.device.as_ref().unwrap().media.clone();
        media.borrow_mut().events.clear();
        media.borrow_mut().durable.get_mut(&block).unwrap().0[100] ^= 1;
        // Damage that appears behind already-verified metadata is found on the next uncached read.
        volume.cache = crate::tree::Cache::default();
        let result = match operation {
            0 => volume.attributes(ROOT).map(|_| ()),
            1 => volume.create_directory(ROOT, name("new")).map(|_| ()),
            _ => volume.sync(),
        };
        if operation == 0 {
            assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
        } else {
            assert!(
                matches!(result, Err(Error::Corruption { .. })),
                "{result:?}"
            );
        }
        // Restoring the injected byte makes this data readable. It must not
        // clear the instance's mutation fault.
        media.borrow_mut().durable.get_mut(&block).unwrap().0[100] ^= 1;
        let mut out = [0; 8];
        assert_eq!(volume.read(&file, 0, &mut out).unwrap(), out.len());
        assert_eq!(&out, b"verified");
        assert!(matches!(
            volume.write(&file, 0, b"changed"),
            Err(Error::Faulted)
        ));
        assert!(matches!(volume.sync(), Err(Error::Faulted)));
        volume.close_handle(file).unwrap();
        assert!(media.borrow().events.is_empty());
    }
}

#[test]
fn regression_rename_noop_preserves_generation_epoch_and_attributes() {
    let mut volume = opened();
    let child = volume.create_directory(ROOT, name("child")).unwrap();
    let parent_before = volume.attributes(ROOT).unwrap();
    let child_before = volume.attributes(child).unwrap();
    let info = volume.info().unwrap();
    let epoch = volume.namespace_epoch;
    let media = volume.device.as_ref().unwrap().media.clone();
    media.borrow_mut().events.clear();
    for source in ["child", "CHILD"] {
        volume
            .rename(ROOT, &name(source), ROOT, name("child"), Replace::Forbidden)
            .unwrap();
        assert_eq!(volume.info().unwrap(), info);
        assert_eq!(volume.namespace_epoch, epoch);
        assert_eq!(volume.attributes(ROOT).unwrap(), parent_before);
        assert_eq!(volume.attributes(child).unwrap(), child_before);
        assert!(media.borrow().events.is_empty());
    }
    replace_disk_spelling(&mut volume, b"other");
    assert!(matches!(
        volume.rename(ROOT, &name("child"), ROOT, name("child"), Replace::Allowed),
        Err(Error::Corrupt(_))
    ));
    assert!(matches!(volume.sync(), Err(Error::Faulted)));
    assert!(media.borrow().events.is_empty());
}

#[test]
fn regression_case_rename_recovers_spelling_and_timestamps_together() {
    let mut volume = opened();
    let object = volume.create_directory(ROOT, name("child")).unwrap();
    // Fix both objects' timestamps so the old-state assertions need no clock
    // delay or timing tolerance.
    volume
        .edit(|transaction| {
            for object in [ROOT, object] {
                let key = namespace::inode_key(object.0);
                let value = transaction
                    .forest
                    .lookup(&transaction.catalog, TreeKind::Catalog, &key)?
                    .unwrap();
                let mut inode = namespace::Inode::decode(&value)?;
                inode.creation = Timestamp::new(1, 0)?;
                inode.modification = Timestamp::new(2, 0)?;
                inode.metadata_change = Timestamp::new(3, 0)?;
                transaction.catalog = transaction.forest.put(
                    transaction.catalog.clone(),
                    TreeKind::Catalog,
                    &key,
                    &inode.encode(),
                )?;
            }
            Ok(())
        })
        .unwrap();
    let base = durable(&volume);
    namespace_crash_matrix(
        &base,
        |volume| {
            volume.rename(
                ROOT,
                &name("CHILD"),
                ROOT,
                name("CHILD"),
                Replace::Forbidden,
            )
        },
        |volume| {
            let directory = volume.open_directory(ROOT).unwrap();
            let entry = volume
                .read_directory(&directory, None)
                .unwrap()
                .entries
                .remove(0);
            volume.close_handle(directory).unwrap();
            let parent = volume.attributes(ROOT).unwrap();
            let child = volume.attributes(entry.object).unwrap();
            let renamed = entry.name.stored() == b"CHILD";
            assert_eq!(parent.creation, Timestamp::new(1, 0).unwrap());
            assert_eq!(child.creation, Timestamp::new(1, 0).unwrap());
            assert_eq!(child.modification, Timestamp::new(2, 0).unwrap());
            if renamed {
                assert_eq!(parent.modification, child.metadata_change);
                assert_eq!(parent.metadata_change, child.metadata_change);
            } else {
                assert_eq!(entry.name.stored(), b"child");
                assert_eq!(parent.modification, Timestamp::new(2, 0).unwrap());
                assert_eq!(parent.metadata_change, Timestamp::new(3, 0).unwrap());
                assert_eq!(child.metadata_change, Timestamp::new(3, 0).unwrap());
            }
            renamed
        },
    );
}

fn raw_free(volume: &mut Volume<FaultDevice>) -> u64 {
    let root = volume.root.as_ref().unwrap();
    root.free_blocks
        - crate::workspace::read(volume.device.as_mut().unwrap(), root)
            .unwrap()
            .len() as u64
}

#[test]
fn orphan_scan_reclaims_closed_objects_beyond_pinned_windows() {
    for count in [32, 65] {
        let mut volume = opened();
        let mut pins = Vec::new();
        for index in 0..count {
            let spelling = name(&format!("pin{index}"));
            let file = volume.create_file(ROOT, spelling.clone()).unwrap();
            volume.write(&file, 0, &[index as u8]).unwrap();
            volume.remove(ROOT, &spelling, RemoveKind::File).unwrap();
            pins.push(file);
        }
        let file = volume.create_file(ROOT, name("closed")).unwrap();
        let object = volume.handles.validate(&file).unwrap();
        volume.write(&file, 0, b"reclaim").unwrap();
        volume
            .remove(ROOT, &name("closed"), RemoveKind::File)
            .unwrap();
        volume.close_handle(file).unwrap();
        for _ in 0..64 {
            volume.sync().unwrap();
        }
        assert!(matches!(volume.attributes(object), Err(Error::NotFound)));
        for (index, file) in pins.iter().enumerate() {
            let mut out = [0];
            assert_eq!(volume.read(file, 0, &mut out).unwrap(), 1);
            assert_eq!(out, [index as u8]);
        }
        for file in pins {
            volume.close_handle(file).unwrap();
        }
        for _ in 0..256 {
            volume.sync().unwrap();
        }
        let mut reader =
            Volume::open(FaultDevice::new(durable(&volume), MountMode::ReadOnly)).unwrap();
        let summary = reader.check().unwrap();
        assert_eq!(summary.objects, 1);
        assert_eq!(summary.data_blocks, 0);
    }
}

#[test]
fn orphan_cursor_wraps_without_publication_and_resets_on_reopen() {
    let mut volume = opened();
    let mut pins = Vec::new();
    for index in 0..65 {
        let spelling = name(&format!("pin{index}"));
        let file = volume.create_file(ROOT, spelling.clone()).unwrap();
        volume.remove(ROOT, &spelling, RemoveKind::File).unwrap();
        pins.push(file);
    }
    let first = volume.handles.validate(&pins[0]).unwrap();
    let generation = volume.info().unwrap().generation;
    // One step scans every pinned window and wraps without a publication.
    volume.sync().unwrap();
    assert_eq!(volume.orphan_cursor, 0);
    assert_eq!(volume.info().unwrap().generation, generation);
    volume.close_handle(pins.remove(0)).unwrap();
    for _ in 0..3 {
        volume.sync().unwrap();
    }
    assert!(matches!(volume.attributes(first), Err(Error::NotFound)));
    let cursor = volume.orphan_cursor;
    // Detached intervals go first; spare budget in the same plan takes further
    // markers, so the cursor may pass them. Pinned objects are never taken.
    volume.sync().unwrap();
    assert!(volume.orphan_cursor >= cursor);
    for file in &pins {
        assert!(volume.handle_attributes(file).unwrap().orphan);
    }
    let mut reopened =
        Volume::open(FaultDevice::new(durable(&volume), MountMode::ReadWrite)).unwrap();
    assert_eq!(reopened.orphan_cursor, 0);
    for _ in 0..256 {
        reopened.sync().unwrap();
    }
    let summary = Volume::open(FaultDevice::new(durable(&reopened), MountMode::ReadOnly))
        .unwrap()
        .check()
        .unwrap();
    assert_eq!(summary.objects, 1);

    // A failed publication cannot adopt the planner's proposed cursor.
    let last = pins.pop().unwrap();
    volume.orphan_cursor = volume.handles.validate(&last).unwrap().0;
    let cursor = volume.orphan_cursor;
    volume.close_handle(last).unwrap();
    let media = volume.device.as_ref().unwrap().media.clone();
    media.borrow_mut().events.clear();
    media.borrow_mut().failure = Some(Failure {
        event: 0,
        whole_mask: 0,
        tear: None,
    });
    assert!(matches!(volume.sync(), Err(Error::Io { .. })));
    assert_eq!(volume.orphan_cursor, cursor);
    assert!(matches!(volume.sync(), Err(Error::Faulted)));
}

#[test]
fn root_magic_identifies_formatted_and_damaged_volumes_only() {
    let path = std::env::temp_dir().join(format!("carryfs-root-magic-{}.img", std::process::id()));
    let open = |mode| crate::ImageDevice::open(&path, mode).unwrap();
    crate::ImageDevice::create_new(&path, 256 << 20).unwrap();
    assert!(!crate::has_root_magic(&mut open(MountMode::ReadOnly)));
    drop(Volume::format(open(MountMode::ReadWrite)).unwrap());
    assert!(crate::has_root_magic(&mut open(MountMode::ReadOnly)));
    // Either slot identifies a volume whose other root is damaged.
    let blocks = std::fs::metadata(&path).unwrap().len() / BLOCK_BYTES as u64;
    let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    for block in [blocks - 257, ROOT_A] {
        use std::io::{Seek, SeekFrom, Write};
        assert!(crate::has_root_magic(&mut open(MountMode::ReadOnly)));
        file.seek(SeekFrom::Start(block * BLOCK_BYTES as u64))
            .unwrap();
        file.write_all(b"damaged!").unwrap();
    }
    assert!(!crate::has_root_magic(&mut open(MountMode::ReadOnly)));
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn unchanged_truncate_does_not_publish() {
    let mut volume = opened();
    let file = volume.create_file(ROOT, name("file")).unwrap();
    volume.write(&file, 0, b"payload").unwrap();
    let before = volume.handle_attributes(&file).unwrap();
    let generation = volume.info().unwrap().generation;
    let media = volume.device.as_ref().unwrap().media.clone();
    media.borrow_mut().events.clear();
    volume.truncate(&file, 7).unwrap();
    assert!(media.borrow().events.is_empty());
    assert_eq!(volume.info().unwrap().generation, generation);
    let after = volume.handle_attributes(&file).unwrap();
    assert_eq!(after.modification, before.modification);
    assert_eq!(after.metadata_change, before.metadata_change);
    // Access and type are still checked.
    let reader = volume.open_file(ROOT, &name("file"), Access::Read).unwrap();
    assert!(matches!(volume.truncate(&reader, 7), Err(Error::ReadOnly)));
    let directory = volume.create_directory(ROOT, name("dir")).unwrap();
    let directory = volume.open_object(directory, Access::Read).unwrap();
    assert!(matches!(
        volume.truncate(&directory, 0),
        Err(Error::ReadOnly | Error::WrongType)
    ));
}

#[test]
fn cleanup_query_matches_sync_and_issues_no_output() {
    let mut volume = opened();
    assert!(!volume.has_cleanup_work().unwrap());
    // An overwrite retires the old unit; a pinned orphan is not work.
    let file = volume.create_file(ROOT, name("file")).unwrap();
    volume.write(&file, 0, b"one").unwrap();
    volume.write(&file, 0, b"two").unwrap();
    volume
        .remove(ROOT, &name("file"), RemoveKind::File)
        .unwrap();
    assert!(volume.has_cleanup_work().unwrap());
    let media = volume.device.as_ref().unwrap().media.clone();
    for _ in 0..16 {
        media.borrow_mut().events.clear();
        let work = volume.has_cleanup_work().unwrap();
        assert!(media.borrow().events.is_empty());
        if !work {
            break;
        }
        let generation = volume.info().unwrap().generation;
        volume.sync().unwrap();
        assert!(volume.info().unwrap().generation > generation);
    }
    assert!(!volume.has_cleanup_work().unwrap());
    let object = volume.handles.validate(&file).unwrap();
    assert!(volume.attributes(object).unwrap().orphan);
    // Closing the last pin makes the orphan work, which sync then removes.
    volume.close_handle(file).unwrap();
    assert!(volume.has_cleanup_work().unwrap());
    while volume.has_cleanup_work().unwrap() {
        volume.sync().unwrap();
    }
    assert!(matches!(volume.attributes(object), Err(Error::NotFound)));

    let mut reader = Volume::open(FaultDevice::new(durable(&volume), MountMode::ReadOnly)).unwrap();
    assert!(!reader.has_cleanup_work().unwrap());
    assert_eq!(reader.check().unwrap().objects, 1);
}

#[test]
fn regression_maximum_write_charges_288_blocks_before_output() {
    const CHARGE: u64 = 288;
    const OFFSET: u64 = 3 * 65_536 - 1;
    let bytes = vec![6; 1 << 20];
    for remaining in [CHARGE - 1, CHARGE] {
        let mut volume = opened();
        let file = volume.create_file(ROOT, name("unaligned")).unwrap();
        volume.write(&file, 0, b"a").unwrap();
        // Reserve other raw blocks as retirement, without writing their data.
        // This is a structurally valid near-full fixture; foreground edits
        // cannot reuse the retirement until a later maintenance publication.
        let fill = raw_free(&mut volume) - crate::allocation::RESERVE - remaining;
        volume
            .edit(|transaction| {
                for (start, count) in transaction.allocate_data(fill, 0)? {
                    transaction.retire_data(start, count);
                }
                Ok(())
            })
            .unwrap();
        checked(&volume);
        let before = raw_free(&mut volume);
        let info = volume.info().unwrap();
        let attributes = volume.handle_attributes(&file).unwrap();
        let media = volume.device.as_ref().unwrap().media.clone();
        media.borrow_mut().events.clear();
        let result = volume.write(&file, OFFSET, &bytes);
        if remaining < CHARGE {
            assert!(matches!(result, Err(Error::NoSpace)));
            assert_eq!(volume.info().unwrap(), info);
            assert_eq!(volume.handle_attributes(&file).unwrap(), attributes);
            assert_eq!(raw_free(&mut volume), before);
            assert!(media.borrow().events.is_empty());
            // A capacity failure leaves the instance available for a smaller
            // valid edit. Request errors also must not latch corruption.
            assert!(matches!(
                volume.write(&file, i64::MAX as u64, b"x"),
                Err(Error::LimitExceeded)
            ));
            assert!(matches!(
                volume.create_directory(ROOT, name("unaligned")),
                Err(Error::NameCollision)
            ));
            volume
                .create_directory(ROOT, name("still-writable"))
                .unwrap();
            assert_eq!(read_all(&mut volume, &file, 1), b"a");
        } else {
            assert_eq!(result.unwrap(), bytes.len());
            assert_eq!(before - raw_free(&mut volume), CHARGE);
            assert_eq!(raw_free(&mut volume), crate::allocation::RESERVE);
            let mut expected = vec![0; OFFSET as usize + bytes.len()];
            expected[0] = b'a';
            expected[OFFSET as usize..].copy_from_slice(&bytes);
            assert_eq!(read_all(&mut volume, &file, expected.len()), expected);
            assert_eq!(
                volume.handle_attributes(&file).unwrap().allocated_bytes,
                CHARGE * 4096
            );
        }
        checked(&volume);
    }
}

#[test]
fn read_directory_after_resumes_across_namespace_changes() {
    let mut volume = opened();
    for index in 0..130 {
        volume
            .create_directory(ROOT, name(&format!("d{index:03}")))
            .unwrap();
    }
    let handle = volume.open_directory(ROOT).unwrap();
    let mut seen = Vec::new();
    let mut page = volume.read_directory(&handle, None).unwrap();
    // Remove every entry as it is listed, like `rm -rf`, and add one late name.
    loop {
        for entry in &page.entries {
            seen.push(String::from_utf8(entry.name.stored().to_vec()).unwrap());
            volume
                .remove(ROOT, &entry.name, RemoveKind::Directory)
                .unwrap();
        }
        let Some(last) = page.entries.last().map(|entry| entry.name.clone()) else {
            break;
        };
        if seen.len() == 63 {
            volume.create_directory(ROOT, name("zzz")).unwrap();
        }
        page = volume.read_directory_after(&handle, &last).unwrap();
    }
    let expected: Vec<_> = (0..130)
        .map(|index| format!("d{index:03}"))
        .chain(["zzz".to_string()])
        .collect();
    assert_eq!(seen, expected);
    volume.close_handle(handle).unwrap();
    checked(&volume);
}

#[test]
fn open_object_reaches_pinned_orphans_and_rejects_reclaimed_or_invalid_ids() {
    assert!(matches!(ObjectId::new(0), Err(Error::InvalidInput(_))));
    assert!(matches!(
        ObjectId::new(1 << 63),
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(ObjectId::new(1).unwrap(), ROOT);

    let mut volume = opened();
    let file = volume.create_file(ROOT, name("file")).unwrap();
    assert_eq!(volume.write(&file, 0, b"kept").unwrap(), 4);
    let object = volume.lookup(ROOT, &name("file")).unwrap();
    volume
        .remove(ROOT, &name("file"), RemoveKind::File)
        .unwrap();

    assert!(volume.attributes(object).unwrap().orphan);
    let reopened = volume
        .open_object(ObjectId::new(object.get()).unwrap(), Access::ReadWrite)
        .unwrap();
    let mut buffer = [0; 4];
    assert_eq!(volume.read(&reopened, 0, &mut buffer).unwrap(), 4);
    assert_eq!(&buffer, b"kept");
    assert!(matches!(
        volume.open_object(ROOT, Access::ReadWrite),
        Err(Error::WrongType)
    ));
    let directory = volume.open_object(ROOT, Access::Read).unwrap();
    assert!(
        volume
            .read_directory(&directory, None)
            .unwrap()
            .entries
            .is_empty()
    );

    for handle in [file, reopened, directory] {
        volume.close_handle(handle).unwrap();
    }
    (0..4).for_each(|_| volume.sync().unwrap());
    assert!(matches!(
        volume.open_object(object, Access::Read),
        Err(Error::NotFound)
    ));
    checked(&volume);
}

#[test]
fn space_tracks_data_allocation_and_excludes_the_reserve() {
    let mut volume = opened();
    let before = volume.space().unwrap();
    assert_eq!(before.block_bytes, 4096);
    assert_eq!(before.total_blocks, GEOMETRY.blocks());
    assert_eq!(
        before.available_blocks,
        before.free_blocks - crate::allocation::RESERVE
    );
    let file = volume.create_file(ROOT, name("data")).unwrap();
    assert_eq!(volume.write(&file, 0, &[7; 1 << 20]).unwrap(), 1 << 20);
    let after = volume.space().unwrap();
    assert!(after.available_blocks + 256 <= before.available_blocks);
    volume.close_handle(file).unwrap();
}
