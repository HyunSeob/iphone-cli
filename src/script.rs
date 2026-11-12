//! Scripts: JavaScript that drives the phone.
//!
//! `iph script` runs code as it is given; `iph run <name>` runs a saved one,
//! from `~/.config/iph/scripts/` or from the examples in this repository's
//! `scripts/` folder (which are built into the binary). A script calls the
//! commands the CLI has (`read`, `tap`, `scroll` …) as functions and gets the
//! same result objects back. A call that fails throws.
//!
//! The engine is QuickJS. A script has no files, no network and no
//! processes: only the functions set up below. It runs on a thread of its
//! own, and every call crosses over to the daemon's task, where it runs
//! exactly as a command typed at the shell.
//!
//! A script cannot go on after a call whose input may or may not have
//! reached the phone. The run ends there, with `outcome: "unknown"`.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use rquickjs::{CaughtError, Context, Function, Runtime, context::EvalOptions};
use serde_json::{Map, Value, json};

use crate::{
    actions::{Target, TapArgs, brief, parse_args, parse_target},
    ax::tree::{BuildOptions, Screen},
    device::session::Session,
    fields,
    i18n::t,
    paths,
    reply::{Fail, Outcome, Res, envelope},
    selector::Selector,
};

/// Example scripts shipped in the repository's `scripts/` folder.
const BUILTIN: &[(&str, &str)] = &[
    ("settings-version", include_str!("../scripts/settings-version.js")),
    ("calculator-multiply", include_str!("../scripts/calculator-multiply.js")),
    ("clock-world-list", include_str!("../scripts/clock-world-list.js")),
    ("settings-list", include_str!("../scripts/settings-list.js")),
];

/// What a script says it does: its first comment line.
fn description(code: &str) -> String {
    code.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .and_then(|l| l.strip_prefix("//"))
        .map(|l| l.trim().to_string())
        .unwrap_or_default()
}

/// Saved scripts and where each comes from. Personal ones win over examples.
pub fn list() -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    if let Ok(dir) = std::fs::read_dir(paths::personal_scripts()) {
        let mut files: Vec<std::path::PathBuf> = dir
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("js"))
            .collect();
        files.sort();
        for path in files {
            let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string();
            let about = std::fs::read_to_string(&path).map(|c| description(&c)).unwrap_or_default();
            out.push((name, about, path.display().to_string()));
        }
    }
    for (name, code) in BUILTIN {
        if !out.iter().any(|(n, _, _)| n == name) {
            out.push((name.to_string(), description(code), format!("builtin:{name}")));
        }
    }
    out
}

/// A saved script by name: the personal folder first, then the examples.
/// Returns its code and where it came from.
pub fn load(name: &str) -> Res<(String, String)> {
    if name.ends_with(".js") || name.contains('/') {
        return Err(Fail::invalid(format!("\"{name}\" is a file, not the name of a saved script")).hint(t(
            "파일은 `iph script <파일>`로 실행합니다.",
            "Run a file with `iph script <file>`.",
        )));
    }
    let path = paths::personal_scripts().join(format!("{name}.js"));
    if let Ok(code) = std::fs::read_to_string(&path) {
        return Ok((code, path.display().to_string()));
    }
    if let Some((_, code)) = BUILTIN.iter().find(|(n, _)| *n == name) {
        return Ok((code.to_string(), format!("builtin:{name}")));
    }
    Err(Fail::not_sent("script_not_found", format!("no script named \"{name}\"")).hint(t(
        "`iph scripts`로 목록을 확인하세요. 개인 스크립트는 ~/.config/iph/scripts/<이름>.js 에 둡니다.",
        "List them with `iph scripts`. Personal scripts live in ~/.config/iph/scripts/<name>.js.",
    )))
}

