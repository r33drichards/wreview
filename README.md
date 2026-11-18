# Minimal MCP Agent

A small Rust terminal agent: **read input → ask the model → run MCP tools → repeat**.
The agent loop is in [`src/agent.rs`](src/agent.rs). No agent framework, database,
local Rust tool registry, or Codex subprocess. Tools come from configured MCP servers.

## Run

Requires Rust 1.88+ and a Unix-like system (macOS/Linux). Authentication uses the
official **Sign in with ChatGPT** direct flow, not an OpenAI API key and not copied
Codex credentials. Account/plan availability and app usage limits apply.

```sh
cargo build --locked
cargo run --locked -- login
cargo run --locked -- models

# Pick a slug from the account-specific models command:
export OPENAI_MODEL='<model-slug>'
export MCP_URL='http://127.0.0.1:8000/mcp'
# Optional, only if your MCP server requires a bearer token:
# read -rs MCP_TOKEN; export MCP_TOKEN

cargo run --locked -- tools   # check connectivity and list tools, no model call
cargo run --locked
```

`login` opens your browser for ChatGPT authorization. Nothing needs to be pasted
into the source code. Choose an existing saved account or add another on subsequent
logins. `logout` revokes the active refresh token and removes its local credentials;
registration metadata is retained for later sign-in.

Tool invocations execute automatically, with no per-call approval or human-in-the-loop
step. Each call logs its name and arguments. `/quit` or EOF exits between turns.
Conversation history is in memory only; starting a new process starts a new conversation.

## Multiple OAuth-protected HTTP MCP servers

Configure endpoint names and **environment variable names**, never inline tokens:

```sh
export MCP_SERVERS_JSON='[{"name":"search","url":"https://search.example/mcp","token_env":"SEARCH_MCP_ACCESS_TOKEN"},{"name":"code","url":"https://code.example/mcp","token_env":"CODE_MCP_ACCESS_TOKEN"}]'
# Set SEARCH_MCP_ACCESS_TOKEN and CODE_MCP_ACCESS_TOKEN via your host's secret store.
cargo run --locked -- tools
```

An OAuth access token is used as a bearer credential. Obtain it through each MCP
provider's supported authorization flow. This is **not** automatic OAuth login,
dynamic client registration, client-credentials exchange, or token refresh.
Expired credentials fail closed; rotate secrets and restart. Token env references
are isolated per server; ChatGPT credentials are never reused for MCP authentication.
`MCP_SERVERS_JSON` takes precedence over the single-server `MCP_URL`/`MCP_TOKEN` form.

## Hosted HTTP mode

```sh
export OPENAI_MODEL='<model-slug>'
export MCP_URL='https://your-server.example/mcp'
# Set these through the deployment platform's encrypted secrets UI:
# CHATGPT_ACCESS_TOKEN: an OAuth access token issued by this app's official direct SIWC flow
# RUNNER_API_TOKEN: a separate random secret at least 32 characters long
export PORT=8080
cargo run --locked -- serve
```

`serve` binds `0.0.0.0:$PORT`. `GET /health` is public. `POST /turn` requires
`Authorization: Bearer <RUNNER_API_TOKEN>` and JSON:

```json
{"input":"Your request", "messages":[]}
```

The response contains `messages`; send that array back with the next input to
continue the conversation. No transcript is shared between requests. Requests
are sequential, bodies are capped at 1 MiB, and model/tool timeouts plus the
20-round limit still apply. A failure may follow completed tool actions; the
response includes partial history, and **clients must not automatically retry**.
Serve behind HTTPS. The runner secret grants autonomous use of all configured
MCP tools, so share it only with trusted callers. There is no built-in rate limiter.

`CHATGPT_ACCESS_TOKEN` overrides local account storage for noninteractive runs.
It is a short-lived OAuth credential, **not** an API key or a Codex auth file.
It is not refreshed automatically in env-token mode. Provision and rotate it with
your host's supported secret mechanism; never commit it or bake it into an image.
The interactive `login` flow remains a local CLI feature, not a public HTTP endpoint.

## The loop

```text
messages = []
for each user input:
    messages += user input
    repeat (at most 20 model rounds):
        output = Responses(messages, discovered MCP tool schemas)
        messages += all output items
        print assistant text
        if no function calls: break
        for each function call:
            result = MCP tools/call (or an error result)
            messages += function_call_output with the original call_id
```

Multiple calls execute sequentially. Tool errors become results the model can
reason about. A network/tool timeout is **not retried automatically**: an action
may already have happened. Check the external system before requesting a retry.
The round limit stops runaway loops after completing the pending result batch.

## What lives where

- `src/agent.rs`: the loop, provider-independent via closures, and its tests.
- `src/main.rs`: terminal input and automatic MCP connection/discovery/calls.
- `src/model.rs`: Responses streaming and account-specific model listing.
- `src/auth.rs`: OAuth plumbing, separate so the core loop stays easy to read.

The official `rmcp` SDK handles Streamable HTTP/SSE, modern protocol discovery,
legacy initialization fallback, and paginated tool discovery. This starter does
not launch stdio servers or run MCP OAuth browser/refresh flows; supply already-issued
OAuth access tokens through secret environment variables. Old two-endpoint HTTP+SSE servers are not supported. Tool schemas are
loaded at startup; restart after a server's tools change. MCP prompts/resources
are not loaded automatically.

