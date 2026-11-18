use super::*;
use carryfs_core::ObjectKind;
use carryfs_core::{ImageDevice, MountMode, Name, ROOT, Volume};
use model::bytes;
use platform::DeviceKind;
use rune::{Env, FileDialogKind, Runtime};
use std::{
    fs,
    hash::{Hash, Hasher},
    io::Read,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};
use view::{device_rows, view};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "carryfs-manager-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let mut volume = Volume::format(
            ImageDevice::create_new(path.join("test.cfs"), 256 * 1024 * 1024).unwrap(),
        )
        .unwrap();
        let folder = volume
            .create_directory(ROOT, Name::parse("Documents").unwrap())
            .unwrap();
        let empty = volume
            .create_directory(folder, Name::parse("Empty").unwrap())
            .unwrap();
        assert_ne!(folder, empty);
        let file = volume
            .create_file(ROOT, Name::parse("readme.txt").unwrap())
            .unwrap();
        volume.write(&file, 0, b"CarryFS export test\n").unwrap();
        volume.close_handle(file).unwrap();
        for index in 0..70 {
            let file = volume
                .create_file(
                    folder,
                    Name::parse(&format!("file-{index:02}.txt")).unwrap(),
                )
                .unwrap();
            volume.close_handle(file).unwrap();
        }
        volume.eject().unwrap();
        Self(path)
    }
    fn image(&self) -> PathBuf {
        self.0.join("test.cfs")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn hash(path: &Path) -> u64 {
    let mut file = fs::File::open(path).unwrap();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let n = file.read(&mut buffer).unwrap();
        if n == 0 {
            break;
        }
        buffer[..n].hash(&mut hasher);
    }
    hasher.finish()
}

fn runtime() -> Runtime<State> {
    let state = State::empty();
    let mut rt = Runtime::new(
        state,
        view,
        (1040.0, 700.0),
        Env {
            system_dark: false,
            scale: 1.0,
        },
    );
    rt.poll = Some(poll);
    rt.frame(0.0);
    rt
}

fn click(rt: &mut Runtime<State>, key: &str) {
    let (x, y) = rt
        .find(key)
        .unwrap_or_else(|| panic!("Missing element {key}"));
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(1.0 / 60.0);
}

fn finish(rt: &mut Runtime<State>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while rt.state.pending.is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "Filesystem worker timed out"
        );
        rt.frame(1.0 / 60.0);
        std::thread::sleep(Duration::from_millis(1));
    }
    for _ in 0..60 {
        rt.frame(1.0 / 60.0);
    }
    assert!(!rt.state.error, "{}", rt.state.status);
}

/// Wait for the running background check to report.
fn settle_checks(rt: &mut Runtime<State>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while rt.state.background.is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "Background check timed out"
        );
        rt.frame(1.0 / 60.0);
        std::thread::sleep(Duration::from_millis(1));
    }
    rt.frame(1.0 / 60.0);
}

/// Run First Aid from the toolbar and wait for its report.
fn first_aid(rt: &mut Runtime<State>) {
    click(rt, "first-aid");
    click(rt, "confirm-first-aid");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while rt.state.pending.is_some() {
        assert!(std::time::Instant::now() < deadline, "First Aid timed out");
        rt.frame(1.0 / 60.0);
        std::thread::sleep(Duration::from_millis(1));
    }
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    assert_eq!(
        rt.state.sheet.as_ref().map(|s| s.phase),
        Some(Phase::Done),
        "{}",
        rt.state.status
    );
}