pub fn cmd_scripts() -> Res {
    let list: Vec<Value> = list()
        .into_iter()
        .map(|(name, description, source)| json!({ "name": name, "description": description, "source": source }))
        .collect();
    Ok(fields! { "count": list.len(), "scripts": list })
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

/// What a script sees. Every function goes through `__iph`, the one door to
/// the phone, as JSON text both ways; a result that is not `ok` is thrown.
const PRELUDE: &str = r#"
"use strict";
(() => {
  const opts = (o) => (o && typeof o === "object" && !Array.isArray(o)) ? o : {};
  const call = (cmd, args) => {
    const r = JSON.parse(__iph(cmd, JSON.stringify(args === undefined ? {} : args)));
    if (r.ok) return r;
    const e = new Error((r.error && r.error.message) || "the call failed");
    e.name = "IphError";
    e.cmd = cmd;
    e.code = r.error && r.error.code;
    e.outcome = r.outcome;
    e.hint = r.error && r.error.hint;
    e.result = r;
    throw e;
  };
  const text = (x) => (typeof x === "string" ? x : JSON.stringify(x));
  const g = globalThis;
  g.read = (o) => call("read", opts(o));
  g.find = (selector, o) => call("find", { ...opts(o), selector });
  g.tap = (target, o) => call("tap", { ...opts(o), target });
  g.swipe = (how, o) => call("swipe", typeof how === "string" ? { ...opts(o), direction: how } : { ...opts(o), ...opts(how) });
  g.scroll = (direction, o) => call("scroll", { ...opts(o), direction });
  g.type = (value, o) => call("type", { ...opts(o), text: String(value) });
  g.key = (name, o) => call("key", { ...opts(o), key: name });
  g.button = (name, o) => call("button", { ...opts(o), button: name });
  g.launch = (app, o) => call("launch", { ...opts(o), app });
  g.apps = () => call("apps", {});
  g.waitFor = (selector, o) => call("wait_for", { ...opts(o), selector });
  g.readField = (spec) => call("read_field", opts(spec)).value;
  g.readList = (spec) => call("read_list", opts(spec));
  g.sleep = (ms) => { __sleep(Number(ms) || 0); };
  g.log = (...xs) => { __emit("log", "", xs.map(text).join(" ")); };
  g.output = (key, value) => { __emit("output", String(key), JSON.stringify(value === undefined ? null : value)); };
  g.console = { log: g.log, info: g.log, warn: g.log, error: g.log };
  g.__done = (value) => JSON.stringify({ ok: true, value });
  g.__threw = (e) => {
    const error = {
      name: e && e.name,
      message: e && e.message !== undefined ? String(e.message) : String(e),
      stack: e && e.stack ? String(e.stack) : undefined,
    };
    if (e && e.result) {
      error.cmd = e.cmd;
      error.result = e.result;
    }
    return JSON.stringify({ ok: false, error });
  };
})();
"#;

/// The name the script's code goes by in stack traces.
const SCRIPT_FILE: &str = "script";

/// What a call gets back once the run has been stopped.
const STOPPED: &str =
    r#"{"ok":false,"outcome":"not_sent","error":{"code":"script_stopped","message":"the script was stopped"}}"#;

/// Most log lines kept from one run, and the longest a line may be.
const MAX_LOGS: usize = 200;
const MAX_LOG_LINE: usize = 2000;

/// The longest a script may be allowed to run, whatever the config says:
/// the resident process gives up on a request after [`crate::daemon::budget`].
pub const MAX_TIMEOUT_SECS: u64 = 840;

pub struct Limits {
    pub timeout: Duration,
    /// Bytes the script may hold.
    pub memory: usize,
}

/// The phone side of a run.
#[allow(async_fn_in_trait)]
pub trait ScriptHost {
    /// Run one call and give back the object the command would print.
    async fn call(&mut self, cmd: &str, args: Value) -> Value;
    /// True once the run must not go on.
    fn must_stop(&self) -> bool;
    /// A call was cut off by the time limit.
    fn cut_off(&mut self, cmd: &str);
}

#[derive(Debug, Clone, PartialEq)]
pub enum End {
    /// The script ran to its end; what it returned, if anything.
    Returned(Option<Value>),
    /// The script threw, or a call it did not catch failed:
    /// `{name, message, stack, cmd?, result?}`.
    Threw(Value),
    /// Stopped from outside: "timeout", or "unknown_outcome" after a call
    /// that must not be followed by another.
    Stopped(&'static str),
    /// The engine could not run the script at all.
    Engine(String),
}

#[derive(Debug)]
pub struct Ended {
    pub end: End,
    pub outputs: Map<String, Value>,
    pub logs: Vec<String>,
    pub calls: u32,
    /// How many lines the script has.
    pub lines: u64,
}

struct Call {
    cmd: String,
    args: String,
    reply: std::sync::mpsc::Sender<String>,
}

#[derive(Default)]
struct Shared {
    /// Set to end the run: the engine is interrupted and calls are refused.
    stop: AtomicBool,
    outputs: Mutex<Map<String, Value>>,
    logs: Mutex<Vec<String>>,
}

/// The script's own thread: set the engine up, run the code, say how it ended.
fn js_main(
    code: &str,
    args: &Value,
    memory: usize,
    shared: &Arc<Shared>,
    calls: tokio::sync::mpsc::UnboundedSender<Call>,
) -> End {
    let runtime = match Runtime::new() {
        Ok(r) => r,
        Err(e) => return End::Engine(format!("cannot start the engine: {e}")),
    };
    runtime.set_memory_limit(memory);
    runtime.set_max_stack_size(1 << 20);
    let flag = shared.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || flag.stop.load(Ordering::Relaxed))));
    // `full` is the language and its built-ins (JSON, Date, RegExp, Map …).
    // Nothing that reaches outside comes with it: no modules, no std, no os.
    let context = match Context::full(&runtime) {
        Ok(c) => c,
        Err(e) => return End::Engine(format!("cannot start the engine: {e}")),
    };
    context.with(|ctx| {
        let ran = (|| -> rquickjs::Result<String> {
            let globals = ctx.globals();
            let (stop, door) = (shared.clone(), calls.clone());
            globals.set(
                "__iph",
                Function::new(ctx.clone(), move |cmd: String, args: String| -> String {
                    if stop.stop.load(Ordering::Relaxed) {
                        return STOPPED.to_string();
                    }
                    let (reply, answer) = std::sync::mpsc::channel();
                    if door.send(Call { cmd, args, reply }).is_err() {
                        return STOPPED.to_string();
                    }
                    answer.recv().unwrap_or_else(|_| STOPPED.to_string())
                })?,
            )?;
            let stop = shared.clone();
            globals.set(
                "__sleep",
                Function::new(ctx.clone(), move |ms: f64| {
                    let until = Instant::now() + Duration::from_millis(ms.clamp(0.0, 600_000.0) as u64);
                    while !stop.stop.load(Ordering::Relaxed) {
                        let left = until.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            break;
                        }
                        std::thread::sleep(left.min(Duration::from_millis(20)));
                    }
                })?,
            )?;
            let kept = shared.clone();
            globals.set(
                "__emit",
                Function::new(ctx.clone(), move |kind: String, key: String, body: String| {
                    if kind == "output" {
                        let value = serde_json::from_str(&body).unwrap_or(Value::Null);
                        kept.outputs.lock().unwrap().insert(key, value);
                    } else {
                        let mut logs = kept.logs.lock().unwrap();
                        if logs.len() < MAX_LOGS {
                            logs.push(body.chars().take(MAX_LOG_LINE).collect());
                        }
                    }
                })?,
            )?;
            // Named apart, so a stack trace tells the script's lines from these.
            let named = |name: &str| {
                let mut options = EvalOptions::default();
                options.filename = Some(name.into());
                options
            };
            ctx.eval_with_options::<(), _>(PRELUDE, named("iph"))?;
            // The script is the body of a function, so `return` works at its
            // top level, and its first line is line 2 of what is evaluated.
            let run: Function = ctx.eval_with_options(
                format!(
                    "(function(args){{try{{return __done((function(args){{\"use strict\";\n{code}\n}})(args))}}catch(e){{return __threw(e)}}}})"
                ),
                named(SCRIPT_FILE),
            )?;
            let args = ctx.json_parse(args.to_string())?;
            run.call((args,))
        })();
        match ran {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(v) if v["ok"] == true => End::Returned(v.get("value").cloned().filter(|x| !x.is_null())),
                Ok(v) => End::Threw(v["error"].clone()),
                Err(e) => End::Engine(format!("the script's result is not JSON: {e}")),
            },
            Err(_) if shared.stop.load(Ordering::Relaxed) => End::Stopped("timeout"),
            // Not thrown by the script's own code: it does not parse, or the
            // engine ran out of memory before the script could catch it.
            Err(e) => End::Threw(match CaughtError::from_error(&ctx, e) {
                CaughtError::Exception(ex) => json!({
                    "name": "Error",
                    "message": ex.message().unwrap_or_else(|| "the script could not run".into()),
                    "stack": ex.stack(),
                }),
                other => json!({ "name": "Error", "message": other.to_string() }),
            }),
        }
    })
}

