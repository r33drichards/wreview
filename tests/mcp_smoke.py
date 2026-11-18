"""Local HTTP MCP discovery smoke test. No credentials/model calls or packages needed.
Run after cargo build: python3 tests/mcp_smoke.py
"""
import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import subprocess
import threading
import sys

seen = []

class MCP(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        self.send_error(405)

    def do_DELETE(self):
        self.send_response(200)
        self.end_headers()

    def do_POST(self):
        assert self.headers.get('Authorization') == 'Bearer local-test-credential'
        assert self.headers.get('X-MCP-Session-Id') == 'test-session'
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        method = request['method']
        seen.append(method)
        if 'id' not in request:
            self.send_response(202)
            self.end_headers()
            return
        result = None
        if method == 'initialize':
            result = {'protocolVersion': '2025-11-25', 'capabilities': {'tools': {}},
                      'serverInfo': {'name': 'local-test', 'version': '1'}}
        elif method == 'tools/list':
            second = request.get('params', {}).get('cursor') == 'page2'
            result = {'tools': [{'name': 'second' if second else 'first',
                                 'inputSchema': {'type': 'object', 'properties': {}}}]}
            if not second:
                result['nextCursor'] = 'page2'
        response = {'jsonrpc': '2.0', 'id': request['id']}
        if result is None:
            response['error'] = {'code': -32601, 'message': 'Method not found'}
        else:
            response['result'] = result
        body = json.dumps(response).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

server = ThreadingHTTPServer(('127.0.0.1', 0), MCP)
threading.Thread(target=server.serve_forever, daemon=True).start()
env = dict(os.environ, MCP_URL=f'http://127.0.0.1:{server.server_port}/mcp')
env['MCP_TOKEN'] = 'local-test-credential'
env['MCP_SESSION_ID'] = 'test-session'
env.pop('MCP_SERVERS_JSON', None)
try:
    command = ['target/debug/minimal-mcp-agent', 'tools']
    if len(sys.argv) == 3 and sys.argv[1] == '--wasmer':
        command = [os.environ.get('WASMER_BIN', 'wasmer'), 'run', sys.argv[2], '--net', '--env', 'MCP_URL='+env['MCP_URL'], '--env', 'MCP_TOKEN='+env['MCP_TOKEN'], '--env', 'MCP_SESSION_ID='+env['MCP_SESSION_ID'], '--', 'tools']
    result = subprocess.run(command, env=env,
                            capture_output=True, text=True, timeout=45, check=True)
    assert result.stdout.splitlines() == ['first', 'second'], result.stdout
    assert seen.count('tools/list') == 2, seen
    assert 'initialize' in seen, seen
    print('PASS: exact bearer header, local HTTP connection, legacy fallback, and paginated MCP discovery')
finally:
    server.shutdown()
