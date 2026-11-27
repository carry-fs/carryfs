//! Windows host operations used by offline extraction. Every entry is
//! created with `NtCreateFile` relative to a directory handle the sink opened
//! itself, never through a reparse point, and existing entries are refused.
//! Names Windows forbids get the adapter's reversible aliases (FORMAT.md).

use std::collections::HashMap;
use std::ffi::{OsStr, c_void};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Component, Path, PathBuf};

use carryfs_core::{Attributes, Error, ExtractionSink, Name, Result, windows_symlink_target};

type Handle = *mut c_void;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}

#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root_directory: Handle,
    object_name: *const UnicodeString,
    attributes: u32,
    security_descriptor: *const c_void,
    security_quality_of_service: *const c_void,
}

#[repr(C)]
struct IoStatusBlock {
    status: usize,
    information: usize,
}

/// `FILE_LINK_INFORMATION` with room for any one component.
#[repr(C)]
struct LinkInformation {
    replace_if_exists: u32,
    root_directory: Handle,
    file_name_length: u32,
    file_name: [u16; 255],
}

#[repr(C)]
struct AttributeTagInformation {
    file_attributes: u32,
    reparse_tag: u32,
}

#[repr(C)]
struct TokenPrivileges {
    count: u32,
    luid_low: u32,
    luid_high: i32,
    attributes: u32,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateFile(
        handle: *mut Handle,
        access: u32,
        object: *const ObjectAttributes,
        status: *mut IoStatusBlock,
        allocation: *const i64,
        file_attributes: u32,
        share: u32,
        disposition: u32,
        options: u32,
        extended_attributes: *const c_void,
        extended_attributes_length: u32,
    ) -> i32;
    fn NtSetInformationFile(
        handle: Handle,
        status: *mut IoStatusBlock,
        information: *const c_void,
        length: u32,
        class: u32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn DeviceIoControl(
        handle: Handle,
        code: u32,
        input: *const c_void,
        input_len: u32,
        output: *mut c_void,
        output_len: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn GetFileInformationByHandleEx(
        handle: Handle,
        class: u32,
        info: *mut c_void,
        size: u32,
    ) -> i32;
    fn GetVolumeInformationByHandleW(
        handle: Handle,
        name: *mut u16,
        name_size: u32,
        serial: *mut u32,
        max_component: *mut u32,
        flags: *mut u32,
        file_system: *mut u16,
        file_system_size: u32,
    ) -> i32;
    fn GetCurrentProcess() -> Handle;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    fn AdjustTokenPrivileges(
        token: Handle,
        disable_all: i32,
        new: *const TokenPrivileges,
        previous_len: u32,
        previous: *mut c_void,
        returned: *mut u32,
    ) -> i32;
}

const SYNCHRONIZE: u32 = 0x0010_0000;
const DELETE: u32 = 0x0001_0000;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_TRAVERSE: u32 = 0x20;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const FILE_SHARE_ALL: u32 = 1 | 2 | 4;
const FILE_OPEN: u32 = 1;
const FILE_CREATE: u32 = 2;
const FILE_DIRECTORY_FILE: u32 = 0x1;
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
const FILE_NON_DIRECTORY_FILE: u32 = 0x40;
const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const OBJ_CASE_INSENSITIVE: u32 = 0x40;
const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_SUPPORTS_SPARSE_FILES: u32 = 0x40;
const FILE_SUPPORTS_REPARSE_POINTS: u32 = 0x80;
const FILE_ATTRIBUTE_TAG_INFO: u32 = 9;
const FILE_LINK_INFORMATION: u32 = 11;
const FILE_DISPOSITION_INFORMATION: u32 = 13;
const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00a4;
const FSCTL_SET_SPARSE: u32 = 0x0009_00c4;
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
/// Symlinks, junctions, and other tags that redirect a name elsewhere.
const NAME_SURROGATE: u32 = 0x2000_0000;
const SYMLINK_FLAG_RELATIVE: u32 = 1;
const MAXIMUM_REPARSE_DATA_BUFFER_SIZE: usize = 16 * 1024;
const TOKEN_ADJUST_PRIVILEGES: u32 = 0x20;
const SE_CREATE_SYMBOLIC_LINK_PRIVILEGE: u32 = 35;
const SE_PRIVILEGE_ENABLED: u32 = 2;
const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;

fn io_error(operation: &'static str, source: io::Error) -> Error {
    Error::Io { operation, source }
}

fn nt_result(status: i32) -> io::Result<()> {
    if status < 0 {
        // SAFETY: a pure status-code translation.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    Ok(())
}

/// Open or create `name` in `parent` without following a reparse point.
fn open_at(
    parent: &File,
    name: &[u16],
    access: u32,
    attributes: u32,
    disposition: u32,
    options: u32,
) -> io::Result<File> {
    let bytes = u16::try_from(name.len() * 2)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "host name is too long"))?;
    let string = UnicodeString {
        length: bytes,
        maximum_length: bytes,
        buffer: name.as_ptr(),
    };
    let object = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root_directory: parent.as_raw_handle(),
        object_name: &string,
        attributes: OBJ_CASE_INSENSITIVE,
        security_descriptor: std::ptr::null(),
        security_quality_of_service: std::ptr::null(),
    };
    let mut handle = std::ptr::null_mut();
    let mut status = IoStatusBlock {
        status: 0,
        information: 0,
    };
    // SAFETY: every pointer refers to a live local for the duration of the
    // call, and the parent handle stays open.
    nt_result(unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE,
            &object,
            &mut status,
            std::ptr::null(),
            attributes,
            FILE_SHARE_ALL,
            disposition,
            options | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
            std::ptr::null(),
            0,
        )
    })?;
    // SAFETY: NtCreateFile succeeded and returned a new handle; File becomes
    // its sole owner.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// Set `information` of `class` on an open handle.
