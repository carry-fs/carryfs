use core_trace as fs;
use fs::{BlockDevice, Geometry, MountMode, Name, ROOT, RemoveKind, Volume};
use std::{hint::black_box, time::Instant};

const CAPACITY: usize = 256 << 20;
struct Ram<'a>(&'a mut [u8], MountMode);
impl BlockDevice for Ram<'_> {
    fn mode(&self) -> MountMode { self.1 }
    fn geometry(&mut self) -> fs::Result<Geometry> {
        Ok(Geometry { byte_len: self.0.len() as u64, logical_sector_bytes: 4096, physical_sector_bytes: 4096 })
    }
    fn read_exact_at(&mut self, offset: u64, out: &mut [u8]) -> fs::Result<()> {
        let start = usize::try_from(offset).unwrap();
        let source = self.0.get(start..start.checked_add(out.len()).unwrap()).ok_or(fs::Error::InvalidInput("RAM read bounds"))?;
        out.copy_from_slice(source);
        Ok(())
    }
    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> fs::Result<()> {
        let start = usize::try_from(offset).unwrap();
        self.0.get_mut(start..start.checked_add(data.len()).unwrap()).ok_or(fs::Error::InvalidInput("RAM write bounds"))?.copy_from_slice(data);
        Ok(())
    }
    fn flush_durable(&mut self) -> fs::Result<()> { Ok(()) }
}
fn run<D: BlockDevice>(device: D, case: &str, count: usize, deferred: bool) {
    let mut volume = Volume::format(device).unwrap();
    volume.set_deferred_checkpoints(deferred).unwrap();
    let names: Vec<_> = (0..count).map(|i| Name::parse(&format!("file-{i:06}")).unwrap()).collect();
    let data: Vec<u8> = (0..(1 << 20)).map(|i| ((i * 13 + 7) % 251) as u8).collect();
    let mut read = vec![0; 1 << 20];
    let mut check = 0_u64;
    let start = Instant::now();
    match case {
        "create" | "churn" | "small-write" => {
            for name in &names {
                let handle = volume.create_file(ROOT, name.clone()).unwrap();
                if case != "create" { assert_eq!(volume.write(&handle, 0, &data[..4096]).unwrap(), 4096); }
                volume.close_handle(handle).unwrap();
            }
            volume.sync().unwrap();
            if case != "create" {
                for name in &names {
                    let handle = volume.open_file(ROOT, name, fs::Access::Read).unwrap();
                    let len = volume.read(&handle, 0, &mut read[..4096]).unwrap();
                    assert_eq!(len, 4096);
                    assert_eq!(&read[..len], &data[..len]);
                    check += read[0] as u64;
                    volume.close_handle(handle).unwrap();
                }
            }
            if case == "churn" {
                for name in &names { volume.remove(ROOT, name, RemoveKind::File).unwrap(); }
                volume.sync().unwrap();
            }
        }
        "large-write" | "partial-overwrite" => {
            let handle = volume.create_file(ROOT, names[0].clone()).unwrap();
            for i in 0..count {
                let offset = (i % 32) as u64 * (1 << 20);
                assert_eq!(volume.write(&handle, offset, &data).unwrap(), data.len());
                if case == "partial-overwrite" {
                    assert_eq!(volume.write(&handle, offset + 123, &data[..4096]).unwrap(), 4096);
                }
                if (i + 1) % 16 == 0 { volume.sync().unwrap(); }
            }
            volume.sync().unwrap();
            let len = volume.read(&handle, 0, &mut read).unwrap();
            assert_eq!(len, data.len());
            if case == "large-write" { assert_eq!(read, data); }
            else {
                assert_eq!(&read[..123], &data[..123]);
                assert_eq!(&read[123..4219], &data[..4096]);
                assert_eq!(&read[4219..], &data[4219..]);
            }
            check += read[0] as u64;
            volume.close_handle(handle).unwrap();
        }
        _ => panic!("unknown workload"),
    }
    let elapsed = start.elapsed().as_nanos();
    black_box(check);
    for name in names.iter().take(if case == "large-write" || case == "partial-overwrite" { 1 } else { count }) {
        if case == "churn" { assert!(matches!(volume.lookup(ROOT, name), Err(fs::Error::NotFound))); }
        else { volume.lookup(ROOT, name).unwrap(); }
    }
    volume.eject().unwrap();
    println!("{case}\t{deferred}\t{count}\t{elapsed}\t{check}");
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let case = &args[1];
    let count: usize = args[2].parse().unwrap();
    let deferred: bool = args[3].parse().unwrap();
    if let Some(path) = args.get(4) {
        run(fs::ImageDevice::create_new(path, CAPACITY as u64).unwrap(), case, count, deferred);
        let mut reopened = Volume::open(fs::ImageDevice::open(path, MountMode::ReadOnly).unwrap()).unwrap();
        reopened.check().unwrap();
    } else {
        let mut bytes = vec![0; CAPACITY];
        run(Ram(&mut bytes, MountMode::ReadWrite), case, count, deferred);
        let mut reopened = Volume::open(Ram(&mut bytes, MountMode::ReadOnly)).unwrap();
        reopened.check().unwrap();
    }
    fs::map_sizes::report();
}
