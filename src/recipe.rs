//! Recipes: a fixed sequence of steps run in one go, without a round trip to
//! the agent between steps. `iph batch` runs the same steps from JSON.
//!
//! A step is a one-key map. Kinds: `launch`, `wait_for`, `tap`, `type`,
//! `swipe`, `scroll_until`, `read_field`, `read_list`, `assert`, `button`,
//! `key`, `sleep`. Any step may add `confirm: true`. A step that fails stops
//! the run; nothing is guessed.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use regex::Regex;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    actions::{self, ButtonArgs, KeyArgs, LaunchArgs, ScrollArgs, SwipeArgs, TapArgs, TypeArgs, brief},
    ax::tree::{BuildOptions, Element, Screen},
    device::session::{Fresh, Session, reuse},
    fields,
    i18n::t,
    paths,
    reply::{Fail, Outcome, Res},
    safety::{self, OpRecord},
    selector::{Selector, dedupe_targets},
};

/// Example recipes shipped in the repository's `recipes/` folder.
const BUILTIN: &[(&str, &str)] = &[
    ("settings-version", include_str!("../recipes/settings-version.yaml")),
    ("calculator-multiply", include_str!("../recipes/calculator-multiply.yaml")),
    ("clock-world-list", include_str!("../recipes/clock-world-list.yaml")),
];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub app: Option<String>,
    /// Argument name → default value (null = required).
    #[serde(default)]
    pub args: Map<String, Value>,
    pub steps: Vec<Value>,
    /// Names of read values to return. Empty = all.
    #[serde(default)]
    pub output: Vec<String>,
    #[serde(skip)]
    pub source: String,
}

impl Recipe {
    pub fn parse(text: &str, source: &str) -> Result<Recipe, String> {
        let mut r: Recipe =
            serde_yaml_ng::from_str(text).map_err(|e| format!("{source}: {e}"))?;
        r.source = source.to_string();
        if r.steps.is_empty() {
            return Err(format!("{source}: a recipe needs at least one step"));
        }
        for (i, step) in r.steps.iter().enumerate() {
            split_step(step).map_err(|e| format!("{source}: step {}: {e}", i + 1))?;
        }
        Ok(r)
    }
}

/// Recipe names and where each comes from. Personal recipes win over examples.
pub fn list() -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    let personal = paths::personal_recipes();
    if let Ok(dir) = std::fs::read_dir(&personal) {
        let mut files: Vec<PathBuf> = dir
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("yaml" | "yml")))
            .collect();
        files.sort();
        for path in files {
            let source = path.display().to_string();
            match std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|text| Recipe::parse(&text, &source))
            {
                Ok(r) => out.push((r.name, r.description, source)),
                Err(e) => out.push((
                    path.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string(),
                    format!("invalid: {e}"),
                    source,
                )),
            }
        }
    }
    for (name, text) in BUILTIN {
        if out.iter().any(|(n, _, _)| n == name) {
            continue;
        }
        if let Ok(r) = Recipe::parse(text, &format!("builtin:{name}")) {
            out.push((r.name, r.description, format!("builtin:{name}")));
        }
    }
    out
}

/// Find a recipe by name (personal folder first, then the examples) or by
/// path to a YAML file.
pub fn load(name: &str) -> Result<Recipe, Fail> {
    let as_path = Path::new(name);
    if name.ends_with(".yaml") || name.ends_with(".yml") || name.contains('/') {
        let text = std::fs::read_to_string(as_path)
            .map_err(|e| Fail::not_sent("recipe_not_found", format!("{name}: {e}")))?;
        return Recipe::parse(&text, name).map_err(|e| Fail::not_sent("recipe_invalid", e));
    }
    for ext in ["yaml", "yml"] {
        let path = paths::personal_recipes().join(format!("{name}.{ext}"));
        if let Ok(text) = std::fs::read_to_string(&path) {
            return Recipe::parse(&text, &path.display().to_string())
                .map_err(|e| Fail::not_sent("recipe_invalid", e));
        }
    }
    // A personal file may be named differently from the recipe it holds.
    if let Some((_, _, source)) = list().into_iter().find(|(n, _, s)| n == name && !s.starts_with("builtin:")) {
        let text = std::fs::read_to_string(&source)
            .map_err(|e| Fail::not_sent("recipe_not_found", format!("{source}: {e}")))?;
        return Recipe::parse(&text, &source).map_err(|e| Fail::not_sent("recipe_invalid", e));
    }
    if let Some((_, text)) = BUILTIN.iter().find(|(n, _)| *n == name) {
        return Recipe::parse(text, &format!("builtin:{name}"))
            .map_err(|e| Fail::not_sent("recipe_invalid", e));
    }
    Err(Fail::not_sent("recipe_not_found", format!("no recipe named \"{name}\""))
        .hint(t(
            "`iph recipes`로 목록을 확인하세요. 개인 레시피는 ~/.config/iph/recipes/ 에 둡니다.",
            "List them with `iph recipes`. Personal recipes live in ~/.config/iph/recipes/.",
        )))
}

