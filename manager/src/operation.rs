use crate::{
    model::{Browser, Source},
    platform::Result,
};
use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

/// The bundled CLI, with no input unless a device is attached.
fn cli() -> Result<Command> {
    let mut command = Command::new(companion_for(&std::env::current_exe()?)?);
    command.stdin(Stdio::null());
    Ok(command)
}

/// Read `device` through a descriptor the administrator helper opened: the
/// CLI receives it as standard input and reads `/dev/fd/0`.
fn device_source(command: &mut Command, device: &crate::platform::Device) -> Result<()> {
    command.stdin(crate::platform::device_file(device)?).args([
        "--bytes",
        &device.bytes.to_string(),
        "/dev/fd/0",
    ]);
    Ok(())
}

fn companion_for(exe: &Path) -> Result<PathBuf> {
    let exe = exe.canonicalize()?;
    let mut directory = exe.parent().ok_or("Cannot locate the CarryFS tools")?;
    if directory.file_name().is_some_and(|name| name == "deps") {
        directory = directory
            .parent()
            .ok_or("Cannot locate the CarryFS tools")?;
    }
    let path = directory.join(if cfg!(windows) {
        "carryfs.exe"
    } else {
        "carryfs"
    });
    if !path.is_file() {
        return Err("The companion CarryFS tool is missing. Rebuild the app with manager/scripts/bundle-macos.sh, or reinstall the complete app.".into());
    }
    Ok(path)
}

/// The stream's last 256 KiB. With `control`, its `progress N` lines go there instead.
fn drain(stream: impl Read, control: Option<&Control>) -> std::io::Result<String> {
    const LIMIT: usize = 256 * 1024;
    let mut tail = Vec::new();
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        let progress = control.zip(
            line.strip_prefix(b"progress ")
                .and_then(|n| std::str::from_utf8(n).ok()?.trim().parse().ok()),
        );
        match progress {
            Some((control, bytes)) => control.progress.store(bytes, Ordering::Relaxed),
            None => tail.extend_from_slice(&line),
        }
        line.clear();
        if tail.len() > LIMIT {
            tail.drain(..tail.len() - LIMIT);
        }
    }
    Ok(String::from_utf8_lossy(&tail).into_owned())
}

#[derive(Default)]
pub struct Control {
    cancelled: AtomicBool,
    child: Mutex<Option<Child>>,
    progress: AtomicU64,
}
impl Control {
    /// File data bytes verified so far by a running scrub.
    pub fn progress(&self) -> u64 {
        self.progress.load(Ordering::Relaxed)
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
    pub fn stop(&self) -> Result<()> {
        self.cancel();
        let mut slot = self
            .child
            .lock()
            .map_err(|_| "Cannot access the active process")?;
        if let Some(child) = slot.as_mut()
            && child.try_wait()?.is_none()
        {
            #[cfg(unix)]
            {
                let status = Command::new("/bin/kill")
                    .args(["-KILL", "--", &format!("-{}", child.id())])
                    .status()?;
                if !status.success() {
                    child.kill()?;
                }
            }
            #[cfg(not(unix))]
            child.kill()?;
            child.wait()?;
        }
        Ok(())
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("CarryFS Manager: release operation process: {error}");
        }
    }
}

pub fn run(command: Command, control: Arc<Control>) -> Result<String> {
    let (status, report) = run_status(command, control)?;
    if !status.success() {
        return Err(if report.is_empty() {
            format!("Operation exited with {status}")
        } else {
            report
        }
        .into());
    }
    Ok(report)
}

