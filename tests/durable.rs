use anyhow::{Result, bail};
use minimal_mcp_agent::{
    agent::{Checkpoint, Event},
    durable::{DurableBackend, MemoryBackend, SessionStore, Turn, run_durable_turn},
};
use serde_json::{Value, json};
use std::cell::Cell;
fn turn(id: &str) -> Turn<'_> {
    Turn {
        session_id: "s",
        request_id: id,
        input: "hello",
        max_rounds: 4,
    }
}
fn noop(_: Event, _: &Checkpoint) -> Result<()> {
    Ok(())
}
fn output() -> Vec<Value> {
    vec![
        json!({"type":"reasoning","encrypted_content":"opaque"}),
        json!({"type":"function_call","call_id":"a","name":"execute","arguments":"{}"}),
        json!({"type":"function_call","call_id":"b","name":"execute","arguments":"{}"}),
    ]
}

#[test]
fn crash_at_every_boundary_never_blindly_replays_effects() {
    for boundary in [
        Event::TurnStarted,
        Event::ModelStarted,
        Event::ModelOutput,
        Event::ToolStarted,
        Event::ToolCompleted,
        Event::TurnCompleted,
    ] {
        let db = MemoryBackend::default();
        let effects = Cell::new(0);
        let mut stopped = false;
        let mut crash = |event, _: &Checkpoint| {
            if event == boundary && !stopped {
                stopped = true;
                bail!("simulated crash");
            }
            Ok(())
        };
        let first = run_durable_turn(
            &db,
            turn("r"),
            |messages| {
                Ok(
                    if messages.iter().any(|m| m["type"] == "function_call_output") {
                        vec![]
                    } else {
                        output()
                    },
                )
            },
            |_, _| {
                effects.set(effects.get() + 1);
                Ok(json!(true))
            },
            |_| {},
            &mut crash,
        );
        assert!(first.is_err());
        let resumed = run_durable_turn(
            &db,
            turn("r"),
            |messages| {
                Ok(
                    if messages.iter().any(|m| m["type"] == "function_call_output") {
                        vec![]
                    } else {
                        output()
                    },
                )
            },
            |_, _| {
                effects.set(effects.get() + 1);
                Ok(json!(true))
            },
            |_| {},
            &mut noop,
        );
        if boundary == Event::ToolStarted {
            assert!(resumed.unwrap_err().to_string().contains("uncertain"));
            assert_eq!(effects.get(), 0);
        } else {
            let messages = resumed.unwrap();
            assert_eq!(effects.get(), 2);
            assert_eq!(
                messages
                    .iter()
                    .filter(|m| m["type"] == "function_call_output")
                    .count(),
                2
            );
            // Completed duplicate makes zero calls and returns exactly the transcript.
            let duplicate = run_durable_turn(
                &db,
                turn("r"),
                |_| panic!("duplicate model"),
                |_, _| panic!("duplicate tool"),
                |_| {},
                &mut noop,
            )
            .unwrap();
            assert_eq!(messages, duplicate);
        }
    }
}

#[test]
fn lost_tool_receipt_is_uncertain_and_never_replayed() {
    let db = MemoryBackend::default();
    let effects = Cell::new(0);
    let err = run_durable_turn(
        &db,
        turn("r"),
        |_| Ok(output()),
        |_, _| {
            effects.set(effects.get() + 1);
            bail!("connection lost after effect")
        },
        |_| {},
        &mut noop,
    )
    .unwrap_err();
    assert!(err.to_string().contains("uncertain"));
    assert!(
        run_durable_turn(
            &db,
            turn("r"),
            |_| panic!(),
            |_, _| panic!(),
            |_| {},
            &mut noop
        )
        .is_err()
    );
    assert_eq!(effects.get(), 1);
    assert!(
        run_durable_turn(
            &db,
            turn("new"),
            |_| panic!(),
            |_, _| panic!(),
            |_| {},
            &mut noop
        )
        .is_err()
    );
}

#[test]
fn dedup_conflicts_sessions_and_transcript_carryover() {
    let db = MemoryBackend::default();
    run_durable_turn(
        &db,
        turn("one"),
        |_| Ok(vec![]),
        |_, _| panic!(),
        |_| {},
        &mut noop,
    )
    .unwrap();
    run_durable_turn(
        &db,
        turn("two"),
        |messages| {
            assert_eq!(messages.len(), 2);
            Ok(vec![])
        },
        |_, _| panic!(),
        |_| {},
        &mut noop,
    )
    .unwrap();
    let mut changed = turn("one");
    changed.input = "different";
    assert!(
        run_durable_turn(
            &db,
            changed,
            |_| panic!(),
            |_, _| panic!(),
            |_| {},
            &mut noop
        )
        .is_err()
    );
    let mut isolated = turn("one");
    isolated.session_id = "other";
    run_durable_turn(
        &db,
        isolated,
        |messages| {
            assert_eq!(messages.len(), 1);
            Ok(vec![])
        },
        |_, _| panic!(),
        |_| {},
        &mut noop,
    )
    .unwrap();
}

