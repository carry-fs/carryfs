//! Extended foreground workloads. Measurements come from the independent checker.

use super::*;
use std::fs::File;
use std::io::{BufWriter, Write};

const UNIT: usize = 65_536;
const CHUNK: usize = 1 << 20;

#[test]
fn namespace_links_reject_missing_and_duplicate_entries() {
    let mut volume = opened();
    for index in 0..1025 {
        let file = volume
            .create_file(ROOT, name(&format!("link-{index:04}")))
            .unwrap();
        volume.close_handle(file).unwrap();
    }
    checked(&volume);
    let base = durable(&volume);
    for duplicate in [false, true] {
        let mut volume =
            Volume::open(FaultDevice::new(base.clone(), MountMode::ReadWrite)).unwrap();
        volume
            .edit(|transaction| {
                let key = namespace::dirent_key(ROOT.0, b"link-1024");
                let mut catalog = transaction.catalog.clone();
                if duplicate {
                    let mut value = transaction
                        .forest
                        .lookup(&catalog, TreeKind::Catalog, &key)?
                        .unwrap();
                    value[12..].copy_from_slice(b"dupe-1024");
                    catalog = transaction.forest.put(
                        catalog,
                        TreeKind::Catalog,
                        &namespace::dirent_key(ROOT.0, b"dupe-1024"),
                        &value,
                    )?;
                } else {
                    catalog = transaction
                        .forest
                        .remove(catalog, TreeKind::Catalog, &key)?;
                }
                let key = namespace::inode_key(ROOT.0);
                let value = transaction
                    .forest
                    .lookup(&catalog, TreeKind::Catalog, &key)?
                    .unwrap();
                let mut inode = namespace::Inode::decode(&value)?;
                inode.child_count = if duplicate {
                    inode.child_count + 1
                } else {
                    inode.child_count - 1
                };
                transaction.catalog =
                    transaction
                        .forest
                        .put(catalog, TreeKind::Catalog, &key, &inode.encode())?;
                Ok(())
            })
            .unwrap();
        let root = volume.root.clone().unwrap();
        crate::check::validate_ownership(volume.device.as_mut().unwrap(), &root).unwrap();
        assert!(matches!(
            crate::check::validate(volume.device.as_mut().unwrap(), &root),
            Err(Error::Corruption {
                reason: "linked inode does not have exactly one directory entry",
                ..
            })
        ));
    }
}

#[test]
#[ignore = "measure one structural check of populated namespaces"]
fn measure_namespace_check() {
    let mut volume = opened();
    for index in 1..=1025 {
        let file = volume
            .create_file(ROOT, name(&format!("file-{index:04}")))
            .unwrap();
        volume.write(&file, 0, &[1; 4096]).unwrap();
        volume.close_handle(file).unwrap();
        if [256, 1025].contains(&index) {
            let root = volume.root.clone().unwrap();
            let start = std::time::Instant::now();
            let summary = crate::check::validate(volume.device.as_mut().unwrap(), &root).unwrap();
            eprintln!(
                "{index} files: check {:?}, {} metadata pages",
                start.elapsed(),
                summary.metadata_pages
            );
        }
    }
}

pub(super) fn counts(volume: &mut Volume<FaultDevice>) -> [usize; 3] {
    volume
        .read_catalog(|forest, _| {
            let cleanup = forest.root(forest.base.cleanup, TreeKind::Cleanup)?;
            let mut start = vec![1];
            let mut counts = [0; 3];
            loop {
                let records = forest.scan(&cleanup, TreeKind::Cleanup, &start, 64)?;
                for (key, _) in &records {
                    counts[usize::from(key[0] - 1)] += 1;
                }
                let Some((last, _)) = records.last() else {
                    break;
                };
                start = last.clone();
                start.push(0);
            }
            Ok(counts)
        })
        .unwrap()
}

struct Measurements {
    output: BufWriter<File>,
    cycle: usize,
    pages: u64,
    slots: u64,
    maxima: BTreeMap<&'static str, i64>,
    operations: usize,
    minimum_slots: u64,
    started: std::time::Instant,
    reported: std::time::Instant,
}

impl Measurements {
    fn new(layout: &str) -> Self {
        std::fs::create_dir_all("target/full-disk").unwrap();
        let mut output =
            BufWriter::new(File::create(format!("target/full-disk/{layout}.csv")).unwrap());
        writeln!(output, "cycle,operation,generation,metadata_pages,metadata_growth,workspace_slots,workspace_change,raw_free,data_blocks,retirement,detached,orphans,output_events").unwrap();
        Self {
            output,
            cycle: 0,
            pages: 0,
            slots: 0,
            maxima: BTreeMap::new(),
            operations: 0,
            minimum_slots: u64::MAX,
            started: std::time::Instant::now(),
            reported: std::time::Instant::now(),
        }
    }

