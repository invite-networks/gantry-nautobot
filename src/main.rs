//! gantry-nautobot: a gantry plugin (api 1) that records pods, containers,
//! volumes and addresses in Nautobot through the invite-nautobot app, and
//! allocates pod addresses from Nautobot IPAM.
//!
//! gantry starts it on demand and talks JSON lines over stdin and stdout:
//! `hello` first, then `event` (`post_up`, `post_down`) and `allocate`
//! messages. Requests with an `id` get one answer each. Logs go to stderr,
//! which gantry appends to /var/log/gantry/plugins/nautobot.log.

mod allocate;
mod config;
mod host;
mod nautobot;
mod sync;

use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

/// Everything a message handler needs, set up by `hello`.
pub struct Ctx {
    pub nb: nautobot::Client,
    pub host: String,
    pub project: String,
    /// Configured node name, when it differs from the host name.
    pub node: Option<String>,
    pub ip_status: String,
    /// Held for the life of the process, so events for one project are
    /// handled in the order gantry sent them, across processes.
    _lock: Option<nix::fcntl::Flock<std::fs::File>>,
}

fn main() -> ExitCode {
    let stdin = std::io::stdin().lock();
    let mut ctx: Option<Result<Ctx, String>> = None;
    for line in stdin.lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            log("", &format!("not a JSON line: {line:.200}"));
            continue;
        };
        let id = msg.get("id").and_then(Value::as_u64);
        let kind = msg.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "hello" {
            ctx = Some(setup(&msg));
            if let Some(Err(e)) = &ctx {
                log("", e);
            }
            continue;
        }
        if past_deadline(&msg) {
            log(
                project(&ctx),
                &format!("{kind}: past its deadline, skipped"),
            );
            if let Some(id) = id {
                answer(id, Err("past its deadline".into()));
            }
            continue;
        }
        let data = msg.get("data").cloned().unwrap_or(Value::Null);
        let result = match &ctx {
            None => Err("no hello received".to_string()),
            Some(Err(e)) => Err(e.clone()),
            Some(Ok(c)) => handle(c, kind, &msg, &data),
        };
        if let Err(e) = &result {
            log(project(&ctx), &format!("{kind}: {e}"));
        }
        if let Some(id) = id {
            answer(id, result);
        }
    }
    ExitCode::SUCCESS
}

fn handle(c: &Ctx, kind: &str, msg: &Value, data: &Value) -> Result<Value, String> {
    match kind {
        "event" => match msg.get("hook").and_then(Value::as_str) {
            Some("post_up") => sync::up(c, data).map(|()| Value::Null),
            Some("post_down") => sync::down(c, data).map(|()| Value::Null),
            Some(other) => Err(format!("hook {other} is not handled")),
            None => Err("event without a hook".into()),
        },
        "allocate" => allocate::allocate(c, data),
        other => Err(format!("unknown message type {other:?}")),
    }
}

fn setup(hello: &Value) -> Result<Ctx, String> {
    let api = hello.get("api").and_then(Value::as_u64);
    if api != Some(1) {
        return Err(format!("gantry speaks api {api:?}; this plugin speaks 1"));
    }
    let project = hello
        .get("project")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let host = hello
        .get("host")
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| {
            nix::unistd::gethostname()
                .ok()
                .and_then(|h| h.into_string().ok())
        })
        .ok_or("no host name")?;
    let path = std::env::var_os("GANTRY_PLUGIN_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/gantry/plugins/nautobot.toml"));
    let cfg = config::Config::load(&path)?;
    let lock = hello
        .get("lock")
        .and_then(Value::as_str)
        .map(|p| take_lock(p).map_err(|e| format!("lock {p}: {e}")))
        .transpose()?;
    Ok(Ctx {
        nb: nautobot::Client::new(&cfg)?,
        host,
        project,
        node: cfg.node.clone(),
        ip_status: cfg.ip_status.clone(),
        _lock: lock,
    })
}

fn take_lock(path: &str) -> Result<nix::fcntl::Flock<std::fs::File>, String> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    nix::fcntl::Flock::lock(f, nix::fcntl::FlockArg::LockExclusive).map_err(|(_, e)| e.to_string())
}

fn past_deadline(msg: &Value) -> bool {
    let Some(deadline) = msg.get("deadline_ms").and_then(Value::as_u64) else {
        return false;
    };
    now_ms() > deadline
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Writes one answer. gantry may be gone after an async event; a closed
/// pipe is ignored (Rust programs ignore SIGPIPE, so this is an error).
fn answer(id: u64, result: Result<Value, String>) {
    let line = match result {
        Ok(v) => json!({"id": id, "ok": true, "result": v}),
        Err(e) => json!({"id": id, "ok": false, "error": e}),
    };
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn project(ctx: &Option<Result<Ctx, String>>) -> &str {
    match ctx {
        Some(Ok(c)) => &c.project,
        _ => "",
    }
}

pub fn log(project: &str, msg: &str) {
    let secs = now_ms() / 1000;
    if project.is_empty() {
        eprintln!("{secs} {msg}");
    } else {
        eprintln!("{secs} [{project}] {msg}");
    }
}
