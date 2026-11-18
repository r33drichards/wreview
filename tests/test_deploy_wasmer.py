"""No credentials, network requests, or Wasmer executable required."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('deploy', Path(__file__).resolve().parents[1] / 'scripts/deploy_wasmer.py')
deploy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(deploy)
SECRET = 'do-not-log-this-fake-secret'


class DiagnosticsTests(unittest.TestCase):
    def invoke(self, args, result=None, error=None):
        output = io.StringIO()
        with patch.dict(os.environ, {'WASMER_TOKEN': SECRET}, clear=True), patch('sys.argv', ['deploy'] + args), patch.object(deploy.subprocess, 'run', return_value=result, side_effect=error) as command, contextlib.redirect_stdout(output):
            try:
                deploy.main()
            except SystemExit as failure:
                output.write(str(failure))
        self.assertNotIn(SECRET, output.getvalue())
        return output.getvalue(), command

    def test_read_only_success_never_deploys_or_prints_config(self):
        app = {'id': deploy.APP_ID, 'url': 'https://example.wasmer.app', 'config': SECRET, 'env': {'OTHER_SECRET': SECRET}}
        result = subprocess.CompletedProcess([], 0, json.dumps(app), SECRET)
        output, command = self.invoke(['--diagnose-only'], result)
        command.assert_called_once()
        self.assertEqual(command.call_args.args[0], ['wasmer', 'app', 'get', deploy.APP_ID, '--format', 'json'])
        self.assertIn('no publish or deploy command ran', output)
        self.assertNotIn('OTHER_SECRET', output)

    def test_preflight_failure_stops_deployment(self):
        result = subprocess.CompletedProcess([], 1, SECRET, 'Unauthorized ' + SECRET)
        output, command = self.invoke([], result)
        self.assertEqual(command.call_count, 1)
        self.assertIn('app-read: exit=1; text-signals=authentication', output)

    def test_unknown_error_is_not_echoed(self):
        output, _ = self.invoke(['--diagnose-only'], subprocess.CompletedProcess([], 9, SECRET, 'eyJlbmNvZGVkIjoic2VjcmV0In0='))
        self.assertIn('unclassified', output)
        self.assertNotIn('eyJlbmNvZGVk', output)

    def test_timeout_with_secret_output_is_suppressed(self):
        error = subprocess.TimeoutExpired(['wasmer', SECRET], 60, output=SECRET, stderr=SECRET)
        output, command = self.invoke(['--diagnose-only'], error=error)
        self.assertIn('timeout; outcome unknown', output)
        self.assertEqual(command.call_count, 1)

    def test_launch_error_is_suppressed(self):
        output, _ = self.invoke(['--diagnose-only'], error=OSError(SECRET))
        self.assertIn('executable-start-failed', output)

    def test_malformed_json_is_suppressed(self):
        output, _ = self.invoke(['--diagnose-only'], subprocess.CompletedProcess([], 0, SECRET, ''))
        self.assertIn('invalid-app-identity', output)

    def test_wrong_app_stops_deployment(self):
        output, command = self.invoke([], subprocess.CompletedProcess([], 0, json.dumps({'id': SECRET}), ''))
        self.assertIn('invalid-app-identity', output)
        self.assertEqual(command.call_count, 1)

    def test_non_object_json_is_suppressed(self):
        output, _ = self.invoke(['--diagnose-only'], subprocess.CompletedProcess([], 0, json.dumps([SECRET]), ''))
        self.assertIn('invalid-app-identity', output)

    def test_collision_requires_specific_signature(self):
        self.assertEqual(deploy.failure_signals('already exists ' + SECRET), 'unclassified')
        self.assertEqual(deploy.failure_signals('Registry already has version 0.1.0 of example, but with different contents. ' + SECRET), 'package-version-conflict')

    def test_deploy_failure_uses_fixed_signal_only(self):
        result = subprocess.CompletedProcess([], 1, SECRET, 'connection refused ' + SECRET)
        with patch.object(deploy.subprocess, 'run', return_value=result), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(SystemExit) as failure:
                deploy.run_cli('deploy', ['wasmer', 'deploy'], 600)
        self.assertIn('text-signals=network', str(failure.exception))
        self.assertNotIn(SECRET, str(failure.exception))


class PackageVersionTests(unittest.TestCase):
    def test_version_is_deterministic_and_only_package_version_changes(self):
        import tempfile
        source = '[package]\nname = "r33drichards/minimal-mcp-agent"\nversion = "0.1.0"\n[[module]]\nname = "agent"\nsource = "dist/agent.wasm"\n'
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'wasmer.toml'
            path.write_text(source)
            with contextlib.redirect_stdout(io.StringIO()):
                deploy.prepare_package('a' * 40, path)
                first = path.read_text()
                deploy.prepare_package('a' * 40, path)
            self.assertEqual(first, path.read_text())
            self.assertEqual(first, source.replace('version = "0.1.0"', 'version = "0.1.0-g' + 'a'*40 + '"'))

    def test_unexpected_manifest_is_not_modified(self):
        import tempfile
        for source in ['bad toml ' + SECRET, '[package]\nname="other"\nversion="0.1.0"', '[package]\nname="r33drichards/minimal-mcp-agent"\nversion="0.2.0"']:
            with tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / 'wasmer.toml'
                path.write_text(source)
                with self.assertRaises(SystemExit) as failure:
                    deploy.prepare_package('a' * 40, path)
                self.assertNotIn(SECRET, str(failure.exception))
                self.assertEqual(path.read_text(), source)

    def test_invalid_commit_is_rejected(self):
        with self.assertRaises(SystemExit):
            deploy.prepare_package(SECRET)


if __name__ == '__main__':
    unittest.main()
