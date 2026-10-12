//! Deferred checkpoints: chained plans never disturb the durable state.

use super::*;

fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| (index as u32).wrapping_mul(2_654_435_761).to_le_bytes()[2] ^ seed)
        .collect()
}

fn put(volume: &mut Volume<FaultDevice>, file: &str, seed: u8, len: usize) {
    let handle = volume.create_file(ROOT, name(file)).unwrap();
    for (index, chunk) in pattern(seed, len).chunks(1 << 20).enumerate() {
        assert_eq!(
            volume.write(&handle, (index << 20) as u64, chunk).unwrap(),
            chunk.len()
        );
    }
    volume.close_handle(handle).unwrap();
}

fn contents(volume: &mut Volume<FaultDevice>, file: &str) -> Option<Vec<u8>> {
    let object = volume.lookup(ROOT, &name(file)).ok()?;
    let size = volume.attributes(object).unwrap().size as usize;
    let handle = volume.open_object(object, Access::Read).unwrap();
    let mut out = vec![0; size];
    let mut done = 0;
    while done < size {
        done += volume.read(&handle, done as u64, &mut out[done..]).unwrap();
    }
    volume.close_handle(handle).unwrap();
    Some(out)
}

/// Reopen what reached durable storage, as after power loss, and check it.
fn crashed(volume: &Volume<FaultDevice>) -> Volume<FaultDevice> {
    checked(volume);
    Volume::open(FaultDevice::new(durable(volume), MountMode::ReadWrite)).unwrap()
}

/// The chain's safety rule, observed on the device: between two flushes no
/// block is written twice.
fn assert_single_writes_between_flushes(volume: &Volume<FaultDevice>) {
    let media = volume.device.as_ref().unwrap().media.borrow();
    let mut interval = BTreeSet::new();
    for event in &media.events {
        match event {
            Event::Write(block) => assert!(interval.insert(*block), "block {block} rewritten"),
            Event::Flush => interval.clear(),
        }
    }
}

fn flushes(volume: &Volume<FaultDevice>) -> usize {
    let media = volume.device.as_ref().unwrap().media.borrow();
    media.events.iter().filter(|e| **e == Event::Flush).count()
}

/// A file sync with nothing unpublished touches the device no more: the last
/// checkpoint flushed both roots. What it then promises is already what a
/// crash recovers. A full sync that finds nothing to do still ends in a flush.
#[test]
fn a_publication_sync_with_nothing_unsynced_issues_no_device_call() {
    let events = |volume: &Volume<FaultDevice>| {
        let media = volume.device.as_ref().unwrap().media.borrow();
        media.events.len()
    };
    let mut volume = opened();
    volume.set_deferred_checkpoints(true).unwrap();
    put(&mut volume, "kept", 5, 150_000);
    assert!(volume.has_unsynced_changes());
    volume.sync_publication().unwrap();
    assert!(!volume.has_unsynced_changes());
    assert!(flushes(&volume) >= 3, "the publication flushed");

    let (before, flushed) = (events(&volume), flushes(&volume));
    for _ in 0..3 {
        volume.sync_publication().unwrap();
    }
    assert_eq!(
        events(&volume),
        before,
        "a clean file sync reached the device"
    );
    let mut recovered = crashed(&volume);
    assert_eq!(contents(&mut recovered, "kept"), Some(pattern(5, 150_000)));

    // The next change is published by the next file sync as before.
    put(&mut volume, "next", 6, 20_000);
    volume.sync_publication().unwrap();
    assert!(flushes(&volume) >= flushed + 3);
    let mut recovered = crashed(&volume);
    assert_eq!(contents(&mut recovered, "next"), Some(pattern(6, 20_000)));

    // Drain cleanup, then a full sync with nothing to do: one flush each.
    for _ in 0..8 {
        volume.sync().unwrap();
    }
    let flushed = flushes(&volume);
    volume.sync().unwrap();
    assert_eq!(flushes(&volume), flushed + 1);
}

