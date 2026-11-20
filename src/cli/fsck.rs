//! Offline diagnosis, repair plans, in-place repair, and source-preserving export.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use carryfs_core::{BlockDevice, ImageDevice, MountMode, Repair};

use super::Result;

const USAGE: &str = "usage: fsck.carryfs [--plan|-n | --repair [-y] [--output NEW_IMAGE [--verify-data]] [--accept-data-loss] | --recover NEW_DIRECTORY] [--report NEW_FILE] [--bytes N] IMAGE_OR_DEVICE\nDiagnosis is read-only by default. --repair --output rebuilds a new image with an embedded JSON manifest and JSON sidecar. Unmount first.";

#[derive(Default)]
struct Options {
    source: PathBuf,
    repair: bool,
    accept_loss: bool,
    recover: Option<PathBuf>,
    output: Option<PathBuf>,
    verify_data: bool,
    report: Option<PathBuf>,
    bytes: Option<u64>,
}

impl Options {
    fn parse(args: Vec<OsString>) -> Result<Self> {
        let mut options = Self::default();
        let mut source = None;
        let mut readonly = false;
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--plan" | "--readonly" | "-n") => readonly = true,
                Some("--repair" | "-y") => options.repair = true,
                Some("--accept-data-loss") => options.accept_loss = true,
                Some("--verify-data") => options.verify_data = true,
                Some("--recover") => {
                    options.recover =
                        Some(args.next().ok_or("--recover needs a new directory")?.into())
                }
                Some("--report") => {
                    options.report = Some(args.next().ok_or("--report needs a new file")?.into())
                }
                Some("--output") => {
                    options.output = Some(args.next().ok_or("--output needs a new image")?.into())
                }
                Some("--bytes") => {
                    options.bytes = Some(
                        args.next()
                            .ok_or("--bytes needs a byte count")?
                            .to_str()
                            .ok_or("invalid byte count")?
                            .parse()?,
                    )
                }
                Some("--") => {
                    if source.is_some() {
                        return Err(USAGE.into());
                    }
                    source = args.next();
                    if args.next().is_some() {
                        return Err(USAGE.into());
                    }
                    break;
                }
                Some(flag) if flag.starts_with('-') => {
                    return Err(format!("unknown option: {flag}\n{USAGE}").into());
                }
                _ if source.is_none() => source = Some(arg),
                _ => return Err(USAGE.into()),
            }
        }
        options.source = source.ok_or(USAGE)?.into();
        if options.bytes.is_some()
            && options.repair
            && options.output.is_none()
            && options.report.is_none()
        {
            return Err(
                "raw-device repair requires --report NEW_FILE on another filesystem".into(),
            );
        }
        if (options.repair && (readonly || options.recover.is_some()))
            || (options.accept_loss && !options.repair)
            || (options.output.is_some() && !options.repair)
            || (options.verify_data && options.output.is_none())
        {
            return Err("--repair conflicts with read-only diagnosis and --recover; --output and --accept-data-loss require --repair; --verify-data requires --output".into());
        }
        Ok(options)
    }
}

pub(crate) fn main(args: Vec<OsString>) -> ExitCode {
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let options = match Options::parse(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("fsck.carryfs: {error}");
            return ExitCode::from(16);
        }
    };
    match run(options) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            let code = match error.downcast_ref::<carryfs_core::Error>() {
                Some(carryfs_core::Error::Corrupt(_) | carryfs_core::Error::Corruption { .. }) => 4,
                _ => 8,
            };
            eprintln!("fsck.carryfs: {error}");
            ExitCode::from(code)
        }
    }
}

