#![allow(clippy::result_large_err)]

use std::{io::Read, time::Instant};

use clap::{Parser, Subcommand};
use iphone_cli::{
    client, clock, connect, daemon, devices, diagnose, events, fields, human,
    mcp,
    reply::{Fail, envelope},
    script, setup,
};
use serde_json::{Map, Value, json};

#[derive(Parser)]
#[command(
    name = "iph",
    version,
    about = "Read and operate any app on a real iPhone through its accessibility tree",
    long_about = "Read and operate any app on a real iPhone through its accessibility tree.\n\
                  No XCUITest, WebDriverAgent or Xcode signing. Output is JSON; add --human for people.\n\
                  Exit codes: 0 ok, 1 nothing was sent, 2 outcome unknown (check the screen)."
)]
struct Cli {
    /// Device to use. Defaults to IPH_UDID, config.toml, or the only connected iPhone.
    #[arg(long, global = true)]
    udid: Option<String>,
    /// Print for people instead of JSON.
    #[arg(long, global = true)]
    human: bool,
    /// Accepted for symmetry; JSON is already the default.
    #[arg(long, global = true, hide = true)]
    json: bool,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List connected iPhones.
    Devices,
    /// Verify and save the selected paired iPhone's IP when automatic discovery fails.
    Connect(connect::Args),
    /// Save connection diagnostics and optional reproduction output for a GitHub issue.
    Diagnose(diagnose::Args),
    /// Walk through the one-time preparation of a phone.
    Setup {
        /// Enable disabled Wi-Fi connection/debugging flags over USB.
        #[arg(long)]
        fix_wifi: bool,
    },
    /// Check that the phone is ready; says what to do for each problem.
    Doctor {
        /// Skip the check that sends a real touch (it scrolls Settings down and back).
        #[arg(long)]
        no_touch_test: bool,
        /// Enable disabled Wi-Fi connection/debugging flags over USB.
        #[arg(long)]
        fix_wifi: bool,
    },
    /// List the elements on screen.
    Read {
        /// Only the foreground app's elements.
        #[arg(long)]
        app: bool,
        /// Include elements outside the screen.
        #[arg(long)]
        all: bool,
        /// Wait for identifiers (about 0.6 s more).
        #[arg(long)]
        full: bool,
        /// Attach every numbered accessibility attribute.
        #[arg(long)]
        raw: bool,
        /// Accept a reading up to this many milliseconds old.
        #[arg(long)]
        max_age: Option<u64>,
    },
    /// Find elements matching a selector.
    Find {
        selector: String,
        #[arg(long)]
        all: bool,
    },
    /// Tap an element (selector) or a point (x,y).
    Tap {
        target: String,
        /// Hold for this many milliseconds.
        #[arg(long)]
        long: Option<u64>,
        #[arg(long)]
        double: bool,
        /// Read the screen once right after the tap instead of waiting for it to settle.
        #[arg(long)]
        no_wait: bool,
    },
    /// Swipe: up, down, left, right, or x1,y1,x2,y2.
    Swipe {
        direction: String,
        /// Milliseconds the movement takes.
        #[arg(long)]
        duration: Option<u64>,
        /// Slow to a stop before lifting (a drag) instead of flicking.
        #[arg(long)]
        drag: bool,
    },
    /// Pinch in or out, or turn: two fingers around the middle of the screen or a target.
    Pinch {
        /// `in` or `out`; or give `--scale` and/or `--rotate` instead.
        direction: Option<String>,
        /// End span over start span, 0.25..4 (above 1 spreads the fingers).
        #[arg(long)]
        scale: Option<f64>,
        /// Degrees, positive clockwise, -180..180.
        #[arg(long, allow_negative_numbers = true)]
        rotate: Option<f64>,
        /// A selector or x,y where the fingers meet. Default: the middle of the screen.
        #[arg(long)]
        at: Option<String>,
        /// Milliseconds the fingers move (default 500).
        #[arg(long)]
        duration: Option<u64>,
    },
    /// Scroll the content up or down, optionally until an element shows.
    Scroll {
        direction: String,
        #[arg(long)]
        until: Option<String>,
        #[arg(long)]
        max: Option<u32>,
    },
    /// Type text into the focused field (pasted, so any language works).
    Type {
        text: String,
        /// Replace what the field holds.
        #[arg(long)]
        clear: bool,
        /// Press Return if the input's effect was observed; otherwise withhold it.
        #[arg(long)]
        submit: bool,
    },
    /// Press a key: return, esc, tab, cmd+a …
    Key { key: String },
    /// Press a hardware button: home, lock, volume_up, volume_down.
    Button { button: String },
    /// Launch an app by bundle id or name.
    Launch {
        app: String,
        /// Quit it first so it starts from its first screen.
        #[arg(long)]
        restart: bool,
        /// Environment variable for the app's process, repeatable: --env NAME=value. Starts the app anew.
        #[arg(long = "env", value_name = "K=V")]
        env: Vec<String>,
        /// Launch arguments for the app, after `--`: iph launch <app> -- -key value. Starts the app anew.
        #[arg(last = true)]
        arguments: Vec<String>,
    },
    /// List installed apps.
    Apps,
    /// Save a screenshot (PNG).
    Screenshot {
        #[arg(short, long)]
        output: Option<String>,
    },
    /// Record the screen to an MP4 file while other commands run: start, stop, status.
    Record {
        action: String,
        /// start: the file to write (default: iph-record-<time>.mp4 in this directory).
        #[arg(short, long)]
        output: Option<String>,
        /// start: the display to record (default: the one in use on a phone with two displays, else the first that shows the screen).
        #[arg(long)]
        display: Option<i64>,
    },
    /// Run a JSON array of steps from a file or stdin (-).
    Batch { file: String },
    /// Run JavaScript from a file or stdin (-): read(), tap(), scroll() … as functions.
    Script {
        file: String,
        /// Argument the script reads as `args.name`, repeatable: --arg name=value
        #[arg(long = "arg", value_name = "K=V")]
        args: Vec<String>,
    },
    /// Run a saved script by name (see `iph scripts`).
    Run {
        name: String,
        /// Argument the script reads as `args.name`, repeatable: --arg name=value
        #[arg(long = "arg", value_name = "K=V")]
        args: Vec<String>,
    },
    /// List saved scripts.
    Scripts,
    /// Serve the same commands as MCP tools over stdio.
    Mcp {
        /// Offer only these tools (comma separated, e.g. read,find,run).
        #[arg(long, value_delimiter = ',')]
        tools: Vec<String>,
    },
    /// Do Not Disturb on the phone: on, off (back to the Focus that was on), status.
    Dnd { action: String },
    /// Capture the phone's log while a test runs and read it back: start, read, stop, status.
    Syslog {
        action: String,
        /// start: keep only this process, by executable name (Settings is Preferences). Repeatable.
        #[arg(long)]
        process: Vec<String>,
        /// start: keep only this app's entries, by bundle id or name as `iph launch` takes it. Repeatable; with --process an entry passes if it matches any.
        #[arg(long)]
        app: Vec<String>,
        /// start: keep only this pid (the phone filters it).
        #[arg(long)]
        pid: Option<u32>,
        /// start: lowest level kept: debug, info, notice, error, fault.
        #[arg(long)]
        level: Option<String>,
        /// start: also write the kept entries to this JSON Lines file. Without it nothing is written to disk.
        #[arg(short, long)]
        output: Option<String>,
        /// start, with -o: move the file to <file>.1 when it reaches this size in MiB (1 to 4096, default 64).
        #[arg(long)]
        max_mb: Option<u64>,
        /// start: most entries the buffer holds (1 to 1000000, default 100000). The oldest are dropped.
        #[arg(long)]
        buffer_entries: Option<u64>,
        /// start: most memory the buffer takes in MiB (1 to 1024, default 64).
        #[arg(long)]
        buffer_mb: Option<u64>,
        /// read: only entries after this seq: the `cursor` of the previous read (default: from the oldest kept).
        #[arg(long)]
        after: Option<u64>,
        /// read, stop: only entries whose message matches this regular expression.
        #[arg(long)]
        grep: Option<String>,
        /// read: most entries returned, the oldest first. stop: most entries returned, the latest (default 50, at most 1000).
        #[arg(long)]
        max: Option<u64>,
    },
    /// Stop the background daemon for this phone.
    Stop,
    /// Internal: the resident process.
    #[command(hide = true)]
    Daemon,
    /// Internal: send a raw daemon command (recovery tests).
    #[command(hide = true)]
    Debug { command: String },
}

