//! CPU-only recovery benchmark. Setup and fresh destination allocation are untimed.
//! cargo bench --bench recovery -- --case orphan --files 1000 --rounds 5
//! --save DIR also writes the input image for separate CLI/I/O measurements.

use carryfs_core::{BlockDevice, Geometry, MountMode, Name, ROOT, Repair, Result, Volume};
use carryfs_ondisk::format::{BLOCK_BYTES, Root, TreeKind, decode_page, encode_leaf};
use std::{cell::RefCell, hint::black_box, path::PathBuf, rc::Rc, time::Instant};

const BYTES: usize = 256 * 1024 * 1024;
#[derive(Default)]
struct Counts {
    reads: u64,
    writes: u64,
    flushes: u64,
}
#[derive(Clone)]
struct Ram {
    bytes: Rc<RefCell<Vec<u8>>>,
    counts: Rc<RefCell<Counts>>,
    mode: MountMode,
}
impl Ram {
    fn new() -> Self {
        Self {
            bytes: Rc::new(RefCell::new(vec![0; BYTES])),
            counts: Rc::new(RefCell::new(Counts::default())),
            mode: MountMode::ReadWrite,
        }
    }
}
impl BlockDevice for Ram {
    fn mode(&self) -> MountMode {
        self.mode
    }
    fn geometry(&mut self) -> Result<Geometry> {
        Ok(Geometry {
            byte_len: BYTES as u64,
            logical_sector_bytes: 4096,
            physical_sector_bytes: 4096,
        })
    }
    fn read_exact_at(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
        out.copy_from_slice(&self.bytes.borrow()[offset as usize..offset as usize + out.len()]);
        self.counts.borrow_mut().reads += out.len() as u64;
        Ok(())
    }
    fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        assert_eq!(self.mode, MountMode::ReadWrite);
        self.bytes.borrow_mut()[offset as usize..offset as usize + bytes.len()]
            .copy_from_slice(bytes);
        self.counts.borrow_mut().writes += bytes.len() as u64;
        Ok(())
    }
    fn flush_durable(&mut self) -> Result<()> {
        assert_eq!(self.mode, MountMode::ReadWrite);
        self.counts.borrow_mut().flushes += 1;
        Ok(())
    }
}

