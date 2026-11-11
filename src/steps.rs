//! Steps: the pieces `iph batch` runs from a JSON list, and that scripts call
//! as `waitFor`, `readField` and `readList`.
//!
//! A step is a one-key map. Kinds: `launch`, `wait_for`, `tap`, `type`,
//! `swipe`, `scroll_until`, `read_field`, `read_list`, `assert`, `button`,
//! `key`, `sleep`. Any step may add `confirm`, `when: "a == b"` (run the
//! step only then) and `hint: "…"` (what to tell the person if it fails). A
//! step that fails stops the run; nothing is guessed.
//!
//! Strings may use today's date and the date some days ago:
//! `{{today.day}}`, `{{ago.6.month}}`, `{{ago.6.year}}`.

use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::{
    actions::{self, ButtonArgs, KeyArgs, LaunchArgs, ScrollArgs, SwipeArgs, TapArgs, TypeArgs, brief},
    ax::tree::{BuildOptions, Element, Screen},
    device::session::{Fresh, Session, reuse},
    fields,
    i18n::t,
    reply::{Fail, Outcome, Res},
    safety::{self, OpRecord},
    selector::{Selector, dedupe_targets},
};

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

/// Keys a step may carry next to its kind.
const MODIFIERS: &[&str] = &["confirm", "when", "hint"];

/// Split a step into (kind, body, confirm).
fn split_step(step: &Value) -> Result<(&str, &Value, &Value), String> {
    let map = step.as_object().ok_or("a step is a map like `tap: {...}`")?;
    static NULL: Value = Value::Null;
    let confirm = map.get("confirm").unwrap_or(&NULL);
    for key in ["when", "hint"] {
        if map.get(key).is_some_and(|v| !v.is_string()) {
            return Err(format!("`{key}` must be text"));
        }
    }
    if let Some(expr) = map.get("when").and_then(|v| v.as_str()) {
        // Only the form is checked here; the values come at run time.
        if !expr.contains("==") && !expr.contains("!=") {
            return Err("`when` must be `a == b` or `a != b`".into());
        }
    }
    let mut kinds = map.iter().filter(|(k, _)| !MODIFIERS.contains(&k.as_str()));
    let (kind, body) = kinds.next().ok_or("a step needs a kind")?;
    if kinds.next().is_some() {
        return Err("a step has exactly one kind (plus optional `confirm`, `when`, `hint`)".into());
    }
    if !KINDS.contains(&kind.as_str()) {
        return Err(format!("unknown step kind `{kind}` ({})", KINDS.join(", ")));
    }
    Ok((kind.as_str(), body, confirm))
}

/// Whether a step's `when` holds: `a == b` or `a != b`, compared as text
/// after the arguments and dates have been filled in.
fn holds(expr: &str) -> Result<bool, String> {
    let (left, right, equal) = match (expr.split_once("!="), expr.split_once("==")) {
        (Some((l, r)), _) => (l, r, false),
        (None, Some((l, r))) => (l, r, true),
        (None, None) => return Err(format!("`when: {expr}` must be `a == b` or `a != b`")),
    };
    Ok((left.trim() == right.trim()) == equal)
}

/// Days since 1970-01-01 of a calendar date, and back (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Today in the Mac's time zone as days since 1970-01-01. `IPH_TODAY`
/// (YYYY-MM-DD) overrides it, to try a run as if it were another day.
fn today_days() -> i64 {
    if let Ok(text) = std::env::var("IPH_TODAY") {
        let parts: Vec<i64> = text.split('-').filter_map(|p| p.parse().ok()).collect();
        if let [y, m, d] = parts[..] {
            return days_from_civil(y, m, d);
        }
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    // SAFETY: localtime_r / localtime_s fill the struct they are given and keep nothing.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        #[cfg(unix)]
        libc::localtime_r(&now, &mut tm);
        #[cfg(windows)]
        libc::localtime_s(&mut tm, &now);
        tm
    };
    days_from_civil(i64::from(tm.tm_year) + 1900, i64::from(tm.tm_mon) + 1, i64::from(tm.tm_mday))
}

/// Fill in `{{today.day}}`, `{{ago.6.month}}` and the like.
fn fill_dates(text: &str, today: i64) -> String {
    static DATE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = DATE.get_or_init(|| {
        Regex::new(r"\{\{\s*(?:today|ago\.(\d{1,4}))\.(year|month|day)\s*\}\}").expect("date pattern")
    });
    re.replace_all(text, |c: &regex::Captures| {
        let back: i64 = c.get(1).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
        let (y, m, d) = civil_from_days(today - back);
        match &c[2] {
            "year" => y.to_string(),
            "month" => m.to_string(),
            _ => d.to_string(),
        }
    })
    .into_owned()
}

/// Replace `{{name}}` and the date expressions in every string of `v`.
fn substitute(v: &Value, args: &Map<String, Value>, today: i64) -> Value {
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
            if out.contains("{{") {
                out = fill_dates(&out, today);
            }
            Value::String(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| substitute(x, args, today)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), substitute(x, args, today)))
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

/// One list row on one reading: what is reported for it, what identifies it,
/// and where it sat.
#[derive(Debug, Clone)]
struct Row {
    value: Value,
    key: String,
    y: f64,
}

/// Key of the entry that stands for a group heading among the rows while a
/// list is being collected. It never reaches the result.
const GROUP_MARK: &str = "\u{1}group";
/// On a row: the heading that stood above it on the reading it was taken
/// from. Removed before the result goes out.
const GROUP_OF: &str = "\u{1}group_of";

