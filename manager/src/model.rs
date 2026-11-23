use carryfs_core::{
    Attributes, DirectoryEntry, Error, ExtractionSink, ImageDevice, Mode, MountMode, Name,
    ObjectId, ObjectKind, ROOT, Space, Volume, VolumeInfo,
};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone)]
pub struct Entry {
    pub item: DirectoryEntry,
    pub attrs: Attributes,
}

/// Where a volume lives, whether or not it opens.
#[derive(Clone)]
pub struct Source {
    pub path: PathBuf,
    pub device: Option<crate::platform::Device>,
}

impl Source {
    pub fn name(&self) -> String {
        match &self.device {
            Some(device) => format!("{} · {}", device.name, device.id),
            None => self
                .path
                .file_name()
                .unwrap_or(self.path.as_os_str())
                .to_string_lossy()
                .into_owned(),
        }
    }
}

/// Whether `error` is metadata damage that offline repair may handle.
pub fn corrupt(error: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    matches!(
        error.downcast_ref::<Error>(),
        Some(Error::Corrupt(_) | Error::Corruption { .. })
    )
}

#[derive(Clone)]
pub struct Browser {
    pub device: Option<crate::platform::Device>,
    pub path: PathBuf,
    pub info: VolumeInfo,
    pub space: Space,
    /// Formatted by CarryFS 0.1.0: readable, but writes need an upgrade.
    pub legacy: bool,
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
        Self::load(
            None,
            path.clone(),
            ImageDevice::open(&path, MountMode::ReadOnly)?,
        )
    }

    pub fn open_device(device: crate::platform::Device) -> Result<Self> {
        let image = crate::platform::open_device(&device)?;
        Self::load(Some(device.clone()), device.path(), image)
    }

    fn load(
        device: Option<crate::platform::Device>,
        path: PathBuf,
        image: ImageDevice,
    ) -> Result<Self> {
        let mut volume = open_volume(image)?;
        let info = volume.info()?;
        Ok(Self {
            legacy: info.revision < u32::from(carryfs_core::REVISION),
            info,
            space: volume.space()?,
            entries: list(&mut volume, ROOT)?,
            device,
            path,
            trail: vec![(ROOT, "/".into())],
        })
    }

    /// Bytes in use, metadata included: the most a scrub reads.
    pub fn used_bytes(&self) -> u64 {
        let free = self.info.free_blocks * u64::from(self.space.block_bytes);
        self.info.byte_len.saturating_sub(free)
    }

    pub fn source(&self) -> Source {
        Source {
            path: self.path.clone(),
            device: self.device.clone(),
        }
    }

    pub fn volume(&self) -> Result<Volume<ImageDevice>> {
        let device = match &self.device {
            Some(device) => crate::platform::open_device(device)?,
            None => ImageDevice::open(&self.path, MountMode::ReadOnly)?,
        };
        let volume = open_volume(device)?;
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
        let attributes = volume.attributes(object)?;
        if attributes.kind != ObjectKind::File {
            return Err("Select a regular file to export.".into());
        }
        let result = (|| -> Result<()> {
            // create_new prevents overwriting the source image, links, or an existing export.
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let output = options.open(destination)?;
            let copied = (|| -> Result<()> {
                let mut failure = None;
                let summary =
                    volume.extract_to(object, &mut ExportFile(&output), &mut |finding| {
                        if failure.is_none() {
                            failure = Some(finding.error.to_string());
                        }
                    })?;
                if !summary.complete || summary.unreadable_ranges != 0 {
                    return Err(failure
                        .unwrap_or_else(|| "File export did not complete".into())
                        .into());
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    output.set_permissions(fs::Permissions::from_mode(u32::from(
                        attributes.mode.bits(),
                    )))?;
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
        result?;
        Ok(format!("Exported to {}", destination.display()))
    }
}

struct ExportFile<'a>(&'a File);

impl ExtractionSink for ExportFile<'_> {
    fn create_file(&mut self, path: &[Name], _: &Attributes) -> carryfs_core::Result<File> {
        if !path.is_empty() {
            return Err(Error::InvalidInput(
                "single-file export received a child entry",
            ));
        }
        self.0.try_clone().map_err(|source| Error::Io {
            operation: "open export descriptor",
            source,
        })
    }

    fn create_directory(&mut self, _: &[Name], _: &Attributes) -> carryfs_core::Result<()> {
        Err(Error::WrongType)
    }

    fn create_symlink(&mut self, _: &[Name], _: &[u8], _: &Attributes) -> carryfs_core::Result<()> {
        Err(Error::WrongType)
    }

    fn create_link(&mut self, _: &[Name], _: &[Name]) -> carryfs_core::Result<()> {
        Err(Error::WrongType)
    }
}

fn open_volume(device: ImageDevice) -> Result<Volume<ImageDevice>> {
    Volume::open(device).map_err(|error| match error {
        Error::UnsupportedFormat => "This volume uses a CarryFS format feature this manager does not support. Update CarryFS Manager.".into(),
        error => error.into(),
    })
}

fn list(volume: &mut Volume<ImageDevice>, directory: ObjectId) -> Result<Vec<Entry>> {
    volume.enable_readonly_metadata_cache()?;
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

/// `rwxr-xr-x` for the nine permission bits.
pub fn permissions(mode: Mode) -> String {
    let bits = mode.bits();
    (0..9)
        .map(|i| {
            if bits & (0o400 >> i) == 0 {
                '-'
            } else {
                ['r', 'w', 'x'][i % 3]
            }
        })
        .collect()
}

pub fn kind(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Directory => "Folder",
        ObjectKind::File => "File",
        ObjectKind::Symlink => "Symbolic link",
    }
}
