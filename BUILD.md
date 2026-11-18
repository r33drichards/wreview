# Build for Wasmer / WASIX

The native CLI and the hosted WASIX binary use the same agent loop. Browser OAuth
and Unix credential storage compile only on native Unix. WASIX requires a
`CHATGPT_ACCESS_TOKEN` supplied through Wasmer Secrets.

Verified toolchain: Wasmer 7.5.0, cargo-wasix 0.1.34 (WASIX Rust 1.97), WASI SDK 34.
Use official installation instructions at https://wasix.org/docs/language-guide/rust/installation
and https://docs.wasmer.io/runtime/get-started/ .

```sh
# If the host clang lacks the WASM target, point this at the official WASI SDK:
export CC_wasm32_wasmer_wasi=/path/to/wasi-sdk/bin/clang
python3 scripts/build_wasix.py
wasmer run dist/minimal-mcp-agent.wasm -- --help
python3 tests/mcp_smoke.py --wasmer dist/minimal-mcp-agent.wasm
wasmer package build --check
```

The script copies only source inputs into ignored `.wasix-build/`, verifies the
pinned rmcp 3.10.0 crate archive SHA-256, and changes only its reqwest dependency
from 0.13 to 0.12 plus the renamed rustls feature. The WASIX registry currently
provides reqwest 0.12.22+wasix.1, not 0.13. The native Cargo.toml/Cargo.lock stay
unchanged. `wasix/Cargo.lock` pins the compatibility build. The final artifact is
`dist/minimal-mcp-agent.wasm`; neither it nor package output contains credentials.

## Deployment

After `wasmer login`, confirm the intended account with `wasmer whoami`.
`wasmer.toml` packages only the compiled module; it does not mount this checkout.
`app.yaml` starts `serve` behind HTTPS, without enabling database/volume features.

Before sending real requests, provision secrets through Wasmer's app Settings →
Environment Vars or the supported CLI secret mechanism:

- `RUNNER_API_TOKEN`: independent random secret (at least 32 bytes).
- `CHATGPT_ACCESS_TOKEN`: current direct Sign in with ChatGPT access token.
- `OPENAI_MODEL`: available model slug for that account.
- `MCP_URL`: mcp-js's HTTPS `/mcp` endpoint.
- `MCP_TOKEN`: mcp-js-compatible OAuth/JWT access token, if authentication enabled.
- Optional `MCP_SESSION_ID`: mcp-js's persistent logical JS session name.

Do not paste secrets into shell history, GitHub, app.yaml or this repo. Tokens
provided through env vars are not refreshed automatically; rotate and redeploy.
The application intentionally fails startup while required config is missing.

```sh
wasmer deploy --owner r33drichards --app-name minimal-mcp-agent
```

Check costs/plan before approving any upgrade. A successful package upload is
not evidence of a healthy app; verify `/health`, unauthorized `/turn` rejection,
and an authorized real turn after credentials are provisioned.

## Optional MySQL backend

`python3 scripts/build_wasix.py --features mysql-backend` builds the hosted adapter.
The pinned WASIX registry supplies MySQL 26.0.1+wasix.1; no driver source patch is
needed. Configure verified TLS DB settings as documented in
[MYSQL_BACKEND.md](MYSQL_BACKEND.md), run the explicit schema migration, then set
`DURABLE_BACKEND=mysql`. Do not reuse an unrelated application database. The
HTTP payload requires stable `session_id` and `request_id`, not client messages.

The MySQL integration suite is tested on a disposable MySQL 8.4 fixture, both
natively and under Wasmer. This does not provision or validate a production DB.

## GitHub Actions pipeline

`.github/workflows/wasmer.yml` runs native checks, disposable MySQL 8.4
transaction/recovery tests, a MySQL-enabled WASIX release build, authenticated
local MCP discovery, and package validation. Pull requests only test/build.
Pushes to `main` and **Run workflow** on `main` deploy the tested artifact to
existing app `da_nBBIbtXUDZZG`; deployment runs are serialized and never cancelled
mid-deploy. Other branches cannot deploy.

