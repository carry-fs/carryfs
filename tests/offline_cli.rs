//! Exercise the actual CLI with disposable images and host directories.
#![cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

/// A test's scratch directory, held with the lock that runs these tests one
/// at a time. Linux refuses an image lease while any other open file refers to
/// the image, and a child another test thread is spawning briefly holds copies
/// of every descriptor this process has open, its images included.
struct Temp(
    PathBuf,
    #[expect(dead_code, reason = "held only for its lock")] MutexGuard<'static, ()>,
);
impl Temp {
    fn new() -> Self {
        static SERIAL: Mutex<()> = Mutex::new(());
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // A failed test poisons the lock; the next one still needs it.
        let serial = SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "carryfs-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path, serial)
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
    let before = fingerprint(&temp.0.join("image"));
    let result = fsck(&temp, &["--recover", "fsck-out", "image"]);
    assert_eq!(result.status.code(), Some(4));
    let report = fs::read_to_string(temp.0.join("fsck-out.recovery.txt")).unwrap();
    assert!(report.contains("offset Some(0), length 65536"));
    assert_eq!(
        &fs::read(temp.0.join("fsck-out/data")).unwrap()[65_536..],
        &bytes[65_536..]
    );
    assert_eq!(fingerprint(&temp.0.join("image")), before);
    let result = fsck(&temp, &["--repair", "--output", "partial-image", "image"]);
    assert_eq!(
        result.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report = fs::read_to_string(temp.0.join("partial-image.recovery.txt")).unwrap();
    assert!(report.contains("offset Some(0), length 65536"));
    assert!(report.contains("path [\"data\"]"));
    assert!(report.contains("unreadable_ranges: 1"));
    let parsed = Command::new("python3").args(["-c", "import json,sys; d=json.load(open(sys.argv[1])); assert d['state']=='partial'; f=d['files'][0]; assert f['path']==['data']; assert f['status']=='partial'; assert f['source_damage'][0]['offset']==0; assert f['source_damage'][0]['length']==65536", temp.0.join("partial-image.recovery.json").to_str().unwrap()]).output().unwrap();
    assert!(
        parsed.status.success(),
        "{}",
        String::from_utf8_lossy(&parsed.stderr)
    );
    temp.pass(&["check", "partial-image"]);
    temp.pass(&["scrub", "partial-image"]);
    temp.pass(&["get", "partial-image", "/data", "partial-copy"]);
    let partial = fs::read(temp.0.join("partial-copy")).unwrap();
    assert_eq!(&partial[..65_536], &[0; 65_536]);
    assert_eq!(&partial[65_536..], &bytes[65_536..]);
    assert_eq!(fingerprint(&temp.0.join("image")), before);
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

fn fsck(temp: &Temp, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fsck-carryfs"))
        .current_dir(&temp.0)
        .args(args)
        .output()
        .unwrap()
}

fn disk_root(temp: &Temp) -> carryfs_ondisk::format::Root {
    use std::io::{Read, Seek, SeekFrom};
    let mut image = fs::File::open(temp.0.join("image")).unwrap();
    let mut page = [0; 4096];
    image.seek(SeekFrom::Start(256 * 4096)).unwrap();
    image.read_exact(&mut page).unwrap();
    carryfs_ondisk::format::Root::decode(&page, 256, 268435456 / 4096).unwrap()
}

fn damage_page(temp: &Temp, block: u64) {
    use std::io::{Seek, SeekFrom, Write};
    let mut image = fs::OpenOptions::new()
        .write(true)
        .open(temp.0.join("image"))
        .unwrap();
    image.seek(SeekFrom::Start(block * 4096)).unwrap();
    image.write_all(&[0xff]).unwrap();
}

#[test]
fn fsck_plans_exports_and_repairs_the_original_offline() {
    let temp = Temp::new();
    let bytes = vec![0x79; 150_001];
    fs::write(temp.0.join("source"), &bytes).unwrap();
    temp.pass(&["mkfs", "image", "268435456"]);
    temp.pass(&["put", "image", "source", "/notes"]);
    damage_page(&temp, disk_root(&temp).allocation.block);
    let before = fingerprint(&temp.0.join("image"));
    let plan = fsck(&temp, &["--plan", "image"]);
    assert_eq!(
        plan.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    assert!(
        String::from_utf8_lossy(&plan.stdout).contains("rebuild catalog, allocation and cleanup")
    );
    assert_eq!(fingerprint(&temp.0.join("image")), before);
    assert!(!temp.0.join("image.repair.txt").exists());
    let export = fsck(&temp, &["--recover", "recovered", "image"]);
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    assert_eq!(fs::read(temp.0.join("recovered/notes")).unwrap(), bytes);
    assert!(temp.0.join("recovered.recovery.txt").exists());
    assert_eq!(fingerprint(&temp.0.join("image")), before);
    fs::write(temp.0.join("existing"), b"keep").unwrap();
    assert_eq!(
        fsck(&temp, &["--repair", "--report", "existing", "image"])
            .status
            .code(),
        Some(8)
    );
    assert_eq!(fs::read(temp.0.join("existing")).unwrap(), b"keep");
    assert_eq!(fingerprint(&temp.0.join("image")), before);
    let repair = fsck(&temp, &["--repair", "image"]);
    assert_eq!(
        repair.status.code(),
        Some(1),
        "{}\n{}",
        String::from_utf8_lossy(&repair.stdout),
        String::from_utf8_lossy(&repair.stderr)
    );
    assert!(
        fs::read_to_string(temp.0.join("image.repair.txt"))
            .unwrap()
            .contains("Repair committed")
    );
    temp.pass(&["check", "image"]);
    temp.pass(&["scrub", "image"]);
    temp.pass(&["get", "image", "/notes", "verified"]);
    assert_eq!(fs::read(temp.0.join("verified")).unwrap(), bytes);
    temp.pass(&["put", "image", "source", "/after-repair"]);
    temp.pass(&["check", "image"]);
    temp.pass(&["scrub", "image"]);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_carryfs"), temp.0.join("fsck.carryfs")).unwrap();
    assert!(
        Command::new(temp.0.join("fsck.carryfs"))
            .current_dir(&temp.0)
            .arg("image")
            .status()
            .unwrap()
            .success()
    );
    let repaired = fingerprint(&temp.0.join("image"));
    assert_eq!(fsck(&temp, &["--repair", "image"]).status.code(), Some(0));
    temp.pass(&["fsck", "image"]);
    assert_eq!(fingerprint(&temp.0.join("image")), repaired);
}

#[test]
fn fsck_repair_and_rebuild_keep_every_name_of_a_hard_linked_file() {
    use std::os::unix::fs::MetadataExt;
    let temp = Temp::new();
    fs::create_dir_all(temp.0.join("source/sub")).unwrap();
    fs::write(temp.0.join("source/a"), b"shared").unwrap();
    fs::hard_link(temp.0.join("source/a"), temp.0.join("source/sub/b")).unwrap();
    temp.pass(&["mkfs", "image", "268435456"]);
    temp.pass(&["put", "image", "source", "/backup"]);
    damage_page(&temp, disk_root(&temp).allocation.block);
    // Rebuild into a new image first; it leaves the damaged source untouched.
    for args in [
        &["--repair", "--output", "new-image", "image"][..],
        &["--repair", "image"],
    ] {
        let result = fsck(&temp, args);
        assert_eq!(
            result.status.code(),
            Some(1),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    for image in ["image", "new-image"] {
        temp.pass(&["check", image]);
        let out = temp.0.join(format!("{image}.out"));
        temp.pass(&["get", image, "/backup", out.to_str().unwrap()]);
        let (a, b) = (out.join("a"), out.join("sub/b"));
        assert_eq!(
            fs::metadata(&a).unwrap().ino(),
            fs::metadata(&b).unwrap().ino(),
            "{image}"
        );
        assert_eq!(fs::read(&b).unwrap(), b"shared");
    }
}

#[test]
fn fsck_refuses_implicit_data_loss_and_exports_with_a_broken_workspace() {
    let temp = Temp::new();
    fs::write(temp.0.join("source"), b"keep these bytes").unwrap();
    temp.pass(&["mkfs", "image", "268435456"]);
    temp.pass(&["put", "image", "source", "/notes"]);
    damage_page(&temp, disk_root(&temp).workspace.block);
    let before = fingerprint(&temp.0.join("image"));
    let export = fsck(&temp, &["--recover", "recovered", "image"]);
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    assert_eq!(
        fs::read(temp.0.join("recovered/notes")).unwrap(),
        b"keep these bytes"
    );
    assert_eq!(fsck(&temp, &["--repair", "image"]).status.code(), Some(4));
    assert_eq!(fingerprint(&temp.0.join("image")), before);
    // A fresh volume with its catalog root destroyed requires explicit loss
    // acceptance, and the report remains next to the repaired original.
    temp.pass(&["mkfs", "fresh", "268435456"]);
    fs::rename(temp.0.join("image"), temp.0.join("workspace-damaged")).unwrap();
    fs::rename(temp.0.join("fresh"), temp.0.join("image")).unwrap();
    damage_page(&temp, disk_root(&temp).catalog.block);
    let before = fingerprint(&temp.0.join("image"));
    assert_eq!(fsck(&temp, &["--repair", "image"]).status.code(), Some(4));
    assert_eq!(fingerprint(&temp.0.join("image")), before);
    let copy = fsck(
        &temp,
        &["--repair", "--output", "metadata-partial", "image"],
    );
    assert_eq!(
        copy.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&copy.stderr)
    );
    assert_eq!(fingerprint(&temp.0.join("image")), before);
    temp.pass(&["check", "metadata-partial"]);
    assert!(
        fs::read_to_string(temp.0.join("metadata-partial.recovery.txt"))
            .unwrap()
            .contains("complete: false")
    );
    let result = fsck(&temp, &["--repair", "--accept-data-loss", "image"]);
    assert_eq!(
        result.status.code(),
        Some(1),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    temp.pass(&["check", "image"]);
    assert!(
        fs::read_to_string(temp.0.join("image.repair.txt"))
            .unwrap()
            .contains("possible data loss: true")
    );
    damage_page(&temp, 256);
    damage_page(&temp, 268435456 / 4096 - 257);
    let before = fingerprint(&temp.0.join("image"));
    assert_eq!(fsck(&temp, &["--plan", "image"]).status.code(), Some(4));
    assert_eq!(fingerprint(&temp.0.join("image")), before);
}

#[cfg(target_os = "linux")]
#[test]
fn fsck_refuses_busy_images_before_planning_or_creating_output() {
    let temp = Temp::new();
    temp.pass(&["mkfs", "image", "268435456"]);
    let reader = fs::File::open(temp.0.join("image")).unwrap();
    let output = fsck(&temp, &["--repair", "image"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("image is in use"));
    assert!(!temp.0.join("image.repair.txt").exists());
    drop(reader);
    let writer = fs::OpenOptions::new()
        .write(true)
        .open(temp.0.join("image"))
        .unwrap();
    assert!(
        !fsck(&temp, &["--recover", "recovered", "image"])
            .status
            .success()
    );
    assert!(!temp.0.join("recovered").exists());
    drop(writer);
}

#[test]
fn fsck_rebuilds_a_new_image_without_repairing_the_source() {
    let temp = Temp::new();
    let bytes = vec![0x79; 150_001];
    fs::write(temp.0.join("source"), &bytes).unwrap();
    temp.pass(&["mkfs", "--attributes", "image", "268435456"]);
    temp.pass(&["put", "image", "source", "/notes"]);
    temp.pass(&["attr", "image", "/notes", "mode=400", "hidden=yes"]);
    let original_uuid = disk_root(&temp).uuid;
    damage_page(&temp, disk_root(&temp).workspace.block);
    let before = fingerprint(&temp.0.join("image"));
    // A concurrent read handle is compatible with a read-only source lease.
    let reader = fs::File::open(temp.0.join("image")).unwrap();
    let result = fsck(
        &temp,
        &[
            "--repair",
            "--output",
            "new-image",
            "--verify-data",
            "image",
        ],
    );
    assert_eq!(
        result.status.code(),
        Some(1),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    drop(reader);
    assert_eq!(fingerprint(&temp.0.join("image")), before);
    assert_eq!(
        fs::metadata(temp.0.join("new-image")).unwrap().len(),
        268435456
    );
    assert!(!temp.run(&["check", "image"]).status.success());
    temp.pass(&["check", "new-image"]);
    temp.pass(&["scrub", "new-image"]);
    temp.pass(&["get", "new-image", "/notes", "verified"]);
    assert_eq!(fs::read(temp.0.join("verified")).unwrap(), bytes);
    assert_eq!(
        String::from_utf8(temp.pass(&["attr", "new-image", "/notes"]).stdout).unwrap(),
        "file 0400 hidden\n"
    );
    let mut volume = carryfs_core::Volume::open(
        carryfs_core::ImageDevice::open(
            temp.0.join("new-image"),
            carryfs_core::MountMode::ReadOnly,
        )
        .unwrap(),
    )
    .unwrap();
    assert_ne!(volume.info().unwrap().uuid, original_uuid);
    volume.eject().unwrap();
    let report = fs::read_to_string(temp.0.join("new-image.recovery.txt")).unwrap();
    assert!(report.contains("Image recovery finished; structural verification passed"));
    assert!(report.contains("complete: true"));
    temp.pass(&["get", "new-image", "/RECOVERY-REPORT.json", "embedded.json"]);
    assert_eq!(
        fs::read(temp.0.join("embedded.json")).unwrap(),
        fs::read(temp.0.join("new-image.recovery.json")).unwrap()
    );
    let parsed = Command::new("python3").args(["-c", "import json,sys; d=json.load(open(sys.argv[1])); assert d['state']=='complete'; assert d['verification']=='passed'; assert d['source']['metadata_complete']; assert d['files'][0]['path']==['notes']; assert d['files'][0]['copied_bytes']==150001; assert d['files'][0]['destination_verified_bytes']==150001", temp.0.join("embedded.json").to_str().unwrap()]).output().unwrap();
    assert!(
        parsed.status.success(),
        "{}",
        String::from_utf8_lossy(&parsed.stderr)
    );
    temp.pass(&["put", "new-image", "source", "/new-write"]);
    temp.pass(&["check", "new-image"]);
}

#[test]
fn fsck_new_image_refuses_existing_outputs_aliases_reports_and_invalid_flags() {
    use std::os::unix::fs::symlink;
    let temp = Temp::new();
    temp.pass(&["mkfs", "image", "268435456"]);
    fs::write(temp.0.join("existing"), b"keep").unwrap();
    fs::hard_link(temp.0.join("image"), temp.0.join("hardlink")).unwrap();
    symlink("image", temp.0.join("symlink")).unwrap();
    symlink("missing", temp.0.join("dangling")).unwrap();
    let before = fingerprint(&temp.0.join("image"));
    fs::write(temp.0.join("json-collision.recovery.json"), b"keep-json").unwrap();
    assert_eq!(
        fsck(&temp, &["--repair", "--output", "json-collision", "image"])
            .status
            .code(),
        Some(8)
    );
    assert!(!temp.0.join("json-collision").exists());
    assert_eq!(
        fs::read(temp.0.join("json-collision.recovery.json")).unwrap(),
        b"keep-json"
    );

    for output in ["existing", "image", "hardlink", "symlink", "dangling"] {
        let result = fsck(&temp, &["--repair", "--output", output, "image"]);
        assert_eq!(
            result.status.code(),
            Some(8),
            "{output}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    assert_eq!(fs::read(temp.0.join("existing")).unwrap(), b"keep");
    assert_eq!(
        fs::read_link(temp.0.join("symlink")).unwrap(),
        Path::new("image")
    );
    assert_eq!(
        fs::read_link(temp.0.join("dangling")).unwrap(),
        Path::new("missing")
    );
    assert!(!temp.0.join("missing").exists());
    assert_eq!(
        fsck(
            &temp,
            &[
                "--repair",
                "--output",
                "not-created",
                "--report",
                "existing",
                "image"
            ]
        )
        .status
        .code(),
        Some(8)
    );
    assert!(!temp.0.join("not-created").exists());
    for arguments in [
        vec!["--output", "invalid", "image"],
        vec!["--repair", "--verify-data", "image"],
        vec!["--repair", "--plan", "--output", "invalid", "image"],
        vec![
            "--repair",
            "--recover",
            "directory",
            "--output",
            "invalid",
            "image",
        ],
        vec!["image", "--repair", "--output"],
    ] {
        assert_eq!(
            fsck(&temp, &arguments).status.code(),
            Some(16),
            "{arguments:?}"
        );
    }
    assert!(!temp.0.join("invalid").exists());
    assert_eq!(fingerprint(&temp.0.join("image")), before);
    let writer = fs::OpenOptions::new()
        .write(true)
        .open(temp.0.join("image"))
        .unwrap();
    #[cfg(target_os = "linux")]
    {
        assert!(
            !fsck(&temp, &["--repair", "--output", "busy-output", "image"])
                .status
                .success()
        );
        assert!(!temp.0.join("busy-output").exists());
        assert!(!temp.0.join("busy-output.recovery.txt").exists());
    }
    drop(writer);
}

#[test]
fn cli_keeps_hard_links_through_put_and_get() {
    use std::os::unix::fs::MetadataExt;
    let temp = Temp::new();
    let source = temp.0.join("source");
    fs::create_dir_all(source.join("sub")).unwrap();
    fs::write(source.join("a"), b"shared").unwrap();
    fs::hard_link(source.join("a"), source.join("sub/b")).unwrap();
    std::os::unix::fs::symlink("a", source.join("s")).unwrap();
    fs::hard_link(source.join("s"), source.join("sub/s2")).unwrap();
    let inode = |path: &Path| fs::symlink_metadata(path).unwrap().ino();
    assert_eq!(inode(&source.join("s")), inode(&source.join("sub/s2")));

    temp.pass(&["mkfs", "image", "268435456"]);
    temp.pass(&["put", "image", "source", "/backup"]);
    temp.pass(&["check", "image"]);
    temp.pass(&["get", "image", "/backup", "recovered"]);
    let recovered = temp.0.join("recovered");
    for (first, second) in [("a", "sub/b"), ("s", "sub/s2")] {
        let (first, second) = (recovered.join(first), recovered.join(second));
        assert_eq!(inode(&first), inode(&second), "{first:?}");
        assert_eq!(fs::symlink_metadata(&first).unwrap().nlink(), 2);
    }
    assert_eq!(fs::read(recovered.join("sub/b")).unwrap(), b"shared");
    assert_eq!(
        fs::read_link(recovered.join("sub/s2")).unwrap(),
        Path::new("a")
    );
}

/// A privileged parent passes an opened device as `/dev/fd/0`.
#[test]
#[cfg(target_os = "macos")]
#[ignore = "attaches a disk image with hdiutil"]
fn cli_reads_a_device_passed_as_an_open_descriptor() {
    let temp = Temp::new();
    temp.pass(&["mkfs", "image", "268435456"]);
    let image = temp.0.join("image");
    damage_page(&temp, disk_root(&temp).allocation.block);
    let attach = Command::new("hdiutil")
        .args([
            "attach",
            "-nomount",
            "-imagekey",
            "diskimage-class=CRawDiskImage",
        ])
        .arg(&image)
        .output()
        .unwrap();
    assert!(
        attach.status.success(),
        "{}",
        String::from_utf8_lossy(&attach.stderr)
    );
    let disk = String::from_utf8_lossy(&attach.stdout)
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let raw = disk.replace("/dev/disk", "/dev/rdisk");
    // Like a root-only /dev/rdiskN: the CLI can use the descriptors it is
    // given but cannot open the node itself.
    let devices = [fs::File::open(&raw).unwrap(), fs::File::open(&raw).unwrap()];
    let mode = |bits: u32| {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&raw, fs::Permissions::from_mode(bits)).unwrap()
    };
    mode(0o000);
    let [plan, check] = devices;
    let run = |args: &[&str], device: fs::File| {
        Command::new(env!("CARGO_BIN_EXE_carryfs"))
            .current_dir(&temp.0)
            .args(args)
            .stdin(device)
            .output()
            .unwrap()
    };
    let plan = run(
        &["fsck", "--plan", "--bytes", "268435456", "--", "/dev/fd/0"],
        plan,
    );
    let check = run(&["check", "--bytes", "268435456", "/dev/fd/0"], check);
    mode(0o640);
    Command::new("hdiutil")
        .args(["detach", &disk])
        .output()
        .unwrap();
    assert_eq!(
        plan.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    assert!(
        String::from_utf8_lossy(&check.stderr).contains("corrupt"),
        "check reads the same damaged device"
    );
}