#[test]
fn ui_opens_browses_checks_exports_and_closes_a_real_image() {
    let fixture = Fixture::new();
    let before = hash(&fixture.image());
    let mut rt = runtime();
    click(&mut rt, "open-image");
    assert_eq!(rt.file_dialog().unwrap().0, FileDialogKind::Open);
    rt.resolve_file_dialog(Some(fixture.image())).unwrap();
    rt.frame(0.01);
    finish(&mut rt);
    click(&mut rt, "tab-files");
    assert!(
        rt.presented("file-list").unwrap().0[2] > 600.0,
        "The file table fills its workspace"
    );
    let browser = rt.state.browser.as_ref().unwrap();
    assert_eq!(browser.entries.len(), 2);
    let folder = browser.entries[0].item.object;
    click(&mut rt, &format!("entry-{}", folder.get()));
    click(&mut rt, "selection-action");
    finish(&mut rt);
    assert_eq!(
        rt.state.browser.as_ref().unwrap().entries.len(),
        71,
        "All directory pages are listed"
    );
    let empty = rt.state.browser.as_ref().unwrap().entries[0].item.object;
    click(&mut rt, &format!("entry-{}", empty.get()));
    click(&mut rt, "selection-action");
    finish(&mut rt);
    assert!(rt.state.browser.as_ref().unwrap().entries.is_empty());
    click(&mut rt, "up");
    finish(&mut rt);
    click(&mut rt, "crumb-0");
    finish(&mut rt);
    click(&mut rt, "tab-overview");
    first_aid(&mut rt);
    assert_eq!(rt.state.status, "First Aid found no problems.");
    click(&mut rt, "close-sheet");
    assert!(rt.state.sheet.is_none());
    click(&mut rt, "tab-files");
    let file = rt.state.browser.as_ref().unwrap().entries[1].item.object;
    click(&mut rt, &format!("entry-{}", file.get()));
    click(&mut rt, "selection-action");
    assert_eq!(rt.file_dialog().unwrap().0, FileDialogKind::Save);
    let output = fixture.0.join("export.txt");
    rt.resolve_file_dialog(Some(output.clone())).unwrap();
    rt.frame(0.01);
    finish(&mut rt);
    assert_eq!(fs::read(output).unwrap(), b"CarryFS export test\n");
    click(&mut rt, "tab-overview");
    click(&mut rt, "close-image");
    assert!(rt.state.browser.is_none());
    assert_eq!(before, hash(&fixture.image()), "No image bytes changed");
}

#[test]
fn opening_errors_and_dialog_cancellation_preserve_the_current_image() {
    let fixture = Fixture::new();
    let mut rt = runtime();
    rt.state.apply(Ok(Update::Browser(Box::new(
        Browser::open(fixture.image()).unwrap(),
    ))));
    rt.frame(0.01);
    click(&mut rt, "open-image");
    rt.resolve_file_dialog(None).unwrap();
    rt.frame(0.01);
    assert!(rt.state.pending.is_none());
    click(&mut rt, "open-image");
    let invalid = fixture.0.join("invalid.cfs");
    fs::write(&invalid, b"not a filesystem").unwrap();
    rt.resolve_file_dialog(Some(invalid)).unwrap();
    rt.frame(0.01);
    let result = rt
        .state
        .pending
        .take()
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    rt.update(|s, _| s.apply(result));
    assert!(rt.state.error);
    assert_eq!(rt.state.browser.as_ref().unwrap().path, fixture.image());
}

#[test]
fn exports_stream_large_files_and_refuse_existing_destinations() {
    let fixture = Fixture::new();
    let data = vec![0x5a; 2 * 1024 * 1024 + 17];
    let mut volume =
        Volume::open(ImageDevice::open(fixture.image(), MountMode::ReadWrite).unwrap()).unwrap();
    let file = volume
        .create_file(ROOT, Name::parse("large.bin").unwrap())
        .unwrap();
    let mut offset = 0;
    while offset < data.len() {
        offset += volume.write(&file, offset as u64, &data[offset..]).unwrap();
    }
    volume.close_handle(file).unwrap();
    volume.eject().unwrap();
    drop(volume);
    let browser = Browser::open(fixture.image()).unwrap();
    let object = browser
        .entries
        .iter()
        .find(|e| e.item.name.stored() == b"large.bin")
        .unwrap()
        .item
        .object;
    let output = fixture.0.join("large.bin");
    browser.export(object, &output).unwrap();
    assert_eq!(fs::read(&output).unwrap(), data);
    assert!(browser.export(object, &output).is_err());
    assert_eq!(fs::read(&output).unwrap(), data);
    assert!(browser.export(object, &fixture.image()).is_err());
    assert!(browser.export(ROOT, &fixture.0.join("folder")).is_err());
}

#[test]
fn capacities_use_binary_units() {
    assert_eq!(bytes(0), "0 B");
    assert_eq!(bytes(1023), "1023 B");
    assert_eq!(bytes(1024), "1.0 KiB");
    assert_eq!(bytes(256 * 1024 * 1024), "256.0 MiB");
}

