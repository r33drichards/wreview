use crate::{config, model};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use jsonwebtoken::{Algorithm, EncodingKey, Header as JwtHeader, encode};
use minimal_mcp_agent::agent;
use postgres::{Client, Config, config::SslMode};
use reqwest::blocking::Client as Http;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{
    collections::HashSet,
    env,
    io::Read,
    net::ToSocketAddrs,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tiny_http::{Method, Request, Response, Server, StatusCode};
use tokio_postgres_rustls::MakeRustlsConnect;

const API: &str = "https://api.github.com";
const LIMIT: usize = 3;
const PROMPT: &str = include_str!("../review/prompt.md");

#[derive(Serialize, Deserialize)]
struct Job {
    repo: String,
    number: u64,
    head: String,
    base: String,
}

struct Service {
    db: Config,
    tls: MakeRustlsConnect,
    http: Http,
    owner: String,
    installation: u64,
    webhook_secret: String,
    github_token: String,
    model: String,
    mcp_url: String,
    mcp_token: String,
}

impl Service {
    fn new() -> Result<Self> {
        let get = |name| env::var(name).with_context(|| format!("Missing {name}"));
        let mut db = Config::new();
        let host = get("DB_HOST")?;
        let port: u16 = get("DB_PORT")?.parse()?;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Ok(ca) = env::var("REVIEW_DB_CA_DER_BASE64") {
            roots = rustls::RootCertStore::empty();
            roots.add(STANDARD.decode(ca.trim())?.into())?;
            let address = (host.as_str(), port)
                .to_socket_addrs()?
                .next()
                .context("Database hostname has no address")?;
            db.host(&get("REVIEW_DB_TLS_NAME")?).hostaddr(address.ip());
        } else {
            db.host(&host);
        }
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        db.port(port)
            .dbname(&get("DB_NAME")?)
            .user(&get("DB_USERNAME")?)
            .password(get("DB_PASSWORD")?)
            .ssl_mode(SslMode::Require)
            .connect_timeout(Duration::from_secs(3));
        let mcp_url = reqwest::Url::parse(&get("REVIEW_MCP_URL")?)?;
        ensure!(
            mcp_url.scheme() == "https"
                && mcp_url.host().is_some()
                && mcp_url.username().is_empty()
                && mcp_url.password().is_none()
                && mcp_url.query().is_none()
                && mcp_url.fragment().is_none(),
            "Invalid MCP relay URL"
        );
        Ok(Self {
            db,
            tls: MakeRustlsConnect::new(tls),
            http: Http::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(15))
                .build()?,
            owner: get("GITHUB_APP_REPOSITORY_OWNER")?,
            installation: get("GITHUB_APP_INSTALLATION_ID")?.parse()?,
            webhook_secret: get("GITHUB_APP_WEBHOOK_SECRET")?,
            github_token: get("GITHUB_REVIEW_TOKEN")?,
            model: get("OPENAI_MODEL")?,
            mcp_url: mcp_url.as_str().trim_end_matches('/').into(),
            mcp_token: get("REVIEW_MCP_TOKEN")?,
        })
    }

    fn db(&self) -> Result<Client> {
        self.db
            .connect(self.tls.clone())
            .with_context(|| "Connecting to review database")
    }

    fn github(
        &self,
        method: reqwest::Method,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut request = self
            .http
            .request(method, format!("{API}{path}"))
            .header("User-Agent", "wreview")
            .header("Accept", "application/vnd.github+json")
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        Ok(request.send()?.error_for_status()?.json()?)
    }

    fn get(&self, path: &str) -> Result<Value> {
        self.github(reqwest::Method::GET, path, &self.github_token, None)
    }

    fn pages(&self, path: &str) -> Result<Vec<Value>> {
        let mut result = Vec::new();
        for page in 1..=40 {
            let value = self.get(&format!("{path}?per_page=100&page={page}"))?;
            let batch = value.as_array().context("Invalid GitHub page")?;
            result.extend(batch.iter().cloned());
            if batch.len() < 100 {
                return Ok(result);
            }
        }
        bail!("GitHub pagination limit reached")
    }

    fn decode_event(&self, event: &Value) -> Result<Option<Job>> {
        if event["action"] != "labeled" || event["label"]["name"] != "wreview" {
            return Ok(None);
        }
        let pr = &event["pull_request"];
        let repo = event["repository"]["full_name"]
            .as_str()
            .context("Missing repository")?;
        let (owner, name) = repo.split_once('/').context("Invalid repository")?;
        ensure!(
            owner.eq_ignore_ascii_case(&self.owner)
                && !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                && event["installation"]["id"].as_u64() == Some(self.installation)
                && pr["base"]["repo"]["full_name"] == repo,
            "Repository is outside the installation"
        );
        Ok(Some(Job {
            repo: repo.into(),
            number: pr["number"]
                .as_u64()
                .filter(|n| *n > 0)
                .context("Invalid PR number")?,
            head: sha(&pr["head"]["sha"])?,
            base: sha(&pr["base"]["sha"])?,
        }))
    }

    fn enqueue(&self, id: &str, job: &Job) -> Result<()> {
        ensure!(delivery_id(id), "Invalid GitHub delivery ID");
        self.db()?.execute(
            "INSERT INTO wreview_jobs (delivery, payload) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            &[&id, &serde_json::to_string(job)?],
        )?;
        Ok(())
    }

    fn run(&self, id: &str) -> Result<()> {
        let mut db = self.db()?;
        let Some(row) = db.query_opt(
            "SELECT payload FROM wreview_jobs WHERE delivery=$1 AND NOT done AND next_attempt<=now()",
            &[&id],
        )? else { return Ok(()); };
        let job: Job = serde_json::from_str(row.get(0))?;
        // Session-scoped lock survives transactions and releases if Wasmer kills the job.
        let key = format!("wreview:{}/{}", job.repo, job.number);
        let locked: bool = db
            .query_one(
                "SELECT pg_try_advisory_lock(hashtextextended($1,0))",
                &[&key],
            )?
            .get(0);
        if !locked {
            return Ok(());
        }
        let claimed = db.execute(
            "UPDATE wreview_jobs SET attempts=attempts+1, next_attempt=now()+interval '2 minutes'
             WHERE delivery=$1 AND NOT done AND next_attempt<=now()",
            &[&id],
        )?;
        if claimed == 0 {
            return Ok(());
        }
        match self.review(&mut db, id, job) {
            Ok(()) => {
                db.execute(
                    "UPDATE wreview_jobs SET done=true WHERE delivery=$1",
                    &[&id],
                )?;
            }
            Err(err) => {
                eprintln!("Review {id} will be retried: {err}");
                db.execute(
                    "UPDATE wreview_jobs SET next_attempt=now()+make_interval(mins=>least(30,attempts*2))
                     WHERE delivery=$1", &[&id],
                )?;
            }
        }
        Ok(())
    }

    fn reconcile_events(&self) -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let jwt = encode(
            &JwtHeader::new(Algorithm::RS256),
            &json!({"iat": now.saturating_sub(30), "exp": now+300, "iss": env::var("GITHUB_APP_ID")?}),
            &EncodingKey::from_rsa_pem(env::var("GITHUB_APP_PRIVATE_KEY")?.as_bytes())?,
        )?;
        // Scan a bounded number of pages. Existing jobs are checked before loading
        // delivery detail. The rollout floor prevents reviewing historical labels.
        let floor = env::var("GITHUB_APP_RECONCILE_AFTER")?;
        let mut db = self.db()?;
        let mut path = "/app/hook/deliveries?per_page=100".to_string();
        for _ in 0..10 {
            let response = self
                .http
                .get(format!("{API}{path}"))
                .header("User-Agent", "wreview")
                .header("Accept", "application/vnd.github+json")
                .bearer_auth(&jwt)
                .send()?
                .error_for_status()?;
            let next = response
                .headers()
                .get(reqwest::header::LINK)
                .and_then(|value| value.to_str().ok())
                .and_then(|links| {
                    links.split(',').find_map(|link| {
                        let (url, rel) = link.trim().split_once('>')?;
                        if rel.trim() != r#"; rel="next""# {
                            return None;
                        }
                        url.strip_prefix("<https://api.github.com")
                            .filter(|path| path.starts_with("/app/hook/deliveries?"))
                            .map(str::to_owned)
                    })
                });
            let list: Value = response.json()?;
            let list = list.as_array().context("Invalid GitHub deliveries")?;
            for item in list {
                if item["delivered_at"]
                    .as_str()
                    .is_some_and(|ts| ts <= floor.as_str())
                {
                    return Ok(());
                }
                if item["event"] != "pull_request" || item["action"] != "labeled" {
                    continue;
                }
                let guid = item["guid"]
                    .as_str()
                    .filter(|id| delivery_id(id))
                    .context("Invalid delivery GUID")?;
                if db
                    .query_opt("SELECT 1 FROM wreview_jobs WHERE delivery=$1", &[&guid])?
                    .is_some()
                {
                    continue;
                }
                let id = item["id"].as_u64().context("Invalid delivery number")?;
                let detail = self.github(
                    reqwest::Method::GET,
                    &format!("/app/hook/deliveries/{id}"),
                    &jwt,
                    None,
                )?;
                if let Some(job) = self.decode_event(&detail["request"]["payload"])? {
                    self.enqueue(guid, &job)?;
                }
            }
            let Some(next) = next else {
                break;
            };
            path = next;
        }
        Ok(())
    }
}

