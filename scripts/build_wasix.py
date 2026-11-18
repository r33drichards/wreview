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
parser.add_argument('--update-lock', action='store_true')
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
(PROJECT / 'review').mkdir(exist_ok=True)
shutil.copy2(ROOT / 'review' / 'prompt.md', PROJECT / 'review' / 'prompt.md')
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
# tokio-postgres excludes the keepalive configuration field on WASI, but
# its runtime connection path still reads it. WASIX sockets can connect
# with the optional TCP keepalive argument disabled.
PG_VERSION = '0.7.18'
PG_HASH = 'a528f7d280f6d5b9cd149635c8705b0dd049754bc67d81d31fa25169a93809d3'
pg = BUILD / 'tokio-postgres'
pg_archive = urllib.request.urlopen(
    f'https://static.crates.io/crates/tokio-postgres/tokio-postgres-{PG_VERSION}.crate', timeout=60
).read()
if hashlib.sha256(pg_archive).hexdigest() != PG_HASH:
    raise SystemExit('tokio-postgres crate checksum mismatch')
if pg.exists():
    shutil.rmtree(pg)
with tarfile.open(fileobj=io.BytesIO(pg_archive), mode='r:gz') as tar:
    prefix = f'tokio-postgres-{PG_VERSION}/'
    for member in tar.getmembers():
        if member.issym() or member.islnk() or '..' in Path(member.name).parts or not member.name.startswith(prefix):
            raise SystemExit('Unexpected tokio-postgres archive entry')
    tar.extractall(BUILD)
(BUILD / f'tokio-postgres-{PG_VERSION}').rename(pg)
connect = pg / 'src' / 'connect.rs'
source = connect.read_text()
assert source.count('if config.keepalives {') == 2
source = source.replace('        if config.keepalives {',
    '        #[cfg(target_os = "wasi")] None,\n'
    '        #[cfg(not(target_os = "wasi"))] if config.keepalives {', 1)
source = source.replace('        keepalive: if config.keepalives {',
    '        #[cfg(target_os = "wasi")] keepalive: None,\n'
    '        #[cfg(not(target_os = "wasi"))] keepalive: if config.keepalives {', 1)
connect.write_text(source)
library = pg / 'src' / 'lib.rs'
lib_source = library.read_text()
assert 'mod keepalive;' in lib_source
library.write_text(lib_source.replace('mod keepalive;',
    'mod keepalive;\n#[cfg(target_os = "wasi")]\nmod keepalive {\n'
    '    #[derive(Clone, PartialEq, Eq)] pub(crate) struct KeepaliveConfig {\n'
    '        pub idle: std::time::Duration, pub interval: Option<std::time::Duration>,\n'
    '        pub retries: Option<u32>,\n'
    '    }\n}'))
configuration = pg / 'src' / 'config.rs'
config_source = configuration.read_text()
assert '#[cfg(not(target_arch = "wasm32"))]' in config_source
configuration.write_text(config_source.replace('#[cfg(not(target_arch = "wasm32"))]', ''))
# The implicit username lookup imports WASI Preview 2 through whoami.
# The managed database always provides an explicit user.
raw = pg / 'src' / 'connect_raw.rs'
raw_source = raw.read_text()
fallback = '        None => Cow::Owned(whoami::username().map_err(|err| Error::io(err.into()))?),'
assert fallback in raw_source
raw.write_text(raw_source.replace(fallback,
    '        #[cfg(not(target_os = "wasi"))]\n' + fallback + '\n'
    '        #[cfg(target_os = "wasi")]\n'
    '        None => return Err(Error::config("a database username is required".into())),'))
socket = pg / 'src' / 'connect_socket.rs'
socket_source = socket.read_text()
assert 'use socket2::{SockRef, TcpKeepalive};' in socket_source
socket_source = socket_source.replace('use socket2::{SockRef, TcpKeepalive};',
    '#[cfg(not(target_os = "wasi"))]\nuse socket2::{SockRef, TcpKeepalive};')
for line in ('            stream.set_nodelay(true).map_err(Error::connect)?;',
             '            let sock_ref = SockRef::from(&stream);',
             '            if let Some(keepalive_config) = keepalive_config {'):
    assert line in socket_source
    socket_source = socket_source.replace(line, '            #[cfg(not(target_os = "wasi"))]\n' + line)
# WASIX can block when Tokio reads an empty socket after the TLS reply.
# Set nonblocking explicitly before the connected socket enters the driver.
tcp_connect = '                connect_with_timeout(TcpStream::connect((*ip, port)), connect_timeout).await?;'
assert tcp_connect in socket_source
socket_source = socket_source.replace(tcp_connect, tcp_connect + """
            #[cfg(target_os = "wasi")]
            let stream = {
                let stream = stream.into_std().map_err(Error::connect)?;
                stream.set_nonblocking(true).map_err(Error::connect)?;
                TcpStream::from_std(stream).map_err(Error::connect)?
            };
""")
socket.write_text(socket_source)

manifest += '\n[patch.crates-io]\nrmcp = { path = "../rmcp" }\ntokio-postgres = { path = "../tokio-postgres" }\n'
(PROJECT / 'Cargo.toml').write_text(manifest)
lock = ROOT / 'wasix' / 'Cargo.lock'
if lock.exists():
    shutil.copy2(lock, PROJECT / 'Cargo.lock')
command = ['cargo', 'wasix', 'build', '--release', '--bin', 'minimal-mcp-agent']
if args.features:
    command += ['--features', args.features]
if lock.exists() and not args.update_lock:
    command += ['--locked']
subprocess.run(command, cwd=PROJECT, check=True)
(ROOT / 'dist').mkdir(exist_ok=True)
shutil.copy2(PROJECT / 'target/wasm32-wasmer-wasi/release/minimal-mcp-agent.wasm', ROOT / 'dist/minimal-mcp-agent.wasm')
if args.update_lock or not lock.exists():
    shutil.copy2(PROJECT / 'Cargo.lock', lock)
print('Built dist/minimal-mcp-agent.wasm; no credentials or environment files packaged.')
