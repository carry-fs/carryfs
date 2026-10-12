//! Bounded, sparse crash model. Persistence within a flush interval is unordered.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    io,
    rc::Rc,
};

use super::*;

mod files;

pub(crate) fn fuzz_files(data: &[u8]) {
    files::fuzz(data);
}

pub(crate) const GEOMETRY: Geometry = Geometry {
    byte_len: 256 * 1024 * 1024,
    logical_sector_bytes: 4096,
    physical_sector_bytes: 4096,
};
pub(crate) const UUID: [u8; 16] = [0x42; 16];

#[derive(Clone, Copy, Debug)]
pub(crate) enum Tear {
    Prefix { write: usize, bytes: usize },
    Sectors { write: usize, mask: u8 },
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Failure {
    pub event: usize,
    pub whole_mask: u64,
    pub tear: Option<Tear>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Write(u64),
    Flush,
}

#[derive(Default)]
pub(crate) struct Media {
    pub durable: BTreeMap<u64, Page>,
    pending: Vec<(u64, Page)>,
    pub events: Vec<Event>,
    pub failure: Option<Failure>,
    /// Further pages torn by the same failure, for multi-page tears.
    pub also_torn: Vec<Tear>,
    pub unreadable: Option<u64>,
    /// Media defects remain after volatile state is lost.
    pub persistent_unreadable: BTreeSet<u64>,
    /// A persistent wrong-address response: requested block, returned block.
    pub redirected_read: Option<(u64, u64)>,
    /// A persistent wrong-address write: requested block, block that is written.
    pub redirected_write: Option<(u64, u64)>,
    pub discarded: usize,
    pub reads: usize,
    /// Each file-data read's offset, and the run end and next run it named.
    pub data_runs: Vec<(u64, u64, Option<u64>)>,
}

impl Media {
    pub fn crash(&mut self) {
        self.pending.clear();
        self.events.clear();
        self.failure = None;
        self.also_torn.clear();
        self.unreadable = None;
    }

    fn finish_event(&mut self) -> Result<()> {
        if let Some(failure) = self
            .failure
            .filter(|failure| failure.event + 1 == self.events.len())
        {
            for (index, (block, page)) in self.pending.iter().enumerate() {
                if failure.whole_mask == u64::MAX
                    || (index < 64 && failure.whole_mask & (1 << index) != 0)
                {
                    self.durable.insert(*block, page.clone());
                }
            }
            for tear in failure.tear.into_iter().chain(self.also_torn.clone()) {
                let index = match tear {
                    Tear::Prefix { write, .. } | Tear::Sectors { write, .. } => write,
                };
                if let Some((block, page)) = self.pending.get(index) {
                    let durable = self
                        .durable
                        .entry(*block)
                        .or_insert_with(|| Page(Box::new([0; BLOCK_BYTES])));
                    match tear {
                        Tear::Prefix { bytes, .. } => {
                            let bytes = bytes.min(BLOCK_BYTES);
                            durable.0[..bytes].copy_from_slice(&page.0[..bytes]);
                        }
                        Tear::Sectors { mask, .. } => {
                            for sector in 0..8 {
                                if mask & (1 << sector) != 0 {
                                    let start = sector * 512;
                                    durable.0[start..start + 512]
                                        .copy_from_slice(&page.0[start..start + 512]);
                                }
                            }
                        }
                    }
                }
            }
            return Err(Error::io(
                "injected output failure",
                io::Error::other("simulated interruption"),
            ));
        }
        Ok(())
    }
}

pub(crate) struct FaultDevice {
    pub media: Rc<RefCell<Media>>,
    pub geometry: Geometry,
    mode: MountMode,
}

impl FaultDevice {
    #[cfg(test)]
    pub fn with_mode(&self, mode: MountMode) -> Self {
        Self {
            media: Rc::clone(&self.media),
            geometry: self.geometry,
            mode,
        }
    }

    pub fn new(durable: BTreeMap<u64, Page>, mode: MountMode) -> Self {
        Self::with_geometry(durable, mode, GEOMETRY)
    }

