use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    time::Duration,
};

pub fn client() -> Result<Client> {
    Ok(Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(180))
        .build()?)
}

pub fn models(token: &str) -> Result<Vec<Value>> {
    let response = client()?
        .get("https://api.openai.com/v1/models")
        .bearer_auth(token)
        .send()?
        .error_for_status()?;
    let value: Value = response.json()?;
    Ok(value["models"]
        .as_array()
        .context("Missing models catalog")?
        .iter()
        .filter(|m| m["visibility"] == "list")
        .cloned()
        .collect())
}

pub fn prompt(token: &str, model: &str, messages: &[Value], tools: &[Value]) -> Result<Vec<Value>> {
    let response = client()?
        .post("https://api.openai.com/v1/responses")
        .bearer_auth(token)
        .json(&json!({"model": model, "input": messages,
            "tools": tools, "store": false, "stream": true,
            "include": ["reasoning.encrypted_content"]}))
        .send()?
        .error_for_status()?;
    read_completed(BufReader::new(response))
}

// SSE data can span multiple lines. Only a completed response is a successful turn.
fn read_completed(reader: impl BufRead) -> Result<Vec<Value>> {
    let mut data = String::new();
    for line in reader.lines().chain(std::iter::once(Ok(String::new()))) {
        let line = line?;
        if line.is_empty() {
            if data.is_empty() {
                continue;
            }
            let event: Value =
                serde_json::from_str(&data).context("Invalid Responses stream event")?;
            data.clear();
            match event["type"].as_str() {
                Some("response.completed") => {
                    return Ok(event["response"]["output"]
                        .as_array()
                        .context("Completed response missing output")?
                        .clone());
                }
                Some("response.failed" | "response.incomplete" | "error") => {
                    // Do not print the whole event: it can contain conversation data.
                    let kind = event["type"].as_str().unwrap();
                    let code = event["response"]["error"]["code"]
                        .as_str()
                        .or(event["code"].as_str())
                        .unwrap_or("unknown");
                    bail!("Responses stream ended with {kind} ({code})");
                }
                _ => {}
            }
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    bail!("Stream interrupted before response.completed; not retrying automatically")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_completed_sse() {
        let s = "event: response.completed\ndata: {\ndata: \"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n";
        assert!(read_completed(s.as_bytes()).unwrap().is_empty());
    }
    #[test]
    fn fails_closed_for_interruption_or_incomplete() {
        for s in [
            "",
            "data: {\"type\":\"response.incomplete\"}\n\n",
            "data: {\"type\":\"response.failed\"}\n\n",
        ] {
            assert!(read_completed(s.as_bytes()).is_err());
        }
    }
}
