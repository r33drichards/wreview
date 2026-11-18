//! Real local HTTP tests with fake model/tool callbacks. No paid API requests.
//! The MySQL test is opt-in and requires a disposable MYSQL_TEST_URL database.
#[allow(dead_code)]
#[path = "../src/server.rs"]
mod server;

use anyhow::{Context, Result, ensure};
use minimal_mcp_agent::{
    agent::Checkpoint,
    durable::{DurableBackend, MemoryBackend, Turn, run_durable_turn},
};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// Deliberately fake, checked-in test-only credential. Never use in a deployment.
const TEST_TOKEN: &str = "test-only-http-durable-fixture-secret";

fn run_fixture(backend: impl DurableBackend) -> Result<()> {
    let effects = PathBuf::from(std::env::var("HTTP_DURABLE_EFFECTS")?);
    server::serve_durable(|session_id, request_id, input| {
        let mut noop = |_, _: &Checkpoint| Ok(());
        run_durable_turn(
            &backend,
            Turn {
                session_id,
                request_id,
                input,
                max_rounds: 3,
            },
            |messages| {
                if messages
                    .last()
                    .is_some_and(|m| m["type"] == "function_call_output")
                {
                    Ok(vec![
                        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}),
                    ])
                } else {
                    Ok(vec![
                        json!({"type":"function_call","call_id":format!("call-{session_id}-{request_id}"),"name":"fake_effect","arguments":"{}"}),
                    ])
                }
            },
            |_, _| {
                let mut log = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&effects)?;
                writeln!(log, "{session_id}/{request_id}")?;
                log.sync_all()?;
                if input == "uncertain" {
                    anyhow::bail!("private diagnostic: response lost after fake effect");
                }
                Ok(json!({"effect":"done", "process":std::process::id()}))
            },
            |_| {},
            &mut noop,
        )
    })
}

// Test-process entry point, started only by the parent integration test below.
#[test]
#[ignore = "private HTTP fixture; spawned with a test-only secret by the integration tests"]
fn durable_http_fixture() -> Result<()> {
    if std::env::var_os("HTTP_DURABLE_USE_MYSQL").is_some() {
        #[cfg(feature = "mysql-backend")]
        {
            let backend = minimal_mcp_agent::durable_mysql::MySqlBackend::new(
                mysql::Opts::from_url(&std::env::var("MYSQL_TEST_URL")?)?,
            );
            backend.migrate()?;
            return run_fixture(backend);
        }
        #[cfg(not(feature = "mysql-backend"))]
        anyhow::bail!("mysql-backend feature required");
    }
    run_fixture(MemoryBackend::default())
}

