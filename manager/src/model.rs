use carryfs_core::{
    Access, Attributes, DirectoryEntry, ImageDevice, MountMode, ObjectId, ObjectKind, ROOT, Space,
    Volume, VolumeInfo,
};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone)]
pub struct Entry {
    pub item: DirectoryEntry,
    pub attrs: Attributes,
}

#[derive(Clone)]
pub struct Browser {
    pub device: Option<crate::platform::Device>,
    pub path: PathBuf,
    pub info: VolumeInfo,
    pub space: Space,
    pub trail: Vec<(ObjectId, String)>,
    pub entries: Vec<Entry>,
}

impl Browser {
    pub fn open(path: PathBuf) -> Result<Self> {
        if !fs::metadata(&path)?.is_file() {
            return Err(
                "Choose a regular CarryFS image file; raw devices are not supported.".into(),
            );
        }
        let mut volume = Volume::open(ImageDevice::open(&path, MountMode::ReadOnly)?)?;
        let info = volume.info()?;
        let space = volume.space()?;
        let entries = list(&mut volume, ROOT)?;
        Ok(Self {
            device: None,
            path,
            info,
            space,
            trail: vec![(ROOT, "/".into())],
            entries,
        })
    }

    pub fn open_device(device: crate::platform::Device) -> Result<Self> {
        let mut volume = Volume::open(crate::platform::open_device(&device)?)?;
        let info = volume.info()?;
        let space = volume.space()?;
        let entries = list(&mut volume, ROOT)?;
        Ok(Self {
            path: device.path(),
            device: Some(device),
            info,
            space,
            trail: vec![(ROOT, "/".into())],
            entries,
        })
    }

    pub fn volume(&self) -> Result<Volume<ImageDevice>> {
        let device = match &self.device {
            Some(device) => crate::platform::open_device(device)?,
            None => ImageDevice::open(&self.path, MountMode::ReadOnly)?,
        };
        let volume = Volume::open(device)?;
        let info = volume.info()?;
        if info.uuid != self.info.uuid || info.generation != self.info.generation {
            return Err(
                "The volume changed. Reopen it to refresh its contents before continuing.".into(),
            );
        }
        Ok(volume)
    }

    pub fn navigate(mut self, trail: Vec<(ObjectId, String)>) -> Result<Self> {
        let directory = trail.last().ok_or("Missing directory")?.0;
        self.entries = list(&mut self.volume()?, directory)?;
        self.trail = trail;
        Ok(self)
    }

    pub fn export(&self, object: ObjectId, destination: &Path) -> Result<String> {
        let mut volume = self.volume()?;
        if volume.attributes(object)?.kind != ObjectKind::File {
            return Err("Select a regular file to export.".into());
        }
        let handle = volume.open_object(object, Access::Read)?;
        let result = (|| -> Result<()> {
            // create_new prevents overwriting the source image, links, or an existing export.
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)?;
            let copied = (|| -> Result<()> {
                let mut buffer = vec![0; 1024 * 1024];
                let mut offset = 0;
                loop {
                    let n = volume.read(&handle, offset, &mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    output.write_all(&buffer[..n])?;
                    offset += n as u64;
                }
                output.sync_all()?;
                Ok(())
            })();
            drop(output);
            if let Err(error) = copied {
                fs::remove_file(destination)
                    .map_err(|cleanup| format!("{error}; removing incomplete export: {cleanup}"))?;
                return Err(error);
            }
            Ok(())
        })();
        let closed = volume.close_handle(handle);
        result?;
        closed?;
        Ok(format!("Exported to {}", destination.display()))
    }
}

fn list(volume: &mut Volume<ImageDevice>, directory: ObjectId) -> Result<Vec<Entry>> {
    let handle = volume.open_directory(directory)?;
    let result = (|| -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        let mut cursor = None;
        loop {
            let page = volume.read_directory(&handle, cursor.as_ref())?;
            for item in page.entries {
                let attrs = volume.attributes(item.object)?;
                entries.push(Entry { item, attrs });
            }
            cursor = page.cursor;
            if cursor.is_none() {
                break;
            }
        }
        entries.sort_by_key(|entry| entry.item.kind != ObjectKind::Directory);
        Ok(entries)
    })();
    let closed = volume.close_handle(handle);
    let entries = result?;
    closed?;
    Ok(entries)
}

pub fn bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut amount = value as f64;
    let mut unit = 0;
    while amount >= 1024.0 && unit < UNITS.len() - 1 {
        amount /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{amount:.1} {}", UNITS[unit])
    }
}

pub fn kind(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Directory => "Folder",
        ObjectKind::File => "File",
        ObjectKind::Symlink => "Symbolic link",
    }
}