    fn record(&mut self, operation: &'static str, volume: &mut Volume<FaultDevice>) {
        let root = volume.root.clone().unwrap();
        let summary =
            crate::check::validate_ownership(volume.device.as_mut().unwrap(), &root).unwrap();
        let slots = crate::workspace::read(volume.device.as_mut().unwrap(), &root)
            .unwrap()
            .len() as u64;
        let raw = summary.free_blocks - slots;
        assert!(slots >= 4096);
        if matches!(operation, "write" | "overwrite") {
            assert!(raw >= crate::allocation::RESERVE);
        }
        let growth = summary.metadata_pages as i64 - self.pages as i64;
        if operation != "initial" {
            self.maxima
                .entry(operation)
                .and_modify(|max| *max = (*max).max(growth))
                .or_insert(growth);
        }
        let [retired, detached, orphans] = counts(volume);
        let mut media = volume.device.as_ref().unwrap().media.borrow_mut();
        writeln!(
            self.output,
            "{},{operation},{},{},{growth},{slots},{},{raw},{},{retired},{detached},{orphans},{}",
            self.cycle,
            root.generation,
            summary.metadata_pages,
            slots as i64 - self.slots as i64,
            summary.data_blocks,
            media.events.len()
        )
        .unwrap();
        media.events.clear();
        self.pages = summary.metadata_pages;
        self.slots = slots;
        self.minimum_slots = self.minimum_slots.min(slots);
        self.operations += 1;
        if self.reported.elapsed().as_secs() >= 10 {
            eprintln!(
                "cycle {}: {} operations, {operation}, elapsed {:?}",
                self.cycle,
                self.operations,
                self.started.elapsed()
            );
            self.output.flush().unwrap();
            self.reported = std::time::Instant::now();
        }
        assert!(
            self.started.elapsed().as_secs() < 600,
            "cycle exceeded its ten-minute budget"
        );
    }

    fn drain(&mut self, volume: &mut Volume<FaultDevice>) {
        for _ in 0..20_000 {
            let old = volume.info().unwrap().generation;
            volume.sync().unwrap();
            self.record("sync", volume);
            if counts(volume) == [0; 3] && volume.info().unwrap().generation == old {
                return;
            }
        }
        panic!("cleanup did not quiesce within 20,000 syncs");
    }
}

fn verify_bytes(volume: &mut Volume<FaultDevice>, file: &Handle, size: u64, byte: u8) {
    let mut out = vec![0; CHUNK];
    let mut offset = 0;
    while offset < size {
        let take = volume.read(file, offset, &mut out).unwrap();
        assert!(take > 0);
        assert!(
            out[..take].iter().all(|value| *value == byte),
            "contents differ at {offset}"
        );
        offset += take as u64;
    }
    assert_eq!(offset, size);
    assert_eq!(volume.read(file, size, &mut out).unwrap(), 0);
}

fn fill(
    volume: &mut Volume<FaultDevice>,
    metrics: &mut Measurements,
    file: &Handle,
    byte: u8,
) -> u64 {
    let chunk = vec![byte; CHUNK];
    let mut size = 0;
    // Down to single blocks: a unit keeps the blocks a write does not touch,
    // so a partial-tail rewrite needs one block, and a disk that refused a
    // whole unit can still have that.
    for len in [CHUNK, UNIT, BLOCK_BYTES] {
        loop {
            let before = volume.handle_attributes(file).unwrap();
            let generation = volume.info().unwrap().generation;
            match volume.write(file, size, &chunk[..len]) {
                Ok(written) => {
                    assert_eq!(written, len);
                    size += written as u64;
                    metrics.record("write", volume);
                }
                Err(Error::NoSpace) => {
                    assert_eq!(volume.handle_attributes(file).unwrap(), before);
                    assert_eq!(volume.info().unwrap().generation, generation);
                    assert!(
                        volume
                            .device
                            .as_ref()
                            .unwrap()
                            .media
                            .borrow()
                            .events
                            .is_empty()
                    );
                    metrics.record("no-space", volume);
                    break;
                }
                result => panic!("unexpected fill result at {size}: {result:?}"),
            }
        }
    }
    assert!(size >= CHUNK as u64);
    size
}

