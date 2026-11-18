use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{collections::HashSet, env};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    pub name: String,
    pub url: String,
    /// Name of an environment variable containing this server's OAuth access token.
    pub token_env: Option<String>,
    /// Optional mcp-js logical session, separate from MCP transport session IDs.
    pub session_id_env: Option<String>,
}

pub fn mcp_servers() -> Result<Vec<McpServer>> {
    let servers = if let Ok(value) = env::var("MCP_SERVERS_JSON") {
        parse_servers(&value)?
    } else {
        vec![McpServer {
            name: "default".into(),
            url: env::var("MCP_URL").context("Set MCP_SERVERS_JSON or MCP_URL")?,
            token_env: env::var_os("MCP_TOKEN").map(|_| "MCP_TOKEN".into()),
            session_id_env: env::var_os("MCP_SESSION_ID").map(|_| "MCP_SESSION_ID".into()),
        }]
    };
    validate(&servers)?;
    Ok(servers)
}

fn parse_servers(value: &str) -> Result<Vec<McpServer>> {
    // Errors omit raw JSON so accidentally inline credentials never reach logs.
    let servers =
        serde_json::from_str(value).map_err(|_| anyhow::anyhow!("Invalid MCP_SERVERS_JSON"))?;
    Ok(servers)
}

fn validate(servers: &[McpServer]) -> Result<()> {
    ensure!(
        !servers.is_empty() && servers.len() <= 16,
        "Configure between 1 and 16 MCP servers"
    );
    let mut names = HashSet::new();
    for server in servers {
        ensure!(
            !server.name.is_empty() && names.insert(&server.name),
            "MCP server names must be nonempty and unique"
        );
        let url = reqwest::Url::parse(&server.url).context("Invalid MCP server URL")?;
        if url.scheme() != "https"
            && !(url.scheme() == "http"
                && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")))
        {
            bail!("MCP servers require HTTPS, except HTTP on loopback");
        }
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "MCP URLs must not contain credentials, query strings, or fragments"
        );
        for name in [&server.token_env, &server.session_id_env]
            .into_iter()
            .flatten()
        {
            ensure!(
                !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
                "Invalid MCP token environment variable name"
            );
        }
    }
    Ok(())
}

pub fn bearer(server: &McpServer) -> Result<Option<String>> {
    server
        .token_env
        .as_ref()
        .map(|key| {
            let value = env::var(key)
                .with_context(|| format!("Missing MCP access token environment variable {key}"))?;
            ensure!(
                !value.trim().is_empty() && !value.contains(['\r', '\n']),
                "Invalid MCP access token"
            );
            Ok(value)
        })
        .transpose()
}

pub fn chatgpt_token() -> Result<String> {
    match env::var("CHATGPT_ACCESS_TOKEN") {
        Ok(token) => {
            ensure!(!token.trim().is_empty(), "CHATGPT_ACCESS_TOKEN is empty");
            Ok(token)
        }
        Err(env::VarError::NotPresent) => crate::auth::access_token(),
        Err(_) => bail!("CHATGPT_ACCESS_TOKEN is not valid Unicode"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn multiple_servers_use_environment_references() {
        let servers = parse_servers(r#"[{"name":"one","url":"https://one.example/mcp","token_env":"ONE_TOKEN"},{"name":"two","url":"http://127.0.0.1:8000/mcp"}]"#).unwrap();
        validate(&servers).unwrap();
        assert_eq!(servers[0].token_env.as_deref(), Some("ONE_TOKEN"));
    }
    #[test]
    fn reject_insecure_inline_or_duplicate_configuration() {
        for value in [
            r#"[]"#,
            r#"[{"name":"x","url":"http://remote.example/mcp"}]"#,
            r#"[{"name":"x","url":"https://token@remote.example/mcp"}]"#,
            r#"[{"name":"x","url":"https://remote.example/mcp?token=secret"}]"#,
            r#"[{"name":"x","url":"https://x.example"},{"name":"x","url":"https://y.example"}]"#,
        ] {
            assert!(validate(&parse_servers(value).unwrap()).is_err());
        }
        assert!(
            parse_servers(r#"[{"name":"x","url":"https://x.example","token":"inline-secret"}]"#)
                .is_err()
        );
    }
}
