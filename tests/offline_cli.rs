//! Exercise the actual CLI with disposable images and host directories.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "carryfs-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carryfs"))
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap()
    }
    fn pass(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn compare(a: &Path, b: &Path) {
    for entry in fs::read_dir(a).unwrap() {
        let entry = entry.unwrap();
        let other = b.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            compare(&entry.path(), &other);
        } else {
            assert_eq!(fs::read(entry.path()).unwrap(), fs::read(other).unwrap());
        }
    }
    assert_eq!(
        fs::read_dir(a).unwrap().count(),
        fs::read_dir(b).unwrap().count()
    );
}

#[test]
fn cli_round_trips_trees_and_refuses_collisions_and_aliases() {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::symlink;
    let temp = Temp::new();
    let source = temp.0.join("source");
    fs::create_dir(&source).unwrap();
    fs::create_dir(source.join("nested")).unwrap();
    fs::create_dir(source.join("empty-dir")).unwrap();
    fs::write(source.join("empty"), b"").unwrap();
    fs::write(source.join("nested/Notes.txt"), b"mixed tree\n").unwrap();
    let data: Vec<_> = (0..140_007).map(|i| (i % 251) as u8).collect();
    fs::write(source.join("data.bin"), data).unwrap();
    let mut sparse = fs::File::create(source.join("sparse")).unwrap();
    sparse.seek(SeekFrom::Start(4 * 1024 * 1024)).unwrap();
    sparse.write_all(b"tail").unwrap();
    temp.pass(&["mkfs", "image", "268435456"]);
    assert!(
        String::from_utf8_lossy(&temp.pass(&["dump", "image"]).stdout)
            .contains("Format revision: 1")
    );
    temp.pass(&["put", "image", "source", "/backup"]);
    temp.pass(&["check", "image"]);
    temp.pass(&["scrub", "image"]);
    let before = fingerprint(&temp.0.join("image"));
    temp.pass(&["get", "image", "/backup", "recovered"]);
    compare(&source, &temp.0.join("recovered"));
    temp.pass(&["get", "image", "/backup/nested/notes.TXT", "single"]);
    assert_eq!(fs::read(temp.0.join("single")).unwrap(), b"mixed tree\n");
    for args in [
        ["put", "image", "source", "/BACKUP"],
        ["get", "image", "/backup", "recovered"],
        ["get", "image", "/backup/data.bin", "image"],
        ["put", "image", "image", "/self"],
    ] {
        assert!(!temp.run(&args).status.success(), "{args:?}");
    }
    fs::hard_link(temp.0.join("image"), temp.0.join("alias")).unwrap();
    assert!(
        !temp
            .run(&["get", "image", "/backup/data.bin", "alias"])
            .status
            .success()
    );
    assert_eq!(fingerprint(&temp.0.join("image")), before);
    assert!(!temp.run(&["mount", "image"]).status.success());

    // A symlink is stored as a link, never followed, and extracted as one.
    symlink(&source, temp.0.join("source-link")).unwrap();
    assert!(
        temp.run(&["put", "image", "source-link", "/link"])
            .status
            .success()
    );
    assert!(
        temp.run(&["get", "image", "/link", "link-out"])
            .status
            .success()
    );
    assert_eq!(fs::read_link(temp.0.join("link-out")).unwrap(), source);
}

#[test]
#[cfg(target_os = "linux")]
fn cli_refuses_conflicting_image_access_through_any_alias() {
    use std::os::unix::fs::symlink;

    let temp = Temp::new();
    temp.pass(&["mkfs", "image", "268435456"]);
    fs::hard_link(temp.0.join("image"), temp.0.join("hardlink")).unwrap();
    symlink("image", temp.0.join("symlink")).unwrap();
    let writer = fs::OpenOptions::new()
        .write(true)
        .open(temp.0.join("hardlink"))
        .unwrap();
    for path in ["image", "hardlink", "symlink"] {
        for command in ["dump", "check", "scrub"] {
            let output = temp.run(&[command, path]);
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("the image is in use"));
        }
        let output = temp.run(&["get", path, "/", "recovered"]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("the image is in use"));
        assert!(!temp.0.join("recovered").exists());
    }
    drop(writer);
    temp.pass(&["check", "symlink"]);
    fs::write(temp.0.join("source"), b"imported").unwrap();
    let reader = fs::File::open(temp.0.join("symlink")).unwrap();
    let output = temp.run(&["put", "hardlink", "source", "/imported"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("the image is in use"));
    drop(reader);
    temp.pass(&["put", "hardlink", "source", "/imported"]);
}