    pub fn with_geometry(
        durable: BTreeMap<u64, Page>,
        mode: MountMode,
        geometry: Geometry,
    ) -> Self {
        Self {
            media: Rc::new(RefCell::new(Media {
                durable,
                ..Media::default()
            })),
            geometry,
            mode,
        }
    }
}

impl BlockDevice for FaultDevice {
    fn mode(&self) -> MountMode {
        self.mode
    }
    fn geometry(&mut self) -> Result<Geometry> {
        Ok(self.geometry)
    }
    // Reads serve whole pages; a contiguous data run arrives as one call, and
    // an injected failure anywhere in it fails the whole read, as a device does.
    fn read_exact_at(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
        if out.is_empty()
            || !out.len().is_multiple_of(BLOCK_BYTES)
            || !offset.is_multiple_of(BLOCK_BYTES as u64)
        {
            return Err(Error::InvalidInput("model requires full pages"));
        }
        let first = offset / BLOCK_BYTES as u64;
        self.media.borrow_mut().reads += out.len() / BLOCK_BYTES;
        let media = self.media.borrow();
        for (index, page) in out.chunks_mut(BLOCK_BYTES).enumerate() {
            let block = first + index as u64;
            self.geometry.block_offset(block)?;
            if media.unreadable == Some(block) || media.persistent_unreadable.contains(&block) {
                return Err(Error::io(
                    "injected read failure",
                    io::Error::other("unreadable block"),
                ));
            }
            let block = match media.redirected_read {
                Some((requested, returned)) if requested == block => returned,
                _ => block,
            };
            self.geometry.block_offset(block)?;
            if let Some((_, durable)) = media
                .pending
                .iter()
                .rev()
                .find(|(candidate, _)| *candidate == block)
            {
                page.copy_from_slice(&durable.0[..]);
            } else if let Some(durable) = media.durable.get(&block) {
                page.copy_from_slice(&durable.0[..]);
            } else {
                page.fill(0);
            }
        }
        Ok(())
    }
    fn read_data_exact_at(
        &mut self,
        offset: u64,
        out: &mut [u8],
        run_end: u64,
        next_run: Option<u64>,
    ) -> Result<()> {
        self.media
            .borrow_mut()
            .data_runs
            .push((offset, run_end, next_run));
        self.read_exact_at(offset, out)
    }
    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if self.mode != MountMode::ReadWrite {
            return Err(Error::ReadOnly);
        }
        if data.len() != BLOCK_BYTES || !offset.is_multiple_of(BLOCK_BYTES as u64) {
            return Err(Error::InvalidInput("model requires full pages"));
        }
        let block = offset / BLOCK_BYTES as u64;
        self.geometry.block_offset(block)?;
        let mut page = Page(Box::new([0; BLOCK_BYTES]));
        page.0.copy_from_slice(data);
        let mut media = self.media.borrow_mut();
        media.events.push(Event::Write(block));
        let landed = match media.redirected_write {
            Some((requested, landed)) if requested == block => landed,
            _ => block,
        };
        self.geometry.block_offset(landed)?;
        media.pending.push((landed, page));
        media.finish_event()
    }
    fn flush_durable(&mut self) -> Result<()> {
        if self.mode != MountMode::ReadWrite {
            return Err(Error::ReadOnly);
        }
        let mut media = self.media.borrow_mut();
        media.events.push(Event::Flush);
        media.finish_event()?;
        // Reverse order deliberately: only successful flush completion is ordered.
        while let Some((block, page)) = media.pending.pop() {
            media.durable.insert(block, page);
        }
        Ok(())
    }
    // Whatever the device keeps of a discarded page is garbage, pending or not.
    fn discard(&mut self, offset: u64) {
        let block = offset / BLOCK_BYTES as u64;
        let garbage = Page(Box::new([0xA5; BLOCK_BYTES]));
        let mut media = self.media.borrow_mut();
        media.discarded += 1;
        for (candidate, page) in &mut media.pending {
            if *candidate == block {
                *page = garbage.clone();
            }
        }
        media.durable.insert(block, garbage);
    }
}

