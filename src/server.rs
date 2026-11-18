//! A deliberately small, authenticated HTTP wrapper around the same agent loop.
//! Ephemeral requests own their transcript; durable requests reference backend-owned sessions.
//! The HTTP layer never retries model/tool work or implements persistence itself.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{env, io::Read};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const MAX_BODY_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnRequest {
    input: String,
    #[serde(default)]
    messages: Vec<Value>,
}

#[cfg(any(test, feature = "mysql-backend"))]
const MAX_ID_BYTES: usize = 128;

#[cfg(any(test, feature = "mysql-backend"))]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableTurnRequest {
    session_id: String,
    request_id: String,
    input: String,
}

/// Fail fast before opening upstream MCP/model connections.
pub fn validate_environment() -> Result<()> {
    configuration().map(|_| ())
}

fn configuration() -> Result<(String, u16)> {
    let token = env::var("RUNNER_API_TOKEN")
        .context("Set RUNNER_API_TOKEN to a random secret of at least 32 bytes")?;
    ensure!(
        token.len() >= 32,
        "RUNNER_API_TOKEN must contain at least 32 bytes"
    );
    ensure!(
        !token.bytes().any(|b| b.is_ascii_whitespace()),
        "RUNNER_API_TOKEN must not contain whitespace"
    );
    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "8080".into())
        .parse()
        .context("PORT must be an integer between 1 and 65535")?;
    ensure!(port != 0, "PORT must be between 1 and 65535");
    Ok((token, port))
}

/// Runs sequentially; the handler may own a model client and MCP connections.
/// Never put this behind plain HTTP on the public internet: terminate HTTPS at
/// your hosting proxy. RUNNER_API_TOKEN is required even for local serving.
pub fn serve(mut handler: impl FnMut(&str, &mut Vec<Value>) -> Result<()>) -> Result<()> {
    serve_http(|body| handle_payload(body, &mut handler))
}

/// Durable mode uses the same authenticated POST /turn route, but accepts only
/// {session_id, request_id, input}; client-supplied transcripts are rejected.
///
/// The callback must atomically bind request_id to its input within the session,
/// persist/recover progress, and return the persisted transcript. Duplicate IDs
/// must not repeat completed side effects. It must also enforce session ownership
/// if the deployment serves more than one security principal. This shared-token
/// HTTP layer does not establish separate user identities.
#[cfg(any(test, feature = "mysql-backend"))]
#[cfg_attr(all(test, not(feature = "mysql-backend")), allow(dead_code))]
pub fn serve_durable(
    mut handler: impl FnMut(&str, &str, &str) -> Result<Vec<Value>>,
) -> Result<()> {
    serve_http(|body| handle_durable_payload(body, &mut handler))
}

fn serve_http(mut handle_body: impl FnMut(&[u8]) -> (u16, Value)) -> Result<()> {
    let (token, port) = configuration()?;
    let server = Server::http(("0.0.0.0", port))
        .map_err(|e| anyhow::anyhow!("Could not start HTTP listener: {e}"))?;
    eprintln!("Listening on port {port}; POST /turn requires a bearer token");
    for mut request in server.incoming_requests() {
        if request.method() == &Method::Get && request.url() == "/health" {
            respond(
                request,
                200,
                json!({"status":"ok", "commit": option_env!("AGENT_COMMIT_SHA").unwrap_or("unknown")}),
            );
            continue;
        }
        // Authenticate before revealing endpoint/method details or reading a body.
        let mut authorization = request
            .headers()
            .iter()
            .filter(|h| h.field.equiv("Authorization"));
        let authorized = authorization
            .next()
            .is_some_and(|h| authorized(h.value.as_str(), &token))
            && authorization.next().is_none();
        if !authorized {
            respond(request, 401, json!({"error":"Unauthorized"}));
            continue;
        }
        if request.url() != "/turn" {
            respond(request, 404, json!({"error":"Not found"}));
            continue;
        }
        if request.method() != &Method::Post {
            respond(request, 405, json!({"error":"Use POST /turn"}));
            continue;
        }
        let content_type = request
            .headers()
            .iter()
            .find(|h| h.field.equiv("Content-Type"));
        if !content_type.is_some_and(|h| {
            h.value
                .as_str()
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/json")
        }) {
            respond(
                request,
                415,
                json!({"error":"Content-Type must be application/json"}),
            );
            continue;
        }
        if request.body_length().is_some_and(|n| n > MAX_BODY_BYTES) {
            respond(request, 413, json!({"error":"Request body exceeds 1 MiB"}));
            continue;
        }
        let mut body = Vec::new();
        if request
            .as_reader()
            .take((MAX_BODY_BYTES + 1) as u64)
            .read_to_end(&mut body)
            .is_err()
        {
            respond(request, 400, json!({"error":"Could not read request body"}));
            continue;
        }
        let (status, value) = handle_body(&body);
        respond(request, status, value);
    }
    Ok(())
}

