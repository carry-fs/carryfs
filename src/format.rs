//! Userspace views of the encodings in `carryfs_ondisk::format`: boxed pages,
//! record vectors, device geometry, and the public label type.

use carryfs_ondisk::format as ondisk;

pub(crate) use carryfs_ondisk::crc::UNIT_BYTES;
#[cfg(any(test, feature = "fuzzing"))]
pub(crate) use carryfs_ondisk::crc::crc32c;
pub(crate) use carryfs_ondisk::format::{
    BLOCK_BYTES, CHECKSUM_GROUP_BYTES, REVISION, ROOT_MAGIC, TreeKind, TreeRef, Validation,
    all_zero, compact_group, expand_group, reserved_group_blocks, set_checksum_slot,
};

use crate::device::Geometry;
use crate::error::{Error, Result};

// Deliberately not #[repr(align(4096))]: that alignment corrupted memory in
// optimized x86-64 Linux builds. Direct I/O backends must allocate their own
// aligned buffers.
#[derive(Clone, Debug, PartialEq, Eq)]
// Boxed so a queued or published page moves as a pointer, not 4 KiB.
pub(crate) struct Page(pub Box<[u8; BLOCK_BYTES]>);

impl Page {
    /// A page holding `bytes`, at most one page, followed by zeros. A full
    /// page is copied once, without zeroing first.
    pub(crate) fn from_prefix(bytes: &[u8]) -> Self {
        match <Box<[u8; BLOCK_BYTES]>>::try_from(Box::<[u8]>::from(bytes)) {
            Ok(page) => Self(page),
            Err(bytes) => {
                let mut page = Self::zeroed();
                page.0[..bytes.len()].copy_from_slice(&bytes);
                page
            }
        }
    }

    fn zeroed() -> Self {
        Self(Box::new([0; BLOCK_BYTES]))
    }
}

// The shared checks report `carryfs_ondisk` errors; these convert them.

pub(crate) fn data_unit_crc(uuid: &[u8; 16], object: u64, unit: u64, valid: &[u8]) -> Result<u32> {
    Ok(carryfs_ondisk::crc::data_unit_crc(
        uuid, object, unit, valid,
    )?)
}

pub(crate) fn checksum_slot(group: &[u8], slot: usize) -> Result<Option<u32>> {
    Ok(ondisk::checksum_slot(group, slot)?)
}

pub(crate) fn validate_symlink_target(target: &[u8], mode: Validation) -> Result<()> {
    Ok(ondisk::validate_symlink_target(target, mode)?)
}

pub(crate) fn validate_timestamp_range(seconds: i64, nanos: u32, mode: Validation) -> Result<()> {
    Ok(ondisk::validate_timestamp_range(seconds, nanos, mode)?)
}

pub(crate) fn leaf_record_blocks(
    kind: TreeKind,
    key: &[u8],
    value: &[u8],
    blocks: u64,
    generation: u64,
) -> Result<u64> {
    Ok(ondisk::leaf_record_blocks(
        kind, key, value, blocks, generation,
    )?)
}

