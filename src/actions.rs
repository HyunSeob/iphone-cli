//! The commands themselves, shared by the CLI, the MCP server, `batch` and
//! recipes. Each takes JSON arguments and returns the fields of its result.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use unicode_normalization::UnicodeNormalization;

use crate::{
    ax::tree::{BuildOptions, Element, Rect, Screen},
    device::{
        ax::trace,
        input::{Motion, normalize},
        session::{Fresh, Session, reuse},
    },
    fields,
    i18n::t,
    reply::{Fail, Fields, Res},
    safety::{self, OpRecord},
    selector::{Selector, dedupe_targets},
};

/// How long after an input to ask for the new screen. Measured on iOS 27: a
/// request made while a navigation animation still runs comes back about
/// 0.4 s later than one made just after it (2026-11-10, 14 taps).
pub const AFTER_TAP: Duration = Duration::from_millis(480);
/// A drag that ends with the finger at rest only leaves a short bounce.
pub const AFTER_DRAG: Duration = Duration::from_millis(300);
pub const AFTER_LAUNCH: Duration = Duration::from_millis(600);
pub const AFTER_PASTE: Duration = Duration::from_millis(200);

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReadArgs {
    /// Only the foreground application's elements.
    pub app: bool,
    /// Include elements outside the screen.
    pub all: bool,
    /// Wait for the second pass so identifiers are present.
    pub full: bool,
    /// Attach every numbered attribute.
    pub raw: bool,
    /// Accept a cached reading up to this old instead of asking the phone.
    pub max_age_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindArgs {
    pub selector: Value,
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TapArgs {
    /// A selector (string or object), `"x,y"`, or `{x, y}`.
    pub target: Value,
    pub long_ms: Option<u64>,
    pub double: bool,
    pub confirm: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SwipeArgs {
    pub direction: Option<String>,
    pub from: Option<[f64; 2]>,
    pub to: Option<[f64; 2]>,
    pub duration_ms: Option<u64>,
    /// Slow to a stop before lifting (a drag) instead of flicking.
    pub drag: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScrollArgs {
    pub direction: String,
    pub until: Option<Value>,
    pub max: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TypeArgs {
    pub text: String,
    pub clear: bool,
    pub submit: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyArgs {
    pub key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ButtonArgs {
    pub button: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchArgs {
    pub app: String,
    /// Quit the app first, so it starts from its first screen.
    #[serde(default)]
    pub restart: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotArgs {
    pub path: String,
}

pub fn parse_args<T: for<'de> Deserialize<'de>>(args: &Value) -> Res<T> {
    let v = if args.is_null() { json!({}) } else { args.clone() };
    serde_json::from_value(v).map_err(|e| Fail::invalid(e.to_string()))
}

pub fn brief(el: &Element) -> Value {
    let mut v = json!({ "id": el.id, "role": el.role });
    if let Some(l) = &el.label {
        v["label"] = json!(l);
    }
    if let Some(val) = &el.value {
        v["value"] = json!(val);
    }
    if let Some(i) = &el.identifier {
        v["identifier"] = json!(i);
    }
    if let Some(f) = &el.frame {
        v["frame"] = json!(f);
    }
    v
}

pub enum Target {
    Selector(Selector),
    Point(f64, f64),
}

pub fn parse_target(v: &Value) -> Res<Target> {
    if let Some(obj) = v.as_object() {
        if let (Some(x), Some(y)) = (
            obj.get("x").and_then(|n| n.as_f64()),
            obj.get("y").and_then(|n| n.as_f64()),
        ) {
            if obj.len() == 2 {
                return Ok(Target::Point(x, y));
            }
        }
    }
    if let Some(s) = v.as_str() {
        let parts: Vec<&str> = s.split(',').map(|p| p.trim()).collect();
        if parts.len() == 2 {
            if let (Ok(x), Ok(y)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                return Ok(Target::Point(x, y));
            }
        }
    }
    Selector::from_value(v)
        .map(Target::Selector)
        .map_err(|e| Fail::invalid(e.to_string()))
}

/// The single element a selector names, or an error that lists what matched.
pub fn resolve_one<'a>(sel: &Selector, screen: &'a Screen) -> Res<&'a Element> {
    let matches = dedupe_targets(sel.find(screen));
    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(Fail::not_sent(
            "not_found",
            format!("no element matches {}", sel.describe()),
        )
        .hint(t(
            "`iph read`로 현재 화면의 이름표를 확인하세요. 화면 밖에 있다면 `iph scroll down --until …`을 쓰세요.",
            "Check the labels on screen with `iph read`. If it is off screen, use `iph scroll down --until …`.",
        ))
        .with("selector", sel.describe())
        .with("screen_summary", screen.summary(12))),
        n => Err(Fail::not_sent(
            "ambiguous_selector",
            format!("{n} elements match {}", sel.describe()),
        )
        .hint(t(
            "role=, within=, index= 으로 하나만 남게 좁히세요.",
            "Narrow it to one with role=, within= or index=.",
        ))
        .with("selector", sel.describe())
        .with(
            "candidates",
            Value::Array(matches.iter().take(10).map(|e| brief(e)).collect()),
        )),
    }
}

/// A point inside the visible part of `el` that no other element covers.
fn tap_point(el: &Element, screen: &Screen) -> Res<(f64, f64)> {
    let frame = el.frame.ok_or_else(|| {
        Fail::not_sent("no_frame", "the element has no position on screen").with("element", brief(el))
    })?;
    let bounds = Rect { x: 0.0, y: 0.0, w: screen.width, h: screen.height };
    let visible = frame.intersection(&bounds).ok_or_else(|| {
        Fail::not_sent("off_screen", "the element is outside the screen")
            .hint(t(
                "`iph scroll down --until …`으로 화면 안으로 가져오세요.",
                "Bring it on screen with `iph scroll down --until …`.",
            ))
            .with("element", brief(el))
    })?;
    let covered = |x: f64, y: f64| -> Option<&Element> {
        let hit = screen.element_at(x, y)?;
        if hit.id == el.id {
            return None;
        }
        // A child of the target (a label inside a row) is fine; an element
        // that merely overlaps it (a toolbar over a list) is not.
        let inside = hit.frame.is_some_and(|h| {
            h.x >= frame.x - 1.0
                && h.y >= frame.y - 1.0
                && h.x + h.w <= frame.x + frame.w + 1.0
                && h.y + h.h <= frame.y + frame.h + 1.0
        });
        let encloses = hit.frame.is_some_and(|h| {
            frame.x >= h.x - 1.0
                && frame.y >= h.y - 1.0
                && frame.x + frame.w <= h.x + h.w + 1.0
                && frame.y + frame.h <= h.y + h.h + 1.0
        });
        (!inside && !encloses).then_some(hit)
    };
    let candidates = [(0.5, 0.5), (0.5, 0.25), (0.5, 0.75), (0.25, 0.5), (0.75, 0.5)];
    let mut blocker = None;
    for (fx, fy) in candidates {
        let (x, y) = (visible.x + visible.w * fx, visible.y + visible.h * fy);
        match covered(x, y) {
            None => return Ok((x, y)),
            Some(b) => blocker = blocker.or(Some(b)),
        }
    }
    Err(Fail::not_sent("covered", "another element covers the target")
        .hint(t(
            "스크롤해서 가리는 요소(도구 막대 등) 밖으로 옮긴 뒤 다시 시도하세요.",
            "Scroll so the target is clear of the covering element (a toolbar, for example) and try again.",
        ))
        .with("element", brief(el))
        .with("covered_by", blocker.map(brief).unwrap_or(Value::Null)))
}

fn nfc(s: &str) -> String {
    s.nfc().collect()
}

/// The app now in front, when it is not the one that was in front before.
fn app_changed(before: &Screen, after: &Screen) -> Option<Value> {
    let was: Vec<i64> = before.apps.iter().map(|a| a.pid).collect();
    let now = after.apps.first()?;
    (!was.contains(&now.pid)).then(|| json!({ "name": now.name, "pid": now.pid }))
}

impl Session {
    fn build_opts(all: bool, raw: bool) -> BuildOptions {
        BuildOptions { include_offscreen: all, raw }
    }

    pub async fn cmd_read(&mut self, args: ReadArgs) -> Res {
        let fresh = match args.max_age_ms {
            Some(ms) => Fresh::MaxAge(Duration::from_millis(ms)),
            None => Fresh::Force,
        };
        let (seq, mut screen) = self
            .screen(fresh, args.full || args.raw, Self::build_opts(args.all, args.raw))
            .await?;
        if args.app {
            if let Some(front) = screen.apps.first().map(|a| a.pid) {
                screen.elements.retain(|e| e.pid == front);
                screen.apps.retain(|a| a.pid == front);
            }
        }
        let has_ids = screen.elements.iter().any(|e| e.identifier.is_some());
        Ok(fields! {
            "screen": {"width": screen.width, "height": screen.height},
            "apps": screen.apps,
            "count": screen.elements.len(),
            "pass": if args.full || args.raw || has_ids { "full" } else { "initial" },
            "seq": seq,
            "elements": screen.elements,
        })
    }

    pub async fn cmd_find(&mut self, args: FindArgs) -> Res {
        let sel = Selector::from_value(&args.selector).map_err(|e| Fail::invalid(e.to_string()))?;
        let (_, screen) = self
            .screen(Fresh::Force, sel.needs_identifier(), Self::build_opts(args.all, false))
            .await?;
        let matches = sel.find(&screen);
        let mut out = fields! {
            "selector": sel.describe(),
            "count": matches.len(),
            "elements": matches.iter().map(|e| (*e).clone()).collect::<Vec<_>>(),
        };
        if sel.needs_identifier() && screen.elements.iter().all(|e| e.identifier.is_none()) {
            out.insert(
                "note".into(),
                json!("this reading came without identifiers, so id= could not match; try again"),
            );
        }
        Ok(out)
    }

    /// Read the screen after an input: wait `delay` for it to react, then ask
    /// the phone for a new dump. The phone does not push dumps on its own.
    pub(crate) async fn observe(&mut self, delay: Duration) -> Option<(u64, Screen)> {
        tokio::time::sleep(delay).await;
        self.screen(Fresh::Force, false, BuildOptions::default()).await.ok()
    }

    /// Check the safety guard for a tap on `el`. `Ok(Some(record))` means the
    /// tap is confirmed and the record must be finished afterwards.
    pub fn guard_tap(
        &self,
        el: Option<&Element>,
        confirm: Option<&str>,
        command: &str,
        force_confirm: bool,
    ) -> Res<Option<OpRecord>> {
        let label = el.and_then(|e| e.label.as_deref()).unwrap_or("");
        let matched = self.guard.check(label).map(String::from);
        if matched.is_none() && !force_confirm {
            return Ok(None);
        }
        let element = el.map(brief).unwrap_or(Value::Null);
        let Some(op_id) = confirm else {
            let mut fail = Fail::blocked(
                "confirm_required",
                match &matched {
                    Some(w) => format!("the target's label contains the guarded word \"{w}\""),
                    None => "this step is marked confirm: true".to_string(),
                },
            )
            .hint(t(
                "정말 눌러야 한다면 사람에게 확인받은 뒤 새 ID로 `--confirm <op_id>`를 붙여 다시 실행하세요. 같은 op_id는 한 번만 쓸 수 있습니다.",
                "If it really must be pressed, get a person's approval and run again with `--confirm <op_id>` using a new id. An op_id works once.",
            ))
            .with("element", element);
            if let Some(w) = matched {
                fail = fail.with("matched_word", w);
            }
            return Err(fail);
        };
        if !safety::valid_op_id(op_id) {
            return Err(Fail::invalid(
                "op_id must be 1-128 characters of letters, digits and -_.:#",
            ));
        }
        let rec = OpRecord {
            op_id: op_id.to_string(),
            at: safety::now_utc(),
            udid: self.udid.clone(),
            state: "started".into(),
            command: command.to_string(),
            target: el.and_then(|e| e.label.clone()),
            matched_word: matched,
        };
        match safety::begin(&rec) {
            Ok(Ok(())) => Ok(Some(rec)),
            Ok(Err(earlier)) => Err(Fail::blocked(
                "op_already_used",
                format!("op_id \"{op_id}\" was already used"),
            )
            .hint(t(
                "같은 op_id로는 다시 누르지 않습니다. 결과는 `iph op <op_id>`로 조회하세요. 결과가 불확실하면 새 ID로 재시도하지 말고 화면을 확인하세요.",
                "The same op_id is never pressed twice. Look it up with `iph op <op_id>`. If the outcome is uncertain, check the screen instead of retrying with a new id.",
            ))
            .with("records", json!(earlier))),
            Err(e) => Err(Fail::not_sent("ops_log_failed", format!("{e:#}"))),
        }
    }

    pub fn finish_op(rec: Option<OpRecord>, state: &str) {
        if let Some(mut rec) = rec {
            rec.state = state.to_string();
            rec.at = safety::now_utc();
            let _ = safety::finish(&rec);
        }
    }

    pub async fn cmd_tap(&mut self, args: TapArgs) -> Res {
        self.tap_impl(args, false).await
    }

    /// `force_confirm`: the caller (a recipe step with `confirm: true`)
    /// requires an op_id even when the label is not a guarded word.
    pub(crate) async fn tap_impl(&mut self, args: TapArgs, force_confirm: bool) -> Res {
        if args.double && args.long_ms.is_some() {
            return Err(Fail::invalid("--double and --long cannot be combined"));
        }
        if let Some(ms) = args.long_ms {
            if !(1..=60_000).contains(&ms) {
                return Err(Fail::invalid("--long must be 1..60000 ms"));
            }
        }
        let target = parse_target(&args.target)?;
        let need_ids = matches!(&target, Target::Selector(s) if s.needs_identifier());
        let (_, screen) = self
            .screen(Fresh::MaxAge(reuse()), need_ids, BuildOptions::default())
            .await?;
        if screen.width <= 0.0 || screen.height <= 0.0 {
            return Err(Fail::not_sent("no_screen_size", "the reading has no application frame"));
        }
        let (x, y, element) = match &target {
            Target::Selector(sel) => {
                let el = resolve_one(sel, &screen)?;
                let (x, y) = tap_point(el, &screen)?;
                (x, y, Some(el))
            }
            Target::Point(x, y) => {
                if *x < 0.0 || *y < 0.0 || *x > screen.width || *y > screen.height {
                    return Err(Fail::invalid(format!(
                        "point {x},{y} is outside the {}x{} screen",
                        screen.width, screen.height
                    )));
                }
                (*x, *y, screen.element_at(*x, *y))
            }
        };
        let element_json = element.map(brief);
        self.check_front_allowed(&screen).await?;
        let op = self.guard_tap(element, args.confirm.as_deref(), "tap", force_confirm)?;
        let op_id = op.as_ref().map(|o| o.op_id.clone());

        let (nx, ny) = (normalize(x, screen.width), normalize(y, screen.height));
        let before = screen.signature();
        let input = match self.ensure_input().await {
            Ok(i) => i,
            Err(e) => {
                Self::finish_op(op, "not_sent");
                return Err(e);
            }
        };
        let sent = if args.double {
            input.double_tap(nx, ny).await
        } else {
            let hold = Duration::from_millis(args.long_ms.unwrap_or(50));
            input.tap(nx, ny, hold).await
        };
        if let Err(e) = sent {
            // The report may or may not have reached the phone.
            self.input = None;
            Self::finish_op(op, "unknown");
            let mut fail = Fail::unknown("input_failed", format!("{e:#}")).hint(t(
                "입력이 나갔을 수 있습니다. 다시 보내지 말고 `iph read`로 화면을 확인하세요.",
                "The input may have gone out. Do not resend; check the screen with `iph read`.",
            ));
            if let Some(id) = op_id {
                fail = fail.with("op_id", id);
            }
            return Err(fail);
        }
        let after = self.observe(AFTER_TAP).await;
        let changed = after.as_ref().map(|(_, s)| s.signature() != before);
        Self::finish_op(op, "done");
        let app_change = after.as_ref().and_then(|(_, s)| app_changed(&screen, s));

        let mut out = fields! {
            "sent": true,
            "verified": changed.is_some(),
            "screen_changed": changed.unwrap_or(false),
            "x": (x * 10.0).round() / 10.0,
            "y": (y * 10.0).round() / 10.0,
        };
        if let Some(e) = element_json {
            out.insert("element".into(), e);
        }
        if let Some(id) = op_id {
            out.insert("op_id".into(), json!(id));
        }
        if changed.is_none() {
            out.insert(
                "note".into(),
                json!("the tap was sent but no reading came back to compare"),
            );
        }
        if let Some(change) = app_change {
            out.insert("app_changed".into(), json!(true));
            out.insert("app".into(), change);
        }
        Ok(out)
    }

    /// Tap several targets in a row on the screen as it is now: every target
    /// is found (and checked by the safety guard) before the first tap goes
    /// out, nothing is re-read in between, and the screen is read once at the
    /// end. For keypads and other screens that hold still while tapped.
    pub(crate) async fn tap_sequence(&mut self, targets: &[Value]) -> Res {
        if targets.is_empty() {
            return Err(Fail::invalid("the tap list is empty"));
        }
        let parsed: Vec<Target> = targets.iter().map(parse_target).collect::<Res<_>>()?;
        let need_ids = parsed
            .iter()
            .any(|t| matches!(t, Target::Selector(s) if s.needs_identifier()));
        let (_, screen) = self
            .screen(Fresh::MaxAge(reuse()), need_ids, BuildOptions::default())
            .await?;
        if screen.width <= 0.0 || screen.height <= 0.0 {
            return Err(Fail::not_sent("no_screen_size", "the reading has no application frame"));
        }
        self.check_front_allowed(&screen).await?;
        let mut points: Vec<(f64, f64, Option<&Element>)> = Vec::new();
        for target in &parsed {
            let (x, y, el) = match target {
                Target::Selector(sel) => {
                    let el = resolve_one(sel, &screen)?;
                    let (x, y) = tap_point(el, &screen)?;
                    (x, y, Some(el))
                }
                Target::Point(x, y) => (*x, *y, screen.element_at(*x, *y)),
            };
            // A guarded target stops the whole sequence before it starts; a
            // confirmed tap is always a step of its own.
            self.guard_tap(el, None, "tap", false)?;
            points.push((x, y, el));
        }
        let before = screen.signature();
        let normalized: Vec<(u16, u16)> = points
            .iter()
            .map(|(x, y, _)| (normalize(*x, screen.width), normalize(*y, screen.height)))
            .collect();
        let tapped: Vec<Value> = points
            .iter()
            .map(|(_, _, el)| el.map(brief).unwrap_or(Value::Null))
            .collect();
        let input = self.ensure_input().await?;
        for (i, (nx, ny)) in normalized.iter().enumerate() {
            if let Err(e) = input.tap(*nx, *ny, Duration::from_millis(50)).await {
                self.input = None;
                return Err(Fail::unknown("input_failed", format!("{e:#}"))
                    .hint(t(
                        "입력이 일부 나갔을 수 있습니다. 다시 보내지 말고 `iph read`로 화면을 확인하세요.",
                        "Part of the input may have gone out. Do not resend; check the screen with `iph read`.",
                    ))
                    .with("taps_sent", i));
            }
        }
        let after = self.observe(AFTER_TAP).await;
        let changed = after.as_ref().map(|(_, s)| s.signature() != before);
        Ok(fields! {
            "sent": true,
            "verified": changed.is_some(),
            "screen_changed": changed.unwrap_or(false),
            "taps": tapped.len(),
            "elements": tapped,
        })
    }

    /// Drag between two points given in logical points and report whether the
    /// screen changed.
    async fn drag_points(
        &mut self,
        from: (f64, f64),
        to: (f64, f64),
        duration: Duration,
        motion: Motion,
    ) -> Res<(bool, Option<Screen>)> {
        let (_, screen) = self
            .screen(Fresh::MaxAge(reuse()), false, BuildOptions::default())
            .await?;
        if screen.width <= 0.0 || screen.height <= 0.0 {
            return Err(Fail::not_sent("no_screen_size", "the reading has no application frame"));
        }
        for (x, y) in [from, to] {
            if x < 0.0 || y < 0.0 || x > screen.width || y > screen.height {
                return Err(Fail::invalid(format!(
                    "point {x},{y} is outside the {}x{} screen",
                    screen.width, screen.height
                )));
            }
        }
        self.check_front_allowed(&screen).await?;
        let before = screen.signature();
        let a = (normalize(from.0, screen.width), normalize(from.1, screen.height));
        let b = (normalize(to.0, screen.width), normalize(to.1, screen.height));
        let input = self.ensure_input().await?;
        if let Err(e) = input.drag(a, b, duration, motion).await {
            self.input = None;
            return Err(Fail::unknown("input_failed", format!("{e:#}")).hint(t(
                "입력이 나갔을 수 있습니다. 다시 보내지 말고 `iph read`로 화면을 확인하세요.",
                "The input may have gone out. Do not resend; check the screen with `iph read`.",
            )));
        }
        let mut after = self.observe(AFTER_DRAG).await.map(|(_, s)| s);
        if motion == Motion::Flick {
            // The content coasts after a flick: read until two readings agree.
            for _ in 0..3 {
                let next = self.observe(Duration::from_millis(150)).await.map(|(_, s)| s);
                let still = match (&after, &next) {
                    (Some(a), Some(b)) => a.signature() == b.signature(),
                    _ => true,
                };
                after = next.or(after);
                if still {
                    break;
                }
            }
        }
        let changed = after.as_ref().is_some_and(|s| s.signature() != before);
        Ok((changed, after))
    }

    async fn screen_size(&mut self) -> Res<(f64, f64)> {
        let (_, screen) = self
            .screen(Fresh::MaxAge(reuse()), false, BuildOptions::default())
            .await?;
        Ok((screen.width, screen.height))
    }

    pub async fn cmd_swipe(&mut self, args: SwipeArgs) -> Res {
        let (w, h) = self.screen_size().await?;
        let (from, to) = match (&args.direction, args.from, args.to) {
            (Some(dir), None, None) => {
                // The finger moves in the named direction across the middle.
                let (cx, cy) = (w / 2.0, h / 2.0);
                match dir.as_str() {
                    "up" => ((cx, h * 0.70), (cx, h * 0.30)),
                    "down" => ((cx, h * 0.30), (cx, h * 0.70)),
                    "left" => ((w * 0.80, cy), (w * 0.20, cy)),
                    "right" => ((w * 0.20, cy), (w * 0.80, cy)),
                    other => {
                        return Err(Fail::invalid(format!(
                            "unknown direction `{other}` (up, down, left, right)"
                        )));
                    }
                }
            }
            (None, Some(f), Some(t)) => ((f[0], f[1]), (t[0], t[1])),
            _ => return Err(Fail::invalid("give a direction, or both from and to")),
        };
        let duration = Duration::from_millis(args.duration_ms.unwrap_or(250).clamp(50, 10_000));
        let motion = if args.drag { Motion::Place } else { Motion::Flick };
        let (changed, _) = self.drag_points(from, to, duration, motion).await?;
        Ok(fields! {
            "sent": true,
            "verified": true,
            "screen_changed": changed,
            "from": [from.0, from.1],
            "to": [to.0, to.1],
        })
    }

    /// Move the content one step. "down" shows what is further down.
    async fn scroll_step(&mut self, direction: &str) -> Res<(bool, Option<Screen>)> {
        let (w, h) = self.screen_size().await?;
        let cx = w / 2.0;
        let (from, to) = match direction {
            "down" => ((cx, h * 0.68), (cx, h * 0.32)),
            "up" => ((cx, h * 0.32), (cx, h * 0.68)),
            other => {
                return Err(Fail::invalid(format!("unknown direction `{other}` (up, down)")));
            }
        };
        // A drag that slows to a stop moves the list by the finger's distance,
        // without a fling that would overshoot.
        self.drag_points(from, to, Duration::from_millis(500), Motion::Place)
            .await
    }

    pub async fn cmd_scroll(&mut self, args: ScrollArgs) -> Res {
        if args.direction != "up" && args.direction != "down" {
            return Err(Fail::invalid("direction must be up or down"));
        }
        let Some(until) = &args.until else {
            let (changed, _) = self.scroll_step(&args.direction).await?;
            return Ok(fields! {
                "sent": true,
                "verified": true,
                "screen_changed": changed,
                "scrolls": 1,
            });
        };
        let sel = Selector::from_value(until).map_err(|e| Fail::invalid(e.to_string()))?;
        let max = args.max.unwrap_or(10).clamp(1, 100);
        let full = sel.needs_identifier();
        let mut scrolls = 0u32;
        let (_, mut screen) = self.screen(Fresh::MaxAge(reuse()), full, BuildOptions::default()).await?;
        loop {
            let matches = dedupe_targets(sel.find(&screen));
            // Found means: on screen and tappable (not under a toolbar).
            if let Some(el) = matches.iter().find(|e| tap_point(e, &screen).is_ok()) {
                if matches.len() > 1 {
                    return Err(Fail::not_sent(
                        "ambiguous_selector",
                        format!("{} elements match {}", matches.len(), sel.describe()),
                    )
                    .with("candidates", Value::Array(matches.iter().map(|e| brief(e)).collect()))
                    .with("scrolls", scrolls));
                }
                return Ok(fields! {
                    "found": true,
                    "scrolls": scrolls,
                    "screen_changed": scrolls > 0,
                    "element": brief(el),
                });
            }
            if scrolls >= max {
                return Err(Fail::new(
                    crate::reply::Outcome::Done,
                    "not_found",
                    format!("{} not found after {scrolls} scrolls", sel.describe()),
                )
                .with("found", false)
                .with("scrolls", scrolls)
                .with("screen_summary", screen.summary(12)));
            }
            let (mut changed, mut after) = self.scroll_step(&args.direction).await?;
            scrolls += 1;
            if !changed {
                // One still screen can be a dropped drag; two in a row is the
                // end of the list.
                (changed, after) = self.scroll_step(&args.direction).await?;
                scrolls += 1;
            }
            if !changed {
                return Err(Fail::new(
                    crate::reply::Outcome::Done,
                    "not_found",
                    format!(
                        "{} not found; the list stopped moving after {scrolls} scrolls",
                        sel.describe()
                    ),
                )
                .with("found", false)
                .with("scrolls", scrolls)
                .with("screen_summary", screen.summary(12)));
            }
            screen = match (after, full) {
                (Some(s), false) => s,
                _ => self.screen(Fresh::MaxAge(reuse()), full, BuildOptions::default()).await?.1,
            };
        }
    }

    pub async fn cmd_key(&mut self, args: KeyArgs) -> Res {
        let (usage, modifiers) = crate::keys::parse_combo(&args.key).map_err(Fail::invalid)?;
        let (_, screen) = self
            .screen(Fresh::MaxAge(reuse()), false, BuildOptions::default())
            .await?;
        self.check_front_allowed(&screen).await?;
        let before = Some(screen.signature());
        let input = self.ensure_input().await?;
        if let Err(e) = input.key(usage, &modifiers).await {
            self.input = None;
            return Err(Fail::unknown("input_failed", format!("{e:#}")));
        }
        let changed = match before {
            Some(sig) => self.observe(AFTER_TAP).await.map(|(_, sc)| sc.signature() != sig),
            None => None,
        };
        Ok(fields! {
            "sent": true,
            "verified": changed.is_some(),
            "screen_changed": changed.unwrap_or(false),
            "key": args.key,
        })
    }

    pub async fn cmd_button(&mut self, args: ButtonArgs) -> Res {
        let (page, usage, hold) = crate::keys::button(&args.button).map_err(Fail::invalid)?;
        let (_, screen) = self
            .screen(Fresh::MaxAge(reuse()), false, BuildOptions::default())
            .await?;
        self.check_front_allowed(&screen).await?;
        let before = Some(screen.signature());
        let input = self.ensure_input().await?;
        if let Err(e) = input.button(page, usage, Duration::from_millis(hold)).await {
            self.input = None;
            return Err(Fail::unknown("input_failed", format!("{e:#}")));
        }
        let changed = match before {
            Some(sig) => self.observe(AFTER_LAUNCH).await.map(|(_, sc)| sc.signature() != sig),
            None => None,
        };
        Ok(fields! {
            "sent": true,
            "verified": changed.is_some(),
            "screen_changed": changed.unwrap_or(false),
            "button": args.button,
        })
    }

    pub async fn cmd_apps(&mut self) -> Res {
        let apps = self.list_apps(true).await?;
        let list: Vec<Value> = apps
            .iter()
            .map(|a| {
                json!({
                    "bundle_id": a.bundle_identifier,
                    "name": a.name,
                    "version": a.version,
                    "first_party": a.is_first_party,
                    "removable": a.is_removable,
                })
            })
            .collect();
        Ok(fields! { "count": list.len(), "apps": list })
    }

    async fn resolve_bundle(&mut self, app: &str) -> Res<String> {
        // Something with a dot and no spaces is a bundle id; anything else is
        // looked up by name.
        if app.contains('.') && !app.contains(' ') {
            return Ok(app.to_string());
        }
        let want = nfc(app).to_lowercase();
        let apps = self.list_apps(false).await?;
        let hits: Vec<&idevice::core_device::AppListEntry> = apps
            .iter()
            .filter(|a| nfc(&a.name).to_lowercase() == want)
            .collect();
        match hits.len() {
            1 => Ok(hits[0].bundle_identifier.clone()),
            0 => Err(Fail::not_sent("app_not_found", format!("no installed app is named \"{app}\""))
                .hint(t(
                    "`iph apps`로 이름과 bundle id를 확인하세요.",
                    "See names and bundle ids with `iph apps`.",
                ))),
            _ => Err(Fail::not_sent(
                "ambiguous_app",
                format!("{} apps are named \"{app}\"", hits.len()),
            )
            .with(
                "candidates",
                Value::Array(hits.iter().map(|a| json!(a.bundle_identifier)).collect()),
            )),
        }
    }

    pub async fn cmd_launch(&mut self, args: LaunchArgs) -> Res {
        let bundle = self.resolve_bundle(&args.app).await?;
        self.check_launch_allowed(&bundle)?;
        let mut svc = self.app_service().await?;
        // From here the launch request may have reached the phone.
        let launched = tokio::time::timeout(
            Duration::from_secs(30),
            svc.launch_application(bundle.as_str(), &[], args.restart, false, None, None, None),
        )
        .await
        .map_err(|_| "no answer within 30 s".to_string())
        .and_then(|r| r.map_err(|e| format!("{e:?}")));
        let resp = match launched {
            Ok(r) => r,
            Err(msg) => {
                let locked = msg.to_lowercase().contains("lock");
                return Err(Fail::unknown("launch_failed", msg).hint(if locked {
                    t("폰 잠금을 풀어 주세요.", "Unlock the phone.")
                } else {
                    t(
                        "bundle id가 맞는지 `iph apps`로 확인하세요.",
                        "Check the bundle id with `iph apps`.",
                    )
                }));
            }
        };
        let pid = resp.pid as i64;
        // The app is in front when a reading lists it as an application.
        let mut foreground = false;
        let mut delay = AFTER_LAUNCH;
        for _ in 0..4 {
            if let Some((_, screen)) = self.observe(delay).await {
                if screen.apps.iter().any(|a| a.pid == pid) {
                    foreground = true;
                    break;
                }
            }
            delay = Duration::from_millis(300);
        }
        Ok(fields! {
            "sent": true,
            "verified": foreground,
            "foreground": foreground,
            "bundle_id": bundle,
            "pid": pid,
        })
    }

    pub async fn cmd_screenshot(&mut self, args: ScreenshotArgs) -> Res {
        let png = self.screenshot_png().await?;
        let path = std::path::PathBuf::from(&args.path);
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|e| Fail::not_sent("write_failed", format!("{}: {e}", dir.display())))?;
        }
        std::fs::write(&path, &png)
            .map_err(|e| Fail::not_sent("write_failed", format!("{}: {e}", path.display())))?;
        Ok(fields! { "path": args.path, "bytes": png.len() })
    }

    /// The text element that has the keyboard, if exactly one does.
    fn focused_field(screen: &Screen) -> Option<&Element> {
        let mut focused = screen.elements.iter().filter(|e| e.focused);
        let first = focused.next()?;
        focused.next().is_none().then_some(first)
    }

    pub async fn cmd_type(&mut self, args: TypeArgs) -> Res {
        if args.text.is_empty() && !args.clear {
            return Err(Fail::invalid("nothing to type"));
        }
        if args.text.len() > 1024 * 1024 {
            return Err(Fail::invalid("text is larger than 1 MiB"));
        }
        let (_, screen) = self.screen(Fresh::MaxAge(reuse()), false, BuildOptions::default()).await?;
        let Some(field) = Self::focused_field(&screen).cloned() else {
            return Err(Fail::not_sent(
                "no_focused_field",
                "no text field has the keyboard right now",
            )
            .hint(t(
                "먼저 입력할 칸을 `iph tap`으로 눌러 키보드를 띄우세요.",
                "Tap the field first with `iph tap` so the keyboard comes up.",
            ))
            .with("screen_summary", screen.summary(12)));
        };
        let before_value = field.value.clone().unwrap_or_default();
        self.check_front_allowed(&screen).await?;
        trace(|| "type: focused field found".into());

        // The input session first: if it cannot be had, nothing has been
        // touched yet, the clipboard included.
        self.ensure_input().await?;
        let (adapter, port) = self.service_port(PASTEBOARD_SERVICE).await?;

        // Remember what the user had on the clipboard and, meanwhile, register
        // the temporary keyboard (no key goes out yet).
        let input = self.ensure_input().await?;
        let (saved, attached) = tokio::join!(Clipboard::save(&adapter, port), input.keyboard_attach());
        trace(|| "type: clipboard saved, keyboard attached".into());
        if let Err(e) = attached {
            // Nothing has been touched: the clipboard still holds the user's content.
            return Err(Fail::not_sent("keyboard_failed", format!("{e:#}")));
        }
        if !args.text.is_empty() {
            if let Err(e) = Clipboard::set_text(&adapter, port, &args.text).await {
                let _ = input.keyboard_detach().await;
                self.invalidate_readings();
                return Err(Fail::not_sent("clipboard_failed", e));
            }
        }
        trace(|| "type: clipboard set".into());

        let cmd = vec![crate::keys::MOD_COMMAND];
        let mut combos: Vec<(u64, Vec<u64>)> = Vec::new();
        if args.clear {
            combos.push((crate::keys::usage('a').unwrap(), cmd.clone()));
            combos.push((crate::keys::KEY_BACKSPACE, vec![]));
        }
        if !args.text.is_empty() {
            combos.push((crate::keys::usage('v').unwrap(), cmd.clone()));
        }
        let pressed = input.keyboard_press(&combos).await;
        trace(|| "type: keys sent".into());
        if let Err(e) = pressed {
            let _ = input.keyboard_detach().await;
            self.input = None;
            self.invalidate_readings();
            let restored = saved.restore(&adapter, port, &args.text).await;
            return Err(Fail::unknown("input_failed", format!("{e:#}"))
                .hint(t(
                    "입력이 나갔을 수 있습니다. 다시 보내지 말고 `iph read`로 화면을 확인하세요.",
                    "The input may have gone out. Do not resend; check the screen with `iph read`.",
                ))
                .with("clipboard_restored", restored));
        }

        // The paste has happened by now, so the clipboard can go back while
        // the field is being checked. The temporary keyboard stays attached
        // meanwhile: the screen holds still and Return can follow at once.
        let want = nfc(&args.text);
        let expected_full = if args.clear {
            want.clone()
        } else {
            format!("{}{}", nfc(&before_value), want)
        };
        let restoring = saved.restore(&adapter, port, &args.text);
        let checking = async {
            let mut value = None;
            let mut verified = false;
            for attempt in 0..3 {
                let delay = if attempt == 0 { AFTER_PASTE } else { Duration::from_millis(400) };
                let Some((_, screen)) = self.observe(delay).await else { continue };
                let now = Self::focused_field(&screen)
                    .or_else(|| screen.by_id(&field.id))
                    .and_then(|e| e.value.clone())
                    .unwrap_or_default();
                let now_n = nfc(&now);
                verified = if want.is_empty() {
                    now_n.is_empty() || now_n != nfc(&before_value)
                } else {
                    now_n == expected_full || now_n.contains(&want)
                };
                value = Some(now);
                if verified {
                    break;
                }
            }
            (value, verified)
        };
        let (restored, (value, verified)) = tokio::join!(restoring, checking);
        trace(|| format!("type: verified={verified} clipboard_restored={restored}"));

        let mut submitted = false;
        let mut submit_error = None;
        if verified && args.submit {
            if let Some(input) = self.input.as_mut() {
                match input.keyboard_press(&[(crate::keys::KEY_RETURN, vec![])]).await {
                    Ok(()) => submitted = true,
                    Err(e) => submit_error = Some(format!("{e:#}")),
                }
            }
        }
        if let Some(input) = self.input.as_mut() {
            if input.keyboard_detach().await.is_err() {
                self.input = None;
            }
        }
        // The on-screen keyboard comes back now; readings from before show
        // the screen without it.
        self.invalidate_readings();

        if !verified {
            return Err(Fail::unknown(
                "text_not_verified",
                "the field does not show the text after pasting",
            )
            .hint(t(
                "붙여넣기가 되지 않았을 수 있습니다. `iph read`로 칸의 값을 확인하세요.",
                "The paste may not have happened. Check the field's value with `iph read`.",
            ))
            .with("field", brief(&field))
            .with("value", value.clone().map(Value::String).unwrap_or(Value::Null))
            .with("clipboard_restored", restored));
        }
        if let Some(e) = submit_error {
            self.input = None;
            return Err(Fail::unknown("input_failed", e)
                .with("text_verified", true)
                .with("clipboard_restored", restored));
        }
        Ok(fields! {
            "sent": true,
            "verified": true,
            "text_verified": true,
            "value": value,
            "field": brief(&field),
            "submit_sent": submitted,
            "clipboard_restored": restored,
            "screen_changed": true,
        })
    }
}

const PASTEBOARD_SERVICE: &str = "com.apple.coredevice.pasteboardservice";

/// What the user had on the phone's clipboard before `iph type` borrowed it.
struct Clipboard {
    text: Option<String>,
    image: Option<(String, Vec<u8>)>,
    /// False when the old content could not be read completely; then it
    /// cannot be put back.
    restorable: bool,
}

impl Clipboard {
    /// Every call uses its own connection, so a late answer to one request
    /// cannot be taken for the answer to the next.
    async fn connect(
        adapter: &idevice::tcp::handle::AdapterHandle,
        port: u16,
    ) -> Result<idevice::core_device::PasteboardServiceClient<Box<dyn idevice::ReadWrite>>, String> {
        use idevice::RsdService;
        let mut adapter = adapter.clone();
        let stream = tokio::time::timeout(Duration::from_secs(8), adapter.connect(port))
            .await
            .map_err(|_| "clipboard service: no answer".to_string())?
            .map_err(|e| format!("clipboard service: {e}"))?;
        idevice::core_device::PasteboardServiceClient::from_stream(Box::new(stream))
            .await
            .map_err(|e| format!("clipboard service: {e:?}"))
    }

    async fn get_text(adapter: &idevice::tcp::handle::AdapterHandle, port: u16) -> Option<Option<String>> {
        let mut clip = Self::connect(adapter, port).await.ok()?;
        let general = idevice::core_device::GENERAL_PASTEBOARD;
        match tokio::time::timeout(Duration::from_secs(8), clip.get(general)).await {
            Ok(Ok(snapshot)) => Some(snapshot.text()),
            _ => None,
        }
    }

    async fn set_text(adapter: &idevice::tcp::handle::AdapterHandle, port: u16, text: &str) -> Result<(), String> {
        let mut clip = Self::connect(adapter, port).await?;
        let general = idevice::core_device::GENERAL_PASTEBOARD;
        match tokio::time::timeout(Duration::from_secs(8), clip.set_text(text, general)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("set clipboard: {e:?}")),
            Err(_) => Err("set clipboard: no answer".into()),
        }
    }

    async fn save(adapter: &idevice::tcp::handle::AdapterHandle, port: u16) -> Clipboard {
        let general = idevice::core_device::GENERAL_PASTEBOARD;
        let snapshot = match Self::connect(adapter, port).await {
            Ok(mut clip) => tokio::time::timeout(Duration::from_secs(8), clip.get(general))
                .await
                .ok()
                .and_then(|r| r.ok()),
            Err(_) => None,
        };
        match snapshot {
            Some(snap) => {
                let text = snap.text();
                let image = snap.image().map(|(uti, bytes)| (uti.to_string(), bytes));
                let restorable = text.is_some() || image.is_some() || snap.items.is_empty();
                Clipboard { text, image, restorable }
            }
            None => Clipboard { text: None, image: None, restorable: false },
        }
    }

    /// Put the old content back. True when the clipboard is known to hold
    /// what it held before.
    async fn restore(&self, adapter: &idevice::tcp::handle::AdapterHandle, port: u16, typed: &str) -> bool {
        if typed.is_empty() {
            return true;
        }
        if !self.restorable {
            return false;
        }
        // Give the paste a moment to read the clipboard.
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Only touch the clipboard if it still holds what we put there; the
        // user may have copied something else meanwhile.
        if Self::get_text(adapter, port).await != Some(Some(typed.to_string())) {
            return false;
        }
        let general = idevice::core_device::GENERAL_PASTEBOARD;
        let wrote = match (&self.text, &self.image) {
            (Some(text), _) => Self::set_text(adapter, port, text).await.is_ok(),
            (None, Some((uti, bytes))) => match Self::connect(adapter, port).await {
                Ok(mut clip) => {
                    matches!(
                        tokio::time::timeout(Duration::from_secs(8), clip.set_image(bytes, uti, general)).await,
                        Ok(Ok(()))
                    )
                }
                Err(_) => false,
            },
            // The clipboard was empty: leave it without our text.
            (None, None) => Self::set_text(adapter, port, "").await.is_ok(),
        };
        if !wrote {
            return false;
        }
        match (Self::get_text(adapter, port).await, &self.text) {
            (Some(now), Some(old)) => now.as_deref() == Some(old.as_str()),
            (Some(now), None) => now.as_deref() != Some(typed),
            (None, _) => false,
        }
    }
}

/// Fields of a result as a JSON object.
pub fn to_value(fields: Fields) -> Value {
    Value::Object(fields)
}
