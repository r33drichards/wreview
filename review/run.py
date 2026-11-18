"""Run one review from a trusted GitHub Actions pull_request_target event."""
import contextlib
import http.server
import json
import os
from pathlib import Path
import re
import secrets
import socket
import subprocess
import threading
import time
import urllib.error
import urllib.request

PROMPT = Path(__file__).with_name("prompt.md")
API = "https://api.github.com"
MCP_JS = "http://127.0.0.1:8091"


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def request(url, payload=None, token=None, timeout=30):
    headers = {"Accept": "application/vnd.github+json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if payload is not None:
        headers["Content-Type"] = "application/json"
    data = None if payload is None else json.dumps(payload).encode()
    try:
        with urllib.request.build_opener(NoRedirect()).open(
            urllib.request.Request(url, data, headers), timeout=timeout
        ) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        raise RuntimeError(f"{url.split('?', 1)[0]} returned HTTP {error.code}") from None


def pages(url, token):
    for page in range(1, 101):
        batch = request(f"{url}?per_page=100&page={page}", token=token)
        yield from batch
        if len(batch) < 100:
            return
    raise RuntimeError("GitHub pagination limit reached")


def js(session, code, heap=None):
    payload = {"session": session, "code": code}
    if heap is not None:
        payload["heap"] = heap
    execution = request(f"{MCP_JS}/api/exec", payload)
    url = f"{MCP_JS}/api/executions/{execution['execution_id']}"
    until = time.monotonic() + 30
    while time.monotonic() < until:
        result = request(url)
        if result["status"] == "completed":
            return result, url
        if result["status"] in {"failed", "cancelled", "timed_out"}:
            raise RuntimeError("mcp-js execution failed")
        time.sleep(0.2)
    raise RuntimeError("mcp-js execution timed out")


def schema(name, description, properties, required):
    return {"name": name, "description": description, "inputSchema": {
        "type": "object", "properties": properties, "required": required,
        "additionalProperties": False,
    }}


TOOLS = [
    schema("pr_context", "Read verified PR identity and review coverage from setup state.", {}, []),
    schema("changed_file", "Read one changed file and its patch page by zero-based index and offset.",
           {"index": {"type": "integer"}, "offset": {"type": "integer"}}, ["index"]),
    schema("submit_review", "Stage one comment-only GitHub review for this PR.",
           {"body": {"type": "string"}, "comments": {"type": "array", "items": {
               "type": "object", "properties": {
                   "path": {"type": "string"}, "line": {"type": "integer"},
                   "side": {"enum": ["LEFT", "RIGHT"]}, "body": {"type": "string"},
               }, "required": ["path", "line", "side", "body"], "additionalProperties": False,
           }}}, ["body", "comments"]),
]


def changed_lines(files):
    anchors = set()
    for file in files:
        left = right = None
        for line in file.get("patch", "").splitlines():
            hunk = re.match(r"^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@", line)
            if hunk:
                left, right = map(int, hunk.groups())
            elif left is not None and line.startswith("+"):
                anchors.add((file["filename"], right, "RIGHT"))
                right += 1
            elif left is not None and line.startswith("-"):
                anchors.add((file["filename"], left, "LEFT"))
                left += 1
            elif left is not None and line.startswith(" "):
                left += 1
                right += 1
    return anchors


class ReviewTools:
    def __init__(self, session, heap, files):
        self.session = session
        self.heap = heap
        self.files = files
        self.anchors = changed_lines(files)
        self.review = None

    def call(self, name, args):
        if name == "pr_context" and args == {}:
            return self.read("reviewState.context")
        if name == "changed_file" and set(args) <= {"index", "offset"}:
            index, offset = args.get("index"), args.get("offset", 0)
            if type(index) is not int or not 0 <= index < len(self.files):
                raise ValueError("Invalid file index")
            if type(offset) is not int or not 0 <= offset <= len(self.files[index].get("patch", "")):
                raise ValueError("Invalid patch offset")
            return self.read(f"""(() => {{
              const file = reviewState.files[{index}];
              const patch = Array.from(file.patch ?? "");
              const end = {offset} + 10000;
              return {{...file, patch: patch.slice({offset}, end).join(""),
                       next_offset: end < patch.length ? end : null}};
            }})()""")
        if name == "submit_review" and set(args) == {"body", "comments"}:
            body, comments = args["body"], args["comments"]
            if not isinstance(body, str) or not body.strip() or len(body) > 12000:
                raise ValueError("Review body must contain 1–12000 characters")
            if not isinstance(comments, list) or len(comments) > 30:
                raise ValueError("Provide at most 30 inline comments")
            for item in comments:
                if (not isinstance(item, dict) or set(item) != {"path", "line", "side", "body"}
                    or not isinstance(item["path"], str) or type(item["line"]) is not int
                    or not isinstance(item["side"], str)
                    or (item["path"], item["line"], item["side"]) not in self.anchors
                    or not isinstance(item["body"], str) or not item["body"].strip()
                    or len(item["body"]) > 2000):
                    raise ValueError("Inline comments must target changed lines and contain 1–2000 characters")
            if self.review is not None:
                raise ValueError("A review has already been staged")
            self.review = {"body": body, "comments": comments}
            return {"staged": True}
        raise ValueError("Invalid tool or arguments")

    def read(self, expression):
        # Read the exact snapshot created by setup. Model strings never become JS.
        code = f"console.log(JSON.stringify({expression}));"
        try:
            _, url = js(self.session, code, self.heap)
            output = request(f"{url}/output?byte_limit=100000")
            return json.loads(output["data"])
        except (RuntimeError, OSError, ValueError) as error:
            raise ValueError("Could not read prepared PR state") from error


@contextlib.contextmanager
def mcp_bridge(tools, token):
    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            self.send_error(405)

        def do_POST(self):
            if self.path != "/mcp" or not secrets.compare_digest(
                self.headers.get("Authorization", ""), f"Bearer {token}"
            ):
                self.send_error(401)
                return
            try:
                length = int(self.headers.get("Content-Length", "0"))
                if not 0 < length <= 1024 * 1024:
                    raise ValueError
                rpc = json.loads(self.rfile.read(length))
            except ValueError:
                self.send_error(400)
                return
            if "id" not in rpc:
                self.send_response(202)
                self.end_headers()
                return
            response = {"jsonrpc": "2.0", "id": rpc["id"]}
            match rpc["method"]:
                case "initialize":
                    response["result"] = {
                        "protocolVersion": rpc["params"]["protocolVersion"],
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "wreview", "version": "1"},
                    }
                case "tools/list":
                    response["result"] = {"tools": TOOLS}
                case "tools/call":
                    is_error = False
                    try:
                        params = rpc["params"]
                        value = tools.call(params["name"], params.get("arguments", {}))
                    except ValueError as error:
                        is_error, value = True, str(error)
                    response["result"] = {"isError": is_error, "content": [
                        {"type": "text", "text": json.dumps(value)},
                    ]}
                case "ping":
                    response["result"] = {}
                case _:
                    response["error"] = {"code": -32601, "message": "Method not found"}
            body = json.dumps(response).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    with http.server.HTTPServer(("127.0.0.1", 0), Handler) as server:
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            yield f"http://127.0.0.1:{server.server_port}/mcp"
        finally:
            server.shutdown()
            worker.join()


