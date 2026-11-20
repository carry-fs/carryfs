use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=src/netbsd-perfuse.c");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("netbsd") {
        return;
    }
    let library = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("libcarryfs-perfuse.so");
    let status = Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
        .args([
            "-std=c11", "-Wall", "-Wextra", "-Werror", "-fPIC", "-shared",
        ])
        .arg("src/netbsd-perfuse.c")
        .arg("-o")
        .arg(library)
        .status()
        .expect("compile NetBSD relay compatibility library");
    assert!(
        status.success(),
        "NetBSD relay compatibility compilation failed"
    );
}