fn print(value: &Value, human_out: bool, cmd: &str) {
    if human_out {
        println!("{}", human::render(cmd, value));
    } else if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        println!("{}", serde_json::to_string_pretty(value).unwrap());
    } else {
        println!("{value}");
    }
}

fn exit_code(value: &Value) -> i32 {
    if value["ok"] == true {
        return 0;
    }
    match value["outcome"].as_str() {
        Some("unknown") => 2,
        _ => 1,
    }
}

/// `--arg name=value` pairs as the object a script reads as `args`. Values
/// stay text; a script that wants a number says `Number(args.n)`.
fn script_args(pairs: &[String]) -> Result<Map<String, Value>, Fail> {
    let mut map = Map::new();
    for pair in pairs {
        let Some((k, v)) = pair.split_once('=') else {
            return Err(Fail::invalid(format!("--arg expects name=value, got `{pair}`")));
        };
        map.insert(k.to_string(), json!(v));
    }
    Ok(map)
}

fn swipe_args(direction: &str, duration: Option<u64>, drag: bool) -> Result<Value, Fail> {
    let nums: Vec<f64> = direction
        .split(',')
        .filter_map(|p| p.trim().parse::<f64>().ok())
        .collect();
    let mut v = if direction.contains(',') {
        if nums.len() != 4 {
            return Err(Fail::invalid("swipe takes a direction or x1,y1,x2,y2"));
        }
        json!({ "from": [nums[0], nums[1]], "to": [nums[2], nums[3]] })
    } else {
        json!({ "direction": direction })
    };
    if let Some(d) = duration {
        v["duration_ms"] = json!(d);
    }
    v["drag"] = json!(drag);
    Ok(v)
}

