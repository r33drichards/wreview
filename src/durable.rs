//! Durability is a lifecycle hook, not storage calls embedded in the agent loop.
//! MemoryBackend is a reference/test backend, NOT process-crash persistence.
use crate::agent::{Checkpoint, Event, LifecycleHook, Phase, run_with_hooks};
use anyhow::{Result, anyhow, ensure};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Mutex};

/// A backend must hold exclusive ownership of this session for the ENTIRE closure,
/// including remote calls. Acquisition must fail while another owner is active.
/// Persistent implementations must commit atomically before save returns, handle
/// ambiguous write errors conservatively, and fence stale owners before effects.
/// A shared Wasmer volume is NOT automatically a safe implementation.
pub trait DurableBackend {
    fn with_session<T>(
        &self,
        session_id: &str,
        run: impl FnOnce(&mut dyn SessionStore) -> Result<T>,
    ) -> Result<T>;
}
pub trait SessionStore {
    fn load(&self, request_id: &str) -> Result<Option<Checkpoint>>;
    fn latest(&self) -> Result<Option<Checkpoint>>;
    fn save(&mut self, state: &Checkpoint) -> Result<()>;
}

#[derive(Default)]
struct MemorySession {
    runs: BTreeMap<String, Checkpoint>,
    latest: Option<String>,
}
impl SessionStore for MemorySession {
    fn load(&self, request_id: &str) -> Result<Option<Checkpoint>> {
        Ok(self.runs.get(request_id).cloned())
    }
    fn latest(&self) -> Result<Option<Checkpoint>> {
        Ok(self
            .latest
            .as_ref()
            .and_then(|id| self.runs.get(id))
            .cloned())
    }
    fn save(&mut self, state: &Checkpoint) -> Result<()> {
        self.runs.insert(state.request_id.clone(), state.clone());
        self.latest = Some(state.request_id.clone());
        Ok(())
    }
}
/// Serializes all sessions (deliberately simple); try_lock rejects competing owners.
/// Kept outside the request handler, it supports same-process dedup and recovery.
#[derive(Default)]
pub struct MemoryBackend {
    sessions: Mutex<BTreeMap<String, MemorySession>>,
}
impl DurableBackend for MemoryBackend {
    fn with_session<T>(
        &self,
        session_id: &str,
        run: impl FnOnce(&mut dyn SessionStore) -> Result<T>,
    ) -> Result<T> {
        let mut sessions = self
            .sessions
            .try_lock()
            .map_err(|_| anyhow!("Backend busy or poisoned"))?;
        run(sessions.entry(session_id.into()).or_default())
    }
}

pub struct DurabilityHook<'a> {
    pub store: &'a mut dyn SessionStore,
}
impl LifecycleHook for DurabilityHook<'_> {
    fn on_event(&mut self, _: Event, state: &Checkpoint) -> Result<()> {
        self.store.save(state)
    }
}
/// Persistence always precedes user hooks. A user hook failure is fail-closed;
/// already committed checkpoints are not rolled back. Hooks are not an outbox.
struct OrderedHooks<'a> {
    durable: DurabilityHook<'a>,
    other: &'a mut dyn LifecycleHook,
}
impl LifecycleHook for OrderedHooks<'_> {
    fn on_event(&mut self, event: Event, state: &Checkpoint) -> Result<()> {
        self.durable.on_event(event, state)?;
        self.other.on_event(event, state)
    }
}

pub struct Turn<'a> {
    pub session_id: &'a str,
    pub request_id: &'a str,
    pub input: &'a str,
    /// Total model attempts for this request, including attempts lost in crashes.
    pub max_rounds: usize,
}
/// Retry the SAME request ID to resume; changed input/budget is rejected. New
/// requests append to the last completed transcript. An unfinished run blocks
/// later requests. There is no scheduler or autonomous wakeup in this adapter.
pub fn run_durable_turn(
    backend: &impl DurableBackend,
    turn: Turn<'_>,
    prompt_llm: impl FnMut(&[Value]) -> Result<Vec<Value>>,
    call_tool: impl FnMut(&str, &str) -> Result<Value>,
    print_text: impl FnMut(&str),
    hook: &mut dyn LifecycleHook,
) -> Result<Vec<Value>> {
    ensure!(
        !turn.session_id.is_empty() && !turn.request_id.is_empty(),
        "Session and request IDs are required"
    );
    ensure!(turn.max_rounds > 0, "max_rounds must be positive");
    backend.with_session(turn.session_id, |store| {
        let mut state = if let Some(saved) = store.load(turn.request_id)? {
            ensure!(
                saved.input == turn.input && saved.max_rounds == turn.max_rounds,
                "Request ID reused with different input or budget"
            );
            if saved.phase == Phase::Completed {
                return Ok(saved.messages);
            }
            saved
        } else {
            let latest = store.latest()?;
            ensure!(
                latest.as_ref().is_none_or(|s| s.phase == Phase::Completed),
                "Session has an unfinished request; resume it first"
            );
            let messages = latest.map(|s| s.messages).unwrap_or_default();
            let state = Checkpoint::new(turn.request_id, turn.input, messages, turn.max_rounds);
            OrderedHooks {
                durable: DurabilityHook { store },
                other: hook,
            }
            .on_event(Event::TurnStarted, &state)?;
            state
        };
        let mut hooks = OrderedHooks {
            durable: DurabilityHook { store },
            other: hook,
        };
        run_with_hooks(&mut state, prompt_llm, call_tool, print_text, &mut hooks)?;
        Ok(state.messages)
    })
}
