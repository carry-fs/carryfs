#!/usr/bin/env python3
"""Restore and check the saved experiment under the repository's build directory."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
WORKSPACE = REPO / 'build/bbtree-fs-evaluation'
ARCHIVE = HERE / 'archive/experiment.tar.gz'


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def prepare():
    expected = (HERE / 'archive/SHA256SUMS').read_text().split()[0]
    if sha256(ARCHIVE) != expected:
        raise SystemExit('The experiment archive does not match SHA256SUMS.')
    marker = WORKSPACE / '.archive-sha256'
    if WORKSPACE.exists():
        if not marker.is_file() or marker.read_text().strip() != expected:
            raise SystemExit(f'Refusing to overwrite an unrelated workspace: {WORKSPACE}')
        return WORKSPACE
    if not hasattr(tarfile, 'data_filter'):
        raise SystemExit('Archive restoration requires Python 3.11.8+ or Python 3.12+.')
    WORKSPACE.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='.bbtree-restore-', dir=WORKSPACE.parent) as stage:
        with tarfile.open(ARCHIVE) as archive:
            if any(Path(m.name).parts[0] != 'carryfs-bbtree-benchmark' for m in archive.getmembers()):
                raise SystemExit('Unexpected top-level path in the experiment archive.')
            archive.extractall(stage, filter='data')
        restored = Path(stage) / 'carryfs-bbtree-benchmark'
        (restored / marker.name).write_text(expected + '\n')
        restored.rename(WORKSPACE)
    return WORKSPACE


def verify(workspace):
    manifest = json.loads((HERE / 'preservation-manifest.json').read_text())
    for name, expected in manifest['files'].items():
        if sha256(HERE / name) != expected:
            raise SystemExit(f'The saved record has changed: {name}')
    subprocess.run([sys.executable, str(workspace / 'size-sweep/verify.py')], check=True)
    print(f"Verified {len(manifest['files'])} readable records and archive integrity.")


def build(workspace):
    # Resolve the installed toolchain before changing CARGO_HOME for the offline cache.
    tool = lambda name: subprocess.check_output(
        ['rustup', 'which', '--toolchain', '1.99.0', name], text=True
    ).strip()
    cargo, rustc = tool('cargo'), tool('rustc')
    env = dict(os.environ, CARGO_HOME=str(workspace / 'cargo-home'), RUSTC=rustc)
    for suite, manifest in [(workspace, workspace / 'harness/Cargo.toml'),
                            (workspace / 'size-sweep', workspace / 'size-sweep/Cargo.toml')]:
        for variant in ['std', 'bb']:
            subprocess.run([
                cargo, 'build', '--offline', '--locked', '--release',
                '--manifest-path', str(manifest), '--target-dir', str(suite / f'target-{variant}'),
                '--no-default-features', '--features', variant + 'map', '--bins',
            ], env=env, check=True)


def smoke(workspace):
    if sys.platform != 'darwin':
        raise SystemExit('The CPU-time scaling harness targets macOS.')
    checksums = []
    for variant in ['std', 'bb']:
        core = workspace / f'target-{variant}/release/core'
        maps = workspace / f'size-sweep/target-{variant}/release/maps'
        if not core.is_file() or not maps.is_file():
            raise SystemExit('Build the restored binaries before the smoke check.')
        subprocess.run([str(core), 'create', '16', 'true'], check=True)
        output = subprocess.check_output(
            [str(maps), 'data_index', '256', 'ordered', '100'], text=True
        )
        checksums.append(output.strip().split('\t')[5])
    if checksums[0] != checksums[1]:
        raise SystemExit('The map variants produced different checksums.')
    print('Both restored variants passed the core and map smoke checks.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['prepare', 'verify', 'build', 'smoke'])
    args = parser.parse_args()
    workspace = prepare()
    if args.command == 'verify':
        verify(workspace)
    elif args.command == 'build':
        build(workspace)
    elif args.command == 'smoke':
        smoke(workspace)
    print(workspace)


if __name__ == '__main__':
    main()