fn damage(ram: &Ram, case: &str) {
    let mut bytes = ram.bytes.borrow_mut();
    let root = Root::decode(
        &bytes[256 * BLOCK_BYTES..257 * BLOCK_BYTES],
        256,
        (BYTES / BLOCK_BYTES) as u64,
    )
    .unwrap();
    let mut pending = vec![root.catalog];
    while let Some(reference) = pending.pop() {
        let at = reference.block as usize * BLOCK_BYTES;
        let decoded = decode_page(
            &bytes[at..at + BLOCK_BYTES],
            reference,
            &root.uuid,
            TreeKind::Catalog,
            root.generation,
            None,
            root.blocks,
        )
        .unwrap();
        if decoded.level != 0 {
            for (_, value) in decoded.records() {
                pending.push(carryfs_ondisk::format::TreeRef {
                    block: u64::from_le_bytes(value[..8].try_into().unwrap()),
                    generation: u64::from_le_bytes(value[8..].try_into().unwrap()),
                });
            }
        } else {
            let records: Vec<_> = decoded
                .records()
                .filter(|(key, _)| key[8] != if case == "orphan" { 2 } else { 4 })
                .map(|(key, value)| (key.to_vec(), value.to_vec()))
                .collect();
            let borrowed: Vec<_> = records
                .iter()
                .map(|(key, value)| (key.as_slice(), value.as_slice()))
                .collect();
            let mut page = [0; BLOCK_BYTES];
            encode_leaf(
                &mut page,
                reference,
                &root.uuid,
                TreeKind::Catalog,
                &borrowed,
                root.blocks,
            )
            .unwrap();
            bytes[at..at + BLOCK_BYTES].copy_from_slice(&page);
        }
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut case = String::from("clean");
    let mut files = 1000usize;
    let mut mib = 16usize;
    let mut rounds = 5usize;
    let mut save = None::<PathBuf>;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--case" => case = args.next().ok_or("missing case")?,
            "--files" => files = args.next().ok_or("missing files")?.parse()?,
            "--mib" => mib = args.next().ok_or("missing mib")?.parse()?,
            "--rounds" => rounds = args.next().ok_or("missing rounds")?.parse()?,
            "--save" => save = Some(args.next().ok_or("missing directory")?.into()),
            _ => return Err(format!("unknown argument {arg}").into()),
        }
    }
    if case == "bandwidth" {
        let input = vec![0x5a; 64 * 1024 * 1024];
        let mut output = vec![0; input.len()];
        println!("bytes\tround\tcopy_ms");
        for round in 0..=rounds {
            let start = Instant::now();
            black_box(&mut output).copy_from_slice(black_box(&input));
            let elapsed = start.elapsed();
            assert_eq!(black_box(&output)[output.len() - 1], 0x5a);
            if round != 0 {
                println!(
                    "{}\t{round}\t{:.6}",
                    input.len(),
                    elapsed.as_secs_f64() * 1000.0
                );
            }
        }
        return Ok(());
    }
    assert!(matches!(
        case.as_str(),
        "clean" | "orphan" | "missing" | "data" | "sparse"
    ));
    assert!(rounds > 0 && files > 0);
    let ram = Ram::new();
    let mut volume = Volume::format(ram.clone())?;
    volume.set_deferred_checkpoints(true)?;
    let data = vec![0x5a; if case == "data" { 1024 * 1024 } else { 4096 }];
    let expected = if case == "data" {
        let file = volume.create_file(ROOT, Name::parse("large")?)?;
        for index in 0..mib {
            assert_eq!(
                volume.write(&file, (index * data.len()) as u64, &data)?,
                data.len()
            );
        }
        volume.close_handle(file)?;
        files = 1;
        (mib * data.len()) as u64
    } else if case == "sparse" {
        let file = volume.create_file(ROOT, Name::parse("sparse")?)?;
        volume.write(&file, 1 << 40, b"tail")?;
        volume.close_handle(file)?;
        files = 1;
        4
    } else {
        for index in 0..files {
            let file = volume.create_file(ROOT, Name::parse(&format!("f{index:06}"))?)?;
            volume.write(&file, 0, &data)?;
            volume.close_handle(file)?;
        }
        (files * data.len()) as u64
    };
    volume.eject()?;
    if case == "orphan" || case == "missing" {
        damage(&ram, &case);
    }
    let source = Ram {
        mode: MountMode::ReadOnly,
        ..ram.clone()
    };
    if let Some(directory) = save {
        std::fs::create_dir_all(&directory)?;
        let path = directory.join(format!("{case}-{files}.cfs"));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        use std::io::Write;
        file.write_all(&source.bytes.borrow())?;
        file.sync_all()?;
    }
    println!(
        "case\tfiles\tround\tverify\tdiagnose_ms\trecover_ms\tsource_read_bytes\tdestination_write_bytes\tflushes\tcopied_bytes"
    );
    for round in 0..=rounds {
        for verify in [false, true] {
            let destination = Ram::new();
            let writes = destination.counts.clone();
            *source.counts.borrow_mut() = Counts::default();
            let start = Instant::now();
            let mut repair = Repair::diagnose(black_box(source.clone()))?;
            let diagnosed = start.elapsed();
            let start = Instant::now();
            let recovery = repair.recover_image_with_verification(
                black_box(destination),
                verify,
                &mut |finding| panic!("{finding:?}"),
            )?;
            let recovered = start.elapsed();
            assert_eq!(recovery.summary.files, files as u64);
            assert_eq!(
                recovery.summary.verified_bytes,
                if case == "missing" { 0 } else { expected }
            );
            assert_eq!(recovery.summary.complete, case != "missing");
            assert_eq!(recovery.summary.unreadable_ranges, 0);
            assert!(!recovery.verification_failed);
            assert_eq!(source.counts.borrow().writes, 0);
            assert_eq!(source.counts.borrow().flushes, 0);
            black_box(&recovery.manifest_json);
            if round != 0 {
                let counts = writes.borrow();
                println!(
                    "{case}\t{files}\t{round}\t{verify}\t{:.6}\t{:.6}\t{}\t{}\t{}\t{}",
                    diagnosed.as_secs_f64() * 1000.0,
                    recovered.as_secs_f64() * 1000.0,
                    source.counts.borrow().reads,
                    counts.writes,
                    counts.flushes,
                    recovery.summary.verified_bytes
                );
            }
        }
    }
    Ok(())
}