struct Fixture {
    process: Child,
    client: Client,
    base: String,
}
impl Fixture {
    fn start(dir: &Path, mysql: bool) -> Result<Self> {
        // Bind briefly to ask the OS for an unused local port.
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args([
                "--exact",
                "durable_http_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("PORT", port.to_string())
            .env("RUNNER_API_TOKEN", TEST_TOKEN)
            .env("HTTP_DURABLE_EFFECTS", dir.join("effects.txt"))
            .env_remove("HTTP_DURABLE_USE_MYSQL")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if mysql {
            command.env("HTTP_DURABLE_USE_MYSQL", "1");
        }
        let process = command.spawn()?;
        let mut fixture = Self {
            process,
            client: Client::builder().timeout(Duration::from_secs(5)).build()?,
            base: format!("http://127.0.0.1:{port}"),
        };
        for _ in 0..100 {
            ensure!(
                fixture.process.try_wait()?.is_none(),
                "HTTP fixture exited before becoming ready"
            );
            if fixture
                .client
                .get(format!("{}/health", fixture.base))
                .send()
                .is_ok_and(|r| r.status().is_success())
            {
                return Ok(fixture);
            }
            thread::sleep(Duration::from_millis(50));
        }
        anyhow::bail!("HTTP fixture did not become ready")
    }
    fn turn(&self, session: &str, request: &str, input: &str) -> Result<(u16, Value)> {
        self.request(
            json!({"session_id":session,"request_id":request,"input":input}),
            true,
        )
    }
    fn request(&self, body: Value, authenticated: bool) -> Result<(u16, Value)> {
        let mut request = self.client.post(format!("{}/turn", self.base)).json(&body);
        if authenticated {
            request = request.bearer_auth(TEST_TOKEN);
        }
        let response = request.send()?;
        Ok((response.status().as_u16(), response.json()?))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}
struct TestDirectory(PathBuf);
impl TestDirectory {
    fn new() -> Result<Self> {
        let suffix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "minimal-agent-http-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
    fn effects(&self) -> usize {
        fs::read_to_string(self.0.join("effects.txt"))
            .unwrap_or_default()
            .lines()
            .count()
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn exercise_http(fixture: &Fixture, dir: &TestDirectory, session: &str) -> Result<Value> {
    let (status, _) = fixture.request(
        json!({"session_id":session,"request_id":"r1","input":"one"}),
        false,
    )?;
    assert_eq!(status, 401);
    assert_eq!(dir.effects(), 0);
    let (status, _) = fixture.request(
        json!({"session_id":session,"request_id":"r1","input":"one","messages":[]}),
        true,
    )?;
    assert_eq!(status, 400);
    assert_eq!(dir.effects(), 0);

    let first = fixture.turn(session, "r1", "one")?;
    assert_eq!(first.0, 200);
    assert_eq!(first.1["session_id"], session);
    assert_eq!(first.1["request_id"], "r1");
    assert_eq!(dir.effects(), 1);
    assert_eq!(fixture.turn(session, "r1", "one")?, first);
    assert_eq!(dir.effects(), 1, "duplicate completed request reran tool");

    let (status, changed) = fixture.turn(session, "r1", "changed")?;
    assert_eq!(status, 502);
    assert_eq!(changed["request_id"], "r1");
    assert_eq!(dir.effects(), 1, "conflicting input ran tool");

    let second = fixture.turn(session, "r2", "two")?;
    assert_eq!(second.0, 200);
    assert_eq!(dir.effects(), 2);
    let before = first.1["messages"].as_array().context("first messages")?;
    let after = second.1["messages"].as_array().context("second messages")?;
    assert!(
        after.starts_with(before),
        "new request lost session history"
    );
    assert_eq!(after.iter().filter(|m| m["role"] == "user").count(), 2);
    assert_eq!(
        fixture.turn(session, "r1", "one")?,
        first,
        "old duplicate returned wrong transcript"
    );
    assert_eq!(dir.effects(), 2);
    Ok(second.1)
}

#[test]
fn authenticated_durable_http_memory_dedup_carryover_and_conflict() -> Result<()> {
    let dir = TestDirectory::new()?;
    let fixture = Fixture::start(&dir.0, false)?;
    exercise_http(&fixture, &dir, "memory-session")?;
    let error = fixture.turn("memory-session", "r3", "uncertain")?;
    assert_eq!(error.0, 502);
    assert_eq!(dir.effects(), 3);
    assert!(!error.1.to_string().contains("private diagnostic"));
    assert_eq!(fixture.turn("memory-session", "r3", "uncertain")?.0, 502);
    assert_eq!(dir.effects(), 3, "ambiguous effect was replayed");
    Ok(())
}

#[cfg(feature = "mysql-backend")]
#[test]
#[ignore = "requires disposable MYSQL_TEST_URL; starts only local HTTP fixtures with fake model/tools"]
fn authenticated_durable_http_mysql_survives_process_restart() -> Result<()> {
    std::env::var("MYSQL_TEST_URL")
        .context("Set MYSQL_TEST_URL to an isolated disposable database")?;
    let dir = TestDirectory::new()?;
    let session = format!(
        "http-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let fixture = Fixture::start(&dir.0, true)?;
    let second = exercise_http(&fixture, &dir, &session)?;
    drop(fixture);
    let fixture = Fixture::start(&dir.0, true)?;
    assert_eq!(fixture.turn(&session, "r2", "two")?, (200, second));
    assert_eq!(dir.effects(), 2, "completed effect replayed after restart");
    assert_eq!(fixture.turn(&session, "r3", "uncertain")?.0, 502);
    assert_eq!(dir.effects(), 3);
    drop(fixture);
    let fixture = Fixture::start(&dir.0, true)?;
    assert_eq!(fixture.turn(&session, "r3", "uncertain")?.0, 502);
    assert_eq!(dir.effects(), 3, "ambiguous effect replayed after restart");
    Ok(())
}
