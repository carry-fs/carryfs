//! Native CarryFS manager, following Rune's state/view/AppSpec pattern.
mod model;
mod operation;
mod platform;

use carryfs_core::ObjectKind;
use model::{Browser, Source, bytes, kind, permissions};
use operation::Fsck;
use platform::{Device, DeviceAction, DeviceKind, Versions};
use rune::tok::{self, TextStyle, type_};
use rune::{
    AppSpec, Cx, El, FileDialogOptions, LiveRegion, ScrollSpec, TextEdit, Tx, col, el, row, text,
};
use rune_kit::{
    Button, ButtonVariant, IconButton, IconButtonVariant, Kit, PaletteSeed, ScrollBar, TextInput,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Files,
    Activity,
    Software,
}
enum Update {
    Browser(Box<Browser>),
    /// A browser reopened after a change, with the change's report.
    Reopened(Box<Browser>, String),
    Damaged(Box<Damaged>),
    Device(Box<Device>),
    Devices(Vec<Device>),
    Report(String),
    Versions(Versions),
}
struct Record {
    task: String,
    target: String,
    message: String,
    failed: bool,
    at: SystemTime,
}
type VolumeKey = ([u8; 16], u64);
struct ScanResult {
    summary: String,
    passed: bool,
    at: SystemTime,
}
#[derive(Default)]
struct Integrity {
    structure: Option<ScanResult>,
    scrub: Option<ScanResult>,
    plan: Option<ScanResult>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scan {
    Structure,
    Data,
    Plan,
}
impl ScanResult {
    /// A diagnosed plan the tool would apply in place: damaged, but neither
    /// blocked nor losing data. The tool itself refuses any other plan.
    fn repairable(&self) -> bool {
        !self.passed
            && self.summary.starts_with("Repair plan:")
            && self.summary.contains("possible data loss: false")
            && !self.summary.contains("In-place repair blocked")
    }
}

/// A source whose metadata is too damaged to open.
struct Damaged {
    source: Source,
    error: String,
    plan: Option<ScanResult>,
}

struct State {
    browser: Option<Browser>,
    damaged: Option<Damaged>,
    device: Option<Device>,
    devices: Vec<Device>,
    show_system: bool,
    expanded_devices: BTreeSet<String>,
    show_details: bool,
    startup_image: Option<PathBuf>,
    elapsed_seconds: u64,
    integrity: BTreeMap<VolumeKey, Integrity>,
    /// The running scan and its volume; `None` is the damaged source.
    inspection: Option<(Option<VolumeKey>, Scan)>,
    images: Vec<PathBuf>,
    tab: Tab,
    selected: Option<usize>,
    pending: Option<Receiver<Result<Update, String>>>,
    cancel: Option<Arc<operation::Control>>,
    started: Instant,
    task: String,
    target: String,
    initialized: bool,
    history: Vec<Record>,
    versions: Versions,
    status: String,
    error: bool,
    format: Option<Device>,
    confirmation: TextEdit,
    eject: Option<Device>,
    upgrade: bool,
    repair: bool,
}
impl State {
    fn empty() -> Self {
        Self {
            browser: None,
            damaged: None,
            device: None,
            devices: Vec::new(),
            show_system: false,
            expanded_devices: BTreeSet::new(),
            show_details: false,
            startup_image: None,
            elapsed_seconds: 0,
            integrity: BTreeMap::new(),
            inspection: None,
            images: Vec::new(),
            tab: Tab::Overview,
            selected: None,
            pending: None,
            cancel: None,
            started: Instant::now(),
            task: String::new(),
            target: String::new(),
            initialized: true,
            history: Vec::new(),
            versions: platform::versions(),
            status: "Select a device or open an image.".into(),
            error: false,
            format: None,
            confirmation: TextEdit::new(""),
            eject: None,
            upgrade: false,
            repair: false,
        }
    }
    fn target_name(&self) -> String {
        self.device
            .as_ref()
            .map(|d| format!("{} · {}", d.name, d.id))
            .or_else(|| self.browser.as_ref().map(|b| b.path.display().to_string()))
            .unwrap_or_else(|| "CarryFS Manager".into())
    }
    fn apply(&mut self, result: Result<Update, String>) {
        self.error = result.is_err();
        if let Some((key, scan)) = self.inspection.take() {
            let message = match &result {
                Ok(Update::Report(message)) => message.as_str(),
                Err(message) => message.as_str(),
                _ => "No result reported",
            };
            let result_scan = ScanResult {
                summary: message.chars().take(8 * 1024).collect(),
                passed: !self.error,
                at: SystemTime::now(),
            };
            match key {
                Some(key) => {
                    let state = self.integrity.entry(key).or_default();
                    *match scan {
                        Scan::Structure => &mut state.structure,
                        Scan::Data => &mut state.scrub,
                        Scan::Plan => &mut state.plan,
                    } = Some(result_scan);
                }
                None => {
                    if let Some(damaged) = &mut self.damaged {
                        damaged.plan = Some(result_scan);
                    }
                }
            }
        }

        match result {
            Ok(Update::Browser(browser)) => self.apply_browser(*browser),
            Ok(Update::Reopened(browser, report)) => {
                self.apply_browser(*browser);
                self.status = report;
            }
            Ok(Update::Damaged(damaged)) => {
                self.status = format!(
                    "Cannot open {}: {}. Diagnose damage or recover files from Overview.",
                    damaged.source.name(),
                    damaged.error
                );
                self.error = true;
                self.device = damaged.source.device.clone();
                if damaged.source.device.is_none() && !self.images.contains(&damaged.source.path) {
                    self.images.push(damaged.source.path.clone());
                }
                self.browser = None;
                self.selected = None;
                self.damaged = Some(*damaged);
            }
            Ok(Update::Device(device)) => {
                self.status = format!("Selected {}", device.id);
                self.device = Some(*device);
                self.damaged = None;
                self.browser = None;
                self.selected = None;
            }
            Ok(Update::Devices(devices)) => {
                if let Some(selected) = &self.device {
                    if !devices
                        .iter()
                        .any(|d| d.id == selected.id && d.uuid == selected.uuid)
                    {
                        self.device = None;
                        self.browser = None;
                        self.damaged = None;
                        self.selected = None;
                        self.status="The selected device disconnected. Its operations were not redirected to another device.".into();
                    } else {
                        self.status = "Storage list refreshed".into();
                    }
                } else {
                    self.status = "Storage list refreshed".into();
                }
                self.devices = devices;
            }
            Ok(Update::Report(message)) => self.status = message,
            Ok(Update::Versions(versions)) => {
                self.versions = versions;
                self.status =
                    "Software versions refreshed. Volume formats were not changed.".into();
            }
            Err(message) => self.status = message,
        }
        if !self.task.is_empty() {
            self.history.push(Record {
                task: self.task.clone(),
                target: self.target.clone(),
                message: self.status.clone(),
                failed: self.error,
                at: SystemTime::now(),
            });
            if self.history.len() > 100 {
                self.history.remove(0);
            }
            self.task.clear();
        }
    }
    fn apply_browser(&mut self, browser: Browser) {
        self.status = format!("{} items · Read-only access", browser.entries.len());
        self.device = browser.device.clone();
        if browser.device.is_none() && !self.images.contains(&browser.path) {
            self.images.push(browser.path.clone());
        }
        self.browser = Some(browser);
        self.damaged = None;
        self.selected = None;
    }
    fn start(
        &mut self,
        name: &str,
        cancellable: bool,
        tx: &mut Tx,
        job: impl FnOnce(Arc<operation::Control>) -> platform::Result<Update> + Send + 'static,
    ) {
        if self.pending.is_some() {
            return;
        }
        let cancel = Arc::new(operation::Control::default());
        self.cancel = cancellable.then(|| cancel.clone());
        self.task = name.into();
        self.target = if name == "Discover devices" {
            "System storage".into()
        } else if self.tab == Tab::Software {
            "CarryFS software".into()
        } else {
            self.target_name()
        };
        self.started = Instant::now();
        self.elapsed_seconds = 0;
        self.status = format!("{name}…");
        self.error = false;
        let (sender, receiver) = mpsc::channel();
        self.pending = Some(receiver);
        std::thread::spawn(move || {
            let _ = sender.send(job(cancel).map_err(|e| e.to_string()));
        });
        tx.wake_after(Duration::from_millis(20));
    }
    fn open(&mut self, path: PathBuf, tx: &mut Tx) {
        if self.pending.is_some() {
            return;
        }
        let requested_target = path.display().to_string();
        self.tab = Tab::Overview;
        self.show_details = false;
        self.format = None;
        self.eject = None;
        self.upgrade = false;
        self.repair = false;
        self.start("Open image", false, tx, move |_| {
            opened(Browser::open(path.clone()), Source { path, device: None })
        });
        self.target = requested_target;
    }
    fn select_device(&mut self, device: Device, tx: &mut Tx) {
        if self.pending.is_some() {
            return;
        }
        let requested_target = format!("{} · {}", device.name, device.id);
        self.tab = Tab::Overview;
        self.show_details = false;
        self.format = None;
        self.eject = None;
        self.upgrade = false;
        self.repair = false;
        self.start("Read device information", false, tx, move |_| {
            platform::details(&device).map(|d| Update::Device(Box::new(d)))
        });
        self.target = requested_target;
    }
    fn discover(&mut self, tx: &mut Tx) {
        self.start("Discover devices", false, tx, |_| {
            platform::discover().map(Update::Devices)
        });
    }
    fn navigate(&mut self, trail: Vec<(carryfs_core::ObjectId, String)>, tx: &mut Tx) {
        if let Some(browser) = self.browser.clone() {
            self.start("Read folder", false, tx, move |_| {
                browser
                    .navigate(trail)
                    .map(|b| Update::Browser(Box::new(b)))
            });
        }
    }
    fn enter(&mut self, index: usize, tx: &mut Tx) {
        if let Some(browser) = &self.browser
            && let Some(entry) = browser.entries.get(index)
            && entry.item.kind == ObjectKind::Directory
        {
            let mut trail = browser.trail.clone();
            trail.push((
                entry.item.object,
                String::from_utf8_lossy(entry.item.name.stored()).into_owned(),
            ));
            self.navigate(trail, tx);
        }
    }
    /// The selected volume's location, whether it opened or is damaged.
    fn source(&self) -> Option<Source> {
        self.browser
            .as_ref()
            .map(Browser::source)
            .or_else(|| self.damaged.as_ref().map(|d| d.source.clone()))
    }
    fn inspect(&mut self, scan: Scan, tx: &mut Tx) {
        if self.pending.is_some() {
            return;
        }
        let key = self
            .browser
            .as_ref()
            .map(|b| (b.info.uuid, b.info.generation));
        if scan == Scan::Plan {
            let Some(source) = self.source() else {
                return;
            };
            self.inspection = Some((key, scan));
            self.start("Diagnose damage", true, tx, move |cancel| {
                operation::fsck(source, Fsck::Plan, cancel).map(Update::Report)
            });
        } else if let Some(browser) = self.browser.clone() {
            self.inspection = Some((key, scan));
            let scrub = scan == Scan::Data;
            self.start(
                if scrub {
                    "Scrub file data"
                } else {
                    "Check structure"
                },
                true,
                tx,
                move |cancel| operation::inspect(browser, scrub, cancel).map(Update::Report),
            );
        }
    }
    fn repair(&mut self, tx: &mut Tx) {
        let Some(source) = self.source() else {
            return;
        };
        self.repair = false;
        self.start("Repair volume", true, tx, move |cancel| {
            let path = source.path.clone();
            let report = operation::fsck(source, Fsck::Repair, cancel)?;
            Ok(Update::Reopened(Box::new(Browser::open(path)?), report))
        });
    }
    fn upgrade(&mut self, tx: &mut Tx) {
        let Some(browser) = self.browser.clone() else {
            return;
        };
        self.upgrade = false;
        match browser.device.clone() {
            Some(device) => self.start("Prepare volume upgrade", false, tx, move |_| {
                platform::upgrade_in_terminal(&device).map(Update::Report)
            }),
            None => self.start("Upgrade volume", true, tx, move |cancel| {
                operation::upgrade(browser, cancel)
                    .map(|(b, report)| Update::Reopened(Box::new(b), report))
            }),
        }
    }
    fn device_action(&mut self, action: DeviceAction, tx: &mut Tx) {
        if let Some(device) = self.device.clone() {
            self.browser = None;
            self.eject = None;
            self.start(
                match action {
                    DeviceAction::Mount => "Mount volume",
                    DeviceAction::Unmount => "Unmount volume",
                    DeviceAction::Eject => "Eject device",
                },
                false,
                tx,
                move |_| {
                    platform::act(&device, action)?;
                    if action == DeviceAction::Eject {
                        Ok(Update::Devices(platform::discover()?))
                    } else {
                        Ok(Update::Device(Box::new(platform::details(&device)?)))
                    }
                },
            );
        }
    }
}
impl Drop for State {
    fn drop(&mut self) {
        if let Some(control) = &self.cancel
            && let Err(error) = control.stop()
        {
            eprintln!("CarryFS Manager: stop operation on close: {error}");
        }
    }
}
impl Default for State {
    fn default() -> Self {
        let mut state = Self::empty();
        state.initialized = false;
        state.startup_image = std::env::var_os("CARRYFS_IMAGE").map(PathBuf::from);
        state
    }
}
fn poll(s: &mut State, _: f64, tx: &mut Tx) -> bool {
    if s.pending.is_none() {
        if let Some(path) = s.startup_image.take() {
            s.open(path, tx);
            return true;
        }
        if !s.initialized {
            s.initialized = true;
            s.discover(tx);
            return true;
        }
    }
    let Some(receiver) = &s.pending else {
        return false;
    };
    match receiver.try_recv() {
        Ok(result) => {
            s.pending = None;
            s.cancel = None;
            s.apply(result);
            true
        }
        Err(mpsc::TryRecvError::Empty) => {
            tx.wake_after(Duration::from_millis(100));
            let elapsed = s.started.elapsed().as_secs();
            let changed = elapsed != s.elapsed_seconds;
            s.elapsed_seconds = elapsed;
            changed
        }
        Err(mpsc::TryRecvError::Disconnected) => {
            s.pending = None;
            s.cancel = None;
            s.apply(Err(
                "The worker stopped unexpectedly. Refresh the source and try again.".into(),
            ));
            true
        }
    }
}
/// A browser, or the damaged source when its metadata is corrupt.
fn opened(result: platform::Result<Browser>, source: Source) -> platform::Result<Update> {
    match result {
        Ok(browser) => Ok(Update::Browser(Box::new(browser))),
        Err(error) if model::corrupt(&*error) => Ok(Update::Damaged(Box::new(Damaged {
            source,
            error: error.to_string(),
            plan: None,
        }))),
        Err(error) => Err(error),
    }
}
fn label(value: impl Into<String>, k: &Kit) -> El {
    text(value).font(type_::BODY).color(k.c.text_base)
}
fn muted(value: impl Into<String>, k: &Kit) -> El {
    text(value).font(type_::LABEL).color(k.c.text_weak)
}
fn rule(k: &Kit) -> El {
    el().w_full().h(1.0).fill(k.c.border_base)
}
fn detail(title: &str, value: impl Into<String>, k: &Kit) -> El {
    col()
        .gap(4.0)
        .child(muted(title, k))
        .child(label(value, k).wrap_anywhere())
}
fn heading(title: impl Into<String>, k: &Kit) -> El {
    text(title)
        .font(TextStyle::new(24.0, 600.0))
        .color(k.c.text_base)
        .truncate()
}
fn open_dialog(_: &mut State, tx: &mut Tx) {
    tx.open_file(
        FileDialogOptions {
            title: "Open CarryFS image".into(),
            ..Default::default()
        },
        |s: &mut State, result, tx| match result {
            Ok(Some(path)) => s.open(path, tx),
            Ok(None) => {}
            Err(e) => s.apply(Err(e.to_string())),
        },
    );
}
fn device_rows(s: &State) -> Vec<(usize, usize)> {
    if !s.show_system {
        return s
            .devices
            .iter()
            .enumerate()
            .filter(|(_, d)| d.visible_by_default(&s.devices))
            .map(|(i, _)| (i, 0))
            .collect();
    }
    fn append(s: &State, index: usize, depth: usize, rows: &mut Vec<(usize, usize)>) {
        if rows.iter().any(|(i, _)| *i == index) {
            return;
        }
        rows.push((index, depth));
        let device = &s.devices[index];
        if s.expanded_devices.contains(&device.id) {
            for (child, _) in s
                .devices
                .iter()
                .enumerate()
                .filter(|(_, d)| d.parent.as_deref() == Some(&device.id))
            {
                append(s, child, depth + 1, rows);
            }
        }
    }
    let mut rows = Vec::new();
    for (i, d) in s.devices.iter().enumerate() {
        if !s.devices.iter().any(|p| Some(&p.id) == d.parent.as_ref()) {
            append(s, i, 0, &mut rows);
        }
    }
    rows
}
fn sidebar(s: &State, k: &Kit) -> El {
    let busy = s.pending.is_some();
    let mut items = col().w_full().gap(4.0).role("tree").focus_group();
    let visible = device_rows(s);
    if visible.is_empty() {
        items = items.child(
            muted(
                if busy {
                    "Reading system devices…"
                } else {
                    "Connect an external drive, or open an image."
                },
                k,
            )
            .wrap(),
        );
    }
    for (index, depth) in visible {
        let device = &s.devices[index];
        let chosen = s.device.as_ref().is_some_and(|d| d.id == device.id);
        let device = device.clone();
        let title = device.name.clone();
        let identity = if s.show_system {
            format!(
                "{} · {}",
                device.id,
                match device.kind {
                    DeviceKind::Physical => "Physical disk",
                    DeviceKind::Virtual => "Virtual disk",
                    DeviceKind::Unknown => "Disk kind not reported",
                }
            )
        } else {
            format!(
                "{} · {}{}",
                device.format_name(),
                bytes(device.bytes),
                if device.mount.is_some() {
                    " · Mounted"
                } else {
                    ""
                }
            )
        };
        let weak = k.c.text_weak;
        let has_children = s.show_system
            && s.devices
                .iter()
                .any(|d| d.parent.as_deref() == Some(&device.id));
        let expanded = s.expanded_devices.contains(&device.id);
        let mut device_row = row()
            .w_full()
            .min_w(0.0)
            .items_center()
            .pl(depth as f32 * 12.0);
        if has_children {
            let id = device.id.clone();
            device_row = device_row.child(
                IconButton::new(if expanded {
                    k.glyph.triangle_down
                } else {
                    k.glyph.triangle_right
                })
                .variant(IconButtonVariant::Ghost)
                .label(format!(
                    "{} {}",
                    if expanded { "Collapse" } else { "Expand" },
                    device.id
                ))
                .expanded(expanded)
                .on_press(move |s: &mut State| {
                    if !s.expanded_devices.remove(&id) {
                        s.expanded_devices.insert(id.clone());
                    }
                })
                .build(k)
                .key(format!("expand-{}", device.id)),
            );
        }
        let source = Button::new(format!("{} · {}", device.name, device.id))
            .label_with(move |font, ink| {
                col()
                    .w_full()
                    .min_w(0.0)
                    .gap(2.0)
                    .child(text(title).font(font).color(ink).truncate())
                    .child(text(identity).font(type_::LABEL).color(weak).truncate())
            })
            .variant(if chosen {
                ButtonVariant::Neutral
            } else {
                ButtonVariant::Ghost
            })
            .disabled(busy)
            .on_press_tx(move |s: &mut State, tx| s.select_device(device.clone(), tx))
            .build(k)
            .h(44.0)
            .px(8.0)
            .grow(1.0)
            .min_w(0.0)
            .role("treeitem")
            .focus_selected(chosen)
            .key(format!("device-{index}"));
        let source = if has_children {
            source.expanded(expanded)
        } else {
            source
        };
        items = items.child(device_row.child(source));
    }
    let mut images = col().w_full().gap(4.0).role("listbox").focus_group();
    for (index, path) in s.images.iter().enumerate() {
        let chosen = s
            .browser
            .as_ref()
            .is_some_and(|b| b.device.is_none() && b.path == *path)
            || s.damaged
                .as_ref()
                .is_some_and(|d| d.source.device.is_none() && d.source.path == *path);
        let image = path.clone();
        let title = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned();
        let title = s
            .browser
            .as_ref()
            .filter(|b| b.device.is_none() && b.path == *path)
            .map_or(title, |b| b.info.name());
        images = images.child(
            Button::new(title.clone())
                .label_with(move |font, ink| {
                    text(title)
                        .font(font)
                        .color(ink)
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate()
                })
                .variant(if chosen {
                    ButtonVariant::Neutral
                } else {
                    ButtonVariant::Ghost
                })
                .disabled(busy)
                .on_press_tx(move |s: &mut State, tx| s.open(image.clone(), tx))
                .build(k)
                .w_full()
                .role("option")
                .focus_selected(chosen)
                .key(format!("image-{index}")),
        );
    }
    if s.images.is_empty() {
        images = images.child(muted("No images open", k));
    }
    col()
        .w(248.0)
        .h_full()
        .padding(20.0)
        .gap(16.0)
        .fill(k.c.layer_01)
        .child(
            text("CarryFS Manager")
                .font(TextStyle::new(18.0, 600.0))
                .color(k.c.text_base),
        )
        .child(
            Button::new("Open image…")
                .disabled(busy)
                .on_press_tx(open_dialog)
                .build(k)
                .key("open-image"),
        )
        .child(
            row()
                .items_center()
                .gap(8.0)
                .child(label("Devices", k).grow(1.0))
                .child(
                    Button::new("Refresh")
                        .variant(ButtonVariant::Ghost)
                        .disabled(busy)
                        .on_press_tx(|s: &mut State, tx| s.discover(tx))
                        .build(k)
                        .key("refresh-devices"),
                ),
        )
        .child(
            col()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .key("device-list")
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(items)
                .scroll_bar(k),
        )
        .child(
            Button::new(if s.show_system {
                "Hide system devices"
            } else {
                "Show system devices"
            })
            .variant(ButtonVariant::Ghost)
            .disabled(busy)
            .on_press(|s: &mut State| s.show_system = !s.show_system)
            .build(k)
            .key("show-system"),
        )
        .child(rule(k))
        .child(label("Image files", k))
        .child(
            col()
                .w_full()
                .max_h(112.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(images)
                .scroll_bar(k),
        )
        .child(rule(k))
        .child(
            Button::new("Software & updates")
                .variant(if s.tab == Tab::Software {
                    ButtonVariant::Neutral
                } else {
                    ButtonVariant::Ghost
                })
                .disabled(busy)
                .on_press(|s: &mut State| s.tab = Tab::Software)
                .build(k)
                .description(if s.tab == Tab::Software {
                    "Current view"
                } else {
                    "Show software versions and updates"
                })
                .key("software"),
        )
        .child(muted(format!("Manager {}", env!("CARGO_PKG_VERSION")), k))
}

/// `labels` name a passed and a failed result.
fn scan_detail(title: &str, result: Option<&ScanResult>, labels: (&str, &str), k: &Kit) -> El {
    let value = result.map_or_else(
        || "Not checked yet".to_owned(),
        |result| {
            let seconds = result
                .at
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            format!(
                "{} · {:02}:{:02} UTC",
                if result.passed { labels.0 } else { labels.1 },
                seconds / 3600 % 24,
                seconds / 60 % 60
            )
        },
    );
    let node = detail(title, value, k);
    match result {
        // The CLI's first line carries the counts: objects, pages, blocks, units.
        Some(result) => {
            let first = result.summary.lines().next().unwrap_or("");
            node.child(
                muted(
                    first.split_once(": ").map_or(first, |(_, counts)| counts),
                    k,
                )
                .wrap(),
            )
            .help(first)
        }
        None => node,
    }
}
fn overview(s: &State, k: &Kit) -> El {
    let busy = s.pending.is_some();
    let mut body = col().w_full().gap(24.0);
    if let Some(browser) = &s.browser {
        let total = browser.info.byte_len;
        let free = browser.info.free_blocks * u64::from(browser.space.block_bytes);
        let available = browser.space.available_blocks * u64::from(browser.space.block_bytes);
        let used = total.saturating_sub(free);
        let key = (browser.info.uuid, browser.info.generation);
        let integrity = s.integrity.get(&key);
        let plan = integrity.and_then(|i| i.plan.as_ref());
        let mut title = row()
            .w_full()
            .gap(16.0)
            .items_center()
            .child(heading(browser.info.name(), k).grow(1.0).min_w(0.0));
        if browser.device.is_none() {
            title = title.child(
                Button::new("Close image")
                    .disabled(busy)
                    .on_press(|s: &mut State| {
                        s.browser = None;
                        s.damaged = None;
                        s.selected = None;
                        s.status = "Image closed.".into();
                        s.error = false;
                    })
                    .build(k)
                    .key("close-image"),
            );
        }
        body = body
            .child(title)
            .child(muted(
                format!(
                    "{} · Format {} · Read-only",
                    if browser.device.is_some() {
                        "Device"
                    } else {
                        "Image file"
                    },
                    browser.info.revision
                ),
                k,
            ))
            .child(
                row()
                    .gap(48.0)
                    .child(detail("Capacity", bytes(total), k))
                    .child(detail("Used", bytes(used), k))
                    .child(detail("Available", bytes(available), k)),
            )
            .child(
                row()
                    .w_full()
                    .h(6.0)
                    .fill(k.c.border_base)
                    .child(el().h(6.0).grow(used as f32).fill(tok::accent(0, false)))
                    .child(el().h(6.0).grow(free as f32)),
            )
            .child(muted(
                format!(
                    "{} reserved for filesystem maintenance",
                    bytes(free.saturating_sub(available))
                ),
                k,
            ))
            .child(rule(k))
            .child(label("Integrity", k))
            .child(
                row()
                    .w_full()
                    .gap(40.0)
                    .child(
                        scan_detail(
                            "Structure",
                            integrity.and_then(|i| i.structure.as_ref()),
                            ("Passed", "Incomplete / failed"),
                            k,
                        )
                        .grow(1.0)
                        .min_w(0.0),
                    )
                    .child(
                        scan_detail(
                            "File data",
                            integrity.and_then(|i| i.scrub.as_ref()),
                            ("Passed", "Incomplete / failed"),
                            k,
                        )
                        .grow(1.0)
                        .min_w(0.0),
                    )
                    .child(
                        scan_detail("Damage", plan, ("None found", "Repairs needed"), k)
                            .grow(1.0)
                            .min_w(0.0),
                    ),
            )
            .child(
                row()
                    .gap(8.0)
                    .child(
                        Button::new("Check structure")
                            .disabled(busy)
                            .on_press_tx(|s: &mut State, tx| s.inspect(Scan::Structure, tx))
                            .build(k)
                            .key("check"),
                    )
                    .child(
                        Button::new("Scrub file data")
                            .disabled(busy)
                            .on_press_tx(|s: &mut State, tx| s.inspect(Scan::Data, tx))
                            .build(k)
                            .key("scrub"),
                    )
                    .child(
                        Button::new("Diagnose damage")
                            .disabled(busy)
                            .on_press_tx(|s: &mut State, tx| s.inspect(Scan::Plan, tx))
                            .build(k)
                            .key("diagnose"),
                    ),
            )
            .child(
                muted(
                    "Checks and diagnosis only read the source. Diagnosis lists what an offline repair would change.",
                    k,
                )
                .wrap(),
            );
        body = body.child(recovery(s, &browser.source(), plan, k));
        body = body
            .child(rule(k))
            .child(label("Format features", k))
            .child(
                row()
                    .gap(40.0)
                    .child(detail(
                        "Permissions & hidden flag",
                        if browser.attributes {
                            "Stored"
                        } else {
                            "Not stored · files read as rw-r--r--, folders as rwxr-xr-x"
                        },
                        k,
                    ))
                    .child(detail(
                        "Preallocation",
                        if browser.preallocation {
                            "Enabled"
                        } else {
                            "Not used"
                        },
                        k,
                    ))
                    .child(detail(
                        "Hard links",
                        if browser.links { "Used" } else { "Not used" },
                        k,
                    )),
            )
            .child(
                muted(
                    if browser.attributes || browser.preallocation || browser.links {
                        "Releases older than these features, including CarryFS 0.1.0, cannot mount this volume. Use a current CarryFS on every computer that uses it."
                    } else {
                        "Every CarryFS release can mount this volume. Preallocation and hard links turn on the first time software reserves space or links a file on it."
                    },
                    k,
                )
                .wrap(),
            );
        if !browser.attributes {
            body = body.child(
                Button::new("Upgrade volume…")
                    .disabled(busy)
                    .on_press_tx(|s: &mut State, tx| {
                        s.upgrade = true;
                        tx.scroll_into_view("upgrade-confirmation");
                    })
                    .build(k)
                    .key("upgrade"),
            );
        }
        if s.upgrade {
            body = body.child(
                col()
                    .w_full()
                    .gap(12.0)
                    .key("upgrade-confirmation")
                    .child(
                    label(
                        format!(
                            "Upgrade {} to store permissions and the hidden flag? Files are kept. CarryFS 0.1.0 can no longer mount it, and the upgrade cannot be undone.",
                            browser.info.name()
                        ),
                        k,
                    )
                    .wrap(),
                )
                .child(
                    row()
                        .gap(8.0)
                        .child(
                            Button::new(if browser.device.is_some() {
                                "Review upgrade in Terminal…"
                            } else {
                                "Upgrade volume"
                            })
                            .disabled(busy)
                            .on_press_tx(|s: &mut State, tx| s.upgrade(tx))
                            .build(k)
                            .key("confirm-upgrade"),
                        )
                        .child(
                            Button::new("Keep current format")
                                .on_press(|s: &mut State| s.upgrade = false)
                                .build(k)
                                .key("cancel-upgrade"),
                        ),
                ),
            );
        }
        body = body.child(
            Button::new(if s.show_details {
                "Hide volume details"
            } else {
                "Show volume details"
            })
            .variant(ButtonVariant::Ghost)
            .on_press(|s: &mut State| s.show_details = !s.show_details)
            .build(k)
            .key("volume-details"),
        );
        if s.show_details {
            body = body
                .child(detail("Source", browser.path.display().to_string(), k))
                .child(detail("Generation", browser.info.generation.to_string(), k))
                .child(detail(
                    "Volume UUID",
                    browser
                        .info
                        .uuid
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>(),
                    k,
                ));
        }
    } else if let Some(damaged) = &s.damaged {
        let image = damaged.source.device.is_none();
        let mut title = row()
            .w_full()
            .gap(16.0)
            .items_center()
            .child(heading(damaged.source.name(), k).grow(1.0).min_w(0.0));
        if image {
            title = title.child(
                Button::new("Close image")
                    .disabled(busy)
                    .on_press(|s: &mut State| {
                        s.damaged = None;
                        s.status = "Image closed.".into();
                        s.error = false;
                    })
                    .build(k)
                    .key("close-image"),
            );
        }
        body = body
            .child(title)
            .child(muted(
                format!(
                    "{} · Cannot be opened",
                    if image { "Image file" } else { "Device" }
                ),
                k,
            ))
            .child(label(format!("Damaged metadata: {}", damaged.error), k).wrap_anywhere())
            .child(scan_detail(
                "Damage",
                damaged.plan.as_ref(),
                ("None found", "Repairs needed"),
                k,
            ))
            .child(
                row().child(
                    Button::new("Diagnose damage")
                        .disabled(busy)
                        .on_press_tx(|s: &mut State, tx| s.inspect(Scan::Plan, tx))
                        .build(k)
                        .key("diagnose"),
                ),
            )
            .child(
                muted(
                    "Diagnosis only reads the source and lists what an offline repair would change.",
                    k,
                )
                .wrap(),
            )
            .child(recovery(s, &damaged.source, damaged.plan.as_ref(), k));
    } else if let Some(device) = &s.device {
        let state = if device.mount.is_some() {
            "Mounted"
        } else if device.whole {
            "Disk"
        } else {
            "Unmounted"
        };
        body=body.child(heading(&device.name,k))
            .child(muted(format!("{} · {} · {state}",device.path().display(),device.filesystem),k).wrap())
            .child(row().gap(32.0).child(detail("Capacity",bytes(device.bytes),k))
                .child(detail("Location",if device.internal{"Internal"}else{"External"},k))
                .child(detail("Reported UUID",device.uuid.as_deref().unwrap_or("Not reported"),k)))
            .child(detail("Mount point",device.mount.as_ref().map_or_else(||"Not mounted".into(),|p|p.display().to_string()),k))
            .child(rule(k)).child(label("Inspect this source",k))
            .child(muted(if device.whole && !device.carryfs(){"Select a volume under this disk to inspect its filesystem."}else if device.mount.is_some(){"Unmount the volume before inspection, checks, or recovery."}else{"Open the volume read-only to inspect its CarryFS format, browse files, and run maintenance. Administrator approval may be required."},k).wrap())
            .child(Button::new("Open read-only").variant(ButtonVariant::Contrast).disabled(busy||device.mount.is_some()||(device.whole&&!device.carryfs()))
                .on_press_tx(|s:&mut State,tx|if let Some(device)=s.device.clone(){s.start("Open device read-only",false,tx,move |_|{let source=Source{path:device.path(),device:Some(device.clone())};opened(Browser::open_device(device),source)});}).build(k).key("inspect-device"));
    } else {
        body=body.child(heading("Choose a source",k))
            .child(label("Select a system device in the sidebar, or open a CarryFS image file.",k).wrap())
            .child(muted("Device discovery does not open raw disks. Inspection, recovery, and changes each require an explicit action on the selected source.",k).wrap());
    }
    if let Some(device) = &s.device {
        let can_change = !device.protected() && device.carryfs() && !device.whole;
        body = body.child(rule(k)).child(
            row()
                .gap(8.0)
                .child(
                    Button::new(if device.mount.is_some() {
                        "Unmount"
                    } else {
                        "Mount"
                    })
                    .disabled(busy || !can_change)
                    .on_press_tx(|s: &mut State, tx| {
                        let action = if s.device.as_ref().is_some_and(|d| d.mount.is_some()) {
                            DeviceAction::Unmount
                        } else {
                            DeviceAction::Mount
                        };
                        s.device_action(action, tx);
                    })
                    .build(k)
                    .key("mount"),
                )
                .child(
                    Button::new("Eject device…")
                        .disabled(busy || device.protected() || device.uuid.is_none())
                        .on_press(|s: &mut State| s.eject = s.device.clone())
                        .build(k)
                        .key("eject"),
                )
                .child(
                    Button::new("Format partition…")
                        .disabled(busy || !device.external_partition() || device.mount.is_some())
                        .on_press(|s: &mut State| {
                            s.format = s.device.clone();
                            s.confirmation = TextEdit::new("");
                        })
                        .build(k)
                        .key("format"),
                ),
        );
        if device.protected() {
            body=body.child(muted("Internal and system devices are protected from mount, eject, and format actions.",k).wrap());
        }
    }
    if let Some(device) = &s.eject {
        body = body
            .child(
                label(
                    format!(
                        "Eject {}? Other mounted volumes on its device will also be ejected.",
                        device.name
                    ),
                    k,
                )
                .wrap(),
            )
            .child(
                row()
                    .gap(8.0)
                    .child(
                        Button::new("Eject device")
                            .disabled(busy)
                            .on_press_tx(|s: &mut State, tx| {
                                s.device_action(DeviceAction::Eject, tx)
                            })
                            .build(k)
                            .key("confirm-eject"),
                    )
                    .child(
                        Button::new("Keep connected")
                            .on_press(|s: &mut State| s.eject = None)
                            .build(k),
                    ),
            );
    }
    if let Some(device) = &s.format {
        body=body.child(rule(k)).child(label(format!("Erase {} · {} · {}",device.name,device.id,bytes(device.bytes)),k).wrap())
            .child(muted("All files on this partition will be lost. This is formatting, not a data-preserving format upgrade.",k).wrap())
            .child(TextInput::new(&s.confirmation).label(format!("Type {} to confirm",device.id)).placeholder(&device.id)
                .on_edit(|s:&mut State|&mut s.confirmation).build(k).key("format-confirmation"))
            .child(row().gap(8.0).child(Button::new("Review formatting in Terminal…").disabled(busy||s.confirmation.text!=device.id)
                .on_press_tx(|s:&mut State,tx|if let Some(device)=s.format.clone(){let confirmation=s.confirmation.text.to_owned();s.start("Prepare format confirmation",false,tx,move |_|platform::format_in_terminal(&device,&confirmation).map(Update::Report));}).build(k).key("confirm-format"))
                .child(Button::new("Cancel formatting").on_press(|s:&mut State|s.format=None).build(k)));
    }
    body
}
/// Diagnosis results, recovery, and in-place repair for a source that may not open.
fn recovery(s: &State, source: &Source, plan: Option<&ScanResult>, k: &Kit) -> El {
    let busy = s.pending.is_some();
    let mut body = col().w_full().gap(24.0);
    if let Some(plan) = plan.filter(|plan| !plan.passed) {
        let actions: Vec<_> = plan.summary.lines().skip(1).take(12).collect();
        body = body.child(muted(actions.join("\n"), k).wrap_anywhere());
    }
    body = body
            .child(rule(k))
            .child(label("Recovery", k))
            .child(
                row()
                    .gap(8.0)
                    .child(
                        Button::new("Recover files…")
                            .disabled(busy)
                            .on_press_tx(|s: &mut State, tx| fsck_dialog(s, tx, false))
                            .build(k)
                            .key("recover"),
                    )
                    .child(
                        Button::new("Rebuild into new image…")
                            .disabled(busy)
                            .on_press_tx(|s: &mut State, tx| fsck_dialog(s, tx, true))
                            .build(k)
                            .key("rebuild"),
                    ),
            )
            .child(
                muted(
                    "Both keep the source read-only and work around damaged metadata. Recovery exports accessible files, with their permissions, hidden flags and hard links, to a new folder. Rebuild copies them into a new verified image. Each writes a report beside its destination; unreadable ranges stay missing, never zero-filled.",
                    k,
                )
                .wrap(),
            );
    if source.device.is_none() && plan.is_some_and(ScanResult::repairable) {
        body = body.child(
            Button::new("Repair in place…")
                .disabled(busy)
                .on_press_tx(|s: &mut State, tx| {
                    s.repair = true;
                    tx.scroll_into_view("repair-confirmation");
                })
                .build(k)
                .key("repair"),
        );
    } else if plan.is_some_and(|plan| !plan.passed) {
        body = body.child(
                muted(
                    "In-place repair is not offered here: it is blocked, would discard data, or the source is a device. Recover or rebuild instead; RECOVERY.md covers the command-line options.",
                    k,
                )
                .wrap(),
            );
    }
    if s.repair {
        body = body.child(
                col()
                    .w_full()
                    .gap(12.0)
                    .key("repair-confirmation")
                    .child(
                        label(
                            format!(
                                "Repair {} in place? This rewrites its metadata. Recover or rebuild first if you need the current state.",
                                source.name()
                            ),
                            k,
                        )
                        .wrap(),
                    )
                    .child(
                        row()
                            .gap(8.0)
                            .child(
                                Button::new("Repair volume")
                                    .disabled(busy)
                                    .on_press_tx(|s: &mut State, tx| s.repair(tx))
                                    .build(k)
                                    .key("confirm-repair"),
                            )
                            .child(
                                Button::new("Cancel repair")
                                    .on_press(|s: &mut State| s.repair = false)
                                    .build(k)
                                    .key("cancel-repair"),
                            ),
                    ),
            );
    }
    body
}
/// Ask for a new destination, then recover files to a folder or rebuild a new image.
fn fsck_dialog(s: &mut State, tx: &mut Tx, rebuild: bool) {
    let Some(source) = s.source() else {
        return;
    };
    tx.save_file(
        FileDialogOptions {
            title: if rebuild {
                "Rebuild into a new image"
            } else {
                "Recover files to a new folder"
            }
            .into(),
            file_name: Some(
                if rebuild {
                    "Recovered.cfs"
                } else {
                    "CarryFS recovered files"
                }
                .into(),
            ),
            ..Default::default()
        },
        move |s: &mut State, result, tx| match result {
            Ok(Some(path)) => {
                let source = source.clone();
                let (name, mode) = if rebuild {
                    ("Rebuild into new image", Fsck::Rebuild(path))
                } else {
                    ("Recover files", Fsck::Recover(path))
                };
                s.start(name, true, tx, move |cancel| {
                    operation::fsck(source, mode, cancel).map(Update::Report)
                });
            }
            Ok(None) => {}
            Err(e) => s.apply(Err(e.to_string())),
        },
    );
}
fn recover_dialog(s: &mut State, tx: &mut Tx) {
    let Some(browser) = s.browser.clone() else {
        return;
    };
    tx.save_file(
        FileDialogOptions {
            title: "Recover folder to a new destination".into(),
            file_name: Some("CarryFS recovered files".into()),
            ..Default::default()
        },
        move |s: &mut State, result, tx| match result {
            Ok(Some(path)) => {
                let browser = browser.clone();
                s.start("Recover files", true, tx, move |cancel| {
                    operation::recover(browser, &path, cancel).map(Update::Report)
                });
            }
            Ok(None) => {}
            Err(e) => s.apply(Err(e.to_string())),
        },
    );
}
fn files(s: &State, k: &Kit) -> El {
    let Some(browser) = &s.browser else {
        return col().gap(12.0).child(heading("Files",k)).child(label("Open a CarryFS image or an unmounted device read-only from Overview to browse its files.",k).wrap());
    };
    let busy = s.pending.is_some();
    let mut crumbs = row().items_center().gap(4.0);
    for (index, (_, name)) in browser.trail.iter().enumerate() {
        let title = if index == 0 { "Root" } else { name }.to_owned();
        crumbs = crumbs.child(
            Button::new(title.clone())
                .label_with(move |font, ink| {
                    text(title)
                        .font(font)
                        .color(ink)
                        .min_w(0.0)
                        .grow(1.0)
                        .truncate()
                })
                .variant(ButtonVariant::Ghost)
                .disabled(busy)
                .on_press_tx(move |s: &mut State, tx| {
                    if let Some(browser) = &s.browser {
                        s.navigate(browser.trail[..=index].to_vec(), tx);
                    }
                })
                .build(k)
                .key(format!("crumb-{index}")),
        );
    }
    let mut rows = col().w_full().gap(2.0);
    for (index, entry) in browser.entries.iter().enumerate() {
        let name = String::from_utf8_lossy(entry.item.name.stored()).into_owned();
        let size = if entry.item.kind == ObjectKind::Directory {
            "—".into()
        } else {
            bytes(entry.attrs.size)
        };
        rows = rows.child(
            row()
                .key(format!("entry-{}", entry.item.object.get()))
                .w_full()
                .h(38.0)
                .px(12.0)
                .gap(12.0)
                .items_center()
                .role("button")
                .label(format!("{}: {name}", kind(entry.item.kind)))
                .focusable(!busy)
                .disabled(busy)
                .fill(if s.selected == Some(index) {
                    k.c.layer_01
                } else {
                    k.c.bg_base
                })
                .radius(5.0)
                .hover(k.c.border_base)
                .on_press(move |s: &mut State| {
                    if s.pending.is_none() {
                        s.selected = Some(index);
                    }
                })
                .on_double_press_tx(move |s: &mut State, tx| {
                    if s.pending.is_none() {
                        s.enter(index, tx);
                    }
                })
                .child(
                    text(name)
                        .font(type_::BODY)
                        .color(if entry.attrs.hidden {
                            k.c.text_weak
                        } else {
                            k.c.text_base
                        })
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate(),
                )
                .child(muted(kind(entry.item.kind), k).w(100.0))
                .child(muted(permissions(entry.attrs.mode), k).w(90.0))
                .child(muted(size, k).w(90.0)),
        );
    }
    if browser.entries.is_empty() {
        rows = rows.child(label("This folder is empty.", k).padding(24.0));
    }
    let mut body = col()
        .w_full()
        .h_full()
        .min_h(0.0)
        .gap(16.0)
        .child(
            col()
                .w_full()
                .gap(4.0)
                .child(heading(browser.info.name(), k))
                .child(muted(browser.path.display().to_string(), k).truncate_middle())
                .child(muted(folder_summary(browser), k)),
        )
        .child(
            row()
                .gap(8.0)
                .child(
                    Button::new("Up")
                        .disabled(busy || browser.trail.len() == 1)
                        .on_press_tx(|s: &mut State, tx| {
                            if let Some(browser) = &s.browser {
                                s.navigate(browser.trail[..browser.trail.len() - 1].to_vec(), tx);
                            }
                        })
                        .build(k)
                        .key("up"),
                )
                .child(el().grow(1.0).min_w(0.0).scroll_x().child(crumbs))
                .child(
                    Button::new("Recover this folder…")
                        .disabled(busy)
                        .on_press_tx(recover_dialog)
                        .build(k)
                        .key("recover-folder"),
                ),
        )
        .child(
            row()
                .px(12.0)
                .gap(12.0)
                .child(muted("Name", k).grow(1.0))
                .child(muted("Kind", k).w(100.0))
                .child(muted("Permissions", k).w(90.0))
                .child(muted("Size", k).w(90.0)),
        )
        .child(
            col()
                .w_full()
                .key("file-list")
                .grow(1.0)
                .min_h(0.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(rows)
                .scroll_bar(k),
        );
    if let Some(index) = s.selected
        && let Some(entry) = browser.entries.get(index)
    {
        let folder = entry.item.kind == ObjectKind::Directory;
        let regular = entry.item.kind == ObjectKind::File;
        body = body.child(
            row()
                .items_center()
                .gap(12.0)
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(4.0)
                        .child(
                            label(
                                String::from_utf8_lossy(entry.item.name.stored()).into_owned(),
                                k,
                            )
                            .truncate(),
                        )
                        .child(muted(
                            format!(
                                "{} · {} · {} allocated · {}{}{} · Object {}",
                                kind(entry.item.kind),
                                bytes(entry.attrs.size),
                                bytes(entry.attrs.allocated_bytes),
                                permissions(entry.attrs.mode),
                                if entry.attrs.hidden { " · Hidden" } else { "" },
                                if entry.attrs.links > 1 {
                                    format!(" · {} names", entry.attrs.links)
                                } else {
                                    String::new()
                                },
                                entry.item.object.get()
                            ),
                            k,
                        )),
                )
                .child(
                    Button::new(if folder {
                        "Open folder"
                    } else {
                        "Export file…"
                    })
                    .disabled(busy || !(folder || regular))
                    .on_press_tx(move |s: &mut State, tx| {
                        if folder {
                            s.enter(index, tx);
                            return;
                        }
                        if let Some(browser) = s.browser.clone()
                            && let Some(entry) = browser.entries.get(index)
                        {
                            let object = entry.item.object;
                            let file_name =
                                String::from_utf8_lossy(entry.item.name.stored()).into_owned();
                            tx.save_file(
                                FileDialogOptions {
                                    title: "Export file to a new path".into(),
                                    file_name: Some(file_name),
                                    ..Default::default()
                                },
                                move |s: &mut State, result, tx| match result {
                                    Ok(Some(path)) => {
                                        let browser = browser.clone();
                                        s.start("Export file", false, tx, move |_| {
                                            browser.export(object, &path).map(Update::Report)
                                        });
                                    }
                                    Ok(None) => {}
                                    Err(e) => s.apply(Err(e.to_string())),
                                },
                            );
                        }
                    })
                    .build(k)
                    .key("selection-action"),
                ),
        );
    } else {
        body = body.child(muted(
            "Select a file to export. Double-click a folder to open it.",
            k,
        ));
    }
    body
}
/// Counts and sizes of the listed folder's direct entries.
fn folder_summary(browser: &Browser) -> String {
    let count = |wanted| {
        browser
            .entries
            .iter()
            .filter(|e| e.item.kind == wanted)
            .count()
    };
    let size: u64 = browser
        .entries
        .iter()
        .filter(|e| e.item.kind == ObjectKind::File)
        .map(|e| e.attrs.size)
        .sum();
    let allocated: u64 = browser
        .entries
        .iter()
        .map(|e| e.attrs.allocated_bytes)
        .sum();
    let hidden = browser.entries.iter().filter(|e| e.attrs.hidden).count();
    let mut summary = format!(
        "Folders {} · Files {} · Symlinks {} · {} in files · {} allocated",
        count(ObjectKind::Directory),
        count(ObjectKind::File),
        count(ObjectKind::Symlink),
        bytes(size),
        bytes(allocated)
    );
    if hidden > 0 {
        summary += &format!(" · Hidden {hidden}");
    }
    summary
}
fn activity(s: &State, k: &Kit) -> El {
    let mut body = col().w_full().gap(20.0).child(heading("Activity", k));
    if s.history.is_empty() {
        body = body.child(
            label(
                "Operation results will appear here, with their source and completion time.",
                k,
            )
            .wrap(),
        );
    }
    for record in s.history.iter().rev() {
        let seconds = record
            .at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let time = format!(
            "{:02}:{:02}:{:02} UTC",
            seconds / 3600 % 24,
            seconds / 60 % 60,
            seconds % 60
        );
        body = body.child(
            col()
                .w_full()
                .gap(8.0)
                .child(rule(k))
                .child(
                    label(
                        format!(
                            "{} · {} · {time}",
                            record.task,
                            if record.failed {
                                "Incomplete / failed"
                            } else {
                                "Completed"
                            }
                        ),
                        k,
                    )
                    .wrap(),
                )
                .child(muted(&record.target, k).wrap_anywhere())
                .child(label(&record.message, k).wrap_anywhere()),
        );
    }
    body
}
fn software(s: &State, k: &Kit) -> El {
    let busy = s.pending.is_some();
    let core = include_str!("../../Cargo.toml")
        .lines()
        .find_map(|s| {
            s.strip_prefix("version = \"")
                .and_then(|s| s.strip_suffix('"'))
        })
        .unwrap_or("Unknown");
    let mut body=col().w_full().gap(24.0).child(heading("Software & updates",k))
        .child(muted("Software releases and volume formats are versioned separately.",k).wrap())
        .child(row().gap(40.0).child(detail("Manager",env!("CARGO_PKG_VERSION"),k)).child(detail("Bundled core",core,k))
            .child(detail("Installed adapter",s.versions.installed.as_deref().unwrap_or("Not detected"),k)).child(detail("Supported format","Revision 1 · permissions, preallocation, hard links",k)))
        .child(detail("Latest stable release",s.versions.latest.as_deref().unwrap_or("Not checked"),k))
        .child(row().gap(8.0).child(Button::new("Check for updates").disabled(busy).on_press_tx(|s:&mut State,tx|s.start("Check software releases",false,tx,|_|platform::check_updates().map(Update::Versions))).build(k).key("check-updates"))
            .child(Button::new("Refresh installed version").disabled(busy).on_press(|s:&mut State|{let latest=s.versions.latest.clone();s.versions=platform::versions();s.versions.latest=latest;}).build(k).key("refresh-versions")))
        .child(rule(k)).child(label("Install CarryFS software",k))
        .child(muted("The interactive installer opens in Terminal. It verifies the release package and requires all CarryFS volumes to be ejected before replacing the adapter. It asks for confirmation and administrator permission.",k).wrap())
        .child(muted("This updates software only. It does not convert or reformat any volume, and an unknown format is never automatically rewritten.",k).wrap());
    if let Some(version) = &s.versions.latest {
        let version = version.clone();
        body = body.child(
            Button::new(format!("Review installation of {version}…"))
                .disabled(busy || !s.versions.update_available())
                .on_press_tx(move |s: &mut State, tx| {
                    let version = version.clone();
                    s.start("Prepare software installer", false, tx, move |_| {
                        platform::install(&version).map(Update::Report)
                    });
                })
                .build(k)
                .key("install-update"),
        );
    }
    body.child(rule(k)).child(label("Volume format upgrades",k))
        .child(muted("Revision 1 volumes gain features in place, without rewriting files. Upgrade volume, in a volume's Overview, adds permissions and the hidden flag. Preallocation and hard links turn on the first time software reserves space or links a file. None can be turned off.",k).wrap())
        .child(muted("CarryFS 0.1.0 cannot mount a volume with any of them, and a software rollback does not undo them. Update CarryFS on every computer that uses a volume before upgrading it.",k).wrap())
}
fn view(s: &State, _: &mut Cx<State>) -> El {
    let t = tok::theme(false);
    let k = Kit::from_palette(PaletteSeed {
        bg: t.bg,
        fg: t.text,
        accent: tok::accent(0, false),
    });
    let busy = s.pending.is_some();
    let mut nav = row().gap(8.0).role("tablist").focus_group();
    for (tab, title, key) in [
        (Tab::Overview, "Overview", "tab-overview"),
        (Tab::Files, "Files", "tab-files"),
        (Tab::Activity, "Activity", "tab-activity"),
    ] {
        nav = nav.child(
            Button::new(title)
                .variant(if s.tab == tab {
                    ButtonVariant::Neutral
                } else {
                    ButtonVariant::Ghost
                })
                .on_press(move |s: &mut State| s.tab = tab)
                .build(&k)
                .role("tab")
                .focus_selected(s.tab == tab)
                .key(key),
        );
    }
    let body = match s.tab {
        Tab::Overview => overview(s, &k),
        Tab::Files => files(s, &k),
        Tab::Activity => activity(s, &k),
        Tab::Software => software(s, &k),
    };
    let main = if s.tab == Tab::Files {
        el().w_full().grow(1.0).min_h(0.0).child(body)
    } else {
        col()
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .child(body)
            .scroll_bar(&k)
    };
    let mut footer = col().gap(8.0);
    if busy {
        footer = footer.child(
            row()
                .gap(12.0)
                .items_center()
                .child(label(format!("{} · {}s", s.task, s.elapsed_seconds), &k).grow(1.0))
                .child(
                    Button::new("Cancel operation")
                        .disabled(s.cancel.is_none())
                        .on_press(|s: &mut State| {
                            if let Some(cancel) = &s.cancel {
                                cancel.cancel();
                                s.status = "Cancelling at the process boundary…".into();
                            }
                        })
                        .build(&k)
                        .key("cancel-operation"),
                ),
        );
    }
    footer = footer.child(
        text(
            s.status
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(220)
                .collect::<String>(),
        )
        .key("status")
        .role(if s.error { "alert" } else { "status" })
        .live_region(LiveRegion::Polite)
        .font(type_::LABEL)
        .color(if s.error {
            tok::accent(1, false)
        } else {
            k.c.text_weak
        })
        .wrap_anywhere(),
    );
    row()
        .w_full()
        .h_full()
        .fill(k.c.bg_base)
        .child(sidebar(s, &k))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .h_full()
                .padding(24.0)
                .gap(20.0)
                .child(nav)
                .child(rule(&k))
                .child(main)
                .child(footer),
        )
}
fn capture_sources(s: &mut State) {
    s.initialized = true;
    if let Some(path) = s.startup_image.take() {
        s.apply(
            opened(Browser::open(path.clone()), Source { path, device: None })
                .map_err(|e| e.to_string()),
        );
    }
    s.apply(
        platform::discover()
            .map(Update::Devices)
            .map_err(|e| e.to_string()),
    );
}
fn main() {
    rune::run(AppSpec {
        min_size: Some((940.0, 640.0)),
        scenes: &[
            ("overview", capture_sources),
            ("files", |s: &mut State| {
                capture_sources(s);
                s.tab = Tab::Files;
            }),
            ("software", |s: &mut State| {
                capture_sources(s);
                s.tab = Tab::Software;
            }),
            ("activity", |s: &mut State| {
                capture_sources(s);
                s.tab = Tab::Activity;
            }),
        ],
        poll: Some(poll),
        ..AppSpec::new("CarryFS Manager", (1120.0, 760.0), State::default, view)
    });
}
#[cfg(test)]
mod tests;
