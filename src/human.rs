//! `--human`: the same results, laid out for a person reading a terminal.

use serde_json::Value;

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn element_line(e: &Value) -> String {
    let mut line = format!("{:<13}", s(&e["role"]));
    if let Some(l) = e["label"].as_str() {
        line.push_str(&format!(" \"{l}\""));
    }
    if let Some(v) = e["value"].as_str() {
        line.push_str(&format!(" = \"{v}\""));
    }
    if let Some(i) = e["identifier"].as_str() {
        line.push_str(&format!(" #{i}"));
    }
    if let Some(f) = e["frame"].as_object() {
        line.push_str(&format!(
            "  @{},{} {}x{}",
            s(&f["x"]),
            s(&f["y"]),
            s(&f["w"]),
            s(&f["h"])
        ));
    }
    if e["enabled"] == false {
        line.push_str("  (disabled)");
    }
    if e["selected"] == true {
        line.push_str("  (selected)");
    }
    if e["focused"] == true {
        line.push_str("  (focused)");
    }
    if e["on_screen"] == false {
        line.push_str("  (off screen)");
    }
    line.push_str(&format!("  [{}]", s(&e["id"])));
    line
}

pub fn render(cmd: &str, v: &Value) -> String {
    let mut out = Vec::new();
    if v["ok"] != true {
        out.push(format!(
            "✗ {} ({}): {}",
            s(&v["error"]["code"]),
            s(&v["outcome"]),
            s(&v["error"]["message"])
        ));
        if let Some(h) = v["error"]["hint"].as_str() {
            out.push(format!("  → {h}"));
        }
        if let Some(c) = v["candidates"].as_array() {
            for e in c {
                out.push(format!("  · {}", element_line(e)));
            }
        }
        if let Some(step) = v["failed_step"].as_object() {
            out.push(format!("  step {} ({})", s(&step["index"]), s(&step["kind"])));
        }
        if let Some(labels) = v["screen_summary"]["labels"].as_array() {
            let l: Vec<String> = labels.iter().map(s).collect();
            out.push(format!("  on screen: {}", l.join(" · ")));
        }
        out.push(format!("  {} ms", s(&v["elapsed_ms"])));
        return out.join("\n");
    }
    match cmd {
        "read" | "find" => {
            if cmd == "read" {
                let apps: Vec<String> = v["apps"]
                    .as_array()
                    .map(|a| a.iter().map(|x| s(&x["name"])).collect())
                    .unwrap_or_default();
                out.push(format!(
                    "{}  {}x{}  {} elements",
                    apps.join(", "),
                    s(&v["screen"]["width"]),
                    s(&v["screen"]["height"]),
                    s(&v["count"])
                ));
            } else {
                out.push(format!("{} match(es) for {}", s(&v["count"]), s(&v["selector"])));
            }
            for e in v["elements"].as_array().into_iter().flatten() {
                let indent = "  ".repeat(e["depth"].as_u64().unwrap_or(0) as usize);
                out.push(format!("{indent}{}", element_line(e)));
            }
        }
        "doctor" => {
            for c in v["checks"].as_array().into_iter().flatten() {
                let mark = match c["status"].as_str() {
                    Some("ok") => "✓",
                    Some("fail") => "✗",
                    Some("warn") => "!",
                    Some("skipped") => "-",
                    _ => "·",
                };
                let req = if c["required"] == true { "required" } else { "recommended" };
                let mut line = format!("{mark} {:<22} {req}", s(&c["check"]));
                if let Some(d) = c["detail"].as_str() {
                    line.push_str(&format!("  ({d})"));
                }
                out.push(line);
                if c["status"] != "ok"
                    && let Some(a) = c["user_action"].as_str() {
                        out.push(format!("    → {a}"));
                    }
            }
            out.push(if v["ready"] == true { "ready".into() } else { "not ready".to_string() });
        }
        "devices" => {
            for d in v["devices"].as_array().into_iter().flatten() {
                out.push(format!(
                    "{}  {}  iOS {}  {}  {}",
                    s(&d["udid"]),
                    s(&d["connection"]),
                    s(&d["ios_version"]),
                    s(&d["model"]),
                    s(&d["state"])
                ));
            }
            if out.is_empty() {
                out.push("no devices".into());
            }
        }
        "apps" => {
            for a in v["apps"].as_array().into_iter().flatten() {
                out.push(format!("{:<44} {}", s(&a["bundle_id"]), s(&a["name"])));
            }
        }
        "recipes" => {
            for r in v["recipes"].as_array().into_iter().flatten() {
                out.push(format!("{:<24} {}  [{}]", s(&r["name"]), s(&r["description"]), s(&r["source"])));
            }
        }
        "run" | "batch" => {
            for st in v["steps"].as_array().into_iter().flatten() {
                out.push(format!(
                    "✓ {:>2} {:<13} {:>5} ms",
                    s(&st["index"]),
                    s(&st["kind"]),
                    s(&st["elapsed_ms"])
                ));
            }
            if let Some(o) = v["outputs"].as_object() {
                for (k, val) in o {
                    out.push(format!("{k} = {}", match val {
                        Value::String(x) => x.clone(),
                        other => serde_json::to_string_pretty(other).unwrap_or_default(),
                    }));
                }
            }
            out.push(format!("{} ms", s(&v["elapsed_ms"])));
        }
        _ => {
            let mut parts = vec!["✓".to_string()];
            for key in ["screen_changed", "foreground", "found", "scrolls", "value", "clipboard_restored", "path", "state", "stopped"] {
                if !v[key].is_null() {
                    parts.push(format!("{key}={}", s(&v[key])));
                }
            }
            if let Some(e) = v["element"].as_object() {
                parts.push(format!("element=\"{}\"", s(&e["label"])));
            }
            parts.push(format!("{} ms", s(&v["elapsed_ms"])));
            out.push(parts.join("  "));
        }
    }
    out.join("\n")
}