fn launch_args(app: &str, restart: bool, env: &[String], arguments: &[String]) -> Result<Value, Fail> {
    let mut environment = serde_json::Map::new();
    for pair in env {
        let (name, value) = pair
            .split_once('=')
            .filter(|(name, _)| !name.is_empty())
            .ok_or_else(|| Fail::invalid(format!("--env takes NAME=value, got \"{pair}\"")))?;
        environment.insert(name.to_string(), json!(value));
    }
    Ok(json!({ "app": app, "restart": restart, "arguments": arguments, "environment": environment }))
}

fn main() {
    let cli = Cli::parse();
    events::init_tracing();
    // The command futures nest deeply; give the thread that polls them room.
    let worker = std::thread::Builder::new()
        .name("iph-main".into())
        .stack_size(64 << 20)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(16 << 20)
                .build()
                .expect("tokio runtime");
            let code = runtime.block_on(run(cli));
            // Background tasks (tunnel packet pump) must not hold the process open.
            runtime.shutdown_background();
            code
        })
        .expect("spawn main thread");
    std::process::exit(worker.join().unwrap_or(1));
}

async fn run(cli: Cli) -> i32 {
    let started = Instant::now();
    let human_out = cli.human;

    // Commands that need no phone.
    match &cli.command {
        Cmd::Diagnose(args) => {
            // Run one finite iph command without a shell. Reject recursive
            // collectors and interactive/long-lived commands.
            if !args.command.is_empty() {
                let child = Cli::try_parse_from(std::iter::once("iph".to_string()).chain(args.command.iter().cloned()));
                if child.is_ok_and(|c| matches!(c.command, Cmd::Diagnose(_) | Cmd::Daemon | Cmd::Mcp { .. } | Cmd::Setup { .. })) {
                    let v = envelope(Err(Fail::invalid("diagnose can capture a finite iph command; diagnose, daemon, mcp and setup cannot be nested")), started);
                    print(&v, human_out, "diagnose");
                    return 1;
                }
            }
            let v = envelope(diagnose::run(args.clone(), cli.udid.clone()).await, started);
            print(&v, human_out, "diagnose");
            return exit_code(&v);
        }
        Cmd::Scripts => {
            let v = envelope(script::cmd_scripts(), started);
            print(&v, human_out, "scripts");
            return exit_code(&v);
        }
        Cmd::Devices => {
            let v = envelope(devices::cmd_devices(cli.udid.clone()).await, started);
            print(&v, human_out, "devices");
            return exit_code(&v);
        }
        _ => {}
    }

    let udid = match client::resolve_udid(cli.udid.clone()).await {
        Ok(u) => u,
        Err(f) => {
            let v = envelope(Err(f), started);
            print(&v, human_out, "error");
            return exit_code(&v);
        }
    };

    let (cmd, args): (&str, Value) = match cli.command {
        Cmd::Connect(args) => {
            let v = envelope(connect::run(&udid, args).await, started);
            print(&v, human_out, "connect");
            return exit_code(&v);
        }
        Cmd::Daemon => {
            return match daemon::run(udid).await {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("iph daemon: {e:#}");
                    1
                }
            };
        }
        Cmd::Mcp { tools } => {
            return match mcp::serve(udid, tools).await {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("iph mcp: {e:#}");
                    1
                }
            };
        }
        Cmd::Setup { fix_wifi } => return setup::run(&udid, fix_wifi).await,
        Cmd::Stop => {
            let last = client::stop_daemon(&udid).await;
            let v = envelope(Ok(fields! { "stopped": last.is_some() }), started);
            print(&v, human_out, "stop");
            return 0;
        }
        Cmd::Debug { command } => {
            let v = client::request(&udid, &command, Value::Null).await;
            print(&v, human_out, "debug");
            return exit_code(&v);
        }
        Cmd::Doctor { no_touch_test, fix_wifi } => ("doctor", json!({ "no_touch_test": no_touch_test, "fix_wifi": fix_wifi })),
        Cmd::Read { app, all, full, raw, max_age } => (
            "read",
            json!({ "app": app, "all": all, "full": full, "raw": raw, "max_age_ms": max_age }),
        ),
        Cmd::Find { selector, all } => ("find", json!({ "selector": selector, "all": all })),
        Cmd::Tap { target, long, double, no_wait } => {
            ("tap", json!({ "target": target, "long_ms": long, "double": double, "no_wait": no_wait }))
        }
        Cmd::Swipe { direction, duration, drag } => match swipe_args(&direction, duration, drag) {
            Ok(v) => ("swipe", v),
            Err(f) => {
                let v = envelope(Err(f), started);
                print(&v, human_out, "swipe");
                return exit_code(&v);
            }
        },
        Cmd::Pinch { direction, scale, rotate, at, duration } => (
            "pinch",
            json!({ "direction": direction, "scale": scale, "rotate": rotate, "at": at, "duration_ms": duration }),
        ),
        Cmd::Scroll { direction, until, max } => (
            "scroll",
            json!({ "direction": direction, "until": until, "max": max }),
        ),
        Cmd::Type { text, clear, submit } => {
            ("type", json!({ "text": text, "clear": clear, "submit": submit }))
        }
        Cmd::Key { key } => ("key", json!({ "key": key })),
        Cmd::Button { button } => ("button", json!({ "button": button })),
        Cmd::Dnd { action } => ("dnd", json!({ "action": action })),
        Cmd::Syslog { action, process, app, pid, level, output, max_mb, buffer_entries, buffer_mb, after, grep, max } => {
            // The daemon writes the file, so the path must not depend on its cwd.
            let output = output.map(|o| std::path::absolute(&o).map(|p| p.display().to_string()).unwrap_or(o));
            let process = (!process.is_empty()).then_some(process);
            let app = (!app.is_empty()).then_some(app);
            ("syslog", json!({
                "action": action, "process": process, "app": app, "pid": pid, "level": level, "output": output,
                "max_mb": max_mb, "buffer_entries": buffer_entries, "buffer_mb": buffer_mb,
                "after": after, "grep": grep, "max": max,
            }))
        }
        Cmd::Launch { app, restart, env, arguments } => match launch_args(&app, restart, &env, &arguments) {
            Ok(v) => ("launch", v),
            Err(f) => {
                let v = envelope(Err(f), started);
                print(&v, human_out, "launch");
                return exit_code(&v);
            }
        },
        Cmd::Apps => ("apps", Value::Null),
        Cmd::Screenshot { output } => {
            let name = output.unwrap_or_else(|| {
                format!("iph-screenshot-{}.png", clock::now_utc().replace([':', '-'], ""))
            });
            // The daemon writes the file, so the path must not depend on its cwd.
            let path = std::path::absolute(&name)
                .map(|p| p.display().to_string())
                .unwrap_or(name);
            ("screenshot", json!({ "path": path }))
        }
        Cmd::Record { action, output, display } => {
            let output = match (action.as_str(), output) {
                ("start", None) => Some(format!("iph-record-{}.mp4", clock::file_stamp())),
                (_, output) => output,
            };
            let path = output.map(|o| std::path::absolute(&o).map(|p| p.display().to_string()).unwrap_or(o));
            ("record", json!({ "action": action, "path": path, "display": display }))
        }
        Cmd::Batch { file } => {
            let text = if file == "-" {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s).map(|_| s)
            } else {
                std::fs::read_to_string(&file)
            };
            let steps = text
                .map_err(|e| Fail::invalid(format!("{file}: {e}")))
                .and_then(|t| {
                    serde_json::from_str::<Value>(&t)
                        .map_err(|e| Fail::invalid(format!("{file}: not JSON: {e}")))
                })
                .and_then(|v| match v {
                    Value::Array(_) => Ok(v),
                    _ => Err(Fail::invalid("batch needs a JSON array of steps")),
                });
            match steps {
                Ok(steps) => ("batch", json!({ "steps": steps })),
                Err(f) => {
                    let v = envelope(Err(f), started);
                    print(&v, human_out, "batch");
                    return exit_code(&v);
                }
            }
        }
        Cmd::Script { file, args } => {
            let code = if file == "-" {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s).map(|_| s)
            } else {
                std::fs::read_to_string(&file)
            };
            let request = code
                .map_err(|e| Fail::invalid(format!("{file}: {e}")))
                .and_then(|code| Ok(script::request(&code, &script_args(&args)?, None)));
            match request {
                Ok(request) => ("script", request),
                Err(f) => {
                    let v = envelope(Err(f), started);
                    print(&v, human_out, "script");
                    return exit_code(&v);
                }
            }
        }
        Cmd::Run { name, args } => {
            let request = script::load(&name).and_then(|(code, source)| {
                Ok(script::request(&code, &script_args(&args)?, Some((&name, &source))))
            });
            match request {
                Ok(request) => ("script", request),
                Err(f) => {
                    let v = envelope(Err(f), started);
                    print(&v, human_out, "script");
                    return exit_code(&v);
                }
            }
        }
        Cmd::Scripts | Cmd::Devices | Cmd::Diagnose(_) => unreachable!(),
    };

    let value = client::request(&udid, cmd, args).await;
    print(&value, human_out, cmd);
    exit_code(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn launch_env_splits_at_the_first_equals() {
        let v = launch_args("a", false, &strings(&["TZ=UTC", "TOKEN=a=b", "EMPTY="]), &strings(&["-k", "v"])).unwrap();
        assert_eq!(v["environment"], json!({"TZ": "UTC", "TOKEN": "a=b", "EMPTY": ""}));
        assert_eq!(v["arguments"], json!(["-k", "v"]));
    }

    #[test]
    fn syslog_flags_reach_the_command() {
        use clap::Parser;
        let parse = |args: &[&str]| Cli::try_parse_from(std::iter::once("iph").chain(args.iter().copied())).unwrap();
        let Cmd::Syslog { action, after, grep, max, .. } =
            parse(&["syslog", "read", "--after", "120", "--grep", "row", "--max", "5"]).command
        else {
            panic!("syslog read")
        };
        assert_eq!((action.as_str(), after, grep.as_deref(), max), ("read", Some(120), Some("row"), Some(5)));
        let Cmd::Syslog { process, app, output, max_mb, buffer_entries, buffer_mb, .. } = parse(&[
            "syslog", "start", "--process", "A", "--process", "B", "--app", "com.naver.papago", "--app", "설정",
            "-o", "cap.jsonl", "--max-mb", "8", "--buffer-entries", "500", "--buffer-mb", "2",
        ])
        .command
        else {
            panic!("syslog start")
        };
        assert_eq!((process, app), (strings(&["A", "B"]), strings(&["com.naver.papago", "설정"])));
        assert_eq!((output.as_deref(), max_mb, buffer_entries, buffer_mb), (Some("cap.jsonl"), Some(8), Some(500), Some(2)));
    }

    #[test]
    fn launch_env_without_a_name_or_equals_is_not_sent() {
        for bad in ["NOEQUALS", "=v", ""] {
            let f = launch_args("a", false, &strings(&[bad]), &[]).unwrap_err();
            assert_eq!(f.code, "invalid_argument");
        }
    }
}
