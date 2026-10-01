//! One PostgreSQL server shared by every test process of a test run.
//!
//! nextest runs each test in its own process, so a per-process container
//! (what a `static` testcontainers handle gives) means one container per
//! test. Instead, the first process to call [`server`] starts a container
//! named after the run, and every other process of the run finds it under
//! that name and reuses it:
//!
//! - the run is identified by `NEXTEST_RUN_ID`, or by the parent process
//!   (`cargo test` / nextest) when that is unset;
//! - start-or-reuse happens under a cross-process file lock ([`run_lock`]);
//! - a detached watchdog removes the container (and the run's lock files)
//!   once the parent process exits, and Docker's `--rm` deletes it then.
//!
//! Tests get isolation from separate databases rather than separate
//! servers: [`PgServer::create_database`] makes a fresh one per test, and
//! suites sharing a database (the e2e crate) keep their rows apart with
//! unique keys.
//!
//! The image is `pgvector/pgvector:pg18` (the official image plus the
//! `vector` extension the e2e migrations need), with PGDATA on tmpfs and
//! TLS on a self-signed certificate (so TLS fallback paths can be tested;
//! plain connections are still accepted).

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const IMAGE: &str = "pgvector/pgvector:pg18";
const USER: &str = "postgres";
const PASSWORD: &str = "postgres";

/// The shared server of this test run.
#[derive(Debug)]
pub struct PgServer {
    host: String,
    port: u16,
}

/// The run's server, started by the first caller of the run and reused by
/// every later one (in this process or another).
///
/// # Panics
///
/// If Docker is unavailable or the container does not become ready.
pub fn server() -> &'static PgServer {
    static SERVER: OnceLock<PgServer> = OnceLock::new();
    SERVER.get_or_init(start_or_reuse)
}

/// What identifies this test run: every test process of one
/// `cargo nextest run` / `cargo test` invocation agrees on it.
fn run_key() -> String {
    match std::env::var("NEXTEST_RUN_ID") {
        Ok(id) if !id.is_empty() => id.chars().filter(char::is_ascii_alphanumeric).collect(),
        _ => format!("ppid{}", parent_pid()),
    }
}

fn parent_pid() -> i32 {
    // SAFETY: getppid has no preconditions and cannot fail.
    unsafe { libc::getppid() }
}

fn container_name() -> String {
    format!("typedpg-test-{}", run_key())
}

fn lock_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("typedpg-test-{}-{name}.lock", run_key()))
}

/// An exclusive lock shared by every process of the test run, released on
/// drop. Use it to serialize one-time setup (creating and migrating a
/// shared database) across test processes.
pub struct RunLock {
    file: File,
}

