//! Host operations used by offline transfer. All extraction creates are relative
//! to held directory descriptors and refuse existing entries.

use std::ffi::{CString, OsStr};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path};

use carryfs_core::{Attributes, Error, ExtractionSink, Name, Result};

// Linux asm-generic/fcntl.h and Apple's xnu bsd/sys/fcntl.h.
#[cfg(target_os = "linux")]
mod flags {
    pub const DIRECTORY: i32 = 0o200000;
    pub const NOFOLLOW: i32 = 0o400000;
    pub const CLOEXEC: i32 = 0o2000000;
    pub const CREATE: i32 = 0o100 | 0o200 | 1;
    pub const NONBLOCK: i32 = 0o4000;
    pub type Mode = u32;
    pub type OpenMode = u32;
}
#[cfg(target_os = "macos")]
mod flags {
    pub const DIRECTORY: i32 = 0x00100000;
    pub const NOFOLLOW: i32 = 0x100;
    pub const CLOEXEC: i32 = 0x01000000;
    pub const CREATE: i32 = 0x200 | 0x800 | 1;
    pub const NONBLOCK: i32 = 4;
    pub type Mode = u16;
    pub type OpenMode = i32;
}
use flags::*;

unsafe extern "C" {
    fn openat(dirfd: i32, path: *const std::ffi::c_char, flags: i32, ...) -> i32;
    fn mkdirat(dirfd: i32, path: *const std::ffi::c_char, mode: Mode) -> i32;
    fn symlinkat(target: *const std::ffi::c_char, dirfd: i32, path: *const std::ffi::c_char)
    -> i32;
}

fn io_error(operation: &'static str, source: io::Error) -> Error {
    Error::Io { operation, source }
}

fn component(name: &OsStr) -> Result<CString> {
    if name.as_bytes().contains(&b'/') || name == "." || name == ".." || name.is_empty() {
        return Err(Error::InvalidInput("expected one normal path component"));
    }
    CString::new(name.as_bytes()).map_err(|_| Error::InvalidInput("NUL in host path"))
}

fn open_at(parent: &File, name: &CString, flags: i32) -> Result<File> {
    // SAFETY: parent is live, name is NUL-terminated, and the variadic mode
    // uses the target's promoted C integer type.
    let fd = unsafe {
        openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | NOFOLLOW | CLOEXEC,
            0o600 as OpenMode,
        )
    };
    if fd < 0 {
        return Err(io_error(
            "open relative host entry",
            io::Error::last_os_error(),
        ));
    }
    // SAFETY: openat returned a new descriptor; File becomes its sole owner.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(super) fn open_source(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW | NONBLOCK)
        .open(path)
        .map_err(|e| io_error("open import source", e))?;
    if !file
        .metadata()
        .map_err(|e| io_error("inspect import source", e))?
        .is_file()
    {
        return Err(Error::InvalidInput("import source is not a regular file"));
    }
    Ok(file)
}

pub(super) struct Sink {
    parent: File,
    name: CString,
    // Root followed by the currently visited ancestors. Completed branches are
    // dropped when traversal returns to a shallower path.
    directories: Vec<File>,
}

impl Sink {
    pub(super) fn new(path: &Path) -> Result<Self> {
        let name = component(
            path.file_name()
                .ok_or(Error::InvalidInput("destination needs a new name"))?,
        )?;
        let parent_path = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut parent = File::open(if parent_path.is_absolute() { "/" } else { "." })
            .map_err(|e| io_error("open destination parent", e))?;
        for part in parent_path.components() {
            match part {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => parent = open_at(&parent, &component(name)?, DIRECTORY)?,
                _ => {
                    return Err(Error::InvalidInput(
                        "destination parent cannot contain '..' or a platform prefix",
                    ));
                }
            }
        }
        // Reject any existing root, including aliases of the source image.
        // The exclusive creation below repeats this check atomically.
        match open_at(&parent, &name, NONBLOCK) {
            Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            Ok(_) => return Err(Error::InvalidInput("extraction destination already exists")),
        }
        Ok(Self {
            parent,
            name,
            directories: Vec::new(),
        })
    }

    fn entry(&mut self, path: &[Name]) -> Result<(&File, CString)> {
        self.directories.truncate(path.len());
        if let Some(name) = path.last() {
            let parent = self
                .directories
                .last()
                .ok_or(Error::InvalidInput("extraction parent was not created"))?;
            Ok((parent, component(OsStr::from_bytes(name.stored()))?))
        } else {
            Ok((&self.parent, self.name.clone()))
        }
    }
}

