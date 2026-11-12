//! The commands themselves, shared by the CLI, the MCP server, `batch` and
//! scripts. Each takes JSON arguments and returns the fields of its result.

use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use unicode_normalization::UnicodeNormalization;

use crate::{
    ax::{
        attrs,
        dump::ObjKey,
        tree::{BuildOptions, Element, ElementSignature, Rect, Screen},
    },
    device::{
        ax::trace,
        input::{Motion, normalize},
        session::{Fresh, Gesture, Session, reuse},
    },
    fields,
    i18n::t,
    reply::{Fail, Res},
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
/// How long a tap waits for the screen to start changing. A picker that
/// another part of the app puts up came about 1.5 s after the tap
/// (2026-11-12, 당근 '이미지 추가').
pub const CHANGE_WAIT: Duration = Duration::from_millis(2500);
/// How long after the tap a changed screen may take to hold still.
pub const SETTLE_WAIT: Duration = Duration::from_secs(5);
/// A reading up to this old is used for a tap once the phone confirms, by a
/// few quick questions, that the screen is still as read. Older ones are read
/// again in full.
const CONFIRMED_REUSE: Duration = Duration::from_secs(10);
/// How long a pasted text may take to show in its field.
const PASTE_SHOWS: Duration = Duration::from_millis(1500);
/// How long to wait for the answer to one quick question.
const ASK_WAIT: Duration = Duration::from_millis(1000);
/// An unasked dump this recently before an input means the app is one that
/// triggers them; its readings are then double-checked.
const UNASKED_MEMORY: Duration = Duration::from_secs(5);

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
    /// Read the screen once right after the tap instead of waiting for it to
    /// change and hold still.
    pub no_wait: bool,
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
    if let Some(obj) = v.as_object()
        && let (Some(x), Some(y)) = (
            obj.get("x").and_then(|n| n.as_f64()),
            obj.get("y").and_then(|n| n.as_f64()),
        )
            && obj.len() == 2 {
                return Ok(Target::Point(x, y));
            }
    if let Some(s) = v.as_str() {
        let parts: Vec<&str> = s.split(',').map(|p| p.trim()).collect();
        if parts.len() == 2
            && let (Ok(x), Ok(y)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                return Ok(Target::Point(x, y));
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
        if args.app
            && let Some(front) = screen.apps.first().map(|a| a.pid) {
                screen.elements.retain(|e| e.pid == front);
                screen.apps.retain(|a| a.pid == front);
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

    /// Read the screen after an input that began at `since`: wait `delay` for
    /// the screen to react, then ask the phone for a new dump.
    ///
    /// Some apps make the phone send dumps unasked while they change
    /// (Calculator on every key). Such a dump shows the screen as it was a
    /// moment earlier and can arrive late enough to pass for the answer; the
    /// calculator example then read "12×3" instead of "408". So with `careful`
    /// (inputs that change the screen several times), or when the phone has
    /// lately sent unasked dumps, the screen is read until two readings in a
    /// row agree.
    pub(crate) async fn observe_with(
        &mut self,
        since: Instant,
        delay: Duration,
        careful: bool,
    ) -> Option<(u64, Screen)> {
        tokio::time::sleep(delay).await;
        let mut latest = self.screen(Fresh::Force, false, BuildOptions::default()).await.ok()?;
        let lately = since.checked_sub(UNASKED_MEMORY).unwrap_or(since);
        let chatty = self.ax.as_ref().is_some_and(|a| a.unasked_since(lately));
        if !careful && !chatty {
            return Some(latest);
        }
        for _ in 0..2 {
            trace(|| "observe: reading again until two readings agree".into());
            let Ok(next) = self.screen(Fresh::Force, false, BuildOptions::default()).await else {
                break;
            };
            let same = next.1.signature() == latest.1.signature();
            latest = next;
            if same {
                break;
            }
        }
        Some(latest)
    }

    pub(crate) async fn observe(&mut self, since: Instant, delay: Duration) -> Option<(u64, Screen)> {
        self.observe_with(since, delay, false).await
    }

    /// Read the screen after an input until it has changed from `before` and
    /// then holds still, as a browser waits for a page to load. The phone
    /// sends no event when a screen comes up, so "came up" is two readings in
    /// a row that agree. A screen that does not change within `CHANGE_WAIT`,
    /// or keeps moving until `SETTLE_WAIT`, comes back as it is then.
    pub(crate) async fn observe_settled(
        &mut self,
        since: Instant,
        before: &[ElementSignature],
    ) -> Option<(u64, Screen)> {
        let mut latest = self.observe(since, AFTER_TAP).await?;
        while latest.1.signature() == before && since.elapsed() < CHANGE_WAIT {
            trace(|| "observe: the screen has not changed yet; reading again".into());
            match self.screen(Fresh::Force, false, BuildOptions::default()).await {
                Ok(next) => latest = next,
                Err(_) => return Some(latest),
            }
        }
        if latest.1.signature() == before {
            return Some(latest);
        }
        while since.elapsed() < SETTLE_WAIT {
            let Ok(next) = self.screen(Fresh::Force, false, BuildOptions::default()).await else {
                break;
            };
            let same = next.1.signature() == latest.1.signature();
            latest = next;
            if same {
                break;
            }
            trace(|| "observe: the screen is still moving; reading again".into());
        }
        Some(latest)
    }

    /// Read the screen and find where to tap for each target. Fails without
    /// sending anything when a target is missing, ambiguous or covered.
    async fn locate(
        &mut self,
        targets: &[Target],
        fresh: Fresh,
    ) -> Res<(Screen, Vec<(f64, f64, Option<Element>)>)> {
        let need_ids = targets
            .iter()
            .any(|t| matches!(t, Target::Selector(s) if s.needs_identifier()));
        let (_, screen) = self.screen(fresh, need_ids, BuildOptions::default()).await?;
        if screen.width <= 0.0 || screen.height <= 0.0 {
            return Err(Fail::not_sent("no_screen_size", "the reading has no application frame"));
        }
        let mut points = Vec::with_capacity(targets.len());
        for target in targets {
            points.push(match target {
                Target::Selector(sel) => {
                    let el = resolve_one(sel, &screen)?;
                    let (x, y) = tap_point(el, &screen)?;
                    (x, y, Some(el.clone()))
                }
                Target::Point(x, y) => {
                    if *x < 0.0 || *y < 0.0 || *x > screen.width || *y > screen.height {
                        return Err(Fail::invalid(format!(
                            "point {x},{y} is outside the {}x{} screen",
                            screen.width, screen.height
                        )));
                    }
                    (*x, *y, screen.element_at(*x, *y).cloned())
                }
            });
        }
        Ok((screen, points))
    }

    /// Locate the targets on the latest reading when it is too old to reuse
    /// as it is (an agent's turn takes seconds) but the phone confirms it:
    /// every application still shows the same visible children (nothing came
    /// up or went away) and every target is still where it was. Each question
    /// takes tens of milliseconds; a full reading takes over a second. `None`
    /// sends the caller to a full reading.
    async fn locate_confirmed(&mut self, targets: &[Target]) -> Option<(Screen, Vec<(f64, f64, Option<Element>)>)> {
        let age = self.reading_age()?;
        if age <= reuse() || age > CONFIRMED_REUSE || !targets.iter().all(|t| matches!(t, Target::Selector(_))) {
            return None;
        }
        let (_, dump, _) = self.ax.as_ref()?.latest()?;
        let located = self.locate(targets, Fresh::MaxAge(CONFIRMED_REUSE)).await.ok()?;
        let ax = self.ax.as_mut()?;
        for app in &dump.applications {
            let was = dump.elements.get(app).map(|raw| crate::ax::tree::children_of(raw, false))?;
            let now: Vec<ObjKey> = ax
                .ask(*app, attrs::VISIBLE_CHILDREN, ASK_WAIT)
                .await?
                .as_array()?
                .iter()
                .filter_map(ObjKey::from_ka)
                .collect();
            if now != was {
                trace(|| "locate: the screen has changed since the last reading".into());
                return None;
            }
        }
        for (_, _, el) in &located.1 {
            let el = el.as_ref()?;
            let now = ax
                .ask(el.key?, attrs::FRAME, ASK_WAIT)
                .await?
                .get("NS.rectval")
                .and_then(|v| v.as_str())
                .and_then(Rect::parse)?;
            let round = |r: &Rect| (r.x.round(), r.y.round(), r.w.round(), r.h.round());
            if el.frame.as_ref().map(round) != Some(round(&now)) {
                trace(|| "locate: a target has moved since the last reading".into());
                return None;
            }
        }
        trace(|| format!("locate: a reading {} ms old confirmed", age.as_millis()));
        Some(located)
    }

    /// Locate the targets, make sure input can be sent, and locate them again
    /// if getting ready took long enough for the screen to have moved on: a
    /// cold input session takes seconds to open, and a tap must never go to
    /// where something used to be.
    async fn locate_ready(&mut self, targets: &[Target]) -> Res<(Screen, Vec<(f64, f64, Option<Element>)>)> {
        let located = match self.locate_confirmed(targets).await {
            Some(l) => l,
            None => self.locate(targets, Fresh::MaxAge(reuse())).await?,
        };
        let started = std::time::Instant::now();
        self.ensure_input().await?;
        if started.elapsed() < Duration::from_millis(400) {
            return Ok(located);
        }
        self.locate(targets, Fresh::Force).await
    }

    pub async fn cmd_tap(&mut self, args: TapArgs) -> Res {
        if args.double && args.long_ms.is_some() {
            return Err(Fail::invalid("--double and --long cannot be combined"));
        }
        if let Some(ms) = args.long_ms
            && !(1..=60_000).contains(&ms)
        {
            return Err(Fail::invalid("--long must be 1..60000 ms"));
        }
        let target = parse_target(&args.target)?;
        let (screen, mut points) = self.locate_ready(std::slice::from_ref(&target)).await?;
        let (x, y, element) = points.remove(0);
        let element_json = element.as_ref().map(brief);

        let (nx, ny) = (normalize(x, screen.width), normalize(y, screen.height));
        let before = screen.signature();
        let input_began = Instant::now();
        let gesture = if args.double {
            Gesture::DoubleTap { x: nx, y: ny }
        } else {
            Gesture::Tap { x: nx, y: ny, hold: Duration::from_millis(args.long_ms.unwrap_or(50)) }
        };
        self.send_gesture(gesture).await?;
        let after = if args.no_wait {
            self.observe(input_began, AFTER_TAP).await
        } else {
            self.observe_settled(input_began, &before).await
        };
        let changed = after.as_ref().map(|(_, s)| s.signature() != before);
        let app_change = after.as_ref().and_then(|(_, s)| app_changed(&screen, s));
        if app_change.as_ref().is_some_and(|c| c["name"].is_null()) {
            // Something of the system's is over the app now.
            self.input_suspect = true;
        }

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
        // The screen it waited for, so that no reading has to follow.
        if !args.no_wait
            && let Some((seq, screen)) = after {
                out.insert("screen".into(), json!({"width": screen.width, "height": screen.height}));
                out.insert("apps".into(), json!(screen.apps));
                out.insert("count".into(), json!(screen.elements.len()));
                out.insert("seq".into(), json!(seq));
                out.insert("elements".into(), json!(screen.elements));
            }
        Ok(out)
    }

    /// Tap several targets in a row on the screen as it is now: every target
    /// is found before the first tap goes
    /// out, nothing is re-read in between, and the screen is read once at the
    /// end. For keypads and other screens that hold still while tapped.
    pub(crate) async fn tap_sequence(&mut self, targets: &[Value]) -> Res {
        if targets.is_empty() {
            return Err(Fail::invalid("the tap list is empty"));
        }
        let parsed: Vec<Target> = targets.iter().map(parse_target).collect::<Res<_>>()?;
        let (screen, points) = self.locate_ready(&parsed).await?;
        let before = screen.signature();
        let normalized: Vec<(u16, u16)> = points
            .iter()
            .map(|(x, y, _)| (normalize(*x, screen.width), normalize(*y, screen.height)))
            .collect();
        let tapped: Vec<Value> = points
            .iter()
            .map(|(_, _, el)| el.as_ref().map(brief).unwrap_or(Value::Null))
            .collect();
        let input_began = Instant::now();
        for (i, (nx, ny)) in normalized.iter().enumerate() {
            let tap = Gesture::Tap { x: *nx, y: *ny, hold: Duration::from_millis(50) };
            if let Err(mut fail) = self.send_gesture(tap).await {
                if i > 0 {
                    // The earlier taps of the list did go out.
                    fail.outcome = crate::reply::Outcome::Unknown;
                }
                return Err(fail
                    .hint(t(
                        "입력이 일부 나갔을 수 있습니다. 다시 보내지 말고 `iph read`로 화면을 확인하세요.",
                        "Part of the input may have gone out. Do not resend; check the screen with `iph read`.",
                    ))
                    .with("taps_sent", i));
            }
        }
        let after = self.observe_with(input_began, AFTER_TAP, true).await;
        let changed = after.as_ref().map(|(_, s)| s.signature() != before);
        let app_change = after.as_ref().and_then(|(_, s)| app_changed(&screen, s));
        let mut out = fields! {
            "sent": true,
            "verified": changed.is_some(),
            "screen_changed": changed.unwrap_or(false),
            "taps": tapped.len(),
            "elements": tapped,
        };
        if let Some(change) = app_change {
            out.insert("app_changed".into(), json!(true));
            out.insert("app".into(), change);
        }
        Ok(out)
    }

    /// Drag between two points given in logical points and report whether the
    /// screen changed.
    pub(crate) async fn drag_points(
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
        let before = screen.signature();
        let a = (normalize(from.0, screen.width), normalize(from.1, screen.height));
        let b = (normalize(to.0, screen.width), normalize(to.1, screen.height));
        let input_began = Instant::now();
        self.send_gesture(Gesture::Drag { from: a, to: b, duration, motion }).await?;
        let mut after = self.observe(input_began, AFTER_DRAG).await.map(|(_, s)| s);
        if motion == Motion::Flick {
            // The content coasts after a flick: read until two readings agree.
            for _ in 0..3 {
                let next = self.observe(Instant::now(), Duration::from_millis(150)).await.map(|(_, s)| s);
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
        let (changed, after) = self.drag_points(from, to, duration, motion).await?;
        Ok(fields! {
            "sent": true,
            "verified": after.is_some(),
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
            let (changed, after) = self.scroll_step(&args.direction).await?;
            return Ok(fields! {
                "sent": true,
                "verified": after.is_some(),
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
        let before = Some(screen.signature());
        let input_began = Instant::now();
        let input = self.ensure_input().await?;
        if let Err(e) = input.key(usage, &modifiers).await {
            self.drop_input();
            return Err(Fail::unknown("input_failed", format!("{e:#}")));
        }
        let changed = match before {
            Some(sig) => self.observe(input_began, AFTER_TAP).await.map(|(_, sc)| sc.signature() != sig),
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
        let before = Some(screen.signature());
        let input_began = Instant::now();
        self.send_gesture(Gesture::Button { page, usage, hold: Duration::from_millis(hold) }).await?;
        let changed = match before {
            Some(sig) => self.observe(input_began, AFTER_LAUNCH).await.map(|(_, sc)| sc.signature() != sig),
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
        let mut svc = self.app_service().await?;
        let launch_began = Instant::now();
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
            if let Some((_, screen)) = self.observe(launch_began, delay).await
                && screen.apps.iter().any(|a| a.pid == pid) {
                    foreground = true;
                    break;
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

    /// What a field shows after a paste, asked of the phone directly: again
    /// every 0.1 s until it differs from before (the paste has landed) or
    /// `PASTE_SHOWS` has passed. `None` when the phone does not answer, and
    /// the caller reads the whole screen instead.
    async fn field_value(&mut self, field: &Element) -> Option<String> {
        let key = field.key?;
        let before = field.value.clone().unwrap_or_default();
        let began = Instant::now();
        loop {
            let value = self.ax.as_mut()?.ask(key, attrs::VALUE, ASK_WAIT).await?;
            let now = crate::ax::tree::text(Some(&value)).unwrap_or_default();
            if now != before || began.elapsed() >= PASTE_SHOWS {
                return Some(now);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
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
        if !args.text.is_empty()
            && let Err(e) = Clipboard::set_text(&adapter, port, &args.text).await {
                let _ = input.keyboard_detach().await;
                self.invalidate_readings();
                return Err(Fail::not_sent("clipboard_failed", e));
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
        let keys_began = Instant::now();
        let pressed = input.keyboard_press(&combos).await;
        trace(|| "type: keys sent".into());
        if let Err(e) = pressed {
            let _ = input.keyboard_detach().await;
            self.drop_input();
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
        // the field is read. The temporary keyboard stays attached meanwhile:
        // the screen holds still and Return can follow at once. What the field
        // shows is reported, not judged: apps format, mask, correct and clear
        // what was pasted (₩ 30,000 for 30000), so only the caller can tell
        // whether it is right, as with Playwright's fill().
        let restoring = saved.restore(&adapter, port, &args.text);
        let reading = async {
            tokio::time::sleep(AFTER_PASTE).await;
            if let Some(value) = self.field_value(&field).await {
                return Some(value);
            }
            let (_, screen) = self.observe(keys_began, Duration::ZERO).await?;
            Self::focused_field(&screen)
                .or_else(|| screen.by_id(&field.id))
                .map(|e| e.value.clone().unwrap_or_default())
        };
        let (restored, value) = tokio::join!(restoring, reading);
        trace(|| format!("type: read back={} clipboard_restored={restored}", value.is_some()));

        let mut submitted = false;
        let mut submit_error = None;
        if args.submit
            && let Some(input) = self.input.as_mut() {
                match input.keyboard_press(&[(crate::keys::KEY_RETURN, vec![])]).await {
                    Ok(()) => submitted = true,
                    Err(e) => submit_error = Some(format!("{e:#}")),
                }
            }
        if let Some(input) = self.input.as_mut()
            && input.keyboard_detach().await.is_err() {
                self.drop_input();
            }
        // The on-screen keyboard comes back now; readings from before show
        // the screen without it.
        self.invalidate_readings();

        if let Some(e) = submit_error {
            self.drop_input();
            return Err(Fail::unknown("input_failed", e)
                .with("value", value.clone().map(Value::String).unwrap_or(Value::Null))
                .with("clipboard_restored", restored));
        }
        let mut out = fields! {
            "sent": true,
            "verified": value.is_some(),
            "value": value,
            "field": brief(&field),
            "submit_sent": submitted,
            "clipboard_restored": restored,
            "screen_changed": true,
        };
        if !restored && saved.unreadable() {
            out.insert(
                "clipboard_note".into(),
                json!(t(
                    "폰 클립보드에 앱에서 복사한 내용이 있었는데, 그 내용은 컴퓨터에서 읽을 수 없어(읽으면 폰 클립보드가 1분쯤 멈춤) 되돌리지 못했습니다. 입력한 글은 클립보드에서 지웠습니다.",
                    "The phone's clipboard held something copied in an app, which cannot be read from here (reading it stalls the phone's clipboard for about a minute), so it could not be put back. The typed text was cleared from the clipboard.",
                )),
            );
        }
        Ok(out)
    }
}

const PASTEBOARD_SERVICE: &str = "com.apple.coredevice.pasteboardservice";

/// Kinds of data the phone's clipboard service holds itself: what `iph type`
/// puts there, and plain text or a picture. An item that carries any other kind
/// (rich text, an app's own format) was copied in an app, and its bytes are
/// only promised: the service asks that app for them when someone reads them.
/// Read from here, that request never came back. Text copied in Notes stalled
/// past 8 s for every kind on offer (rtf and plain text alike), and the
/// service then answered nothing at all, writes included, for about a minute
/// (2026-11-11: `iph type` right after ⌘C in Notes failed twice with "set
/// clipboard: no answer" and lost the clipboard the third time). Such content
/// is not read.
const SERVICE_KINDS: [&str; 6] = [
    "public.utf8-plain-text",
    "public.plain-text",
    "public.text",
    "public.png",
    "public.jpeg",
    "public.tiff",
];
/// What the service's own items took to read: 75 ~ 124 ms.
const RESOLVE_WAIT: Duration = Duration::from_secs(2);

/// Whether an item with these kinds was copied in an app, so reading its
/// bytes would wait on that app.
fn app_provided(types: &[String]) -> bool {
    types.iter().any(|t| !SERVICE_KINDS.contains(&t.as_str()))
}

/// What the clipboard holds, as far as it can be read without waiting on an app.
#[derive(Debug, PartialEq)]
enum Peek {
    Empty,
    Text(String),
    Image(String, Vec<u8>),
    /// Copied in an app; left unread (see `SERVICE_KINDS`).
    AppContent,
    /// No answer, or something not understood.
    Unknown,
}

/// What the user had on the phone's clipboard before `iph type` borrowed it.
struct Clipboard {
    before: Peek,
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

    /// Look at the clipboard. Bytes come along only as promises, and only
    /// those the service holds itself are fetched.
    async fn peek(adapter: &idevice::tcp::handle::AdapterHandle, port: u16) -> Peek {
        use idevice::core_device::{DataInclusionPolicy, PasteboardPayload};
        let general = idevice::core_device::GENERAL_PASTEBOARD;
        let Ok(mut clip) = Self::connect(adapter, port).await else {
            return Peek::Unknown;
        };
        let pulled = tokio::time::timeout(
            Duration::from_secs(8),
            clip.get_with_policy(general, DataInclusionPolicy::AllPromised),
        )
        .await;
        let Ok(Ok(snap)) = pulled else {
            return Peek::Unknown;
        };
        trace(|| {
            let kinds: Vec<String> = snap.items.iter().map(|i| i.types.join(",")).collect();
            format!("clipboard: kinds [{}]", kinds.join(" | "))
        });
        let Some(item) = snap.items.first() else {
            return Peek::Empty;
        };
        if snap.items.iter().any(|i| app_provided(&i.types)) {
            return Peek::AppContent;
        }
        // Text before a picture, in the order of `SERVICE_KINDS`.
        for kind in SERVICE_KINDS {
            let Some(entry) = item.data.iter().find(|e| e.uti == kind) else {
                continue;
            };
            let bytes = match &entry.payload {
                PasteboardPayload::Inline(b) => Some(b.clone()),
                PasteboardPayload::Promised { .. } => {
                    match tokio::time::timeout(RESOLVE_WAIT, clip.resolve(general, item.index as i64, kind)).await {
                        Ok(Ok(b)) => b,
                        Ok(Err(_)) => None,
                        // Whatever holds it up may hold up the service too.
                        Err(_) => return Peek::Unknown,
                    }
                }
                PasteboardPayload::Error(_) => None,
            };
            let Some(bytes) = bytes else { continue };
            return if ["public.png", "public.jpeg", "public.tiff"].contains(&kind) {
                Peek::Image(kind.to_string(), bytes)
            } else {
                match String::from_utf8(bytes) {
                    Ok(text) => Peek::Text(text),
                    Err(_) => continue,
                }
            };
        }
        Peek::Unknown
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
        Clipboard { before: Self::peek(adapter, port).await }
    }

    /// True when what was on the clipboard could not be read and is lost
    /// once `iph type` puts its text there.
    fn unreadable(&self) -> bool {
        matches!(self.before, Peek::AppContent | Peek::Unknown)
    }

    /// Put the old content back. True when the clipboard is known to hold
    /// what it held before.
    async fn restore(&self, adapter: &idevice::tcp::handle::AdapterHandle, port: u16, typed: &str) -> bool {
        if typed.is_empty() {
            return true;
        }
        // Give the paste a moment to read the clipboard.
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Only touch the clipboard if it still holds what we put there; the
        // user may have copied something else meanwhile.
        if Self::peek(adapter, port).await != Peek::Text(typed.to_string()) {
            return false;
        }
        let general = idevice::core_device::GENERAL_PASTEBOARD;
        let wrote = match &self.before {
            Peek::Text(text) => Self::set_text(adapter, port, text).await.is_ok(),
            Peek::Image(uti, bytes) => match Self::connect(adapter, port).await {
                Ok(mut clip) => {
                    matches!(
                        tokio::time::timeout(Duration::from_secs(8), clip.set_image(bytes, uti, general)).await,
                        Ok(Ok(()))
                    )
                }
                Err(_) => false,
            },
            // The clipboard was empty, or what was there is gone: either way
            // the typed text does not stay behind.
            Peek::Empty | Peek::AppContent | Peek::Unknown => {
                let _ = Self::set_text(adapter, port, "").await;
                return matches!(self.before, Peek::Empty);
            }
        };
        if !wrote {
            return false;
        }
        match (Self::peek(adapter, port).await, &self.before) {
            (Peek::Text(now), Peek::Text(old)) => &now == old,
            (Peek::Image(..), Peek::Image(..)) => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod clipboard_tests {
    use super::app_provided;

    fn kinds(k: &[&str]) -> Vec<String> {
        k.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn notes_copy_is_app_content() {
        // What ⌘C in Notes put on the clipboard (2026-11-11).
        assert!(app_provided(&kinds(&[
            "public.utf8-plain-text",
            "com.apple.notes.richtext",
            "com.apple.webarchive",
            "com.apple.flat-rtfd",
            "iOS rich content paste pasteboard type",
            "public.html",
            "net.daringfireball.markdown",
            "public.rtf",
        ])));
    }

    #[test]
    fn what_iph_puts_there_is_not() {
        // `set_text` writes these three kinds.
        assert!(!app_provided(&kinds(&["public.utf8-plain-text", "public.plain-text", "public.text"])));
        assert!(!app_provided(&kinds(&["public.png"])));
    }
}
