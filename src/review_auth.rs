use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use postgres::Client;
use reqwest::blocking::Client as Http;
use ring::{
    aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use std::{
    env,
    time::{SystemTime, UNIX_EPOCH},
};

const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const AAD: &[u8] = b"wreview/oauth/v1";

#[derive(Deserialize, Serialize)]
struct Session {
    client_id: String,
    access_token: String,
    refresh_token: String,
    expires_at: u64,
    scope: String,
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
    token_type: String,
    scope: Option<String>,
}

impl Session {
    fn refresh(&mut self, http: &Http) -> Result<()> {
        let response = http
            .post("https://auth.openai.com/api/accounts/oauth/token")
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", self.client_id.as_str()),
                ("refresh_token", self.refresh_token.as_str()),
                ("resource", "https://api.openai.com/v1"),
            ])
            .send()?;
        ensure!(
            response.status().is_success(),
            "Model credential refresh rejected (HTTP {}); sign in again to replace the worker session",
            response.status()
        );
        let response: RefreshResponse = response
            .json()
            .map_err(|_| anyhow::anyhow!("Invalid model credential refresh response"))?;
        ensure!(
            response.token_type.eq_ignore_ascii_case("bearer")
                && !response.access_token.is_empty()
                && response.expires_in > 0,
            "Invalid refreshed model credential"
        );
        let scope = response.scope.unwrap_or_else(|| self.scope.clone());
        ensure!(
            scope.split_whitespace().any(|scope| scope == PLAN_SCOPE),
            "ChatGPT plan access was not granted"
        );
        self.scope = scope;
        self.access_token = response.access_token;
        if let Some(token) = response.refresh_token {
            ensure!(!token.is_empty(), "Empty rotated refresh token");
            self.refresh_token = token;
        }
        self.expires_at = now()?
            .checked_add(response.expires_in)
            .context("Invalid model token expiration")?;
        Ok(())
    }
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn decrypt(key: &LessSafeKey, mut data: Vec<u8>) -> Result<Session> {
    let (nonce, data) = data
        .split_at_mut_checked(12)
        .context("Invalid credential envelope")?;
    let nonce = Nonce::try_assume_unique_for_key(nonce)
        .map_err(|_| anyhow::anyhow!("Invalid credential nonce"))?;
    let plaintext = key
        .open_in_place(nonce, Aad::from(AAD), data)
        .map_err(|_| anyhow::anyhow!("Cannot decrypt stored review credentials"))?;
    serde_json::from_slice(plaintext)
        .map_err(|_| anyhow::anyhow!("Invalid stored review credentials"))
}

fn encrypt(key: &LessSafeKey, session: &Session) -> Result<Vec<u8>> {
    let mut nonce = [0; 12];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| anyhow::anyhow!("Cannot generate credential nonce"))?;
    let mut data = serde_json::to_vec(session)?;
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(AAD),
        &mut data,
    )
    .map_err(|_| anyhow::anyhow!("Cannot encrypt review credentials"))?;
    Ok([nonce.as_slice(), &data].concat())
}

/// Serializes provider refreshes across jobs, including rotation of refresh tokens.
/// A model 401 can force refresh only if its rejected token is still current.
pub fn access_token(
    db: &mut Client,
    http: &Http,
    key: &str,
    rejected: Option<&str>,
) -> Result<String> {
    let key = STANDARD
        .decode(key.trim())
        .context("Invalid review credential encryption key")?;
    let key = LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, &key)
            .map_err(|_| anyhow::anyhow!("Invalid review credential encryption key"))?,
    );
    let mut tx = db.transaction()?;
    tx.query_one("SELECT pg_advisory_xact_lock(707039257)", &[])?;
    let row = tx.query_opt(
        "SELECT generation, envelope FROM wreview_model_credential WHERE slot=1",
        &[],
    )?;
    let stored_generation: i64 = row.as_ref().map(|row| row.get(0)).unwrap_or(0);
    let configured = env::var("WREVIEW_OAUTH_GENERATION")
        .ok()
        .map(|value| value.parse::<i64>())
        .transpose()?;
    let mut changed = configured.is_some_and(|generation| generation > stored_generation);
    let generation = configured.unwrap_or(0).max(stored_generation);
    let mut session = if changed {
        let seed =
            env::var("WREVIEW_OAUTH_BOOTSTRAP").context("Missing review OAuth bootstrap secret")?;
        serde_json::from_str::<Session>(&seed)
            .map_err(|_| anyhow::anyhow!("Invalid review OAuth bootstrap secret"))?
    } else {
        let row = row.context("No hosted model session. Configure its OAuth bootstrap secret")?;
        decrypt(&key, row.get(1))?
    };
    ensure!(
        !session.refresh_token.is_empty()
            && !session.client_id.is_empty()
            && session
                .scope
                .split_whitespace()
                .any(|scope| scope == PLAN_SCOPE),
        "Invalid hosted model session"
    );
    let refreshed = session.expires_at <= now()?.saturating_add(60)
        || session.access_token.is_empty()
        || rejected.is_some_and(|token| session.access_token == token);
    if refreshed {
        session.refresh(http)?;
        changed = true;
    }
    if changed {
        tx.execute("INSERT INTO wreview_model_credential(slot, generation, envelope) VALUES(1,$1,$2)
                    ON CONFLICT (slot) DO UPDATE SET generation=excluded.generation, envelope=excluded.envelope",
                   &[&generation, &encrypt(&key, &session)?])?;
    }
    tx.commit()?;
    if refreshed {
        println!(
            "{}",
            serde_json::json!({"event":"model_credential_refreshed"})
        );
    }
    Ok(session.access_token)
}