/// Fully accounted leaf-only generations for commit tests, with no reclamation.
fn generation(number: u64) -> Result<(Root, Vec<BlockWrite>)> {
    if !(1..=32).contains(&number) {
        return Err(Error::InvalidInput("model generation exceeds bound"));
    }
    let (mut root, initial) = bootstrap(GEOMETRY, UUID, number as i64, 0)?;
    let first = 512 + (number - 1) * 3;
    root.generation = number;
    root.catalog = TreeRef {
        block: first,
        generation: number,
    };
    root.allocation = TreeRef {
        block: first + 1,
        generation: number,
    };
    root.cleanup = TreeRef {
        block: first + 2,
        generation: number,
    };
    root.free_blocks = root.blocks - 1024 - 3 * number;
    let catalog = decode_page(
        &initial[0].page.0[..],
        TreeRef {
            block: 512,
            generation: 1,
        },
        &UUID,
        TreeKind::Catalog,
        1,
        Some(0),
        root.blocks,
    )?;
    let catalog = encode_leaf(
        root.catalog,
        &UUID,
        TreeKind::Catalog,
        &catalog.records,
        root.blocks,
    )?;
    let mut bitmap = [0; 2048];
    for block in (0..first + 3).chain(crate::workspace::INITIAL) {
        bitmap[block as usize / 8] |= 1 << (block % 8);
    }
    let allocation = encode_leaf(
        root.allocation,
        &UUID,
        TreeKind::Allocation,
        &[(&0_u64.to_be_bytes(), &bitmap)],
        root.blocks,
    )?;
    let retirement: Vec<_> = (2..=number)
        .map(|retired| {
            let mut key = [0; 17];
            key[0] = 1;
            key[1..9].copy_from_slice(&retired.to_be_bytes());
            key[9..17].copy_from_slice(&(512 + (retired - 2) * 3).to_be_bytes());
            let mut value = [0; 8];
            value[..4].copy_from_slice(&3_u32.to_le_bytes());
            (key, value)
        })
        .collect();
    let records: Vec<_> = retirement
        .iter()
        .map(|(key, value)| (key.as_slice(), value.as_slice()))
        .collect();
    let cleanup = encode_leaf(
        root.cleanup,
        &UUID,
        TreeKind::Cleanup,
        &records,
        root.blocks,
    )?;
    Ok((
        root,
        vec![
            BlockWrite {
                block: first,
                page: catalog,
            },
            BlockWrite {
                block: first + 1,
                page: allocation,
            },
            BlockWrite {
                block: first + 2,
                page: cleanup,
            },
        ],
    ))
}

pub(crate) fn advance(volume: &mut Volume<FaultDevice>) -> Result<()> {
    let next = volume
        .root
        .as_ref()
        .ok_or(Error::InvalidInput("model volume has no root"))?
        .generation
        .checked_add(1)
        .ok_or(Error::InvalidInput("model generation exhausted"))?;
    let (root, writes) = generation(next)?;
    volume.publish(root, writes.clone(), &[], None, Vec::new(), 0)
}

#[cfg(any(test, feature = "fuzzing"))]
pub(crate) fn formatted_volume() -> Volume<FaultDevice> {
    Volume::format_at(
        FaultDevice::new(BTreeMap::new(), MountMode::ReadWrite),
        UUID,
        1,
        0,
    )
    .expect("model format")
}

