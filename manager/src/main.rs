//! Native CarryFS manager, following Rune's state/view/AppSpec pattern.
// A release build opens no console window; debug builds keep it for logs.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
mod model;
mod operation;
mod platform;
#[cfg(target_os = "macos")]
mod privileged;
mod view;

use model::{Browser, Source};
use operation::Fsck;
use platform::{Device, DeviceAction, Versions};
use rune::{AppSpec, TextEdit, Tx};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant, SystemTime},
};
#[cfg(test)]
use view::folder_summary;
use view::view;

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
    /// First Aid's diagnosis and, for a clean volume that opens, its scrub.
    /// `opened` is the volume First Aid opened because it wasn't open yet.
    FirstAid {
        opened: Option<Box<Update>>,
        key: Option<VolumeKey>,
        plan: ScanResult,
        scrub: Option<ScanResult>,
    },
}
struct Record {
    task: String,
    target: String,
    message: String,
    failed: bool,
    at: SystemTime,
}
type VolumeKey = ([u8; 16], u64);
#[derive(Clone)]
struct ScanResult {
    summary: String,
    passed: bool,
    findings: bool,
    can_repair: bool,
    at: SystemTime,
}
impl ScanResult {
    fn new(result: platform::Result<String>) -> Self {
        let (passed, findings, summary) = match result {
            Ok(text) => (true, false, text),
            Err(error) => (
                false,
                error.is::<operation::ScanFinding>(),
                error.to_string(),
            ),
        };
        let can_repair = findings
            && summary.lines().next().is_some_and(|line| {
                line.starts_with("Repair plan:")
                    && line.ends_with("; clean: false; possible data loss: false; blocked: false")
            });
        Self {
            summary: summary.chars().take(8 * 1024).collect(),
            passed,
            findings,
            can_repair,
            at: SystemTime::now(),
        }
    }
    fn repairable(&self) -> bool {
        self.can_repair
    }
    fn incomplete(&self) -> bool {
        !self.passed && !self.findings
    }
}
#[derive(Default)]
struct Integrity {
    plan: Option<ScanResult>,
    scrub: Option<ScanResult>,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Check {
    Structure,
    Data,
}
/// A check of the open volume that runs beside other work; a task that needs
/// the volume stops it.
struct Background {
    check: Check,
    key: VolumeKey,
    control: Arc<operation::Control>,
    receiver: Receiver<ScanResult>,
    started: Instant,
}
/// How long nothing else must run before the file data check starts by itself.
const QUIET: Duration = Duration::from_secs(30);

/// A source whose metadata is too damaged to open.
struct Damaged {
    source: Source,
    error: String,
    plan: Option<ScanResult>,
}

/// A task that runs in a sheet: asked, running, then reported.
#[derive(Clone, PartialEq, Eq, Debug)]
enum SheetKind {
    FirstAid,
    Erase,
    Eject,
    Upgrade,
    Repair,
    Recover,
    Install(String),
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Ask,
    Running,
    Done,
}
struct Sheet {
    kind: SheetKind,
    phase: Phase,
    /// The outcome once done: success and its report.
    outcome: Option<(bool, String)>,
    /// Where recovery wrote, for Show in Finder.
    destination: Option<PathBuf>,
}

struct State {
    browser: Option<Browser>,
    damaged: Option<Damaged>,
    device: Option<Device>,
    devices: Vec<Device>,
    show_system: bool,
    expanded_devices: BTreeSet<String>,
    startup_image: Option<PathBuf>,
    elapsed_seconds: u64,
    integrity: BTreeMap<VolumeKey, Integrity>,
    images: Vec<PathBuf>,
    /// A disk to select once disks are listed: `--select ID`, which an
    /// unlocking Windows manager passes to its elevated successor.
    startup_device: Option<String>,
    tab: Tab,
    selected: Option<usize>,
    pending: Option<Receiver<Result<Update, String>>>,
    cancel: Option<Arc<operation::Control>>,
    started: Instant,
    task: String,
    target: String,
    initialized: bool,
    /// Rediscover devices once the current task finishes.
    refresh: bool,
    history: Vec<Record>,
    versions: Versions,
    status: String,
    error: bool,
    sheet: Option<Sheet>,
    erase_name: TextEdit,
    background: Option<Background>,
    /// When the last task started or finished.
    quiet_since: Instant,
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
            startup_image: None,
            elapsed_seconds: 0,
            integrity: BTreeMap::new(),
            images: Vec::new(),
            startup_device: None,
            tab: Tab::Overview,
            selected: None,
            pending: None,
            cancel: None,
            started: Instant::now(),
            task: String::new(),
            target: String::new(),
            initialized: true,
            refresh: false,
            history: Vec::new(),
            versions: platform::versions(),
            status: "Select a disk or open a disk image.".into(),
            error: false,
            sheet: None,
            erase_name: TextEdit::new(""),
            background: None,
            quiet_since: Instant::now(),
        }
    }
    fn target_name(&self) -> String {
        self.device
            .as_ref()
            .map(|d| format!("{} · {}", d.name, d.id))
            .or_else(|| self.source().map(|s| s.path.display().to_string()))
            .unwrap_or_else(|| "CarryFS Manager".into())
    }
    fn apply(&mut self, result: Result<Update, String>) {
        self.error = result.is_err();
        match result {
            Ok(Update::Browser(browser)) => self.apply_browser(*browser),
            Ok(Update::Reopened(browser, report)) => {
                self.apply_browser(*browser);
                self.status = report;
            }
            Ok(Update::Damaged(damaged)) => {
                self.status = format!(
                    "{} cannot be opened: {}. Run First Aid to diagnose it, or recover its files.",
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
                self.status = format!("Selected {}", device.name);
                self.device = Some(*device);
                self.damaged = None;
                self.browser = None;
                self.selected = None;
            }
            Ok(Update::Devices(devices)) => {
                self.status = "Disks refreshed".into();
                if let Some(selected) = &self.device {
                    // Take the fresh record: an erase or mount changes format and mount point.
                    match devices
                        .iter()
                        .find(|d| d.id == selected.id && d.uuid == selected.uuid)
                    {
                        Some(fresh) => self.device = Some(fresh.clone()),
                        None => {
                            self.stop_background();
                            self.device = None;
                            self.browser = None;
                            self.damaged = None;
                            self.selected = None;
                            self.status = "The selected disk is no longer connected.".into();
                        }
                    }
                }
                self.devices = devices;
            }
            Ok(Update::Report(message)) => self.status = message,
            Ok(Update::Versions(versions)) => {
                self.versions = versions;
                self.status = "Software versions refreshed.".into();
            }
            Ok(Update::FirstAid {
                opened,
                key,
                plan,
                scrub,
            }) => {
                if let Some(update) = opened {
                    self.apply(Ok(*update));
                }
                self.error = !plan.passed || scrub.as_ref().is_some_and(|s| !s.passed);
                self.status =
                    if plan.incomplete() || scrub.as_ref().is_some_and(ScanResult::incomplete) {
                        "First Aid did not complete.".into()
                    } else if self.error {
                        "First Aid found problems.".into()
                    } else {
                        "First Aid found no problems.".into()
                    };
                match key {
                    Some(key) => {
                        let state = self.integrity.entry(key).or_default();
                        state.plan = Some(plan);
                        if scrub.is_some() {
                            state.scrub = scrub;
                        }
                    }
                    None => {
                        if let Some(damaged) = &mut self.damaged {
                            damaged.plan = Some(plan);
                        }
                    }
                }
            }
            Err(message) => self.status = message,
        }
        if let Some(sheet) = &mut self.sheet
            && sheet.phase == Phase::Running
        {
            if !self.error && matches!(sheet.kind, SheetKind::Eject) {
                self.sheet = None;
            } else {
                sheet.phase = Phase::Done;
                sheet.outcome = Some((!self.error, self.status.clone()));
                if !self.error && matches!(sheet.kind, SheetKind::Erase) {
                    self.refresh = true;
                }
            }
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
        self.status = format!("{} opened read-only", browser.info.name());
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
        // Discovery and installs leave the volume alone; other tasks may need it.
        if !matches!(name, "Discover disks" | "Install CarryFS") {
            self.stop_background();
        }
        self.quiet_since = Instant::now();
        let cancel = Arc::new(operation::Control::default());
        self.cancel = cancellable.then(|| cancel.clone());
        self.task = name.into();
        self.target = if name == "Discover disks" {
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
    /// Run `check` on the open volume beside other work.
    fn check(&mut self, check: Check, tx: &mut Tx) {
        let Some(browser) = self.browser.clone() else {
            return;
        };
        self.stop_background();
        let key = (browser.info.uuid, browser.info.generation);
        let control = Arc::new(operation::Control::default());
        let job = control.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = match check {
                Check::Structure => operation::fsck(browser.source(), Fsck::Plan, job),
                Check::Data => operation::inspect(browser, true, job),
            };
            let _ = sender.send(ScanResult::new(result));
        });
        self.background = Some(Background {
            check,
            key,
            control,
            receiver,
            started: Instant::now(),
        });
        tx.wake_after(Duration::from_millis(250));
    }
    fn stop_background(&mut self) {
        if let Some(background) = self.background.take()
            && let Err(error) = background.control.stop()
        {
            eprintln!("CarryFS Manager: stop background check: {error}");
        }
    }
    fn finish_check(&mut self, background: Background, result: ScanResult) {
        self.history.push(Record {
            task: match background.check {
                Check::Structure => "Check structure",
                Check::Data => "Check file data",
            }
            .into(),
            target: self.target_name(),
            message: result.summary.lines().next().unwrap_or_default().to_owned(),
            failed: !result.passed,
            at: SystemTime::now(),
        });
        let state = self.integrity.entry(background.key).or_default();
        match background.check {
            Check::Structure => state.plan = Some(result),
            Check::Data => state.scrub = Some(result),
        }
    }
    /// Check the open volume unprompted: its structure at once, its file
    /// data once nothing else has run for [`QUIET`], since that reads every
    /// stored byte. Returns whether a check started.
    fn schedule_checks(&mut self, tx: &mut Tx) -> bool {
        if self.background.is_some() || self.pending.is_some() || self.sheet.is_some() {
            return false;
        }
        let Some(browser) = &self.browser else {
            return false;
        };
        // Never ask for the administrator password unprompted.
        if browser.device.is_some() && !unlocked() {
            return false;
        }
        let state = self
            .integrity
            .get(&(browser.info.uuid, browser.info.generation));
        let Some(plan) = state.and_then(|i| i.plan.as_ref()) else {
            self.check(Check::Structure, tx);
            return true;
        };
        if !plan.passed || state.is_some_and(|i| i.scrub.is_some()) {
            return false;
        }
        let waited = self.quiet_since.elapsed();
        if waited < QUIET {
            tx.wake_after(QUIET - waited);
            return false;
        }
        self.check(Check::Data, tx);
        true
    }
    /// Open `kind`'s sheet at its question.
    fn ask(&mut self, kind: SheetKind) {
        if kind == SheetKind::Erase {
            let name = self
                .browser
                .as_ref()
                .map(|b| b.info.name())
                .or_else(|| self.device.as_ref().map(|d| d.name.clone()))
                .filter(|name| carryfs_core::VolumeLabel::parse(name).is_ok())
                .unwrap_or_else(|| "CarryFS".into());
            self.erase_name = TextEdit::new(&name);
        }
        self.sheet = Some(Sheet {
            kind,
            phase: Phase::Ask,
            outcome: None,
            destination: None,
        });
    }
    /// Run `job` as the open sheet's task.
    fn run_sheet(
        &mut self,
        name: &str,
        cancellable: bool,
        tx: &mut Tx,
        job: impl FnOnce(Arc<operation::Control>) -> platform::Result<Update> + Send + 'static,
    ) {
        if self.pending.is_some() {
            return;
        }
        if let Some(sheet) = &mut self.sheet {
            sheet.phase = Phase::Running;
        }
        self.start(name, cancellable, tx, job);
    }
    fn open(&mut self, path: PathBuf, tx: &mut Tx) {
        if self.pending.is_some() {
            return;
        }
        let requested_target = path.display().to_string();
        self.tab = Tab::Overview;
        self.sheet = None;
        self.start("Open disk image", false, tx, move |_| {
            opened(Browser::open(path.clone()), Source { path, device: None })
        });
        self.target = requested_target;
    }
    fn select_device(&mut self, device: Device, tx: &mut Tx) {
        if self.pending.is_some() {
            return;
        }
        let requested_target = format!("{} · {}", device.name, device.id);
        if self.tab != Tab::Activity {
            self.tab = Tab::Overview;
        }
        self.sheet = None;
        let open = unlocked();
        self.start("Read disk information", false, tx, move |_| {
            shown(platform::details(&device)?, open)
        });
        self.target = requested_target;
    }
    fn discover(&mut self, tx: &mut Tx) {
        self.start("Discover disks", false, tx, |_| {
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
            && entry.item.kind == carryfs_core::ObjectKind::Directory
        {
            let mut trail = browser.trail.clone();
            trail.push((
                entry.item.object,
                String::from_utf8_lossy(entry.item.name.stored()).into_owned(),
            ));
            self.navigate(trail, tx);
        }
    }
    /// The selected volume's location: opened, damaged, or an unmounted
    /// CarryFS partition not yet opened.
    fn source(&self) -> Option<Source> {
        self.browser
            .as_ref()
            .map(Browser::source)
            .or_else(|| self.damaged.as_ref().map(|d| d.source.clone()))
            .or_else(|| {
                self.device
                    .as_ref()
                    .filter(|d| openable(d))
                    .map(|d| Source {
                        path: d.path(),
                        device: Some(d.clone()),
                    })
            })
    }
    fn first_aid(&mut self, tx: &mut Tx) {
        let Some(source) = self.source() else {
            return;
        };
        let browser = self.browser.clone();
        let unopened = browser.is_none() && self.damaged.is_none();
        self.run_sheet("First Aid", true, tx, move |cancel| {
            let plan = ScanResult::new(operation::fsck(source.clone(), Fsck::Plan, cancel.clone()));
            // A volume selected while locked had nowhere to keep the results;
            // fsck just authorized the device, so open it now.
            let (browser, opened) = match (browser, source.device.clone()) {
                (None, Some(device)) if unopened => {
                    match opened(Browser::open_device(device), source)? {
                        Update::Browser(b) => {
                            (Some((*b).clone()), Some(Box::new(Update::Browser(b))))
                        }
                        update => (None, Some(Box::new(update))),
                    }
                }
                (browser, _) => (browser, None),
            };
            let key = browser.as_ref().map(|b| (b.info.uuid, b.info.generation));
            // Data checksums are verified only when the structure is sound.
            let scrub = match browser {
                Some(browser) if plan.passed => {
                    Some(ScanResult::new(operation::inspect(browser, true, cancel)))
                }
                _ => None,
            };
            Ok(Update::FirstAid {
                opened,
                key,
                plan,
                scrub,
            })
        });
    }
    fn repair(&mut self, tx: &mut Tx) {
        let Some(source) = self.source() else {
            return;
        };
        self.run_sheet("Repair", false, tx, move |cancel| {
            let path = source.path.clone();
            let report = operation::fsck(source, Fsck::Repair, cancel)?;
            Ok(Update::Reopened(Box::new(Browser::open(path)?), report))
        });
    }
    fn upgrade(&mut self, tx: &mut Tx) {
        let Some(browser) = self.browser.clone() else {
            return;
        };
        self.run_sheet("Upgrade format", false, tx, move |cancel| {
            match browser.device.clone() {
                Some(device) => {
                    let report = platform::upgrade(&device)?;
                    Ok(Update::Reopened(
                        Box::new(Browser::open_device(device)?),
                        report,
                    ))
                }
                None => operation::upgrade(browser, cancel)
                    .map(|(b, report)| Update::Reopened(Box::new(b), report)),
            }
        });
    }
    fn erase(&mut self, tx: &mut Tx) {
        let Some(device) = self.device.clone() else {
            return;
        };
        let name = self.erase_name.text.trim().to_owned();
        self.browser = None;
        self.damaged = None;
        self.run_sheet("Erase", false, tx, move |_| {
            platform::erase(&device, &name).map(Update::Report)
        });
    }
    fn install(&mut self, version: String, tx: &mut Tx) {
        self.run_sheet("Install CarryFS", false, tx, move |_| {
            platform::install(&version)?;
            let mut versions = platform::versions();
            versions.latest = Some(version);
            Ok(Update::Versions(versions))
        });
    }
    fn toggle_lock(&mut self, tx: &mut Tx) {
        // Windows elevates the whole app: Unlock restarts it as administrator,
        // and only quitting gives that up.
        #[cfg(windows)]
        {
            if unlocked() {
                self.status = "CarryFS Manager runs as administrator until you quit it.".into();
                return;
            }
            let select = self.device.as_ref().map(|d| d.id.clone());
            self.start("Unlock", false, tx, move |_| {
                platform::windows::relaunch_elevated(select.as_deref())?;
                std::process::exit(0)
            });
        }
        #[cfg(not(windows))]
        self.unlock(tx);
    }
    #[cfg(not(windows))]
    fn unlock(&mut self, tx: &mut Tx) {
        if unlocked() {
            #[cfg(target_os = "macos")]
            privileged::lock();
            self.status = "Locked. Disk operations will ask for your password again.".into();
            return;
        }
        let device = self.device.clone().filter(openable);
        self.start("Unlock", false, tx, move |_| {
            #[cfg(target_os = "macos")]
            privileged::unlock()?;
            match device {
                Some(device) => opened(
                    Browser::open_device(device.clone()),
                    Source {
                        path: device.path(),
                        device: Some(device),
                    },
                ),
                None => Ok(Update::Report(
                    "Unlocked until you quit CarryFS Manager.".into(),
                )),
            }
        });
    }
    fn device_action(&mut self, action: DeviceAction, tx: &mut Tx) {
        if let Some(device) = self.device.clone() {
            self.browser = None;
            self.damaged = None;
            let name = match action {
                DeviceAction::Mount => "Mount",
                DeviceAction::Unmount => "Unmount",
                DeviceAction::Eject => "Eject",
            };
            let open = unlocked();
            let job = move |_| {
                platform::act(&device, action)?;
                if action == DeviceAction::Eject {
                    Ok(Update::Devices(platform::discover()?))
                } else {
                    shown(platform::details(&device)?, open)
                }
            };
            if action == DeviceAction::Eject {
                self.run_sheet(name, false, tx, job);
            } else {
                self.start(name, false, tx, job);
            }
        }
    }
}
/// An unmounted CarryFS partition the manager can open read-only.
fn openable(device: &Device) -> bool {
    device.carryfs() && !device.whole && device.mount.is_none()
}
/// The device as selected: once unlocked (`open`), an unmounted CarryFS
/// volume opens read-only right away.
fn shown(fresh: Device, open: bool) -> platform::Result<Update> {
    if open && openable(&fresh) {
        opened(
            Browser::open_device(fresh.clone()),
            Source {
                path: fresh.path(),
                device: Some(fresh),
            },
        )
    } else {
        Ok(Update::Device(Box::new(fresh)))
    }
}
fn unlocked() -> bool {
    #[cfg(target_os = "macos")]
    return privileged::unlocked();
    #[cfg(windows)]
    return platform::windows::elevated();
    #[cfg(not(any(target_os = "macos", windows)))]
    false
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
        let args: Vec<String> = std::env::args().collect();
        state.startup_device = args
            .windows(2)
            .find(|pair| pair[0] == "--select")
            .map(|pair| pair[1].clone())
            .filter(|id| platform::valid_id(id));
        state
    }
}
fn poll(s: &mut State, _: f64, tx: &mut Tx) -> bool {
    let background = poll_background(s, tx);
    let task = poll_task(s, tx);
    let scheduled = s.schedule_checks(tx);
    background || task || scheduled
}
fn poll_background(s: &mut State, tx: &mut Tx) -> bool {
    let Some(background) = &s.background else {
        return false;
    };
    match background.receiver.try_recv() {
        Ok(result) => {
            if let Some(background) = s.background.take() {
                s.finish_check(background, result);
            }
        }
        // Redraw for its progress.
        Err(mpsc::TryRecvError::Empty) => tx.wake_after(Duration::from_millis(250)),
        Err(mpsc::TryRecvError::Disconnected) => s.background = None,
    }
    true
}
fn poll_task(s: &mut State, tx: &mut Tx) -> bool {
    if s.pending.is_none() {
        if let Some(path) = s.startup_image.take() {
            s.open(path, tx);
            return true;
        }
        if !s.initialized || s.refresh {
            s.initialized = true;
            s.refresh = false;
            s.discover(tx);
            return true;
        }
        if let Some(id) = s.startup_device.take()
            && let Some(device) = s.devices.iter().find(|d| d.id == id).cloned()
        {
            s.select_device(device, tx);
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
            s.quiet_since = Instant::now();
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
    #[cfg(target_os = "macos")]
    {
        let args: Vec<String> = std::env::args().collect();
        if args.get(1).map(String::as_str) == Some(privileged::FLAG) {
            if let Err(error) = privileged::serve(&args[2..]) {
                eprintln!("carryfs-manager helper: {error}");
                std::process::exit(1);
            }
            return;
        }
    }
    let app = rune::App::new(AppSpec {
        min_size: Some((960.0, 640.0)),
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
            ("first-aid", |s: &mut State| {
                capture_sources(s);
                s.ask(SheetKind::FirstAid);
            }),
            ("erase", |s: &mut State| {
                capture_sources(s);
                s.ask(SheetKind::Erase);
            }),
        ],
        poll: Some(poll),
        ..AppSpec::new("CarryFS Manager", (1180.0, 780.0), State::default, view)
    });
    #[cfg(any(target_os = "macos", windows))]
    {
        let handle = app.handle();
        let watched = platform::watch(move || {
            handle.update(|s, tx| {
                if s.pending.is_none() {
                    s.discover(tx);
                } else {
                    s.refresh = true;
                }
            })
        });
        if let Err(error) = watched {
            eprintln!("CarryFS Manager: cannot watch disks, use Refresh: {error}");
        }
    }
    app.run();
}
#[cfg(test)]
mod tests;
