//! `iph mcp`: the commands as MCP tools over stdio (JSON-RPC 2.0, one message
//! per line). Each tool forwards to the same daemon the CLI uses, so results
//! have the same shape.

use anyhow::Result;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::{client, recipe, reply::envelope};

const PROTOCOL_VERSION: &str = "2025-06-18";

const SELECTOR_DOC: &str = "Selector: a plain string is an exact label (the name part of a \"name, value\" row also counts). \
Or combine terms: label=\"exact\", label~=\"contains\", label/regex/, role=button, id=<identifier or element id>, \
value~=…, within=\"<selector>\", index=N. An object works too: {label, label_contains, label_regex, value, \
value_contains, role, id, id_contains, id_regex, within, index}. More than one match is an error that lists the candidates.";

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
    })
}

fn tools() -> Vec<Value> {
    let selector = json!({ "description": SELECTOR_DOC, "type": ["string", "object"] });
    vec![
        tool(
            "iph_read",
            "Read the iPhone's current screen as a list of elements (id, role, label, value, frame in points, traits, enabled). \
             Read before acting; coordinates and labels come from here. Only on-screen elements unless all=true.",
            json!({
                "app": { "type": "boolean", "description": "Only the foreground app's elements." },
                "all": { "type": "boolean", "description": "Include elements outside the screen." },
                "full": { "type": "boolean", "description": "Wait ~0.6 s more for accessibility identifiers." },
                "max_age_ms": { "type": "integer", "description": "Accept a cached reading this recent instead of asking the phone (a fresh read takes about 1.2 s)." },
            }),
            &[],
        ),
        tool(
            "iph_find",
            "Find elements on screen that match a selector.",
            json!({ "selector": selector, "all": { "type": "boolean" } }),
            &["selector"],
        ),
        tool(
            "iph_tap",
            "Tap an element (selector) or a point (\"x,y\" in points). The result says whether the screen changed. \
             A target whose label is a guarded word (pay, send, order …) returns outcome \"blocked\" with code confirm_required: \
             ask the user before passing `confirm` with a new one-time id. Never retry a tap whose outcome is \"unknown\".",
            json!({
                "target": { "description": format!("A selector, or \"x,y\". {SELECTOR_DOC}"), "type": ["string", "object"] },
                "long_ms": { "type": "integer", "description": "Hold this long instead of a short tap." },
                "double": { "type": "boolean" },
                "confirm": { "type": "string", "description": "One-time op_id that allows a guarded tap." },
            }),
            &["target"],
        ),
        tool(
            "iph_swipe",
            "Swipe the finger up, down, left or right across the middle of the screen, or between two points.",
            json!({
                "direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
                "from": { "type": "array", "items": { "type": "number" }, "minItems": 2, "maxItems": 2 },
                "to": { "type": "array", "items": { "type": "number" }, "minItems": 2, "maxItems": 2 },
                "duration_ms": { "type": "integer" },
                "drag": { "type": "boolean", "description": "Slow to a stop before lifting (a drag) instead of flicking." },
            }),
            &[],
        ),
        tool(
            "iph_scroll",
            "Scroll the content. \"down\" shows what is further down. With `until`, keeps scrolling until that element is on screen.",
            json!({
                "direction": { "type": "string", "enum": ["up", "down"] },
                "until": selector,
                "max": { "type": "integer", "description": "Most scrolls to try (default 10)." },
            }),
            &["direction"],
        ),
        tool(
            "iph_type",
            "Type text into the field that has the keyboard (tap the field first). Pasted through the clipboard, so Korean and emoji work; \
             the user's clipboard is put back and the field's value is checked.",
            json!({
                "text": { "type": "string" },
                "clear": { "type": "boolean", "description": "Replace what the field holds." },
                "submit": { "type": "boolean", "description": "Press Return afterwards." },
            }),
            &["text"],
        ),
        tool(
            "iph_key",
            "Press a key: return, esc, tab, space, backspace, delete, arrows, or a combination like cmd+a.",
            json!({ "key": { "type": "string" } }),
            &["key"],
        ),
        tool(
            "iph_button",
            "Press a hardware button.",
            json!({ "button": { "type": "string", "enum": ["home", "lock", "volume_up", "volume_down"] } }),
            &["button"],
        ),
        tool(
            "iph_launch",
            "Bring an app to the front by bundle id (com.apple.Preferences) or by its name.",
            json!({
                "app": { "type": "string" },
                "restart": { "type": "boolean", "description": "Quit the app first so it starts from its first screen." },
            }),
            &["app"],
        ),
        tool("iph_apps", "List installed apps with their bundle ids.", json!({}), &[]),
        tool(
            "iph_screenshot",
            "Save a PNG screenshot to a file on the Mac. Use iph_read for content; this is for looks.",
            json!({ "path": { "type": "string", "description": "Absolute path of the PNG to write." } }),
            &["path"],
        ),
        tool(
            "iph_batch",
            "Run several steps in one call, stopping at the first failure. Each step is a one-key object: \
             {\"launch\": \"…\"}, {\"wait_for\": selector}, {\"tap\": selector}, {\"type\": {\"text\": \"…\"}}, {\"swipe\": \"up\"}, \
             {\"scroll_until\": selector}, {\"read_field\": {\"name\", \"selector\", \"take\"}}, {\"read_list\": {\"name\", \"row\", \"columns\"}}, \
             {\"assert\": selector}, {\"button\": \"home\"}, {\"key\": \"return\"}, {\"sleep\": ms}.",
            json!({ "steps": { "type": "array", "items": { "type": "object" } } }),
            &["steps"],
        ),
        tool(
            "iph_run",
            "Run a named recipe (see iph_recipes) in one call and get its outputs with per-step timing.",
            json!({
                "recipe": { "type": "string" },
                "args": { "type": "object", "description": "Recipe arguments, name → value." },
                "confirm": { "type": "string", "description": "One-time op_id for steps marked confirm: true." },
            }),
            &["recipe"],
        ),
        tool("iph_recipes", "List available recipes.", json!({}), &[]),
        tool(
            "iph_doctor",
            "Check that the phone is ready. Each check says what the user must do when it fails.",
            json!({ "no_touch_test": { "type": "boolean", "description": "Skip the check that scrolls Settings down and back." } }),
            &[],
        ),
    ]
}