fn delivery_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn sha(value: &Value) -> Result<String> {
    Ok(value
        .as_str()
        .filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .context("Invalid commit SHA")?
        .into())
}

fn header(request: &Request, name: &'static str) -> Option<String> {
    let mut values = request.headers().iter().filter(|h| h.field.equiv(name));
    let value = values.next()?.value.as_str().to_owned();
    values.next().is_none().then_some(value)
}

fn authenticate(secret: &str, body: &[u8], signature: Option<String>) -> bool {
    let Some(hex) = signature.as_deref().and_then(|s| s.strip_prefix("sha256=")) else {
        return false;
    };
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let bytes: Option<Vec<u8>> = (0..64)
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect();
    let mut hmac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC supports any key length");
    hmac.update(body);
    bytes.is_some_and(|bytes| hmac.verify_slice(&bytes).is_ok())
}

pub fn migrate() -> Result<()> {
    Service::new()?.migrate()
}

impl Service {
    fn migrate(&self) -> Result<()> {
        self.db()?.batch_execute(
            "BEGIN;
             SELECT pg_advisory_xact_lock(707039256);
             CREATE TABLE IF NOT EXISTS wreview_jobs (
            delivery TEXT PRIMARY KEY, payload TEXT NOT NULL,
            done BOOLEAN NOT NULL DEFAULT FALSE, attempts INTEGER NOT NULL DEFAULT 0,
            next_attempt TIMESTAMPTZ NOT NULL DEFAULT now(),
            received_at TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        ALTER TABLE wreview_jobs ADD COLUMN IF NOT EXISTS received_at TIMESTAMPTZ NOT NULL DEFAULT now();
        COMMIT;",
        )?;
        Ok(())
    }
}

pub fn reconcile() -> Result<()> {
    let service = Service::new()?;
    service.migrate()?;
    if let Err(error) = service.reconcile_events() {
        eprintln!("GitHub reconciliation failed: {error}");
    }
    let rows = service.db()?.query(
        "SELECT delivery FROM wreview_jobs WHERE NOT done AND next_attempt<=now() ORDER BY next_attempt LIMIT 3",
        &[],
    )?;
    thread::scope(|scope| {
        for row in rows {
            let id: String = row.get(0);
            let service = &service;
            scope.spawn(move || {
                if let Err(error) = service.run(&id) {
                    eprintln!("Recovery of {id} failed: {error}");
                }
            });
        }
    });
    Ok(())
}

pub fn serve() -> Result<()> {
    let service = Arc::new(Service::new()?);
    service.migrate()?;
    let active = Arc::new(AtomicUsize::new(0));
    let port: u16 = env::var("PORT").unwrap_or_else(|_| "8080".into()).parse()?;
    let server = Server::http(("0.0.0.0", port)).map_err(|e| anyhow::anyhow!("{e}"))?;
    for mut req in server.incoming_requests() {
        let outcome = match (req.method(), req.url()) {
            (&Method::Get, "/health") => Ok((200, None)),
            (&Method::Post, "/github/webhook") => (|| -> Result<(u16, Option<String>)> {
                let mut body = Vec::new();
                req.as_reader().take(1_048_577).read_to_end(&mut body)?;
                if body.len() > 1_048_576 {
                    return Ok((413, None));
                }
                if !authenticate(
                    &service.webhook_secret,
                    &body,
                    header(&req, "X-Hub-Signature-256"),
                ) {
                    return Ok((401, None));
                }
                if header(&req, "X-GitHub-Event").as_deref() != Some("pull_request") {
                    return Ok((204, None));
                }
                let Ok(event) = serde_json::from_slice(&body) else {
                    return Ok((400, None));
                };
                let job = match service.decode_event(&event) {
                    Ok(Some(job)) => job,
                    Ok(None) => return Ok((204, None)),
                    Err(_) => return Ok((403, None)),
                };
                let Some(id) = header(&req, "X-GitHub-Delivery").filter(|id| delivery_id(id))
                else {
                    return Ok((400, None));
                };
                service.enqueue(&id, &job)?;
                Ok((202, Some(id)))
            })(),
            _ => Ok((404, None)),
        };
        let (status, job) = outcome.unwrap_or_else(|error| {
            eprintln!("Webhook persistence failed: {error}");
            (503, None)
        });
        let _ = req.respond(Response::empty(StatusCode(status)));
        if let Some(id) = job.filter(|_| active.load(Ordering::Relaxed) < LIMIT) {
            active.fetch_add(1, Ordering::Relaxed);
            let active = Arc::clone(&active);
            let service = Arc::clone(&service);
            thread::spawn(move || {
                if let Err(error) = service.run(&id) {
                    eprintln!("Review {id} could not start: {error}");
                }
                active.fetch_sub(1, Ordering::Relaxed);
            });
        }
    }
    Ok(())
}

impl Service {
    fn write(&self, method: reqwest::Method, path: &str, body: Value) -> Result<Value> {
        self.github(method, path, &self.github_token, Some(body))
    }

