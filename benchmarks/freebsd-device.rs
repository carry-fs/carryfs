//! Release microbenchmark of the production Disk adapter on a disposable md device.
//! Compile with rustc and the workspace's release rlibs.
//! Writes stay held and are dropped without publication. Never use a physical device.

#[path = "../fuse/src/device.rs"]
mod device;

use carryfs_core::{BlockDevice, MountMode};
use std::{hint::black_box, path::Path, time::Instant};

fn measure(label: &str, iterations: usize, mut operation: impl FnMut()) {
    for _ in 0..100 {
        operation();
    }
    let mut samples = Vec::new();
    for _ in 0..7 {
        let start = Instant::now();
        for _ in 0..iterations {
            operation();
        }
        samples.push(start.elapsed().as_nanos() as f64 / iterations as f64);
    }
    println!("{label}\t{samples:?}\tns/op");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("expected disposable /dev/mdN")?;
    let number = path.strip_prefix("/dev/md").ok_or("expected /dev/mdN")?;
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("expected /dev/mdN".into());
    }
    let mut disk = device::Disk::open(Path::new(&path), MountMode::ReadWrite)?;
    let data = [0x5a; 4096];
    let mut out = [0; 4096];
    measure("copy_4096", 20_000, || {
        out.copy_from_slice(black_box(&data));
        black_box(&out);
    });
    measure("device_4096", 2_000, || {
        disk.read_exact_at(black_box(4096), &mut out).unwrap();
        black_box(&out);
    });
    let mut following = [0; 4096];
    disk.read_exact_at(8192, &mut following)?;
    disk.write_all_at(4096, &data)?;
    measure("held_4096", 2_000, || {
        disk.read_exact_at(black_box(4096), &mut out).unwrap();
        black_box(&out);
    });
    assert_eq!(out, data);
    let mut partial = [0; 8192];
    measure("partial_8192", 2_000, || {
        disk.read_exact_at(black_box(4096), &mut partial).unwrap();
        black_box(&partial);
    });
    assert_eq!(partial[..4096], data);
    assert_eq!(partial[4096..], following);
    Ok(())
}
