# Lifecycle hooks and pluggable durability

`src/agent.rs` owns the state machine; `src/durable.rs` implements persistence as
an ordinary `LifecycleHook`. The engine does not know about a database.

The **CLI and default `/turn` endpoint remain ephemeral**. Select the optional
MySQL HTTP adapter using `DURABLE_BACKEND=mysql`; see [MySQL setup](../MYSQL_BACKEND.md).
The durable API is
opt-in and currently ships a `MemoryBackend` reference implementation, not a
process-persistent storage implementation. It proves ordering, recovery and
same-process request deduplication without choosing an unsafe hosting backend.

```rust
use minimal_mcp_agent::{agent::{Checkpoint, Event}, durable::{MemoryBackend, Turn, run_durable_turn}};
# fn demo() -> anyhow::Result<()> {
let backend = MemoryBackend::default(); // retain outside the request handler
let mut hook = |event: Event, state: &Checkpoint| {
    eprintln!("{event:?}: {}", state.request_id);
    Ok(())
};
let transcript = run_durable_turn(
    &backend,
    Turn { session_id: "conversation-1", request_id: "request-1", input: "Hello", max_rounds: 20 },
    |_messages| Ok(vec![]), // replace with Responses request
    |_name, _arguments| Ok(serde_json::json!({})), // replace with MCP call
    |text| println!("{text}"),
    &mut hook,
)?;
# Ok(())
# }
```

## Event order and failure policy

1. `TurnStarted`: user input exists in checkpoint; persist before any model call.
2. `ModelStarted`: increment and persist bounded attempt counter before model.
3. `ModelOutput`: preserve all output/reasoning and validate/save **all** tool
   intents before printing text or executing any tool.
4. `ToolStarted`: record the specific call ID as started before external effect.
5. `ToolCompleted`: save returned result and original call ID before another tool
   or model call. Calls are sequential.
6. `TurnCompleted`: save completion before returning successful response.

The durability hook runs first, then the caller's hook. Either returning an error
stops immediately before the next effect. Already committed state is not rolled
back if a later hook fails. Hooks may run again and must be idempotent; they are
not a transactional outbox. stdout is not durable delivery: text may be lost
between saving output and printing. The returned transcript is authoritative.

## Resume and deduplication

Retry the same `(session_id, request_id)` and same input/budget to resume. A
completed request returns its saved transcript without repeating model/tool
calls. Changed input/budget under the same ID is rejected. New requests use the
last completed transcript; an unfinished request blocks later requests.

Saved model output is not regenerated. Completed tools are not repeated. A
`Started` tool is **uncertain** and stops recovery, even if it may not actually
have begun. Transport errors similarly leave the tool started: an MCP server
could have applied the effect before the connection failed. This conservative
first version has no automatic replay policy or reconciliation endpoint. Repair
requires an application-specific reconciliation workflow/backend update; don't
start a fresh request as a substitute for determining what happened.

MCP-JS `execute`/`run_js` is not presumed safe to replay. JSON-RPC call IDs alone
are not server-side idempotency. Exactly-once effects require cooperation from
the remote tool or action-specific reconciliation, and are **not claimed** here.
A server-returned MCP `isError` value is a recorded result; a transport `Err` is
uncertain. Model attempts lost to failures consume the request's fixed budget.
There is no timed retry scheduler, credential refresh, or autonomous wakeup.

## Backend contract

Implement `DurableBackend::with_session` and `SessionStore` for your storage.
The backend must provide exclusive ownership for the entire closure (including
model/tool calls), consistent reads, atomic checkpoint commits, and persistence
before `save` returns. A commit error may be ambiguous, so stop and reload from
authoritative storage, never continue from the mutated in-memory checkpoint.
Distributed backends need a real ownership/fencing design that prevents stale
workers from performing effects, not merely optimistic writes after effects.

`MemoryBackend` intentionally uses one try-lock for all sessions. Concurrent
owners fail fast; a poisoned lock fails closed. It is for tests/embedding and
loses everything when the process exits. No SQLite dependency or volume is added.
A SQLite/JSONL file on a shared Wasmer Edge volume is not automatically safe.
An actual restart-durable hosted deployment still needs an appropriate backend
and request adapter, selected and validated separately.

Tests cover every post-commit interruption boundary, failures before each storage
commit, dedup/input conflicts, transcript carryover/isolation, competing owners,
invalid tool batches, bounded failed model attempts, and external-effect-before-
receipt failure. They simulate failures with errors, not real process kill or
filesystem/network fault injection.
