use crate::{config, model};
use anyhow::{Context, Result, ensure};
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
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelStep {
    messages: Vec<Value>,
    tools: Vec<Value>,
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

fn run_step(step: ModelStep, model_name: &str) -> Result<Vec<Value>> {
    let tools: Vec<_> = step
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function", "name": tool["name"], "description": tool["description"],
                "parameters": tool["inputSchema"], "strict": false
            })
        })
        .collect();
    model::prompt(
        &config::chatgpt_token()?,
        model_name,
        &step.messages,
        &tools,
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
            (&Method::Post, "/model-step") => {
                if !header(&request, "Authorization")
                    .is_some_and(|value| crate::server::authorized(&value, &token))
                {
                    401
                } else if busy.swap(true, Ordering::AcqRel) {
                    409
                } else {
                    let busy = Arc::clone(&busy);
                    let model_name = model_name.clone();
                    std::thread::spawn(move || {
                        let response = match body(&mut request, 1024 * 1024)
                            .and_then(|bytes| Ok(serde_json::from_slice::<ModelStep>(&bytes)?))
                        {
                            Ok(step) => match run_step(step, &model_name) {
                                Ok(output) => Response::from_string(json!(output).to_string())
                                    .with_header(
                                        Header::from_bytes("Content-Type", "application/json")
                                            .unwrap(),
                                    ),
                                Err(error) => {
                                    eprintln!("Review model step failed: {error}");
                                    Response::from_string("").with_status_code(502)
                                }
                            },
                            Err(_) => Response::from_string("").with_status_code(400),
                        };
                        busy.store(false, Ordering::Release);
                        let _ = request.respond(response);
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