    fn mcp(&self, path: &str, payload: Option<Value>) -> Result<Value> {
        let url = format!("{}{path}", self.mcp_url);
        let request = match payload {
            Some(payload) => self.http.post(url).json(&payload),
            None => self.http.get(url),
        };
        Ok(request
            .bearer_auth(&self.mcp_token)
            .send()?
            .error_for_status()?
            .json()?)
    }

    fn js(&self, session: &str, code: String, heap: Option<&Value>) -> Result<(String, Value)> {
        let mut input = json!({"session":session,"code":code});
        if let Some(heap) = heap {
            input["heap"] = heap.clone();
        }
        let response = self.mcp("/api/exec", Some(input))?;
        let id = response["execution_id"]
            .as_str()
            .filter(|id| {
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            })
            .context("Invalid MCP execution ID")?;
        let path = format!("/api/executions/{id}");
        for _ in 0..100 {
            let result = self.mcp(&path, None)?;
            match result["status"].as_str() {
                Some("completed") => return Ok((path, result)),
                Some("failed" | "cancelled" | "timed_out") => bail!("MCP execution failed"),
                _ => thread::sleep(Duration::from_millis(200)),
            }
        }
        bail!("MCP execution did not complete")
    }

    fn review(&self, db: &mut Client, id: &str, job: Job) -> Result<()> {
        let post = reqwest::Method::POST;
        let patch = reqwest::Method::PATCH;
        let actor = self.get("/user")?;
        let root = format!("/repos/{}", job.repo);
        let pull = format!("{root}/pulls/{}", job.number);
        let comments = format!("{root}/issues/{}/comments", job.number);
        let reviews = format!("{pull}/reviews");
        let marker = format!("<!-- wreview-delivery:{id} -->");
        let status_marker = format!("<!-- wreview-status:{id} -->");
        let find = |path: &str, suffix: &str| -> Result<Option<Value>> {
            Ok(self.pages(path)?.into_iter().find(|entry| {
                entry["user"]["login"] == actor["login"]
                    && entry["body"]
                        .as_str()
                        .is_some_and(|body| body.ends_with(suffix))
            }))
        };
        let current = || -> Result<bool> {
            let pr = self.get(&pull)?;
            Ok(pr["state"] == "open"
                && pr["head"]["sha"] == job.head
                && pr["base"]["sha"] == job.base
                && pr["labels"]
                    .as_array()
                    .is_some_and(|labels| labels.iter().any(|label| label["name"] == "wreview")))
        };
        let status = match find(&comments, &status_marker)? {
            Some(value) => value,
            None => self.write(
                post.clone(),
                &comments,
                json!({"body":format!("wreview is starting.\n\n{status_marker}")}),
            )?,
        };
        let status_id = status["id"].as_u64().context("Invalid GitHub comment ID")?;
        let status_path = format!("{root}/issues/comments/{status_id}");
        let result = (|| -> Result<Option<Value>> {
            if let Some(review) = find(&reviews, &marker)? {
                return Ok(Some(review));
            }
            if !current()? {
                return Ok(None);
            }
            let files: Vec<_> = self.pages(&format!("{pull}/files"))?.into_iter().map(|file|
                json!({"filename":file["filename"],"status":file["status"],"patch":file["patch"],
                       "previous_filename":file["previous_filename"]})).collect();
            if !current()? {
                return Ok(None);
            }
            let mut review = self.model_review(id, &job, &files)?;
            // Check the lock connection and PR head again before an external write.
            db.simple_query("SELECT 1")?;
            if !current()? {
                return Ok(None);
            }
            let body = review["body"]
                .as_str()
                .context("Missing staged review body")?;
            review["body"] = json!(format!("{body}\n\n{marker}"));
            review["event"] = json!("COMMENT");
            review["commit_id"] = json!(job.head);
            Ok(Some(self.write(post.clone(), &reviews, review)?))
        })();
        let status = match &result {
            Ok(Some(review)) => format!(
                "wreview finished. [Read the review]({}).",
                review["html_url"].as_str().context("Missing review URL")?
            ),
            Ok(None) => "wreview skipped because the pull request changed.".into(),
            Err(_) => {
                "wreview could not finish. Scheduled recovery will retry this delivery.".into()
            }
        };
        self.write(
            patch,
            &status_path,
            json!({"body":format!("{status}\n\n{status_marker}")}),
        )?;
        if result?.is_some() && current()? {
            let newer: bool = db
                .query_one(
                    "SELECT EXISTS (SELECT 1 FROM wreview_jobs AS later
                        JOIN wreview_jobs AS earlier ON earlier.delivery=$1
                        WHERE NOT later.done AND later.delivery<>earlier.delivery
                        AND later.payload=earlier.payload AND later.received_at>earlier.received_at)",
                    &[&id],
                )?
                .get(0);
            if !newer {
                self.github(
                    reqwest::Method::DELETE,
                    &format!("{root}/issues/{}/labels/wreview", job.number),
                    &self.github_token,
                    None,
                )?;
            }
        }
        Ok(())
    }

