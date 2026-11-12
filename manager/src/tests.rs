use super::*;
use carryfs_core::{ImageDevice, MountMode, Name, ROOT, Volume};
use rune::{Env, FileDialogKind, Runtime};
use std::{
    fs,
    hash::{Hash, Hasher},
    io::Read,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

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
    click(&mut rt, "check");
    finish(&mut rt);
    assert!(rt.state.status.starts_with("Structural check passed"));
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
    click(&mut rt, "scrub");
    finish(&mut rt);
    assert!(
        rt.state.status.contains("0 unreadable ranges"),
        "{}",
        rt.state.status
    );
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
fn formatting_requires_an_exact_identifier_and_internal_devices_are_protected() {
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
    click(&mut rt, "format");
    assert!(rt.state.format.is_some());
    click(&mut rt, "confirm-format");
    assert!(
        rt.state.pending.is_none(),
        "An empty confirmation cannot start formatting"
    );
    rt.update(|s, _| s.confirmation.text = "disk8s2".into());
    rt.frame(0.01);
    click(&mut rt, "confirm-format");
    assert!(
        rt.state.pending.is_none(),
        "A different partition cannot authorize formatting"
    );
    rt.update(|s, _| {
        s.format = None;
        s.device.as_mut().unwrap().internal = true;
    });
    rt.frame(0.01);
    click(&mut rt, "format");
    assert!(rt.state.format.is_none());
    click(&mut rt, "eject");
    assert!(rt.state.eject.is_none());
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
    click(&mut rt, "check");
    finish(&mut rt);
    let browser = rt.state.browser.as_ref().unwrap();
    let key = (browser.info.uuid, browser.info.generation);
    assert!(rt.state.integrity[&key].structure.as_ref().unwrap().passed);
    rt.update(|s, _| s.apply(Ok(Update::Report("Another operation finished".into()))));
    rt.frame(0.01);
    assert!(rt.state.integrity[&key].structure.as_ref().unwrap().passed);
    assert!(rt.state.integrity[&key].scrub.is_none());
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