#[test]
fn ui_scrubs_recovers_folders_and_records_results_with_the_source() {
    let fixture = Fixture::new();
    let before = hash(&fixture.image());
    let mut rt = runtime();
    rt.update(|s, _| {
        s.apply(Ok(Update::Browser(Box::new(
            Browser::open(fixture.image()).unwrap(),
        ))))
    });
    rt.frame(0.01);
    first_aid(&mut rt);
    let key = {
        let b = rt.state.browser.as_ref().unwrap();
        (b.info.uuid, b.info.generation)
    };
    let scrub = rt.state.integrity[&key].scrub.as_ref().unwrap();
    assert!(
        scrub.summary.contains("0 unreadable ranges"),
        "{}",
        scrub.summary
    );
    click(&mut rt, "close-sheet");
    click(&mut rt, "recover-tool");
    click(&mut rt, "recover");
    let destination = fixture.0.join("recovered");
    rt.resolve_file_dialog(Some(destination.clone())).unwrap();
    rt.frame(0.01);
    finish(&mut rt);
    assert_eq!(
        fs::read(destination.join("readme.txt")).unwrap(),
        b"CarryFS export test\n"
    );
    assert!(destination.join("Documents/Empty").is_dir());
    assert_eq!(
        fs::read_dir(destination.join("Documents")).unwrap().count(),
        71
    );
    assert!(
        rt.state
            .history
            .iter()
            .any(|r| r.task == "Recover files" && !r.failed && r.target.contains("test.cfs"))
    );
    click(&mut rt, "tab-activity");
    assert!(rt.state.history.len() >= 2);
    assert_eq!(before, hash(&fixture.image()));
}

#[test]
fn reopening_the_image_is_required_after_its_generation_changes() {
    let fixture = Fixture::new();
    let browser = Browser::open(fixture.image()).unwrap();
    let object = browser
        .entries
        .iter()
        .find(|e| e.item.kind == ObjectKind::File)
        .unwrap()
        .item
        .object;
    let mut volume =
        Volume::open(ImageDevice::open(fixture.image(), MountMode::ReadWrite).unwrap()).unwrap();
    volume
        .create_directory(ROOT, Name::parse("New folder").unwrap())
        .unwrap();
    volume.eject().unwrap();
    drop(volume);
    let destination = fixture.0.join("stale.txt");
    assert!(
        browser
            .export(object, &destination)
            .unwrap_err()
            .to_string()
            .contains("volume changed")
    );
    assert!(!destination.exists());
}

#[test]
fn erasing_needs_a_valid_name_and_internal_disks_are_protected() {
    let mut rt = runtime();
    let device = platform::Device {
        id: "disk8s1".into(),
        parent: Some("disk8".into()),
        name: "Disposable fixture".into(),
        bytes: 268435456,
        uuid: Some("ABC-123".into()),
        filesystem: "exfat".into(),
        mount: None,
        whole: false,
        internal: false,
        kind: platform::DeviceKind::Physical,
    };
    rt.update(|s, _| s.device = Some(device));
    rt.frame(0.01);
    click(&mut rt, "erase");
    assert!(
        rt.state
            .sheet
            .as_ref()
            .is_some_and(|s| s.kind == SheetKind::Erase)
    );
    assert_eq!(rt.state.erase_name.text, "Disposable fixture");
    // Never confirm a valid name here: that would erase a real disk8s1.
    rt.update(|s, _| s.erase_name.text = "".into());
    rt.frame(0.01);
    click(&mut rt, "confirm-erase");
    assert!(
        rt.state.pending.is_none(),
        "An empty name cannot start erasing"
    );
    click(&mut rt, "cancel-sheet");
    assert!(rt.state.sheet.is_none());
    // A mounted foreign partition can be unmounted, but not erased in place.
    rt.update(|s, _| s.device.as_mut().unwrap().mount = Some("/Volumes/Fixture".into()));
    rt.frame(0.01);
    assert!(rt.state.device.as_ref().unwrap().erasable());
    click(&mut rt, "erase");
    assert!(rt.state.sheet.is_none());
    rt.update(|s, _| {
        let d = s.device.as_mut().unwrap();
        (d.id, d.parent, d.uuid, d.whole, d.mount) = ("disk8".into(), None, None, true, None);
    });
    rt.frame(0.01);
    click(&mut rt, "erase-disk-callout");
    assert!(
        rt.state
            .sheet
            .as_ref()
            .is_some_and(|s| s.kind == SheetKind::Erase),
        "A whole external disk can be erased"
    );
    click(&mut rt, "cancel-sheet");
    rt.update(|s, _| s.device.as_mut().unwrap().internal = true);
    rt.frame(0.01);
    click(&mut rt, "erase");
    assert!(rt.state.sheet.is_none());
    click(&mut rt, "eject");
    assert!(rt.state.sheet.is_none());
}