    fn model_review(&self, id: &str, job: &Job, files: &[Value]) -> Result<Value> {
        let session = format!("wreview-{id}");
        let context = json!({"repo":job.repo,"number":job.number,"head_sha":job.head,"base_sha":job.base,
            "files_available":files.len(),"files_without_patches":files.iter().filter(|f| f["patch"].is_null()).count()});
        let data = serde_json::to_string(&json!({"context":context,"files":files}))?;
        let (_, seed) = self.js(
            &session,
            format!(
                "globalThis.reviewState = JSON.parse({});",
                serde_json::to_string(&data)?
            ),
            None,
        )?;
        let heap = seed
            .get("heap")
            .filter(|v| !v.is_null())
            .context("MCP did not save a review snapshot")?;
        let read = |expression: &str| -> Result<Value> {
            let (path, _) = self.js(
                &session,
                format!("console.log(JSON.stringify({expression}));"),
                Some(heap),
            )?;
            let value = self.mcp(&format!("{path}/output?byte_limit=100000"), None)?;
            Ok(serde_json::from_str(
                value["data"].as_str().context("Missing MCP output")?,
            )?)
        };
        let tools = [
            function(
                "pr_context",
                "Read the verified repository, commit and review coverage.",
                json!({}),
                &[],
            ),
            function(
                "changed_file",
                "Read a changed file by zero-based index and optional patch offset.",
                json!({"index":{"type":"integer"},"offset":{"type":"integer"}}),
                &["index"],
            ),
            function(
                "submit_review",
                "Stage a comment-only review with optional changed-line comments.",
                json!({"body":{"type":"string"},"comments":{"type":"array","items":{"type":"object",
                    "properties":{"path":{"type":"string"},"line":{"type":"integer"},"side":{"enum":["LEFT","RIGHT"]},
                    "body":{"type":"string"}},"required":["path","line","side","body"],"additionalProperties":false}}}),
                &["body", "comments"],
            ),
        ];
        let token = config::chatgpt_token()?;
        let anchors = anchors(files);
        let mut staged = None;
        let mut messages = vec![json!({"role":"developer","content":PROMPT})];
        agent::run_turn(
            &mut messages,
            &format!(
                "Review {} pull request #{} at {}.",
                job.repo, job.number, job.head
            ),
            30,
            |messages| model::prompt(&token, &self.model, messages, &tools),
            |name, args| {
                let args: Value = serde_json::from_str(args)?;
                match name {
                    "pr_context" => read("reviewState.context"),
                    "changed_file" => {
                        let index = args["index"]
                            .as_u64()
                            .filter(|n| *n < files.len() as u64)
                            .context("Invalid file index")?;
                        let offset = args
                            .get("offset")
                            .map(|v| v.as_u64().context("Invalid offset"))
                            .transpose()?
                            .unwrap_or(0);
                        ensure!(
                            offset
                                <= files[index as usize]["patch"]
                                    .as_str()
                                    .unwrap_or("")
                                    .chars()
                                    .count() as u64,
                            "Invalid patch offset"
                        );
                        read(&format!(
                            "(() => {{ const f=reviewState.files[{index}]; const p=Array.from(f.patch ?? ''); const end={offset}+10000; return {{...f,patch:p.slice({offset},end).join(''),next_offset:end<p.length?end:null}}; }})()"
                        ))
                    }
                    "submit_review" => {
                        ensure!(staged.is_none(), "Review already staged");
                        let body = bounded_text(&args["body"], 12000)?;
                        let comments = args["comments"]
                            .as_array()
                            .context("Missing review comments")?;
                        ensure!(comments.len() <= 30, "Too many review comments");
                        let comments: Vec<Value> = comments
                            .iter()
                            .map(|comment| {
                                let path = comment["path"].as_str().context("Missing file path")?;
                                let line = comment["line"].as_u64().context("Invalid line")?;
                                let side = comment["side"].as_str().context("Missing side")?;
                                ensure!(
                                    anchors.contains(&(path, line, side)),
                                    "Review comment is outside changed lines"
                                );
                                let body = bounded_text(&comment["body"], 2000)?;
                                Ok(json!({"path":path,"line":line,"side":side,"body":body}))
                            })
                            .collect::<Result<_>>()?;
                        staged = Some(json!({"body":body,"comments":comments}));
                        Ok(json!({"staged":true}))
                    }
                    _ => bail!("Unknown review tool"),
                }
            },
            |_| {},
        )?;
        staged.context("Model completed without submitting a review")
    }
}

