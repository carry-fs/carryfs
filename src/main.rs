mod cli;

use std::{env, path::Path, process::ExitCode};

use carryfs_core::{ImageDevice, Volume};

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<_> = env::args_os().skip(1).collect();
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
        [command, path, bytes] if command == "mkfs" => {
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
        }
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
        [command, image] if command == "scrub" => cli::scrub(Path::new(image), device_bytes)?,
        [command, image, source, destination] if command == "put" => {
            cli::put(Path::new(image), Path::new(source), destination)?;
        }
        [command, image, source, destination] if command == "get" => {
            cli::get(Path::new(image), device_bytes, source, Path::new(destination))?;
        }
        _ => return Err(
            "usage: carryfs mkfs NEW_IMAGE BYTE_COUNT | carryfs dump IMAGE | carryfs check IMAGE | carryfs scrub IMAGE | carryfs put IMAGE HOST_SOURCE /DESTINATION | carryfs get IMAGE /SOURCE HOST_DESTINATION; dump, check, scrub and get accept --bytes N before IMAGE to read a raw device of N bytes"
                .into(),
        ),
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("carryfs: {error}");
            ExitCode::FAILURE
        }
    }
}