/// An independently encoded orphan, including extent/checksum ownership.
pub(crate) fn orphan_image(data_blocks: u32) -> BTreeMap<u64, Page> {
    assert!(data_blocks <= 1024 && data_blocks.is_multiple_of(16));
    let mut image = formatted_volume().into_model_image();
    let mut root = Root::decode(&image[&256].0[..], 256, GEOMETRY).expect("fixture root");
    let directory = decode_page(
        &image[&512].0[..],
        root.catalog,
        &UUID,
        TreeKind::Catalog,
        1,
        None,
        root.blocks,
    )
    .expect("fixture catalog")
    .records[0]
        .1
        .to_vec();
    let mut file = directory.clone();
    file[0] = 1;
    file[1] = 1;
    file[16..24].copy_from_slice(&(u64::from(data_blocks) * BLOCK_BYTES as u64).to_le_bytes());
    file[24..32].copy_from_slice(&u64::from(data_blocks).to_le_bytes());
    let key = |owner: u64, kind: u8, suffix: Option<u64>| {
        let mut key = owner.to_be_bytes().to_vec();
        key.push(kind);
        if let Some(suffix) = suffix {
            key.extend_from_slice(&suffix.to_be_bytes());
        }
        key
    };
    let mut records = vec![(key(1, 1, None), directory), (key(2, 1, None), file)];
    if data_blocks != 0 {
        let mut extent = 4096u64.to_le_bytes().to_vec();
        extent.extend_from_slice(&data_blocks.to_le_bytes());
        extent.extend_from_slice(&[0; 4]);
        records.push((key(2, 3, Some(0)), extent));
        let mut checksums = vec![0; 1056];
        let crc = crate::format::crc32c(&[0; 65_536]);
        for unit in 0..data_blocks as usize / 16 {
            checksums[unit / 8] |= 1 << (unit % 8);
            checksums[32 + unit * 4..36 + unit * 4].copy_from_slice(&crc.to_le_bytes());
        }
        records.push((
            key(2, 4, Some(0)),
            crate::format::compact_group(&crate::format::expand_group(&checksums)).to_vec(),
        ));
    }
    let borrowed: Vec<_> = records
        .iter()
        .map(|(k, v)| (k.as_slice(), v.as_slice()))
        .collect();
    image.insert(
        512,
        encode_leaf(
            root.catalog,
            &UUID,
            TreeKind::Catalog,
            &borrowed,
            root.blocks,
        )
        .expect("orphan catalog"),
    );
    let mut bitmap = decode_page(
        &image[&513].0[..],
        root.allocation,
        &UUID,
        TreeKind::Allocation,
        1,
        None,
        root.blocks,
    )
    .expect("fixture allocation")
    .records[0]
        .1
        .to_vec();
    for block in 4096..4096 + data_blocks as usize {
        bitmap[block / 8] |= 1 << (block % 8);
    }
    image.insert(
        513,
        encode_leaf(
            root.allocation,
            &UUID,
            TreeKind::Allocation,
            &[(&0u64.to_be_bytes(), &bitmap)],
            root.blocks,
        )
        .expect("orphan allocation"),
    );
    let mut marker = vec![3];
    marker.extend_from_slice(&2u64.to_be_bytes());
    image.insert(
        514,
        encode_leaf(
            root.cleanup,
            &UUID,
            TreeKind::Cleanup,
            &[(&marker, &[])],
            root.blocks,
        )
        .expect("orphan marker"),
    );
    root.next_object = 3;
    root.free_blocks -= u64::from(data_blocks);
    image.insert(256, root.encode(256, GEOMETRY).expect("root A"));
    image.insert(
        root.blocks - 257,
        root.encode(root.blocks - 257, GEOMETRY).expect("root B"),
    );
    image
}

#[cfg(any(test, feature = "fuzzing"))]
impl Volume<FaultDevice> {
    pub(crate) fn into_model_image(self) -> BTreeMap<u64, Page> {
        self.device
            .as_ref()
            .expect("model device")
            .media
            .borrow()
            .durable
            .clone()
    }
}

#[cfg(feature = "fuzzing")]
pub(crate) fn fuzz(data: &[u8]) {
    let device = FaultDevice::new(BTreeMap::new(), MountMode::ReadWrite);
    let mut volume = Volume::format_at(device, UUID, 1, 0).expect("model bootstrap");
    let mut acknowledged = 1;
    for action in data.chunks(5).take(16) {
        if action.len() < 5 {
            break;
        }
        let media = volume.device.as_ref().expect("live device").media.clone();
        media.borrow_mut().events.clear();
        if action[0] & 1 == 0 {
            media.borrow_mut().failure = Some(Failure {
                event: usize::from(action[1] % 8),
                whole_mask: u64::from(action[2]),
                tear: Some(if action[0] & 2 == 0 {
                    Tear::Sectors {
                        write: usize::from(action[3] % 3),
                        mask: action[4],
                    }
                } else {
                    Tear::Prefix {
                        write: usize::from(action[3] % 3),
                        bytes: usize::from(action[4]) * 16,
                    }
                }),
            });
        }
        let before = volume
            .info()
            .expect("healthy before publication")
            .generation;
        if advance(&mut volume).is_ok() {
            acknowledged = before + 1;
        } else {
            let calls = media.borrow().events.len();
            assert!(matches!(volume.sync(), Err(Error::Faulted)));
            assert!(matches!(volume.eject(), Err(Error::Faulted)));
            assert_eq!(media.borrow().events.len(), calls);
        }
        drop(volume);
        media.borrow_mut().crash();
        let durable = media.borrow().durable.clone();
        volume =
            Volume::open(FaultDevice::new(durable, MountMode::ReadWrite)).expect("crash recovery");
        let recovered = volume.info().expect("recovered info").generation;
        assert!(recovered >= acknowledged && recovered >= before && recovered <= before + 1);
    }
}

