use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Hooks run synchronously, in registration order, at committed-state boundaries.
/// Any error aborts the loop before the next model/tool effect. Hooks must not
/// perform non-idempotent external actions: they may be invoked again on recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    TurnStarted,
    ModelStarted,
    ModelOutput,
    ToolStarted,
    ToolCompleted,
    TurnCompleted,
}

pub trait LifecycleHook {
    fn on_event(&mut self, event: Event, state: &Checkpoint) -> Result<()>;
}
impl<F: FnMut(Event, &Checkpoint) -> Result<()>> LifecycleHook for F {
    fn on_event(&mut self, event: Event, state: &Checkpoint) -> Result<()> {
        self(event, state)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Phase {
    Model,
    Tools,
    Completed,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToolStatus {
    Ready,
    Started,
    Completed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolIntent {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
    pub status: ToolStatus,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    pub request_id: String,
    pub input: String,
    pub messages: Vec<Value>,
    pub phase: Phase,
    pub model_attempts: usize,
    pub max_rounds: usize,
    pub tools: Vec<ToolIntent>,
}
impl Checkpoint {
    pub fn new(request_id: &str, input: &str, mut messages: Vec<Value>, max_rounds: usize) -> Self {
        messages.push(json!({"role":"user","content":input}));
        Self {
            request_id: request_id.into(),
            input: input.into(),
            messages,
            phase: Phase::Model,
            model_attempts: 0,
            max_rounds,
            tools: vec![],
        }
    }
}

/// One engine for ephemeral and durable execution. Recovery uses the checkpoint's
/// phase, never re-prompting a saved model output or repeating a completed tool.
/// A Started tool is ambiguous and stops recovery; no automatic unsafe replay.
pub fn run_with_hooks(
    state: &mut Checkpoint,
    mut prompt_llm: impl FnMut(&[Value]) -> Result<Vec<Value>>,
    mut call_tool: impl FnMut(&str, &str) -> Result<Value>,
    mut print_text: impl FnMut(&str),
    hook: &mut dyn LifecycleHook,
) -> Result<()> {
    loop {
        match state.phase {
            Phase::Completed => return Ok(()),
            Phase::Model => {
                ensure!(
                    state.model_attempts < state.max_rounds,
                    "Model attempt limit reached; saved tool results will not be replayed"
                );
                state.model_attempts += 1;
                hook.on_event(Event::ModelStarted, state)?;
                let output = prompt_llm(&state.messages)?;
                // Validate the entire batch before allowing ANY tool effect.
                let mut ids: std::collections::HashSet<String> = state
                    .messages
                    .iter()
                    .filter(|v| v["type"] == "function_call")
                    .filter_map(|v| v["call_id"].as_str().map(str::to_owned))
                    .collect();
                let mut intents = vec![];
                for item in &output {
                    if item["type"] != "function_call" {
                        continue;
                    }
                    let id = item["call_id"]
                        .as_str()
                        .context("Function call missing call_id")?;
                    ensure!(
                        !id.is_empty() && ids.insert(id.into()),
                        "Duplicate or empty tool call ID"
                    );
                    intents.push(ToolIntent {
                        call_id: id.into(),
                        name: item["name"]
                            .as_str()
                            .context("Function call missing name")?
                            .into(),
                        arguments: item["arguments"]
                            .as_str()
                            .context("Function call missing arguments")?
                            .into(),
                        status: ToolStatus::Ready,
                    });
                }
                state.messages.extend(output.iter().cloned());
                state.tools = intents;
                state.phase = Phase::Tools;
                // Complete output (including reasoning) and all intents precede effects.
                hook.on_event(Event::ModelOutput, state)?;
                for item in output {
                    if item["type"] == "message"
                        && let Some(content) = item["content"].as_array()
                    {
                        for part in content {
                            if let Some(text) = part["text"].as_str().or(part["refusal"].as_str()) {
                                print_text(text);
                            }
                        }
                    }
                }
            }
            Phase::Tools => {
                for index in 0..state.tools.len() {
                    match state.tools[index].status {
                        ToolStatus::Completed => continue,
                        ToolStatus::Started => bail!(
                            "Tool {} has an uncertain outcome; reconcile it before continuing (not replayed)",
                            state.tools[index].call_id
                        ),
                        ToolStatus::Ready => {}
                    }
                    state.tools[index].status = ToolStatus::Started;
                    hook.on_event(Event::ToolStarted, state)?;
                    let tool = &state.tools[index];
                    // Transport errors can follow a remote effect. Leave Started intact.
                    let result = call_tool(&tool.name, &tool.arguments)
                        .context("Tool outcome uncertain; automatic replay is disabled")?;
                    state.messages.push(
                        json!({"type":"function_call_output", "call_id":tool.call_id,
                        "output":serde_json::to_string(&result)?}),
                    );
                    state.tools[index].status = ToolStatus::Completed;
                    hook.on_event(Event::ToolCompleted, state)?;
                }
                if state.tools.is_empty() {
                    state.phase = Phase::Completed;
                    hook.on_event(Event::TurnCompleted, state)?;
                } else {
                    state.phase = Phase::Model;
                }
            }
        }
    }
}

/// Compatibility entry point: existing CLI/HTTP history remains ephemeral.
/// Remote errors are displayed as tool results here, as in the original loop.
pub fn run_turn(
    messages: &mut Vec<Value>,
    input: &str,
    max_rounds: usize,
    prompt_llm: impl FnMut(&[Value]) -> Result<Vec<Value>>,
    mut call_tool: impl FnMut(&str, &str) -> Result<Value>,
    print_text: impl FnMut(&str),
) -> Result<()> {
    let mut state = Checkpoint::new("ephemeral", input, messages.clone(), max_rounds);
    let mut hook = |_: Event, _: &Checkpoint| Ok(());
    hook.on_event(Event::TurnStarted, &state)?;
    let result = run_with_hooks(
        &mut state,
        prompt_llm,
        |name, args| {
            Ok(match call_tool(name, args) {
                Ok(value) => value,
                Err(error) => json!({"isError":true,"error":error.to_string()}),
            })
        },
        print_text,
        &mut hook,
    );
    *messages = state.messages;
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn no_tools_breaks_and_next_turn_keeps_history() {
        let mut history = vec![];
        for input in ["hello", "again"] {
            run_turn(&mut history, input, 3, |_| Ok(vec![json!({"type":"message", "role":"assistant", "content":[{"type":"output_text","text":"hi"}]})]), |_,_| panic!("no tools expected"), |_|{}).unwrap();
        }
        assert_eq!(history.len(), 4);
    }
    #[test]
    fn multiple_calls_errors_and_reasoning_keep_ids_and_order() {
        let mut history = vec![];
        let mut round = 0;
        run_turn(
            &mut history,
            "test",
            3,
            |messages| {
                round += 1;
                if round == 1 {
                    Ok(vec![
                        json!({"type":"reasoning", "encrypted_content":"opaque"}),
                        json!({"type":"function_call","call_id":"a","name":"one","arguments":"{}"}),
                        json!({"type":"function_call","call_id":"b","name":"two","arguments":"{}"}),
                    ])
                } else {
                    assert_eq!(messages[4]["call_id"], "a");
                    assert_eq!(messages[5]["call_id"], "b");
                    Ok(vec![])
                }
            },
            |name, _| {
                if name == "two" {
                    bail!("tool unavailable")
                }
                Ok(json!({"value":1}))
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(round, 2);
        assert!(
            history[5]["output"]
                .as_str()
                .unwrap()
                .contains("tool unavailable")
        );
        assert_eq!(history[1]["encrypted_content"], "opaque");
    }
    #[test]
    fn round_limit_finishes_all_pending_tool_results() {
        let mut history = vec![];
        let result = run_turn(
            &mut history,
            "x",
            1,
            |_| {
                Ok(vec![
                    json!({"type":"function_call","call_id":"a","name":"x","arguments":"{}"}),
                ])
            },
            |_, _| Ok(json!(true)),
            |_| {},
        );
        assert!(result.is_err());
        assert_eq!(history.last().unwrap()["type"], "function_call_output");
    }
}