/// A durable file, then a deferred chain that overwrites it, removes it,
/// reuses its name, and churns metadata. Until `sync` the durable file and
/// namespace survive a crash; after it the new state does.
#[test]
fn deferred_chain_leaves_the_durable_state_until_sync() {
    let mut volume = opened();
    put(&mut volume, "keep", 1, 300_000);
    put(&mut volume, "gone", 2, 70_000);
    volume.sync().unwrap();
    volume.set_deferred_checkpoints(true).unwrap();
    volume
        .device
        .as_ref()
        .unwrap()
        .media
        .borrow_mut()
        .events
        .clear();

    put(&mut volume, "fresh", 3, 200_000);
    let keep = volume
        .open_file(ROOT, &name("keep"), Access::ReadWrite)
        .unwrap();
    volume.write(&keep, 4096, &pattern(9, 100_000)).unwrap();
    volume.close_handle(keep).unwrap();
    volume
        .remove(ROOT, &name("gone"), RemoveKind::File)
        .unwrap();
    for index in 0..100 {
        volume
            .create_directory(ROOT, name(&format!("d{index}")))
            .unwrap();
    }
    for index in 0..100 {
        volume
            .remove(ROOT, &name(&format!("d{index}")), RemoveKind::Directory)
            .unwrap();
    }
    put(&mut volume, "gone", 4, 90_000);
    assert!(volume.has_unsynced_changes());
    assert_eq!(flushes(&volume), 0, "the chain stayed below its bounds");
    assert_single_writes_between_flushes(&volume);

    let mut old = crashed(&volume);
    assert_eq!(contents(&mut old, "keep"), Some(pattern(1, 300_000)));
    assert_eq!(contents(&mut old, "gone"), Some(pattern(2, 70_000)));
    assert_eq!(contents(&mut old, "fresh"), None);
    assert!(old.lookup(ROOT, &name("d0")).is_err());

    volume.sync().unwrap();
    assert!(!volume.has_unsynced_changes());
    assert_single_writes_between_flushes(&volume);
    let mut new = crashed(&volume);
    let mut expected = pattern(1, 300_000);
    expected[4096..104_096].copy_from_slice(&pattern(9, 100_000));
    assert_eq!(contents(&mut new, "keep"), Some(expected));
    assert_eq!(contents(&mut new, "gone"), Some(pattern(4, 90_000)));
    assert_eq!(contents(&mut new, "fresh"), Some(pattern(3, 200_000)));
}

/// A transaction reaches the pages the previous one built through the cache,
/// not the device. Rereading them cost about 12 device reads per file creation
/// through the Linux adapter. Creating files in one
/// directory copies the same path each time, so once the chain is warm a
/// creation reads nothing at all.
#[test]
fn a_chain_does_not_read_back_the_pages_it_built() {
    let mut volume = opened();
    volume.set_deferred_checkpoints(true).unwrap();
    for index in 0..20 {
        put(&mut volume, &format!("warm{index}"), index as u8, 4_096);
    }
    let reads = |volume: &Volume<FaultDevice>| volume.device.as_ref().unwrap().media.borrow().reads;
    let before = reads(&volume);
    for index in 0..50 {
        put(&mut volume, &format!("f{index}"), index as u8, 4_096);
    }
    let read = reads(&volume) - before;
    assert_eq!(read, 0, "50 creations read {read} pages back");
    volume.sync().unwrap();
    let mut reopened = crashed(&volume);
    for index in 0..50 {
        let file = format!("f{index}");
        assert_eq!(
            contents(&mut reopened, &file),
            Some(pattern(index as u8, 4_096))
        );
    }
}

/// Pages a chain wrote and then superseded are discarded, and nothing reads
/// them again: the model turns each into garbage, yet a crash before the sync
/// recovers the durable state, and one after it recovers every file.
#[test]
fn superseded_chain_pages_are_discarded_unread() {
    let mut volume = opened();
    put(&mut volume, "keep", 1, 150_000);
    volume.sync().unwrap();
    volume.set_deferred_checkpoints(true).unwrap();
    // Each file is its own plan, as when an operation breaks a batch bound:
    // within one batch superseded pages are never written at all.
    for index in 0..60 {
        put(&mut volume, &format!("f{index}"), index as u8, 5_000);
        volume.seal_batch().unwrap();
    }
    volume.remove(ROOT, &name("f7"), RemoveKind::File).unwrap();
    volume.seal_batch().unwrap();
    let discarded = volume.device.as_ref().unwrap().media.borrow().discarded;
    assert!(discarded > 60, "only {discarded} pages discarded");
    assert_eq!(contents(&mut volume, "f59"), Some(pattern(59, 5_000)));
    let mut old = crashed(&volume);
    assert_eq!(contents(&mut old, "keep"), Some(pattern(1, 150_000)));
    assert_eq!(contents(&mut old, "f0"), None);
    volume.sync().unwrap();
    let mut reopened = crashed(&volume);
    assert_eq!(contents(&mut reopened, "keep"), Some(pattern(1, 150_000)));
    assert_eq!(contents(&mut reopened, "f7"), None);
    for index in (0..60).filter(|index| *index != 7) {
        let file = format!("f{index}");
        assert_eq!(
            contents(&mut reopened, &file),
            Some(pattern(index as u8, 5_000))
        );
    }
}

