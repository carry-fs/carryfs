//! Create a disposable image for exploring the manager; refuses an existing path.
use carryfs_core::{ImageDevice, Name, ObjectId, ROOT, Volume, VolumeLabel};
use std::path::Path;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub fn create(path: &Path) -> Result<()> {
    let mut volume = Volume::format(ImageDevice::create_new(path, 256 * 1024 * 1024)?)?;
    volume.set_label(VolumeLabel::parse("CarryFS Demo")?)?;
    write(&mut volume, ROOT, "README.txt", b"Welcome to CarryFS.\nThis disposable image demonstrates browsing, structural checks and exporting files.\n")?;
    let docs = volume.create_directory(ROOT, Name::parse("Documents")?)?;
    write(
        &mut volume,
        docs,
        "architecture.md",
        b"# CarryFS\n\nA portable filesystem with checksummed data and atomic publication.\n",
    )?;
    write(
        &mut volume,
        docs,
        "packing-list.txt",
        b"Laptop\nCharger\nCarryFS drive\n",
    )?;
    let projects = volume.create_directory(ROOT, Name::parse("Projects")?)?;
    let manager = volume.create_directory(projects, Name::parse("manager")?)?;
    write(
        &mut volume,
        manager,
        "notes.md",
        b"# Image manager\n\nBuilt with Rune. Images are opened read-only.\n",
    )?;
    let backups = volume.create_directory(ROOT, Name::parse("Backups")?)?;
    write(
        &mut volume,
        backups,
        "manifest.csv",
        b"path,kind\n/Documents,folder\n/Projects,folder\n",
    )?;
    volume.eject()?;
    Ok(())
}

pub fn write(
    volume: &mut Volume<ImageDevice>,
    parent: ObjectId,
    name: &str,
    data: &[u8],
) -> Result<()> {
    let file = volume.create_file(parent, Name::parse(name)?)?;
    let result = (|| -> Result<()> {
        let mut offset = 0;
        while offset < data.len() {
            let n = volume.write(&file, offset as u64, &data[offset..])?;
            if n == 0 {
                return Err("Filesystem returned an empty write".into());
            }
            offset += n;
        }
        Ok(())
    })();
    let closed = volume.close_handle(file);
    result?;
    closed?;
    Ok(())
}

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("Usage: cargo run --example demo -- /path/to/new-demo.cfs")?;
    create(Path::new(&path))?;
    println!("Created {}", Path::new(&path).display());
    Ok(())
}
