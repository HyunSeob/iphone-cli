//! The resident process (`iph daemon`, started on demand by the first `iph`
//! call). It keeps the tunnel, the accessibility subscription and the input
//! session open for one phone and serves requests from a Unix socket, one at a
//! time.

use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::Mutex,
};

use crate::{
    actions::parse_args,
    config::Config,
    device::session::Session,
    doctor, fields, paths,
    recipe::RunOptions,
    reply::{Fail, Res, envelope},
};

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    /// Identifies the client's binary; a daemon from an older build steps down.
    pub build: String,
    pub cmd: String,
    #[serde(default)]
    pub args: Value,
}

/// Stamp of the running executable: version plus the file's size and mtime.
pub fn build_stamp() -> String {
    let meta = std::env::current_exe().and_then(std::fs::metadata);
    let detail = match meta {
        Ok(m) => {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("{}-{}", m.len(), mtime)
        }
        Err(_) => "unknown".into(),
    };
    format!("{}-{detail}", env!("CARGO_PKG_VERSION"))
}

fn log(msg: &str) {
    eprintln!("[{}] {msg}", crate::safety::now_utc());
}

async fn dispatch(s: &mut Session, cmd: &str, args: &Value) -> Res {
    match cmd {
        "read" => s.cmd_read(parse_args(args)?).await,
        "find" => s.cmd_find(parse_args(args)?).await,
        "tap" => s.cmd_tap(parse_args(args)?).await,
        "swipe" => s.cmd_swipe(parse_args(args)?).await,
        "scroll" => s.cmd_scroll(parse_args(args)?).await,
        "type" => s.cmd_type(parse_args(args)?).await,
        "key" => s.cmd_key(parse_args(args)?).await,
        "button" => s.cmd_button(parse_args(args)?).await,
        "launch" => s.cmd_launch(parse_args(args)?).await,
        "apps" => s.cmd_apps().await,
        "screenshot" => s.cmd_screenshot(parse_args(args)?).await,
        "batch" => {
            let steps = args
                .get("steps")
                .and_then(|v| v.as_array())
                .ok_or_else(|| Fail::invalid("batch needs a JSON array of steps"))?;
            s.run_steps(steps, &Map::new(), &RunOptions { confirm: None, batch: true })
                .await
        }
        "run" => {
            let name = args
                .get("recipe")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Fail::invalid("run needs a recipe name"))?;
            let empty = Map::new();
            let given = args.get("args").and_then(|v| v.as_object()).unwrap_or(&empty);
            let confirm = args.get("confirm").and_then(|v| v.as_str()).map(String::from);
            s.cmd_run(name, given, confirm).await
        }
        "doctor" => doctor::run(s, parse_args(args)?).await,
        "status" => Ok(fields! {
            "udid": s.udid.clone(),
            "connection": s.connection(),
            "tunnel": s.tunnel.is_some(),
            "accessibility": s.ax.as_ref().is_some_and(|a| !a.is_closed()),
            "input_session": s.input.is_some(),
            "connects": s.connects,
            "pid": std::process::id(),
            "build": build_stamp(),
        }),
        other => Err(Fail::invalid(format!("unknown command `{other}`"))),
    }
}

/// How long one request may run before the daemon gives up on it.
pub fn budget(cmd: &str) -> Duration {
    match cmd {
        "run" | "batch" => Duration::from_secs(600),
        "doctor" => Duration::from_secs(120),
        _ => Duration::from_secs(90),
    }
}

struct Shared {
    session: Mutex<Session>,
    last_request: StdMutex<Instant>,
    build: String,
    udid: String,
}

async fn handle(stream: UnixStream, shared: Arc<Shared>) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    if line.trim().is_empty() {
        return Ok(());
    }
    let started = Instant::now();
    let req: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            let out = envelope(Err(Fail::invalid(format!("bad request: {e}"))), started);
            write.write_all(format!("{out}\n").as_bytes()).await?;
            return Ok(());
        }
    };
    *shared.last_request.lock().unwrap() = Instant::now();

    let reply = match req.cmd.as_str() {
        "ping" => json!({ "ok": true, "build": shared.build, "pid": std::process::id(), "udid": shared.udid }),
        "shutdown" => {
            write.write_all(b"{\"ok\":true}\n").await?;
            write.shutdown().await.ok();
            log("shutdown requested");
            shared.session.lock().await.shutdown().await;
            let _ = std::fs::remove_file(paths::socket(&shared.udid));
            let _ = std::fs::remove_file(paths::daemon_pid(&shared.udid));
            std::process::exit(0);
        }
        _ if req.build != shared.build => envelope(
            Err(Fail::not_sent("daemon_outdated", "the daemon runs an older build")),
            started,
        ),
        cmd => {
            let mut session = shared.session.lock().await;
            let result = match tokio::time::timeout(budget(cmd), dispatch(&mut session, cmd, &req.args)).await {
                Ok(r) => r,
                Err(_) => {
                    // Whatever was in flight is abandoned with its connections.
                    session.reset();
                    Err(Fail::unknown(
                        "timeout",
                        format!("`{cmd}` did not finish within {} s", budget(cmd).as_secs()),
                    )
                    .hint(crate::i18n::t(
                        "입력이 일부 나갔을 수 있습니다. `iph read`로 화면을 확인하세요.",
                        "Some input may have gone out. Check the screen with `iph read`.",
                    )))
                }
            };
            if let Err(f) = &result {
                log(&format!("{cmd} -> {} {}", f.outcome.as_str(), f.code));
            }
            envelope(result, started)
        }
    };
    *shared.last_request.lock().unwrap() = Instant::now();
    write.write_all(format!("{reply}\n").as_bytes()).await?;
    write.shutdown().await.ok();
    Ok(())
}

pub async fn run(udid: String) -> Result<()> {
    paths::ensure_private_dir(&paths::state_dir())?;
    let sock = paths::socket(&udid);
    if UnixStream::connect(&sock).await.is_ok() {
        bail!("a daemon for {udid} is already running");
    }
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    // So a client can put down a daemon that stopped answering.
    std::fs::write(paths::daemon_pid(&udid), std::process::id().to_string())?;

    let config = Config::load()?;
    let idle_exit = Duration::from_secs(config.daemon.idle_exit_secs.max(30));
    let shared = Arc::new(Shared {
        session: Mutex::new(Session::new(udid.clone(), config)),
        last_request: StdMutex::new(Instant::now()),
        build: build_stamp(),
        udid: udid.clone(),
    });
    log(&format!("daemon for {udid} listening, build {}", shared.build));

    // Requests run one at a time, so everything stays on this thread: the
    // device futures need not be `Send`.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            // Housekeeping: close an idle input session, exit when unused.
            let keeper = shared.clone();
            let keeper_sock = sock.clone();
            tokio::task::spawn_local(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let idle = keeper.last_request.lock().unwrap().elapsed();
                    if let Ok(mut session) = keeper.session.try_lock() {
                        session.close_idle_input().await;
                        if idle > idle_exit {
                            log("idle, exiting");
                            session.shutdown().await;
                            let _ = std::fs::remove_file(&keeper_sock);
                            let _ = std::fs::remove_file(paths::daemon_pid(&keeper.udid));
                            std::process::exit(0);
                        }
                    }
                }
            });

            loop {
                let (stream, _) = listener.accept().await?;
                let shared = shared.clone();
                tokio::task::spawn_local(async move {
                    if let Err(e) = handle(stream, shared).await {
                        log(&format!("connection error: {e:#}"));
                    }
                });
            }
        })
        .await
}