// Compare every expected byte without early exit on a mismatch. Secret length is
// not hidden; the comparison result does not reveal matching token prefixes.
fn authorized(header: &str, expected: &str) -> bool {
    let Some(supplied) = header.strip_prefix("Bearer ") else {
        return false;
    };
    let mut different = supplied.len() ^ expected.len();
    for (i, byte) in expected.bytes().enumerate() {
        different |= usize::from(byte ^ supplied.as_bytes().get(i).copied().unwrap_or(0));
    }
    different == 0
}

fn handle_payload(
    body: &[u8],
    handler: &mut impl FnMut(&str, &mut Vec<Value>) -> Result<()>,
) -> (u16, Value) {
    if body.len() > MAX_BODY_BYTES {
        return (413, json!({"error":"Request body exceeds 1 MiB"}));
    }
    let Ok(mut turn) = serde_json::from_slice::<TurnRequest>(body) else {
        return (
            400,
            json!({"error":"Expected JSON with input string and optional messages array"}),
        );
    };
    if turn.input.trim().is_empty() {
        return (400, json!({"error":"input must not be empty"}));
    }
    if handler(&turn.input, &mut turn.messages).is_err() {
        // The tool may already have acted. Preserve the partial transcript so the
        // client can inspect it; neither client nor proxy should retry blindly.
        eprintln!("Agent turn failed; no automatic retry performed");
        return (
            502,
            json!({"error":"Turn failed. An action may already have completed; inspect messages before retrying.","messages":turn.messages}),
        );
    }
    (200, json!({"messages":turn.messages}))
}

#[cfg(any(test, feature = "mysql-backend"))]
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(any(test, feature = "mysql-backend"))]
fn handle_durable_payload(
    body: &[u8],
    handler: &mut impl FnMut(&str, &str, &str) -> Result<Vec<Value>>,
) -> (u16, Value) {
    if body.len() > MAX_BODY_BYTES {
        return (413, json!({"error":"Request body exceeds 1 MiB"}));
    }
    let Ok(turn) = serde_json::from_slice::<DurableTurnRequest>(body) else {
        return (
            400,
            json!({"error":"Expected JSON containing only session_id, request_id, and input strings; client messages are not accepted"}),
        );
    };
    if !valid_id(&turn.session_id) || !valid_id(&turn.request_id) {
        return (
            400,
            json!({"error":"session_id and request_id must each contain 1 to 128 ASCII letters, digits, underscores, or hyphens"}),
        );
    }
    if turn.input.trim().is_empty() {
        return (400, json!({"error":"input must not be empty"}));
    }
    match handler(&turn.session_id, &turn.request_id, &turn.input) {
        Ok(messages) => (
            200,
            json!({"session_id":turn.session_id,"request_id":turn.request_id,"messages":messages}),
        ),
        Err(_) => {
            eprintln!("Durable agent turn failed; no automatic retry performed");
            (
                502,
                json!({"session_id":turn.session_id,"request_id":turn.request_id,
                "error":"Turn failed or remains incomplete. An action may already have completed. Check durable state before retrying; any recovery must reuse these IDs and the identical input, not create a new request."}),
            )
        }
    }
}