async fn call(udid: &str, name: &str, args: Value) -> Value {
    let started = std::time::Instant::now();
    let args = if args.is_null() { json!({}) } else { args };
    match name {
        "iph_recipes" => envelope(recipe::cmd_recipes(), started),
        "iph_read" | "iph_find" | "iph_tap" | "iph_swipe" | "iph_scroll" | "iph_type" | "iph_key"
        | "iph_button" | "iph_launch" | "iph_apps" | "iph_screenshot" | "iph_batch" | "iph_run"
        | "iph_doctor" => {
            let cmd = name.trim_start_matches("iph_");
            let args = if cmd == "apps" { Value::Null } else { args };
            client::request(udid, cmd, args).await
        }
        other => envelope(
            Err(crate::reply::Fail::invalid(format!("unknown tool `{other}`"))),
            started,
        ),
    }
}

fn response(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Tools this server offers: all of them, or the ones named with `--tools`.
fn offered(only: &[String]) -> Vec<Value> {
    tools()
        .into_iter()
        .filter(|t| {
            let name = t["name"].as_str().unwrap_or("");
            only.is_empty() || only.iter().any(|o| o == name || format!("iph_{o}") == name)
        })
        .collect()
}

async fn handle(udid: &str, only: &[String], msg: &Value) -> Option<Value> {
    let method = msg.get("method").and_then(|m| m.as_str())?;
    // Notifications carry no id and get no answer.
    let id = msg.get("id")?;
    let empty = Map::new();
    let params = msg.get("params").and_then(|p| p.as_object()).unwrap_or(&empty);
    Some(match method {
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or(PROTOCOL_VERSION);
            response(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "iph", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "Operate a real iPhone through its accessibility tree. Read the screen with iph_read, act with iph_tap / iph_type / iph_scroll, \
                                     and check `screen_changed` in each result. For a known sequence prefer iph_run or iph_batch: one call instead of one per step. \
                                     Outcome \"unknown\" means input may have gone out: read the screen, never resend. Outcome \"blocked\" means the safety guard \
                                     stopped a pay/send/order tap: ask the user before confirming.",
                }),
            )
        }
        "ping" => response(id, json!({})),
        "tools/list" => response(id, json!({ "tools": offered(only) })),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(|n| n.as_str()) else {
                return Some(error(id, -32602, "tools/call needs a tool name"));
            };
            if !offered(only).iter().any(|t| t["name"] == name) {
                return Some(error(id, -32602, "this server does not offer that tool"));
            }
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            let result = call(udid, name, args).await;
            let is_error = result["ok"] != true;
            response(
                id,
                json!({
                    "content": [{ "type": "text", "text": result.to_string() }],
                    "isError": is_error,
                }),
            )
        }
        "resources/list" => response(id, json!({ "resources": [] })),
        "prompts/list" => response(id, json!({ "prompts": [] })),
        _ => error(id, -32601, "method not found"),
    })
}

/// Serve MCP on stdio. `only` limits the tools on offer (empty = all).
pub async fn serve(udid: String, only: Vec<String>) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(line) {
            Ok(msg) => handle(&udid, &only, &msg).await,
            Err(_) => Some(error(&Value::Null, -32700, "parse error")),
        };
        if let Some(reply) = reply {
            stdout.write_all(format!("{reply}\n").as_bytes()).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_list_covers_the_spec() {
        let names: Vec<String> = tools()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        for expected in [
            "iph_read", "iph_find", "iph_tap", "iph_swipe", "iph_scroll", "iph_type", "iph_button",
            "iph_launch", "iph_batch", "iph_run", "iph_recipes", "iph_doctor",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
        for t in tools() {
            assert_eq!(t["inputSchema"]["type"], "object");
            assert!(!t["description"].as_str().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn initialize_and_notifications() {
        let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{}}});
        let r = handle("X", &[], &init).await.unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(r["result"]["serverInfo"]["name"], "iph");
        let note = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert!(handle("X", &[], &note).await.is_none());
        let unknown = json!({"jsonrpc":"2.0","id":2,"method":"nope"});
        assert_eq!(handle("X", &[], &unknown).await.unwrap()["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn tools_can_be_limited() {
        let only = vec!["read".to_string(), "iph_find".to_string()];
        let list = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
        let r = handle("X", &only, &list).await.unwrap();
        let names: Vec<&str> = r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["iph_read", "iph_find"]);
        // A tool that is not on offer cannot be called either.
        let call = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iph_tap","arguments":{"target":"x"}}});
        assert_eq!(handle("X", &only, &call).await.unwrap()["error"]["code"], -32602);
    }
}