#[test]
fn system_storage_is_hidden_until_explicitly_requested() {
    let mut rt = runtime();
    let internal = platform::Device {
        id: "disk0s1".into(),
        parent: Some("disk0".into()),
        name: "System Preboot".into(),
        bytes: 524288000,
        uuid: Some("SYSTEM".into()),
        filesystem: "apfs".into(),
        mount: Some("/System/Volumes/Preboot".into()),
        whole: false,
        internal: true,
        kind: platform::DeviceKind::Virtual,
    };
    let external = platform::Device {
        id: "disk8s1".into(),
        parent: Some("disk8".into()),
        name: "Backup SSD".into(),
        bytes: 268435456,
        uuid: Some("BACKUP".into()),
        filesystem: "carryfs".into(),
        mount: None,
        whole: false,
        internal: false,
        kind: platform::DeviceKind::Physical,
    };
    let installer = platform::Device {
        id: "disk9s1".into(),
        parent: Some("disk9".into()),
        name: "Application installer".into(),
        bytes: 536870912,
        uuid: Some("INSTALLER".into()),
        filesystem: "hfs".into(),
        mount: Some("/Volumes/Installer".into()),
        whole: false,
        internal: false,
        kind: platform::DeviceKind::Virtual,
    };
    rt.update(|s, _| s.devices = vec![internal, external, installer]);
    rt.frame(0.01);
    assert!(rt.find("device-0").is_none());
    assert!(rt.find("device-1").is_some());
    assert!(
        rt.find("device-2").is_none(),
        "Unrelated installer images stay hidden"
    );
    click(&mut rt, "show-system");
    assert!(rt.find("device-0").is_some());
    click(&mut rt, "show-system");
    assert!(rt.find("device-0").is_none());
}

#[test]
fn integrity_results_survive_unrelated_status_changes_and_are_bound_to_the_generation() {
    let fixture = Fixture::new();
    let mut rt = runtime();
    rt.update(|s, _| {
        s.apply(Ok(Update::Browser(Box::new(
            Browser::open(fixture.image()).unwrap(),
        ))))
    });
    rt.frame(0.01);
    first_aid(&mut rt);
    let browser = rt.state.browser.as_ref().unwrap();
    let key = (browser.info.uuid, browser.info.generation);
    assert!(rt.state.integrity[&key].plan.as_ref().unwrap().passed);
    assert!(rt.state.integrity[&key].scrub.as_ref().unwrap().passed);
    rt.update(|s, _| s.apply(Ok(Update::Report("Another operation finished".into()))));
    rt.frame(0.01);
    assert!(rt.state.integrity[&key].plan.as_ref().unwrap().passed);
    assert!(!rt.state.integrity.contains_key(&(key.0, key.1 + 1)));
}

#[test]
fn opening_a_second_image_records_the_requested_source() {
    let first = Fixture::new();
    let second = Fixture::new();
    let mut rt = runtime();
    rt.update(|s, tx| s.open(first.image(), tx));
    finish(&mut rt);
    rt.update(|s, tx| s.open(second.image(), tx));
    finish(&mut rt);
    assert_eq!(
        rt.state.history.last().unwrap().target,
        second.image().display().to_string()
    );
    assert_eq!(rt.state.browser.as_ref().unwrap().path, second.image());
}

fn system_disk(index: usize) -> Device {
    Device {
        id: format!("disk{index}"),
        parent: None,
        name: format!("System disk {index}"),
        bytes: 268435456,
        uuid: Some(format!("UUID-{index}")),
        filesystem: "apfs".into(),
        mount: None,
        whole: true,
        internal: true,
        kind: DeviceKind::Physical,
    }
}

#[test]
fn device_hierarchy_expands_in_order_and_collapses_all_descendants() {
    let mut rt = runtime();
    rt.update(|s, _| {
        s.devices = (0..5).map(system_disk).collect();
        s.devices[1].parent = Some("disk0".into());
        s.devices[2].parent = Some("disk1".into());
        s.devices[3].parent = Some("disk2".into());
    });
    rt.frame(0.01);
    click(&mut rt, "show-system");
    assert_eq!(device_rows(&rt.state), vec![(0, 0), (4, 0)]);
    // The rows were already shown; let them slide aside for the expand buttons.
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    click(&mut rt, "expand-disk0");
    click(&mut rt, "expand-disk1");
    click(&mut rt, "expand-disk2");
    assert_eq!(
        device_rows(&rt.state),
        vec![(0, 0), (1, 1), (2, 2), (3, 3), (4, 0)]
    );
    click(&mut rt, "expand-disk0");
    assert_eq!(device_rows(&rt.state), vec![(0, 0), (4, 0)]);
}