/// Run `command` and return its exit status with its combined output.
fn run_status(mut command: Command, control: Arc<Control>) -> Result<(ExitStatus, String)> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let (stdout, stderr) = {
        // Publication shares the lock with stop: closing the app cannot race an unpublished child.
        let mut slot = control
            .child
            .lock()
            .map_err(|_| "Cannot start the active process")?;
        if control.cancelled.load(Ordering::Relaxed) {
            return Err("Cancelled before starting.".into());
        }
        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or("Cannot capture operation output")?;
        let stderr = child
            .stderr
            .take()
            .ok_or("Cannot capture operation errors")?;
        *slot = Some(child);
        (stdout, stderr)
    };
    let out = std::thread::spawn(move || drain(stdout, None));
    let err_control = control.clone();
    let err = std::thread::spawn(move || drain(stderr, Some(&err_control)));
    let status = loop {
        if control.cancelled.load(Ordering::Relaxed) {
            control.stop()?;
        }
        let status = control
            .child
            .lock()
            .map_err(|_| "Cannot poll the active process")?
            .as_mut()
            .ok_or("The active process disappeared")?
            .try_wait()?;
        if let Some(status) = status {
            break status;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    control
        .child
        .lock()
        .map_err(|_| "Cannot release the active process")?
        .take();
    let stdout = out
        .join()
        .map_err(|_| "Operation output reader stopped")??;
    let stderr = err.join().map_err(|_| "Operation error reader stopped")??;
    if control.cancelled.load(Ordering::Relaxed) && !status.success() {
        return Err("Cancelled. The source was not modified. Recovery may have left incomplete files at the destination.".into());
    }
    Ok((status, format!("{stdout}\n{stderr}").trim().to_owned()))
}

pub fn inspect(browser: Browser, scrub: bool, cancel: Arc<Control>) -> Result<String> {
    // Validate the selected snapshot, then release its lease before the CLI opens it.
    drop(browser.volume()?);
    let mut command = cli()?;
    command.args(if scrub {
        &["scrub", "--progress"][..]
    } else {
        &["check"]
    });
    match &browser.device {
        Some(device) => device_source(&mut command, device)?,
        None => drop(command.arg(&browser.path)),
    }
    run(command, cancel)
}

/// Let an image store permissions and the hidden attribute, then reopen it.
pub fn upgrade(browser: Browser, cancel: Arc<Control>) -> Result<(Browser, String)> {
    if browser.device.is_some() {
        return Err("Upgrade a device through its Terminal review.".into());
    }
    drop(browser.volume()?);
    let mut command = cli()?;
    command.arg("upgrade").arg(&browser.path);
    let report = run(command, cancel)?;
    Ok((Browser::open(browser.path)?, report))
}

/// What `carryfs fsck` does with the selected source.
pub enum Fsck {
    /// Diagnose and print the repair plan; read-only.
    Plan,
    /// Export accessible files to a new directory; read-only.
    Recover(PathBuf),
    /// Rebuild accessible files into a new verified image; read-only.
    Rebuild(PathBuf),
    /// Repair an image in place. The tool refuses blocked or lossy plans.
    Repair,
}

/// Run `carryfs fsck`. Its exit status separates clean (0), changed or
/// rebuilt (1), damaged or partial (4), and failed (8) results.
pub fn fsck(source: Source, mode: Fsck, cancel: Arc<Control>) -> Result<String> {
    // Damaged volumes may not open, so recheck only the device's identity.
    if let Some(device) = &source.device
        && crate::platform::details(device)?.mount.is_some()
    {
        return Err("Unmount the volume before diagnosis or recovery.".into());
    }
    let mut command = cli()?;
    command.arg("fsck");
    let success = match &mode {
        Fsck::Plan => {
            command.arg("--plan");
            0
        }
        Fsck::Recover(destination) => {
            command.arg("--recover").arg(new_path(destination)?);
            0
        }
        Fsck::Rebuild(destination) => {
            command
                .args(["--repair", "--verify-data", "--output"])
                .arg(new_path(destination)?);
            1
        }
        Fsck::Repair if source.device.is_none() => {
            // The tool refuses an existing report; a later repair needs a new name.
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            let mut report = source.path.clone().into_os_string();
            report.push(format!(".repair-{stamp}.txt"));
            command.arg("--repair").arg("--report").arg(report);
            1
        }
        Fsck::Repair => return Err("Repair a device by rebuilding it into a new image.".into()),
    };
    match &source.device {
        Some(device) => device_source(&mut command, device)?,
        None => drop(command.arg("--").arg(&source.path)),
    }
    let (status, report) = run_status(command, cancel)?;
    // Lead with the outcome: the tool prints the source path first.
    let report = report
        .lines()
        .filter(|line| !line.starts_with("Source: "))
        .collect::<Vec<_>>()
        .join("\n");
    let report = match &mode {
        Fsck::Plan => report,
        _ => {
            let outcome = report.lines().next_back().unwrap_or("").to_owned();
            format!("{outcome}\n{report}")
        }
    };
    if status.code() == Some(success) {
        Ok(report)
    } else {
        Err(report.into())
    }
}

/// `destination` with its parent resolved once, so the tool opens a new name
/// in the folder the user chose.
fn new_path(destination: &Path) -> Result<PathBuf> {
    let parent = std::fs::canonicalize(
        destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    Ok(parent.join(
        destination
            .file_name()
            .ok_or("Choose a new destination name")?,
    ))
}

pub fn recover(browser: Browser, destination: &Path, cancel: Arc<Control>) -> Result<String> {
    drop(browser.volume()?);
    let source = if browser.trail.len() == 1 {
        "/".to_owned()
    } else {
        format!(
            "/{}",
            browser.trail[1..]
                .iter()
                .map(|(_, name)| name.as_str())
                .collect::<Vec<_>>()
                .join("/")
        )
    };
    let mut command = cli()?;
    command.arg("get");
    match &browser.device {
        Some(device) => device_source(&mut command, device)?,
        None => drop(command.arg(&browser.path)),
    }
    // The CLI then opens every component without following links.
    command.arg(source).arg(new_path(destination)?);
    run(command, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn progress_lines_leave_the_report() {
        let control = Control::default();
        let report = drain(
            &b"progress 5\nScrub: 1 files\nprogress 9\n"[..],
            Some(&control),
        )
        .unwrap();
        assert_eq!(report, "Scrub: 1 files\n");
        assert_eq!(control.progress(), 9);
        assert_eq!(drain(&b"progress 5\n"[..], None).unwrap(), "progress 5\n");
    }
    #[cfg(unix)]
    #[test]
    fn cancellation_stops_a_read_only_child_and_reports_incomplete_work() {
        let control = Arc::new(Control::default());
        let worker_control = control.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut command = Command::new("/bin/sleep");
            command.arg("30");
            sender
                .send(run(command, worker_control).map_err(|e| e.to_string()))
                .unwrap();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while control.child.lock().unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "Child did not start");
            std::thread::sleep(Duration::from_millis(1));
        }
        control.cancel();
        let error = receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err();
        assert!(error.starts_with("Cancelled."), "{error}");
    }
}

#[cfg(all(test, unix))]
mod companion_tests {
    use super::*;
    #[test]
    fn companion_resolves_development_symlinks_and_packaged_binaries() {
        let directory =
            std::env::temp_dir().join(format!("carryfs-companion-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let directory = directory.canonicalize().unwrap();
        let build = directory.join("debug");
        let bundle = directory.join("MacOS");
        std::fs::create_dir(&build).unwrap();
        std::fs::create_dir(&bundle).unwrap();
        std::fs::write(build.join("carryfs-manager"), "").unwrap();
        std::fs::write(build.join("carryfs"), "").unwrap();
        let exe = bundle.join("carryfs-manager");
        std::os::unix::fs::symlink(build.join("carryfs-manager"), &exe).unwrap();
        assert_eq!(companion_for(&exe).unwrap(), build.join("carryfs"));
        std::fs::remove_file(&exe).unwrap();
        std::fs::write(&exe, "").unwrap();
        assert!(companion_for(&exe).is_err());
        std::fs::write(bundle.join("carryfs"), "").unwrap();
        assert_eq!(companion_for(&exe).unwrap(), bundle.join("carryfs"));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