Create the GitHub `production` environment secret `WASMER_TOKEN` with a newly
rotated Wasmer deployment token. Do not reuse a token exposed in previous CLI
logs. Runtime secrets stay in the existing Wasmer app; CI does not copy or log
them, run production database migrations, or make model/tool calls. Provision
runtime configuration listed above separately. MySQL remains opt-in; CI's
MySQL service is disposable and unrelated to production. The CI transaction
suite does not replace the separately documented verified-TLS integration test.

The artifact includes its commit and SHA-256 manifest. `/health` reports the
compiled `AGENT_COMMIT_SHA` (`unknown` outside CI unless set at build time).
Deployment only succeeds after both the version URL and live app route report
the exact tested commit, and unauthenticated `/turn` returns 401. A package
upload alone cannot pass this gate. CLI output is captured and withheld because
it can contain secret-bearing app configuration; inspect the Wasmer dashboard
for deployment failures. Authorized real turns still require valid model/MCP
credentials and are not implied by a green health check.

Tool installation follows the official [cargo-wasix action](https://github.com/wasix-org/cargo-wasix/blob/v0.1.34/action.yml),
[Wasmer release installer](https://github.com/wasmerio/wasmer/releases/tag/v7.5.0),
and [WASI SDK release](https://github.com/WebAssembly/wasi-sdk/releases/tag/wasi-sdk-34).

## Credential-safe deployment diagnostics

The deployment wrapper first reads the expected app by ID. It prints only fixed
stage names, numeric exit codes and allowlisted text-signature labels; raw CLI
stdout/stderr, app JSON and exception text remain withheld, including on timeouts.
A signature label is evidence that text matched, not a confirmed root cause.
Unknown messages produce `unclassified`. The wrapper does not retry failures.
Timeouts have uncertain outcomes: inspect the app before retrying.

The manual **Read-only Wasmer diagnostics** workflow uses the `production`
secret and runs `python3 scripts/deploy_wasmer.py --diagnose-only`. It only calls
`wasmer app get` for the fixed app ID and verifies the returned identity. It does
not publish, deploy, change app secrets, or call a model/MCP tool. Successful app
read access does not prove publish/deploy permission or runtime readiness.

Tests run without credentials or network access:

```sh
python3 -m unittest discover -s tests -p test_deploy_wasmer.py
```

Attempt 2 of run 43551438316 passed artifact verification and CLI installation,
then `wasmer deploy` returned nonzero. Its raw output was deliberately not retained,
so the cause cannot be recovered from that run. A fixed package version collision
is possible, not established. Wasmer 7.5.0's local-package deploy path publishes
before deploying, and its tag implementation checks version/hash relationships:
[deploy source](https://github.com/wasmerio/wasmer/blob/v7.5.0/lib/cli/src/commands/app/deploy.rs),
[tag source](https://github.com/wasmerio/wasmer/blob/v7.5.0/lib/cli/src/commands/package/tag.rs).
A subsequent read-only comparison verified a concrete version/content conflict:
registry `0.1.0` V3 hash `9701044b4cdc8fc3d9ce58c70f1f9fda19e584cc4595af6cdbf3b8c0e0854c64`
versus original CI artifact repackaged by Wasmer 7.5.0 hash
`50505873c0b682eefbcaf079407e1654170fa20e5707cdaaf78840c727ae3d38`.
The source artifact ZIP digest and its embedded commit/WASM checksum were verified.
This establishes the conflict, not that it was the only cause of attempt 2.

CI now assigns `0.1.0-g<GITHUB_SHA>` in the runner's Wasmer manifest before
deployment. The source manifest and Rust crate version remain unchanged. Repeating
the same commit selects the same package version; a different commit uses a new
version, leaving published `0.1.0` untouched. If an identical commit somehow has
different package bytes, registry conflict detection still fails closed.
