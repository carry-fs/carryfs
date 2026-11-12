//! Native CarryFS manager, following Rune's state/view/AppSpec pattern.
mod model;
mod operation;
mod platform;

use carryfs_core::ObjectKind;
use model::{Browser, bytes, kind};
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
}

struct State {
    browser: Option<Browser>,
    device: Option<Device>,
    devices: Vec<Device>,
    show_system: bool,
    expanded_devices: BTreeSet<String>,
    show_details: bool,
    startup_image: Option<PathBuf>,
    elapsed_seconds: u64,
    integrity: BTreeMap<VolumeKey, Integrity>,
    inspection: Option<(VolumeKey, bool)>,
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
}
impl State {
    fn empty() -> Self {
        Self {
            browser: None,
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
        if let Some((key, scrub)) = self.inspection.take() {
            let message = match &result {
                Ok(Update::Report(message)) => message.as_str(),
                Err(message) => message.as_str(),
                _ => "No result reported",
            };
            let scan = ScanResult {
                summary: message
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(180)
                    .collect(),
                passed: !self.error,
                at: SystemTime::now(),
            };
            let state = self.integrity.entry(key).or_default();
            if scrub {
                state.scrub = Some(scan);
            } else {
                state.structure = Some(scan);
            }
        }

        match result {
            Ok(Update::Browser(browser)) => {
                self.status = format!("{} items · Read-only access", browser.entries.len());
                self.device = browser.device.clone();
                if browser.device.is_none() && !self.images.contains(&browser.path) {
                    self.images.push(browser.path.clone());
                }
                self.browser = Some(*browser);
                self.selected = None;
            }
            Ok(Update::Device(device)) => {
                self.status = format!("Selected {}", device.id);
                self.device = Some(*device);
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
        self.start("Open image", false, tx, move |_| {
            Browser::open(path).map(|b| Update::Browser(Box::new(b)))
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
    fn inspect(&mut self, scrub: bool, tx: &mut Tx) {
        if self.pending.is_some() {
            return;
        }
        if let Some(browser) = self.browser.clone() {
            self.inspection = Some(((browser.info.uuid, browser.info.generation), scrub));
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
            .is_some_and(|b| b.device.is_none() && b.path == *path);
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

fn scan_detail(title: &str, result: Option<&ScanResult>, k: &Kit) -> El {
    let value = result.map_or_else(
        || "Not checked yet".to_owned(),
        |result| {
            let seconds = result
                .at
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            format!(
                "{} · {:02}:{:02} UTC",
                if result.passed {
                    "Passed"
                } else {
                    "Incomplete / failed"
                },
                seconds / 3600 % 24,
                seconds / 60 % 60
            )
        },
    );
    let node = detail(title, value, k);
    if let Some(result) = result {
        node.help(&result.summary)
    } else {
        node
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
                    .gap(40.0)
                    .child(scan_detail(
                        "Structure",
                        integrity.and_then(|i| i.structure.as_ref()),
                        k,
                    ))
                    .child(scan_detail(
                        "File data",
                        integrity.and_then(|i| i.scrub.as_ref()),
                        k,
                    )),
            )
            .child(
                row()
                    .gap(8.0)
                    .child(
                        Button::new("Check structure")
                            .disabled(busy)
                            .on_press_tx(|s: &mut State, tx| s.inspect(false, tx))
                            .build(k)
                            .key("check"),
                    )
                    .child(
                        Button::new("Scrub file data")
                            .disabled(busy)
                            .on_press_tx(|s: &mut State, tx| s.inspect(true, tx))
                            .build(k)
                            .key("scrub"),
                    )
                    .child(
                        Button::new("Recover files…")
                            .disabled(busy)
                            .on_press_tx(recover_dialog)
                            .build(k)
                            .key("recover"),
                    ),
            )
            .child(
                muted(
                    "Checks read the source. Recovery saves accessible files to a new destination.",
                    k,
                )
                .wrap(),
            )
            .child(
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
                .on_press_tx(|s:&mut State,tx|if let Some(device)=s.device.clone(){s.start("Open device read-only",false,tx,move |_|Browser::open_device(device).map(|b|Update::Browser(Box::new(b))));}).build(k).key("inspect-device"));
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
                .child(label(name, k).grow(1.0).min_w(0.0).truncate())
                .child(muted(kind(entry.item.kind), k).w(100.0))
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
                .child(muted(browser.path.display().to_string(), k).truncate_middle()),
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
                                "{} · {} · Object {}",
                                kind(entry.item.kind),
                                bytes(entry.attrs.size),
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
            .child(detail("Installed adapter",s.versions.installed.as_deref().unwrap_or("Not detected"),k)).child(detail("Supported format","Revision 1",k)))
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
        .child(muted("Supported on-disk revision: 1. No in-place migration is currently available. Revision 1 is not frozen, so the revision number alone is not a compatibility guarantee between releases.",k).wrap())
        .child(muted("For a future incompatible format, keep the original volume intact and migrate through a separate verified copy. A software rollback cannot undo a disk-format conversion.",k).wrap())
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
            Browser::open(path)
                .map(|b| Update::Browser(Box::new(b)))
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
