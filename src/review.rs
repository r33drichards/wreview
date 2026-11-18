use crate::{config, model};
use anyhow::{Context, Result, ensure};
use minimal_mcp_agent::agent;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    env,
    io::Read,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tiny_http::{Method, Request, Response, Server, StatusCode};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Turn {
    capability: String,
    input: String,
    instructions: String,
    tools: Vec<Value>,
    #[serde(default)]
    async_completion: bool,
}

fn header(request: &Request, name: &'static str) -> Option<String> {
    let mut values = request.headers().iter().filter(|h| h.field.equiv(name));
    let value = values.next()?.value.as_str().to_owned();
    values.next().is_none().then_some(value)
}

fn body(request: &mut Request, max: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    request.as_reader().take(max + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= max, "Request too large");
    Ok(bytes)
}

fn run_turn(turn: Turn, worker: &str, model_name: &str) -> Result<()> {
    ensure!(
        turn.capability.len() == 64 && turn.capability.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid review capability"
    );
    let tools: Vec<_> = turn
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function", "name": tool["name"], "description": tool["description"],
                "parameters": tool["inputSchema"], "strict": false
            })
        })
        .collect();
    let token = config::chatgpt_token()?;
    let client = model::client()?;
    let mut messages = vec![json!({"role": "developer", "content": turn.instructions})];
    agent::run_turn(
        &mut messages,
        &turn.input,
        20,
        |messages| model::prompt(&token, model_name, messages, &tools),
        |name, arguments| {
            let arguments: Value = serde_json::from_str(arguments)?;
            Ok(client
                .post(format!("{worker}/tool"))
                .bearer_auth(&turn.capability)
                .json(&json!({"name": name, "arguments": arguments}))
                .send()?
                .error_for_status()?
                .json()?)
        },
        |_| {},
    )
}

pub fn serve() -> Result<()> {
    let token = env::var("RUNNER_API_TOKEN").context("Set RUNNER_API_TOKEN")?;
    ensure!(
        token.len() >= 32,
        "RUNNER_API_TOKEN must contain at least 32 bytes"
    );
    let worker = env::var("REVIEW_WORKER_URL").context("Set REVIEW_WORKER_URL")?;
    let url = reqwest::Url::parse(&worker)?;
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.path() == "/"
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "REVIEW_WORKER_URL must be an HTTPS origin"
    );
    let worker = worker.trim_end_matches('/').to_owned();
    let model_name = env::var("OPENAI_MODEL").context("Set OPENAI_MODEL")?;
    let port: u16 = env::var("PORT").unwrap_or_else(|_| "8080".into()).parse()?;
    let server = Server::http(("0.0.0.0", port)).map_err(|e| anyhow::anyhow!("{e}"))?;
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()?;
    let busy = Arc::new(AtomicBool::new(false));
    for mut request in server.incoming_requests() {
        let status = match (request.method(), request.url()) {
            (&Method::Get, "/health") => 200,
            (&Method::Post, "/github/webhook") => match (
                header(&request, "X-Hub-Signature-256"),
                header(&request, "X-GitHub-Event"),
                header(&request, "X-GitHub-Delivery"),
            ) {
                (Some(signature), Some(event), Some(delivery)) => {
                    match body(&mut request, 1024 * 1024) {
                        Ok(bytes) => client
                            .post(format!("{worker}/github/webhook"))
                            .header("X-Hub-Signature-256", signature)
                            .header("X-GitHub-Event", event)
                            .header("X-GitHub-Delivery", delivery)
                            .body(bytes)
                            .send()
                            .map_or(502, |response| response.status().as_u16()),
                        Err(_) => 413,
                    }
                }
                _ => 400,
            },
            (&Method::Post, "/turn") => {
                if !header(&request, "Authorization")
                    .is_some_and(|value| crate::server::authorized(&value, &token))
                {
                    401
                } else if busy.swap(true, Ordering::AcqRel) {
                    409
                } else {
                    let busy = Arc::clone(&busy);
                    let worker = worker.clone();
                    let model_name = model_name.clone();
                    std::thread::spawn(move || {
                        let Ok(turn) = body(&mut request, 1024 * 1024)
                            .and_then(|bytes| Ok(serde_json::from_slice::<Turn>(&bytes)?))
                        else {
                            busy.store(false, Ordering::Release);
                            let _ = request.respond(Response::empty(StatusCode(400)));
                            return;
                        };
                        let capability = turn.capability.clone();
                        let pending = if turn.async_completion {
                            if request.respond(Response::empty(StatusCode(202))).is_err() {
                                busy.store(false, Ordering::Release);
                                return;
                            }
                            None
                        } else {
                            Some(request)
                        };
                        let status = match run_turn(turn, &worker, &model_name) {
                            Ok(()) => 200,
                            Err(error) => {
                                eprintln!("Review turn failed: {error}");
                                502
                            }
                        };
                        busy.store(false, Ordering::Release);
                        if let Some(request) = pending {
                            let _ = request.respond(Response::empty(StatusCode(status)));
                        } else {
                            let result = model::client().and_then(|client| {
                                client
                                    .post(format!("{worker}/complete"))
                                    .bearer_auth(capability)
                                    .timeout(Duration::from_secs(5))
                                    .json(&json!({"success": status == 200}))
                                    .send()?
                                    .error_for_status()?;
                                Ok(())
                            });
                            if let Err(error) = result {
                                eprintln!("Review completion callback failed: {error}");
                            }
                        }
                    });
                    continue;
                }
            }
            _ => 404,
        };
        let _ = request.respond(Response::empty(StatusCode(status)));
    }
    Ok(())
}