const KINDS: &[&str] = &[
    "launch",
    "wait_for",
    "tap",
    "type",
    "swipe",
    "scroll_until",
    "read_field",
    "read_list",
    "assert",
    "button",
    "key",
    "sleep",
];

/// Split a step into (kind, body, confirm).
fn split_step(step: &Value) -> Result<(&str, &Value, &Value), String> {
    let map = step.as_object().ok_or("a step is a map like `tap: {...}`")?;
    static NULL: Value = Value::Null;
    let confirm = map.get("confirm").unwrap_or(&NULL);
    let mut kinds = map.iter().filter(|(k, _)| k.as_str() != "confirm");
    let (kind, body) = kinds.next().ok_or("a step needs a kind")?;
    if kinds.next().is_some() {
        return Err("a step has exactly one kind (plus an optional `confirm`)".into());
    }
    if !KINDS.contains(&kind.as_str()) {
        return Err(format!("unknown step kind `{kind}` ({})", KINDS.join(", ")));
    }
    Ok((kind.as_str(), body, confirm))
}

/// Replace `{{name}}` in every string of `v`.
fn substitute(v: &Value, args: &Map<String, Value>) -> Value {
    match v {
        Value::String(s) => {
            let mut out = s.clone();
            for (k, val) in args {
                let needle = format!("{{{{{k}}}}}");
                if out.contains(&needle) {
                    let rep = match val {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    out = out.replace(&needle, &rep);
                }
            }
            Value::String(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| substitute(x, args)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), substitute(x, args)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// A step body that is either the selector itself or `{selector: …, …}`.
fn selector_and_options(body: &Value) -> Result<(Selector, Map<String, Value>), Fail> {
    let (sel_value, options) = match body.as_object() {
        Some(map) if map.contains_key("selector") => {
            let mut rest = map.clone();
            let sel = rest.remove("selector").unwrap();
            (sel, rest)
        }
        _ => (body.clone(), Map::new()),
    };
    let sel = Selector::from_value(&sel_value).map_err(|e| Fail::invalid(e.to_string()))?;
    Ok((sel, options))
}

fn opt_u64(options: &Map<String, Value>, key: &str) -> Option<u64> {
    options.get(key).and_then(|v| v.as_u64())
}

/// Text of an element for `take`. `value` falls back to the part of a
/// "name, value" label after the name.
fn take(el: &Element, what: &str) -> Option<String> {
    match what {
        "label" => el.label.clone(),
        "identifier" => el.identifier.clone(),
        "text" => el.value.clone().or_else(|| el.label.clone()),
        _ => el.value.clone().or_else(|| {
            el.label
                .as_deref()
                .and_then(|l| l.split_once(", "))
                .map(|(_, rest)| rest.to_string())
        }),
    }
}

fn apply_regex(text: Option<String>, pattern: Option<&str>) -> Result<Option<String>, Fail> {
    let (Some(text), Some(pattern)) = (text.clone(), pattern) else {
        return Ok(text);
    };
    let re = Regex::new(pattern).map_err(|e| Fail::invalid(format!("bad regex: {e}")))?;
    Ok(re.captures(&text).map(|c| {
        c.get(1)
            .or_else(|| c.get(0))
            .map(|m| m.as_str().to_string())
            .unwrap_or_default()
    }))
}

/// Append `page` to `all`, skipping the rows both share: the longest run at
/// the end of `all` that equals the start of `page`. Comparing runs instead
/// of single rows keeps two genuinely identical rows apart.
fn merge_rows(all: &mut Vec<Value>, page: Vec<Value>) -> usize {
    let max = all.len().min(page.len());
    let overlap = (0..=max)
        .rev()
        .find(|k| all[all.len() - k..] == page[..*k])
        .unwrap_or(0);
    let added = page.len() - overlap;
    all.extend(page.into_iter().skip(overlap));
    added
}

/// Wrap a step's failure in the recipe-level report: which step, why, and
/// what the screen showed.
fn step_failure(
    index: usize,
    raw: &Value,
    f: Fail,
    kind: &str,
    records: &[Value],
    outputs: &Map<String, Value>,
    ms: u64,
) -> Fail {
    let mut extra = f.extra.clone();
    let summary = extra.remove("screen_summary");
    let mut out = Fail::new(
        f.outcome,
        "step_failed",
        format!("step {index} ({kind}) failed: {}", f.message),
    );
    out.hint = f.hint.clone();
    out = out
        .with("failed_step", json!({ "index": index, "kind": kind, "step": raw, "elapsed_ms": ms }))
        .with("reason", json!({ "code": f.code, "message": f.message }))
        .with("screen_summary", summary.unwrap_or(Value::Null))
        .with("steps", json!(records))
        .with("outputs", json!(outputs));
    for (k, v) in extra {
        out.extra.entry(k).or_insert(v);
    }
    out
}

pub struct RunOptions {
    /// `--confirm <op_id>` for steps marked `confirm: true`.
    pub confirm: Option<String>,
    /// True for `iph batch`: a step's `confirm` may carry its own op_id.
    pub batch: bool,
}

impl Session {
    async fn wait_for(&mut self, sel: &Selector, timeout: Duration) -> Res<Element> {
        let started = Instant::now();
        let full = sel.needs_identifier();
        let mut fresh = Fresh::MaxAge(reuse());
        let last: Option<Screen>;
        loop {
            let (_, screen) = self.screen(fresh, full, BuildOptions::default()).await?;
            if let Some(el) = dedupe_targets(sel.find(&screen)).first() {
                return Ok((*el).clone());
            }
            if started.elapsed() >= timeout {
                last = Some(screen);
                break;
            }
            // Each further look asks the phone again (about 1.1 s).
            fresh = Fresh::Force;
        }
        Err(Fail::not_sent(
            "wait_timeout",
            format!("{} did not appear within {} ms", sel.describe(), timeout.as_millis()),
        )
        .with("screen_summary", last.map(|s| s.summary(12)).unwrap_or(Value::Null)))
    }

    async fn read_field(&mut self, body: &Value) -> Res<(String, Value)> {
        let map = body.as_object().ok_or_else(|| Fail::invalid("read_field needs a map"))?;
        let name = map
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Fail::invalid("read_field needs `name`"))?;
        let sel_value = map.get("selector").ok_or_else(|| Fail::invalid("read_field needs `selector`"))?;
        let sel = Selector::from_value(sel_value).map_err(|e| Fail::invalid(e.to_string()))?;
        let what = map.get("take").and_then(|v| v.as_str()).unwrap_or("value");
        let pattern = map.get("regex").and_then(|v| v.as_str());
        let timeout = Duration::from_millis(opt_u64(map, "timeout_ms").unwrap_or(5000));
        // The element may still be on its way in.
        Box::pin(self.wait_for(&sel, timeout)).await?;
        let (_, screen) = self
            .screen(Fresh::MaxAge(reuse()), sel.needs_identifier() || what == "identifier", BuildOptions::default())
            .await?;
        let el = actions::resolve_one(&sel, &screen)?;
        let text = apply_regex(take(el, what), pattern)?;
        match text {
            Some(text) => Ok((name.to_string(), Value::String(text))),
            None => Err(Fail::not_sent(
                "no_value",
                format!("{} has no {what} to read", sel.describe()),
            )
            .with("element", brief(el))),
        }
    }

    fn rows_on_screen(
        screen: &Screen,
        row: &Selector,
        columns: Option<&Map<String, Value>>,
    ) -> Res<Vec<Value>> {
        let mut out = Vec::new();
        for el in dedupe_targets(row.find(screen)) {
            let Some(cols) = columns else {
                let mut v = json!({});
                if let Some(l) = &el.label {
                    v["label"] = json!(l);
                }
                if let Some(val) = &el.value {
                    v["value"] = json!(val);
                }
                out.push(v);
                continue;
            };
            let mut rec = Map::new();
            for (col, spec) in cols {
                let spec_map = spec.as_object();
                let what = spec_map
                    .and_then(|m| m.get("take"))
                    .and_then(|v| v.as_str())
                    .or_else(|| spec.as_str())
                    .unwrap_or("label");
                let pattern = spec_map.and_then(|m| m.get("regex")).and_then(|v| v.as_str());
                // A column either reads the row itself or an element inside it.
                let source: Option<&Element> = match spec_map.and_then(|m| m.get("selector")) {
                    Some(inner) => {
                        let inner = Selector::from_value(inner).map_err(|e| Fail::invalid(e.to_string()))?;
                        let scoped = Selector {
                            within: Some(Box::new(Selector {
                                id: Some(crate::selector::TextMatch::Exact(el.id.clone())),
                                ..Default::default()
                            })),
                            ..inner
                        };
                        scoped.find(screen).first().copied()
                    }
                    None => Some(el),
                };
                let text = apply_regex(source.and_then(|e| take(e, what)), pattern)?;
                rec.insert(col.clone(), text.map(Value::String).unwrap_or(Value::Null));
            }
            out.push(Value::Object(rec));
        }
        Ok(out)
    }

    async fn read_list(&mut self, body: &Value) -> Res<(String, Value, Value)> {
        let map = body.as_object().ok_or_else(|| Fail::invalid("read_list needs a map"))?;
        let name = map
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Fail::invalid("read_list needs `name`"))?;
        let row = Selector::from_value(map.get("row").ok_or_else(|| Fail::invalid("read_list needs `row`"))?)
            .map_err(|e| Fail::invalid(e.to_string()))?;
        let columns = map.get("columns").and_then(|v| v.as_object());
        let until = match map.get("until") {
            Some(v) => Some(Selector::from_value(v).map_err(|e| Fail::invalid(e.to_string()))?),
            None => None,
        };
        let max_scrolls = opt_u64(map, "max_scrolls").unwrap_or(15).min(200) as u32;
        let limit = opt_u64(map, "limit").unwrap_or(500) as usize;
        let direction = map.get("direction").and_then(|v| v.as_str()).unwrap_or("down").to_string();
        let full = row.needs_identifier() || until.as_ref().is_some_and(|u| u.needs_identifier());

        let mut all: Vec<Value> = Vec::new();
        let mut scrolls = 0u32;
        let mut ended = "max_scrolls";
        loop {
            let (_, screen) = self.screen(Fresh::MaxAge(reuse()), full, BuildOptions::default()).await?;
            merge_rows(&mut all, Self::rows_on_screen(&screen, &row, columns)?);
            if all.len() >= limit {
                all.truncate(limit);
                ended = "limit";
                break;
            }
            if until.as_ref().is_some_and(|u| !u.find(&screen).is_empty()) {
                ended = "until";
                break;
            }
            if scrolls >= max_scrolls {
                break;
            }
            let moved = Box::pin(self.cmd_scroll(ScrollArgs {
                direction: direction.clone(),
                until: None,
                max: None,
            }))
            .await?;
            scrolls += 1;
            if moved.get("screen_changed").and_then(|v| v.as_bool()) != Some(true) {
                ended = "end_of_list";
                break;
            }
        }
        Ok((
            name.to_string(),
            Value::Array(all),
            json!({ "scrolls": scrolls, "ended": ended }),
        ))
    }

    /// A failure when the latest reading shows another app in front than the
    /// one this run launched (a notification was tapped, a link opened …).
    fn unexpected_app(&self, expected_pid: Option<i64>) -> Option<Fail> {
        let expected = expected_pid?;
        let (_, dump, _) = self.ax.as_ref()?.latest()?;
        if dump.applications.iter().any(|a| a.pid == expected) {
            return None;
        }
        let screen = Screen::build(&dump, BuildOptions::default());
        Some(
            Fail::not_sent(
                "unexpected_app",
                format!(
                    "another app came to the front: {}",
                    screen
                        .apps
                        .iter()
                        .filter_map(|a| a.name.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
            .hint(t(
                "알림 배너를 눌렀거나 다른 앱이 열렸을 수 있습니다. 화면을 확인하세요. 집중 모드(방해금지)를 켜 두면 배너가 뜨지 않습니다.",
                "A notification banner may have been tapped or another app opened. Check the screen. A Focus (Do Not Disturb) keeps banners away.",
            ))
            .with("screen_summary", screen.summary(6)),
        )
    }

    /// Run one step. Returns extra fields for the step's record and, for read
    /// steps, the value read.
    async fn run_step(
        &mut self,
        kind: &str,
        body: &Value,
        confirm_op: Option<&str>,
        needs_confirm: bool,
    ) -> Res<(Map<String, Value>, Option<(String, Value)>)> {
        fn no_value(f: Map<String, Value>) -> Res<(Map<String, Value>, Option<(String, Value)>)> {
            Ok((f, None))
        }
        match kind {
            "launch" => {
                let args: LaunchArgs = match body {
                    Value::String(s) => LaunchArgs { app: s.clone(), restart: false },
                    other => actions::parse_args(other)?,
                };
                let app = args.app.clone();
                let r = Box::pin(self.cmd_launch(args)).await?;
                if r.get("foreground").and_then(|v| v.as_bool()) != Some(true) {
                    return Err(Fail::unknown(
                        "launch_not_verified",
                        format!("{app} was launched but is not in front"),
                    ));
                }
                no_value(fields! {
                    "bundle_id": r.get("bundle_id").cloned().unwrap_or(Value::Null),
                    "pid": r.get("pid").cloned().unwrap_or(Value::Null),
                })
            }
            "wait_for" => {
                let (sel, options) = selector_and_options(body)?;
                let timeout = Duration::from_millis(opt_u64(&options, "timeout_ms").unwrap_or(8000));
                let el = Box::pin(self.wait_for(&sel, timeout)).await?;
                no_value(fields! { "element": brief(&el) })
            }
            "assert" => {
                let (sel, _) = selector_and_options(body)?;
                // One patient look: the screen may be a moment behind.
                let el = Box::pin(self.wait_for(&sel, Duration::from_millis(1500))).await.map_err(|mut f| {
                    f.code = "assert_failed".into();
                    f.message = format!("expected {} on screen", sel.describe());
                    f
                })?;
                no_value(fields! { "element": brief(&el) })
            }
            "tap" if body.is_array() => {
                if needs_confirm {
                    return Err(Fail::invalid(
                        "a tap list cannot be confirmed; make the guarded tap a step of its own",
                    ));
                }
                let r = Box::pin(self.tap_sequence(body.as_array().unwrap())).await?;
                no_value(fields! {
                    "screen_changed": r.get("screen_changed").cloned().unwrap_or(Value::Null),
                    "taps": r.get("taps").cloned().unwrap_or(Value::Null),
                })
            }
            "tap" => {
                let (sel, options) = selector_and_options(body)?;
                // Give the target a moment to appear, as a person would.
                let timeout = Duration::from_millis(opt_u64(&options, "timeout_ms").unwrap_or(5000));
                Box::pin(self.wait_for(&sel, timeout)).await?;
                let target = match body.as_object() {
                    Some(m) if m.contains_key("selector") => m["selector"].clone(),
                    _ => body.clone(),
                };
                let r = Box::pin(self.tap_impl(
                    TapArgs {
                        target,
                        long_ms: opt_u64(&options, "long_ms"),
                        double: options.get("double").and_then(|v| v.as_bool()).unwrap_or(false),
                        confirm: confirm_op.map(String::from),
                    },
                    needs_confirm,
                ))
                .await?;
                no_value(fields! {
                    "screen_changed": r.get("screen_changed").cloned().unwrap_or(Value::Null),
                    "element": r.get("element").cloned().unwrap_or(Value::Null),
                })
            }
            "type" => {
                let args: TypeArgs = match body {
                    Value::String(s) => TypeArgs { text: s.clone(), ..Default::default() },
                    other => actions::parse_args(other)?,
                };
                let r = Box::pin(self.cmd_type(args)).await?;
                no_value(fields! {
                    "value": r.get("value").cloned().unwrap_or(Value::Null),
                    "clipboard_restored": r.get("clipboard_restored").cloned().unwrap_or(Value::Null),
                })
            }
            "swipe" => {
                let args: SwipeArgs = match body {
                    Value::String(s) => SwipeArgs { direction: Some(s.clone()), ..Default::default() },
                    other => actions::parse_args(other)?,
                };
                let r = Box::pin(self.cmd_swipe(args)).await?;
                no_value(fields! { "screen_changed": r.get("screen_changed").cloned().unwrap_or(Value::Null) })
            }
            "scroll_until" => {
                let (_, options) = selector_and_options(body)?;
                let until = match body.as_object() {
                    Some(m) if m.contains_key("selector") => m["selector"].clone(),
                    _ => body.clone(),
                };
                let r = Box::pin(self.cmd_scroll(ScrollArgs {
                    direction: options
                        .get("direction")
                        .and_then(|v| v.as_str())
                        .unwrap_or("down")
                        .to_string(),
                    until: Some(until),
                    max: opt_u64(&options, "max").map(|n| n as u32),
                }))
                .await
                .map_err(|mut f| {
                        // Not finding the target fails the step.
                        if f.outcome == Outcome::Done {
                            f.outcome = Outcome::NotSent;
                        }
                        f
                    })?;
                no_value(fields! {
                    "scrolls": r.get("scrolls").cloned().unwrap_or(Value::Null),
                    "element": r.get("element").cloned().unwrap_or(Value::Null),
                })
            }
            "read_field" => {
                let (name, value) = Box::pin(self.read_field(body)).await?;
                Ok((fields! { "name": name.clone(), "value": value.clone() }, Some((name, value))))
            }
            "read_list" => {
                let (name, rows, info) = Box::pin(self.read_list(body)).await?;
                let count = rows.as_array().map(|a| a.len()).unwrap_or(0);
                Ok((
                    fields! { "name": name.clone(), "count": count, "scrolls": info["scrolls"], "ended": info["ended"] },
                    Some((name, rows)),
                ))
            }
            "button" => {
                let button = body.as_str().ok_or_else(|| Fail::invalid("button needs a name"))?;
                let r = Box::pin(self.cmd_button(ButtonArgs { button: button.to_string() })).await?;
                no_value(fields! { "screen_changed": r.get("screen_changed").cloned().unwrap_or(Value::Null) })
            }
            "key" => {
                let key = body.as_str().ok_or_else(|| Fail::invalid("key needs a name"))?;
                let r = Box::pin(self.cmd_key(KeyArgs { key: key.to_string() })).await?;
                no_value(fields! { "screen_changed": r.get("screen_changed").cloned().unwrap_or(Value::Null) })
            }
            "sleep" => {
                let ms = body.as_u64().ok_or_else(|| Fail::invalid("sleep needs milliseconds"))?;
                tokio::time::sleep(Duration::from_millis(ms.min(60_000))).await;
                no_value(Map::new())
            }
            other => Err(Fail::invalid(format!("unknown step kind `{other}`"))),
        }
    }

    /// Run steps in order and stop at the first failure.
    pub async fn run_steps(
        &mut self,
        steps: &[Value],
        args: &Map<String, Value>,
        opts: &RunOptions,
    ) -> Res {
        let mut records: Vec<Value> = Vec::new();
        let mut outputs = Map::new();
        // The app the last `launch` step brought to the front. Every later
        // step must still find it there.
        let mut expected_pid: Option<i64> = None;
        for (i, raw) in steps.iter().enumerate() {
            let index = i + 1;
            let started = Instant::now();
            let step = substitute(raw, args);
            if let Some(f) = self.unexpected_app(expected_pid) {
                let kind = split_step(&step).map(|(k, _, _)| k).unwrap_or("?");
                return Err(step_failure(index, raw, f, kind, &records, &outputs, 0));
            }
            let (kind, body, confirm) = match split_step(&step) {
                Ok(x) => x,
                Err(e) => return Err(step_failure(index, raw, Fail::invalid(e), "?", &records, &outputs, 0)),
            };
            // `confirm: true` needs the run's op_id; in a batch a step may
            // name its own.
            let (needs_confirm, op_base): (bool, Option<String>) = match confirm {
                Value::Bool(true) => (true, opts.confirm.clone()),
                Value::String(s) if opts.batch => (true, Some(s.clone())),
                Value::Null | Value::Bool(false) => (false, None),
                _ => {
                    return Err(step_failure(index, raw, Fail::invalid("`confirm` must be true"),
                        kind,
                        &records,
                        &outputs,
                        0,
                    ));
                }
            };
            let op_id = op_base.map(|base| if opts.batch { base } else { format!("{base}#{index}") });
            // A tap claims its op_id inside the tap, after it knows the target.
            // Other confirmed steps claim it here.
            let mut claimed: Option<OpRecord> = None;
            if needs_confirm && kind != "tap" {
                let Some(id) = &op_id else {
                    let f = Fail::blocked("confirm_required", "this step is marked confirm: true").hint(t(
                        "사람에게 확인받은 뒤 `--confirm <op_id>`를 붙여 다시 실행하세요.",
                        "Get a person's approval and run again with `--confirm <op_id>`.",
                    ));
                    return Err(step_failure(index, raw, f, kind, &records, &outputs, 0));
                };
                if !safety::valid_op_id(id) {
                    return Err(step_failure(index, raw, Fail::invalid("bad op_id"), kind, &records, &outputs, 0));
                }
                let rec = OpRecord {
                    op_id: id.clone(),
                    at: safety::now_utc(),
                    udid: self.udid.clone(),
                    state: "started".into(),
                    command: format!("step:{kind}"),
                    target: None,
                    matched_word: None,
                };
                match safety::begin(&rec) {
                    Ok(Ok(())) => claimed = Some(rec),
                    Ok(Err(earlier)) => {
                        let f = Fail::blocked("op_already_used", format!("op_id \"{id}\" was already used"))
                            .with("records", json!(earlier));
                        return Err(step_failure(index, raw, f, kind, &records, &outputs, 0));
                    }
                    Err(e) => {
                        return Err(step_failure(index, raw, Fail::not_sent("ops_log_failed", format!("{e:#}")),
                            kind,
                            &records,
                            &outputs,
                            0,
                        ));
                    }
                }
            }
            // Boxed: the step futures are large and would otherwise pile up on
            // the stack of whoever polls the run.
            let result = Box::pin(self.run_step(kind, body, op_id.as_deref(), needs_confirm)).await;
            let ms = started.elapsed().as_millis() as u64;
            match result {
                Ok((extra, value)) => {
                    Session::finish_op(claimed, "done");
                    if kind == "launch" {
                        expected_pid = extra.get("pid").and_then(|v| v.as_i64());
                    }
                    let mut rec = fields! { "index": index, "kind": kind, "ok": true, "elapsed_ms": ms };
                    for (k, v) in extra {
                        if !v.is_null() {
                            rec.insert(k, v);
                        }
                    }
                    records.push(Value::Object(rec));
                    if let Some((name, value)) = value {
                        outputs.insert(name, value);
                    }
                }
                Err(f) => {
                    Session::finish_op(
                        claimed,
                        match f.outcome {
                            Outcome::NotSent | Outcome::Blocked => "not_sent",
                            _ => "unknown",
                        },
                    );
                    return Err(step_failure(index, raw, f, kind, &records, &outputs, ms));
                }
            }
        }
        if let Some(f) = self.unexpected_app(expected_pid) {
            return Err(step_failure(steps.len(), &Value::Null, f, "end", &records, &outputs, 0));
        }
        Ok(fields! { "steps": records, "outputs": outputs })
    }

    pub async fn cmd_run(&mut self, name: &str, given: &Map<String, Value>, confirm: Option<String>) -> Res {
        let recipe = load(name)?;
        let mut args = Map::new();
        for (key, default) in &recipe.args {
            match given.get(key).or((!default.is_null()).then_some(default)) {
                Some(v) => {
                    args.insert(key.clone(), v.clone());
                }
                None => {
                    return Err(Fail::invalid(format!(
                        "recipe `{}` needs --arg {key}=…",
                        recipe.name
                    )));
                }
            }
        }
        if let Some(unknown) = given.keys().find(|k| !recipe.args.contains_key(*k)) {
            return Err(Fail::invalid(format!(
                "recipe `{}` has no argument `{unknown}`",
                recipe.name
            )));
        }
        let opts = RunOptions { confirm, batch: false };
        let mut out = match Box::pin(self.run_steps(&recipe.steps, &args, &opts)).await {
            Ok(o) => o,
            Err(f) => return Err(f.with("recipe", recipe.name.clone())),
        };
        if !recipe.output.is_empty() {
            if let Some(Value::Object(all)) = out.get_mut("outputs") {
                all.retain(|k, _| recipe.output.contains(k));
            }
        }
        out.insert("recipe".into(), json!(recipe.name));
        out.insert("source".into(), json!(recipe.source));
        Ok(out)
    }
}

pub fn cmd_recipes() -> Res {
    let list: Vec<Value> = list()
        .into_iter()
        .map(|(name, description, source)| json!({ "name": name, "description": description, "source": source }))
        .collect();
    Ok(fields! { "count": list.len(), "recipes": list })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_recipes_parse() {
        for (name, text) in BUILTIN {
            let r = Recipe::parse(text, name).unwrap();
            assert_eq!(&r.name, name);
            assert!(!r.description.is_empty());
        }
    }

    #[test]
    fn rejects_bad_steps() {
        let bad = "name: x\nsteps:\n  - fly: away\n";
        assert!(Recipe::parse(bad, "t").unwrap_err().contains("unknown step kind"));
        let two = "name: x\nsteps:\n  - tap: a\n    type: b\n";
        assert!(Recipe::parse(two, "t").unwrap_err().contains("exactly one kind"));
        let none = "name: x\nsteps: []\n";
        assert!(Recipe::parse(none, "t").is_err());
    }

    #[test]
    fn substitutes_arguments() {
        let step = json!({"type": {"text": "{{a}}×{{b}}"}});
        let mut args = Map::new();
        args.insert("a".into(), json!("12"));
        args.insert("b".into(), json!(34));
        assert_eq!(substitute(&step, &args)["type"]["text"], "12×34");
    }

    #[test]
    fn merge_keeps_identical_neighbours_apart() {
        let row = |s: &str| json!({ "label": s });
        let mut all = vec![row("a"), row("b"), row("b")];
        // The next page overlaps on "b", "b" and adds another "b" and "c".
        let added = merge_rows(&mut all, vec![row("b"), row("b"), row("b"), row("c")]);
        assert_eq!(added, 2);
        assert_eq!(all.len(), 5);
        // No overlap: everything is new.
        let added = merge_rows(&mut all, vec![row("x")]);
        assert_eq!(added, 1);
        // A page identical to the tail adds nothing.
        let added = merge_rows(&mut all, vec![row("c"), row("x")]);
        assert_eq!(added, 0);
    }

    #[test]
    fn take_value_falls_back_to_the_label_remainder() {
        let mut el = Element {
            id: "a".into(),
            role: "button".into(),
            label: Some("iOS 버전, 27.0.1".into()),
            value: None,
            identifier: None,
            hint: None,
            frame: None,
            traits: vec![],
            enabled: true,
            selected: false,
            focused: false,
            parent: None,
            depth: 1,
            pid: 1,
            on_screen: true,
            raw: None,
        };
        assert_eq!(take(&el, "value").as_deref(), Some("27.0.1"));
        assert_eq!(take(&el, "label").as_deref(), Some("iOS 버전, 27.0.1"));
        el.value = Some("실제 값".into());
        assert_eq!(take(&el, "value").as_deref(), Some("실제 값"));
        assert_eq!(
            apply_regex(Some("합계 12,300원".into()), Some(r"([\d,]+)원")).unwrap().as_deref(),
            Some("12,300")
        );
    }
}