/// Give every row the text of the heading it stands under (as column `name`)
/// and drop the headings. With `below`, rows under a heading whose number is
/// smaller are dropped too.
fn settle_groups(items: Vec<Value>, name: &str, below: Option<i64>) -> Vec<Value> {
    let mut current: Option<String> = None;
    let mut out = Vec::new();
    for item in items {
        if let Some(heading) = item.get(GROUP_MARK) {
            current = heading.as_str().map(String::from);
            continue;
        }
        let mut row = item;
        // The heading the row was seen under wins over the last heading
        // collected: rows can come in out of order (the app put new rows in
        // above while the list was being read), and they would otherwise be
        // filed under whatever heading happened to come last.
        let seen_under = match &mut row {
            Value::Object(map) => map.remove(GROUP_OF).and_then(|v| v.as_str().map(String::from)),
            _ => None,
        };
        let heading = seen_under.or_else(|| current.clone());
        let number = heading.as_deref().and_then(|c| c.trim().parse::<i64>().ok());
        if let (Some(limit), Some(n)) = (below, number)
            && n < limit
        {
            continue;
        }
        if let Value::Object(map) = &mut row {
            map.insert(name.to_string(), heading.map(Value::String).unwrap_or(Value::Null));
        }
        out.push(row);
    }
    out
}

/// Whether a heading with a number below `limit` has been collected.
fn passed_below(items: &[Value], limit: i64) -> bool {
    items
        .iter()
        .filter_map(|i| i.get(GROUP_MARK).and_then(|g| g.as_str()))
        .filter_map(|g| g.trim().parse::<i64>().ok())
        .any(|n| n < limit)
}

/// How far the rows moved between two readings, if they moved as one block:
/// the distance that at least two thirds of the rows appearing once in both
/// readings agree on. None when nothing recognisable moved, or when the rows
/// did not move together (a list stretching at its end, a reading taken
/// mid-motion).
fn scroll_distance(prev: &[Row], page: &[Row]) -> Option<f64> {
    let once = |rows: &[Row], key: &str| rows.iter().filter(|r| r.key == key).count() == 1;
    let deltas: Vec<f64> = page
        .iter()
        .filter(|r| once(page, &r.key) && once(prev, &r.key))
        .filter_map(|r| prev.iter().find(|p| p.key == r.key).map(|p| p.y - r.y))
        .filter(|d| d.abs() > 2.0)
        .collect();
    // The value with the most neighbours within 3 points.
    let (agree, distance) = deltas
        .iter()
        .map(|d| (deltas.iter().filter(|o| (*o - d).abs() <= 3.0).count(), *d))
        .max_by_key(|(n, _)| *n)?;
    (agree * 3 >= deltas.len() * 2).then_some(distance)
}

/// Rows of `page` that were not on the previous reading.
///
/// When the rows moved as one block, position decides: a row was there
/// already if the same row sat where this one would have been before the
/// scroll, or sits exactly where it sat (a pinned element, not a list row).
/// That keeps two identical rows apart and survives pinned headers and
/// toolbars that match the row selector.
///
/// A row can also be moved by something other than the scroll: more rows
/// loading in above it, a heading growing. It is then not where the scroll
/// would have put it and looks new, though it was there before (2026-11-11:
/// the first row after the part of a list that had loaded came back twice).
/// So a previous row that should still be on screen and is not at its place
/// takes one of the "new" rows with its text off the list.
///
/// When they did not move as one block, only rows whose text the previous
/// reading did not have count as new (more of the same text than before
/// counts for the surplus). That can miss an identical neighbour, but never
/// reports a row twice.
fn new_rows(prev: &[Row], page: &[Row]) -> Vec<Value> {
    match scroll_distance(prev, page) {
        Some(dy) => {
            let at = |r: &Row, y: f64, slack: f64| (r.y - y).abs() <= slack;
            let mut fresh: Vec<&Row> = page
                .iter()
                .filter(|r| {
                    let pinned = prev.iter().any(|p| p.key == r.key && at(r, p.y, 2.0));
                    let seen = prev.iter().any(|p| p.key == r.key && at(r, p.y - dy, 3.0));
                    !pinned && !seen
                })
                .collect();
            let lo = page.iter().map(|r| r.y).fold(f64::INFINITY, f64::min);
            let hi = page.iter().map(|r| r.y).fold(f64::NEG_INFINITY, f64::max);
            for p in prev {
                let now = p.y - dy;
                let still_on_screen = now >= lo - 3.0 && now <= hi + 3.0;
                let found = page.iter().any(|r| r.key == p.key && (at(r, now, 3.0) || at(r, p.y, 2.0)));
                if !still_on_screen || found {
                    continue;
                }
                let nearest = fresh
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| r.key == p.key)
                    .min_by(|a, b| (a.1.y - now).abs().total_cmp(&(b.1.y - now).abs()))
                    .map(|(i, _)| i);
                if let Some(i) = nearest {
                    fresh.remove(i);
                }
            }
            fresh.into_iter().map(|r| r.value.clone()).collect()
        }
        None => {
            let mut out = Vec::new();
            for (i, r) in page.iter().enumerate() {
                let before = prev.iter().filter(|p| p.key == r.key).count();
                let so_far = page[..=i].iter().filter(|p| p.key == r.key).count();
                if so_far > before {
                    out.push(r.value.clone());
                }
            }
            out
        }
    }
}

/// How far to scroll a list between two readings.
///
/// A fixed step (a third of the screen) reads every row two or three times.
/// The step that reads each row once depends on the phone and the app: how
/// tall the list's window is, how much of it stays readable under pinned
/// headers, how much shorter the list moves than the finger. So it is worked
/// out from what the readings show.
#[derive(Debug, Default)]
struct Stride {
    /// Highest place a list item has been seen at (smallest y). Items stay
    /// in the accessibility tree up to here, also under a pinned header.
    top: Option<f64>,
    /// How much shorter the list moved than the finger, on the last scroll.
    lost: f64,
    /// The finger's travel on the last scroll, 0 when it was a fixed step.
    last: f64,
}

