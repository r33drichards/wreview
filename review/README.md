# Hosted pull request reviews

GitHub sends JSON pull request events to the new Wasmer app at
`/github/webhook`. Wasmer forwards the signed body to the review worker.
The worker verifies the HMAC and records accepted deliveries on a persistent
SQLite volume before acknowledging them. Run one worker replica with a
mounted volume and a private mcp-js service.

Build the WASIX binary with `scripts/build_wasix.py`. Deploy it under a
separate Wasmer app and package name. Use the root manifests as a starting
point, omit the existing `app_id`, and set `cli_args: [review]`. Configure
`REVIEW_WORKER_URL` as the worker's HTTPS origin, `RUNNER_API_TOKEN` as a
shared random secret, and `CHATGPT_ACCESS_TOKEN` and `OPENAI_MODEL` using the
runner's own ChatGPT sign-in. Replace the access token before it expires.

Deploy `review/Dockerfile` with the repository root as its build context.
Set these worker environment variables using the hosting service's secrets
manager where applicable.

- `GITHUB_REPOSITORY` is the primary allowed repository in owner/name form.
- `GITHUB_ADDITIONAL_REPOSITORIES` optionally lists additional allowed
  repositories as comma-separated owner/name entries. Configure the signed
  webhook on each repository you enable.
- `GITHUB_REVIEW_TOKEN` is a fine-grained GitHub token for that repository
  with pull request write permission. The review is attributed to its owner.
- `GITHUB_WEBHOOK_SECRET` is the secret also configured on the GitHub webhook.
- `RUNNER_API_TOKEN` matches the Wasmer app secret.
- `WASMER_REVIEW_URL` is the Wasmer app's HTTPS origin.
- Set `CHATGPT_ACCESS_TOKEN` and `OPENAI_MODEL` on the worker to run model
  steps directly when model responses can exceed Wasmer's request deadline.
  Use the runner's ChatGPT access token and replace it before it expires.
- `REVIEW_DB` is a SQLite file path on the mounted volume.
- `MCP_JS_URL` is the private HTTP origin of mcp-js. Enable directory heap
  storage with `--heap-store dir --heap-dir /tmp/wreview-heaps`.

For account-wide coverage, register a private GitHub App subscribed to pull
request events with Pull requests read permission. Install it on **All
repositories** for the repository owner. This route uses a separate secret
and only accepts events from its configured installation and owner.

- `GITHUB_APP_WEBHOOK_SECRET` is the GitHub App webhook secret.
- `GITHUB_APP_REPOSITORY_OWNER` is the owner login.
- `GITHUB_APP_INSTALLATION_ID` is the GitHub App installation ID.

Use the Wasmer webhook URL for the GitHub App and disable individual
repository hooks after confirming the app delivers events. The existing
worker credential posts reviews. Apply the `wreview` label to opt in a PR. Each delivery has one status comment and at most one
review. The model receives PR data and a delivery-scoped tool interface.
GitHub write operations happen in the worker.
