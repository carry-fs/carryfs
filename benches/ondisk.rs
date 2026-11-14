//! In-process benchmark of the on-disk encodings every adapter shares,
//! the Linux kernel module included: page validation, in-page search,
//! name checks and checksums.
//!
//! Its pages come from a volume the core fills on a RAM-backed device.
//! Run with `cargo bench --bench ondisk`; the report is the median, lowest
//! and highest of nine samples per step.

use std::{cell::RefCell, hint::black_box, rc::Rc, time::Instant};

use carryfs_core::{BlockDevice, Geometry, MountMode, Name, ROOT, Result, Volume};
use carryfs_ondisk::crc::{crc32c, data_unit_crc};
use carryfs_ondisk::format::{BLOCK_BYTES, TreeKind, TreeRef, decode_page};
use carryfs_ondisk::name::{Scratch, parse, validate_disk_name};

const BYTES: u64 = 256 * 1024 * 1024;
const BLOCKS: u64 = BYTES / BLOCK_BYTES as u64;
const FILES: usize = 10_000;

#[derive(Clone)]
struct Ram(Rc<RefCell<Vec<u8>>>);

impl BlockDevice for Ram {
    fn mode(&self) -> MountMode {
        MountMode::ReadWrite
    }
    fn geometry(&mut self) -> Result<Geometry> {
        Ok(Geometry {
            byte_len: BYTES,
            logical_sector_bytes: 4096,
            physical_sector_bytes: 4096,
        })
    }
    fn read_exact_at(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
        let at = offset as usize;
        out.copy_from_slice(&self.0.borrow()[at..at + out.len()]);
        Ok(())
    }
    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        let at = offset as usize;
        self.0.borrow_mut()[at..at + data.len()].copy_from_slice(data);
        Ok(())
    }
    fn flush_durable(&mut self) -> Result<()> {
        Ok(())
    }
}

/// A valid tree page found in the image, with what `decode_page` expects of it.
struct Found {
    page: Vec<u8>,
    reference: TreeRef,
    uuid: [u8; 16],
    kind: TreeKind,
}

impl Found {
    fn decode(&self) -> carryfs_ondisk::format::PageView<'_> {
        decode_page(
            &self.page,
            self.reference,
            &self.uuid,
            self.kind,
            self.reference.generation,
            None,
            BLOCKS,
        )
        .expect("a harvested page decodes")
    }
}

/// Every block that decodes as a tree page, superseded versions included.
fn harvest(image: &[u8]) -> Vec<Found> {
    let u64_at = |page: &[u8], at: usize| u64::from_le_bytes(page[at..at + 8].try_into().unwrap());
    let mut found = Vec::new();
    for page in image.as_chunks::<BLOCK_BYTES>().0 {
        let kind = match page[13] {
            1 => TreeKind::Catalog,
            2 => TreeKind::Allocation,
            3 => TreeKind::Cleanup,
            4 => TreeKind::Workspace,
            _ => continue,
        };
        let candidate = Found {
            page: page.to_vec(),
            reference: TreeRef {
                block: u64_at(page, 16),
                generation: u64_at(page, 24),
            },
            uuid: page[40..56].try_into().unwrap(),
            kind,
        };
        let valid = decode_page(
            &candidate.page,
            candidate.reference,
            &candidate.uuid,
            kind,
            candidate.reference.generation,
            None,
            BLOCKS,
        )
        .is_ok();
        if valid {
            found.push(candidate);
        }
    }
    found
}

/// Time `work`, which performs `calls` operations, nine times.
fn step(label: &str, unit: &str, calls: usize, quick: bool, mut work: impl FnMut()) {
    let mut samples: Vec<f64> = (0..if quick { 1 } else { 9 })
        .map(|_| {
            let start = Instant::now();
            work();
            start.elapsed().as_secs_f64() * 1e9 / calls as f64
        })
        .collect();
    samples.sort_by(f64::total_cmp);
    println!(
        "{label:<34} {:>10.1} {:>10.1} {:>10.1}  {unit}",
        samples[samples.len() / 2],
        samples[0],
        samples[samples.len() - 1]
    );
}