/// Run `code` against `host`.
pub async fn run<H: ScriptHost>(host: &mut H, code: &str, args: &Map<String, Value>, limits: &Limits) -> Ended {
    let shared = Arc::new(Shared::default());
    let lines = code.lines().count().max(1) as u64;
    let (door, mut calls_in) = tokio::sync::mpsc::unbounded_channel::<Call>();
    let (done, finished) = tokio::sync::oneshot::channel::<End>();
    let (thread_shared, code_owned, args_owned, memory) =
        (shared.clone(), code.to_string(), Value::Object(args.clone()), limits.memory);
    let spawned = std::thread::Builder::new()
        .name("iph-script".into())
        .stack_size(8 << 20)
        .spawn(move || {
            let end = js_main(&code_owned, &args_owned, memory, &thread_shared, door);
            let _ = done.send(end);
        });
    if let Err(e) = spawned {
        return Ended {
            end: End::Engine(format!("cannot start the script: {e}")),
            outputs: Map::new(),
            logs: Vec::new(),
            calls: 0,
            lines,
        };
    }

    let deadline = tokio::time::Instant::now() + limits.timeout;
    let mut calls = 0u32;
    let mut stopped: Option<&'static str> = None;
    loop {
        let next = tokio::select! {
            call = calls_in.recv() => call,
            _ = tokio::time::sleep_until(deadline) => {
                stopped = Some("timeout");
                None
            }
        };
        // No more calls: the script's thread is done, or time is up.
        let Some(call) = next else { break };
        calls += 1;
        let args = serde_json::from_str(&call.args).unwrap_or_else(|_| json!({}));
        let reply = match tokio::time::timeout_at(deadline, host.call(&call.cmd, args)).await {
            Ok(result) => result.to_string(),
            Err(_) => {
                host.cut_off(&call.cmd);
                stopped = Some("timeout");
                STOPPED.to_string()
            }
        };
        if stopped.is_none() && host.must_stop() {
            stopped = Some("unknown_outcome");
        }
        if stopped.is_some() {
            // Before the answer goes back, so the script cannot act on it.
            shared.stop.store(true, Ordering::Relaxed);
        }
        let _ = call.reply.send(reply);
        if stopped.is_some() {
            break;
        }
    }
    if stopped.is_some() {
        shared.stop.store(true, Ordering::Relaxed);
    }
    // Whatever the script still asks for is refused from here on.
    drop(calls_in);
    let end = match tokio::time::timeout(Duration::from_secs(10), finished).await {
        Ok(Ok(end)) => end,
        Ok(Err(_)) => End::Engine("the script engine stopped unexpectedly".into()),
        Err(_) => End::Engine("the script did not stop".into()),
    };
    let end = match stopped {
        Some(why) => End::Stopped(why),
        None => end,
    };
    Ended {
        end,
        outputs: std::mem::take(&mut *shared.outputs.lock().unwrap()),
        logs: std::mem::take(&mut *shared.logs.lock().unwrap()),
        calls,
        lines,
    }
}