MCP tool names are mapped to `mcp_0`, `mcp_1`, etc. for model compatibility, then
mapped back when invoked. Structured/text MCP results are retained as JSON tool
output. Images/audio are not converted into model image/audio inputs.

Responses use `store: false`, `stream: true`; full completed output (including
encrypted reasoning) is preserved before tool results. An interrupted, failed,
or incomplete stream is an error, not a successful assistant turn. This tiny
version buffers a completed response instead of rendering token-by-token.

## Security and limits

- Connect only to MCP servers you trust. Their tool descriptions and results go
  to the selected model, and their tools may perform real external actions.
- Each MCP access token is sent **only** to its configured server; the ChatGPT credential is sent only to
  official OpenAI auth/API endpoints. Credential-bearing redirects are disabled.
- MCP HTTP is allowed only on loopback; use HTTPS for remote servers.
- OAuth uses PKCE, fresh state/nonce, and signed ID-token validation. Credentials
  are saved outside the repo under `~/.config/minimal-mcp-agent/`, with owner-only
  permissions and serialized atomic refresh. The file is permission-protected,
  **not encrypted at rest**. Do not sync or share it.
- No shell/filesystem tools are bundled. **All discovered MCP tools are available
  to the model and execute without approval**, with the server's own permissions.
  Use a least-privilege MCP server and avoid exposing destructive or sensitive
  tools you do not want the model to call autonomously.
- No server-side history persistence, compaction, automatic retry, background jobs, or
  parallel tool execution. Long conversations eventually hit model context limits.

## Check

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
python3 tests/mcp_smoke.py
```

Unit tests use no credentials or paid model requests. End-to-end authentication
and real model inference require your own browser sign-in and eligible plan.

## Protocol references

- [Sign in with ChatGPT: registration](https://developers.openai.com/siwc/token-sharing-open-source/sign-in)
- [ChatGPT plan models and inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference)
- [Official Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk)

## mcp-js

Point `MCP_URL` at mcp-js's Streamable HTTP **`/mcp`** endpoint, not its REST
`/api/exec` endpoint. If mcp-js has `JWKS_URL` configured, use `MCP_TOKEN` for a
JWT from that issuer. The runner sends exactly one `Authorization: Bearer`
prefix; mcp-js verifies requests against its configured JWKS. mcp-js itself is
not the OAuth token issuer, so obtain/refresh the token with your identity provider.

For a stateful JS heap/filesystem, set `MCP_SESSION_ID`. It becomes the
`X-MCP-Session-Id` header, distinct from the MCP protocol's transport session.
With multiple servers, use `session_id_env` beside `token_env` in each
`MCP_SERVERS_JSON` entry. A configured logical session is shared by this runner's
turns; run separate instances/session IDs when you need isolated workspaces.

See [WASIX build and deployment](BUILD.md) and [durable hooks](docs/durability.md).
The durable API is opt-in: the reference MemoryBackend demonstrates ordering,
not crash-persistent storage. The CLI and hosted HTTP handler still use the
ordinary ephemeral loop by default; opt into the MySQL HTTP adapter below.

### Durable hosted MySQL mode (opt-in)

Build with `cargo build --release --features mysql-backend`. Configure the verified
TLS database settings in [MYSQL_BACKEND.md](MYSQL_BACKEND.md), run
`minimal-mcp-agent migrate` once to install the schema, then set
`DURABLE_BACKEND=mysql` and run `minimal-mcp-agent serve`.

Durable `POST /turn` requests use server-owned history:

```json
{"session_id":"project-123","request_id":"turn-001","input":"Inspect the project"}
```

Both IDs must be 1–128 ASCII letters/digits/`_`/`-`. Reuse the same request ID and
input to resume or retrieve its completed result; use a new request ID for the
next turn in that session. Do not send a `messages` array in this mode. A request
that was interrupted during a tool side effect stops as unknown rather than
repeating that tool. Session locks reject competing owners. Never blindly retry
a failed request under a new ID.

`DURABLE_BACKEND=ephemeral` (the default) preserves the original HTTP `messages`
contract. Interactive CLI history remains ephemeral. Backend credentials belong
in secret environment variables, never source control. Database migrations are
explicit and are not silently run on HTTP startup.

## Review a GitHub pull request

In the GitHub repository, add `CHATGPT_ACCESS_TOKEN` as an Actions secret and
`OPENAI_MODEL` as an Actions variable. Use an access token issued by the
runner's Sign in with ChatGPT flow and a model listed for that account. Rotate
the token when it expires. The workflow does not refresh it, so replace the
Actions secret before requesting a review if the token has expired. GitHub
Actions supplies the PR-scoped write credential for the review job.

Create the `wreview` repository label and add it to an open PR after the
workflow is on its base branch. The workflow uses GitHub's
`pull_request_target:labeled` event and checks out the trusted base commit.
It reads changed files through the GitHub API, prepares an isolated mcp-js
snapshot, runs one WASM turn, and publishes a comment-only review. The
review body provides general feedback and inline comments target changed
lines. PR branches, including forks, are read as data.

The Actions run reports the review ID or why publication was skipped. A
rerun reconciles an existing review from the same run. Remove and reapply
`wreview` to request a new review at the current commit.