#[test]
fn device_viewport_accepts_wheel_events_across_its_width() {
    let mut rt = runtime();
    rt.update(|s, _| {
        s.devices = (0..40).map(system_disk).collect();
        s.show_system = true;
    });
    rt.frame(0.01);
    let ([x, y, width, height], _) = rt.presented("device-list").unwrap();
    assert!(
        width > 150.0 && height > 100.0 && height < 500.0,
        "Unusable device viewport: {width} x {height}"
    );
    let before = rt.presented("device-39").unwrap().0[1];
    rt.wheel(x + width / 2.0, y + height / 2.0, -500.0);
    for _ in 0..60 {
        rt.frame(1.0 / 60.0);
    }
    let after = rt.presented("device-39").unwrap().0[1];
    assert!(
        after < before - 100.0,
        "Wheel did not scroll device rows: {before} -> {after}"
    );
}

#[test]
fn accessible_navigation_exposes_the_selected_tab_and_source() {
    fn selected(rt: &Runtime<State>, key: &str) -> Option<bool> {
        let tree = rt.accessibility_tree();
        tree.nodes
            .iter()
            .find(|(_, n)| n.author_id() == Some(key))
            .unwrap_or_else(|| panic!("Missing accessible element {key}"))
            .1
            .is_selected()
    }
    let mut rt = runtime();
    rt.update(|s, _| s.device = Some(system_disk(0)));
    rt.frame(0.01);
    assert_eq!(selected(&rt, "tab-overview"), Some(true));
    assert_eq!(selected(&rt, "tab-files"), Some(false));
    click(&mut rt, "tab-files");
    assert_eq!(selected(&rt, "tab-files"), Some(true));
    assert_eq!(selected(&rt, "tab-overview"), Some(false));
    click(&mut rt, "software");
    let tree = rt.accessibility_tree();
    let software = &tree
        .nodes
        .iter()
        .find(|(_, n)| n.author_id() == Some("software"))
        .unwrap()
        .1;
    assert_eq!(software.description(), Some("Current view"));
    rt.update(|s, _| {
        s.devices = vec![system_disk(0), system_disk(1)];
        s.show_system = true;
        s.device = Some(s.devices[0].clone());
    });
    rt.frame(0.01);
    assert_eq!(selected(&rt, "device-0"), Some(true));
    assert_eq!(selected(&rt, "device-1"), Some(false));
    rt.update(|s, _| s.device = Some(s.devices[1].clone()));
    rt.frame(0.01);
    assert_eq!(selected(&rt, "device-1"), Some(true));
    assert_eq!(selected(&rt, "device-0"), Some(false));
}

#[test]
fn long_directories_scroll_to_files_below_the_viewport() {
    let fixture = Fixture::new();
    let mut browser = Browser::open(fixture.image()).unwrap();
    let folder = browser
        .entries
        .iter()
        .find(|e| e.item.kind == ObjectKind::Directory)
        .unwrap()
        .item
        .object;
    browser = browser
        .navigate(vec![(ROOT, "/".into()), (folder, "Documents".into())])
        .unwrap();
    let last = format!(
        "entry-{}",
        browser.entries.last().unwrap().item.object.get()
    );
    let mut rt = runtime();
    rt.update(|s, _| {
        s.apply(Ok(Update::Browser(Box::new(browser))));
        s.tab = Tab::Files;
    });
    rt.frame(0.01);
    let ([x, y, width, height], _) = rt.presented("file-list").unwrap();
    let before = rt.presented(&last).unwrap().0[1];
    rt.wheel(x + width / 2.0, y + height / 2.0, -10000.0);
    for _ in 0..60 {
        rt.frame(1.0 / 60.0);
    }
    let after = rt.presented(&last).unwrap().0[1];
    assert!(
        after < before - 100.0 && after < y + height,
        "Last file is unreachable: {before} -> {after}"
    );
}

