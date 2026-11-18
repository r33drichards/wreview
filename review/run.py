"""Prepare and validate a single hosted pull request review."""
import json
import os
from pathlib import Path
import re
import time
import urllib.error
import urllib.request

PROMPT = Path(__file__).with_name("prompt.md")
API = "https://api.github.com"
MCP_JS = os.environ.get("MCP_JS_URL", "http://127.0.0.1:8091").rstrip("/")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def request(url, payload=None, token=None, timeout=30, method=None):
    headers = {"Accept": "application/vnd.github+json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if payload is not None:
        headers["Content-Type"] = "application/json"
    data = None if payload is None else json.dumps(payload).encode()
    try:
        with urllib.request.build_opener(NoRedirect()).open(
            urllib.request.Request(url, data, headers, method=method), timeout=timeout
        ) as response:
            content = response.read()
            return json.loads(content) if content else None
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
