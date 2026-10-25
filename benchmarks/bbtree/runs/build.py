import os
from pathlib import Path
import shutil
import subprocess
root = Path(__file__).resolve().parent
cargo = Path('/Users/painhardcore/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin/cargo')
env = dict(os.environ, CARGO_HOME=str(root/'cargo-home'), RUSTC=str(cargo.with_name('rustc')))
for variant, feature in [('std','stdmap'), ('bb','bbmap')]:
    subprocess.run([str(cargo), 'build', '--offline', '--release', '--manifest-path', str(root/'harness/Cargo.toml'), '--target-dir', str(root/f'target-{variant}'), '--no-default-features', '--features', feature, '--bins'], env=env, check=True)