fn main() -> Result<()> {
    // `cargo test --all-targets` runs this without `--bench`: one smoke pass.
    let quick = !std::env::args().any(|arg| arg == "--bench");
    let ram = Ram(Rc::new(RefCell::new(vec![0; BYTES as usize])));
    let mut volume = Volume::format(ram.clone())?;
    volume.set_deferred_checkpoints(true)?;
    let page = vec![0x5a; 4096];
    for i in 0..FILES {
        let file = volume.create_file(ROOT, Name::parse(&format!("file-{i:06}.txt"))?)?;
        volume.write(&file, 0, &page)?;
        volume.close_handle(file)?;
    }
    volume.eject()?;

    let image = ram.0.borrow();
    let pages = harvest(&image);
    let catalog = |leaf: bool| {
        pages
            .iter()
            .filter(move |found| found.kind == TreeKind::Catalog && (found.page[14] == 0) == leaf)
    };
    let leaves: Vec<&Found> = catalog(true).collect();
    let branches: Vec<&Found> = catalog(false).collect();
    let records = |pages: &[&Found]| {
        pages
            .iter()
            .map(|found| found.decode().len())
            .sum::<usize>()
    };
    println!(
        "catalog pages: {} leaves ({} records), {} branches ({} records)",
        leaves.len(),
        records(&leaves),
        branches.len(),
        records(&branches)
    );
    println!(
        "{:<34} {:>10} {:>10} {:>10}",
        "step", "median", "min", "max"
    );

    for (label, set) in [
        ("decode leaf page", &leaves),
        ("decode branch page", &branches),
    ] {
        step(label, "ns/page", set.len(), quick, || {
            for found in set.iter() {
                black_box(black_box(found).decode().len());
            }
        });
    }

    // One search per record of each page, as a lookup makes at each level.
    for (label, set) in [
        ("search leaf page", &leaves),
        ("search branch page", &branches),
    ] {
        let views: Vec<_> = set.iter().map(|found| found.decode()).collect();
        let mut offsets = [0u16; 1024];
        step(label, "ns/search", records(set), quick, || {
            for view in &views {
                for (key, _) in view.records() {
                    black_box(view.floor_record(black_box(key), <[u8]>::cmp, &mut offsets));
                }
            }
        });
    }

    let mut scratch = Box::new(Scratch::new());
    let names: Vec<(&str, Vec<String>)> = vec![
        (
            "15-byte ASCII",
            (0..1000).map(|i| format!("File-{i:06}.txt")).collect(),
        ),
        (
            "42-byte ASCII",
            (0..1000)
                .map(|i| format!("Holiday-Photographs-2026-{i:06}-edited.jpeg"))
                .collect(),
        ),
        (
            "mixed Unicode",
            (0..1000)
                .map(|i| format!("Fotos-Zürich-Ελλάδα-{i:06}.jpeg"))
                .collect(),
        ),
    ];
    for (label, names) in &names {
        step(
            &format!("parse name, {label}"),
            "ns/name",
            names.len(),
            quick,
            || {
                for name in names {
                    black_box(parse(black_box(name), &mut scratch).unwrap().folded.len());
                }
            },
        );
        let stored: Vec<(Vec<u8>, Vec<u8>)> = names
            .iter()
            .map(|name| {
                let parsed = parse(name, &mut scratch).unwrap();
                (parsed.stored.to_vec(), parsed.folded.to_vec())
            })
            .collect();
        step(
            &format!("validate name, {label}"),
            "ns/name",
            stored.len(),
            quick,
            || {
                for (stored, folded) in &stored {
                    validate_disk_name(black_box(stored), black_box(folded), &mut scratch).unwrap();
                }
            },
        );
    }

    let data = vec![0xa5u8; 1024 * 1024];
    for (label, bytes, calls) in [
        ("crc32c 4 KiB", 4096, 20_000),
        ("crc32c 1 MiB", data.len(), 100),
    ] {
        step(label, "ns/call", calls, quick, || {
            for _ in 0..calls {
                black_box(crc32c(black_box(&data[..bytes])));
            }
        });
    }
    step("data unit crc 64 KiB", "ns/call", 2000, quick, || {
        for unit in 0..2000 {
            black_box(data_unit_crc(&[7; 16], 9, unit, black_box(&data[..65536])).unwrap());
        }
    });
    Ok(())
}