// ---------------------------------------------------------------------------
// One run's bookkeeping, shared by the real phone and the tests
// ---------------------------------------------------------------------------

/// Calls that send input to the phone.
const INPUTS: [&str; 8] = ["launch", "tap", "swipe", "scroll", "type", "key", "button", "read_list"];
/// Calls that may send several inputs before they fail.
const MANY_INPUTS: [&str; 2] = ["scroll", "read_list"];
/// Calls that act on the app the script launched, and so need it in front.
const NEEDS_APP: [&str; 8] = ["tap", "swipe", "scroll", "type", "key", "button", "read_field", "read_list"];

/// A call's arguments after the run's rules have been applied.
#[derive(Debug)]
pub struct Prepared {
    pub args: Value,
    pub hint: Option<String>,
}

#[derive(Debug, Default)]
pub struct RunState {
    /// Calls that sent input, or may have.
    pub inputs_sent: u32,
    /// The app the last `launch` brought to the front.
    pub expected_pid: Option<i64>,
    /// The call after which the run may not go on, and its result.
    pub unknown: Option<(String, Value)>,
}

impl RunState {
    /// Apply the run's rules to one call's arguments.
    pub fn prepare(&mut self, cmd: &str, args: Value) -> Res<Prepared> {
        let Value::Object(mut map) = args else {
            return Err(Fail::invalid(format!("`{cmd}` takes an object of arguments")));
        };
        let hint = map.remove("hint").and_then(|v| v.as_str().map(String::from));
        Ok(Prepared { args: Value::Object(map), hint })
    }

    /// Take note of how a call ended.
    pub fn note(&mut self, cmd: &str, result: &Value) {
        let outcome = result["outcome"].as_str().unwrap_or("");
        let sent = result["ok"] == true
            || outcome == "unknown"
            || outcome == "done"
            // These may have scrolled before whatever went wrong.
            || (MANY_INPUTS.contains(&cmd) && result["error"]["code"] != "invalid_argument");
        if INPUTS.contains(&cmd) && sent {
            self.inputs_sent += 1;
        }
        if outcome == "unknown" {
            self.unknown = Some((cmd.to_string(), result.clone()));
        }
    }
}

/// Line of the script a stack trace points at: the first frame in the
/// script's own code, which starts on line 2 of what is evaluated.
fn script_line(stack: Option<&str>) -> Option<u64> {
    let stack = stack?;
    let mark = format!("{SCRIPT_FILE}:");
    let mut from = 0;
    while let Some(found) = stack[from..].find(&mark) {
        let at = from + found;
        from = at + mark.len();
        // "(script:3:7)" or " script:3:7", not the tail of another name.
        if at > 0 && !matches!(stack.as_bytes()[at - 1], b'(' | b' ') {
            continue;
        }
        let digits: String = stack[from..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse::<u64>() {
            return Some(n.saturating_sub(1)).filter(|n| *n > 0);
        }
    }
    None
}

/// Turn the way a run ended into the command's result.
pub fn finish(ended: Ended, state: &RunState) -> Res {
    let Ended { end, mut outputs, logs, calls, lines } = ended;
    let sent = state.inputs_sent;
    // The run's outcome. Anything that stops a run after input has gone out
    // is `unknown`, because running the script again would send that input
    // again.
    let outcome_of = |call: Option<&Value>| match call.and_then(|c| c["outcome"].as_str()) {
        Some("unknown") => Outcome::Unknown,
        _ if sent > 0 => Outcome::Unknown,
        _ => Outcome::NotSent,
    };
    let failed_call = |result: &Value, cmd: &str, line: Option<u64>| -> Fail {
        let reason = &result["error"];
        let mut f = Fail::new(
            outcome_of(Some(result)),
            "script_failed",
            format!("`{cmd}` failed: {}", reason["message"].as_str().unwrap_or("")),
        );
        f.hint = reason["hint"].as_str().map(String::from);
        f = f
            .with("failed_call", json!({ "cmd": cmd, "line": line, "outcome": result["outcome"] }))
            .with("reason", json!({ "code": reason["code"], "message": reason["message"] }));
        // What the failed call had to say (candidates, the screen …).
        if let Some(extra) = result.as_object() {
            for (k, v) in extra {
                if !matches!(k.as_str(), "ok" | "outcome" | "error" | "elapsed_ms") {
                    f.extra.entry(k.clone()).or_insert(v.clone());
                }
            }
        }
        f
    };

    let fail = match end {
        End::Returned(value) => {
            match value {
                Some(Value::Object(returned)) => outputs.extend(returned),
                Some(other) => {
                    outputs.insert("result".into(), other);
                }
                None => {}
            }
            return Ok(fields! { "outputs": outputs, "logs": logs, "calls": calls, "inputs_sent": sent });
        }
        End::Threw(error) => {
            // Code that stops short is found out past its last line.
            let line = script_line(error["stack"].as_str()).map(|n| n.min(lines));
            match (error.get("result"), error["cmd"].as_str()) {
                (Some(result), Some(cmd)) if result.is_object() => failed_call(result, cmd, line),
                _ => Fail::new(
                    outcome_of(None),
                    "script_error",
                    error["message"].as_str().unwrap_or("the script threw").to_string(),
                )
                .with("line", json!(line)),
            }
        }
        End::Stopped("unknown_outcome") => match &state.unknown {
            Some((cmd, result)) => failed_call(result, cmd, None),
            None => Fail::unknown("script_stopped", "the script was stopped"),
        },
        End::Stopped(_) => Fail::new(
            outcome_of(None),
            "script_timeout",
            "the script ran longer than it is allowed to",
        )
        .hint(t(
            "config.toml의 [script] timeout_secs로 한도를 바꿀 수 있습니다. 입력을 보낸 뒤였다면 `iph read`로 화면부터 확인하세요.",
            "[script] timeout_secs in config.toml changes the limit. If input had gone out, check the screen with `iph read` first.",
        )),
        End::Engine(message) => Fail::new(outcome_of(None), "script_error", message),
    };
    let fail = if sent > 0 && fail.hint.is_none() {
        fail.hint(t(
            "스크립트가 입력을 보낸 뒤에 멈췄습니다. 다시 실행하기 전에 `iph read`로 화면을 확인하세요.",
            "The script stopped after it had sent input. Check the screen with `iph read` before running it again.",
        ))
    } else {
        fail
    };
    Err(fail
        .with("outputs", json!(outputs))
        .with("logs", json!(logs))
        .with("calls", calls)
        .with("inputs_sent", sent))
}

// ---------------------------------------------------------------------------
// The real phone
// ---------------------------------------------------------------------------

struct PhoneHost<'a> {
    session: &'a mut Session,
    state: RunState,
}