/// Interrupt every output event of a deferred `sync`, with nothing or all of
/// the pending writes persisted, and recover the old or the new state.
#[test]
fn interrupted_deferred_sync_recovers_old_or_new_state() {
    let base = {
        let mut volume = opened();
        put(&mut volume, "keep", 1, 150_000);
        volume.sync().unwrap();
        volume.into_model_image()
    };
    let run = |publication_only: bool, failure: Option<(usize, u64)>| {
        let mut volume =
            Volume::open(FaultDevice::new(base.clone(), MountMode::ReadWrite)).unwrap();
        volume.set_deferred_checkpoints(true).unwrap();
        let keep = volume
            .open_file(ROOT, &name("keep"), Access::ReadWrite)
            .unwrap();
        volume.write(&keep, 0, &pattern(7, 150_000)).unwrap();
        volume.close_handle(keep).unwrap();
        volume.create_directory(ROOT, name("marker")).unwrap();
        let events = volume.device.as_ref().unwrap().media.borrow().events.len();
        if let Some((event, whole_mask)) = failure {
            volume.device.as_ref().unwrap().media.borrow_mut().failure = Some(Failure {
                event: events + event,
                whole_mask,
                tear: None,
            });
        }
        let result = if publication_only {
            volume.sync_publication()
        } else {
            volume.sync()
        };
        let total = volume.device.as_ref().unwrap().media.borrow().events.len() - events;
        (volume, result, total)
    };
    for publication_only in [false, true] {
        let (_, result, total) = run(publication_only, None);
        result.unwrap();
        for event in 0..total {
            for whole_mask in [0, u64::MAX] {
                let (volume, result, _) = run(publication_only, Some((event, whole_mask)));
                assert!(result.is_err(), "event {event} did not fail");
                let mut recovered = crashed(&volume);
                let new = recovered.lookup(ROOT, &name("marker")).is_ok();
                let keep = contents(&mut recovered, "keep");
                assert_eq!(
                    keep,
                    Some(pattern(if new { 7 } else { 1 }, 150_000)),
                    "publication_only={publication_only}, event {event}, mask {whole_mask:#x}"
                );
            }
        }
    }
}

#[test]
fn publication_sync_persists_orphans_for_later_cleanup() {
    let mut volume = opened();
    volume.set_deferred_checkpoints(true).unwrap();
    put(&mut volume, "removed", 1, 4_096);
    volume.sync_publication().unwrap();
    volume
        .remove(ROOT, &name("removed"), RemoveKind::File)
        .unwrap();
    volume.sync_publication().unwrap();
    assert!(cleanup_records(&mut volume) > 0);

    let mut resumed = crashed(&volume);
    assert_eq!(contents(&mut resumed, "removed"), None);
    assert!(cleanup_records(&mut resumed) > 0);
    drain_cleanup(&mut resumed);
    assert_eq!(data_blocks(&resumed), 0);
}

/// A long chain checkpoints on its own bounds, and a crash at any point
/// recovers a checkpointed prefix that passes the full check. Each file is
/// sealed as its own plan, so this drives the chain's own bounds; batching
/// would otherwise keep the chain far below them.
#[test]
fn long_deferred_chains_checkpoint_within_their_bounds() {
    long_chain(true);
}

/// The same 1,500 files with batching: no block is written twice between
/// flushes, and whatever a crash recovers is a prefix of the files created.
#[test]
fn long_batched_chains_recover_a_prefix() {
    long_chain(false);
}

