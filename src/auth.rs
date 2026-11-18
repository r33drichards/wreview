//! Direct Sign in with ChatGPT. Tokens stay in this application's private local store.
//! See https://developers.openai.com/siwc/token-sharing-open-source/sign-in.
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use fs2::FileExt;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use rand::{RngCore, rngs::OsRng};
use reqwest::{Url, blocking::Client, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const ISSUER: &str = "https://auth.openai.com";
const AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
const RESOURCE: &str = "https://api.openai.com/v1";
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";

#[derive(Serialize, Deserialize, Default)]
struct Store {
    host_id: String,
    active: Option<usize>,
    accounts: Vec<Account>,
}
#[derive(Serialize, Deserialize)]
struct Account {
    client_id: String,
    subject: String,
    email: Option<String>,
    tokens: Option<Tokens>,
}
#[derive(Serialize, Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    id_token: String,
    scope: String,
    expires_at: u64,
}
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    id_token: Option<String>,
    token_type: String,
    expires_in: u64,
    scope: Option<String>,
}
#[derive(Clone, Deserialize)]
struct Claims {
    sub: String,
    email: Option<String>,
    nonce: Option<String>,
    iat: u64,
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}
fn random() -> String {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
fn new_host_id() -> String {
    let mut bytes = [0_u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = format!("{:032x}", u128::from_be_bytes(bytes));
    format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}
fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}
fn client() -> Result<Client> {
    Ok(Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?)
}
fn store_dir() -> Result<PathBuf> {
    #[cfg(not(unix))]
    bail!("This initial version requires Unix owner-only credential permissions.");
    #[cfg(unix)]
    {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        let dir = PathBuf::from(home).join(".config/minimal-mcp-agent");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        let meta = fs::symlink_metadata(&dir)?;
        ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "Credential directory must not be a symlink"
        );
        ensure!(
            meta.permissions().mode() & 0o077 == 0,
            "Credential directory must have owner-only permissions"
        );
        Ok(dir)
    }
}
fn lock(dir: &Path) -> Result<File> {
    let path = dir.join("lock");
    reject_symlink(&path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    options.mode(0o600);
    let lock = options.open(path)?;
    lock.try_lock_exclusive()
        .context("Another agent process is signing in or refreshing; try again shortly")?;
    Ok(lock)
}
fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) => ensure!(
            m.is_file() && !m.file_type().is_symlink(),
            "Credential path must be a regular file"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
fn load(dir: &Path) -> Result<Store> {
    let path = dir.join("accounts.json");
    reject_symlink(&path)?;
    if !path.exists() {
        return Ok(Store {
            host_id: new_host_id(),
            ..Store::default()
        });
    }
    #[cfg(unix)]
    ensure!(
        fs::metadata(&path)?.permissions().mode() & 0o077 == 0,
        "Credential file must have owner-only permissions"
    );
    serde_json::from_slice(&fs::read(path)?).context("Invalid local account store")
}
fn save(dir: &Path, store: &Store) -> Result<()> {
    // Never truncate the live credential file: rotating tokens replace it atomically.
    let temporary = dir.join(format!("accounts-{}.tmp", random()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary)?;
    let outcome = (|| -> Result<()> {
        file.write_all(&serde_json::to_vec(store)?)?;
        file.sync_all()?;
        fs::rename(&temporary, dir.join("accounts.json"))?;
        File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(temporary);
    }
    outcome
}

fn choose_account(store: &Store) -> Result<Option<usize>> {
    if store.accounts.is_empty() {
        return Ok(None);
    }
    println!("Choose a ChatGPT account (registrations may represent different workspaces):");
    for (i, a) in store.accounts.iter().enumerate() {
        println!(
            "  {}. {} [{}]",
            i + 1,
            a.email.as_deref().unwrap_or("ChatGPT account"),
            a.client_id
        );
    }
    println!("  n. Add another ChatGPT account/workspace");
    print!("Account: ");
    std::io::stdout().flush()?;
    let mut choice = String::new();
    std::io::stdin().read_line(&mut choice)?;
    if choice.trim() == "n" {
        return Ok(None);
    }
    let index = choice
        .trim()
        .parse::<usize>()
        .context("Enter an account number or n")?;
    ensure!(
        index > 0 && index <= store.accounts.len(),
        "Invalid account number"
    );
    Ok(Some(index - 1))
}

pub fn login() -> Result<()> {
    let dir = store_dir()?;
    let _lock = lock(&dir)?;
    let mut store = load(&dir)?;
    if store.accounts.is_empty() && store.host_id.starts_with("minimal-mcp-agent:") {
        store.host_id = new_host_id();
    }
    save(&dir, &store)?; // Persist the stable host ID before its first authorization.
    let selected = choose_account(&store)?;
    let old = selected.map(|i| &store.accounts[i]);
    let client_id = old.map_or("dynamic_agent_client", |a| a.client_id.as_str());
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let redirect = format!(
        "http://127.0.0.1:{}/auth/callback",
        listener.local_addr()?.port()
    );
    let state = random();
    let nonce = random();
    let verifier = random();
    let mut url = Url::parse(AUTHORIZE)?;
    url.query_pairs_mut().extend_pairs([
        ("client_id", client_id),
        ("ext_agent_host_id", store.host_id.as_str()),
        ("response_type", "code"),
        ("redirect_uri", redirect.as_str()),
        (
            "scope",
            "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct",
        ),
        ("resource", RESOURCE),
        ("state", state.as_str()),
        ("nonce", nonce.as_str()),
        ("code_challenge_method", "S256"),
        ("code_challenge", challenge(&verifier).as_str()),
    ]);
    if old.is_none() {
        url.query_pairs_mut()
            .append_pair("agent_name_hint", "Minimal MCP Agent");
    }
    // No token-bearing login hints in this printed URL.
    println!("Continue with ChatGPT: open this URL in your browser:\n{url}");
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(url.as_str()).spawn();
    #[cfg(target_os = "linux")]
    let _ = Command::new("xdg-open").arg(url.as_str()).spawn();
    let callback = wait_callback(listener, &state)?;
    let issued = callback_client_id(&callback, old.map(|a| a.client_id.as_str()))?;
    let code = callback
        .get("code")
        .context("Callback did not contain an authorization code")?;
    let http = client()?;
    let response = http
        .post(TOKEN)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", issued.as_str()),
            ("code", code),
            ("code_verifier", &verifier),
            ("redirect_uri", &redirect),
            ("resource", RESOURCE),
        ])
        .send()?;
    let tokens = read_token_response(response)?;
    let id_token = tokens.id_token.as_deref().context("Missing ID token")?;
    let claims = verify_id(&http, id_token, &issued, Some(&nonce))?;
    if let Some(old) = old {
        ensure!(
            claims.sub == old.subject,
            "Sign-in returned a different account; previous credentials retained"
        );
    }
    let account = Account {
        client_id: issued,
        subject: claims.sub,
        email: claims.email,
        tokens: Some(to_tokens(tokens, None)?),
    };
    if let Some(i) = selected {
        store.accounts[i] = account;
        store.active = Some(i);
    } else {
        store.accounts.push(account);
        store.active = Some(store.accounts.len() - 1);
    }
    save(&dir, &store)?;
    println!("Signed in with ChatGPT. Your plan's app usage limits apply.");
    Ok(())
}

fn callback_client_id(query: &HashMap<String, String>, old: Option<&str>) -> Result<String> {
    if let Some(old) = old {
        if let Some(new) = query.get("client_id") {
            ensure!(new == old, "Callback changed the selected client ID");
        }
        return Ok(old.to_owned());
    }
    let issued = query
        .get("client_id")
        .context("Registration callback missing issued client ID")?;
    ensure!(
        !issued.is_empty() && issued != "dynamic_agent_client",
        "Missing issued client ID"
    );
    Ok(issued.clone())
}
fn parse_callback(target: &str, expected_state: &str) -> Result<HashMap<String, String>> {
    ensure!(
        target.starts_with("/auth/callback?"),
        "Unexpected callback path"
    );
    let url = Url::parse(&format!("http://127.0.0.1{target}"))?;
    let mut query = HashMap::new();
    for (k, v) in url.query_pairs() {
        ensure!(
            query.insert(k.into_owned(), v.into_owned()).is_none(),
            "Duplicate callback parameter"
        );
    }
    ensure!(
        query.get("state").map(String::as_str) == Some(expected_state),
        "Invalid OAuth state"
    );
    ensure!(
        !query.contains_key("error"),
        "ChatGPT sign-in was denied or failed"
    );
    Ok(query)
}
fn wait_callback(listener: TcpListener, state: &str) -> Result<HashMap<String, String>> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(300);
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                let mut line = String::new();
                if BufReader::new((&stream).take(8192))
                    .read_line(&mut line)
                    .is_err()
                {
                    continue;
                }
                let parts: Vec<_> = line.split_whitespace().collect();
                let result = if parts.len() == 3 && parts[0] == "GET" {
                    parse_callback(parts[1], state)
                } else {
                    Err(anyhow::anyhow!("Invalid callback request"))
                };
                let body = if result.is_ok() {
                    "Authorization received. Return to your terminal to finish verification."
                } else {
                    "Invalid authorization callback. Return to the terminal."
                };
                let status = if result.is_ok() {
                    "200 OK"
                } else {
                    "400 Bad Request"
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                // Unrelated requests (favicon, stale state) cannot terminate this attempt.
                if result.is_ok() {
                    return result;
                }
                if parts.get(1).is_some_and(|p| {
                    Url::parse(&format!("http://127.0.0.1{p}"))
                        .ok()
                        .is_some_and(|u| {
                            u.query_pairs().any(|(k, v)| k == "state" && v == state)
                                && u.query_pairs().any(|(k, _)| k == "error")
                        })
                }) {
                    return result;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50))
            }
            Err(e) => return Err(e.into()),
        }
    }
    bail!("ChatGPT sign-in timed out after five minutes")
}
fn read_token_response(response: reqwest::blocking::Response) -> Result<TokenResponse> {
    ensure!(
        response.status().is_success(),
        "OAuth token exchange failed (HTTP {}). Run login again if authorization expired",
        response.status()
    );
    // Never include the server's raw response, which may contain credentials, in errors.
    response
        .json()
        .map_err(|_| anyhow::anyhow!("Invalid OAuth token response"))
}
fn to_tokens(response: TokenResponse, previous: Option<&Tokens>) -> Result<Tokens> {
    ensure!(
        response.token_type.eq_ignore_ascii_case("bearer"),
        "Unsupported OAuth token type"
    );
    ensure!(
        !response.access_token.is_empty() && !response.refresh_token.is_empty(),
        "Missing OAuth credentials"
    );
    let scope = response
        .scope
        .or_else(|| previous.map(|p| p.scope.clone()))
        .context("Missing granted OAuth scopes")?;
    ensure!(
        scope.split_whitespace().any(|s| s == PLAN_SCOPE),
        "ChatGPT plan use wasn't granted. Run login and approve plan use"
    );
    let id_token = response
        .id_token
        .or_else(|| previous.map(|p| p.id_token.clone()))
        .context("Missing ID token")?;
    Ok(Tokens {
        access_token: response.access_token,
        refresh_token: response.refresh_token,
        id_token,
        scope,
        expires_at: now()?
            .checked_add(response.expires_in)
            .context("Invalid token expiry")?,
    })
}
fn verify_id(http: &Client, token: &str, client_id: &str, nonce: Option<&str>) -> Result<Claims> {
    let header = decode_header(token).context("Invalid ID token header")?;
    ensure!(
        matches!(header.alg, Algorithm::RS256 | Algorithm::ES256),
        "Unexpected ID token signing algorithm"
    );
    let kid = header.kid.context("Missing signing key identifier")?;
    let keys: JwkSet = http
        .get(format!("{ISSUER}/.well-known/jwks.json"))
        .send()?
        .error_for_status()?
        .json()?;
    let jwk = keys.find(&kid).context("Unknown ID token signing key")?;
    let key = DecodingKey::from_jwk(jwk)?;
    let mut validation = Validation::new(header.alg);
    validation.set_audience(&[client_id]);
    validation.set_issuer(&[ISSUER]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub", "iat"]);
    validation.leeway = 5;
    let claims = decode::<Claims>(token, &key, &validation)
        .context("ID token verification failed")?
        .claims;
    validate_identity(&claims, nonce, now()?)?;
    Ok(claims)
}
fn validate_identity(claims: &Claims, nonce: Option<&str>, timestamp: u64) -> Result<()> {
    ensure!(!claims.sub.is_empty(), "Missing account identity");
    ensure!(claims.iat <= timestamp + 5, "ID token issued in the future");
    if let Some(nonce) = nonce {
        ensure!(
            claims.nonce.as_deref() == Some(nonce),
            "Invalid ID token nonce"
        );
    }
    Ok(())
}

pub fn access_token() -> Result<String> {
    let dir = store_dir()?;
    let _lock = lock(&dir)?;
    let mut store = load(&dir)?;
    let index = store
        .active
        .context("Not signed in. Run: minimal-mcp-agent login")?;
    let account = store
        .accounts
        .get_mut(index)
        .context("Invalid active account")?;
    let previous = account
        .tokens
        .as_ref()
        .context("Signed out. Run: minimal-mcp-agent login")?;
    if previous.expires_at > now()? + 30 {
        return Ok(previous.access_token.clone());
    }
    let http = client()?;
    let response = http
        .post(TOKEN)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", account.client_id.as_str()),
            ("refresh_token", previous.refresh_token.as_str()),
            ("resource", RESOURCE),
        ])
        .send()?;
    let response = read_token_response(response)?;
    if let Some(id) = &response.id_token {
        let claims = verify_id(&http, id, &account.client_id, None)?;
        ensure!(
            claims.sub == account.subject,
            "Refresh changed account identity"
        );
    }
    let next = to_tokens(response, Some(previous))?;
    let access = next.access_token.clone();
    account.tokens = Some(next);
    save(&dir, &store)?;
    Ok(access)
}