pub(crate) fn leaf_data_blocks(
    reference: TreeRef,
    uuid: &[u8; 16],
    kind: TreeKind,
    records: &[(&[u8], &[u8])],
    blocks: u64,
) -> Result<u64> {
    Ok(ondisk::leaf_data_blocks(
        reference, uuid, kind, records, blocks,
    )?)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Root {
    pub label: Option<crate::VolumeLabel>,
    pub uuid: [u8; 16],
    pub generation: u64,
    pub blocks: u64,
    pub catalog: TreeRef,
    pub allocation: TreeRef,
    pub cleanup: TreeRef,
    pub next_object: u64,
    pub free_blocks: u64,
    pub next_job: u64,
    pub workspace: TreeRef,
    pub workspace_cursor: u64,
}

fn geometry_blocks(geometry: &Geometry) -> Result<u64> {
    geometry.validate()?;
    let blocks = geometry.blocks();
    if blocks < ondisk::RESERVED_TOTAL_BLOCKS {
        return Err(Error::InvalidGeometry);
    }
    Ok(blocks)
}

impl Root {
    pub(crate) fn decode(bytes: &[u8], slot: u64, geometry: Geometry) -> Result<Self> {
        let blocks = geometry_blocks(&geometry)?;
        let root = ondisk::Root::decode(bytes, slot, blocks)?;
        let label = root
            .label
            .map(|label| crate::VolumeLabel::parse(label.as_str()))
            .transpose()
            .map_err(|_| Error::Corrupt("invalid root label"))?;
        Ok(Self {
            label,
            uuid: root.uuid,
            generation: root.generation,
            blocks: root.blocks,
            catalog: root.catalog,
            allocation: root.allocation,
            cleanup: root.cleanup,
            next_object: root.next_object,
            free_blocks: root.free_blocks,
            next_job: root.next_job,
            workspace: root.workspace,
            workspace_cursor: root.workspace_cursor,
        })
    }

    pub(crate) fn encode(&self, slot: u64, geometry: Geometry) -> Result<Page> {
        let blocks = geometry_blocks(&geometry)?;
        let label = self
            .label
            .as_ref()
            .map(|label| ondisk::Label::new(label.as_str().as_bytes()))
            .transpose()?;
        let root = ondisk::Root {
            label,
            uuid: self.uuid,
            generation: self.generation,
            blocks: self.blocks,
            catalog: self.catalog,
            allocation: self.allocation,
            cleanup: self.cleanup,
            next_object: self.next_object,
            free_blocks: self.free_blocks,
            next_job: self.next_job,
            workspace: self.workspace,
            workspace_cursor: self.workspace_cursor,
        };
        let mut page = Page::zeroed();
        root.encode(slot, blocks, &mut page.0)?;
        Ok(page)
    }
}

pub(crate) struct DecodedPage<'a> {
    pub level: u8,
    pub data_blocks: u64,
    pub records: Vec<(&'a [u8], &'a [u8])>,
}

pub(crate) fn decode_page<'a>(
    bytes: &'a [u8],
    expected: TreeRef,
    uuid: &[u8; 16],
    kind: TreeKind,
    parent_generation: u64,
    expected_level: Option<u8>,
    blocks: u64,
) -> Result<DecodedPage<'a>> {
    let view = ondisk::decode_page(
        bytes,
        expected,
        uuid,
        kind,
        parent_generation,
        expected_level,
        blocks,
    )?;
    Ok(DecodedPage {
        level: view.level,
        data_blocks: view.data_blocks,
        records: view.records().collect(),
    })
}

pub(crate) fn encode_leaf(
    reference: TreeRef,
    uuid: &[u8; 16],
    kind: TreeKind,
    records: &[(&[u8], &[u8])],
    blocks: u64,
) -> Result<Page> {
    encode_node(reference, uuid, kind, records, blocks, 0, 0)
}

pub(crate) fn encode_node(
    reference: TreeRef,
    uuid: &[u8; 16],
    kind: TreeKind,
    records: &[(&[u8], &[u8])],
    blocks: u64,
    level: u8,
    child_data_blocks: u64,
) -> Result<Page> {
    let mut page = Page::zeroed();
    ondisk::encode_node(
        &mut page.0,
        reference,
        uuid,
        kind,
        records,
        blocks,
        level,
        child_data_blocks,
    )?;
    Ok(page)
}

#[cfg(test)]
mod tests {
    use carryfs_ondisk::format::{
        ALLOCATION_GROUP_BLOCKS, MAX_ID, MAX_LOGICAL_BLOCK, MAX_SYMLINK_TARGET, RESERVED_BLOCKS,
        RESERVED_TOTAL_BLOCKS, read_u16_le, read_u32_le, validate_checksum_group,
        validate_directory_entry, validate_extent, validate_inode, validate_owner_id,
    };

    use super::*;

    #[test]
    fn internal_page_round_trip_preserves_child_references() {
        let reference = TreeRef {
            block: 1024,
            generation: 2,
        };
        let keys = [[3, 0, 0, 0, 0, 0, 0, 0, 1], [3, 0, 0, 0, 0, 0, 0, 0, 2]];
        let values = [1025u64, 1026].map(|block| {
            let mut value = [0; 16];
            value[..8].copy_from_slice(&block.to_le_bytes());
            value[8..].copy_from_slice(&1u64.to_le_bytes());
            value
        });
        let records = [
            (keys[0].as_slice(), values[0].as_slice()),
            (keys[1].as_slice(), values[1].as_slice()),
        ];
        let page = encode_node(
            reference,
            &UUID,
            TreeKind::Cleanup,
            &records,
            GEOMETRY.blocks(),
            1,
            0,
        )
        .unwrap();
        let decoded = decode_page(
            &page.0[..],
            reference,
            &UUID,
            TreeKind::Cleanup,
            2,
            Some(1),
            GEOMETRY.blocks(),
        )
        .unwrap();
        assert_eq!(decoded.records, records);
        assert_eq!(decoded.level, 1);
        assert_eq!(
            encode_node(
                reference,
                &UUID,
                TreeKind::Cleanup,
                &decoded.records,
                GEOMETRY.blocks(),
                decoded.level,
                decoded.data_blocks
            )
            .unwrap(),
            page
        );
    }