fn long_chain(plan_per_file: bool) {
    let mut volume = opened();
    volume.set_deferred_checkpoints(true).unwrap();
    volume
        .device
        .as_ref()
        .unwrap()
        .media
        .borrow_mut()
        .events
        .clear();
    for index in 0..1500 {
        let handle = volume
            .create_file(ROOT, name(&format!("f{index:04}")))
            .unwrap();
        volume.write(&handle, 0, &[index as u8; 5000]).unwrap();
        volume.close_handle(handle).unwrap();
        if plan_per_file {
            volume.seal_batch().unwrap();
        }
    }
    if plan_per_file {
        assert!(
            flushes(&volume) >= 3,
            "slot or page bounds forced a checkpoint"
        );
    }
    assert_single_writes_between_flushes(&volume);
    let mut recovered = crashed(&volume);
    let survivors = (0..1500)
        .take_while(|index| {
            recovered
                .lookup(ROOT, &name(&format!("f{index:04}")))
                .is_ok()
        })
        .count();
    assert!(survivors < 1500, "{survivors} files survived");
    if plan_per_file {
        assert!(survivors > 0, "no checkpoint was forced");
    }
    // A prefix: nothing after the first missing file survived either.
    for index in survivors..1500 {
        assert!(
            recovered
                .lookup(ROOT, &name(&format!("f{index:04}")))
                .is_err(),
            "f{index:04} survived after f{survivors:04} was lost"
        );
    }
}

/// Deleted data is reclaimed across deferred syncs: retirement waits for the
/// checkpoint that makes it durable, then maintenance frees it.
#[test]
fn deferred_syncs_reclaim_deleted_data() {
    let mut volume = opened();
    volume.set_deferred_checkpoints(true).unwrap();
    put(&mut volume, "big", 5, 3 << 20);
    volume.sync().unwrap();
    volume.remove(ROOT, &name("big"), RemoveKind::File).unwrap();
    let data_blocks = |volume: &Volume<FaultDevice>| {
        Volume::open(FaultDevice::new(durable(volume), MountMode::ReadOnly))
            .unwrap()
            .check()
            .unwrap()
            .data_blocks
    };
    let mut syncs = 0;
    while data_blocks(&volume) > 0 {
        volume.sync().unwrap();
        syncs += 1;
        assert!(syncs < 20, "reclamation stalled");
    }
    assert_single_writes_between_flushes(&volume);
}

/// Maintenance chained before a checkpoint must not free data the durable
/// root still references, even though the chained root retired it. Failing the
/// checkpoint's first flush leaves the chained maintenance visible in memory.
#[test]
fn chained_maintenance_keeps_retirement_newer_than_the_durable_root() {
    let mut volume = opened();
    put(&mut volume, "old", 1, 3 << 20);
    volume.sync().unwrap();
    let image = volume.into_model_image();
    let run = |fail_at: Option<usize>| {
        let mut volume =
            Volume::open(FaultDevice::new(image.clone(), MountMode::ReadWrite)).unwrap();
        volume.set_deferred_checkpoints(true).unwrap();
        let old = volume
            .open_file(ROOT, &name("old"), Access::ReadWrite)
            .unwrap();
        for offset in (0..3 << 20).step_by(1 << 20) {
            volume.write(&old, offset, &pattern(2, 1 << 20)).unwrap();
        }
        volume.close_handle(old).unwrap();
        let before = volume.root.as_ref().unwrap().free_blocks;
        let media = volume.device.as_ref().unwrap().media.clone();
        let start = media.borrow().events.len();
        if let Some(event) = fail_at {
            media.borrow_mut().failure = Some(Failure {
                event: start + event,
                whole_mask: 0,
                tear: None,
            });
        }
        let result = volume.sync();
        let first_flush = media.borrow().events[start..]
            .iter()
            .position(|event| *event == Event::Flush);
        let after = volume.root.as_ref().unwrap().free_blocks;
        (volume, result, before, after, first_flush)
    };
    let (_, result, _, _, first_flush) = run(None);
    result.unwrap();
    let (volume, result, before, after, _) = run(first_flush);
    assert!(result.is_err());
    assert!(
        after < before + 512,
        "chained maintenance freed retired data: {before} -> {after}"
    );
    let mut recovered = crashed(&volume);
    assert_eq!(contents(&mut recovered, "old"), Some(pattern(1, 3 << 20)));
}