#[test]
fn cli_reports_damaged_ranges_and_keeps_verified_output() {
    use std::io::{Read, Seek, SeekFrom, Write};
    let temp = Temp::new();
    let bytes = vec![0xB6; 3 * 65_536];
    fs::write(temp.0.join("data"), &bytes).unwrap();
    temp.pass(&["mkfs", "image", "268435456"]);
    temp.pass(&["put", "image", "data", "/data"]);
    // Locate raw payload independently of core's private extent parser.
    let mut image = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(temp.0.join("image"))
        .unwrap();
    let mut block = [0; 4096];
    let mut physical = None;
    for offset in (512 * 4096..268435456 - 512 * 4096).step_by(4096) {
        image.seek(SeekFrom::Start(offset)).unwrap();
        image.read_exact(&mut block).unwrap();
        if block == [0xB6; 4096] {
            physical = Some(offset);
            break;
        }
    }
    image.seek(SeekFrom::Start(physical.unwrap())).unwrap();
    image.write_all(&[0]).unwrap();
    drop(image);
    temp.pass(&["check", "image"]);
    let scrub = temp.run(&["scrub", "image"]);
    assert!(!scrub.status.success());
    assert!(String::from_utf8_lossy(&scrub.stderr).contains("bytes 0..65536"));
    let get = temp.run(&["get", "image", "/data", "recovered"]);
    assert!(!get.status.success());
    assert!(String::from_utf8_lossy(&get.stderr).contains("bytes 0..65536"));
    let recovered = fs::read(temp.0.join("recovered")).unwrap();
    assert_eq!(recovered.len(), bytes.len());
    assert_eq!(&recovered[..65_536], &[0; 65_536]);
    assert_eq!(&recovered[65_536..], &bytes[65_536..]);
}

fn fingerprint(path: &Path) -> u64 {
    use std::hash::Hasher;
    use std::io::Read;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    let mut file = fs::File::open(path).unwrap();
    let mut buffer = [0; 65_536];
    loop {
        let len = file.read(&mut buffer).unwrap();
        if len == 0 {
            return hash.finish();
        }
        hash.write(&buffer[..len]);
    }
}

#[test]
fn cli_keeps_modes_on_images_that_store_them() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &Path| fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777;
    let set = |path: &Path, bits: u32| {
        fs::set_permissions(path, fs::Permissions::from_mode(bits)).unwrap()
    };
    let temp = Temp::new();
    let source = temp.0.join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("script"), b"#!/bin/sh\n").unwrap();
    set(&source.join("script"), 0o755);
    fs::write(source.join("private"), b"secret").unwrap();
    set(&source.join("private"), 0o600);
    fs::create_dir(source.join("locked")).unwrap();
    fs::write(source.join("locked/inside"), b"kept").unwrap();
    set(&source.join("locked/inside"), 0o640);
    set(&source.join("locked"), 0o500);
    set(&source, 0o750);

    temp.pass(&["mkfs", "--attributes", "image", "268435456"]);
    temp.pass(&["put", "image", "source", "/tree"]);
    let attr = |args: &[&str]| String::from_utf8(temp.pass(args).stdout).unwrap();
    assert_eq!(attr(&["attr", "image", "/tree/script"]), "file 0755\n");
    assert_eq!(attr(&["attr", "image", "/tree/locked"]), "directory 0500\n");
    assert_eq!(
        attr(&["attr", "image", "/tree/private", "mode=640", "hidden=yes"]),
        "file 0640 hidden\n"
    );
    assert!(
        !temp
            .run(&["attr", "image", "/tree/private", "mode=4755"])
            .status
            .success()
    );
    temp.pass(&["check", "image"]);

    temp.pass(&["get", "image", "/tree", "out"]);
    let out = temp.0.join("out");
    assert_eq!(
        [
            &out,
            &out.join("script"),
            &out.join("private"),
            &out.join("locked"),
            &out.join("locked/inside")
        ]
        .map(|path| mode(path)),
        [0o750, 0o755, 0o640, 0o500, 0o640]
    );
    assert_eq!(fs::read(out.join("locked/inside")).unwrap(), b"kept");
    for root in [&source, &out] {
        set(&root.join("locked"), 0o700);
    }

    // An image without the feature reads defaults and refuses changes.
    temp.pass(&["mkfs", "plain", "268435456"]);
    temp.pass(&["put", "plain", "source", "/tree"]);
    assert_eq!(attr(&["attr", "plain", "/tree/script"]), "file 0644\n");
    let refused = temp.run(&["attr", "plain", "/tree/script", "mode=755"]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("carryfs upgrade"));
    temp.pass(&["upgrade", "plain"]);
    assert_eq!(
        attr(&["attr", "plain", "/tree/script", "mode=755"]),
        "file 0755\n"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn cli_keeps_hidden_symlinks_without_hiding_their_targets() {
    use std::os::macos::fs::MetadataExt;
    use std::os::unix::fs::symlink;
    let temp = Temp::new();
    let source = temp.0.join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("target"), b"kept").unwrap();
    symlink("target", source.join("link")).unwrap();
    symlink("missing", source.join("dangling")).unwrap();
    assert!(
        Command::new("chflags")
            .args(["-h", "hidden"])
            .arg(source.join("link"))
            .arg(source.join("dangling"))
            .status()
            .unwrap()
            .success()
    );
    temp.pass(&["mkfs", "--attributes", "image", "268435456"]);
    temp.pass(&["put", "image", "source", "/tree"]);
    assert_eq!(
        temp.pass(&["attr", "image", "/tree/link"]).stdout,
        b"symlink 0777 hidden\n"
    );
    temp.pass(&["get", "image", "/tree", "out"]);
    let out = temp.0.join("out");
    assert_ne!(
        fs::symlink_metadata(out.join("link")).unwrap().st_flags() & 0x8000,
        0
    );
    assert_ne!(
        fs::symlink_metadata(out.join("dangling"))
            .unwrap()
            .st_flags()
            & 0x8000,
        0
    );
    assert_eq!(
        fs::metadata(out.join("target")).unwrap().st_flags() & 0x8000,
        0
    );
    assert_eq!(
        fs::read_link(out.join("link")).unwrap(),
        Path::new("target")
    );
    temp.pass(&["check", "image"]);
}