fn set_information<T>(file: Handle, information: &T, class: u32) -> io::Result<()> {
    let mut status = IoStatusBlock {
        status: 0,
        information: 0,
    };
    // SAFETY: the caller keeps the handle open, and `information` is a live
    // value of the size passed, laid out as `class` requires.
    nt_result(unsafe {
        NtSetInformationFile(
            file,
            &mut status,
            (information as *const T).cast(),
            size_of::<T>() as u32,
            class,
        )
    })
}

fn control(file: &File, code: u32, input: &[u8]) -> io::Result<()> {
    let mut returned = 0;
    // SAFETY: the handle is open and synchronous, so no OVERLAPPED, and the
    // input buffer is live for the call.
    if unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            input.as_ptr().cast(),
            input.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn utf16(text: &OsStr) -> Vec<u16> {
    text.encode_wide().collect()
}

fn alias(name: &Name) -> Result<Vec<u16>> {
    Ok(name.windows_alias()?.encode_utf16().collect())
}

/// Open an existing directory below `parent`, refusing a symlink or junction.
/// Other reparse points, such as cloud-synced folders, are plain directories.
fn open_directory(parent: &File, name: &OsStr) -> Result<File> {
    let directory = open_at(
        parent,
        &utf16(name),
        FILE_TRAVERSE | FILE_READ_ATTRIBUTES,
        0,
        FILE_OPEN,
        FILE_DIRECTORY_FILE,
    )
    .map_err(|e| io_error("open destination parent", e))?;
    let mut tag = AttributeTagInformation {
        file_attributes: 0,
        reparse_tag: 0,
    };
    // SAFETY: the handle is open and `tag` is a live value of the size passed.
    if unsafe {
        GetFileInformationByHandleEx(
            directory.as_raw_handle(),
            FILE_ATTRIBUTE_TAG_INFO,
            (&raw mut tag).cast(),
            size_of::<AttributeTagInformation>() as u32,
        )
    } == 0
    {
        return Err(io_error(
            "inspect destination parent",
            io::Error::last_os_error(),
        ));
    }
    if tag.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        && tag.reparse_tag & NAME_SURROGATE != 0
    {
        return Err(Error::InvalidInput(
            "destination parent cannot be a symlink or junction",
        ));
    }
    Ok(directory)
}

fn volume_flags(directory: &File) -> Result<u32> {
    let mut flags = 0;
    // SAFETY: the handle is open; only `flags` is written.
    if unsafe {
        GetVolumeInformationByHandleW(
            directory.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags,
            std::ptr::null_mut(),
            0,
        )
    } == 0
    {
        return Err(io_error(
            "inspect destination volume",
            io::Error::last_os_error(),
        ));
    }
    Ok(flags)
}

/// Enable the symlink privilege, which an administrator holds disabled.
fn enable_symlink_privilege() -> io::Result<()> {
    let mut token = std::ptr::null_mut();
    // SAFETY: the pseudo-handle needs no closing; `token` receives a new handle.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcessToken returned a new handle; this takes ownership.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let privileges = TokenPrivileges {
        count: 1,
        luid_low: SE_CREATE_SYMBOLIC_LINK_PRIVILEGE,
        luid_high: 0,
        attributes: SE_PRIVILEGE_ENABLED,
    };
    // SAFETY: the token is open and `privileges` is live; no previous state
    // is requested.
    if unsafe {
        AdjustTokenPrivileges(
            token.as_raw_handle(),
            0,
            &privileges,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A symlink `REPARSE_DATA_BUFFER` with `target` as both substitute and print
/// name, relative as the adapter presents it: a leading `\` is the drive root.
fn symlink_data(target: &str) -> Option<Vec<u8>> {
    let path: Vec<u8> = target.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let size = 20 + 2 * path.len();
    if size > MAXIMUM_REPARSE_DATA_BUFFER_SIZE {
        return None;
    }
    let length = path.len() as u16;
    let mut data = Vec::with_capacity(size);
    data.extend_from_slice(&IO_REPARSE_TAG_SYMLINK.to_le_bytes());
    data.extend_from_slice(&(12 + 2 * length).to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    for field in [0, length, length, length] {
        data.extend_from_slice(&field.to_le_bytes());
    }
    data.extend_from_slice(&SYMLINK_FLAG_RELATIVE.to_le_bytes());
    data.extend_from_slice(&path);
    data.extend_from_slice(&path);
    Some(data)
}

fn path_text(path: &[Name]) -> String {
    path.iter()
        .map(|name| String::from_utf8_lossy(name.stored()))
        .collect::<Vec<_>>()
        .join("/")
}

pub(super) struct Sink {
    parent: File,
    name: Vec<u16>,
    /// The root followed by the currently visited ancestors. Windows applies
    /// no read-only flag to directories, so each is complete once created.
    directories: Vec<File>,
    /// Created objects with names still to link, by path. Each is linked
    /// through its own handle, so a later name reaches only the object
    /// extraction wrote.
    held: HashMap<Vec<Vec<u8>>, Held>,
    sparse: bool,
    reparse_points: bool,
    /// Symlinks this host could not create, already reported.
    skipped: u64,
}

struct Held {
    object: File,
    /// Names still to link before the handle can close.
    remaining: u32,
}

// ponytail: at most this many objects are held at once; later names of other
// objects are copied. Raise it, or hold fewer per object, if link farms need more.
const MAX_HELD: usize = 128;

fn path_key(path: &[Name]) -> Vec<Vec<u8>> {
    path.iter().map(|name| name.stored().to_vec()).collect()
}

impl Sink {
    pub(super) fn new(path: &Path) -> Result<Self> {
        let name = path
            .file_name()
            .ok_or(Error::InvalidInput("destination needs a new name"))?;
        // The NT API would create `CON` or `out.` literally, beyond the
        // reach of most Windows programs, so require a name Windows accepts.
        let text = name
            .to_str()
            .ok_or(Error::InvalidInput("destination name is not Unicode"))?;
        if Name::parse(text)?.windows_alias()? != text {
            return Err(Error::InvalidInput(
                "destination name is reserved or not valid on Windows",
            ));
        }
        let parent_path = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut components = parent_path.components().peekable();
        let mut root = PathBuf::new();
        while let Some(&part @ (Component::Prefix(_) | Component::RootDir)) = components.peek() {
            root.push(part);
            components.next();
        }
        if root.as_os_str().is_empty() {
            root.push(".");
        } else if !root.has_root() {
            return Err(Error::InvalidInput(
                "destination cannot be relative to a drive's current directory",
            ));
        }
        let mut parent = OpenOptions::new()
            .access_mode(FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&root)
            .map_err(|e| io_error("open destination parent", e))?;
        for part in components {
            match part {
                Component::CurDir => {}
                Component::Normal(name) => parent = open_directory(&parent, name)?,
                _ => {
                    return Err(Error::InvalidInput(
                        "destination parent cannot contain '..' or a platform prefix",
                    ));
                }
            }
        }
        let name = utf16(name);
        // Reject any existing root, including aliases of the source image.
        // The exclusive creation below repeats this check atomically.
        match open_at(&parent, &name, FILE_READ_ATTRIBUTES, 0, FILE_OPEN, 0) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("inspect extraction destination", error)),
            Ok(_) => return Err(Error::InvalidInput("extraction destination already exists")),
        }
        let flags = volume_flags(&parent)?;
        let reparse_points = flags & FILE_SUPPORTS_REPARSE_POINTS != 0;
        if reparse_points && let Err(error) = enable_symlink_privilege() {
            // Without the privilege, symlinks are reported as they come up.
            eprintln!("carryfs: enabling the symlink privilege: {error}");
        }
        Ok(Self {
            parent,
            name,
            directories: Vec::new(),
            held: HashMap::new(),
            sparse: flags & FILE_SUPPORTS_SPARSE_FILES != 0,
            reparse_points,
            skipped: 0,
        })
    }

    /// Close held objects, then fail if any symlink could not be created.
    pub(super) fn finish(mut self) -> super::Result<()> {
        self.held.clear();
        if self.skipped != 0 {
            return Err(format!(
                "{} symlinks were not created; everything else was extracted",
                self.skipped
            )
            .into());
        }
        Ok(())
    }

    fn entry(&mut self, path: &[Name]) -> Result<(&File, Vec<u16>)> {
        self.directories.truncate(path.len());
        match path.last() {
            Some(name) => Ok((
                self.directories
                    .last()
                    .ok_or(Error::InvalidInput("extraction parent was not created"))?,
                alias(name)?,
            )),
            None => Ok((&self.parent, self.name.clone())),
        }
    }

    fn skip(&mut self, path: &[Name], reason: &str) {
        eprintln!(
            "carryfs: {:?}: symlink not created: {reason}",
            path_text(path)
        );
        self.skipped += 1;
    }

    /// Whether to hold a new object for its later names. Objects not held
    /// have their later names extracted as copies.
    fn holds(&self, attributes: &Attributes) -> bool {
        attributes.links > 1 && self.held.len() < MAX_HELD
    }

    fn hold(&mut self, path: &[Name], object: File, attributes: &Attributes) {
        let held = Held {
            object,
            remaining: attributes.links - 1,
        };
        self.held.insert(path_key(path), held);
    }
}

fn hidden(attributes: &Attributes) -> u32 {
    if attributes.hidden {
        FILE_ATTRIBUTE_HIDDEN
    } else {
        0
    }
}

impl ExtractionSink for Sink {
    fn create_directory(&mut self, path: &[Name], attributes: &Attributes) -> Result<()> {
        let (parent, name) = self.entry(path)?;
        let directory = open_at(
            parent,
            &name,
            FILE_TRAVERSE | FILE_READ_ATTRIBUTES,
            hidden(attributes),
            FILE_CREATE,
            FILE_DIRECTORY_FILE,
        )
        .map_err(|e| io_error("create extraction directory", e))?;
        self.directories.push(directory);
        Ok(())
    }

    fn create_file(&mut self, path: &[Name], attributes: &Attributes) -> Result<File> {
        let mut flags = hidden(attributes);
        // The flag limits later opens, not this handle, which extraction
        // writes through.
        if !attributes.mode.owner_writable() {
            flags |= FILE_ATTRIBUTE_READONLY;
        }
        let sparse = self.sparse && attributes.allocated_bytes < attributes.size;
        let hold = self.holds(attributes);
        let (parent, name) = self.entry(path)?;
        let file = open_at(
            parent,
            &name,
            GENERIC_READ | GENERIC_WRITE,
            flags,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE,
        )
        .map_err(|e| io_error("create extraction file", e))?;
        // Holes the source has stay unallocated at the destination.
        if sparse {
            control(&file, FSCTL_SET_SPARSE, &[])
                .map_err(|e| io_error("make extraction file sparse", e))?;
        }
        if hold {
            let object = file
                .try_clone()
                .map_err(|e| io_error("hold extracted file", e))?;
            self.hold(path, object, attributes);
        }
        Ok(file)
    }

    fn create_symlink(
        &mut self,
        path: &[Name],
        target: &[u8],
        attributes: &Attributes,
    ) -> Result<()> {
        if !self.reparse_points {
            self.skip(path, "the destination file system has no symlinks");
            return Ok(());
        }
        let Some(data) = std::str::from_utf8(target)
            .ok()
            .and_then(|target| windows_symlink_target(target).ok())
            .and_then(|target| symlink_data(&target))
        else {
            self.skip(path, "its target cannot be written as a Windows path");
            return Ok(());
        };
        let hold = self.holds(attributes);
        // Windows follows a file symlink only to a file, so a link to a
        // directory must be a directory itself. CarryFS records which one a
        // link was made as; links it never learned this for become files.
        let options = if attributes.directory_link == Some(true) {
            FILE_DIRECTORY_FILE
        } else {
            FILE_NON_DIRECTORY_FILE
        };
        let (parent, name) = self.entry(path)?;
        let link = open_at(
            parent,
            &name,
            GENERIC_WRITE | DELETE,
            hidden(attributes),
            FILE_CREATE,
            options,
        )
        .map_err(|e| io_error("create extraction symlink", e))?;
        if let Err(error) = control(&link, FSCTL_SET_REPARSE_POINT, &data) {
            // Remove the empty placeholder, deleted when the handle closes.
            set_information(link.as_raw_handle(), &1u8, FILE_DISPOSITION_INFORMATION)
                .map_err(|e| io_error("remove failed extraction symlink", e))?;
            if error.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD) {
                self.skip(
                    path,
                    "creating symlinks needs administrator rights or Developer Mode",
                );
                return Ok(());
            }
            return Err(io_error("create extraction symlink", error));
        }
        if hold {
            self.hold(path, link, attributes);
        }
        Ok(())
    }

    fn create_link(&mut self, path: &[Name], existing: &[Name]) -> Result<()> {
        self.link_entry(path, existing).inspect_err(|error| {
            eprintln!(
                "carryfs: could not link {:?}: {error}; trying a separate copy",
                path_text(path)
            );
        })
    }
}

impl Sink {
    fn link_entry(&mut self, path: &[Name], existing: &[Name]) -> Result<()> {
        let key = path_key(existing);
        // The held entry stays in place, and its handle open, until the end.
        let object = self
            .held
            .get(&key)
            .ok_or(Error::InvalidInput("no extracted object to link"))?
            .object
            .as_raw_handle();
        let (parent, name) = self.entry(path)?;
        let mut information = LinkInformation {
            replace_if_exists: 0,
            root_directory: parent.as_raw_handle(),
            file_name_length: (name.len() * 2) as u32,
            file_name: [0; 255],
        };
        information
            .file_name
            .get_mut(..name.len())
            .ok_or(Error::InvalidInput("link name is too long"))?
            .copy_from_slice(&name);
        set_information(object, &information, FILE_LINK_INFORMATION)
            .map_err(|e| io_error("create extraction link", e))?;
        if let Some(held) = self.held.get_mut(&key) {
            held.remaining = held.remaining.saturating_sub(1);
            if held.remaining == 0 {
                self.held.remove(&key);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carryfs_core::{ObjectKind, ROOT, Timestamp};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::windows::fs::MetadataExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
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
            clear_read_only(&self.0);
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[expect(
        clippy::permissions_set_readonly_false,
        reason = "on Windows this clears only the read-only flag"
    )]
    fn clear_read_only(path: &Path) {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            if metadata.is_dir() && !metadata.is_symlink() {
                clear_read_only(&entry.path());
            } else if metadata.file_attributes() & FILE_ATTRIBUTE_READONLY != 0 {
                let mut permissions = metadata.permissions();
                permissions.set_readonly(false);
                std::fs::set_permissions(entry.path(), permissions).unwrap();
            }
        }
    }
    /// A junction, which needs no privilege, standing in for any name surrogate.
    fn junction(link: &Path, target: &Path) {
        let output = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    fn attributes() -> Attributes {
        let time = Timestamp::new(0, 0).unwrap();
        Attributes {
            kind: ObjectKind::Directory,
            size: 0,
            allocated_bytes: 0,
            parent: ROOT,
            links: 1,
            creation: time,
            modification: time,
            metadata_change: time,
            orphan: false,
            directory_link: None,
            mode: carryfs_core::Mode::DIRECTORY,
            hidden: false,
        }
    }
    fn file(links: u32, mode: u32) -> Attributes {
        Attributes {
            kind: ObjectKind::File,
            links,
            mode: carryfs_core::Mode::from_host(mode),
            ..attributes()
        }
    }
    fn name(text: &str) -> [Name; 1] {
        [Name::parse(text).unwrap()]
    }
    fn flags(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().file_attributes()
    }

    #[test]
    fn extraction_rejects_existing_paths_aliases_reserved_names_and_junction_parents() {
        let temp = Temp::new();
        let source = temp.0.join("source");
        std::fs::write(&source, b"source bytes").unwrap();
        assert!(Sink::new(&source).is_err());
        assert!(Sink::new(&temp.0.join("SOURCE")).is_err());
        let alias = temp.0.join("hardlink");
        std::fs::hard_link(&source, &alias).unwrap();
        assert!(Sink::new(&alias).is_err());
        let directory = temp.0.join("directory");
        std::fs::create_dir(&directory).unwrap();
        junction(&temp.0.join("junction"), &directory);
        assert!(Sink::new(&temp.0.join("junction")).is_err());
        assert!(Sink::new(&temp.0.join("junction").join("new")).is_err());
        assert!(Sink::new(&temp.0.join("missing").join("new")).is_err());
        assert!(Sink::new(&directory.join("..").join("new")).is_err());
        for reserved in ["CON", "nul.txt", "out.", "out ", "what?"] {
            assert!(Sink::new(&temp.0.join(reserved)).is_err(), "{reserved}");
        }
        assert!(Sink::new(&temp.0.join("new")).is_ok());
        assert!(Sink::new(&directory.join("new")).is_ok());
        assert_eq!(std::fs::read(&source).unwrap(), b"source bytes");
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
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
        junction(&parent, &outside);
        sink.create_directory(&[], &attributes()).unwrap();
        let root = held.join("output");
        let pinned = held.join("renamed");
        std::fs::rename(&root, &pinned).unwrap();
        junction(&root, &outside);
        let mut data = sink.create_file(&name("data"), &file(1, 0o644)).unwrap();
        data.write_all(b"verified").unwrap();
        drop(data);
        assert_eq!(std::fs::read(pinned.join("data")).unwrap(), b"verified");
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        assert!(sink.create_file(&name("DATA"), &file(1, 0o644)).is_err());
    }

    #[test]
    fn extraction_refuses_entries_inserted_after_preflight() {
        let temp = Temp::new();
        let output = temp.0.join("output");
        let mut sink = Sink::new(&output).unwrap();
        std::fs::write(&output, b"late entry").unwrap();
        assert!(sink.create_file(&[], &file(1, 0o644)).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), b"late entry");
        let outside = temp.0.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let mut sink = Sink::new(&temp.0.join("tree")).unwrap();
        sink.create_directory(&[], &attributes()).unwrap();
        junction(&temp.0.join("tree").join("data"), &outside);
        assert!(sink.create_directory(&name("data"), &attributes()).is_err());
        assert!(sink.create_file(&name("data"), &file(1, 0o644)).is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn extraction_writes_forbidden_names_as_aliases_with_their_flags() {
        let temp = Temp::new();
        let root = temp.0.join("output");
        let mut sink = Sink::new(&root).unwrap();
        sink.create_directory(&[], &attributes()).unwrap();
        let flagged = Attributes {
            hidden: true,
            ..file(1, 0o444)
        };
        for original in ["what?", "CON", "name."] {
            sink.create_file(&name(original), &flagged)
                .unwrap()
                .write_all(original.as_bytes())
                .unwrap();
        }
        let hidden_directory = Attributes {
            hidden: true,
            mode: carryfs_core::Mode::from_host(0o555),
            ..attributes()
        };
        sink.create_directory(&name("folder"), &hidden_directory)
            .unwrap();
        sink.finish().unwrap();
        for (original, visible) in [
            ("what?", "what\u{F025}"),
            ("CON", "\u{FF23}ON"),
            ("name.", "name\u{F029}"),
        ] {
            let path = root.join(visible);
            assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes());
            assert_ne!(flags(&path) & FILE_ATTRIBUTE_READONLY, 0, "{original}");
            assert_ne!(flags(&path) & FILE_ATTRIBUTE_HIDDEN, 0, "{original}");
        }
        // Windows gives directories no read-only meaning, so none is set.
        assert_eq!(
            flags(&root.join("folder")) & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_READONLY),
            FILE_ATTRIBUTE_HIDDEN
        );
        assert_eq!(flags(&root) & FILE_ATTRIBUTE_HIDDEN, 0);
    }

    #[test]
    fn extraction_links_only_the_object_it_wrote() {
        let temp = Temp::new();
        let root = temp.0.join("output");
        let mut sink = Sink::new(&root).unwrap();
        sink.create_directory(&[], &attributes()).unwrap();
        sink.create_directory(&name("sub"), &attributes()).unwrap();
        let first = [Name::parse("sub").unwrap(), Name::parse("first").unwrap()];
        let mut data = sink.create_file(&first, &file(2, 0o444)).unwrap();
        data.write_all(b"verified").unwrap();
        sink.create_link(&name("second"), &first).unwrap();
        // Its last name is linked, so the object is no longer held open.
        assert!(sink.held.is_empty());
        // Both names reach one object, read-only as stored.
        data.write_all(b" once").unwrap();
        drop(data);
        assert_eq!(
            std::fs::read(root.join("second")).unwrap(),
            b"verified once"
        );
        assert_ne!(flags(&root.join("second")) & FILE_ATTRIBUTE_READONLY, 0);

        // A replaced first name never redirects the link: it reaches the
        // held object or fails, and extraction then copies verified bytes.
        sink.create_file(&name("third"), &file(2, 0o644))
            .unwrap()
            .write_all(b"verified")
            .unwrap();
        std::fs::remove_file(root.join("third")).unwrap();
        std::fs::write(root.join("third"), b"unverified").unwrap();
        if sink.create_link(&name("fourth"), &name("third")).is_ok() {
            assert_eq!(std::fs::read(root.join("fourth")).unwrap(), b"verified");
        } else {
            assert!(!root.join("fourth").exists());
        }
        // A name with no recorded object is refused.
        assert!(sink.create_link(&name("fifth"), &name("missing")).is_err());
        sink.finish().unwrap();
    }

    #[test]
    fn extraction_keeps_source_holes_unallocated() {
        let temp = Temp::new();
        let root = temp.0.join("output");
        let mut sink = Sink::new(&root).unwrap();
        sink.create_directory(&[], &attributes()).unwrap();
        let size = 64 << 20;
        let holes = Attributes {
            size,
            allocated_bytes: 4096,
            ..file(1, 0o644)
        };
        let mut data = sink.create_file(&name("holes"), &holes).unwrap();
        data.set_len(size).unwrap();
        data.seek(SeekFrom::Start(size - 4)).unwrap();
        data.write_all(b"tail").unwrap();
        drop(data);
        let full = Attributes { size: 4, ..holes };
        sink.create_file(&name("full"), &full).unwrap();
        sink.finish().unwrap();
        assert_ne!(flags(&root.join("holes")) & FILE_ATTRIBUTE_SPARSE_FILE, 0);
        assert_eq!(flags(&root.join("full")) & FILE_ATTRIBUTE_SPARSE_FILE, 0);
        let mut bytes = Vec::new();
        File::open(root.join("holes"))
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes.len() as u64, size);
        assert!(bytes[..bytes.len() - 4].iter().all(|byte| *byte == 0));
    }

    /// Needs the symlink privilege, as in an elevated session.
    #[test]
    fn extraction_creates_file_and_directory_symlinks_with_aliased_targets() {
        let temp = Temp::new();
        let root = temp.0.join("output");
        let mut sink = Sink::new(&root).unwrap();
        sink.create_directory(&[], &attributes()).unwrap();
        let link = |directory_link, links| Attributes {
            kind: ObjectKind::Symlink,
            directory_link,
            links,
            hidden: true,
            ..attributes()
        };
        sink.create_symlink(&name("file-link"), b"what?", &link(None, 2))
            .unwrap();
        sink.create_link(&name("again"), &name("file-link"))
            .unwrap();
        sink.create_symlink(&name("dir-link"), b"../CON/x", &link(Some(true), 1))
            .unwrap();
        sink.create_symlink(&name("bad"), b"bad\x01name", &link(None, 1))
            .unwrap();
        let error = sink.finish().unwrap_err();
        assert!(
            error.to_string().starts_with("1 symlinks were not created"),
            "{error}"
        );
        for path in ["file-link", "again"] {
            assert_eq!(
                std::fs::read_link(root.join(path)).unwrap(),
                Path::new("what\u{F025}")
            );
        }
        assert_eq!(
            std::fs::read_link(root.join("dir-link")).unwrap(),
            Path::new("..\\\u{FF23}ON\\x")
        );
        assert_ne!(flags(&root.join("dir-link")) & FILE_ATTRIBUTE_DIRECTORY, 0);
        assert_eq!(flags(&root.join("file-link")) & FILE_ATTRIBUTE_DIRECTORY, 0);
        assert_ne!(flags(&root.join("file-link")) & FILE_ATTRIBUTE_HIDDEN, 0);
        assert!(std::fs::symlink_metadata(root.join("bad")).is_err());
    }
}
