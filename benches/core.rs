//! In-process benchmark of the storage core on a RAM-backed device.
//!
//! It measures the library's CPU cost only: no mount, no FUSE, no disk.
//! Run with `cargo bench --bench core`; `CARRYFS_BENCH_ROUNDS` sets rounds (default 5).
//! Each round formats the image afresh; the report is the median per step.

use std::{cell::RefCell, hint::black_box, rc::Rc, time::Instant};

use carryfs_core::{
    Access, BlockDevice, Geometry, MountMode, Name, ROOT, RemoveKind, Result, Volume,
    erase_reserved_bands,
};

const BYTES: u64 = 1024 * 1024 * 1024;
const FILES: usize = 10_000;
const SMALL: usize = 4096;
const LARGE: usize = 256 * 1024 * 1024;
const CHUNK: usize = 1024 * 1024;

#[derive(Clone)]
struct Ram {
    bytes: Rc<RefCell<Vec<u8>>>,
    mode: MountMode,
}

impl BlockDevice for Ram {
    fn mode(&self) -> MountMode {
        self.mode
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
        out.copy_from_slice(&self.bytes.borrow()[at..at + out.len()]);
        Ok(())
    }
    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        let at = offset as usize;
        self.bytes.borrow_mut()[at..at + data.len()].copy_from_slice(data);
        Ok(())
    }
    fn flush_durable(&mut self) -> Result<()> {
        Ok(())
    }
}

fn names() -> Vec<Name> {
    (0..FILES)
        .map(|i| Name::parse(&format!("file-{i:06}.txt")).unwrap())
        .collect()
}

fn round(ram: &Ram, names: &[Name], timings: &mut Vec<(&'static str, f64)>) -> Result<()> {
    let mut step = |label, start: Instant| timings.push((label, start.elapsed().as_secs_f64()));
    let mut ram = ram.clone();
    // Reformatting a used image needs its reserved bands cleared, as mkfs does.
    erase_reserved_bands(&mut ram)?;

    let t = Instant::now();
    let mut volume = Volume::format(ram.clone())?;
    step("format", t);
    // The adapters defer checkpoints on read-write mounts.
    volume.set_deferred_checkpoints(true)?;

    let page = vec![0x5a; SMALL];
    let t = Instant::now();
    for name in names {
        let file = volume.create_file(ROOT, name.clone())?;
        volume.write(&file, 0, &page)?;
        volume.close_handle(file)?;
    }
    volume.sync()?;
    step("create 10k x 4 KiB + sync", t);

    let chunk = vec![0xa5; CHUNK];
    let big = Name::parse("big.bin")?;
    let t = Instant::now();
    let file = volume.create_file(ROOT, big.clone())?;
    for offset in (0..LARGE).step_by(CHUNK) {
        volume.write(&file, offset as u64, &chunk)?;
    }
    volume.close_handle(file)?;
    volume.sync()?;
    step("write 256 MiB + sync", t);

    namespace(&mut volume, names, "rw", &mut step)?;
    volume.eject()?;

    let t = Instant::now();
    let mut volume = Volume::open(Ram {
        mode: MountMode::ReadOnly,
        ..ram.clone()
    })?;
    // The adapters cache metadata on read-only mounts.
    volume.enable_readonly_metadata_cache()?;
    step("open (read-only)", t);

    namespace(&mut volume, names, "ro", &mut step)?;

    let mut out = vec![0; CHUNK];
    let t = Instant::now();
    let file = volume.open_file(ROOT, &big, Access::Read)?;
    for offset in (0..LARGE).step_by(CHUNK) {
        assert_eq!(volume.read(&file, offset as u64, &mut out)?, CHUNK);
    }
    volume.close_handle(file)?;
    step("read 256 MiB", t);

    let t = Instant::now();
    black_box(volume.check()?);
    step("check", t);
    volume.eject()?;

    let mut volume = Volume::open(ram)?;
    volume.set_deferred_checkpoints(true)?;
    let t = Instant::now();
    for name in names {
        volume.remove(ROOT, name, RemoveKind::File)?;
    }
    volume.sync()?;
    step("remove 10k + sync", t);
    volume.eject()
}

/// Lookup each name, then list the directory and stat each entry as `ls -l` does.
fn namespace(
    volume: &mut Volume<Ram>,
    names: &[Name],
    mode: &str,
    step: &mut impl FnMut(&'static str, Instant),
) -> Result<()> {
    let label = |rw, ro| if mode == "rw" { rw } else { ro };
    let t = Instant::now();
    for name in names {
        black_box(volume.lookup(ROOT, name)?);
    }
    step(label("lookup 10k (rw)", "lookup 10k (ro)"), t);

    let t = Instant::now();
    let directory = volume.open_directory(ROOT)?;
    let mut seen = 0;
    let mut last: Option<Name> = None;
    loop {
        let page = match &last {
            Some(name) => volume.read_directory_after(&directory, name)?,
            None => volume.read_directory(&directory, None)?,
        };
        let Some(tail) = page.entries.last() else {
            break;
        };
        last = Some(tail.name.clone());
        for entry in &page.entries {
            black_box(volume.attributes(entry.object)?);
        }
        seen += page.entries.len();
    }
    volume.close_handle(directory)?;
    assert_eq!(seen, FILES + 1);
    step(
        label("readdir + stat 10k (rw)", "readdir + stat 10k (ro)"),
        t,
    );
    Ok(())
}

fn main() -> Result<()> {
    // `cargo test --all-targets` runs this without `--bench`: one smoke round.
    let rounds = if std::env::args().any(|arg| arg == "--bench") {
        std::env::var("CARRYFS_BENCH_ROUNDS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(5)
    } else {
        1
    };
    let names = names();
    // One image for every round, its pages faulted in up front so that
    // first-touch faults are not counted as filesystem work.
    let mut bytes = vec![0; BYTES as usize];
    for page in bytes.chunks_mut(4096) {
        page[0] = black_box(1);
    }
    let ram = Ram {
        bytes: Rc::new(RefCell::new(bytes)),
        mode: MountMode::ReadWrite,
    };
    let mut samples: Vec<(&str, Vec<f64>)> = Vec::new();
    for _ in 0..rounds {
        let mut timings = Vec::new();
        round(&ram, &names, &mut timings)?;
        for (index, (label, seconds)) in timings.into_iter().enumerate() {
            if samples.len() == index {
                samples.push((label, Vec::new()));
            }
            samples[index].1.push(seconds);
        }
    }
    println!(
        "{:<28} {:>10} {:>10} {:>10}",
        "step", "median ms", "min ms", "max ms"
    );
    for (label, mut seconds) in samples {
        seconds.sort_by(f64::total_cmp);
        let ms = |s: f64| s * 1e3;
        println!(
            "{label:<28} {:>10.2} {:>10.2} {:>10.2}",
            ms(seconds[seconds.len() / 2]),
            ms(seconds[0]),
            ms(seconds[seconds.len() - 1])
        );
    }
    Ok(())
}