impl ExtractionSink for Sink {
    fn create_directory(&mut self, path: &[Name], _: &Attributes) -> Result<()> {
        let (parent, name) = self.entry(path)?;
        // SAFETY: the directory descriptor and NUL-terminated component remain
        // live for this call. Mode has the target's mode_t representation.
        if unsafe { mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            return Err(io_error(
                "create extraction directory",
                io::Error::last_os_error(),
            ));
        }
        let directory = open_at(parent, &name, DIRECTORY)?;
        self.directories.push(directory);
        Ok(())
    }

    fn create_file(&mut self, path: &[Name], _: &Attributes) -> Result<File> {
        let (parent, name) = self.entry(path)?;
        open_at(parent, &name, CREATE)
    }

    fn create_symlink(&mut self, path: &[Name], target: &[u8], _: &Attributes) -> Result<()> {
        let (parent, name) = self.entry(path)?;
        let target =
            CString::new(target).map_err(|_| Error::InvalidInput("NUL in symlink target"))?;
        // SAFETY: the directory descriptor and both NUL-terminated strings
        // remain live for this call. symlinkat refuses an existing entry.
        if unsafe { symlinkat(target.as_ptr(), parent.as_raw_fd(), name.as_ptr()) } != 0 {
            return Err(io_error(
                "create extraction symlink",
                io::Error::last_os_error(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carryfs_core::{ObjectKind, ROOT, Timestamp};
    use std::io::Write;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "carryfs-host-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn attributes() -> Attributes {
        let time = Timestamp::new(0, 0).unwrap();
        Attributes {
            kind: ObjectKind::Directory,
            size: 0,
            allocated_bytes: 0,
            parent: ROOT,
            creation: time,
            modification: time,
            metadata_change: time,
            orphan: false,
            directory_link: None,
        }
    }

    #[test]
    fn extraction_rejects_existing_paths_aliases_and_symlink_parents() {
        let temp = Temp::new();
        let source = temp.0.join("source");
        std::fs::write(&source, b"source bytes").unwrap();
        assert!(Sink::new(&source).is_err());
        let alias = temp.0.join("hardlink");
        std::fs::hard_link(&source, &alias).unwrap();
        assert!(Sink::new(&alias).is_err());
        symlink(&source, temp.0.join("symlink")).unwrap();
        assert!(Sink::new(&temp.0.join("symlink")).is_err());
        symlink(&temp.0, temp.0.join("parent-link")).unwrap();
        assert!(Sink::new(&temp.0.join("parent-link/new")).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"source bytes");
    }

    #[test]
    fn extraction_holds_parents_across_path_replacement() {
        let temp = Temp::new();
        let parent = temp.0.join("parent");
        let held = temp.0.join("held");
        let outside = temp.0.join("outside");
        std::fs::create_dir(&parent).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let mut sink = Sink::new(&parent.join("output")).unwrap();
        std::fs::rename(&parent, &held).unwrap();
        symlink(&outside, &parent).unwrap();
        sink.create_directory(&[], &attributes()).unwrap();
        let root = held.join("output");
        let pinned = held.join("renamed");
        std::fs::rename(&root, &pinned).unwrap();
        symlink(&outside, &root).unwrap();
        let child = [Name::parse("data").unwrap()];
        let mut file = sink.create_file(&child, &attributes()).unwrap();
        file.write_all(b"verified").unwrap();
        assert_eq!(std::fs::read(pinned.join("data")).unwrap(), b"verified");
        assert!(!outside.join("data").exists());
        assert!(!outside.join("output").exists());
        assert!(sink.create_file(&child, &attributes()).is_err());
    }

    #[test]
    fn extraction_refuses_entries_inserted_after_preflight() {
        let temp = Temp::new();
        let output = temp.0.join("output");
        let mut sink = Sink::new(&output).unwrap();
        std::fs::write(&output, b"late entry").unwrap();
        assert!(sink.create_file(&[], &attributes()).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), b"late entry");
        let mut sink = Sink::new(&temp.0.join("tree")).unwrap();
        sink.create_directory(&[], &attributes()).unwrap();
        symlink(&output, temp.0.join("tree/data")).unwrap();
        assert!(
            sink.create_file(&[Name::parse("data").unwrap()], &attributes())
                .is_err()
        );
        assert_eq!(std::fs::read(&output).unwrap(), b"late entry");
    }
}