fn function(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"type":"function","name":name,"description":description,"strict":false,
        "parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
}

fn bounded_text(value: &Value, max: usize) -> Result<&str> {
    value
        .as_str()
        .filter(|text| !text.trim().is_empty() && text.chars().count() <= max)
        .context("Invalid review text length")
}

fn anchors(files: &[Value]) -> HashSet<(&str, u64, &'static str)> {
    let mut result = HashSet::new();
    for file in files {
        let Some(path) = file["filename"].as_str() else {
            continue;
        };
        let mut position = None;
        for line in file["patch"].as_str().unwrap_or("").lines() {
            if let Some(hunk) = line.strip_prefix("@@ -") {
                position = hunk.split_once(" +").and_then(|(left, right)| {
                    Some((
                        left.split(',').next()?.parse::<u64>().ok()?,
                        right.split([' ', ',']).next()?.parse::<u64>().ok()?,
                    ))
                });
            } else if let Some((left, right)) = &mut position {
                match line.as_bytes().first() {
                    Some(b'+') => {
                        result.insert((path, *right, "RIGHT"));
                        *right += 1;
                    }
                    Some(b'-') => {
                        result.insert((path, *left, "LEFT"));
                        *left += 1;
                    }
                    Some(b' ') => {
                        *left += 1;
                        *right += 1;
                    }
                    _ => {}
                }
            }
        }
    }
    result
}