/// Fill, delete, and refill a 256 MiB volume with deferred checkpoints, handling
/// `NoSpace` as the Linux adapter does: sync until quiet, then retry once.
#[test]
fn deferred_fill_delete_refill_reaches_the_same_size() {
    fn drain(volume: &mut Volume<FaultDevice>) {
        for _ in 0..10_000 {
            let before = volume.info().unwrap().generation;
            volume.sync().unwrap();
            if volume.info().unwrap().generation == before {
                return;
            }
        }
        panic!("cleanup did not quiesce");
    }
    let mut volume = opened();
    volume.set_deferred_checkpoints(true).unwrap();
    let chunk = pattern(3, 1 << 20);
    let mut sizes = Vec::new();
    for round in 0..3 {
        let file = volume.create_file(ROOT, name("fill")).unwrap();
        let mut size = 0u64;
        loop {
            let result = match volume.write(&file, size, &chunk) {
                Err(Error::NoSpace) => {
                    drain(&mut volume);
                    volume.write(&file, size, &chunk)
                }
                result => result,
            };
            match result {
                Ok(written) => size += written as u64,
                Err(Error::NoSpace) => break,
                Err(error) => panic!("round {round} at {size}: {error:?}"),
            }
        }
        volume.close_handle(file).unwrap();
        volume.sync().unwrap();
        checked(&volume);
        assert_eq!(
            contents(&mut crashed(&volume), "fill").map(|b| b.len() as u64),
            Some(size)
        );
        volume
            .remove(ROOT, &name("fill"), RemoveKind::File)
            .unwrap();
        drain(&mut volume);
        sizes.push(size);
    }
    assert_single_writes_between_flushes(&volume);
    assert!(sizes[0] > 100 << 20, "{sizes:?}");
    assert!(sizes.windows(2).all(|pair| pair[0] == pair[1]), "{sizes:?}");
    let summary = Volume::open(FaultDevice::new(durable(&volume), MountMode::ReadOnly))
        .unwrap()
        .check()
        .unwrap();
    assert_eq!((summary.objects, summary.data_blocks), (1, 0));
}

/// Scrub and extraction carry the covering extent between units. A file whose
/// extents change under the scan, through overwrites and a hole, must still
/// verify and read back byte for byte.
#[test]
fn inspection_follows_extents_that_change_under_the_scan() {
    let mut volume = opened();
    let handle = volume.create_file(ROOT, name("mixed")).unwrap();
    let units = 12;
    let mut expected = pattern(5, units * 65_536);
    volume.write(&handle, 0, &expected).unwrap();
    // Overwriting single units relocates them, so the extents interleave.
    for unit in [1, 4, 5, 9] {
        let fresh = pattern(unit as u8 + 40, 65_536);
        let offset = unit * 65_536;
        volume.write(&handle, offset as u64, &fresh).unwrap();
        expected[offset..offset + 65_536].copy_from_slice(&fresh);
    }
    // A hole past the written range, so the scan also meets an unbacked unit.
    let size = (units + 4) * 65_536;
    volume.truncate(&handle, size as u64).unwrap();
    expected.resize(size, 0);
    volume.close_handle(handle).unwrap();
    volume.sync().unwrap();
    checked(&volume);

    let mut reader = Volume::open(FaultDevice::new(durable(&volume), MountMode::ReadOnly)).unwrap();
    let summary = reader
        .scrub(&mut |finding| panic!("scrub finding: {finding:?}"))
        .unwrap();
    assert_eq!(summary.unreadable_ranges, 0);
    assert!(summary.verified_units >= units as u64);

    let mut volume =
        Volume::open(FaultDevice::new(durable(&volume), MountMode::ReadWrite)).unwrap();
    assert_eq!(contents(&mut volume, "mixed"), Some(expected));
}

fn generation(volume: &Volume<FaultDevice>) -> u64 {
    volume.root.as_ref().unwrap().generation
}

