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
    if let f @ Value::Object(_) = &e["frame"] {
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

fn type_observation(v: &Value, out: &mut Vec<String>) {
    if v.get("value_changed").is_some() {
        let parts: Vec<String> = [
            "verified",
            "value_before",
            "value",
            "value_changed",
            "screen_changed",
            "submit_sent",
        ]
        .iter()
        .map(|key| format!("{key}={}", v[*key]))
        .collect();
        out.push(format!("  {}", parts.join("  ")));
    }
    if let Some(message) = v["warning"]["message"].as_str() {
        out.push(format!("  ⚠ {}: {message}", s(&v["warning"]["code"])));
        if let Some(hint) = v["warning"]["hint"].as_str() {
            out.push(format!("  → {hint}"));
        }
    }
}

fn size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{} KiB", b / 1024),
        b => format!("{:.1} MiB", b as f64 / (1024.0 * 1024.0)),
    }
}

fn syslog_entry(e: &Value) -> String {
    let mut line = format!("  {:>7} {} {:<6} {}[{}]", s(&e["seq"]), s(&e["time"]), s(&e["level"]), s(&e["process"]), s(&e["pid"]));
    if let Some(sub) = e["subsystem"].as_str() {
        line.push_str(&format!(" {sub}/{}", s(&e["category"])));
    }
    line.push_str(&format!(": {}", s(&e["message"]).replace('\n', " ⏎ ")));
    if e["message_cut"] == true {
        line.push_str(" …");
    }
    line
}

/// What a capture is and holds: `start`, `status` and `stop`.
fn syslog_status(v: &Value, out: &mut Vec<String>) {
    if v["state"] == "none" {
        out.push("✓ syslog: no capture".into());
        return;
    }
    out.push(format!(
        "✓ syslog {}  seen {}  kept {}  unparsed {}",
        s(&v["state"]),
        s(&v["seen"]),
        s(&v["kept"]),
        s(&v["unparsed"])
    ));
    let b = &v["buffer"];
    let mut buffer = format!(
        "  buffer {}/{} entries, {} of {}",
        s(&b["entries"]),
        s(&b["max_entries"]),
        size(b["bytes"].as_u64().unwrap_or(0)),
        size(b["max_bytes"].as_u64().unwrap_or(0))
    );
    if let (Some(oldest), Some(newest)) = (b["oldest_seq"].as_u64(), b["newest_seq"].as_u64()) {
        buffer.push_str(&format!(", seq {oldest} to {newest}"));
    }
    buffer.push_str(&format!(", {} evicted", s(&b["evicted"])));
    out.push(buffer);
    let mut only = Vec::new();
    if let Some(a) = v["app"].as_array() {
        only.push(format!("app {}", a.iter().map(s).collect::<Vec<_>>().join(", ")));
    }
    if let Some(p) = v["process"].as_array() {
        only.push(format!("process {}", p.iter().map(s).collect::<Vec<_>>().join(", ")));
    }
    if !v["pid"].is_null() {
        only.push(format!("pid {}", s(&v["pid"])));
    }
    if !v["level"].is_null() {
        only.push(format!("level {} and up", s(&v["level"])));
    }
    if !only.is_empty() {
        out.push(format!("  only {}", only.join("  ")));
    }
    if let Some(path) = v["path"].as_str() {
        out.push(format!("  file {path}  {} rotations", s(&v["rotations"])));
    }
    syslog_tail(v, out);
}