pub fn logout() -> Result<()> {
    let dir = store_dir()?;
    let _lock = lock(&dir)?;
    let mut store = load(&dir)?;
    if let Some(index) = store.active {
        let account = store
            .accounts
            .get_mut(index)
            .context("Invalid active account")?;
        if let Some(tokens) = &account.tokens {
            let revoked = revoke(&account.client_id, &tokens.refresh_token);
            account.tokens = None;
            save(&dir, &store)?;
            if revoked.is_err() {
                eprintln!(
                    "Signed out locally; remote revocation was not confirmed. Disconnect this app in ChatGPT Settings."
                );
            } else {
                println!("Signed out. Registration retained for your next sign-in.");
            }
        }
    }
    Ok(())
}
fn revoke(client_id: &str, refresh: &str) -> Result<()> {
    let http = client()?;
    let discovery: Value = http
        .get(format!("{ISSUER}/.well-known/openid-configuration"))
        .send()?
        .error_for_status()?
        .json()?;
    let endpoint = discovery["revocation_endpoint"]
        .as_str()
        .context("Missing revocation endpoint")?;
    let url = Url::parse(endpoint)?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("auth.openai.com")
            && url.username().is_empty()
            && url.password().is_none(),
        "Unexpected revocation endpoint"
    );
    for attempt in 0..3 {
        let result = http
            .post(url.clone())
            .form(&[
                ("client_id", client_id),
                ("token", refresh),
                ("token_type_hint", "refresh_token"),
            ])
            .send();
        match result {
            Ok(response) if response.status() == reqwest::StatusCode::OK => return Ok(()),
            Ok(response) if !response.status().is_server_error() => {
                bail!("Revocation was not confirmed")
            }
            _ => thread::sleep(Duration::from_millis(200 * (1 << attempt))),
        }
    }
    bail!("Revocation was not confirmed")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pkce_matches_rfc7636_vector() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
    #[test]
    fn callbacks_reject_wrong_state_and_duplicates() {
        assert!(parse_callback("/auth/callback?state=bad&code=x", "good").is_err());
        assert!(parse_callback("/auth/callback?state=x&state=x&code=y", "x").is_err());
        assert!(parse_callback("/auth/callback?state=x&error=access_denied", "x").is_err());
        assert!(parse_callback("/other?state=x&code=y", "x").is_err());
        assert_eq!(
            parse_callback("/auth/callback?state=x&code=a%2Bb", "x").unwrap()["code"],
            "a+b"
        );
    }
    #[test]
    fn registration_and_returning_client_ids_are_distinct() {
        assert!(callback_client_id(&HashMap::new(), None).is_err());
        let mut q = HashMap::from([("client_id".into(), "dynamic_agent_client".into())]);
        assert!(callback_client_id(&q, None).is_err());
        q.insert("client_id".into(), "issued".into());
        assert_eq!(callback_client_id(&q, None).unwrap(), "issued");
        assert!(callback_client_id(&q, Some("different")).is_err());
        assert_eq!(
            callback_client_id(&HashMap::new(), Some("issued")).unwrap(),
            "issued"
        );
    }
    #[test]
    fn nonce_and_identity_checked() {
        let mut claims = Claims {
            sub: "person".into(),
            email: None,
            nonce: Some("n".into()),
            iat: 100,
        };
        assert!(validate_identity(&claims, Some("n"), 100).is_ok());
        assert!(validate_identity(&claims, Some("wrong"), 100).is_err());
        claims.iat = 110;
        assert!(validate_identity(&claims, None, 100).is_err());
        claims.iat = 100;
        claims.sub.clear();
        assert!(validate_identity(&claims, None, 100).is_err());
    }
    #[test]
    fn plan_permission_is_required() {
        let response = TokenResponse {
            access_token: "test".into(),
            refresh_token: "test".into(),
            id_token: Some("test".into()),
            token_type: "Bearer".into(),
            expires_in: 3600,
            scope: Some("openid email".into()),
        };
        assert!(to_tokens(response, None).is_err());
    }
    #[test]
    #[cfg(unix)]
    fn credential_writes_are_private_and_atomic() {
        let dir = std::env::temp_dir().join(format!("minimal-mcp-auth-test-{}", random()));
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let store = Store {
            host_id: "test-host".into(),
            ..Store::default()
        };
        save(&dir, &store).unwrap();
        assert_eq!(
            fs::metadata(dir.join("accounts.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(load(&dir).unwrap().host_id, "test-host");
        fs::remove_dir_all(dir).unwrap();
    }
}