/// Operations with deferred checkpoints share one plan until `sync`, and reads
/// see them before any plan is sealed: lookup, listing, and file data, including
/// a patch that reads back a unit still queued in the batch. A crash before the
/// sync recovers the old state; after it, the new one.
#[test]
fn a_batch_is_one_plan_and_reads_see_it_before_the_seal() {
    let mut volume = opened();
    volume.set_deferred_checkpoints(true).unwrap();
    let before = generation(&volume);
    volume.create_directory(ROOT, name("dir")).unwrap();
    for index in 0..50 {
        put(&mut volume, &format!("f{index}"), index as u8, 70_000);
    }
    let patched = volume
        .open_file(ROOT, &name("f3"), Access::ReadWrite)
        .unwrap();
    volume.write(&patched, 65_530, b"patch").unwrap();
    volume.close_handle(patched).unwrap();
    let mut expected = pattern(3, 70_000);
    expected[65_530..65_535].copy_from_slice(b"patch");

    assert_eq!(
        generation(&volume),
        before,
        "an operation sealed its own plan"
    );
    assert!(volume.has_unsynced_changes());
    assert!(volume.lookup(ROOT, &name("dir")).is_ok());
    let root = volume.open_directory(ROOT).unwrap();
    let (mut listed, mut cursor) = (0, None);
    loop {
        let page = volume.read_directory(&root, cursor.as_ref()).unwrap();
        listed += page.entries.len();
        match page.cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    volume.close_handle(root).unwrap();
    assert_eq!(listed, 51);
    assert_eq!(contents(&mut volume, "f3"), Some(expected.clone()));
    for index in (0..50).filter(|index| *index != 3) {
        let file = format!("f{index}");
        assert_eq!(
            contents(&mut volume, &file),
            Some(pattern(index as u8, 70_000))
        );
    }

    let mut old = crashed(&volume);
    assert!(old.lookup(ROOT, &name("dir")).is_err());
    assert_eq!(contents(&mut old, "f0"), None);
    volume.sync().unwrap();
    assert!(generation(&volume) > before);
    let mut new = crashed(&volume);
    assert!(new.lookup(ROOT, &name("dir")).is_ok());
    assert_eq!(contents(&mut new, "f3"), Some(expected));
    assert_eq!(contents(&mut new, "f49"), Some(pattern(49, 70_000)));
}

/// An edit that fails after changing the batch leaves no trace: not its name,
/// its object ID, or its data blocks. The next file receives the ID the failed
/// edit had taken, and the full check, run on the recovered image, finds no
/// allocated block without an owner.
#[test]
fn a_failed_edit_leaves_the_batch_as_it_was() {
    let mut volume = opened();
    volume.set_deferred_checkpoints(true).unwrap();
    put(&mut volume, "a", 1, 70_000);
    let now = Timestamp::now().unwrap();
    let mut taken = None;
    let failed = volume.edit(|transaction| {
        let ghost = super::super::namespace::create_in(
            transaction,
            ROOT,
            name("ghost"),
            ObjectKind::File,
            &[],
            now,
        )?;
        taken = Some(ghost);
        let runs = transaction.allocate_data(16, 0)?;
        transaction.write_data(runs[0].0, Page(Box::new([9; BLOCK_BYTES])))?;
        Err::<(), _>(Error::InvalidInput("injected after changes"))
    });
    assert!(matches!(
        failed,
        Err(Error::InvalidInput("injected after changes"))
    ));
    assert!(volume.lookup(ROOT, &name("ghost")).is_err());
    put(&mut volume, "b", 2, 70_000);
    assert_eq!(volume.lookup(ROOT, &name("b")).ok(), taken);
    volume.sync().unwrap();
    let mut recovered = crashed(&volume);
    assert!(recovered.lookup(ROOT, &name("ghost")).is_err());
    assert_eq!(contents(&mut recovered, "a"), Some(pattern(1, 70_000)));
    assert_eq!(contents(&mut recovered, "b"), Some(pattern(2, 70_000)));
}

/// An operation that would break a batch bound does not join: the batch is
/// sealed as one plan without it, and it runs as its own. Queued data is
/// bounded at 16 MiB, so the last 1 MiB write of a 17 MiB file cannot join.
#[test]
fn an_operation_over_a_batch_bound_runs_as_its_own_plan() {
    let mut volume = opened();
    volume.set_deferred_checkpoints(true).unwrap();
    let before = generation(&volume);
    put(&mut volume, "small", 1, 5_000);
    assert_eq!(generation(&volume), before);
    put(&mut volume, "big", 2, 17 << 20);
    assert!(
        generation(&volume) >= before + 2,
        "the batch and the refused write were not two plans"
    );
    assert_single_writes_between_flushes(&volume);
    assert_eq!(contents(&mut volume, "small"), Some(pattern(1, 5_000)));
    assert_eq!(contents(&mut volume, "big"), Some(pattern(2, 17 << 20)));
    volume.sync().unwrap();
    let mut recovered = crashed(&volume);
    assert_eq!(contents(&mut recovered, "small"), Some(pattern(1, 5_000)));
    assert_eq!(contents(&mut recovered, "big"), Some(pattern(2, 17 << 20)));
}

/// Every output event of an operation that seals the batch and then runs as
/// its own plan, interrupted, whole or partial: the durable state survives
/// and passes the full check. Creations fill the batch's draft bound, so the
/// refused operation's plan and the batch's are both small.
#[test]
fn interrupting_a_refused_operation_keeps_the_durable_state() {
    let base = {
        let mut volume = opened();
        put(&mut volume, "keep", 1, 150_000);
        volume.sync().unwrap();
        volume.into_model_image()
    };
    // The first creation the batch refuses.
    let refused = {
        let mut volume =
            Volume::open(FaultDevice::new(base.clone(), MountMode::ReadWrite)).unwrap();
        volume.set_deferred_checkpoints(true).unwrap();
        let before = generation(&volume);
        (0..10_000)
            .find(|index| {
                volume
                    .create_directory(ROOT, name(&format!("d{index:05}")))
                    .unwrap();
                generation(&volume) != before
            })
            .expect("the batch never refused a creation")
    };
    let run = |failure: Option<(usize, u64)>| {
        let mut volume =
            Volume::open(FaultDevice::new(base.clone(), MountMode::ReadWrite)).unwrap();
        volume.set_deferred_checkpoints(true).unwrap();
        for index in 0..refused {
            volume
                .create_directory(ROOT, name(&format!("d{index:05}")))
                .unwrap();
        }
        let events = volume.device.as_ref().unwrap().media.borrow().events.len();
        if let Some((event, whole_mask)) = failure {
            volume.device.as_ref().unwrap().media.borrow_mut().failure = Some(Failure {
                event: events + event,
                whole_mask,
                tear: None,
            });
        }
        let result = volume.create_directory(ROOT, name(&format!("d{refused:05}")));
        let total = volume.device.as_ref().unwrap().media.borrow().events.len() - events;
        (volume, result, total)
    };
    let (volume, result, total) = run(None);
    result.unwrap();
    assert!(total > 0, "the refused creation wrote nothing");
    drop(volume);
    for event in 0..total {
        for whole_mask in [0, u64::MAX] {
            let (volume, result, _) = run(Some((event, whole_mask)));
            assert!(result.is_err(), "event {event} did not fail");
            let mut recovered = crashed(&volume);
            assert_eq!(
                contents(&mut recovered, "keep"),
                Some(pattern(1, 150_000)),
                "event {event}, mask {whole_mask:#x}"
            );
        }
    }
}

/// The batched-operations fuzz driver over fixed random inputs, so the
/// ordinary test run covers it without a fuzzer.
#[test]
fn batched_operations_match_their_model() {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    for _ in 0..300 {
        let len = 4 * (1 + (state % 64) as usize);
        let data: Vec<u8> = (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (state >> 56) as u8
            })
            .collect();
        crate::volume::model::fuzz_batch(&data);
    }
}

