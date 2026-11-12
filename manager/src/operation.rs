use crate::{model::Browser, platform::Result};
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

fn companion() -> Result<PathBuf> {
    companion_for(&std::env::current_exe()?)
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

fn drain(mut stream: impl Read) -> std::io::Result<String> {
    const LIMIT: usize = 256 * 1024;
    let mut tail = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let n = stream.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        tail.extend_from_slice(&buffer[..n]);
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
}
impl Control {
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

pub fn run(mut command: Command, control: Arc<Control>) -> Result<String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
    let out = std::thread::spawn(move || drain(stdout));
    let err = std::thread::spawn(move || drain(stderr));
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
    let report = format!("{stdout}\n{stderr}").trim().to_owned();
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

pub fn inspect(browser: Browser, scrub: bool, cancel: Arc<Control>) -> Result<String> {
    // Validate the selected snapshot, then release its lease before the CLI opens it.
    drop(browser.volume()?);
    let mut command = Command::new(companion()?);
    command.arg(if scrub { "scrub" } else { "check" });
    if let Some(device) = &browser.device {
        command.args(["--bytes", &device.bytes.to_string()]);
    }
    command.arg(&browser.path);
    run(command, cancel)
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
    let mut command = Command::new(companion()?);
    command.arg("get");
    if let Some(device) = &browser.device {
        command.args(["--bytes", &device.bytes.to_string()]);
    }
    // Resolve the user's chosen parent once; the CLI then opens every component without following links.
    let parent = std::fs::canonicalize(
        destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    let destination = parent.join(
        destination
            .file_name()
            .ok_or("Choose a new destination name")?,
    );
    command.arg(&browser.path).arg(source).arg(destination);
    run(command, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
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