#[test]
fn competing_owner_cannot_enter_even_during_model_call() {
    let db = MemoryBackend::default();
    run_durable_turn(
        &db,
        turn("r"),
        |_| {
            assert!(
                run_durable_turn(
                    &db,
                    turn("r"),
                    |_| panic!(),
                    |_, _| panic!(),
                    |_| {},
                    &mut noop
                )
                .is_err()
            );
            Ok(vec![])
        },
        |_, _| panic!(),
        |_| {},
        &mut noop,
    )
    .unwrap();
}

#[test]
fn bounded_model_retries_and_invalid_batch_has_no_effects() {
    let db = MemoryBackend::default();
    for _ in 0..4 {
        assert!(
            run_durable_turn(
                &db,
                turn("r"),
                |_| bail!("expired token"),
                |_, _| panic!(),
                |_| {},
                &mut noop
            )
            .is_err()
        );
    }
    assert!(
        run_durable_turn(
            &db,
            turn("r"),
            |_| panic!("budget exhausted"),
            |_, _| panic!(),
            |_| {},
            &mut noop
        )
        .is_err()
    );
    let db = MemoryBackend::default();
    assert!(
        run_durable_turn(
            &db,
            turn("r"),
            |_| {
                let mut items = output();
                items.push(items[1].clone());
                Ok(items)
            },
            |_, _| panic!("invalid batch must not execute"),
            |_| {},
            &mut noop
        )
        .is_err()
    );
}

// Inject failed persistence BEFORE each atomic save. No later external effect
// may occur, even when preceding model/tool actions already happened.
struct FailingBackend {
    inner: MemoryBackend,
    fail_at: usize,
    writes: Cell<usize>,
}
struct FailingStore<'a> {
    inner: &'a mut dyn SessionStore,
    fail_at: usize,
    writes: &'a Cell<usize>,
}
impl SessionStore for FailingStore<'_> {
    fn load(&self, id: &str) -> Result<Option<Checkpoint>> {
        self.inner.load(id)
    }
    fn latest(&self) -> Result<Option<Checkpoint>> {
        self.inner.latest()
    }
    fn save(&mut self, state: &Checkpoint) -> Result<()> {
        self.writes.set(self.writes.get() + 1);
        if self.writes.get() == self.fail_at {
            bail!("storage unavailable");
        }
        self.inner.save(state)
    }
}
impl DurableBackend for FailingBackend {
    fn with_session<T>(
        &self,
        id: &str,
        run: impl FnOnce(&mut dyn SessionStore) -> Result<T>,
    ) -> Result<T> {
        self.inner.with_session(id, |store| {
            run(&mut FailingStore {
                inner: store,
                fail_at: self.fail_at,
                writes: &self.writes,
            })
        })
    }
}
#[test]
fn persistence_failure_stops_before_next_effect_and_before_other_hooks() {
    // input, model start, output, tool A start, tool A result, tool B start, tool B result
    for (fail_at, expected_effects) in [(1, 0), (2, 0), (3, 0), (4, 0), (5, 1), (6, 1), (7, 2)] {
        let db = FailingBackend {
            inner: MemoryBackend::default(),
            fail_at,
            writes: Cell::new(0),
        };
        let effects = Cell::new(0);
        let hooks = Cell::new(0);
        let mut hook = |_: Event, _: &Checkpoint| {
            hooks.set(hooks.get() + 1);
            Ok(())
        };
        assert!(
            run_durable_turn(
                &db,
                turn("r"),
                |_| Ok(output()),
                |_, _| {
                    effects.set(effects.get() + 1);
                    Ok(json!(true))
                },
                |_| {},
                &mut hook
            )
            .is_err()
        );
        assert_eq!(effects.get(), expected_effects);
        assert_eq!(hooks.get(), fail_at - 1);
    }
}

#[test]
fn hooks_observe_required_order_and_complete_reasoning() {
    let db = MemoryBackend::default();
    let mut events = vec![];
    let mut hook = |event, state: &Checkpoint| {
        if event == Event::ModelOutput && !state.tools.is_empty() {
            assert_eq!(state.messages[1]["encrypted_content"], "opaque");
            assert_eq!(state.tools.len(), 2);
        }
        events.push(event);
        Ok(())
    };
    run_durable_turn(
        &db,
        turn("r"),
        |messages| {
            Ok(if messages.len() == 1 {
                output()
            } else {
                vec![]
            })
        },
        |_, _| Ok(json!(true)),
        |_| {},
        &mut hook,
    )
    .unwrap();
    assert_eq!(
        events,
        vec![
            Event::TurnStarted,
            Event::ModelStarted,
            Event::ModelOutput,
            Event::ToolStarted,
            Event::ToolCompleted,
            Event::ToolStarted,
            Event::ToolCompleted,
            Event::ModelStarted,
            Event::ModelOutput,
            Event::TurnCompleted
        ]
    );
}