/// Thousands of removals with no checkpoint between them can exhaust the
/// workspace, because pages they supersede stay reserved until a checkpoint.
/// An edit that hits `LimitExceeded` (`EFBIG` through FUSE) checkpoints and
/// retries. Slow in debug builds: run with
/// `cargo test --release --lib unbatched_removal -- --ignored`.
#[test]
#[ignore = "20,000 creations and removals; run in release"]
fn unbatched_removal_of_thousands_of_files_checkpoints_instead_of_failing() {
    let mut volume = opened();
    volume.set_deferred_checkpoints(true).unwrap();
    let directories: Vec<_> = (0..200)
        .map(|index| {
            volume
                .create_directory(ROOT, name(&format!("d{index:03}")))
                .unwrap()
        })
        .collect();
    for index in 0..20_000usize {
        let handle = volume
            .create_file(directories[index % 200], name(&format!("f{index:05}")))
            .unwrap();
        volume.write(&handle, 0, &[7; 4096]).unwrap();
        volume.close_handle(handle).unwrap();
        if index % 128 == 127 {
            volume.sync().unwrap();
        }
    }
    volume.sync().unwrap();
    for (slot, directory) in directories.iter().enumerate() {
        for index in (slot..20_000).step_by(200) {
            volume
                .remove(*directory, &name(&format!("f{index:05}")), RemoveKind::File)
                .unwrap();
        }
    }
    volume.sync().unwrap();
    let mut reader = Volume::open(FaultDevice::new(durable(&volume), MountMode::ReadOnly)).unwrap();
    assert_eq!(reader.check().unwrap().objects, 201);
}

