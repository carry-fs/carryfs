"""Build both isolated macOS benchmark variants without network access."""
import os
from pathlib import Path
import subprocess
ROOT=Path(__file__).resolve().parent
cargo=Path('/Users/painhardcore/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin/cargo')
env=dict(os.environ,CARGO_HOME=str(ROOT.parent/'cargo-home'),RUSTC=str(cargo.with_name('rustc')))
for variant in ['std','bb']:
    subprocess.run([str(cargo),'build','--offline','--release','--manifest-path',str(ROOT/'Cargo.toml'),'--target-dir',str(ROOT/f'target-{variant}'),'--no-default-features','--features',variant+'map','--bins'],check=True,env=env)
