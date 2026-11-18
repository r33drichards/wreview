"""Durable webhook inbox and PR-scoped tool service for the Wasmer runner."""
from contextlib import closing
from hashlib import sha256
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
import secrets
import sqlite3
from threading import Event, Lock, Thread
from urllib.parse import urlsplit
from uuid import UUID

from run import API, PROMPT, TOOLS, ReviewTools, js, pages, request


class Worker:
    def __init__(self):
        self.repo = os.environ["GITHUB_REPOSITORY"]
        self.repos = {self.repo} | {
            repo.strip() for repo in os.environ.get("GITHUB_ADDITIONAL_REPOSITORIES", "").split(",")
            if repo.strip()
        }
        self.token = os.environ["GITHUB_REVIEW_TOKEN"]
        self.secret = os.environ["GITHUB_WEBHOOK_SECRET"].encode()
        self.app_secret = os.environ.get("GITHUB_APP_WEBHOOK_SECRET", "").encode()
        self.app_owner = os.environ.get("GITHUB_APP_REPOSITORY_OWNER", "").casefold()
        self.installation = int(os.environ.get("GITHUB_APP_INSTALLATION_ID", "0"))
        self.runner_token = os.environ["RUNNER_API_TOKEN"]
        self.runner = os.environ["WASMER_REVIEW_URL"].rstrip("/")
        origin = urlsplit(self.runner)
        if origin.scheme != "https" or not origin.hostname or origin.username or origin.password:
            raise ValueError("WASMER_REVIEW_URL must be an HTTPS URL")
        self.db = os.environ["REVIEW_DB"]
        self.wake = Event()
        self.lock = Lock()
        self.active = None
        self.actor = request(f"{API}/user", token=self.token)["login"]
        with closing(self.connect()) as db, db:
            db.execute("CREATE TABLE IF NOT EXISTS inbox "
                       "(id TEXT PRIMARY KEY, payload TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0)")

    def connect(self):
        return sqlite3.connect(self.db, timeout=3)

    def accept(self, headers, raw):
        values = headers.get_all("X-Hub-Signature-256", [])
        if len(values) != 1:
            return 401

        def signed_with(key):
            return bool(key) and hmac.compare_digest(
                b"sha256=" + hmac.new(key, raw, sha256).hexdigest().encode(), values[0].encode()
            )

        app_delivery = signed_with(self.app_secret)
        if not app_delivery and not signed_with(self.secret):
            return 401
        kind = headers.get("X-GitHub-Event")
        if kind == "ping":
            return 204
        if kind != "pull_request":
            return 204
        try:
            delivery = str(UUID(headers["X-GitHub-Delivery"]))
            event = json.loads(raw)
            if event["action"] != "labeled" or event["label"]["name"] != "wreview":
                return 204
            pr = event["pull_request"]
            repo = event["repository"]["full_name"]
            installation = event.get("installation", {}).get("id") if app_delivery else None
            if (not self.allows_repository(repo, installation)
                    or (app_delivery and installation != self.installation)
                    or pr["base"]["repo"]["full_name"] != repo):
                return 403
            if type(pr["number"]) is not int or pr["number"] < 1:
                return 400
            payload = {key: pr[key] for key in ("number", "changed_files")}
            payload.update(repo=repo, installation=installation, head=pr["head"]["sha"], base=pr["base"]["sha"])
        except (KeyError, TypeError, ValueError):
            return 400
        with closing(self.connect()) as db, db:
            db.execute("INSERT OR IGNORE INTO inbox(id, payload) VALUES (?, ?)",
                       (delivery, json.dumps(payload)))
        self.wake.set()
        return 202

    def serve_queue(self):
        while True:
            self.wake.clear()
            with closing(self.connect()) as db:
                row = db.execute("SELECT id, payload FROM inbox WHERE done = 0 ORDER BY rowid LIMIT 1").fetchone()
            if row is None:
                self.wake.wait(5)
                continue
            delivery, payload = row
            try:
                self.review(delivery, json.loads(payload))
            except Exception as error:
                print(f"Review delivery {delivery} failed: {type(error).__name__}", flush=True)
                with closing(self.connect()) as db, db:
                    db.execute("UPDATE inbox SET done = 1 WHERE id = ?", (delivery,))
                continue
            with closing(self.connect()) as db, db:
                db.execute("UPDATE inbox SET done = 1 WHERE id = ?", (delivery,))

    def allows_repository(self, repo, installation):
        if installation is None:
            return repo in self.repos
        owner, separator, name = repo.partition("/")
        return bool(self.installation and installation == self.installation
                    and self.app_owner and owner.casefold() == self.app_owner
                    and separator and name and "/" not in name)

    def review(self, delivery, pr):
        number = pr["number"]
        repo = pr.get("repo", self.repo)
        if not self.allows_repository(repo, pr.get("installation")):
            return
        root = f"{API}/repos/{repo}"
        pull = f"{root}/pulls/{number}"
        comments = f"{root}/issues/{number}/comments"
        marker = f"<!-- wreview-delivery:{delivery} -->"
        status_marker = f"<!-- wreview-status:{delivery} -->"

        def github(url, payload=None, method=None):
            return request(url, payload, self.token, method=method)

        def find(url, suffix):
            return next((item for item in pages(url, self.token)
                         if item["user"]["login"] == self.actor
                         and (item.get("body") or "").endswith(suffix)), None)

        def current():
            value = github(pull)
            return (value["state"] == "open" and value["head"]["sha"] == pr["head"]
                    and value["base"]["sha"] == pr["base"]
                    and any(label["name"] == "wreview" for label in value["labels"]))

        status = find(comments, status_marker)
        if status is None:
            status = github(comments, {"body": f"wreview is starting.\n\n{status_marker}"})
        status_url = f"{root}/issues/comments/{status['id']}"
        try:
            posted = find(f"{pull}/reviews", marker)
            if posted is None and current():
                files = [{key: file[key] for key in ("filename", "status", "patch", "previous_filename")
                          if key in file} for file in pages(f"{pull}/files", self.token)]
                if current():
                    posted = self.run_review(delivery, repo, pr, files, current, pull, marker)
            message = "wreview finished without posting a review because the pull request changed."
            if posted is not None:
                message = f"wreview finished. [Read the review]({posted['html_url']})."
        except Exception:
            github(status_url, {"body": f"wreview failed.\n\n{status_marker}"}, method="PATCH")
            raise
        github(status_url, {"body": f"{message}\n\n{status_marker}"}, method="PATCH")

    def run_review(self, delivery, repo, pr, files, current, pull, marker):
        context = {"repo": repo, "number": pr["number"], "base_sha": pr["base"],
                   "head_sha": pr["head"], "files_available": len(files),
                   "files_reported": pr["changed_files"],
                   "files_without_patches": sum("patch" not in file for file in files)}
        data = json.dumps({"context": context, "files": files})
        session = f"wreview-{delivery}-{secrets.token_hex(8)}"
        seed, _ = js(session, f"globalThis.reviewState = JSON.parse({json.dumps(data)});")
        tools = ReviewTools(session, seed["heap"], files)
        if not tools.heap:
            raise RuntimeError("mcp-js did not save the prepared state")
        capability = secrets.token_hex(32)
        with self.lock:
            self.active = (capability, tools)
        try:
            # Wasmer returns an empty body on success.
            request(f"{self.runner}/turn", {
                "capability": capability, "instructions": PROMPT.read_text(),
                "input": f"Review {repo} pull request #{pr['number']} at {pr['head']}.",
                "tools": TOOLS,
            }, token=self.runner_token, timeout=600)
        finally:
            with self.lock:
                self.active = None
        if tools.review is None:
            raise RuntimeError("Agent completed without staging a review")
        if not current():
            return None
        review = tools.review
        review["body"] += f"\n\n{marker}"
        return request(f"{pull}/reviews", {
            **review, "event": "COMMENT", "commit_id": pr["head"],
        }, token=self.token)

    def tool(self, authorization, value):
        with self.lock:
            if self.active is None or not secrets.compare_digest(
                authorization.encode(), f"Bearer {self.active[0]}".encode()
            ):
                return 401, {}
            try:
                args = value.get("arguments", {})
                if not isinstance(args, dict):
                    raise ValueError("Tool arguments must be an object")
                return 200, {"result": self.active[1].call(value["name"], args)}
            except (KeyError, ValueError) as error:
                return 200, {"isError": True, "error": str(error)}


def serve():
    worker = Worker()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def reply(self, status, value=None):
            self.send_response(status)
            if value is not None:
                self.send_header("Content-Type", "application/json")
            self.end_headers()
            if value is not None:
                self.wfile.write(json.dumps(value).encode())

        def do_GET(self):
            self.reply(200 if self.path == "/health" else 404)

        def do_POST(self):
            if self.path not in {"/github/webhook", "/tool"}:
                return self.reply(404)
            try:
                length = int(self.headers["Content-Length"])
                if not 0 < length <= 1024 * 1024:
                    return self.reply(413)
                self.connection.settimeout(5)
                body = self.rfile.read(length)
                if self.path == "/github/webhook":
                    return self.reply(worker.accept(self.headers, body))
                payload = json.loads(body)
                if not isinstance(payload, dict):
                    return self.reply(400)
                return self.reply(*worker.tool(self.headers.get("Authorization", ""), payload))
            except (TypeError, ValueError):
                return self.reply(400)
            except (OSError, sqlite3.Error):
                return self.reply(503)

    Thread(target=worker.serve_queue, daemon=True).start()
    ThreadingHTTPServer(("0.0.0.0", int(os.environ.get("PORT", 8080))), Handler).serve_forever()


if __name__ == "__main__":
    serve()