impl PhoneHost<'_> {
    async fn tap(&mut self, args: Value) -> Res {
        let s = &mut *self.session;
        let Value::Object(mut map) = args else {
            return Err(Fail::invalid("tap takes a target"));
        };
        // Like a person, a script waits a moment for what it wants to press
        // and presses it once it holds still.
        let wait = map.remove("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(5000);
        let settle = map.remove("settle").and_then(|v| v.as_bool()).unwrap_or(true);
        let target = map.get("target").cloned().unwrap_or(Value::Null);
        if let Some(list) = target.as_array() {
            return Box::pin(s.tap_sequence(list)).await;
        }
        if let Target::Selector(sel) = parse_target(&target)? {
            if wait > 0 {
                Box::pin(s.wait_for(&sel, Duration::from_millis(wait))).await?;
            }
            if settle {
                Box::pin(s.settle_target(&sel)).await?;
            }
        }
        let tap: TapArgs = parse_args(&Value::Object(map))?;
        Box::pin(s.cmd_tap(tap)).await
    }

    async fn dispatch(&mut self, cmd: &str, args: Value) -> Res {
        if NEEDS_APP.contains(&cmd)
            && let Some(f) = self.session.unexpected_app(self.state.expected_pid)
        {
            return Err(f);
        }
        match cmd {
            "read" => Box::pin(self.session.cmd_read(parse_args(&args)?)).await,
            "find" => Box::pin(self.session.cmd_find(parse_args(&args)?)).await,
            "apps" => self.session.cmd_apps().await,
            "launch" => {
                let r = Box::pin(self.session.cmd_launch(parse_args(&args)?)).await?;
                if r.get("foreground").and_then(|v| v.as_bool()) != Some(true) {
                    return Err(Fail::unknown("launch_not_verified", "the app was launched but is not in front"));
                }
                self.state.expected_pid = r.get("pid").and_then(|v| v.as_i64());
                Ok(r)
            }
            "tap" => self.tap(args).await,
            "swipe" | "scroll" | "type" | "key" | "button" | "read_list" => {
                let s = &mut *self.session;
                match cmd {
                    "swipe" => Box::pin(s.cmd_swipe(parse_args(&args)?)).await,
                    "scroll" => Box::pin(s.cmd_scroll(parse_args(&args)?)).await,
                    "type" => Box::pin(s.cmd_type(parse_args(&args)?)).await,
                    "key" => Box::pin(s.cmd_key(parse_args(&args)?)).await,
                    "button" => Box::pin(s.cmd_button(parse_args(&args)?)).await,
                    _ => Box::pin(s.read_list(&args)).await.map(|(_, rows, info)| {
                        let count = rows.as_array().map(|a| a.len()).unwrap_or(0);
                        fields! { "rows": rows, "count": count, "scrolls": info["scrolls"], "ended": info["ended"] }
                    }),
                }
            }
            "wait_for" => {
                let selector = args.get("selector").cloned().unwrap_or(Value::Null);
                let sel = Selector::from_value(&selector).map_err(|e| Fail::invalid(e.to_string()))?;
                let timeout = args.get("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(8000);
                let el = Box::pin(self.session.wait_for(&sel, Duration::from_millis(timeout))).await?;
                Ok(fields! { "element": brief(&el) })
            }
            "read_field" => {
                let (_, value) = Box::pin(self.session.read_field(&args)).await?;
                Ok(fields! { "value": value })
            }
            other => Err(Fail::invalid(format!(
                "a script has no `{other}` (read, find, tap, swipe, scroll, type, key, button, launch, apps, waitFor, readField, readList)"
            ))),
        }
    }
}

impl ScriptHost for PhoneHost<'_> {
    async fn call(&mut self, cmd: &str, args: Value) -> Value {
        let started = Instant::now();
        let result = match self.state.prepare(cmd, args) {
            Ok(Prepared { args, hint }) => {
                Box::pin(self.dispatch(cmd, args)).await.map_err(|mut f| {
                    // Whoever wrote the script knows what this call waits on.
                    if let Some(hint) = hint {
                        f.hint = Some(hint);
                    }
                    f
                })
            }
            Err(f) => Err(f),
        };
        let value = envelope(result, started);
        self.state.note(cmd, &value);
        value
    }

    fn must_stop(&self) -> bool {
        self.state.unknown.is_some()
    }

    fn cut_off(&mut self, cmd: &str) {
        // Whatever was in flight is abandoned with its connections.
        self.session.reset();
        if INPUTS.contains(&cmd) {
            self.state.inputs_sent += 1;
        }
    }
}