#[cfg(feature = "fuzzing")]
pub(crate) fn fuzz_maintenance(data: &[u8]) {
    let mut volume = if data.first().is_some_and(|byte| byte & 4 != 0) {
        Volume::open(FaultDevice::new(
            orphan_image(if data[0] & 8 == 0 { 16 } else { 1024 }),
            MountMode::ReadWrite,
        ))
        .expect("orphan fixture")
    } else {
        let mut volume = formatted_volume();
        advance(&mut volume).expect("retirement fixture");
        volume
    };
    let mut acknowledged = volume.info().expect("fixture info").generation;
    for action in data.as_chunks::<4>().0.iter().take(8) {
        let media = volume.device.as_ref().expect("model device").media.clone();
        media.borrow_mut().events.clear();
        if action[0] & 1 == 0 {
            media.borrow_mut().failure = Some(Failure {
                event: usize::from(action[1] % 64),
                whole_mask: if action[2] & 1 == 0 { 0 } else { u64::MAX },
                tear: Some(Tear::Sectors {
                    write: usize::from(action[2] % 54),
                    mask: action[3],
                }),
            });
        }
        let before = volume.info().expect("healthy before sync").generation;
        if volume.sync().is_ok() {
            acknowledged = volume.info().expect("successful sync").generation;
        } else {
            let count = media.borrow().events.len();
            assert!(matches!(volume.sync(), Err(Error::Faulted)));
            assert_eq!(media.borrow().events.len(), count);
        }
        drop(volume);
        media.borrow_mut().crash();
        volume = Volume::open(FaultDevice::new(
            media.borrow().durable.clone(),
            MountMode::ReadWrite,
        ))
        .expect("structurally valid after cleanup interruption");
        let recovered = volume.info().expect("recovered").generation;
        assert!(recovered >= acknowledged && recovered >= before && recovered <= before + 1);
    }
}

