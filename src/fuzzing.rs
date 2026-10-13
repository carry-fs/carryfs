//! Entry points for bounded verification, not filesystem operations.

use crate::{
    Geometry,
    format::{self, Page, Root, TreeKind, TreeRef},
};

const GEOMETRY: Geometry = Geometry {
    byte_len: 256 * 1024 * 1024,
    logical_sector_bytes: 4096,
    physical_sector_bytes: 4096,
};

/// Exercise arbitrary bytes, then checksum-correct envelopes to reach deeper validation.
pub fn parsers(data: &[u8]) {
    let split = data
        .first()
        .map_or(0, |byte| usize::from(*byte))
        .min(data.len());
    let _ = crate::name::validate_disk_name(&data[..split], &data[split..]);
    check_root(data);
    check_page(data);
    if data.len() == 4096 {
        let mut page = Page(Box::new([0; 4096]));
        page.0.copy_from_slice(data);
        page.0[..8].copy_from_slice(b"CFSXROOT");
        page.0[4092..].fill(0);
        let crc = format::crc32c(&page.0[..]);
        page.0[4092..].copy_from_slice(&crc.to_le_bytes());
        check_root(&page.0[..]);

        page.0.copy_from_slice(data);
        page.0[..8].copy_from_slice(b"CFSXPAGE");
        page.0[64..68].fill(0);
        let crc = format::crc32c(&page.0[..]);
        page.0[64..68].copy_from_slice(&crc.to_le_bytes());
        check_page(&page.0[..]);
    }
}

fn check_root(data: &[u8]) {
    if let Ok(root) = Root::decode(data, 256, GEOMETRY) {
        assert_eq!(
            root.encode(256, GEOMETRY)
                .expect("decoded root must encode")
                .0[..],
            *data
        );
    }
}

fn check_page(data: &[u8]) {
    if data.len() != 4096 {
        return;
    }
    let block = u64::from_le_bytes(data[16..24].try_into().expect("fixed header"));
    let generation = u64::from_le_bytes(data[24..32].try_into().expect("fixed header"));
    let uuid: [u8; 16] = data[40..56].try_into().expect("fixed header");
    let kind = match data[13] {
        1 => TreeKind::Catalog,
        2 => TreeKind::Allocation,
        4 => TreeKind::Workspace,
        _ => TreeKind::Cleanup,
    };
    let reference = TreeRef { block, generation };
    if let Ok(decoded) = format::decode_page(
        data,
        reference,
        &uuid,
        kind,
        generation,
        None,
        GEOMETRY.blocks(),
    ) {
        let encoded = format::encode_node(
            reference,
            &uuid,
            kind,
            &decoded.records,
            GEOMETRY.blocks(),
            decoded.level,
            decoded.data_blocks,
        )
        .expect("decoded page must encode");
        if kind == TreeKind::Workspace {
            let mut root = Root::decode(
                include_bytes!("../testdata/vectors/root-a.bin"),
                256,
                GEOMETRY,
            )
            .expect("workspace fuzz envelope");
            root.workspace = reference;
            root.generation = generation;
            root.uuid = uuid;
            let _ = crate::workspace::read_with(&root, |_| Ok(encoded.clone()));
        }
        assert_eq!(encoded.0[..], *data);
    }
}

/// Exercise at most 16 crash/reopen cycles, including attempts to use faulted volumes.
pub fn commits(data: &[u8]) {
    crate::volume::model::fuzz(data);
}

/// Exercise bounded tree edits and real allocation/reclamation crash transitions.
pub fn storage(data: &[u8]) {
    crate::tree::fuzz(data);
    crate::volume::model::fuzz_maintenance(data);
    crate::volume::model::fuzz_batch(data);
}

/// Compare bounded file operations, open lifetimes, and crash recovery with a byte oracle.
pub fn files(data: &[u8]) {
    crate::volume::model::fuzz_files(data);
}
