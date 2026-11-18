//! MySQL checkpoints with connection-scoped exclusive ownership. No pooling,
//! reconnect, or expiring lease: dropping the connection releases ownership.
//! A lost connection aborts; ambiguous external effects are NEVER auto-replayed.
use crate::{
    agent::Checkpoint,
    durable::{DurableBackend, SessionStore},
};
use anyhow::{Result, anyhow, ensure};
use mysql::{Conn, Opts, OptsBuilder, SslOpts, TxOpts, prelude::Queryable};
use std::{cell::RefCell, path::PathBuf, time::Duration};

pub struct MySqlBackend {
    opts: Opts,
}
impl MySqlBackend {
    /// Wasmer's injected DB_* variables. TLS and hostname verification are
    /// mandatory. DB_SSL_CA optionally trusts the provider's private CA file.
    pub fn from_env() -> Result<Self> {
        let required = |name| std::env::var(name).map_err(|_| anyhow!("Missing {name}"));
        let host = required("DB_HOST")?;
        let port = std::env::var("DB_PORT")
            .unwrap_or_else(|_| "3306".into())
            .parse::<u16>()?;
        let ssl = SslOpts::default()
            .with_root_cert_path(std::env::var_os("DB_SSL_CA").map(PathBuf::from));
        let opts = OptsBuilder::default()
            .ip_or_hostname(Some(host))
            .tcp_port(port)
            .db_name(Some(required("DB_NAME")?))
            .user(Some(required("DB_USERNAME")?))
            .pass(Some(required("DB_PASSWORD")?))
            .ssl_opts(Some(ssl))
            .prefer_socket(false)
            .tcp_connect_timeout(Some(Duration::from_secs(10)))
            .read_timeout(Some(Duration::from_secs(30)))
            .write_timeout(Some(Duration::from_secs(30)));
        Ok(Self::new(opts.into()))
    }
    /// Explicit options for integration testing. Production should use from_env.
    pub fn new(opts: Opts) -> Self {
        // Select explicitly because native reqwest and mysql may enable two providers.
        let _ = rustls::crypto::ring::default_provider().install_default();
        Self { opts }
    }
    /// Run explicitly during setup, not on every turn. Idempotent initial schema.
    pub fn migrate(&self) -> Result<()> {
        let mut conn = Conn::new(self.opts.clone())?;
        for sql in include_str!("../migrations/001_durable.sql")
            .split(';')
            .filter(|s| !s.trim().is_empty())
        {
            conn.query_drop(sql)?;
        }
        Ok(())
    }
}
fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id.len() <= 255,
        "IDs must contain 1..255 UTF-8 bytes"
    );
    Ok(())
}
struct Store {
    conn: RefCell<Conn>,
    session: String,
    lock_name: String,
    fence: u64,
    poisoned: bool,
}
impl Store {
    fn assert_owner(&self, conn: &mut Conn) -> Result<()> {
        let owned: Option<u8> = conn.exec_first(
            "SELECT COALESCE(IS_USED_LOCK(?) = CONNECTION_ID(), 0)",
            (&self.lock_name,),
        )?;
        ensure!(owned == Some(1), "Session ownership lost");
        Ok(())
    }
    fn decode(&self, bytes: Option<Vec<u8>>) -> Result<Option<Checkpoint>> {
        bytes
            .map(|v| serde_json::from_slice(&v).map_err(Into::into))
            .transpose()
    }
}
impl SessionStore for Store {
    fn load(&self, request: &str) -> Result<Option<Checkpoint>> {
        validate_id(request)?;
        let mut conn = self.conn.borrow_mut();
        self.assert_owner(&mut conn)?;
        self.decode(conn.exec_first(
            "SELECT checkpoint FROM agent_checkpoints WHERE session_id=? AND request_id=?",
            (&self.session, request),
        )?)
    }
    fn latest(&self) -> Result<Option<Checkpoint>> {
        let mut conn = self.conn.borrow_mut();
        self.assert_owner(&mut conn)?;
        self.decode(conn.exec_first("SELECT c.checkpoint FROM agent_sessions s JOIN agent_checkpoints c ON c.session_id=s.session_id AND c.request_id=s.latest_request WHERE s.session_id=?", (&self.session,))?)
    }
    fn save(&mut self, state: &Checkpoint) -> Result<()> {
        ensure!(
            !self.poisoned,
            "Previous checkpoint failed; reconnect and recover"
        );
        // Poison first so even a caller catching a transaction error cannot continue.
        self.poisoned = true;
        validate_id(&state.request_id)?;
        let bytes = serde_json::to_vec(state)?;
        let mut conn = self.conn.borrow_mut();
        self.assert_owner(&mut conn)?;
        let mut tx = conn.start_transaction(TxOpts::default())?;
        let fence: Option<u64> = tx.exec_first(
            "SELECT fence FROM agent_sessions WHERE session_id=? FOR UPDATE",
            (&self.session,),
        )?;
        ensure!(fence == Some(self.fence), "Stale session owner fenced");
        tx.exec_drop("INSERT INTO agent_checkpoints (session_id,request_id,checkpoint,fence) VALUES (?,?,?,?) ON DUPLICATE KEY UPDATE checkpoint=VALUES(checkpoint), fence=VALUES(fence)", (&self.session, &state.request_id, bytes, self.fence))?;
        tx.exec_drop(
            "UPDATE agent_sessions SET latest_request=? WHERE session_id=? AND fence=?",
            (&state.request_id, &self.session, self.fence),
        )?;
        tx.commit()?;
        self.poisoned = false;
        Ok(())
    }
}
impl DurableBackend for MySqlBackend {
    fn with_session<T>(
        &self,
        session: &str,
        run: impl FnOnce(&mut dyn SessionStore) -> Result<T>,
    ) -> Result<T> {
        validate_id(session)?;
        let mut conn = Conn::new(self.opts.clone())?;
        // Server hashes the database + session: bounded lock name, binary-safe IDs.
        let lock_name: String = conn
            .exec_first(
                "SELECT SHA2(CONCAT(COALESCE(DATABASE(), ''), CHAR(0), ?), 256)",
                (session,),
            )?
            .ok_or_else(|| anyhow!("Database name required"))?;
        let acquired: Option<u8> =
            conn.exec_first("SELECT COALESCE(GET_LOCK(?, 0), 0)", (&lock_name,))?;
        ensure!(acquired == Some(1), "Session busy");
        let mut tx = conn.start_transaction(TxOpts::default())?;
        tx.exec_drop("INSERT INTO agent_sessions (session_id) VALUES (?) ON DUPLICATE KEY UPDATE session_id=VALUES(session_id)", (session,))?;
        tx.exec_drop(
            "UPDATE agent_sessions SET fence=fence+1 WHERE session_id=?",
            (session,),
        )?;
        let fence = tx
            .exec_first(
                "SELECT fence FROM agent_sessions WHERE session_id=?",
                (session,),
            )?
            .ok_or_else(|| anyhow!("Missing session"))?;
        tx.commit()?;
        let mut store = Store {
            conn: RefCell::new(conn),
            session: session.into(),
            lock_name,
            fence,
            poisoned: false,
        };
        // Conn is dropped on success/error/unwind, releasing GET_LOCK. Never pooled.
        run(&mut store)
    }
}