fn full_disk_cycles(fragmented: bool) {
    let cycles: usize =
        std::env::var("CARRYFS_TEST_CYCLES").map_or(10, |value| value.parse().unwrap());
    assert!((1..=10).contains(&cycles));
    let layout = if fragmented {
        "fragmented"
    } else {
        "sequential"
    };
    let mut metrics = Measurements::new(layout);
    let mut volume = opened();
    metrics.record("initial", &mut volume);
    for cycle in 0..cycles {
        metrics.cycle = cycle;
        metrics.started = std::time::Instant::now();
        let mut small = BTreeMap::new();
        if fragmented {
            for index in 0..1024 {
                let spelling = format!("small-{index:04}");
                let file = volume.create_file(ROOT, name(&spelling)).unwrap();
                metrics.record("create", &mut volume);
                let byte = 1 + (index % 251) as u8;
                volume.write(&file, 0, &vec![byte; 4096]).unwrap();
                metrics.record("write", &mut volume);
                volume.close_handle(file).unwrap();
                small.insert(spelling, byte);
                if index % 32 == 31 {
                    volume.sync().unwrap();
                    metrics.record("sync", &mut volume);
                }
            }
            for index in (0..1024).step_by(2) {
                let spelling = format!("small-{index:04}");
                volume
                    .remove(ROOT, &name(&spelling), RemoveKind::File)
                    .unwrap();
                metrics.record("remove", &mut volume);
                small.remove(&spelling);
            }
            metrics.drain(&mut volume);
            for (spelling, byte) in small.iter_mut().take(64) {
                let file = volume
                    .open_file(ROOT, &name(spelling), Access::ReadWrite)
                    .unwrap();
                *byte ^= 0x80;
                volume.write(&file, 0, &vec![*byte; 4096]).unwrap();
                metrics.record("overwrite", &mut volume);
                volume.close_handle(file).unwrap();
            }
            let source = "small-0001";
            let destination = "small-0003";
            let pinned = volume
                .open_file(ROOT, &name(destination), Access::Read)
                .unwrap();
            let old = small[destination];
            volume
                .rename(
                    ROOT,
                    &name(source),
                    ROOT,
                    name(destination),
                    Replace::Allowed,
                )
                .unwrap();
            metrics.record("replace", &mut volume);
            let moved = small.remove(source).unwrap();
            small.insert(destination.to_owned(), moved);
            verify_bytes(&mut volume, &pinned, 4096, old);
            volume.close_handle(pinned).unwrap();
            metrics.record("close", &mut volume);
            metrics.drain(&mut volume);
        }

        checked(&volume);

        let file = volume.create_file(ROOT, name("filler")).unwrap();
        metrics.record("create", &mut volume);
        let byte = 1 + cycle as u8;
        let size = fill(&mut volume, &mut metrics, &file, byte);
        verify_bytes(&mut volume, &file, size, byte);
        checked(&volume);
        for (spelling, byte) in &small {
            let file = volume
                .open_file(ROOT, &name(spelling), Access::Read)
                .unwrap();
            verify_bytes(&mut volume, &file, 4096, *byte);
            volume.close_handle(file).unwrap();
        }
        let before = volume.handle_attributes(&file).unwrap();
        let info = volume.info().unwrap();
        assert!(matches!(
            volume.truncate(&file, size - 1),
            Err(Error::NoSpace)
        ));
        assert_eq!(volume.handle_attributes(&file).unwrap(), before);
        assert_eq!(volume.info().unwrap(), info);
        assert!(
            volume
                .device
                .as_ref()
                .unwrap()
                .media
                .borrow()
                .events
                .is_empty()
        );
        metrics.record("partial-tail-no-space", &mut volume);

        if cycle % 2 == 0 {
            // Whole-unit shrink needs no allocation; once maintenance frees
            // those units, the rejected partial-tail rewrite succeeds.
            let half = size / 2 / UNIT as u64 * UNIT as u64;
            volume.truncate(&file, half).unwrap();
            metrics.record("truncate", &mut volume);
            metrics.drain(&mut volume);
            volume.truncate(&file, half - 1).unwrap();
            metrics.record("partial-tail", &mut volume);
            verify_bytes(&mut volume, &file, half - 1, byte);
            volume.truncate(&file, 0).unwrap();
            metrics.record("truncate", &mut volume);
            assert_eq!(volume.read(&file, 0, &mut [0]).unwrap(), 0);
        }
        volume
            .remove(ROOT, &name("filler"), RemoveKind::File)
            .unwrap();
        metrics.record("remove", &mut volume);
        if cycle % 2 == 1 {
            verify_bytes(&mut volume, &file, size, byte);
            volume.sync().unwrap();
            metrics.record("sync-pinned", &mut volume);
            assert_eq!(volume.handle_attributes(&file).unwrap().size, size);
        }
        volume.close_handle(file).unwrap();
        metrics.record("close", &mut volume);
        for spelling in small.keys() {
            volume
                .remove(ROOT, &name(spelling), RemoveKind::File)
                .unwrap();
            metrics.record("remove", &mut volume);
        }
        metrics.drain(&mut volume);
        let root = volume.root.clone().unwrap();
        let summary = crate::check::validate(volume.device.as_mut().unwrap(), &root).unwrap();
        assert_eq!(summary.objects, 1);
        assert_eq!(summary.data_blocks, 0);
        assert_eq!(counts(&mut volume), [0; 3]);
        metrics.output.flush().unwrap();
        eprintln!(
            "{layout} cycle {cycle}: filled {size} bytes, drained; metadata={}, slots={}, raw={}",
            metrics.pages,
            metrics.slots,
            raw_free(&mut volume)
        );
    }
    eprintln!(
        "{layout}: {} operations; minimum workspace {}; maximum metadata growth {:?}",
        metrics.operations, metrics.minimum_slots, metrics.maxima
    );
}

#[test]
#[ignore = "ten sequential foreground full-disk cycles"]
fn sequential_full_disk_cycles() {
    full_disk_cycles(false);
}

#[test]
#[ignore = "ten fragmented foreground full-disk cycles"]
fn fragmented_full_disk_cycles() {
    full_disk_cycles(true);
}
