#![cfg(feature = "mysql-backend")]
use anyhow::Result;
use minimal_mcp_agent::{
    agent::{Checkpoint, Phase},
    durable::{DurableBackend, Turn, run_durable_turn},
    durable_mysql::MySqlBackend,
};
use mysql::{Conn, Opts, prelude::Queryable};
use serde_json::json;
fn test_opts() -> Result<Opts> {
    let mut opts =
        mysql::OptsBuilder::from_opts(Opts::from_url(&std::env::var("MYSQL_TEST_URL")?)?);
    if let Ok(ca) = std::env::var("MYSQL_TEST_CA") {
        opts = opts.ssl_opts(Some(
            mysql::SslOpts::default().with_root_cert_path(Some(std::path::PathBuf::from(ca))),
        ));
    }
    Ok(opts.prefer_socket(false).into())
}
fn backend() -> Result<MySqlBackend> {
    Ok(MySqlBackend::new(test_opts()?))
}
// Explicit opt-in: cargo test --features mysql-backend --test mysql -- --ignored
#[test]
#[ignore = "requires isolated MYSQL_TEST_URL database"]
fn mysql_transactions_ownership_recovery() -> Result<()> {
    let b = backend()?;
    b.migrate()?;
    b.migrate()?;
    let session = format!(
        "test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    b.with_session(&session, |store| {
        assert!(backend()?.with_session(&session, |_| Ok(())).is_err());
        assert!(store.latest()?.is_none());
        let state = Checkpoint::new("r1", "hello", vec![], 3);
        store.save(&state)?;
        assert_eq!(store.load("r1")?.unwrap().input, "hello");
        Ok(())
    })?;
    // New backend/connection recovers prior write. Duplicate completed request
    // never invokes model again, and changed input is rejected.
    let mut noop = |_, _: &Checkpoint| Ok(());
    let turn = || Turn {
        session_id: &session,
        request_id: "r1",
        input: "hello",
        max_rounds: 3,
    };
    let first = run_durable_turn(
        &backend()?,
        turn(),
        |_| Ok(vec![json!({"type":"message","content":[]})]),
        |_, _| unreachable!(),
        |_| {},
        &mut noop,
    )?;
    let second = run_durable_turn(
        &backend()?,
        turn(),
        |_| panic!("duplicate model call"),
        |_, _| unreachable!(),
        |_| {},
        &mut noop,
    )?;
    assert_eq!(first, second);
    assert!(
        run_durable_turn(
            &backend()?,
            Turn {
                input: "different",
                ..turn()
            },
            |_| unreachable!(),
            |_, _| unreachable!(),
            |_| {},
            &mut noop
        )
        .is_err()
    );
    // Effect succeeded but response was lost. Restart must not invoke it again.
    let uncertain = || Turn {
        session_id: &session,
        request_id: "r2",
        input: "effect",
        max_rounds: 3,
    };
    assert!(
        run_durable_turn(
            &backend()?,
            uncertain(),
            |_| Ok(vec![
                json!({"type":"function_call","call_id":"c1","name":"write","arguments":"{}"})
            ]),
            |_, _| anyhow::bail!("lost receipt"),
            |_| {},
            &mut noop
        )
        .is_err()
    );
    assert!(
        run_durable_turn(
            &backend()?,
            uncertain(),
            |_| panic!("model replay"),
            |_, _| panic!("unsafe tool replay"),
            |_| {},
            &mut noop
        )
        .is_err()
    );
    // Force a fencing mismatch; save must fail without altering latest/checkpoint.
    b.with_session(&session, |store| {
        let mut other = Conn::new(test_opts()?)?;
        other.exec_drop(
            "UPDATE agent_sessions SET fence=fence+1 WHERE session_id=?",
            (&session,),
        )?;
        let mut bad = Checkpoint::new("r3", "must not commit", vec![], 3);
        bad.phase = Phase::Completed;
        assert!(store.save(&bad).is_err());
        assert!(store.save(&bad).is_err());
        assert!(store.load("r3")?.is_none());
        assert_eq!(store.latest()?.unwrap().request_id, "r2");
        Ok(())
    })?;
    Ok(())
}

#[test]
#[ignore = "requires isolated MYSQL_TEST_URL database with KILL privilege"]
fn connection_loss_fails_closed() -> Result<()> {
    let b = backend()?;
    b.migrate()?;
    let session = format!(
        "kill-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    b.with_session(&session, |store| {
        let state = Checkpoint::new("request", "hello", vec![], 3);
        store.save(&state)?;
        let mut admin = Conn::new(test_opts()?)?;
        let id: u64 = admin
            .exec_first(
                "SELECT IS_USED_LOCK(SHA2(CONCAT(DATABASE(), CHAR(0), ?),256))",
                (&session,),
            )?
            .unwrap();
        admin.query_drop(format!("KILL CONNECTION {id}"))?;
        assert!(store.save(&state).is_err());
        assert!(store.save(&state).is_err());
        Ok(())
    })?;
    backend()?.with_session(&session, |store| {
        assert_eq!(store.latest()?.unwrap().request_id, "request");
        Ok(())
    })
}

#[test]
#[ignore = "requires configured DB_* and DB_SSL_CA for isolated TLS server"]
fn verified_tls_env_connection() -> Result<()> {
    let backend = MySqlBackend::from_env()?;
    backend.migrate()?;
    backend.with_session("tls-test", |_| Ok(()))?;
    // Without the custom CA, this server must not be trusted.
    let opts = mysql::OptsBuilder::default()
        .ip_or_hostname(Some(std::env::var("DB_HOST")?))
        .tcp_port(std::env::var("DB_PORT")?.parse()?)
        .db_name(Some(std::env::var("DB_NAME")?))
        .user(Some(std::env::var("DB_USERNAME")?))
        .pass(Some(std::env::var("DB_PASSWORD")?))
        .ssl_opts(Some(mysql::SslOpts::default()))
        .prefer_socket(false);
    assert!(MySqlBackend::new(opts.into()).migrate().is_err());
    Ok(())
}

#[test]
#[ignore = "requires isolated MYSQL_TEST_URL database with TRIGGER privilege"]
fn failed_latest_update_rolls_back_checkpoint() -> Result<()> {
    let b = backend()?;
    b.migrate()?;
    let mut admin = Conn::new(test_opts()?)?;
    admin.query_drop("DROP TRIGGER IF EXISTS reject_test_checkpoint")?;
    admin.query_drop("CREATE TRIGGER reject_test_checkpoint BEFORE UPDATE ON agent_sessions FOR EACH ROW BEGIN IF NEW.latest_request = 'rollback-test' THEN SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'injected commit failure'; END IF; END")?;
    let result = b.with_session("atomic-rollback-test", |store| {
        let state = Checkpoint::new("rollback-test", "must roll back", vec![], 3);
        assert!(store.save(&state).is_err());
        assert!(store.load("rollback-test")?.is_none());
        assert!(store.latest()?.is_none());
        Ok(())
    });
    admin.query_drop("DROP TRIGGER reject_test_checkpoint")?;
    result
}
