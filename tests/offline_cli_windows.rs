//! Exercise `get` and `fsck --recover` on Windows. `put` is Unix-only, so the
//! images are written through the core library.
#![cfg(windows)]

use std::fs;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use carryfs_core::{
    AttributeChange, ImageDevice, Mode, Name, NewAttributes, ObjectId, ROOT, Volume,
};

const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
const IMAGE_BYTES: u64 = 268_435_456;

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "carryfs-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn run(&self, program: &str, args: &[&str]) -> Output {
        Command::new(program)
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap()
    }
    fn get(&self, args: &[&str]) -> Output {
        self.run(env!("CARGO_BIN_EXE_carryfs"), &[&["get"], args].concat())
    }
    fn fsck(&self, args: &[&str]) -> Output {
        self.run(env!("CARGO_BIN_EXE_fsck-carryfs"), args)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        clear_read_only(&self.0);
        fs::remove_dir_all(&self.0).unwrap();
    }
}

/// Extracted read-only files would stop the removal.
#[expect(
    clippy::permissions_set_readonly_false,
    reason = "on Windows this clears only the read-only flag"
)]
fn clear_read_only(path: &Path) {
    for entry in fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        let metadata = fs::symlink_metadata(&path).unwrap();
        if metadata.is_dir() && !metadata.is_symlink() {
            clear_read_only(&path);
        } else if metadata.file_attributes() & FILE_ATTRIBUTE_READONLY != 0 {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(false);
            fs::set_permissions(&path, permissions).unwrap();
        }
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn name(text: &str) -> Name {
    Name::parse(text).unwrap()
}

fn write_file(
    volume: &mut Volume<ImageDevice>,
    parent: ObjectId,
    text: &str,
    attributes: NewAttributes,
    pieces: &[(u64, &[u8])],
) -> ObjectId {
    let handle = volume
        .create_file_with(parent, name(text), attributes)
        .unwrap();
    for (offset, bytes) in pieces {
        assert_eq!(volume.write(&handle, *offset, bytes).unwrap(), bytes.len());
    }
    volume.close_handle(handle).unwrap();
    volume.lookup(parent, &name(text)).unwrap()
}

/// An image holding `/tree` with names Windows forbids, flags, a hole, hard
/// links, and file and directory symlinks.
fn image(temp: &Temp) -> PathBuf {
    let path = temp.0.join("image");
    let mut volume = Volume::format(ImageDevice::create_new(&path, IMAGE_BYTES).unwrap()).unwrap();
    let plain = NewAttributes::default();
    let tree = volume
        .create_directory_with(ROOT, name("tree"), plain)
        .unwrap();
    let nested = volume
        .create_directory_with(tree, name("nested"), plain)
        .unwrap();
    write_file(
        &mut volume,
        nested,
        "Notes.txt",
        plain,
        &[(0, b"mixed tree\n")],
    );
    let data: Vec<_> = (0..140_007).map(|i| (i % 251) as u8).collect();
    write_file(&mut volume, tree, "data.bin", plain, &[(0, &data)]);
    write_file(&mut volume, tree, "sparse", plain, &[(4 << 20, b"tail")]);
    for forbidden in ["what?", "CON", "name."] {
        write_file(
            &mut volume,
            tree,
            forbidden,
            plain,
            &[(0, forbidden.as_bytes())],
        );
    }
    let flagged = NewAttributes {
        mode: Some(Mode::from_host(0o444)),
        hidden: true,
    };
    write_file(&mut volume, tree, "flagged", flagged, &[(0, b"flags")]);
    let shared = write_file(&mut volume, tree, "a", plain, &[(0, b"shared")]);
    volume.link(shared, nested, name("b")).unwrap();
    let link = volume
        .create_symlink(tree, name("link"), b"data.bin")
        .unwrap();
    let handle = volume.retain_object(link).unwrap();
    volume
        .set_attributes(
            &handle,
            AttributeChange {
                hidden: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    volume.close_handle(handle).unwrap();
    // A link Windows made to a directory, which it records as one.
    volume
        .create_directory_with(tree, name("dir-link"), plain)
        .unwrap();
    volume
        .make_symlink(tree, &name("dir-link"), b"nested")
        .unwrap();
    volume.eject().unwrap();
    path
}

fn flags(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().file_attributes()
}

#[test]
fn get_extracts_trees_with_aliases_flags_holes_links_and_symlinks() {
    let temp = Temp::new();
    image(&temp);
    // Symlinks need the privilege an elevated session holds.
    assert_success(&temp.get(&["image", "/tree", "out"]));
    let out = temp.0.join("out");
    assert_eq!(
        fs::read(out.join("nested").join("Notes.txt")).unwrap(),
        b"mixed tree\n"
    );
    let data: Vec<_> = (0..140_007).map(|i| (i % 251) as u8).collect();
    assert_eq!(fs::read(out.join("data.bin")).unwrap(), data);
    let sparse = fs::read(out.join("sparse")).unwrap();
    assert_eq!(sparse.len(), (4 << 20) + 4);
    assert!(sparse[..4 << 20].iter().all(|byte| *byte == 0));
    assert_ne!(flags(&out.join("sparse")) & FILE_ATTRIBUTE_SPARSE_FILE, 0);
    for (original, visible) in [
        ("what?", "what\u{F025}"),
        ("CON", "\u{FF23}ON"),
        ("name.", "name\u{F029}"),
    ] {
        assert_eq!(
            fs::read(out.join(visible)).unwrap(),
            original.as_bytes(),
            "{original}"
        );
    }
    let flagged = flags(&out.join("flagged"));
    assert_ne!(flagged & FILE_ATTRIBUTE_READONLY, 0);
    assert_ne!(flagged & FILE_ATTRIBUTE_HIDDEN, 0);
    assert_eq!(flags(&out.join("data.bin")) & FILE_ATTRIBUTE_READONLY, 0);
    // One object under both names.
    std::io::Write::write_all(
        &mut fs::OpenOptions::new()
            .append(true)
            .open(out.join("a"))
            .unwrap(),
        b" once",
    )
    .unwrap();
    assert_eq!(
        fs::read(out.join("nested").join("b")).unwrap(),
        b"shared once"
    );
    assert_eq!(
        fs::read_link(out.join("link")).unwrap(),
        Path::new("data.bin")
    );
    assert_ne!(flags(&out.join("link")) & FILE_ATTRIBUTE_HIDDEN, 0);
    assert_eq!(fs::read(out.join("link")).unwrap(), data);
    assert_ne!(flags(&out.join("dir-link")) & FILE_ATTRIBUTE_DIRECTORY, 0);
    assert_eq!(
        fs::read(out.join("dir-link").join("Notes.txt")).unwrap(),
        b"mixed tree\n"
    );

    assert_success(&temp.get(&["image", "/tree/nested/notes.TXT", "single"]));
    assert_eq!(fs::read(temp.0.join("single")).unwrap(), b"mixed tree\n");
}

#[test]
fn get_refuses_existing_destinations_and_junctions() {
    let temp = Temp::new();
    image(&temp);
    let outside = temp.0.join("outside");
    fs::create_dir(&outside).unwrap();
    let junction = temp.0.join("junction");
    let made = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&outside)
        .output()
        .unwrap();
    assert_success(&made);
    let before = fs::read(temp.0.join("image")).unwrap();
    for destination in ["image", "IMAGE", "junction", "junction\\new", "CON", "out."] {
        let output = temp.get(&["image", "/tree", destination]);
        assert!(!output.status.success(), "{destination}");
    }
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    assert_eq!(fs::read(temp.0.join("image")).unwrap(), before);
}

fn damage_allocation(image: &Path) {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .unwrap();
    let mut page = [0; 4096];
    file.seek(SeekFrom::Start(256 * 4096)).unwrap();
    file.read_exact(&mut page).unwrap();
    let root = carryfs_ondisk::format::Root::decode(&page, 256, IMAGE_BYTES / 4096).unwrap();
    file.seek(SeekFrom::Start(root.allocation.block * 4096))
        .unwrap();
    file.write_all(&[0xff]).unwrap();
}

#[test]
fn fsck_recovers_a_damaged_image_to_a_new_folder() {
    let temp = Temp::new();
    let image = image(&temp);
    damage_allocation(&image);
    let before = fs::read(&image).unwrap();
    let plan = temp.fsck(&["--plan", "image"]);
    assert_eq!(plan.status.code(), Some(4));
    let export = temp.fsck(&["--recover", "recovered", "image"]);
    assert_success(&export);
    let recovered = temp.0.join("recovered").join("tree");
    assert_eq!(
        fs::read(recovered.join("nested").join("Notes.txt")).unwrap(),
        b"mixed tree\n"
    );
    assert_eq!(fs::read(recovered.join("what\u{F025}")).unwrap(), b"what?");
    assert_ne!(flags(&recovered.join("flagged")) & FILE_ATTRIBUTE_HIDDEN, 0);
    assert_eq!(
        fs::read_link(recovered.join("link")).unwrap(),
        Path::new("data.bin")
    );
    assert!(temp.0.join("recovered.recovery.txt").exists());
    assert_eq!(fs::read(&image).unwrap(), before);
    // A second export refuses the existing folder and keeps it.
    fs::remove_file(temp.0.join("recovered.recovery.txt")).unwrap();
    assert_eq!(
        temp.fsck(&["--recover", "recovered", "image"])
            .status
            .code(),
        Some(8)
    );
    assert!(recovered.join("data.bin").exists());
}