/// Device reads taken to look up every file's attributes.
fn reads_to_stat(volume: &mut Volume<FaultDevice>, files: usize) -> usize {
    let reads = |volume: &Volume<FaultDevice>| volume.device.as_ref().unwrap().media.borrow().reads;
    let before = reads(volume);
    for index in 0..files {
        let object = volume.lookup(ROOT, &name(&format!("f{index:03}"))).unwrap();
        assert_eq!(volume.attributes(object).unwrap().size, 100);
    }
    reads(volume) - before
}

/// A catalog of several leaves under one branch, published.
fn many_small_files() -> Volume<FaultDevice> {
    let mut volume = opened();
    volume.set_deferred_checkpoints(true).unwrap();
    for index in 0..300 {
        put(&mut volume, &format!("f{index:03}"), index as u8, 100);
    }
    volume.sync().unwrap();
    volume
}

/// A writable open keeps the catalog leaves its check verified, so the first
/// lookups read the branch above them and no leaf again.
#[test]
fn a_writable_open_keeps_the_leaves_its_check_read() {
    let volume = many_small_files();
    let image = durable(&volume);
    let mut read_only = Volume::open(FaultDevice::new(image.clone(), MountMode::ReadOnly)).unwrap();
    read_only.enable_readonly_metadata_cache().unwrap();
    let leaves = reads_to_stat(&mut read_only, 300);
    assert!(leaves > 10, "the catalog has {leaves} pages to read");
    let mut writable = Volume::open(FaultDevice::new(image, MountMode::ReadWrite)).unwrap();
    let read = reads_to_stat(&mut writable, 300);
    assert!(
        read <= 2,
        "{read} pages read after the check kept its leaves"
    );
}

/// Leaves read ahead on a second handle serve a read-only volume's lookups,
/// and leaves read for another root are not kept.
#[test]
fn leaves_read_ahead_serve_a_read_only_volume() {
    let image = durable(&many_small_files());
    let device = FaultDevice::new(image, MountMode::ReadOnly);
    let mut reader = device.with_mode(MountMode::ReadOnly);
    let mut volume = Volume::open(device).unwrap();
    assert!(matches!(volume.read_ahead(), Err(Error::InvalidInput(_))));
    volume.enable_readonly_metadata_cache().unwrap();
    let mut ahead = volume.read_ahead().unwrap();
    let mut batches = 0;
    loop {
        let leaves = ahead.next(&mut reader, 4).unwrap();
        if leaves.is_empty() {
            break;
        }
        assert!(volume.keep_read_ahead(leaves).unwrap());
        batches += 1;
    }
    assert!(batches > 2, "{batches} batches of four leaves");
    let read = reads_to_stat(&mut volume, 300);
    assert!(read <= 2, "{read} pages read after the read-ahead");

    // Another volume's leaves, and a reader pointed at another medium.
    let mut other = opened();
    put(&mut other, "f000", 9, 100);
    other.sync().unwrap();
    let device = FaultDevice::new(durable(&other), MountMode::ReadOnly);
    let mut other_reader = device.with_mode(MountMode::ReadOnly);
    let mut other = Volume::open(device).unwrap();
    other.enable_readonly_metadata_cache().unwrap();
    let foreign = other
        .read_ahead()
        .unwrap()
        .next(&mut other_reader, 4)
        .unwrap();
    assert!(!foreign.is_empty());
    assert!(!volume.keep_read_ahead(foreign).unwrap());
    assert!(
        volume
            .read_ahead()
            .unwrap()
            .next(&mut other_reader, 4)
            .is_err()
    );
}