def run_wasm(tools, context):
    runner_token, mcp_token = secrets.token_hex(32), secrets.token_hex(32)
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    url = f"http://127.0.0.1:{port}"
    with mcp_bridge(tools, mcp_token) as mcp_url:
        runner_env = {
            "MCP_URL": mcp_url, "MCP_TOKEN": mcp_token, "MCP_SESSION_ID": tools.session,
            "PORT": str(port), "RUNNER_API_TOKEN": runner_token,
            "OPENAI_MODEL": os.environ["OPENAI_MODEL"],
            "CHATGPT_ACCESS_TOKEN": os.environ["CHATGPT_ACCESS_TOKEN"],
        }
        command = ["wasmer", "run", "dist/minimal-mcp-agent.wasm", "--net"]
        for key, value in runner_env.items():
            command.extend(["--env", f"{key}={value}"])
        with subprocess.Popen([*command, "--", "serve"], env={
            key: os.environ[key] for key in ("PATH", "HOME") if key in os.environ
        }) as runner:
            try:
                until = time.monotonic() + 120
                while time.monotonic() < until:
                    if runner.poll() is not None:
                        raise RuntimeError("WASM runner exited before it was ready")
                    try:
                        request(f"{url}/health", timeout=2)
                        break
                    except OSError:
                        time.sleep(0.5)
                else:
                    raise RuntimeError("WASM runner readiness timed out")
                request(f"{url}/turn", {
                    "messages": [{"role": "developer", "content": PROMPT.read_text()}],
                    "input": f"REPO: {context['repo']}\nPR NUMBER: {context['number']}\n"
                             f"HEAD SHA: {context['head_sha']}\nReview this pull request.",
                }, token=runner_token, timeout=600)
            finally:
                runner.terminate()
                try:
                    runner.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    runner.kill()


