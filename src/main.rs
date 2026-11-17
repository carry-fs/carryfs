mod cli;

use std::{env, ffi::OsString, path::Path, process::ExitCode};

use carryfs_core::{ImageDevice, Volume};

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<_> = env::args_os().skip(1).collect();
    // `scrub --progress` reports verified bytes on stderr for a front end.
    let progress = args.len() > 2 && args[0] == "scrub" && args[1] == "--progress";
    if progress {
        args.remove(1);
    }
    // `--bytes N` after a read-only command opens a raw device of N bytes,
    // whose length macOS does not report through the file system.
    let device_bytes = match args.as_slice() {
        [command, flag, bytes, ..]
            if flag == "--bytes"
                && ["dump", "check", "scrub", "get"]
                    .iter()
                    .any(|c| command == c) =>
        {
            let bytes: u64 = bytes
                .to_str()
                .ok_or("device size must be a byte count")?
                .parse()?;
            args.drain(1..3);
            Some(bytes)
        }
        _ => None,
    };
    match args.as_slice() {
        [command, path, bytes] if command == "mkfs" => mkfs(path, bytes)?,
        [command, path] if command == "dump" => {
            let device = cli::open_read_only(Path::new(path), device_bytes)?;
            let mut volume = Volume::open(device)?;
            let info = volume.info()?;
            println!(
                "Format revision: {}\nVolume name: {}\nGeneration: {}\nImage bytes: {}\nRecorded free blocks: {}",
                info.revision, info.name(), info.generation, info.byte_len, info.free_blocks
            );
            println!(
                "Root records and tree-root pages validated; run check for full structural validation."
            );
            volume.eject()?;
        }
        [command, path] if command == "check" => {
            let device = cli::open_read_only(Path::new(path), device_bytes)?;
            let mut volume = Volume::open(device)?;
            let summary = volume.check()?;
            volume.eject()?;
            println!(
                "Structural check passed: {} objects, {} metadata pages, {} data blocks, {} free blocks",
                summary.objects, summary.metadata_pages, summary.data_blocks, summary.free_blocks
            );
            println!("File data checksums were not scrubbed.");
        }
        [command, image] if command == "scrub" => {
            cli::scrub(Path::new(image), device_bytes, progress)?
        }
        [command, image] if command == "upgrade" => cli::upgrade(Path::new(image))?,
        [command, image, path, changes @ ..] if command == "attr" => {
            cli::attr(Path::new(image), path, changes)?
        }
        [command, image, source, destination] if command == "put" => {
            cli::put(Path::new(image), Path::new(source), destination)?;
        }
        [command, image, source, destination] if command == "get" => {
            cli::get(Path::new(image), device_bytes, source, Path::new(destination))?;
        }
        _ => return Err(
            "usage: carryfs fsck [OPTIONS] IMAGE | carryfs mkfs NEW_IMAGE BYTE_COUNT | carryfs dump IMAGE | carryfs check IMAGE | carryfs scrub [--progress] IMAGE | carryfs upgrade IMAGE | carryfs attr IMAGE /PATH [mode=OCTAL] [hidden=yes|no] | carryfs put IMAGE HOST_SOURCE /DESTINATION | carryfs get IMAGE /SOURCE HOST_DESTINATION; dump, check, scrub and get accept --bytes N before IMAGE to read a raw device of N bytes"
                .into(),
        ),
    }
    Ok(())
}

fn mkfs(path: &OsString, bytes: &OsString) -> Result<(), Box<dyn std::error::Error>> {
    let bytes: u64 = bytes
        .to_str()
        .ok_or("image size must be a byte count")?
        .parse()?;
    let device = ImageDevice::create_new(Path::new(path), bytes)?;
    let mut volume = Volume::format(device)?;
    let info = volume.info()?;
    volume.eject()?;
    println!(
        "Created {} byte CarryFS image, generation {}",
        info.byte_len, info.generation
    );
    Ok(())
}

fn main() -> ExitCode {
    let mut args = env::args_os();
    let executable = args.next().unwrap_or_default();
    let mut args: Vec<_> = args.collect();
    if Path::new(&executable)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            matches!(
                name,
                "fsck.carryfs" | "fsck-carryfs" | "fsck.carryfs.exe" | "fsck-carryfs.exe"
            )
        })
    {
        return cli::fsck::main(args);
    }
    if args.first().is_some_and(|arg| arg == "--version") {
        println!("{}", carryfs_core::version_line("carryfs"));
        return ExitCode::SUCCESS;
    }
    if args.first().is_some_and(|arg| arg == "fsck") {
        args.remove(0);
        return cli::fsck::main(args);
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("carryfs: {error}");
            ExitCode::FAILURE
        }
    }
}