/// The request for the resident process: the code travels with it, so a
/// saved script or a file is read by whoever asks, not by the daemon.
pub fn request(code: &str, args: &Map<String, Value>, saved: Option<(&str, &str)>) -> Value {
    let mut req = json!({ "code": code, "args": args });
    if let Some((name, source)) = saved {
        req["script"] = json!(name);
        req["source"] = json!(source);
    }
    req
}

impl Session {
    /// What the latest reading showed, for a failure with nothing of its own to show.
    fn last_seen(&self) -> Option<Value> {
        let (_, dump, _) = self.ax.as_ref()?.latest()?;
        Some(Screen::build(&dump, BuildOptions::default()).summary(12))
    }

    pub async fn cmd_script(&mut self, code: &str, args: &Map<String, Value>) -> Res {
        let limits = Limits {
            timeout: Duration::from_secs(self.config.script.timeout_secs.clamp(1, MAX_TIMEOUT_SECS)),
            memory: (self.config.script.memory_mb.clamp(8, 1024) as usize) << 20,
        };
        let mut host = PhoneHost { session: self, state: RunState::default() };
        let ended = run(&mut host, code, args, &limits).await;
        let PhoneHost { session, state } = host;
        finish(ended, &state).map_err(|mut f| {
            if !f.extra.contains_key("screen_summary")
                && let Some(seen) = session.last_seen()
            {
                f = f.with("screen_summary", seen);
            }
            f
        })
    }