fn respond(request: Request, status: u16, body: Value) {
    let response = Response::from_string(body.to_string())
        .with_status_code(StatusCode(status))
        .with_header(Header::from_bytes("Content-Type", "application/json").expect("static header"))
        .with_header(Header::from_bytes("Cache-Control", "no-store").expect("static header"));
    // A disconnected caller is not a reason to rerun its tool calls.
    if request.respond(response).is_err() {
        eprintln!("HTTP caller disconnected; no automatic retry performed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bearer_token_requires_exact_match() {
        assert!(authorized("Bearer abcdef", "abcdef"));
        for header in [
            "abcdef",
            "Bearer abcdeg",
            "Bearer abcde",
            "Bearer abcdefg",
            "Bearer ",
        ] {
            assert!(!authorized(header, "abcdef"));
        }
    }
    #[test]
    fn rejects_malformed_empty_unknown_and_oversized_requests() {
        let mut handler =
            |_: &str, _: &mut Vec<Value>| -> Result<()> { panic!("invalid request must not run") };
        for input in [
            r#"{}"#,
            r#"{"input":" "}"#,
            r#"{"input":"hi","messages":{}}"#,
            r#"{"input":"hi","url":"https://other"}"#,
        ] {
            assert_eq!(handle_payload(input.as_bytes(), &mut handler).0, 400);
        }
        assert_eq!(
            handle_payload(&vec![b' '; MAX_BODY_BYTES + 1], &mut handler).0,
            413
        );
    }
    #[test]
    fn requests_do_not_share_history() {
        let mut handler = |input: &str, history: &mut Vec<Value>| {
            assert!(history.is_empty());
            history.push(json!({"role":"user","content":input}));
            Ok(())
        };
        for input in ["one", "two"] {
            let body = json!({"input":input}).to_string();
            let (status, response) = handle_payload(body.as_bytes(), &mut handler);
            assert_eq!(status, 200);
            assert_eq!(response["messages"].as_array().unwrap().len(), 1);
            assert_eq!(response["messages"][0]["content"], input);
        }
    }
    #[test]
    fn failure_retains_partial_transcript_but_not_error_details() {
        let mut calls = 0;
        let mut handler = |_: &str, history: &mut Vec<Value>| -> Result<()> {
            calls += 1;
            history.push(json!({"type":"function_call_output","call_id":"id","output":"done"}));
            anyhow::bail!("secret diagnostic")
        };
        let (status, response) = handle_payload(br#"{"input":"do it"}"#, &mut handler);
        assert_eq!(status, 502);
        assert_eq!(calls, 1);
        assert_eq!(response["messages"][0]["output"], "done");
        assert!(!response.to_string().contains("secret diagnostic"));
    }
    #[test]
    fn durable_ids_are_bounded_ascii_and_not_normalized() {
        assert!(valid_id("Session_A-09"));
        assert!(valid_id(&"x".repeat(MAX_ID_BYTES)));
        for id in ["", " ", " x", "x ", "a/b", "a.b", "é", "a\nb"] {
            assert!(!valid_id(id));
        }
        assert!(!valid_id(&"x".repeat(MAX_ID_BYTES + 1)));
    }

    #[test]
    fn durable_payload_rejects_missing_ids_messages_and_invalid_input() {
        let mut handler = |_: &str, _: &str, _: &str| -> Result<Vec<Value>> {
            panic!("invalid durable payload must not execute")
        };
        for body in [
            r#"{}"#,
            r#"{"session_id":"s","input":"hi"}"#,
            r#"{"session_id":"s","request_id":"r","input":"hi","messages":[]}"#,
            r#"{"session_id":"s","request_id":"r","input":42}"#,
            r#"{"session_id":"s","request_id":"r","input":" "}"#,
            r#"{"session_id":"s","request_id":"","input":"hi"}"#,
            r#"{"session_id":"../s","request_id":"r","input":"hi"}"#,
        ] {
            assert_eq!(handle_durable_payload(body.as_bytes(), &mut handler).0, 400);
        }
        assert_eq!(
            handle_durable_payload(&vec![b' '; MAX_BODY_BYTES + 1], &mut handler).0,
            413
        );
    }

    #[test]
    fn durable_duplicates_return_backend_result_without_repeating_work() {
        let mut saved = std::collections::HashMap::new();
        let mut executions = 0;
        let mut handler = |session: &str, request: &str, input: &str| -> Result<Vec<Value>> {
            let key = (session.to_owned(), request.to_owned());
            if let Some((original_input, messages)) = saved.get(&key) {
                ensure!(original_input == input, "request input conflict");
                return Ok(Vec::<Value>::clone(messages));
            }
            executions += 1;
            let messages = vec![json!({"role":"user","content":input})];
            saved.insert(key, (input.to_owned(), messages.clone()));
            Ok(messages)
        };
        let body = br#"{"session_id":"Session_A","request_id":"Request_B","input":"do it"}"#;
        let first = handle_durable_payload(body, &mut handler);
        let second = handle_durable_payload(body, &mut handler);
        assert_eq!(first.0, 200);
        assert_eq!(first, second);
        assert_eq!(first.1["session_id"], "Session_A");
        assert_eq!(first.1["request_id"], "Request_B");
        assert_eq!(executions, 1);
    }

    #[test]
    fn durable_recovery_reuses_identity_and_redacts_backend_error() {
        let mut persisted = None;
        let mut executions = 0;
        let mut handler = |session: &str, request: &str, input: &str| -> Result<Vec<Value>> {
            assert_eq!((session, request, input), ("s", "r", "do it"));
            if let Some(messages) = &persisted {
                return Ok(Vec::<Value>::clone(messages));
            }
            executions += 1;
            persisted = Some(vec![
                json!({"type":"function_call_output","call_id":"call-1","output":"done"}),
            ]);
            anyhow::bail!("private database connection credentials")
        };
        let body = br#"{"session_id":"s","request_id":"r","input":"do it"}"#;
        let (status, error) = handle_durable_payload(body, &mut handler);
        assert_eq!(status, 502);
        assert_eq!(error["session_id"], "s");
        assert_eq!(error["request_id"], "r");
        assert!(error.get("messages").is_none());
        assert!(!error.to_string().contains("credentials"));
        // This is an explicit caller recovery, never a retry inside the HTTP layer.
        let (status, result) = handle_durable_payload(body, &mut handler);
        assert_eq!(status, 200);
        assert_eq!(result["messages"][0]["output"], "done");
        assert_eq!(executions, 1);
    }
}