def report_review(review):
    if output := os.environ.get("GITHUB_OUTPUT"):
        with open(output, "a", encoding="utf-8") as stream:
            print(f"url={review['html_url']}", file=stream)


def main():
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    if (os.environ["GITHUB_EVENT_NAME"] != "pull_request_target"
        or event["action"] != "labeled" or event["label"]["name"] != "wreview"):
        return
    pr = event["pull_request"]
    repo, number = os.environ["REPO"], int(os.environ["PR_NUMBER"])
    if (not re.fullmatch(r"[\w.-]+/[\w.-]+", repo, flags=re.ASCII)
        or repo != event["repository"]["full_name"] or number != pr["number"] or number < 1):
        raise RuntimeError("Repository or PR mismatch")
    for key in ("CHATGPT_ACCESS_TOKEN", "OPENAI_MODEL"):
        if not os.environ.get(key):
            raise RuntimeError(f"Configure {key} in GitHub Actions settings")
    token = os.environ["GH_TOKEN"]
    url = f"{API}/repos/{repo}/pulls/{number}"
    run_id = int(os.environ["GITHUB_RUN_ID"])
    marker = f"<!-- wreview-run:{run_id} -->"
    for review in pages(f"{url}/reviews", token):
        if (review.get("user", {}).get("login") == "github-actions[bot]"
            and (review.get("body") or "").endswith(marker)):
            report_review(review)
            print("Review already posted for this run")
            return

    def still_current():
        current = request(url, token=token)
        return (current["state"] == "open" and current["head"]["sha"] == pr["head"]["sha"]
                and current["base"]["sha"] == pr["base"]["sha"]
                and any(label["name"] == "wreview" for label in current["labels"]))

    if not still_current():
        print("PR changed or review label was removed")
        return
    files = [{key: file[key] for key in ("filename", "status", "patch", "previous_filename")
              if key in file} for file in pages(f"{url}/files", token)]
    if not still_current():
        print("PR changed while reading its files")
        return
    context = {"repo": repo, "number": number, "base_sha": pr["base"]["sha"],
               "head_sha": pr["head"]["sha"], "files_available": len(files),
               "files_reported": pr["changed_files"],
               "files_without_patches": sum("patch" not in file for file in files)}
    # Embed only serialized data. No repository content is evaluated as code.
    data = json.dumps({"context": context, "files": files})
    session = f"wreview-{run_id}-{secrets.token_hex(8)}"
    seed, _ = js(session, f"globalThis.reviewState = JSON.parse({json.dumps(data)});")
    tools = ReviewTools(session, seed["heap"], files)
    if not tools.heap:
        raise RuntimeError("mcp-js did not save the prepared state")
    run_wasm(tools, context)
    if tools.review is None:
        raise RuntimeError("Agent completed without staging a review")
    if not still_current():
        print("PR changed during the review. Skipping publication.")
        return
    review = tools.review
    review["body"] += f"\n\n{marker}"
    result = request(f"{url}/reviews", {
        **review, "event": "COMMENT", "commit_id": context["head_sha"],
    }, token=token)
    report_review(result)
    print(f"Posted review {result['id']} on PR #{number}")


if __name__ == "__main__":
    main()