    const GEOMETRY: Geometry = Geometry {
        byte_len: 256 * 1024 * 1024,
        logical_sector_bytes: 4096,
        physical_sector_bytes: 4096,
    };
    const UUID: [u8; 16] = [0x42; 16];

    fn root_a() -> Root {
        Root {
            label: None,
            uuid: [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ],
            generation: 7,
            blocks: GEOMETRY.blocks(),
            catalog: TreeRef {
                block: 1024,
                generation: 5,
            },
            allocation: TreeRef {
                block: 1025,
                generation: 7,
            },
            cleanup: TreeRef {
                block: 1026,
                generation: 7,
            },
            next_object: 18,
            free_blocks: 50_000,
            next_job: 9,
            workspace: TreeRef {
                block: 32,
                generation: 7,
            },
            workspace_cursor: 0,
        }
    }

    fn root_crc(page: &mut [u8; BLOCK_BYTES]) {
        page[4092..].fill(0);
        let crc = crc32c(page);
        page[4092..].copy_from_slice(&crc.to_le_bytes());
    }

    fn page_crc(page: &mut [u8; BLOCK_BYTES]) {
        page[64..68].fill(0);
        let crc = crc32c(page);
        page[64..68].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn data_unit_crcs_match_the_published_vectors() {
        let json = include_str!("../testdata/checksum-vectors.json");
        let field = |object: &str, name: &str| -> String {
            let start = object.find(&format!("\"{name}\": ")).expect(name) + name.len() + 4;
            object[start..]
                .split([',', '\n', '}'])
                .next()
                .unwrap()
                .trim()
                .trim_matches('"')
                .to_owned()
        };
        let hex = |text: &str| -> Vec<u8> {
            (0..text.len())
                .step_by(2)
                .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
                .collect()
        };
        let uuid: [u8; 16] = hex(&field(json, "uuid_hex")).try_into().unwrap();
        let units = &json[json.find("\"data_units\"").unwrap()..];
        let mut checked = 0;
        for object in units.split('{').skip(1) {
            let valid = field(object, "valid_bytes").parse::<usize>().unwrap();
            let payload = match field(object, "payload").as_str() {
                "zeros" => vec![0; valid],
                _ => hex(&field(object, "payload_hex")),
            };
            assert_eq!(payload.len(), valid);
            let expected = u32::from_str_radix(&field(object, "crc32c"), 16).unwrap();
            let actual = data_unit_crc(
                &uuid,
                field(object, "object_id").parse().unwrap(),
                field(object, "unit_index").parse().unwrap(),
                &payload,
            )
            .unwrap();
            assert_eq!(actual, expected, "{object}");
            checked += 1;
        }
        assert_eq!(checked, 5);
        assert!(data_unit_crc(&uuid, 2, 0, &[]).is_err());
        assert!(data_unit_crc(&uuid, 2, 0, &vec![0; UNIT_BYTES + 1]).is_err());
    }

    #[test]
    fn zero_checks_find_any_nonzero_byte() {
        for len in 0..=65 {
            let mut bytes = vec![0; len + 1];
            assert!(all_zero(&bytes[1..]));
            for index in 1..=len {
                for nonzero in [1, 128, 255] {
                    bytes[index] = nonzero;
                    assert!(!all_zero(&bytes[1..]), "len {len}, index {index}");
                }
                bytes[index] = 0;
            }
        }
    }

    #[test]
    fn checksum_slots_round_trip_through_group_validation() {
        let mut group = [0u8; CHECKSUM_GROUP_BYTES];
        let key = |index: u64| {
            let mut key = 2_u64.to_be_bytes().to_vec();
            key.push(4);
            key.extend_from_slice(&index.to_be_bytes());
            key
        };
        set_checksum_slot(&mut group, 0, Some(0));
        set_checksum_slot(&mut group, 255, Some(0xdead_beef));
        let stored = compact_group(&group);
        assert_eq!(stored.len(), CHECKSUM_GROUP_BYTES);
        validate_checksum_group(&key(0), stored, Validation::Disk).unwrap();
        assert_eq!(checksum_slot(stored, 0).unwrap(), Some(0));
        assert_eq!(checksum_slot(stored, 255).unwrap(), Some(0xdead_beef));
        assert_eq!(checksum_slot(stored, 1).unwrap(), None);
        assert!(checksum_slot(stored, 256).is_err());

        set_checksum_slot(&mut group, 255, None);
        let stored = compact_group(&group);
        assert_eq!(stored.len(), 36, "a one-unit group ends after slot 0");
        validate_checksum_group(&key(0), stored, Validation::Disk).unwrap();
        assert_eq!(checksum_slot(stored, 255).unwrap(), None);
        assert_eq!(expand_group(stored), group);
        set_checksum_slot(&mut group, 0, None);
        assert_eq!(group, [0u8; CHECKSUM_GROUP_BYTES]);
        assert!(validate_checksum_group(&key(0), compact_group(&group), Validation::Disk).is_err());
    }

    #[test]
    fn compact_groups_validate_exactly_their_canonical_form() {
        let mut key = 2_u64.to_be_bytes().to_vec();
        key.push(4);
        key.extend_from_slice(&0_u64.to_be_bytes());
        // FORMAT.md, slot by slot: the value ends at its last present slot,
        // no bitmap bit lies beyond it, and holes before it store CRC zero.
        let canonical = |value: &[u8]| {
            let present = |slot: usize| value[slot / 8] & (1 << (slot % 8)) != 0;
            let stored = (value.len() - 32) / 4;
            present(stored - 1)
                && (stored..256).all(|slot| !present(slot))
                && (0..stored)
                    .all(|slot| present(slot) || value[32 + slot * 4..36 + slot * 4] == [0; 4])
        };
        let valid = |value: &[u8]| validate_checksum_group(&key, value, Validation::Disk).is_ok();
        for stored in 1..=256usize {
            for mask in 0..=255u8 {
                let mut group = [0u8; CHECKSUM_GROUP_BYTES];
                group[(stored - 1) / 8] = mask;
                for slot in 0..stored {
                    if group[slot / 8] & (1 << (slot % 8)) != 0 {
                        group[32 + slot * 4] = 0xaa;
                    }
                }
                let value = &group[..32 + 4 * stored];
                let context = format!("stored {stored}, mask {mask:#04x}");
                assert_eq!(valid(value), canonical(value), "{context}");
                if let Some(hole) =
                    (0..stored).find(|slot| value[slot / 8] & (1 << (slot % 8)) == 0)
                {
                    let mut changed = value.to_vec();
                    changed[32 + hole * 4] = 1;
                    assert!(!valid(&changed), "{context}: CRC in hole {hole}");
                }
                if stored < 256 {
                    let mut changed = value.to_vec();
                    changed[stored / 8] |= 1 << (stored % 8);
                    assert!(!valid(&changed), "{context}: bit beyond the stored slots");
                }
                if canonical(value) {
                    assert_eq!(compact_group(&expand_group(value)), value, "{context}");
                }
            }
        }
        for len in [0, 32, 35, 37, 1_057, 1_060] {
            let mut value = vec![0u8; len];
            if let Some(first) = value.first_mut() {
                *first = 1;
            }
            assert!(!valid(&value), "length {len}");
        }
    }

    /// `reserved_group_blocks` names exactly the blocks a per-block scan of the
    /// group would: the first and last groups, a short final group, and the
    /// empty answer for every group between them.
    #[test]
    fn reserved_group_blocks_match_a_per_block_scan() {
        for blocks in [
            RESERVED_TOTAL_BLOCKS,
            ALLOCATION_GROUP_BLOCKS - 1,
            ALLOCATION_GROUP_BLOCKS,
            ALLOCATION_GROUP_BLOCKS + 1,
            3 * ALLOCATION_GROUP_BLOCKS + RESERVED_BLOCKS - 1,
            5 * ALLOCATION_GROUP_BLOCKS,
        ] {
            for group in 0..blocks.div_ceil(ALLOCATION_GROUP_BLOCKS) {
                let first = group * ALLOCATION_GROUP_BLOCKS;
                let scanned: Vec<u64> = (first..first + ALLOCATION_GROUP_BLOCKS)
                    .filter(|block| *block < RESERVED_BLOCKS || *block >= blocks - RESERVED_BLOCKS)
                    .collect();
                let listed: Vec<u64> = reserved_group_blocks(group, blocks).collect();
                assert_eq!(listed, scanned, "group {group} of {blocks} blocks");
            }
        }
    }

    #[test]
    fn root_fixtures_round_trip_byte_for_byte() {
        for (slot, fixture) in [
            (256, include_bytes!("../testdata/vectors/root-a.bin")),
            (
                GEOMETRY.blocks() - 257,
                include_bytes!("../testdata/vectors/root-b.bin"),
            ),
        ] {
            let decoded = Root::decode(fixture, slot, GEOMETRY).unwrap();
            assert_eq!(read_u32_le(fixture, 8, "revision").unwrap(), 1);
            assert_eq!(decoded.encode(slot, GEOMETRY).unwrap().0[..], fixture[..]);
            for revision in [0_u32, 2] {
                let mut unsupported = *fixture;
                unsupported[8..12].copy_from_slice(&revision.to_le_bytes());
                root_crc(&mut unsupported);
                assert!(matches!(
                    Root::decode(&unsupported, slot, GEOMETRY),
                    Err(Error::UnsupportedFormat)
                ));
            }
        }
    }

    #[test]
    fn labeled_root_fixture_and_malformed_labels() {
        let fixture = include_bytes!("../testdata/vectors/labeled-root-a.bin");
        let decoded = Root::decode(fixture, 256, GEOMETRY).unwrap();
        assert_eq!(
            decoded.label.as_ref().unwrap().as_str(),
            "Andrei, Backup \\ 東京"
        );
        assert_eq!(decoded.encode(256, GEOMETRY).unwrap().0[..], fixture[..]);
        for slot in [256, GEOMETRY.blocks() - 257] {
            let mut page = decoded.encode(slot, GEOMETRY).unwrap().0;
            assert_eq!(Root::decode(&page[..], slot, GEOMETRY).unwrap(), decoded);
            for length in [0u16, 256, u16::MAX] {
                page = decoded.encode(slot, GEOMETRY).unwrap().0;
                page[184..186].copy_from_slice(&length.to_le_bytes());
                root_crc(&mut page);
                assert!(matches!(
                    Root::decode(&page[..], slot, GEOMETRY),
                    Err(Error::Corrupt(_))
                ));
            }
            for byte in [0, b'/', b'\n', 0xff] {
                page = decoded.encode(slot, GEOMETRY).unwrap().0;
                page[186] = byte;
                root_crc(&mut page);
                assert!(matches!(
                    Root::decode(&page[..], slot, GEOMETRY),
                    Err(Error::Corrupt(_))
                ));
            }
            for offset in [441, 4091] {
                page = decoded.encode(slot, GEOMETRY).unwrap().0;
                page[offset] = 1;
                root_crc(&mut page);
                assert!(matches!(
                    Root::decode(&page[..], slot, GEOMETRY),
                    Err(Error::Corrupt(_))
                ));
            }
        }
    }

    #[test]
    fn page_fixtures_round_trip_byte_for_byte() {
        let uuid = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let reference = TreeRef {
            block: 1024,
            generation: 5,
        };
        for fixture in [
            include_bytes!("../testdata/vectors/empty-catalog-leaf.bin"),
            include_bytes!("../testdata/vectors/extent-catalog-leaf.bin"),
            include_bytes!("../testdata/vectors/symlink-catalog-leaf.bin"),
            include_bytes!("../testdata/vectors/checksum-catalog-leaf.bin"),
        ] {
            assert_eq!(read_u16_le(fixture, 8, "revision").unwrap(), 1);
            for revision in [0_u16, 2] {
                let mut unsupported = *fixture;
                unsupported[8..10].copy_from_slice(&revision.to_le_bytes());
                page_crc(&mut unsupported);
                assert!(matches!(
                    decode_page(
                        &unsupported,
                        reference,
                        &uuid,
                        TreeKind::Catalog,
                        7,
                        Some(0),
                        GEOMETRY.blocks(),
                    ),
                    Err(Error::UnsupportedFormat)
                ));
            }
            let decoded = decode_page(
                fixture,
                reference,
                &uuid,
                TreeKind::Catalog,
                7,
                Some(0),
                GEOMETRY.blocks(),
            )
            .unwrap();
            assert_eq!(
                encode_leaf(
                    reference,
                    &uuid,
                    TreeKind::Catalog,
                    &decoded.records,
                    GEOMETRY.blocks(),
                )
                .unwrap()
                .0[..],
                fixture[..]
            );
            assert_eq!(
                leaf_data_blocks(
                    reference,
                    &uuid,
                    TreeKind::Catalog,
                    &decoded.records,
                    GEOMETRY.blocks(),
                )
                .unwrap(),
                u64::from_le_bytes(fixture[56..64].try_into().unwrap())
            );
            // A repeated key fails both ways alike.
            if let Some(&record) = decoded.records.first() {
                let repeated = [record, record];
                let encoded = encode_leaf(
                    reference,
                    &uuid,
                    TreeKind::Catalog,
                    &repeated,
                    GEOMETRY.blocks(),
                );
                let totalled = leaf_data_blocks(
                    reference,
                    &uuid,
                    TreeKind::Catalog,
                    &repeated,
                    GEOMETRY.blocks(),
                );
                assert!(encoded.is_err());
                assert_eq!(
                    format!("{:?}", encoded.map(|_| ())),
                    format!("{:?}", totalled.map(|_| ()))
                );
            }
        }
    }

    #[test]
    fn malformed_lengths_and_identity_are_corruption_after_recomputed_crc() {
        let uuid = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let reference = TreeRef {
            block: 1024,
            generation: 5,
        };
        let fixture = *include_bytes!("../testdata/vectors/empty-catalog-leaf.bin");

        let mut malformed = fixture;
        malformed[32..34].copy_from_slice(&1u16.to_le_bytes());
        page_crc(&mut malformed);
        assert!(matches!(
            decode_page(
                &malformed,
                reference,
                &uuid,
                TreeKind::Catalog,
                7,
                Some(0),
                GEOMETRY.blocks(),
            ),
            Err(Error::Corrupt(_))
        ));

        let mut identity = fixture;
        identity[16..24].copy_from_slice(&1025u64.to_le_bytes());
        page_crc(&mut identity);
        assert!(matches!(
            decode_page(
                &identity,
                reference,
                &uuid,
                TreeKind::Catalog,
                7,
                Some(0),
                GEOMETRY.blocks(),
            ),
            Err(Error::Corrupt(_))
        ));

        let mut root = *include_bytes!("../testdata/vectors/root-a.bin");
        root[40..48].copy_from_slice(&257u64.to_le_bytes());
        root_crc(&mut root);
        assert!(matches!(
            Root::decode(&root, 256, GEOMETRY),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn short_inputs_never_panic() {
        for length in 0..BLOCK_BYTES {
            let bytes = vec![0u8; length];
            assert!(Root::decode(&bytes, 256, GEOMETRY).is_err());
            assert!(
                decode_page(
                    &bytes,
                    TreeRef {
                        block: 1024,
                        generation: 1
                    },
                    &UUID,
                    TreeKind::Catalog,
                    1,
                    None,
                    GEOMETRY.blocks(),
                )
                .is_err()
            );
        }
    }

    fn inode(seconds: i64) -> [u8; 128] {
        let mut value = [0; 128];
        value[0] = 1;
        value[8..16].copy_from_slice(&1u64.to_le_bytes());
        for offset in [40, 56, 72] {
            value[offset..offset + 8].copy_from_slice(&seconds.to_le_bytes());
        }
        value
    }

    #[test]
    fn timestamp_and_id_boundaries_are_checked() {
        let key = [0, 0, 0, 0, 0, 0, 0, 2, 1];
        for seconds in [-11_644_473_600, 253_402_300_799] {
            assert!(
                encode_leaf(
                    TreeRef {
                        block: 1024,
                        generation: 1
                    },
                    &UUID,
                    TreeKind::Catalog,
                    &[(&key, &inode(seconds))],
                    GEOMETRY.blocks(),
                )
                .is_ok()
            );
        }
        for seconds in [-11_644_473_601, 253_402_300_800] {
            assert!(
                encode_leaf(
                    TreeRef {
                        block: 1024,
                        generation: 1
                    },
                    &UUID,
                    TreeKind::Catalog,
                    &[(&key, &inode(seconds))],
                    GEOMETRY.blocks(),
                )
                .is_err()
            );
        }

        assert!(validate_owner_id(MAX_ID - 1, Validation::Input).is_ok());
        assert!(validate_owner_id(MAX_ID, Validation::Input).is_err());
    }

    #[test]
    fn symlink_inodes_carry_exactly_their_target() {
        let symlink = |target: &[u8], edit: fn(&mut Vec<u8>)| {
            let mut value = inode(0).to_vec();
            value[0] = 3;
            value[16..24].copy_from_slice(&(target.len() as u64).to_le_bytes());
            value.extend_from_slice(target);
            edit(&mut value);
            validate_inode(&value, GEOMETRY.blocks(), Validation::Disk)
        };
        assert!(symlink(b"../notes.txt", |_| {}).is_ok());
        assert!(symlink(&[b'x'; MAX_SYMLINK_TARGET], |_| {}).is_ok());
        assert!(symlink(b"file link", |value| value[1] = 2).is_ok());
        assert!(symlink(b"directory link", |value| value[1] = 6).is_ok());
        let mut file = inode(0);
        file[1] = 2;
        assert!(validate_inode(&file, GEOMETRY.blocks(), Validation::Disk).is_err());
        for (target, edit) in [
            (&b""[..], (|_| {}) as fn(&mut Vec<u8>)),
            (&[b'x'; MAX_SYMLINK_TARGET + 1], |_| {}),
            (b"a\0b", |_| {}),
            (&[0xff], |_| {}),
            (b"size", |value| value[16] += 1),
            (b"allocated", |value| value[24] = 1),
            (b"children", |value| value[32] = 1),
            (b"reserved", |value| value[100] = 1),
            (b"flags", |value| value[1] = 8),
            (b"kind without record", |value| value[1] = 4),
            // A file or directory inode must end at 128 bytes.
            (b"file", |value| value[0] = 1),
            (b"directory", |value| value[0] = 2),
        ] {
            assert!(
                matches!(
                    symlink(target, edit),
                    Err(carryfs_ondisk::error::Error::Corrupt(_))
                ),
                "{target:?}"
            );
        }
        let mut entry = vec![0; 12];
        entry[..8].copy_from_slice(&2u64.to_le_bytes());
        entry[10..12].copy_from_slice(&1u16.to_le_bytes());
        entry.push(b'a');
        for (kind, valid) in [(1, true), (2, true), (3, true), (4, false)] {
            entry[8] = kind;
            assert_eq!(
                validate_directory_entry(&entry, Validation::Disk).is_ok(),
                valid
            );
        }
    }

    #[test]
    fn extent_and_checksum_boundaries_are_checked() {
        let mut key = [0; 17];
        key[..8].copy_from_slice(&2u64.to_be_bytes());
        key[8] = 3;
        let mut value = [0; 16];
        value[..8].copy_from_slice(&512u64.to_le_bytes());
        value[8..12].copy_from_slice(&1u32.to_le_bytes());

        key[9..17].copy_from_slice(&(MAX_LOGICAL_BLOCK - 1).to_be_bytes());
        assert!(validate_extent(&key, &value, GEOMETRY.blocks(), Validation::Input).is_ok());
        key[9..17].copy_from_slice(&MAX_LOGICAL_BLOCK.to_be_bytes());
        assert!(validate_extent(&key, &value, GEOMETRY.blocks(), Validation::Input).is_err());
        key[9..17].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(validate_extent(&key, &value, GEOMETRY.blocks(), Validation::Input).is_err());

        key[8] = 4;
        key[9..17].fill(0);
        let checksum_group = [0u8; CHECKSUM_GROUP_BYTES];
        assert!(
            encode_leaf(
                TreeRef {
                    block: 1024,
                    generation: 1
                },
                &UUID,
                TreeKind::Catalog,
                &[(&key, &checksum_group)],
                GEOMETRY.blocks(),
            )
            .is_err()
        );
    }

    #[test]
    fn root_free_count_includes_tree_roots() {
        let mut root = root_a();
        root.free_blocks = root.blocks - RESERVED_TOTAL_BLOCKS - 3;
        assert!(root.encode(256, GEOMETRY).is_ok());
        root.free_blocks += 1;
        assert!(root.encode(256, GEOMETRY).is_err());
    }
}
