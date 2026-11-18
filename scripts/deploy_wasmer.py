#!/usr/bin/env python3
"""Deploy the tested artifact; never print CLI output (it can contain secrets)."""
import argparse
import json
import os
from pathlib import Path
import tomllib
import re
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request


APP_ID = 'da_nBBIbtXUDZZG'


def failure_signals(output):
    """Return only fixed labels, never substrings of untrusted CLI output.

    These are observed text signatures, not a proven root cause. In particular,
    'already exists' alone is not enough to claim a package version collision.
    """
    output = output.lower()
    signatures = {
        'authentication': ('unauthorized', 'invalid token', 'not logged in'),
        'permission': ('permission denied', 'forbidden', "you don't have access"),
        'network': ('dns error', 'connection refused', 'connection reset', 'timed out'),
        'configuration': ('no app configuration was found', 'no owner specified', 'no package version specified'),
    }
    signals = [name for name, patterns in signatures.items() if any(p in output for p in patterns)]
    if ('registry already has version' in output and 'different contents' in output) or 'refusing to map two different releases' in output:
        signals.append('package-version-conflict')
    return ','.join(signals) or 'unclassified'


def run_cli(stage, command, timeout):
    # stage and command are fixed by this module, never derived from CLI output.
    print(f'Wasmer stage: {stage}', flush=True)
    try:
        result = subprocess.run(command, capture_output=True, text=True,
                                errors='replace', timeout=timeout)
    except subprocess.TimeoutExpired:
        raise SystemExit(f'Wasmer {stage}: timeout; outcome unknown. Inspect the app before any retry.') from None
    except OSError:
        raise SystemExit(f'Wasmer {stage}: executable-start-failed; output withheld.') from None
    if result.returncode:
        signals = failure_signals(result.stdout + '\n' + result.stderr)
        # Never print exception objects, command output, configuration or URLs
        # obtained from a failure. Even unknown/encoded secrets remain withheld.
        raise SystemExit(f'Wasmer {stage}: exit={result.returncode}; text-signals={signals}; raw output withheld. Signals are not a root-cause diagnosis.')
    return result


def inspect_app():
    result = run_cli('app-read', ['wasmer', 'app', 'get', APP_ID, '--format', 'json'], 60)
    try:
        app = json.loads(result.stdout)
        if not isinstance(app, dict) or app.get('id') != APP_ID:
            raise ValueError()
    except (ValueError, TypeError):
        raise SystemExit('Wasmer app-read: invalid-app-identity; raw output withheld.') from None
    print('Wasmer app-read: expected app is accessible. Deploy/publish permission and runtime health remain unverified.')
    return app


def prepare_package(commit, path=Path('wasmer.toml')):
    """Use one registry version per source commit; never overwrite 0.1.0.

    This changes only the CI checkout's package manifest, not Rust's version.
    Repeated deployments of the same commit keep the same package identity.
    """
    if not re.fullmatch(r'[0-9a-f]{40}', commit):
        raise SystemExit('Invalid package commit')
    version = '0.1.0-g' + commit
    try:
        source = path.read_text()
        manifest = tomllib.loads(source)
        package = manifest['package']
        if package['name'] != 'r33drichards/minimal-mcp-agent' or package['version'] not in ('0.1.0', version):
            raise ValueError()
        section = re.search(r'(?ms)^\[package\]\s*\n(.*?)(?=^\[|\Z)', source)
        if section is None:
            raise ValueError()
        body, count = re.subn(r'(?m)^version[ \t]*=[ \t]*"[^"\n]+"[ \t]*$', 'version = "' + version + '"', section.group(1))
        if count != 1:
            raise ValueError()
        updated = source[:section.start(1)] + body + source[section.end(1):]
        expected = dict(manifest)
        expected['package'] = dict(package, version=version)
        if tomllib.loads(updated) != expected:
            raise ValueError()
        path.write_text(updated)
    except (OSError, ValueError, KeyError, TypeError):
        raise SystemExit('Package manifest could not be safely versioned; contents withheld') from None
    print('Package version selected from verified source commit: ' + version)


def check_url(url):
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme != 'https' or not (parsed.hostname or '').endswith('.wasmer.app') or parsed.username or parsed.password:
        raise SystemExit('Unexpected Wasmer deployment URL')
    return url.rstrip('/')


def verify(url, commit):
    # These probes never carry credentials and never invoke a model or MCP tool.
    for attempt in range(30):
        try:
            with urllib.request.urlopen(url + '/health', timeout=10) as response:
                health = json.load(response)
            if health.get('status') == 'ok' and health.get('commit') == commit:
                break
        except (OSError, ValueError):
            pass
        time.sleep(5)
    else:
        raise SystemExit('Deployment did not become healthy at the expected commit')
    try:
        request = urllib.request.Request(url + '/turn', data=b'{}', headers={'Content-Type': 'application/json'})
        with urllib.request.urlopen(request, timeout=10):
            raise SystemExit('Unauthenticated /turn was not rejected')
    except urllib.error.HTTPError as error:
        if error.code != 401:
            raise SystemExit('Unauthenticated /turn did not return 401') from None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--diagnose-only', action='store_true', help='Read the existing app only; never publish or deploy')
    args = parser.parse_args()
    if not os.environ.get('WASMER_TOKEN'):
        raise SystemExit('Set the production environment secret WASMER_TOKEN to a newly rotated Wasmer deployment token')
    inspect_app()
    if args.diagnose_only:
        print('Read-only diagnostics complete; no publish or deploy command ran.')
        return
    commit = os.environ.get('GITHUB_SHA', '')
    if not re.fullmatch(r'[0-9a-f]{40}', commit):
        raise SystemExit('GITHUB_SHA must identify the tested commit')
    prepare_package(commit)
    # Capture both streams: Wasmer diagnostics and JSON config may contain secrets.
    result = run_cli('deploy', ['wasmer', 'deploy', '--non-interactive', '--no-wait', '--format', 'json'], 600)
    try:
        deployment = json.loads(result.stdout)
        url = check_url(deployment['url'])
        app_id = deployment['app']['id']
    except (ValueError, KeyError, TypeError):
        raise SystemExit('Could not read deployment identity; CLI output withheld') from None
    if app_id != 'da_nBBIbtXUDZZG':
        raise SystemExit('Unexpected deployed application ID')
    verify(url, commit)
    # Also verify the stable app route, not only the immutable version URL.
    result = run_cli('live-app-read', ['wasmer', 'app', 'get', APP_ID, '--format', 'json'], 60)
    try:
        live = check_url(json.loads(result.stdout)['url'])
    except (ValueError, KeyError, TypeError):
        raise SystemExit('Could not read live app URL; CLI output withheld') from None
    verify(live, commit)
    summary = f'Deployed commit `{commit}`. Healthy version: {url}; live app: {live}. Unauthenticated /turn returned 401.\n'
    print(summary)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as stream:
            stream.write(summary)


if __name__ == '__main__':
    main()
