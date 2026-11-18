#!/usr/bin/env python3
"""Reproducible compatibility overlay; leaves the native source/dependencies intact.
Prerequisites: cargo-wasix 0.1.34+, official WASI SDK clang, Python3, network access.
Set CC_wasm32_wasmer_wasi to the WASI SDK clang binary when the host clang lacks WASM.
"""
import argparse
import hashlib
import io
from pathlib import Path
import shutil
import subprocess
import tarfile
import urllib.request

parser = argparse.ArgumentParser()
parser.add_argument('--features', default='')
args = parser.parse_args()

ROOT = Path(__file__).resolve().parent.parent
BUILD = ROOT / '.wasix-build'
PROJECT = BUILD / 'project'
SDK = BUILD / 'rmcp'
VERSION = '3.10.0'
CHECKSUM = 'b5949e504d5ab9944a168259afc0c39ac0230627b6ec5367ace194c59bcbe163'
PROJECT.mkdir(parents=True, exist_ok=True)
# Copy only explicitly intended source inputs, never .env, auth stores, .git or targets.
for name in ['Cargo.toml', 'README.md']:
    shutil.copy2(ROOT / name, PROJECT / name)
for name in ['src', 'tests', 'migrations']:
    if (PROJECT / name).exists():
        shutil.rmtree(PROJECT / name)
    shutil.copytree(ROOT / name, PROJECT / name)
archive = urllib.request.urlopen(f'https://static.crates.io/crates/rmcp/rmcp-{VERSION}.crate', timeout=60).read()
if hashlib.sha256(archive).hexdigest() != CHECKSUM:
    raise SystemExit('rmcp crate checksum mismatch; refusing to build')
if SDK.exists():
    shutil.rmtree(SDK)
with tarfile.open(fileobj=io.BytesIO(archive), mode='r:gz') as tar:
    # Reject paths escaping the fixed versioned crate root (Python 3.9 compatible).
    for member in tar.getmembers():
        if member.issym() or member.islnk() or '..' in Path(member.name).parts or not member.name.startswith(f'rmcp-{VERSION}/'):
            raise SystemExit('Unexpected crate archive entry')
    tar.extractall(BUILD)
(BUILD / f'rmcp-{VERSION}').rename(SDK)
manifest = (SDK / 'Cargo.toml').read_text()
assert 'version = "0.13.2"' in manifest
manifest = manifest.replace('version = "0.13.2"', 'version = "0.12"')
manifest = manifest.replace('reqwest?/rustls', 'reqwest?/rustls-tls')
(SDK / 'Cargo.toml').write_text(manifest)
manifest = (PROJECT / 'Cargo.toml').read_text()
old = 'reqwest = { version = "0.13", default-features = false, features = ["blocking", "json", "rustls", "form"] }'
new = 'reqwest = { version = "0.12", default-features = false, features = ["blocking", "json", "rustls-tls", "stream"] }'
assert old in manifest
manifest = manifest.replace(old, new)
manifest += '\n[patch.crates-io]\nrmcp = { path = "../rmcp" }\n'
(PROJECT / 'Cargo.toml').write_text(manifest)
lock = ROOT / 'wasix' / 'Cargo.lock'
if lock.exists():
    shutil.copy2(lock, PROJECT / 'Cargo.lock')
command = ['cargo', 'wasix', 'build', '--release', '--bin', 'minimal-mcp-agent']
if args.features:
    command += ['--features', args.features]
if lock.exists():
    command += ['--locked']
subprocess.run(command, cwd=PROJECT, check=True)
(ROOT / 'dist').mkdir(exist_ok=True)
shutil.copy2(PROJECT / 'target/wasm32-wasmer-wasi/release/minimal-mcp-agent.wasm', ROOT / 'dist/minimal-mcp-agent.wasm')
if not lock.exists():
    shutil.copy2(PROJECT / 'Cargo.lock', lock)
print('Built dist/minimal-mcp-agent.wasm; no credentials or environment files packaged.')