impl Stride {
    /// Share of the screen's height the finger may cross in one scroll.
    const REACH: f64 = 0.72;
    /// The item brought up stops this far below the top, so that it stays
    /// readable when the list moves a little further than meant.
    const MARGIN: f64 = 8.0;

    /// Take in what a reading shows: where items can be, and how far the
    /// last scroll really moved them.
    fn learn(&mut self, before: &[Row], page: &[Row]) {
        if let Some(y) = page.iter().map(|r| r.y).reduce(f64::min) {
            self.top = Some(self.top.map_or(y, |t| t.min(y)));
        }
        if self.last > 0.0
            && let Some(moved) = scroll_distance(before, page)
        {
            self.lost = (self.last - moved.abs()).clamp(0.0, 200.0);
        }
    }

    /// The finger's travel for the next scroll, or `None` when the reading
    /// gives nothing to go by (fewer than three items) and a fixed step has
    /// to do.
    fn next(&mut self, page: &[Row], screen_height: f64) -> Option<f64> {
        let top = self.top?;
        let mut ys: Vec<f64> = page.iter().map(|r| r.y).collect();
        ys.sort_by(f64::total_cmp);
        if ys.len() < 3 || screen_height <= 0.0 {
            return None;
        }
        // The item before the last one goes to the top: two items in common.
        let want = ys[ys.len() - 2] - (top + Self::MARGIN);
        // Until a scroll has been measured, count on losing little: that
        // errs towards moving too short, which only costs overlap.
        let lost = if self.last > 0.0 { self.lost } else { 40.0 };
        let travel = (want + lost).min(screen_height * Self::REACH);
        if travel < 120.0 {
            return None;
        }
        self.last = travel;
        Some(travel)
    }
}

