#![allow(clippy::result_large_err)]

use std::{io::Read, time::Instant};

use clap::{Parser, Subcommand};
use iphone_cli::{
    client, daemon, devices, fields, human, mcp,
    recipe,
    reply::{Fail, envelope},
    safety, setup,
};
use serde_json::{Map, Value, json};

#[derive(Parser)]
#[command(
    name = "iph",
    version,
    about = "Read and operate any app on a real iPhone through its accessibility tree",
    long_about = "Read and operate any app on a real iPhone through its accessibility tree.\n\
                  No XCUITest, WebDriverAgent or Xcode signing. Output is JSON; add --human for people.\n\
                  Exit codes: 0 ok, 1 nothing was sent, 2 outcome unknown (check the screen), 3 blocked by the safety guard."
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
    /// Walk through the one-time preparation of a phone.
    Setup,
    /// Check that the phone is ready; says what to do for each problem.
    Doctor {
        /// Skip the check that sends a real touch (it scrolls Settings down and back).
        #[arg(long)]
        no_touch_test: bool,
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
        /// One-time id that allows a guarded tap (pay, send, order …).
        #[arg(long)]
        confirm: Option<String>,
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
        /// Press Return afterwards.
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
    },
    /// List installed apps.
    Apps,
    /// Save a screenshot (PNG).
    Screenshot {
        #[arg(short, long)]
        output: Option<String>,
    },
    /// Run a JSON array of steps from a file or stdin (-).
    Batch { file: String },
    /// Run a recipe.
    Run {
        recipe: String,
        /// Recipe argument, repeatable: --arg name=value
        #[arg(long = "arg", value_name = "K=V")]
        args: Vec<String>,
        /// One-time id for steps marked `confirm: true`.
        #[arg(long)]
        confirm: Option<String>,
    },
    /// List recipes.
    Recipes,
    /// Look up what happened to a confirmed operation.
    Op { op_id: String },
    /// Serve the same commands as MCP tools over stdio.
    Mcp {
        /// Offer only these tools (comma separated, e.g. read,find,run).
        #[arg(long, value_delimiter = ',')]
        tools: Vec<String>,
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
        Some("blocked") => 3,
        _ => 1,
    }
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

fn main() {
    let cli = Cli::parse();
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
        Cmd::Recipes => {
            let v = envelope(recipe::cmd_recipes(), started);
            print(&v, human_out, "recipes");
            return exit_code(&v);
        }
        Cmd::Op { op_id } => {
            let result = safety::lookup(op_id)
                .map_err(|e| Fail::not_sent("ops_log_failed", format!("{e:#}")))
                .map(|records| {
                    let state = records.last().map(|r| r.state.clone());
                    fields! {
                        "op_id": op_id,
                        "found": !records.is_empty(),
                        "state": state,
                        "records": records,
                    }
                });
            let v = envelope(result, started);
            print(&v, human_out, "op");
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
        Cmd::Setup => return setup::run(&udid).await,
        Cmd::Stop => {
            let stopped = client::stop_daemon(&udid).await;
            let v = envelope(Ok(fields! { "stopped": stopped }), started);
            print(&v, human_out, "stop");
            return 0;
        }
        Cmd::Debug { command } => {
            let v = client::request(&udid, &command, Value::Null).await;
            print(&v, human_out, "debug");
            return exit_code(&v);
        }
        Cmd::Doctor { no_touch_test } => ("doctor", json!({ "no_touch_test": no_touch_test })),
        Cmd::Read { app, all, full, raw, max_age } => (
            "read",
            json!({ "app": app, "all": all, "full": full, "raw": raw, "max_age_ms": max_age }),
        ),
        Cmd::Find { selector, all } => ("find", json!({ "selector": selector, "all": all })),
        Cmd::Tap { target, long, double, confirm } => (
            "tap",
            json!({ "target": target, "long_ms": long, "double": double, "confirm": confirm }),
        ),
        Cmd::Swipe { direction, duration, drag } => match swipe_args(&direction, duration, drag) {
            Ok(v) => ("swipe", v),
            Err(f) => {
                let v = envelope(Err(f), started);
                print(&v, human_out, "swipe");
                return exit_code(&v);
            }
        },
        Cmd::Scroll { direction, until, max } => (
            "scroll",
            json!({ "direction": direction, "until": until, "max": max }),
        ),
        Cmd::Type { text, clear, submit } => {
            ("type", json!({ "text": text, "clear": clear, "submit": submit }))
        }
        Cmd::Key { key } => ("key", json!({ "key": key })),
        Cmd::Button { button } => ("button", json!({ "button": button })),
        Cmd::Launch { app, restart } => ("launch", json!({ "app": app, "restart": restart })),
        Cmd::Apps => ("apps", Value::Null),
        Cmd::Screenshot { output } => {
            let name = output.unwrap_or_else(|| {
                format!("iph-screenshot-{}.png", safety::now_utc().replace([':', '-'], ""))
            });
            // The daemon writes the file, so the path must not depend on its cwd.
            let path = std::path::absolute(&name)
                .map(|p| p.display().to_string())
                .unwrap_or(name);
            ("screenshot", json!({ "path": path }))
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
        Cmd::Run { recipe, args, confirm } => {
            let mut map = Map::new();
            for pair in args {
                let Some((k, v)) = pair.split_once('=') else {
                    let v = envelope(
                        Err(Fail::invalid(format!("--arg expects name=value, got `{pair}`"))),
                        started,
                    );
                    print(&v, human_out, "run");
                    return exit_code(&v);
                };
                map.insert(k.to_string(), json!(v));
            }
            ("run", json!({ "recipe": recipe, "args": map, "confirm": confirm }))
        }
        Cmd::Recipes | Cmd::Op { .. } | Cmd::Devices => unreachable!(),
    };

    let value = client::request(&udid, cmd, args).await;
    print(&value, human_out, cmd);
    exit_code(&value)
}
