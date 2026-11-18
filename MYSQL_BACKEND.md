# MySQL durability backend

Enable `--features mysql-backend`. `MySqlBackend::from_env()` reads Wasmer's
`DB_HOST`, `DB_PORT` (default 3306), `DB_NAME`, `DB_USERNAME`, `DB_PASSWORD`.
TLS certificate-chain and hostname verification are mandatory. `DB_SSL_CA` may
point to a trusted provider CA PEM/DER file. Do not turn verification off if the
managed service's CA or hostname cannot be verified: obtain its CA or stop.
The low-level `new(Opts)` constructor is for explicit application/test config;
it can select plaintext, unlike the secure environment constructor.

Run `migrate()` explicitly once as setup with schema privileges. Runtime needs
SELECT/INSERT/UPDATE on the two InnoDB tables, not general administration. Initial
migration is idempotent and deliberately uses two independent DDL statements;
MySQL DDL implicitly commits. An interrupted initial migration can be rerun.
Future schema changes need versioned migrations, not edits to this initial file.

Each session owns a dedicated, non-pooled MySQL connection and a server GET_LOCK
for its entire turn. Busy sessions reject immediately. No automatic reconnect or
lease expiry occurs. Connection drop releases the lock; network partitions may
hold it until the server notices. This favors safety over availability. A
monotonically increasing session fence is checked under row lock on every save.
The checkpoint (including phase, transcript and tool intents/results) and latest
request pointer commit in ONE transaction. Any save failure poisons the store.
An ambiguous commit aborts; retry the SAME request ID and recover from storage.
IDs are case-sensitive 1..255 UTF-8 bytes. Keep IDs stable across retries.

This requires one authoritative MySQL primary and connection affinity. Do not
use statement-level multiplexing/proxies or multiple independent MySQL servers:
GET_LOCK is connection- and server-scoped, not a distributed/global lock.
Fencing protects DB writes, not external MCP side effects. A lost connection can
coincide with an in-flight effect; persisted Started tools stop for reconciliation
rather than replay. Neither this backend nor MCP provides exactly-once effects.
There is no scheduler: a caller must retry/resume interrupted requests.

Snapshots contain user text and tool results; use database access controls and
avoid secrets in model/tool content. Data is retained indefinitely; capacity,
retention and backup verification are deployment responsibilities. Do not assume
Hobby database backups from generic Wasmer database documentation.

## Local integration tests

Use a disposable MySQL8 database bound only to loopback. Set MYSQL_TEST_URL for
the test-only explicit constructor. Run:

```
cargo test --features mysql-backend --test mysql -- --ignored --skip verified_tls
```

For strict TLS tests also set DB_* and DB_SSL_CA for a test server with a matching
hostname/SAN certificate signed by that test CA, then run the ignored
`verified_tls_env_connection` test. It verifies valid custom-CA connection and
rejection when the CA is missing. Do not use production credentials in test URLs
or command logs. Native tests alone do not establish WASIX/Wasmer Edge support.

For the WASIX integration test binary set `MYSQL_TEST_CA` to the guest-mounted CA
path as well as `DB_SSL_CA`. The test fixture and production environment builder
use TCP-only (`prefer_socket(false)`): MySQL's automatic local Unix-socket
upgrade is not supported by the WASIX runtime. The explicit low-level options
constructor does not override caller choices. Keep database networking private;
never expose a test or production database publicly just to make this work.