#[test]
fn ui_upgrades_an_image_in_place_and_shows_stored_permissions() {
    let fixture = Fixture::new();
    let mut rt = runtime();
    let browser = Browser::open(fixture.image()).unwrap();
    rt.update(|s, _| s.apply(Ok(Update::Browser(Box::new(browser)))));
    rt.frame(0.01);
    settle_checks(&mut rt);
    assert!(rt.find("upgrade").is_none(), "New volumes need no upgrade");
    // mkfs only makes revision 2; the core tests upgrade a revision 1 root.
    rt.update(|s, _| s.browser.as_mut().unwrap().legacy = true);
    rt.frame(0.01);
    let before = rt.state.browser.as_ref().unwrap().info.clone();
    // The format section sits below the fold of the Overview.
    reveal(&mut rt);
    click(&mut rt, "upgrade");
    click(&mut rt, "cancel-sheet");
    assert!(rt.state.sheet.is_none());
    click(&mut rt, "upgrade");
    click(&mut rt, "confirm-upgrade");
    finish(&mut rt);
    click(&mut rt, "close-sheet");
    let browser = rt.state.browser.as_ref().unwrap();
    assert!(!browser.legacy, "{}", rt.state.status);
    assert_eq!(browser.info.uuid, before.uuid);
    assert!(rt.state.status.contains("already up to date"), "{}", rt.state.status);
    assert!(
        rt.find("upgrade").is_none(),
        "An upgraded volume offers no upgrade"
    );
    assert_eq!(rt.state.history.last().unwrap().task, "Upgrade format");

    let mut volume =
        Volume::open(ImageDevice::open(fixture.image(), MountMode::ReadWrite).unwrap()).unwrap();
    let file = volume
        .lookup(ROOT, &Name::parse("readme.txt").unwrap())
        .unwrap();
    let handle = volume.retain_object(file).unwrap();
    volume
        .set_attributes(
            &handle,
            carryfs_core::AttributeChange {
                mode: Some(carryfs_core::Mode::from_host(0o600)),
                hidden: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    volume.close_handle(handle).unwrap();
    volume.eject().unwrap();
    let browser = Browser::open(fixture.image()).unwrap();
    let entry = browser
        .entries
        .iter()
        .find(|e| e.item.object == file)
        .unwrap();
    assert_eq!(model::permissions(entry.attrs.mode), "rw-------");
    assert!(entry.attrs.hidden);
    assert!(folder_summary(&browser).ends_with("Hidden 1"));
}

#[test]
fn permissions_read_like_ls() {
    use carryfs_core::Mode;
    assert_eq!(model::permissions(Mode::FILE), "rw-r--r--");
    assert_eq!(model::permissions(Mode::DIRECTORY), "rwxr-xr-x");
    assert_eq!(model::permissions(Mode::from_host(0o751)), "rwxr-x--x");
    assert_eq!(model::permissions(Mode::from_host(0)), "---------");
}

/// Scroll the main view to its end, where recovery actions sit.
fn reveal(rt: &mut Runtime<State>) {
    rt.wheel(700.0, 400.0, -10000.0);
    for _ in 0..60 {
        rt.frame(1.0 / 60.0);
    }
}

/// Flip a byte in the newest root's allocation tree page.
fn damage_allocation(image: &Path) {
    use std::io::{Seek, SeekFrom, Write};
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .unwrap();
    let blocks = file.metadata().unwrap().len() / 4096;
    let mut newest = (0, 0);
    for slot in [256, blocks - 257] {
        let mut root = [0; 4096];
        file.seek(SeekFrom::Start(slot * 4096)).unwrap();
        file.read_exact(&mut root).unwrap();
        let field = |at: usize| u64::from_le_bytes(root[at..at + 8].try_into().unwrap());
        if &root[..8] == b"CFSXROOT" && field(48) > newest.0 {
            newest = (field(48), field(80));
        }
    }
    let at = newest.1 * 4096;
    let mut byte = [0];
    file.seek(SeekFrom::Start(at)).unwrap();
    file.read_exact(&mut byte).unwrap();
    file.seek(SeekFrom::Start(at)).unwrap();
    file.write_all(&[byte[0] ^ 1]).unwrap();
}

#[test]
fn damaged_images_can_be_diagnosed_and_repaired_in_place() {
    let fixture = Fixture::new();
    damage_allocation(&fixture.image());
    let mut rt = runtime();
    click(&mut rt, "open-image");
    rt.resolve_file_dialog(Some(fixture.image())).unwrap();
    rt.frame(0.01);
    while rt.state.pending.is_some() {
        rt.frame(1.0 / 60.0);
    }
    assert!(rt.state.browser.is_none());
    assert!(rt.state.damaged.is_some(), "{}", rt.state.status);
    assert!(rt.state.status.contains("Run First Aid"));
    assert!(rt.find("repair").is_none(), "Repair waits for a diagnosis");

    click(&mut rt, "diagnose");
    click(&mut rt, "confirm-first-aid");
    while rt.state.pending.is_some() {
        rt.frame(1.0 / 60.0);
    }
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    let plan = rt.state.damaged.as_ref().unwrap().plan.as_ref().unwrap();
    assert!(!plan.passed && plan.repairable(), "{}", plan.summary);
    assert!(plan.summary.contains("Inaccessible Allocation page"));

    click(&mut rt, "repair");
    click(&mut rt, "confirm-repair");
    finish(&mut rt);
    assert!(rt.state.damaged.is_none());
    assert!(rt.state.browser.is_some());
    assert!(
        rt.state.status.starts_with("Repair committed"),
        "{}",
        rt.state.status
    );
    let reports = fs::read_dir(&fixture.0)
        .unwrap()
        .filter(|e| {
            let name = e.as_ref().unwrap().file_name();
            name.to_string_lossy().starts_with("test.cfs.repair-")
        })
        .count();
    assert_eq!(reports, 1, "The repair report sits beside the image");
    click(&mut rt, "close-sheet");
    first_aid(&mut rt);
    assert_eq!(rt.state.status, "First Aid found no problems.");
}

#[test]
fn a_clean_volume_with_hard_links_rebuilds_into_a_new_verified_image() {
    let fixture = Fixture::new();
    let mut volume =
        Volume::open(ImageDevice::open(fixture.image(), MountMode::ReadWrite).unwrap()).unwrap();
    let readme = volume
        .lookup(ROOT, &Name::parse("readme.txt").unwrap())
        .unwrap();
    let documents = volume
        .lookup(ROOT, &Name::parse("Documents").unwrap())
        .unwrap();
    volume
        .link(readme, documents, Name::parse("readme-link.txt").unwrap())
        .unwrap();
    volume.eject().unwrap();
    let before = hash(&fixture.image());

    let mut rt = runtime();
    let browser = Browser::open(fixture.image()).unwrap();
    let entry = browser
        .entries
        .iter()
        .find(|e| e.item.object == readme)
        .unwrap();
    assert_eq!(entry.attrs.links, 2);
    rt.update(|s, _| s.apply(Ok(Update::Browser(Box::new(browser)))));
    rt.frame(0.01);

    first_aid(&mut rt);
    click(&mut rt, "close-sheet");
    let key = {
        let b = rt.state.browser.as_ref().unwrap();
        (b.info.uuid, b.info.generation)
    };
    assert!(rt.state.integrity[&key].plan.as_ref().unwrap().passed);

    click(&mut rt, "recover-tool");
    click(&mut rt, "rebuild");
    assert_eq!(rt.file_dialog().unwrap().0, FileDialogKind::Save);
    let output = fixture.0.join("rebuilt.cfs");
    rt.resolve_file_dialog(Some(output.clone())).unwrap();
    rt.frame(0.01);
    finish(&mut rt);
    assert!(
        rt.state.status.starts_with("Recovered image"),
        "{}",
        rt.state.status
    );
    assert!(fixture.0.join("rebuilt.cfs.recovery.json").is_file());
    let rebuilt = Browser::open(output).unwrap();
    assert_ne!(rebuilt.info.uuid, key.0);
    assert!(
        rebuilt
            .entries
            .iter()
            .any(|e| e.item.name.stored() == b"readme.txt" && e.attrs.links == 2)
    );
    assert_eq!(before, hash(&fixture.image()), "The source stays read-only");
}

#[test]
fn default_sidebar_shows_physical_disks_with_their_user_partitions() {
    let device = |id: &str, parent: Option<&str>, fs: &str, internal, kind| Device {
        id: id.into(),
        parent: parent.map(Into::into),
        name: id.into(),
        bytes: 1 << 30,
        uuid: Some(id.into()),
        filesystem: fs.into(),
        mount: None,
        whole: parent.is_none() || id == "disk3",
        internal,
        kind,
    };
    let mut rt = runtime();
    rt.update(|s, _| {
        s.devices = vec![
            device(
                "disk0",
                None,
                "GUID_partition_scheme",
                true,
                DeviceKind::Unknown,
            ),
            device(
                "disk0s2",
                Some("disk0"),
                "Apple_APFS",
                true,
                DeviceKind::Unknown,
            ),
            device("disk3", Some("disk0s2"), "apfs", true, DeviceKind::Virtual),
            device("disk3s1", Some("disk3"), "apfs", true, DeviceKind::Virtual),
            device(
                "disk4",
                None,
                "GUID_partition_scheme",
                false,
                DeviceKind::Physical,
            ),
            device(
                "disk4s1",
                Some("disk4"),
                "exfat",
                false,
                DeviceKind::Unknown,
            ),
        ];
    });
    assert_eq!(device_rows(&rt.state), vec![(0, 0), (4, 0), (5, 1)]);
}

#[test]
fn refresh_after_erase_shows_the_new_mounted_volume() {
    let before = Device {
        id: "disk8s1".into(),
        parent: Some("disk8".into()),
        name: "LEXAR".into(),
        bytes: 1 << 30,
        uuid: Some("PARTITION".into()),
        filesystem: "exfat".into(),
        mount: None,
        whole: false,
        internal: false,
        kind: DeviceKind::Physical,
    };
    let after = Device {
        name: "CARROCHKA".into(),
        filesystem: "carryfs".into(),
        mount: Some("/Volumes/CARROCHKA".into()),
        ..before.clone()
    };
    let mut rt = runtime();
    rt.update(|s, _| {
        s.device = Some(before.clone());
        s.apply(Ok(Update::Devices(vec![after.clone()])));
    });
    assert_eq!(rt.state.device, Some(after));
}

#[test]
fn first_aid_on_an_unopened_volume_keeps_its_results() {
    let fixture = Fixture::new();
    let browser = Browser::open(fixture.image()).unwrap();
    let key = (browser.info.uuid, browser.info.generation);
    let mut rt = runtime();
    rt.update(|s, _| {
        s.apply(Ok(Update::FirstAid {
            opened: Some(Box::new(Update::Browser(Box::new(browser)))),
            key: Some(key),
            plan: ScanResult::new(Ok("No problems found.".into())),
            scrub: Some(ScanResult::new(Ok("File data verified.".into()))),
        }));
    });
    assert!(rt.state.browser.is_some());
    let health = &rt.state.integrity[&key];
    assert!(health.plan.as_ref().is_some_and(|p| p.passed));
    assert!(health.scrub.as_ref().is_some_and(|p| p.passed));
    assert_eq!(rt.state.status, "First Aid found no problems.");
}

#[test]
fn checks_run_by_themselves_structure_first_then_file_data_once_quiet() {
    let fixture = Fixture::new();
    let mut rt = runtime();
    let browser = Browser::open(fixture.image()).unwrap();
    let key = (browser.info.uuid, browser.info.generation);
    rt.update(|s, _| s.apply(Ok(Update::Browser(Box::new(browser)))));
    rt.frame(0.01);
    assert_eq!(
        rt.state.background.as_ref().map(|b| b.check),
        Some(Check::Structure)
    );
    settle_checks(&mut rt);
    assert!(rt.state.integrity[&key].plan.as_ref().unwrap().passed);
    assert!(
        rt.state.background.is_none(),
        "File data waits for a quiet period"
    );
    rt.update(|s, _| s.quiet_since -= QUIET);
    rt.frame(0.01);
    assert_eq!(
        rt.state.background.as_ref().map(|b| b.check),
        Some(Check::Data)
    );
    settle_checks(&mut rt);
    let scrub = rt.state.integrity[&key].scrub.as_ref().unwrap();
    assert!(scrub.passed, "{}", scrub.summary);
    assert_eq!(rt.state.history.last().unwrap().task, "Check file data");
    rt.frame(0.01);
    assert!(
        rt.state.background.is_none(),
        "Checked volumes aren't checked again"
    );
}

#[test]
fn a_task_on_the_volume_stops_a_background_check() {
    let fixture = Fixture::new();
    let mut rt = runtime();
    let browser = Browser::open(fixture.image()).unwrap();
    let key = (browser.info.uuid, browser.info.generation);
    rt.update(|s, _| s.apply(Ok(Update::Browser(Box::new(browser)))));
    rt.frame(0.01);
    settle_checks(&mut rt);
    rt.update(|s, tx| {
        s.check(Check::Data, tx);
        s.start("Discover disks", false, tx, |_| {
            Ok(Update::Report("Done".into()))
        });
    });
    assert!(
        rt.state.background.is_some(),
        "Discovery leaves the volume alone"
    );
    finish(&mut rt);
    settle_checks(&mut rt);
    rt.update(|s, tx| {
        s.integrity.get_mut(&key).unwrap().scrub = None;
        s.check(Check::Data, tx);
        s.start("Read folder", false, tx, |_| {
            Ok(Update::Report("Done".into()))
        });
    });
    assert!(rt.state.background.is_none());
    finish(&mut rt);
    assert!(
        rt.state.integrity[&key].scrub.is_none(),
        "A stopped check reports nothing, and doesn't restart before the quiet period"
    );
}

#[test]
fn file_data_progress_estimates_the_time_left() {
    let gib = 1 << 30;
    assert_eq!(view::data_progress(0, gib, Duration::from_secs(5)).0, None);
    let (value, line) = view::data_progress(gib / 4, gib, Duration::from_secs(60));
    assert_eq!(value, Some(0.25));
    assert_eq!(line, "256.0 MiB of 1.0 GiB · about 3 min left");
    let (value, line) = view::data_progress(2 * gib, gib, Duration::from_secs(60));
    assert_eq!(value, Some(1.0));
    assert!(line.ends_with("under a minute left"), "{line}");
}