impl Drop for RunLock {
    fn drop(&mut self) {
        // SAFETY: the fd is owned by `self.file`, still open here.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Take the run-wide lock called `name`, blocking until it is free.
pub fn run_lock(name: &str) -> RunLock {
    let path = lock_path(name);
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .unwrap_or_else(|e| panic!("open lock file {}: {e}", path.display()));
    // SAFETY: the fd is owned by `file`, open for the call's duration.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    assert_eq!(rc, 0, "flock {}", path.display());
    RunLock { file }
}

/// Run `setup` once per test run, whichever process gets there first; every
/// other caller (in any process) waits for it to finish, then skips it.
/// A setup that panics is retried by the next caller.
pub async fn once_per_run<F>(name: &str, setup: impl FnOnce() -> F)
where
    F: std::future::Future<Output = ()>,
{
    let _lock = run_lock(name);
    let done = std::env::temp_dir().join(format!("typedpg-test-{}-{name}.done", run_key()));
    if done.exists() {
        return;
    }
    setup().await;
    std::fs::write(&done, b"").expect("record the setup as done");
}

fn docker(args: &[&str]) -> std::process::Output {
    Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("run docker {}: {e} — is Docker installed?", args.join(" ")))
}

fn start_or_reuse() -> PgServer {
    let name = container_name();
    let _lock = run_lock("server");

    // The process that started the server recorded its port: reusing it
    // then costs no Docker call at all.
    let port_file = std::env::temp_dir().join(format!("typedpg-test-{}-server.port", run_key()));
    if let Some(port) = std::fs::read_to_string(&port_file)
        .ok()
        .and_then(|s| s.trim().parse().ok())
    {
        return PgServer {
            host: "127.0.0.1".into(),
            port,
        };
    }

    let running = docker(&["inspect", "-f", "{{.State.Running}}", &name]);
    if !(running.status.success() && String::from_utf8_lossy(&running.stdout).trim() == "true") {
        // A stopped leftover with this name would block `docker run`.
        let _ = docker(&["rm", "-f", &name]);
        let out = docker(&[
            "run",
            "-d",
            "--rm",
            "--name",
            &name,
            "--label",
            "typedpg-test=1",
            "-p",
            "127.0.0.1::5432",
            "--tmpfs",
            "/var/lib/postgresql",
            "-e",
            &format!("POSTGRES_USER={USER}"),
            "-e",
            &format!("POSTGRES_PASSWORD={PASSWORD}"),
            IMAGE,
            "-c",
            "fsync=off",
            "-c",
            "max_connections=500",
            "-c",
            "ssl=on",
            "-c",
            "ssl_cert_file=/etc/ssl/certs/ssl-cert-snakeoil.pem",
            "-c",
            "ssl_key_file=/etc/ssl/private/ssl-cert-snakeoil.key",
        ]);
        assert!(
            out.status.success(),
            "docker run {IMAGE}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        spawn_watchdog(&name);
    }
    wait_ready(&name);

    let out = docker(&["port", &name, "5432/tcp"]);
    let mapping = String::from_utf8_lossy(&out.stdout);
    let port: u16 = mapping
        .lines()
        .find_map(|l| l.rsplit(':').next()?.trim().parse().ok())
        .unwrap_or_else(|| panic!("no host port for {name}: {mapping:?}"));
    std::fs::write(&port_file, port.to_string()).expect("record the server port");
    PgServer {
        host: "127.0.0.1".into(),
        port,
    }
}

/// The entrypoint first runs initdb behind a server listening on the unix
/// socket only, then restarts it on TCP: ready means TCP answers.
fn wait_ready(name: &str) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let out = docker(&[
            "exec",
            name,
            "pg_isready",
            "-q",
            "-h",
            "127.0.0.1",
            "-U",
            USER,
        ]);
        if out.status.success() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{name} not ready after 120s: {}",
            String::from_utf8_lossy(&docker(&["logs", "--tail", "20", name]).stderr)
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Remove the container once the process driving the test run exits. The
/// watchdog leaves the test's session (so a test timeout killing the
/// process group does not take it along), closes every stdio pipe (so the
/// test runner does not wait on it) and is backgrounded by a shell that
/// exits at once, which reparents it away from this short-lived process.
fn spawn_watchdog(name: &str) {
    use std::os::unix::process::CommandExt;
    let script = r#"(while kill -0 "$1" 2>/dev/null; do sleep 1; done
docker rm -f "$2" >/dev/null 2>&1
rm -f "$3"-*) &"#;
    let lock_prefix = std::env::temp_dir().join(format!("typedpg-test-{}", run_key()));
    let mut cmd = Command::new("sh");
    cmd.args(["-c", script, "typedpg-test-watchdog"])
        .arg(parent_pid().to_string())
        .arg(name)
        .arg(lock_prefix)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe and the closure touches nothing
    // else of the parent's state.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let status = cmd.status().expect("spawn the container watchdog");
    assert!(status.success(), "container watchdog: {status}");
}

impl PgServer {
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// A `postgres://` URL for `dbname` (TLS left to the caller's
    /// `sslmode`, appended as `?sslmode=…`).
    pub fn url(&self, dbname: &str) -> String {
        format!(
            "postgres://{USER}:{PASSWORD}@{}:{}/{dbname}",
            self.host, self.port
        )
    }

    /// Connection settings for `dbname`, without TLS.
    pub fn config(&self, dbname: &str) -> tokio_postgres::Config {
        let mut cfg = tokio_postgres::Config::new();
        cfg.host(&self.host)
            .port(self.port)
            .user(USER)
            .password(PASSWORD)
            .dbname(dbname)
            .ssl_mode(tokio_postgres::config::SslMode::Disable);
        cfg
    }

    /// A plain connection to `dbname`; its connection task runs on the
    /// current Tokio runtime.
    pub async fn connect(&self, dbname: &str) -> tokio_postgres::Client {
        let (client, conn) = self
            .config(dbname)
            .connect(tokio_postgres::NoTls)
            .await
            .unwrap_or_else(|e| panic!("connect to {dbname}: {e}"));
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                eprintln!("connection error: {e}");
            }
        });
        client
    }

    /// Create an empty database no other test uses, named `<prefix>_…`.
    pub async fn create_database(&self, prefix: &str) -> String {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let name = format!(
            "{prefix}_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        self.connect("postgres")
            .await
            .batch_execute(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap_or_else(|e| panic!("create database {name}: {e:?}"));
        name
    }

    /// Create `name` unless it exists. Call it under a [`run_lock`] when
    /// several processes may race on the same name.
    pub async fn ensure_database(&self, name: &str) {
        let admin = self.connect("postgres").await;
        let exists = admin
            .query_opt("SELECT 1 FROM pg_database WHERE datname = $1", &[&name])
            .await
            .expect("look up database")
            .is_some();
        if !exists {
            admin
                .batch_execute(&format!("CREATE DATABASE {name}"))
                .await
                .unwrap_or_else(|e| panic!("create database {name}: {e:?}"));
        }
    }
}