fn run(options: Options) -> Result<u8> {
    let mode = if options.repair && options.output.is_none() {
        MountMode::ReadWrite
    } else {
        MountMode::ReadOnly
    };
    let mut device = if let Some(bytes) = options.bytes {
        ImageDevice::from_exclusive_device_file(
            super::open_exclusive(&options.source, mode)?,
            bytes,
            mode,
        )?
    } else if mode == MountMode::ReadWrite {
        super::open_writable(&options.source)?
    } else {
        super::open_read_only(&options.source, None)?
    };
    let image_bytes = device.geometry()?.byte_len;
    let mut repair = Repair::diagnose(device)?;
    let plan = repair.plan();
    let mut text = format!(
        "Source: {:?}\nRepair plan: {} objects; clean: {}; possible data loss: {}\n",
        options.source, plan.objects, plan.clean, plan.data_loss
    );
    for action in &plan.actions {
        text.push_str(&format!("- {action}\n"));
    }
    if let Some(reason) = &plan.blocked {
        text.push_str(&format!("In-place repair blocked: {reason}\n"));
    }
    text.push_str("File data checksums have not been scrubbed.\n");
    io::stdout().write_all(text.as_bytes())?;
    if let Some(output) = options.output {
        if !plan.recovery_available {
            eprintln!("fsck.carryfs: a validated recovery forest is required; no image created");
            return Ok(4);
        }
        let path = options
            .report
            .unwrap_or_else(|| suffix(&output, ".recovery.txt"));
        text.push_str(&format!("Rebuild destination: {output:?}; {image_bytes} bytes\nImage recovery starting; a missing completion entry means the output is partial.\n"));
        let mut report = new_report(&path, &text)?;
        let json_path = suffix(&output, ".recovery.json");
        let mut json_report =
            new_report(&json_path, "{\"version\":1,\"state\":\"in_progress\"}\n")?;
        let destination = ImageDevice::create_new(&output, image_bytes)?;
        let mut report_error = Ok(());
        let recovery = repair.recover_image_with_verification(
            destination,
            options.verify_data,
            &mut |finding| {
                if report_error.is_ok() {
                    report_error = record_finding(&mut report, &finding);
                }
                super::report(finding);
            },
        )?;
        report_error?;
        json_report.set_len(0)?;
        use std::io::{Seek, SeekFrom};
        json_report.seek(SeekFrom::Start(0))?;
        json_report.write_all(recovery.manifest_json.as_bytes())?;
        json_report.sync_all()?;
        let summary = recovery.summary;
        writeln!(
            report,
            "Image recovery finished; structural verification passed. Summary: {summary:?}"
        )?;
        writeln!(
            report,
            "Embedded manifest: {:?}; JSON sidecar: {json_path:?}; destination data verification: {}",
            String::from_utf8_lossy(recovery.manifest_name.stored()),
            if recovery.verification_failed {
                "FAILED"
            } else if options.verify_data {
                "passed"
            } else {
                "not requested"
            }
        )?;
        report.sync_all()?;
        println!(
            "Recovered image: {}. Report: {}",
            output.display(),
            path.display()
        );
        return Ok(if recovery.verification_failed {
            8
        } else if summary.complete && summary.unreadable_ranges == 0 {
            1
        } else {
            4
        });
    }
    if let Some(output) = options.recover {
        let path = options
            .report
            .unwrap_or_else(|| suffix(&output, ".recovery.txt"));
        let mut report = new_report(&path, &text)?;
        let summary = recover(&mut repair, &output, &mut report)?;
        writeln!(report, "Recovery summary: {summary:?}")?;
        report.sync_all()?;
        println!("Recovery report: {}", path.display());
        return Ok(if summary.complete && summary.unreadable_ranges == 0 {
            0
        } else {
            4
        });
    }
    if options.repair && !plan.clean {
        if plan.blocked.is_some() || (plan.data_loss && !options.accept_loss) {
            eprintln!(
                "fsck.carryfs: no changes made; recover files first or review the blocked plan{}",
                if plan.data_loss && !options.accept_loss {
                    "; --accept-data-loss is required to discard the reported inaccessible metadata or mappings"
                } else {
                    ""
                }
            );
            return Ok(4);
        }
        let path = options
            .report
            .unwrap_or_else(|| suffix(&options.source, ".repair.txt"));
        let mut report = new_report(&path, &text)?;
        writeln!(
            report,
            "Repair starting; interruption requires a fresh diagnosis."
        )?;
        report.sync_all()?;
        repair.apply(options.accept_loss)?;
        writeln!(
            report,
            "Repair committed and structural verification passed."
        )?;
        report.sync_all()?;
        println!(
            "Repair committed and structural verification passed. Report: {}",
            path.display()
        );
        return Ok(1);
    }
    if let Some(path) = options.report {
        new_report(&path, &text)?;
    }
    Ok(if plan.clean { 0 } else { 4 })
}

fn suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut path = path.as_os_str().to_owned();
    path.push(suffix);
    path.into()
}

fn new_report(path: &Path, text: &str) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| {
        format!(
            "create report {}: {error}; choose a new --report path",
            path.display()
        )
    })?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        OpenOptions::new()
            .write(true)
            .custom_flags(0x0200_0000)
            .open(parent)?
            .sync_all()?;
    }
    #[cfg(not(any(unix, windows)))]
    return Err("durable report directory synchronization is unsupported on this platform".into());
    Ok(file)
}

fn record_finding(report: &mut File, finding: &carryfs_core::Finding<'_>) -> io::Result<()> {
    let path: Vec<_> = finding
        .path
        .iter()
        .map(|name| String::from_utf8_lossy(name.stored()))
        .collect();
    writeln!(
        report,
        "Unreadable object {:?}, path {:?}, offset {:?}, length {}: {}",
        finding.object, path, finding.offset, finding.length, finding.error
    )?;
    report.sync_all()
}

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd"
))]
fn recover(
    repair: &mut Repair<ImageDevice>,
    output: &Path,
    report: &mut File,
) -> Result<carryfs_core::ExtractionSummary> {
    let mut sink = super::host::Sink::new(output)?;
    let mut report_error = Ok(());
    let summary = repair.recover_to(&mut sink, &mut |finding| {
        if report_error.is_ok() {
            report_error = record_finding(report, &finding);
        }
        super::report(finding);
    })?;
    report_error?;
    sink.finish()?;
    Ok(summary)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd"
)))]
fn recover(
    _: &mut Repair<ImageDevice>,
    _: &Path,
    _: &mut File,
) -> Result<carryfs_core::ExtractionSummary> {
    Err("secure recovery export requires Linux, macOS, FreeBSD, or NetBSD".into())
}