fn syslog_tail(v: &Value, out: &mut Vec<String>) {
    if let Some(reason) = v["ended_reason"].as_str() {
        out.push(format!("  ended: {reason}"));
    }
    if let Some(hint) = v["hint"].as_str() {
        out.push(format!("  → {hint}"));
    }
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
        if let step @ Value::Object(_) = &v["failed_step"] {
            out.push(format!("  step {} ({})", s(&step["index"]), s(&step["kind"])));
        }
        if let call @ Value::Object(_) = &v["failed_call"] {
            out.push(match call["line"].as_u64() {
                Some(line) => format!("  line {line}: {}", s(&call["cmd"])),
                None => format!("  in {}", s(&call["cmd"])),
            });
        }
        if let Some(line) = v["line"].as_u64() {
            out.push(format!("  line {line}"));
        }
        if let Some(sent) = v["inputs_sent"].as_u64().filter(|n| *n > 0) {
            out.push(format!("  {sent} calls had already sent input"));
        }
        if let Some(labels) = v["screen_summary"]["labels"].as_array() {
            let l: Vec<String> = labels.iter().map(s).collect();
            out.push(format!("  on screen: {}", l.join(" · ")));
        }
        type_observation(v, &mut out);
        out.push(format!("  {} ms", s(&v["elapsed_ms"])));
        return out.join("\n");
    }
    match cmd {
        "connect" => {
            out.push(format!("✓ {}  wifi  {}  UDID verified, address saved", s(&v["udid"]), s(&v["ip"])));
            out.push(s(&v["hint"]));
        }
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
        "diagnose" => {
            out.push(s(&v["report"]));
            for finding in v["findings"].as_array().into_iter().flatten() {
                out.push(format!("  {}", s(finding)));
            }
            out.push(s(&v["hint"]));
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
        "scripts" => {
            for r in v["scripts"].as_array().into_iter().flatten() {
                out.push(format!("{:<24} {}  [{}]", s(&r["name"]), s(&r["description"]), s(&r["source"])));
            }
        }
        "syslog" => match v["entries"].as_array() {
            Some(entries) if v.get("cursor").is_some() => {
                let mut head = format!("✓ syslog {}  {} entries  cursor {}", s(&v["state"]), entries.len(), s(&v["cursor"]));
                if v["more"] == true {
                    head.push_str("  more waiting");
                }
                if let Some(dropped) = v["dropped"].as_u64().filter(|d| *d > 0) {
                    head.push_str(&format!("  {dropped} dropped before this read"));
                }
                out.push(head);
                out.extend(entries.iter().map(syslog_entry));
                syslog_tail(v, &mut out);
            }
            Some(entries) => {
                syslog_status(v, &mut out);
                out.push(format!("  matched {}, the latest {} below", s(&v["matched"]), entries.len()));
                out.extend(entries.iter().map(syslog_entry));
            }
            None => syslog_status(v, &mut out),
        },
        "script" | "batch" => {
            for line in v["logs"].as_array().into_iter().flatten() {
                out.push(format!("  {}", s(line)));
            }
            for st in v["steps"].as_array().into_iter().flatten() {
                out.push(format!(
                    "✓ {:>2} {:<13} {:>5} ms",
                    s(&st["index"]),
                    s(&st["kind"]),
                    s(&st["elapsed_ms"])
                ));
                if st["kind"] == "type" {
                    type_observation(st, &mut out);
                }
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
            for key in ["screen_changed", "foreground", "found", "scrolls", "value", "clipboard_restored", "path", "state", "duration_s", "frames", "lost_packets", "stopped", "on", "focus", "set_by_iph"] {
                if v.get("value_changed").is_some() && matches!(key, "value" | "screen_changed") {
                    continue;
                }
                if !v[key].is_null() {
                    parts.push(format!("{key}={}", s(&v[key])));
                }
            }
            let e = &v["element"];
            if e.is_object() {
                let name = ["label", "value", "role"].iter().map(|k| s(&e[*k])).find(|n| !n.is_empty()).unwrap_or_default();
                parts.push(format!("element=\"{name}\""));
            }
            parts.push(format!("{} ms", s(&v["elapsed_ms"])));
            out.push(parts.join("  "));
        }
    }
    type_observation(v, &mut out);
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn type_warnings_and_unknown_observations_are_visible() {
        let mut value = json!({
            "ok": true, "value_before": "", "value": "", "verified": true,
            "value_changed": false, "screen_changed": false, "submit_sent": false,
            "warning": {"code": "value_unchanged", "message": "value is unchanged", "hint": "check paste permission"},
        });
        let text = render("type", &value);
        assert!(text.contains("value_before=\"\""));
        assert!(text.contains("value_changed=false"));
        assert!(text.contains("value_unchanged: value is unchanged"));
        assert!(text.contains("check paste permission"));
        let mut step = value.clone();
        step["kind"] = json!("type");
        let batch = json!({"ok": true, "steps": [step]});
        assert!(render("batch", &batch).contains("value_unchanged: value is unchanged"));
        value["value"] = Value::Null;
        value["value_changed"] = Value::Null;
        value["screen_changed"] = Value::Null;
        value["verified"] = json!(false);
        let text = render("type", &value);
        assert!(text.contains("value=null"));
        assert!(text.contains("screen_changed=null"));
        value["ok"] = json!(false);
        value["error"] = json!({"code": "type_not_verified", "message": "Return withheld"});
        let text = render("type", &value);
        assert!(text.contains("Return withheld"));
        assert!(text.contains("value_changed=null"));
        assert!(text.contains("value_unchanged: value is unchanged"));
    }

    #[test]
    fn syslog_prints_counts_a_page_and_the_matches_of_a_stop() {
        let status = json!({
            "ok": true, "state": "running", "seen": 5000, "kept": 1234, "unparsed": 0,
            "buffer": {"entries": 1234, "bytes": 700_000, "max_entries": 100_000, "max_bytes": 67_108_864,
                       "oldest_seq": 1, "newest_seq": 1234, "evicted": 0},
            "started_at": "2026-11-19T01:00:00Z", "app": ["com.naver.papago"], "process": ["Preferences"], "level": "info",
            "path": "/tmp/cap.jsonl", "rotations": 2,
        });
        let text = render("syslog", &status);
        assert!(text.contains("syslog running  seen 5000  kept 1234  unparsed 0"));
        assert!(text.contains("buffer 1234/100000 entries, 683 KiB of 64.0 MiB, seq 1 to 1234, 0 evicted"));
        assert!(text.contains("only app com.naver.papago  process Preferences  level info and up"));
        assert!(text.contains("file /tmp/cap.jsonl  2 rotations"));
        assert!(render("syslog", &json!({"ok": true, "state": "none"})).contains("no capture"));

        let entry = json!({"seq": 12, "time": "2026-11-19T01:02:03.004005Z", "pid": 412, "process": "Papago", "level": "error",
                           "subsystem": "com.naver.papago", "category": "ui", "message": "line one\nline two", "message_cut": true});
        let page = json!({"ok": true, "state": "ended", "count": 1, "entries": [entry], "cursor": 12, "more": true, "dropped": 3,
                          "ended_reason": "the connection to the phone was reset"});
        let text = render("syslog", &page);
        assert!(text.starts_with("✓ syslog ended  1 entries  cursor 12  more waiting  3 dropped before this read"));
        assert!(text.contains("12 2026-11-19T01:02:03.004005Z error  Papago[412] com.naver.papago/ui: line one ⏎ line two …"));
        assert!(text.contains("ended: the connection to the phone was reset"));

        let mut stopped = status.clone();
        stopped["state"] = json!("ended");
        stopped["matched"] = json!(7);
        stopped["entries"] = json!([entry]);
        let text = render("syslog", &stopped);
        assert!(text.contains("syslog ended  seen 5000"));
        assert!(text.contains("matched 7, the latest 1 below"));
        assert!(text.contains("Papago[412]"));

        let empty = json!({"ok": true, "state": "running", "count": 0, "entries": [], "cursor": 0, "more": false, "dropped": 0,
                           "hint": "No entries yet."});
        let text = render("syslog", &empty);
        assert!(text.starts_with("✓ syslog running  0 entries  cursor 0\n"));
        assert!(!text.contains("dropped") && text.contains("→ No entries yet."));
    }

    #[test]
    fn element_without_label() {
        let v = json!({"ok": true, "element": {"role": "search_field", "value": "검색"}, "elapsed_ms": 5});
        assert_eq!(render("tap", &v), "✓  element=\"검색\"  5 ms");
        let v = json!({"ok": false, "error": {"code": "x", "message": "m"}, "failed_step": {}, "failed_call": {}});
        render("script", &v);
    }
}