    /// Serve a `script` request (see [`request`]).
    pub async fn serve_script(&mut self, req: &Value) -> Res {
        let code = req
            .get("code")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Fail::invalid("script needs `code`"))?;
        let empty = Map::new();
        let args = req.get("args").and_then(|v| v.as_object()).unwrap_or(&empty);
        let saved: Vec<(&str, Value)> = ["script", "source"]
            .into_iter()
            .filter_map(|k| req.get(k).filter(|v| v.is_string()).map(|v| (k, v.clone())))
            .collect();
        match Box::pin(self.cmd_script(code, args)).await {
            Ok(mut out) => {
                out.extend(saved.into_iter().map(|(k, v)| (k.to_string(), v)));
                Ok(out)
            }
            Err(f) => Err(saved.into_iter().fold(f, |f, (k, v)| f.with(k, v))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A phone that answers from a table and remembers what it was asked.
    #[derive(Default)]
    struct Fake {
        state: RunState,
        asked: Vec<(String, Value)>,
        /// cmd → the result to give instead of plain success.
        answers: std::collections::HashMap<String, Value>,
        delay: Option<Duration>,
    }

    impl ScriptHost for Fake {
        async fn call(&mut self, cmd: &str, args: Value) -> Value {
            if let Some(d) = self.delay {
                tokio::time::sleep(d).await;
            }
            let value = match self.state.prepare(cmd, args) {
                Ok(p) => {
                    self.asked.push((cmd.to_string(), p.args));
                    self.answers
                        .get(cmd)
                        .cloned()
                        .unwrap_or_else(|| json!({"ok": true, "outcome": "done", "cmd": cmd}))
                }
                Err(f) => envelope(Err(f), Instant::now()),
            };
            self.state.note(cmd, &value);
            value
        }
        fn must_stop(&self) -> bool {
            self.state.unknown.is_some()
        }
        fn cut_off(&mut self, _cmd: &str) {
            self.state.inputs_sent += 1;
        }
    }

    fn limits() -> Limits {
        Limits { timeout: Duration::from_secs(5), memory: 32 << 20 }
    }

    fn go(fake: &mut Fake, code: &str, args: Value, limits: Limits) -> (Res, Ended0) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let args = args.as_object().cloned().unwrap_or_default();
        let ended = rt.block_on(run(fake, code, &args, &limits));
        let kept = Ended0 { end: ended.end.clone(), calls: ended.calls };
        (finish(ended, &fake.state), kept)
    }

    struct Ended0 {
        end: End,
        calls: u32,
    }

    #[test]
    fn a_script_returns_outputs_and_reads_its_arguments() {
        let mut fake = Fake::default();
        let (res, _) = go(
            &mut fake,
            "output('seen', read().cmd);\nlog('hello', {n: 1});\nreturn { who: args.name, twice: Number(args.n) * 2 };",
            json!({"name": "설정", "n": "21"}),
            limits(),
        );
        let out = res.unwrap();
        assert_eq!(out["outputs"], json!({"seen": "read", "who": "설정", "twice": 42}));
        assert_eq!(out["logs"], json!(["hello {\"n\":1}"]));
        assert_eq!(out["calls"], 1);
        assert_eq!(out["inputs_sent"], 0);
        // A plain value comes back as `result`.
        let (res, _) = go(&mut Fake::default(), "return 7", json!({}), limits());
        assert_eq!(res.unwrap()["outputs"], json!({"result": 7}));
    }

    #[test]
    fn calls_take_the_same_arguments_as_the_commands() {
        let mut fake = Fake::default();
        let code = r#"
            tap("일반");
            tap({label: "정보", role: "button"}, {long_ms: 300});
            scroll("down", {until: "개발자", max: 5});
            swipe("up"); swipe({from: [1, 2], to: [3, 4]}, {drag: true});
            type("안녕", {submit: true}); key("return"); button("home");
            launch("com.apple.Preferences", {restart: true});
            find("label~=일"); waitFor("일반", {timeout_ms: 100});
        "#;
        let (res, _) = go(&mut fake, code, json!({}), limits());
        res.unwrap();
        let asked: Vec<(&str, &Value)> = fake.asked.iter().map(|(c, a)| (c.as_str(), a)).collect();
        assert_eq!(asked[0], ("tap", &json!({"target": "일반"})));
        assert_eq!(asked[1], ("tap", &json!({"long_ms": 300, "target": {"label": "정보", "role": "button"}})));
        assert_eq!(asked[2], ("scroll", &json!({"until": "개발자", "max": 5, "direction": "down"})));
        assert_eq!(asked[3], ("swipe", &json!({"direction": "up"})));
        assert_eq!(asked[4], ("swipe", &json!({"drag": true, "from": [1, 2], "to": [3, 4]})));
        assert_eq!(asked[5], ("type", &json!({"submit": true, "text": "안녕"})));
        assert_eq!(asked[8], ("launch", &json!({"restart": true, "app": "com.apple.Preferences"})));
        assert_eq!(asked[10], ("wait_for", &json!({"timeout_ms": 100, "selector": "일반"})));
        assert_eq!(fake.state.inputs_sent, 9);
    }

    #[test]
    fn a_failed_call_throws_and_stops_the_script_unless_caught() {
        let not_found = json!({"ok": false, "outcome": "not_sent",
            "error": {"code": "not_found", "message": "no element matches", "hint": "scroll"},
            "screen_summary": {"labels": ["a", "b"]}});
        let mut fake = Fake::default();
        fake.answers.insert("tap".into(), not_found.clone());
        let (res, _) = go(&mut fake, "read();\ntap('없는 것');\nread();", json!({}), limits());
        let f = res.unwrap_err();
        assert_eq!((f.code.as_str(), f.outcome), ("script_failed", Outcome::NotSent));
        assert_eq!(f.extra["reason"]["code"], "not_found");
        assert_eq!(f.extra["failed_call"], json!({"cmd": "tap", "line": 2, "outcome": "not_sent"}));
        assert_eq!(f.extra["screen_summary"]["labels"], json!(["a", "b"]));
        assert_eq!(f.hint.as_deref(), Some("scroll"));
        assert_eq!(fake.asked.len(), 2, "nothing runs after the failure");

        // Caught, the script goes on and can look at what went wrong.
        let mut fake = Fake::default();
        fake.answers.insert("tap".into(), not_found);
        let code = "try { tap('x') } catch (e) { output('code', e.code); output('outcome', e.outcome) }\nreturn { after: read().ok }";
        let (res, _) = go(&mut fake, code, json!({}), limits());
        assert_eq!(res.unwrap()["outputs"], json!({"code": "not_found", "outcome": "not_sent", "after": true}));
    }

    #[test]
    fn a_script_that_throws_reports_its_line() {
        let mut fake = Fake::default();
        let (res, _) = go(&mut fake, "tap('a');\n\nthrow new Error('기대한 화면이 아닙니다');", json!({}), limits());
        let f = res.unwrap_err();
        assert_eq!(f.code, "script_error");
        assert_eq!(f.message, "기대한 화면이 아닙니다");
        assert_eq!(f.extra["line"], 3);
        // A tap had gone out: the caller must look before running it again.
        assert_eq!(f.outcome, Outcome::Unknown);
        assert_eq!(f.extra["inputs_sent"], 1);
        // Code that does not parse never starts.
        let (res, kept) = go(&mut Fake::default(), "tap('a'", json!({}), limits());
        let f = res.unwrap_err();
        assert_eq!((f.code.as_str(), f.outcome, kept.calls), ("script_error", Outcome::NotSent, 0));
        assert_eq!(f.extra["line"], 1);
    }

    #[test]
    fn nothing_follows_a_call_with_an_unknown_outcome() {
        let mut fake = Fake::default();
        fake.answers.insert(
            "tap".into(),
            json!({"ok": false, "outcome": "unknown", "error": {"code": "input_failed", "message": "write failed"}}),
        );
        // The script tries to swallow the failure and press again.
        let code = "for (let i = 0; i < 5; i++) { try { tap('보내기 아님') } catch (e) {} }\nreturn 'went on'";
        let (res, kept) = go(&mut fake, code, json!({}), limits());
        let f = res.unwrap_err();
        assert_eq!(f.outcome, Outcome::Unknown);
        assert_eq!(f.extra["reason"]["code"], "input_failed");
        assert_eq!(fake.asked.len(), 1, "the tap is not sent a second time");
        assert_eq!(kept.end, End::Stopped("unknown_outcome"));
    }

    #[test]
    fn a_script_that_never_ends_is_stopped() {
        let short = || Limits { timeout: Duration::from_millis(300), memory: 32 << 20 };
        let began = Instant::now();
        let (res, _) = go(&mut Fake::default(), "while (true) {}", json!({}), short());
        let f = res.unwrap_err();
        assert_eq!((f.code.as_str(), f.outcome), ("script_timeout", Outcome::NotSent));
        assert!(began.elapsed() < Duration::from_secs(5));
        // Input had gone out before it hung: unknown, not "safe to run again".
        let (res, _) = go(&mut Fake::default(), "tap('a'); while (true) {}", json!({}), short());
        assert_eq!(res.unwrap_err().outcome, Outcome::Unknown);
        // Asleep, or waiting on a call that does not come back: the same.
        let (res, _) = go(&mut Fake::default(), "sleep(60000)", json!({}), short());
        assert_eq!(res.unwrap_err().code, "script_timeout");
        let mut slow = Fake { delay: Some(Duration::from_secs(30)), ..Default::default() };
        let (res, _) = go(&mut slow, "tap('a')", json!({}), short());
        let f = res.unwrap_err();
        assert_eq!((f.code.as_str(), f.outcome), ("script_timeout", Outcome::Unknown));
    }

    #[test]
    fn a_script_that_eats_memory_is_stopped() {
        let small = Limits { timeout: Duration::from_secs(20), memory: 8 << 20 };
        let code = "const all = []; while (true) { all.push(new Array(100000).fill('x'.repeat(20))) }";
        let (res, _) = go(&mut Fake::default(), code, json!({}), small);
        let f = res.unwrap_err();
        assert_eq!(f.code, "script_error");
        assert!(f.message.contains("memory"), "{}", f.message);
    }

    #[test]
    fn a_script_has_no_way_out_of_the_engine() {
        let code = r#"
            const names = ["require", "process", "fetch", "XMLHttpRequest", "std", "os", "Deno", "Bun",
                           "WebSocket", "setTimeout", "importScripts", "Worker"];
            return { there: names.filter((n) => typeof globalThis[n] !== "undefined") };
        "#;
        let (res, _) = go(&mut Fake::default(), code, json!({}), limits());
        assert_eq!(res.unwrap()["outputs"]["there"], json!([]));
        // Modules cannot be loaded either way.
        let (res, _) = go(&mut Fake::default(), "import fs from 'fs'", json!({}), limits());
        assert_eq!(res.unwrap_err().code, "script_error");
        let (res, _) = go(
            &mut Fake::default(),
            "let got = 'nothing'; import('std').then(() => { got = 'loaded' }, () => { got = 'refused' }); return got",
            json!({}),
            limits(),
        );
        assert_ne!(res.map(|o| o["outputs"]["result"].clone()).unwrap_or(json!("refused")), json!("loaded"));
    }

    #[test]
    fn the_line_comes_from_the_scripts_own_frame() {
        let stack = "    at call (iph:9:15)\n    at <anonymous> (iph:23:20)\n    at <anonymous> (script:4:5)\n    at <anonymous> (script:1:1)\n";
        assert_eq!(script_line(Some(stack)), Some(3));
        assert_eq!(script_line(Some("    at script:2:9\n")), Some(1));
        assert_eq!(script_line(Some("    at f (eval_script:7:1)\n")), None);
        assert_eq!(script_line(None), None);
    }

    #[test]
    fn a_hint_on_a_call_is_what_the_person_is_told() {
        let mut state = RunState::default();
        let p = state.prepare("wait_for", json!({"selector": "홈", "hint": "폰을 봐 주세요"})).unwrap();
        assert_eq!(p.hint.as_deref(), Some("폰을 봐 주세요"));
        assert_eq!(p.args, json!({"selector": "홈"}));
    }

    #[test]
    fn saved_scripts_are_listed_with_their_first_comment() {
        assert_eq!(description("// 설정에서 iOS 버전 읽기\nlaunch('x')"), "설정에서 iOS 버전 읽기");
        assert_eq!(description("\n\n  // spaced  \ncode"), "spaced");
        assert_eq!(description("launch('x')"), "");
        for (name, code) in BUILTIN {
            assert!(!description(code).is_empty(), "{name} has no description");
        }
    }
}