/// Where each row of a reading sat, for the trace: "y:mark" pairs. The mark is
/// a short hash of the row's text, so the log shows which rows repeat or
/// jump without holding what they say.
fn page_marks(page: &[Row]) -> String {
    page.iter()
        .map(|r| {
            // FNV-1a, 32 bits.
            let hash = r.key.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193));
            format!("{:.0}:{:06x}", r.y, hash & 0x00ff_ffff)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Wrap a step's failure in the run's report: which step, why, and
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

/// Extra fields for a step's record and, for read steps, the value read.
type StepOutput = (Map<String, Value>, Option<(String, Value)>);

pub struct RunOptions {
    /// `--confirm <op_id>` for steps marked `confirm: true`.
    pub confirm: Option<String>,
    /// True for `iph batch`: a step's `confirm` may carry its own op_id.
    pub batch: bool,
}

impl Session {
    pub(crate) async fn wait_for(&mut self, sel: &Selector, timeout: Duration) -> Res<Element> {
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

    /// Wait until the element a step is about to tap holds still: two
    /// readings in a row with it at the same place. A screen that has just
    /// come up may still be loading and move its rows; a tap aimed at where
    /// the row was then lands on its neighbour (2026-11-11: right after a
    /// launch a tap on one row of a home screen opened the page of the row
    /// above it).
    pub(crate) async fn settle_target(&mut self, sel: &Selector) -> Res<()> {
        let full = sel.needs_identifier();
        let place = |screen: &Screen| -> Option<(i64, i64, i64, i64)> {
            match dedupe_targets(sel.find(screen)).as_slice() {
                [one] => one.frame.map(|f| {
                    (f.x.round() as i64, f.y.round() as i64, f.w.round() as i64, f.h.round() as i64)
                }),
                _ => None,
            }
        };
        let (_, screen) = self.screen(Fresh::MaxAge(reuse()), full, BuildOptions::default()).await?;
        let mut last = place(&screen);
        for _ in 0..4 {
            let (_, again) = self.screen(Fresh::Force, full, BuildOptions::default()).await?;
            let now = place(&again);
            if now.is_some() && now == last {
                return Ok(());
            }
            last = now;
        }
        // Still moving after four looks: the tap goes ahead on the last one,
        // as it would have without this check.
        Ok(())
    }

    /// `name` is what a batch files the value under; a script has no use for it.
    pub(crate) async fn read_field(&mut self, body: &Value) -> Res<(String, Value)> {
        let map = body.as_object().ok_or_else(|| Fail::invalid("read_field needs a map"))?;
        let name = map.get("name").and_then(|v| v.as_str()).unwrap_or("value");
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
        let mut text = apply_regex(take(el, what), pattern)?;
        // What a run hands back must not come from a reading that was
        // already out of date: look again until two readings give the same.
        let needs_ids = sel.needs_identifier() || what == "identifier";
        for _ in 0..2 {
            let (_, again) = self.screen(Fresh::Force, needs_ids, BuildOptions::default()).await?;
            let next = match dedupe_targets(sel.find(&again)).as_slice() {
                [one] => apply_regex(take(one, what), pattern)?,
                _ => None,
            };
            let same = next == text;
            text = next;
            if same {
                break;
            }
        }
        let el = actions::resolve_one(&sel, &screen)?;
        match text {
            Some(text) => Ok((name.to_string(), Value::String(text))),
            None => Err(Fail::not_sent(
                "no_value",
                format!("{} has no {what} to read", sel.describe()),
            )
            .with("element", brief(el))),
        }
    }

    /// `key`: the columns that say which row a row is. Without it the whole
    /// row does, which takes a row whose text changes while the list is read
    /// (a clock, a counter) for a new one.
    fn rows_on_screen(
        screen: &Screen,
        row: &Selector,
        columns: Option<&Map<String, Value>>,
        key: &[String],
    ) -> Res<Vec<Row>> {
        let mut out = Vec::new();
        for el in dedupe_targets(row.find(screen)) {
            let y = el.frame.map(|f| f.y).unwrap_or(0.0);
            // Two rows are the same row when label and value agree.
            let identity = format!(
                "{}\u{1}{}",
                el.label.as_deref().unwrap_or(""),
                el.value.as_deref().unwrap_or("")
            );
            let Some(cols) = columns else {
                let mut v = json!({});
                if let Some(l) = &el.label {
                    v["label"] = json!(l);
                }
                if let Some(val) = &el.value {
                    v["value"] = json!(val);
                }
                out.push(Row { value: v, key: identity, y });
                continue;
            };
            let mut rec = Map::new();
            for (col, spec) in cols {
                let spec_map = spec.as_object();
                // `{value: "…"}` is the same text on every row (a month, a tag).
                if let Some(fixed) = spec_map.and_then(|m| m.get("value")) {
                    rec.insert(col.clone(), fixed.clone());
                    continue;
                }
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
            let key = if key.is_empty() {
                format!("{identity}\u{1}{}", Value::Object(rec.clone()))
            } else {
                key.iter()
                    .map(|k| rec.get(k).map(|v| v.to_string()).unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\u{1}")
            };
            out.push(Row { value: Value::Object(rec), key, y });
        }
        Ok(out)
    }

    /// The rows on one reading, with the group headings among them when the
    /// list has groups, top to bottom.
    fn list_page(
        screen: &Screen,
        row: &Selector,
        columns: Option<&Map<String, Value>>,
        key: &[String],
        group: Option<&(Selector, String, Option<String>, String)>,
    ) -> Res<Vec<Row>> {
        let mut page = Self::rows_on_screen(screen, row, columns, key)?;
        if let Some((sel, what, pattern, _)) = group {
            // Headings travel with the rows, so the same position logic that
            // keeps rows apart puts each heading where it belongs.
            for el in dedupe_targets(sel.find(screen)) {
                let Some(text) = apply_regex(take(el, what), pattern.as_deref())? else {
                    continue;
                };
                page.push(Row {
                    key: format!("{GROUP_MARK}\u{1}{}", el.label.as_deref().unwrap_or("")),
                    value: json!({ GROUP_MARK: text }),
                    y: el.frame.map(|f| f.y).unwrap_or(0.0),
                });
            }
            page.sort_by(|a, b| a.y.total_cmp(&b.y));
            // Note on every row which heading stands above it on this reading.
            let mut above: Option<Value> = None;
            for item in page.iter_mut() {
                if let Some(heading) = item.value.get(GROUP_MARK) {
                    above = Some(heading.clone());
                } else if let (Some(heading), Value::Object(map)) = (&above, &mut item.value) {
                    map.insert(GROUP_OF.to_string(), heading.clone());
                }
            }
        }
        Ok(page)
    }

    /// Drag the list by `travel` points of finger movement, slowing to a
    /// stop. "down" shows what is further down.
    async fn stride_drag(&mut self, direction: &str, travel: f64) -> Res<bool> {
        let (_, screen) = self.screen(Fresh::MaxAge(reuse()), false, BuildOptions::default()).await?;
        let (w, h) = (screen.width, screen.height);
        let (low, high) = (h * 0.88, h * 0.88 - travel);
        let (from, to) = if direction == "down" { (low, high) } else { (high, low) };
        // A longer way takes the finger longer, as a hand's would.
        let duration = Duration::from_millis((350.0 + travel * 0.6) as u64);
        let (changed, _) = Box::pin(self.drag_points(
            (w / 2.0, from),
            (w / 2.0, to),
            duration,
            crate::device::input::Motion::Place,
        ))
        .await?;
        Ok(changed)
    }

    /// One scroll step of a list; whether the screen moved.
    async fn list_scroll(&mut self, direction: &str) -> Res<bool> {
        let r = Box::pin(self.cmd_scroll(ScrollArgs {
            direction: direction.to_string(),
            until: None,
            max: None,
        }))
        .await?;
        Ok(r.get("screen_changed").and_then(|v| v.as_bool()).unwrap_or(false))
    }

    pub(crate) async fn read_list(&mut self, body: &Value) -> Res<(String, Value, Value)> {
        let map = body.as_object().ok_or_else(|| Fail::invalid("read_list needs a map"))?;
        let name = map.get("name").and_then(|v| v.as_str()).unwrap_or("rows");
        let row = Selector::from_value(map.get("row").ok_or_else(|| Fail::invalid("read_list needs `row`"))?)
            .map_err(|e| Fail::invalid(e.to_string()))?;
        let columns = map.get("columns").and_then(|v| v.as_object());
        // `key: "city"` (or a list): the columns that identify a row.
        let key: Vec<String> = match map.get("key") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(k)) => vec![k.clone()],
            Some(Value::Array(ks)) => ks
                .iter()
                .map(|k| k.as_str().map(String::from).ok_or_else(|| Fail::invalid("key takes column names")))
                .collect::<Res<_>>()?,
            Some(_) => return Err(Fail::invalid("key takes a column name or a list of them")),
        };
        if let Some(missing) = key.iter().find(|k| !columns.is_some_and(|c| c.contains_key(k.as_str()))) {
            return Err(Fail::invalid(format!("key names `{missing}`, which is not one of the columns")));
        }
        let until = match map.get("until") {
            Some(v) => Some(Selector::from_value(v).map_err(|e| Fail::invalid(e.to_string()))?),
            None => None,
        };
        let max_scrolls = opt_u64(map, "max_scrolls").unwrap_or(15).min(200) as u32;
        let limit = opt_u64(map, "limit").unwrap_or(500) as usize;
        let direction = map.get("direction").and_then(|v| v.as_str()).unwrap_or("down").to_string();
        // `group: {selector, take, regex, as}`: headings between the rows (a
        // date above each day's rows). Every row gets the heading it stands
        // under as column `as`.
        let group = match map.get("group") {
            Some(Value::Object(g)) => {
                let sel = Selector::from_value(g.get("selector").ok_or_else(|| Fail::invalid("group needs `selector`"))?)
                    .map_err(|e| Fail::invalid(e.to_string()))?;
                let what = g.get("take").and_then(|v| v.as_str()).unwrap_or("label").to_string();
                let pattern = g.get("regex").and_then(|v| v.as_str()).map(String::from);
                let name = g.get("as").and_then(|v| v.as_str()).unwrap_or("group").to_string();
                Some((sel, what, pattern, name))
            }
            Some(_) => return Err(Fail::invalid("group must be a map: {selector, take, regex, as}")),
            None => None,
        };
        // `stop_below: N`: stop once a heading whose number is below N has
        // come up, and leave its rows out (a list that runs back in time,
        // read down to a day of the month).
        let stop_below = match map.get("stop_below") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                    .ok_or_else(|| Fail::invalid("stop_below must be a whole number"))?,
            ),
        };
        if stop_below.is_some() && group.is_none() {
            return Err(Fail::invalid("stop_below needs `group`"));
        }
        let full = row.needs_identifier()
            || until.as_ref().is_some_and(|u| u.needs_identifier())
            || group.as_ref().is_some_and(|g| g.0.needs_identifier());

        let mut all: Vec<Value> = Vec::new();
        let mut prev: Option<Vec<Row>> = None;
        let mut scrolls = 0u32;
        // Readings in a row that brought no new row.
        let mut barren = 0u32;
        // Whether the end of the list has been checked by scrolling back and forth.
        let mut end_checked = false;
        let back = if direction == "down" { "up" } else { "down" };
        let mut stride = Stride::default();
        let ended;
        loop {
            // One reading a page: the scroll before it already asked the
            // phone for the new screen, and this takes that reading over.
            let (_, mut screen) = self.screen(Fresh::MaxAge(reuse()), full, BuildOptions::default()).await?;
            let mut page = Self::list_page(&screen, &row, columns, &key, group.as_ref())?;
            if let Some(before) = &prev {
                let stitched = page.iter().any(|r| before.iter().any(|p| p.key == r.key));
                if !stitched && !page.is_empty() && !before.is_empty() && stride.last > 0.0 {
                    // The scroll carried the list past everything that was on
                    // screen, so rows may lie unseen in between. Come back
                    // half way and read again.
                    crate::device::ax::trace(|| "read_list: no row in common after the scroll; coming back half way".into());
                    self.stride_drag(back, stride.last / 2.0).await?;
                    stride.lost += 40.0;
                    (_, screen) = self.screen(Fresh::MaxAge(reuse()), full, BuildOptions::default()).await?;
                    page = Self::list_page(&screen, &row, columns, &key, group.as_ref())?;
                }
                stride.learn(before, &page);
            } else {
                stride.learn(&[], &page);
            }
            match &prev {
                None => all.extend(page.iter().map(|r| r.value.clone())),
                Some(before) => {
                    let fresh_rows = new_rows(before, &page);
                    crate::device::ax::trace(|| {
                        format!(
                            "read_list page: rows={} moved={:?} new={} [{}]",
                            page.len(),
                            scroll_distance(before, &page),
                            fresh_rows.len(),
                            page_marks(&page),
                        )
                    });
                    barren = if fresh_rows.is_empty() { barren + 1 } else { 0 };
                    all.extend(fresh_rows);
                }
            }
            prev = Some(page);
            if stop_below.is_some_and(|n| passed_below(&all, n)) {
                ended = "stop_below";
                break;
            }
            if all.iter().filter(|i| i.get(GROUP_MARK).is_none()).count() >= limit {
                ended = "limit";
                break;
            }
            if until.as_ref().is_some_and(|u| !u.find(&screen).is_empty()) {
                ended = "until";
                break;
            }
            if barren >= 2 {
                // Two readings without a new row. That is the end of the list,
                // or scrolls that never arrived: an input session the phone
                // has stopped listening to looks exactly the same, and a list
                // cut short there reads as complete (2026-11-11: two scrolls
                // right after a Face ID prompt moved nothing, and a week of
                // rows came back as two days). So the end is checked once by
                // scrolling back and forward: at a real end the list moves
                // back and returns to the same rows.
                if end_checked {
                    ended = "end_of_list";
                    break;
                }
                end_checked = true;
                let at_end = prev.clone().unwrap_or_default();
                let mut moved = self.list_scroll(back).await?;
                if !moved {
                    // Nothing moves either way: once more on a new input session.
                    self.close_input().await;
                    moved = self.list_scroll(back).await?;
                }
                if !moved {
                    // A list that fits the screen.
                    ended = "end_of_list";
                    break;
                }
                self.list_scroll(&direction).await?;
                let (_, screen) = self.screen(Fresh::MaxAge(reuse()), full, BuildOptions::default()).await?;
                let page = Self::list_page(&screen, &row, columns, &key, group.as_ref())?;
                let fresh_rows = new_rows(&at_end, &page);
                crate::device::ax::trace(|| format!("read_list end check: new={}", fresh_rows.len()));
                if fresh_rows.is_empty() {
                    ended = "end_of_list";
                    break;
                }
                all.extend(fresh_rows);
                barren = 0;
                prev = Some(page);
                if stop_below.is_some_and(|n| passed_below(&all, n)) {
                    ended = "stop_below";
                    break;
                }
            }
            if scrolls >= max_scrolls {
                ended = "max_scrolls";
                break;
            }
            // Scroll by as much as the screen allows: the last two items of
            // this reading go to the top of the next, so that the two
            // readings can be stitched and nothing in between is skipped.
            match stride.next(prev.as_deref().unwrap_or(&[]), screen.height) {
                Some(travel) => {
                    self.stride_drag(&direction, travel).await?;
                }
                None => {
                    self.list_scroll(&direction).await?;
                    stride.last = 0.0;
                }
            }
            scrolls += 1;
        }
        let mut rows = match &group {
            Some((_, _, _, column)) => settle_groups(all, column, stop_below),
            None => all,
        };
        rows.truncate(limit);
        Ok((
            name.to_string(),
            Value::Array(rows),
            json!({ "scrolls": scrolls, "ended": ended }),
        ))
    }

    /// A failure when the latest reading shows another app in front than the
    /// one this run launched (a notification was tapped, a link opened …).
    pub(crate) fn unexpected_app(&self, expected_pid: Option<i64>) -> Option<Fail> {
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
    ) -> Res<StepOutput> {
        fn no_value(f: Map<String, Value>) -> Res<StepOutput> {
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
                if options.get("settle").and_then(|v| v.as_bool()).unwrap_or(true) {
                    Box::pin(self.settle_target(&sel)).await?;
                }
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
                .await?;
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
        // Steps that sent something to the phone and completed.
        let mut inputs_sent = 0u32;
        let today = today_days();
        for (i, raw) in steps.iter().enumerate() {
            let index = i + 1;
            let started = Instant::now();
            let step = substitute(raw, args, today);
            let (kind, body, confirm) = match split_step(&step) {
                Ok(x) => x,
                Err(e) => return Err(step_failure(index, raw, Fail::invalid(e), "?", &records, &outputs, 0)),
            };
            if let Some(expr) = step.get("when").and_then(|v| v.as_str()) {
                match holds(expr) {
                    Ok(true) => {}
                    Ok(false) => {
                        records.push(json!({ "index": index, "kind": kind, "ok": true, "skipped": true, "when": expr }));
                        continue;
                    }
                    Err(e) => return Err(step_failure(index, raw, Fail::invalid(e), kind, &records, &outputs, 0)),
                }
            }
            // A `wait_for` may sit out something the system puts over the app
            // for a moment (a Face ID prompt); every other step needs the app
            // in front.
            if kind != "wait_for"
                && let Some(f) = self.unexpected_app(expected_pid)
            {
                return Err(step_failure(index, raw, f, kind, &records, &outputs, 0));
            }
            let hint = step.get("hint").and_then(|v| v.as_str()).map(String::from);
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
                    if matches!(
                        kind,
                        "launch" | "tap" | "type" | "swipe" | "scroll_until" | "read_list" | "button" | "key"
                    ) {
                        inputs_sent += 1;
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
                Err(mut f) => {
                    Session::finish_op(
                        claimed,
                        match f.outcome {
                            Outcome::NotSent | Outcome::Blocked => "not_sent",
                            _ => "unknown",
                        },
                    );
                    // Whoever wrote the steps knows what this one usually waits on.
                    if let Some(hint) = hint {
                        f.hint = Some(hint);
                    }
                    return Err(step_failure(index, raw, f, kind, &records, &outputs, ms)
                        .with("inputs_sent", inputs_sent));
                }
            }
        }
        if let Some(f) = self.unexpected_app(expected_pid) {
            return Err(step_failure(steps.len(), &Value::Null, f, "end", &records, &outputs, 0));
        }
        Ok(fields! { "steps": records, "outputs": outputs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_steps() {
        assert!(split_step(&json!({"fly": "away"})).unwrap_err().contains("unknown step kind"));
        assert!(split_step(&json!({"tap": "a", "type": "b"})).unwrap_err().contains("exactly one kind"));
        assert!(split_step(&json!({"tap": "a", "confirm": true, "hint": "look"})).is_ok());
    }

    #[test]
    fn substitutes_arguments() {
        let step = json!({"type": {"text": "{{a}}×{{b}}"}});
        let mut args = Map::new();
        args.insert("a".into(), json!("12"));
        args.insert("b".into(), json!(34));
        assert_eq!(substitute(&step, &args, 0)["type"]["text"], "12×34");
    }

    #[test]
    fn dates_fill_in_and_cross_months_and_years() {
        // 2026-11-03: six days earlier is 28 October.
        let day = days_from_civil(2026, 11, 3);
        assert_eq!(civil_from_days(day), (2026, 11, 3));
        assert_eq!(fill_dates("{{today.month}}/{{today.day}}", day), "11/3");
        assert_eq!(fill_dates("{{ago.6.month}}/{{ago.6.day}}", day), "10/28");
        assert_eq!(fill_dates("{{ ago.0.year }}", day), "2026");
        // 2027-01-02: six days earlier is in the year before.
        let day = days_from_civil(2027, 1, 2);
        assert_eq!(fill_dates("{{ago.6.year}}-{{ago.6.month}}-{{ago.6.day}}", day), "2026-12-27");
        // A leap day.
        assert_eq!(civil_from_days(days_from_civil(2028, 3, 1) - 1), (2028, 2, 29));
        // Anything else in braces is left for the arguments.
        assert_eq!(fill_dates("{{name}} {{today.week}}", day), "{{name}} {{today.week}}");
        let step = json!({"read_list": {"stop_below": "{{ago.6.day}}"}, "when": "{{ago.6.month}} != {{today.month}}"});
        let filled = substitute(&step, &Map::new(), days_from_civil(2026, 11, 3));
        assert_eq!(filled["read_list"]["stop_below"], "28");
        assert_eq!(filled["when"], "10 != 11");
    }

    #[test]
    fn when_compares_text() {
        assert!(holds("11 == 11").unwrap());
        assert!(!holds("10 == 11").unwrap());
        assert!(holds("10 != 11").unwrap());
        assert!(!holds(" 11 !=11 ").unwrap());
        assert!(holds("soon").is_err());
        assert!(split_step(&json!({"tap": "a", "when": "maybe"})).unwrap_err().contains("when"));
        assert!(split_step(&json!({"tap": "a", "when": "{{ago.6.month}} != {{today.month}}"})).is_ok());
    }

    fn mark(text: &str) -> Value {
        json!({ GROUP_MARK: text })
    }

    #[test]
    fn rows_take_the_heading_they_stand_under() {
        let items = vec![
            json!({"label": "before any heading"}),
            mark("11"),
            json!({"label": "a"}),
            json!({"label": "b"}),
            mark("9"),
            json!({"label": "c"}),
            mark("4"),
            json!({"label": "d"}),
        ];
        let rows = settle_groups(items.clone(), "day", None);
        let days: Vec<Value> = rows.iter().map(|r| r["day"].clone()).collect();
        assert_eq!(days, [Value::Null, json!("11"), json!("11"), json!("9"), json!("4")]);
        // Reading down to day 5: the rows of day 4 are left out.
        assert!(!passed_below(&items[..6], 5));
        assert!(passed_below(&items, 5));
        let rows = settle_groups(items, "day", Some(5));
        assert_eq!(labels(&rows), ["before any heading", "a", "b", "c"]);
    }

    #[test]
    fn a_row_keeps_the_heading_it_was_seen_under() {
        // Day 11's rows were collected, then the heading of day 10 and its
        // row. Then the app put three new rows in at the top of day 11; they
        // arrive last, but were seen under "11".
        let late = |label: &str| json!({ "label": label, GROUP_OF: "11" });
        let items = vec![
            mark("11"),
            json!({"label": "a"}),
            mark("10"),
            json!({"label": "b"}),
            late("new 1"),
            late("new 2"),
        ];
        let rows = settle_groups(items, "day", None);
        let got: Vec<(&str, &str)> = rows.iter().map(|r| (r["day"].as_str().unwrap(), r["label"].as_str().unwrap())).collect();
        assert_eq!(got, [("11", "a"), ("10", "b"), ("11", "new 1"), ("11", "new 2")]);
        assert!(rows.iter().all(|r| r.get(GROUP_OF).is_none()));
    }

    #[test]
    fn headings_move_with_the_rows_from_one_reading_to_the_next() {
        let heading = |text: &str, y: f64| Row { value: mark(text), key: format!("{GROUP_MARK}\u{1}{text}"), y };
        let row = |label: &str, y: f64| Row { value: json!({ "label": label }), key: label.to_string(), y };
        // Two days with the same transfer on each; the list moved up by 130.
        let before = vec![heading("11", 100.0), row("이체 100,000원", 140.0), row("커피", 205.0), heading("10", 270.0)];
        let after = vec![row("커피", 75.0), heading("10", 140.0), row("이체 100,000원", 180.0), heading("9", 245.0)];
        let mut all: Vec<Value> = before.iter().map(|r| r.value.clone()).collect();
        all.extend(new_rows(&before, &after));
        let rows = settle_groups(all, "day", None);
        let got: Vec<(String, String)> = rows
            .iter()
            .map(|r| (r["day"].as_str().unwrap().to_string(), r["label"].as_str().unwrap().to_string()))
            .collect();
        assert_eq!(
            got,
            [
                ("11".to_string(), "이체 100,000원".to_string()),
                ("11".to_string(), "커피".to_string()),
                ("10".to_string(), "이체 100,000원".to_string()),
            ]
        );
    }

    fn rows(items: &[(&str, f64)]) -> Vec<Row> {
        items
            .iter()
            .map(|(label, y)| Row { value: json!({ "label": label }), key: label.to_string(), y: *y })
            .collect()
    }

    fn labels(v: &[Value]) -> Vec<&str> {
        v.iter().map(|x| x["label"].as_str().unwrap()).collect()
    }

    #[test]
    fn only_rows_that_were_not_on_the_previous_reading_are_new() {
        // The list moved up by 160 points.
        let before = rows(&[("a", 100.0), ("b", 180.0), ("c", 260.0), ("d", 340.0)]);
        let after = rows(&[("c", 100.0), ("d", 180.0), ("e", 260.0), ("f", 340.0)]);
        assert_eq!(scroll_distance(&before, &after), Some(160.0));
        assert_eq!(labels(&new_rows(&before, &after)), ["e", "f"]);
    }

    #[test]
    fn pinned_elements_that_match_the_row_selector_are_counted_once() {
        // "합계" stays at the top and "받아쓰기" at the bottom on every reading.
        let before = rows(&[("합계", 60.0), ("a", 100.0), ("b", 180.0), ("받아쓰기", 860.0)]);
        let after = rows(&[("합계", 60.0), ("b", 100.0), ("c", 180.0), ("받아쓰기", 860.0)]);
        assert_eq!(labels(&new_rows(&before, &after)), ["c"]);
    }

    #[test]
    fn identical_rows_are_kept_apart_by_position() {
        // Three purchases that read the same; the list moved up by 80.
        let before = rows(&[("커피 4,500원", 100.0), ("커피 4,500원", 180.0), ("빵 3,000원", 260.0)]);
        let after = rows(&[
            ("커피 4,500원", 100.0),
            ("빵 3,000원", 180.0),
            ("커피 4,500원", 260.0),
            ("우유 2,000원", 340.0),
        ]);
        // The first two are the old second and third rows; the coffee at 260
        // and the milk are new.
        assert_eq!(labels(&new_rows(&before, &after)), ["커피 4,500원", "우유 2,000원"]);
    }

    #[test]
    fn the_stride_brings_the_last_two_items_to_the_top() {
        let mut stride = Stride::default();
        // First reading: the list starts half way down the screen.
        let first = rows(&[("a", 500.0), ("b", 565.0), ("c", 630.0), ("d", 695.0), ("e", 760.0), ("f", 825.0)]);
        stride.learn(&[], &first);
        // "e" (the one before the last) should go to the top (500 + margin);
        // nothing measured yet, so 40 is added for what the list lags.
        let travel = stride.next(&first, 932.0).unwrap();
        assert_eq!(travel, 760.0 - 508.0 + 40.0);
        // The list moved 232 for a travel of 292: it lags by 60.
        let second = rows(&[("e", 528.0), ("f", 593.0), ("g", 658.0), ("h", 723.0), ("i", 788.0), ("j", 853.0)]);
        stride.learn(&first, &second);
        assert_eq!(stride.lost, 60.0);
        assert_eq!(stride.top, Some(500.0));
        // Next: "i" to the top, with the lag that was measured.
        assert_eq!(stride.next(&second, 932.0).unwrap(), 788.0 - 508.0 + 60.0);
        // Never further than the finger can go on this screen.
        let far = rows(&[("x", 100.0), ("y", 800.0), ("z", 900.0)]);
        stride.top = Some(100.0);
        assert_eq!(stride.next(&far, 932.0).unwrap(), 932.0 * Stride::REACH);
        // Too little on screen to go by: a fixed step instead.
        assert_eq!(stride.next(&rows(&[("a", 500.0), ("b", 565.0)]), 932.0), None);
    }

    #[test]
    fn a_row_pushed_down_by_rows_loading_above_it_is_not_new() {
        // The list moved up by 270. Meanwhile more of it loaded, which pushed
        // the last row 40 further down than the scroll alone would have.
        let before = rows(&[("a", 400.0), ("b", 465.0), ("c", 530.0), ("d", 595.0), ("last loaded", 660.0)]);
        let after = rows(&[
            ("a", 130.0),
            ("b", 195.0),
            ("c", 260.0),
            ("d", 325.0),
            ("last loaded", 430.0),
            ("next", 495.0),
        ]);
        assert_eq!(scroll_distance(&before, &after), Some(270.0));
        assert_eq!(labels(&new_rows(&before, &after)), ["next"]);
        // Two rows that read the same, one of them pushed: the other is new.
        let before = rows(&[("a", 400.0), ("b", 465.0), ("c", 530.0), ("커피", 595.0)]);
        let after = rows(&[("a", 130.0), ("b", 195.0), ("c", 260.0), ("커피", 365.0), ("커피", 430.0)]);
        assert_eq!(new_rows(&before, &after).len(), 1);
    }

    #[test]
    fn rows_that_did_not_move_together_are_matched_by_text() {
        // The end of a list stretching under the finger: every row shifted by
        // a different amount. Nothing is new.
        let before = rows(&[("a", 94.0), ("b", 143.0), ("c", 193.0), ("d", 242.0), ("e", 326.0)]);
        let after = rows(&[("a", 114.0), ("b", 173.0), ("c", 238.0), ("d", 302.0), ("e", 406.0)]);
        assert_eq!(scroll_distance(&before, &after), None);
        assert!(new_rows(&before, &after).is_empty());
        // The same, with one row that was not there before.
        let after = rows(&[("b", 173.0), ("c", 238.0), ("d", 302.0), ("e", 406.0), ("f", 470.0)]);
        assert_eq!(labels(&new_rows(&before, &after)), ["f"]);
    }

    #[test]
    fn a_reading_with_nothing_in_common_is_all_new() {
        let before = rows(&[("a", 100.0), ("b", 180.0)]);
        let after = rows(&[("x", 100.0), ("y", 180.0)]);
        assert_eq!(scroll_distance(&before, &after), None);
        assert_eq!(labels(&new_rows(&before, &after)), ["x", "y"]);
    }

    #[test]
    fn a_key_column_keeps_a_row_whose_text_changed_from_counting_twice() {
        // A world clock: the minute turns between two readings of the list.
        let screen = |minute: u32, top: f64, cities: &[&str]| -> Screen {
            let elements: Vec<Value> = cities
                .iter()
                .enumerate()
                .map(|(i, city)| {
                    json!({
                        "id": format!("row{i}{minute}"), "role": "text", "traits": ["static_text"],
                        "label": format!("{city}, 오후 2:{minute:02}, 오늘, +0시간"),
                        "frame": { "x": 0, "y": top + 90.0 * i as f64, "w": 430, "h": 90 },
                    })
                })
                .collect();
            serde_json::from_value(json!({ "width": 430.0, "height": 932.0, "apps": [], "elements": elements })).unwrap()
        };
        let row = Selector::from_value(&json!({ "role": "text" })).unwrap();
        let columns = json!({
            "city": { "take": "label", "regex": "^([^,]+)" },
            "time": { "take": "label", "regex": "(오후 [0-9:]+)" },
        });
        let columns = columns.as_object();
        let before = screen(3, 200.0, &["서울", "도쿄", "런던", "파리"]);
        let after = screen(4, 20.0, &["런던", "파리", "뉴욕"]);
        let cities = |key: &[String]| -> Vec<String> {
            let prev = Session::rows_on_screen(&before, &row, columns, key).unwrap();
            let page = Session::rows_on_screen(&after, &row, columns, key).unwrap();
            new_rows(&prev, &page).iter().map(|r| r["city"].as_str().unwrap().to_string()).collect()
        };
        // By the whole row, the two that were there already look new.
        assert_eq!(cities(&[]), ["런던", "파리", "뉴욕"]);
        assert_eq!(cities(&["city".to_string()]), ["뉴욕"]);
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