/// Batched operations with deferred checkpoints against a model of a flat
/// namespace: creations, writes (1 MiB ones cross the queued-data bound),
/// removals, edits that fail after changing the batch, syncs, and crashes.
/// Every recovered image passes the full check. A crash with no checkpoint
/// since the last `sync` recovers exactly the model at that sync. A batch
/// that fit and then failed to seal trips the assertion in `seal_batch`.
#[cfg(any(test, feature = "fuzzing"))]
pub(crate) fn fuzz_batch(data: &[u8]) {
    use crate::Name;
    fn contents(volume: &mut Volume<FaultDevice>, object: ObjectId) -> Vec<u8> {
        let size = volume.attributes(object).expect("attributes").size as usize;
        let handle = volume
            .open_object(object, Access::Read)
            .expect("open for read");
        let mut out = vec![0; size];
        let mut done = 0;
        while done < size {
            done += volume
                .read(&handle, done as u64, &mut out[done..])
                .expect("read");
        }
        volume.close_handle(handle).expect("close");
        out
    }
    fn listing(volume: &mut Volume<FaultDevice>) -> BTreeMap<String, Vec<u8>> {
        let root = volume.open_directory(ROOT).expect("open root");
        let (mut entries, mut cursor) = (Vec::new(), None);
        loop {
            let page = volume
                .read_directory(&root, cursor.as_ref())
                .expect("list root");
            entries.extend(page.entries);
            match page.cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        volume.close_handle(root).expect("close root");
        entries
            .into_iter()
            .map(|entry| {
                let text = String::from_utf8(entry.name.stored().to_vec()).expect("ASCII name");
                (text, contents(volume, entry.object))
            })
            .collect()
    }
    fn reopen(volume: Volume<FaultDevice>) -> Volume<FaultDevice> {
        let media = volume.device.as_ref().expect("live device").media.clone();
        drop(volume);
        media.borrow_mut().crash();
        let durable = media.borrow().durable.clone();
        Volume::open(FaultDevice::new(durable.clone(), MountMode::ReadOnly))
            .expect("read-only recovery")
            .check()
            .expect("recovered image passes the full check");
        let mut volume =
            Volume::open(FaultDevice::new(durable, MountMode::ReadWrite)).expect("recovery");
        volume
            .set_deferred_checkpoints(true)
            .expect("deferred checkpoints");
        volume
    }
    let durable_generation =
        |volume: &Volume<FaultDevice>| volume.chain.as_ref().map(|chain| chain.durable.generation);

    let device = FaultDevice::new(BTreeMap::new(), MountMode::ReadWrite);
    let mut volume = Volume::format_at(device, UUID, 1, 0).expect("model bootstrap");
    volume
        .set_deferred_checkpoints(true)
        .expect("deferred checkpoints");
    let mut model: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut synced = (model.clone(), durable_generation(&volume));
    for action in data.chunks(4).take(64) {
        let [op, which, at, len] = *action else {
            break;
        };
        let file = format!("n{:02}", which % 24);
        let now = Timestamp::new(1, 0).expect("fixed time");
        match op % 8 {
            0 | 1 => {
                let created = volume
                    .create_file(ROOT, Name::parse(&file).expect("name"))
                    .map(|handle| volume.close_handle(handle).expect("close"));
                match created {
                    Ok(()) => assert!(model.insert(file, Vec::new()).is_none()),
                    Err(Error::NameCollision) => assert!(model.contains_key(&file)),
                    Err(error) => panic!("create {file}: {error:?}"),
                }
            }
            2 | 3 => {
                let Some(old) = model.get(&file).cloned() else {
                    continue;
                };
                let offset = (usize::from(at) * 509).min(old.len() + 4096);
                let length = if len == 255 {
                    1 << 20
                } else {
                    usize::from(len) * 97 + 1
                };
                let bytes: Vec<u8> = (0..length).map(|index| (index as u8) ^ op ^ at).collect();
                let handle = volume
                    .open_file(ROOT, &Name::parse(&file).expect("name"), Access::ReadWrite)
                    .expect("open for write");
                let mut done = 0;
                while done < length {
                    done += volume
                        .write(&handle, (offset + done) as u64, &bytes[done..])
                        .expect("write");
                }
                volume.close_handle(handle).expect("close");
                let mut new = old;
                if new.len() < offset + length {
                    new.resize(offset + length, 0);
                }
                new[offset..offset + length].copy_from_slice(&bytes);
                model.insert(file, new);
            }
            4 => {
                let removed =
                    volume.remove(ROOT, &Name::parse(&file).expect("name"), RemoveKind::File);
                match removed {
                    Ok(()) => assert!(model.remove(&file).is_some()),
                    Err(Error::NotFound) => assert!(!model.contains_key(&file)),
                    Err(error) => panic!("remove {file}: {error:?}"),
                }
            }
            5 => {
                let failed = volume.edit(|transaction| {
                    super::namespace::create_in(
                        transaction,
                        ROOT,
                        Name::parse("ghost").expect("name"),
                        ObjectKind::File,
                        &[],
                        now,
                    )?;
                    let runs = transaction.allocate_data(u64::from(len % 32) + 1, 0)?;
                    transaction.write_data(runs[0].0, Page(Box::new([at; BLOCK_BYTES])))?;
                    Err::<(), _>(Error::InvalidInput("injected"))
                });
                assert!(matches!(failed, Err(Error::InvalidInput("injected"))));
            }
            6 => {
                volume.sync().expect("sync");
                synced = (model.clone(), durable_generation(&volume));
            }
            _ => {
                let unchanged = durable_generation(&volume) == synced.1;
                volume = reopen(volume);
                let recovered = listing(&mut volume);
                if unchanged {
                    assert_eq!(recovered, synced.0, "crash lost or added synced state");
                }
                model = recovered;
                synced = (model.clone(), durable_generation(&volume));
            }
        }
    }
    volume.sync().expect("final sync");
    let mut volume = reopen(volume);
    assert_eq!(
        listing(&mut volume),
        model,
        "a synced model was not recovered"
    );
}
