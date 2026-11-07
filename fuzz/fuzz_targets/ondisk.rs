#![no_main]
//! The carryfs-ondisk entry points only the kernel driver calls, with the
//! properties it relies on: a page `decode_page` accepts reads the same
//! through `verified_page_view` (the driver's path for pages verified
//! earlier), `floor_record` agrees with a linear scan, an ASCII name parses
//! the same through `parse_ascii` as through `parse_unicode` (the dcache
//! hash's fast path), a stored name recomputes to its key, and a data
//! unit's CRC does not depend on how its bytes are split.

use carryfs_ondisk::crc::{DataUnitCrc, UNIT_BYTES};
use carryfs_ondisk::format::{
    self, BLOCK_BYTES, TreeKind, TreeRef, decode_page, verified_page_view,
};
use carryfs_ondisk::name::{self, MAX_STORED_BYTES, Scratch};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    match mode % 3 {
        0 => page(rest),
        1 => names(rest),
        _ => unit_crc(rest),
    }
});

fn le_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// A page from the input, sealed so its header and checksum are valid and
/// decoding reaches record validation, as the driver reads pages it found
/// through a parent's reference.
fn page(data: &[u8]) {
    if data.len() < BLOCK_BYTES {
        return;
    }
    let (bytes, key) = data.split_at(BLOCK_BYTES);
    let mut page = [0u8; BLOCK_BYTES];
    page.copy_from_slice(bytes);
    let kind = match page[13] % 4 {
        0 => TreeKind::Catalog,
        1 => TreeKind::Allocation,
        2 => TreeKind::Cleanup,
        _ => TreeKind::Workspace,
    };
    let reference = TreeRef {
        block: le_u64(&page, 16),
        generation: le_u64(&page, 24),
    };
    let uuid: [u8; 16] = page[40..56].try_into().unwrap();
    let data_blocks = if kind == TreeKind::Catalog {
        le_u64(&page, 56)
    } else {
        0
    };
    format::seal_page(&mut page, reference, &uuid, kind, data_blocks);
    let generation = reference.generation;
    let Ok(view) = decode_page(&page, reference, &uuid, kind, generation, None, u64::MAX) else {
        return;
    };
    let again = verified_page_view(&page, reference, &uuid, kind, generation, None)
        .expect("a decoded page reads again as verified");
    assert_eq!(
        (view.level, view.data_blocks, view.len()),
        (again.level, again.data_blocks, again.len())
    );
    let records: Vec<_> = view.records().collect();
    assert_eq!(records.len(), view.len(), "records yields the page's count");
    assert!(
        again.records().eq(records.iter().copied()),
        "both views yield the same records"
    );

    let mut keys: Vec<&[u8]> = records.iter().map(|(key, _)| *key).collect();
    keys.push(key);
    keys.push(&[]);
    let mut offsets = [0u16; 160];
    for probe in keys {
        // The last record whose key is at most the probe.
        let linear = records.iter().rposition(|(key, _)| *key <= probe);
        let searched = view.floor_record(probe, |a, b| a.cmp(b), &mut offsets);
        let scanned = view.floor_record(probe, |a, b| a.cmp(b), &mut []);
        assert_eq!(
            searched.as_ref().map(|found| found.0),
            linear,
            "binary search agrees with a scan"
        );
        assert_eq!(
            scanned.as_ref().map(|found| found.0),
            linear,
            "linear search agrees with a scan"
        );
        if let Some((index, rest)) = searched {
            assert!(
                rest.eq(records[index..].iter().copied()),
                "the records from the floor on"
            );
        }
    }
}

fn names(data: &[u8]) {
    let Ok(text) = core::str::from_utf8(data) else {
        return;
    };
    let mut scratch = Scratch::new();
    let unicode = name::parse_unicode(text, &mut scratch)
        .map(|parsed| (parsed.stored.to_vec(), parsed.folded.to_vec()));
    if text.is_ascii() && text.len() <= MAX_STORED_BYTES {
        let mut folded = [0u8; MAX_STORED_BYTES];
        let ascii = name::parse_ascii(text, &mut folded[..text.len()])
            .map(|parsed| (parsed.stored.to_vec(), parsed.folded.to_vec()));
        assert_eq!(
            ascii.is_ok(),
            unicode.is_ok(),
            "both parsers accept the same ASCII names"
        );
        if let (Ok(ascii), Ok(unicode)) = (&ascii, &unicode) {
            assert_eq!(
                ascii, unicode,
                "an ASCII name folds the same through both parsers"
            );
        }
    }
    let Ok((stored, folded)) = unicode else {
        return;
    };
    // On-disk names must recompute to their key (FORMAT.md section 7).
    let stored = core::str::from_utf8(&stored).expect("a stored name is UTF-8");
    let again = name::parse_unicode(stored, &mut scratch).expect("a stored name parses again");
    assert_eq!(
        again.stored,
        stored.as_bytes(),
        "a stored name is its own NFC spelling"
    );
    assert_eq!(
        again.folded,
        &folded[..],
        "a stored name recomputes to its key"
    );
}

fn unit_crc(data: &[u8]) {
    if data.len() < 32 {
        return;
    }
    let (header, bytes) = data.split_at(32);
    let uuid: [u8; 16] = header[..16].try_into().unwrap();
    let (object, unit) = (le_u64(header, 16), le_u64(header, 24));
    let bytes = &bytes[..bytes.len().min(UNIT_BYTES)];
    let Ok(mut whole) = DataUnitCrc::new(&uuid, object, unit, bytes.len()) else {
        assert!(bytes.is_empty(), "only an empty unit is rejected");
        return;
    };
    whole.update(bytes);
    let whole = whole.finish().expect("the whole unit was given");
    let mut split = DataUnitCrc::new(&uuid, object, unit, bytes.len()).unwrap();
    let mut rest = bytes;
    while !rest.is_empty() {
        let take = (usize::from(rest[0]) % 7 + 1).min(rest.len());
        let (piece, tail) = rest.split_at(take);
        split.update(piece);
        rest = tail;
    }
    assert_eq!(
        split.finish().unwrap(),
        whole,
        "the CRC does not depend on the split"
    );
    let mut short = DataUnitCrc::new(&uuid, object, unit, bytes.len()).unwrap();
    short.update(&bytes[..bytes.len() - 1]);
    assert!(short.finish().is_err(), "a unit given short is rejected");
}
