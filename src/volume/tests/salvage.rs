//! What `extract_to` recovers from a populated image after damage to one
//! metadata class or one data unit.

use super::*;
use crate::format::{TreeKind, TreeRef, decode_page};

// Enough 4 KiB files for a three-level catalog, which the damage cases need
// (root, internal page, leaf). Compact checksum groups pack many small files
// per leaf; 30 directories is the measured minimum.
const DIRS: u8 = 32;
const FILES: u8 = 30;

/// `FILES` one-unit files in each of `DIRS` directories plus `big`, which spans
/// three units.
type Files = Vec<(String, Vec<u8>)>;

fn populated() -> (BTreeMap<u64, Page>, Files) {
    let mut volume = opened();
    let mut expected = Vec::new();
    for dir in 0..DIRS {
        let directory = volume
            .create_directory(ROOT, name(&format!("d{dir}")))
            .unwrap();
        for file in 0..FILES {
            let bytes = vec![((u16::from(dir) * 30 + u16::from(file)) % 251 + 1) as u8; 4096];
            let handle = volume
                .create_file(directory, name(&format!("f{file}")))
                .unwrap();
            volume.write(&handle, 0, &bytes).unwrap();
            volume.close_handle(handle).unwrap();
            expected.push((format!("d{dir}/f{file}"), bytes));
        }
    }
    let big: Vec<u8> = (0..3 * UNIT_BYTES)
        .map(|i| (i / UNIT_BYTES) as u8 + 0xFC)
        .collect();
    let handle = volume.create_file(ROOT, name("big")).unwrap();
    volume.write(&handle, 0, &big).unwrap();
    volume.close_handle(handle).unwrap();
    expected.push(("big".into(), big));
    volume.sync().unwrap();
    (durable(&volume), expected)
}

/// The blocks of one tree, root first and then the first page at each lower level.
fn first_path(image: &BTreeMap<u64, Page>, root: TreeRef, kind: TreeKind, blocks: u64) -> Vec<u64> {
    let mut path = Vec::new();
    let mut reference = root;
    loop {
        path.push(reference.block);
        let page = decode_page(
            &image[&reference.block].0[..],
            reference,
            &UUID,
            kind,
            reference.generation,
            None,
            blocks,
        )
        .unwrap();
        if page.level == 0 {
            return path;
        }
        let value = page.records[0].1;
        reference = TreeRef {
            block: u64::from_le_bytes(value[..8].try_into().unwrap()),
            generation: u64::from_le_bytes(value[8..16].try_into().unwrap()),
        };
    }
}

/// The first block of the unit that holds exactly `byte` repeated.
fn block_of(image: &BTreeMap<u64, Page>, byte: u8) -> u64 {
    *image
        .iter()
        .find(|(_, page)| page.0[..] == [byte; BLOCK_BYTES])
        .unwrap()
        .0
}

struct Outcome {
    opened: String,
    objects: u64,
    unreadable: u64,
    complete: bool,
    recovered: usize,
}

/// Open read-only and extract everything, then count the files that came out
/// byte-identical.
fn salvage(image: BTreeMap<u64, Page>, expected: &[(String, Vec<u8>)]) -> Outcome {
    let mut outcome = Outcome {
        opened: "ok".into(),
        objects: 0,
        unreadable: 0,
        complete: false,
        recovered: 0,
    };
    let mut reader = match Volume::open(FaultDevice::new(image, MountMode::ReadOnly)) {
        Ok(reader) => reader,
        Err(error) => {
            outcome.opened = format!("{error:?}");
            return outcome;
        }
    };
    let mut output = RecoveryOutput::new();
    let summary = reader.extract_to(ROOT, &mut output, &mut |_| {}).unwrap();
    outcome.objects = summary.objects;
    outcome.unreadable = summary.unreadable_ranges;
    outcome.complete = summary.complete;
    outcome.recovered = expected
        .iter()
        .filter(|(path, bytes)| {
            std::fs::read(output.root.join(path)).is_ok_and(|got| got == *bytes)
        })
        .count();
    outcome
}

fn flip(image: &mut BTreeMap<u64, Page>, block: u64) {
    image.get_mut(&block).unwrap().0[64] ^= 1;
}

/// One flipped byte in one page. Extraction preflights the whole structure, so
/// damage to any page a root references recovers nothing; damage to the
/// inactive workspace bank, which no root references, changes nothing; damage
/// to a data unit costs only that unit.
#[test]
fn extraction_recovers_nothing_from_damaged_referenced_metadata_and_all_but_the_damaged_unit() {
    let (image, expected) = populated();
    let total = expected.len();
    let root = Volume::open(FaultDevice::new(image.clone(), MountMode::ReadOnly))
        .unwrap()
        .root
        .clone()
        .unwrap();
    let catalog = first_path(&image, root.catalog, TreeKind::Catalog, root.blocks);
    assert_eq!(catalog.len(), 3, "fixture needs a three-level catalog");
    let inactive = if root.workspace.block == 32 {
        root.blocks - 96
    } else {
        32
    };
    // (class, block, files recovered, unreadable ranges, extraction complete)
    let cases = [
        ("no damage", None, total, 0, true),
        ("catalog root", Some(catalog[0]), 0, 0, false),
        ("catalog internal page", Some(catalog[1]), 0, 0, false),
        ("catalog leaf", Some(catalog[2]), 0, 0, false),
        ("allocation", Some(root.allocation.block), 0, 0, false),
        ("cleanup", Some(root.cleanup.block), 0, 0, false),
        (
            "workspace, active bank",
            Some(root.workspace.block),
            0,
            0,
            false,
        ),
        ("workspace, inactive bank", Some(inactive), total, 0, true),
        (
            "data, a one-unit file",
            Some(block_of(&image, 98)),
            total - 1,
            1,
            true,
        ),
        (
            "data, middle unit of a 3-unit file",
            Some(block_of(&image, 0xFD)),
            total - 1,
            1,
            true,
        ),
    ];
    for (class, block, recovered, unreadable, complete) in cases {
        let mut damaged = image.clone();
        if let Some(block) = block {
            flip(&mut damaged, block);
        }
        let outcome = salvage(damaged, &expected);
        eprintln!(
            "| {class} | {} | {}/{total} | {} |",
            if outcome.opened == "ok" {
                "opens"
            } else {
                "open fails"
            },
            outcome.recovered,
            outcome.unreadable
        );
        assert_eq!(
            (outcome.recovered, outcome.unreadable, outcome.complete),
            (recovered, unreadable, complete),
            "{class}"
        );
        if recovered == 0 {
            assert_eq!(outcome.objects, 0, "{class}");
        }
    }
}
