//! The commands themselves, shared by the CLI, the MCP server, `batch` and
//! scripts. Each takes JSON arguments and returns the fields of its result.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

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
        display::{Canvas, Space},
        input::Motion,
        pinch,
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
/// The result's note for an input sent while the screen could not be read.
const UNREAD_NOTE: &str =
    "the screen could not be read; sent on the last known screen size, not verified";

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
pub struct PinchArgs {
    /// `in` (scale 0.5) or `out` (scale 2); not with `scale`.
    pub direction: Option<String>,
    /// End span over start span, 0.25..4.
    pub scale: Option<f64>,
    /// Degrees, positive clockwise, -180..180.
    pub rotate: Option<f64>,
    /// Where the fingers meet: a selector, `"x,y"`, or `{x, y}`. Default: the
    /// middle of the screen.
    pub at: Value,
    pub duration_ms: Option<u64>,
}

impl PinchArgs {
    /// The `(scale, rotate)` these arguments ask for.
    fn motion(&self) -> Res<(f64, f64)> {
        let scale = match (self.direction.as_deref(), self.scale) {
            (Some(_), Some(_)) => return Err(Fail::invalid("a direction and a scale cannot be combined")),
            (Some("in"), None) => 0.5,
            (Some("out"), None) => 2.0,
            (Some(other), None) => {
                return Err(Fail::invalid(format!("unknown direction `{other}` (in, out)")));
            }
            (None, scale) => scale.unwrap_or(1.0),
        };
        let rotate = self.rotate.unwrap_or(0.0);
        pinch::check_motion(scale, rotate).map_err(Fail::invalid)?;
        Ok((scale, rotate))
    }
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

/// Observations before Return. Reading a value is distinct from seeing it
/// change, and neither proves that it equals the caller's intended text.
struct TypeObservation {
    before: Option<String>,
    value: Option<String>,
    changed: Option<bool>,
    warning: Option<Value>,
}

impl TypeObservation {
    fn new(args: &TypeArgs, before: Option<String>, value: Option<String>) -> Self {
        let changed = before.as_ref().zip(value.as_ref()).map(|(a, b)| a != b);
        let retry_hint = t(
            "입력이 반영되었을 수 있습니다. 자동으로 재입력하지 말고 `iph read`나 스크린샷으로 확인하세요.",
            "Input may have taken effect. Do not automatically retype; check with `iph read` or a screenshot.",
        );
        let warning = if value.is_none() {
            Some(json!({
                "code": "value_unreadable",
                "message": "the input was sent but the field's value could not be read afterwards",
                "hint": retry_hint,
            }))
        } else if args.text.is_empty() && value.as_deref() == Some("") {
            // Clearing an already empty field is an observable no-op.
            None
        } else if args.text.is_empty() {
            Some(json!({
                "code": "clear_not_observed",
                "message": "the clear input was sent but the field still shows a nonempty value",
                "hint": retry_hint,
            }))
        } else if changed == Some(false) {
            Some(json!({
                "code": "value_unchanged",
                "message": "the field's value is unchanged; the paste may not have taken effect (identical replacement, masking or app behavior can also cause this)",
                "hint": t(
                    "자동으로 재입력하지 말고 `iph read`나 스크린샷으로 확인하세요. 붙여넣기가 막혔을 수도 있습니다: 설정 → 앱 → 해당 앱 → 다른 앱에서 붙여넣기 → 허용. 값이 같다는 사실만으로 권한 거부를 확정할 수는 없습니다.",
                    "Do not automatically retype; check with `iph read` or a screenshot. Pasting may be blocked: Settings → Apps → this app → Paste from Other Apps → Allow. An unchanged value alone does not prove permission was denied.",
                ),
            }))
        } else if value.as_deref() == Some("") {
            Some(json!({
                "code": "value_empty",
                "message": "the input was sent but the field shows an empty value; the app may have cleared it",
                "hint": retry_hint,
            }))
        } else if changed.is_none() {
            Some(json!({
                "code": "value_change_unknown",
                "message": "the field's value was read afterwards, but its previous value was unavailable for comparison",
                "hint": retry_hint,
            }))
        } else {
            None
        };
        Self {
            before,
            value,
            changed,
            warning,
        }
    }

    fn can_submit(&self) -> bool {
        self.warning.is_none()
    }

    fn result(
        self,
        args: &TypeArgs,
        field: &Element,
        restored: bool,
        submitted: Option<bool>,
        screen_changed: Option<bool>,
        submit_error: Option<String>,
    ) -> Res {
        let blocked_submit = args.submit && !self.can_submit();
        let mut out = fields! {
            "sent": true,
            "verified": self.value.is_some(),
            "value_before": self.before,
            "value": self.value,
            "value_changed": self.changed,
            "field": brief(field),
            "submit_sent": submitted,
            "clipboard_restored": restored,
            "screen_changed": screen_changed,
        };
        if let Some(warning) = self.warning {
            out.insert("warning".into(), warning);
        }
        let fail = if let Some(e) = submit_error {
            Some(Fail::unknown("input_failed", e))
        } else if blocked_submit {
            Some(Fail::unknown(
                "type_not_verified",
                "the input was sent, but its effect is uncertain; Return was not sent",
            ))
        } else {
            None
        };
        if let Some(mut fail) = fail {
            fail.hint = Some(t(
                "입력은 이미 나갔습니다. 자동으로 재입력하거나 제출하지 말고 `iph read`나 스크린샷으로 먼저 확인하세요.",
                "Input has already been sent. Do not automatically retype or submit; check with `iph read` or a screenshot first.",
            ).into());
            fail.extra = out;
            Err(fail)
        } else {
            Ok(out)
        }
    }
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
    /// Passed to the app's process, as after `--` in `devicectl device process launch`.
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
}

impl LaunchArgs {
    pub fn validate(&self) -> Res<()> {
        for key in self.environment.keys() {
            if key.is_empty() || key.contains('=') {
                return Err(Fail::invalid(format!(
                    "environment names must be non-empty and contain no `=`: \"{key}\""
                )));
            }
        }
        Ok(())
    }

    /// Arguments and environment only reach a process that starts now.
    pub fn has_launch_options(&self) -> bool {
        !self.arguments.is_empty() || !self.environment.is_empty()
    }

    pub fn kill_existing(&self) -> bool {
        self.restart || self.has_launch_options()
    }

    fn environment_dictionary(&self) -> Option<plist::Dictionary> {
        (!self.environment.is_empty()).then(|| {
            self.environment
                .iter()
                .map(|(k, v)| (k.clone(), plist::Value::String(v.clone())))
                .collect()
        })
    }
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

/// Where to touch for one target: `at`, a point on `on`, and the element there.
struct Spot {
    at: (f64, f64),
    on: Canvas,
    element: Option<Element>,
}

/// The reading failed because the phone's accessibility daemon gave none on a
/// phone not known to be locked (`Session::unread_size`).
fn daemon_silent(read_failed: &Fail) -> bool {
    let locked = read_failed.extra.get("phone_locked").and_then(Value::as_bool) == Some(true);
    read_failed.code == "ax_timeout" && !locked
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

/// A point inside the visible part of `el` that no other element covers, in
/// the space of `el`'s frame.
fn tap_point(el: &Element, screen: &Screen) -> Res<(f64, f64)> {
    let frame = el.frame.ok_or_else(|| {
        Fail::not_sent("no_frame", "the element has no position on screen").with("element", brief(el))
    })?;
    let space = screen.space_of(el);
    let (w, h) = screen.size(space);
    let bounds = Rect { x: 0.0, y: 0.0, w, h };
    let visible = frame.intersection(&bounds).ok_or_else(|| {
        Fail::not_sent("off_screen", "the element is outside the screen")
            .hint(t(
                "`iph scroll down --until …`으로 화면 안으로 가져오세요.",
                "Bring it on screen with `iph scroll down --until …`.",
            ))
            .with("element", brief(el))
    })?;
    // Frames of the system's surfaces and of apps compare on the panel.
    let frame = screen.panel_frame(el).unwrap_or(frame);
    let covered = |x: f64, y: f64| -> Option<&Element> {
        let hit = screen.element_in(space, x, y)?;
        if hit.id == el.id {
            return None;
        }
        // A child of the target (a label inside a row) is fine; an element
        // that merely overlaps it (a toolbar over a list) is not.
        let hit_frame = screen.panel_frame(hit);
        let inside = hit_frame.is_some_and(|h| {
            h.x >= frame.x - 1.0
                && h.y >= frame.y - 1.0
                && h.x + h.w <= frame.x + frame.w + 1.0
                && h.y + h.h <= frame.y + frame.h + 1.0
        });
        let encloses = hit_frame.is_some_and(|h| {
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

/// Something with a dot and no spaces is a bundle id; anything else is a name.
fn is_bundle_id(app: &str) -> bool {
    app.contains('.') && !app.contains(' ')
}

/// The bundle id `app` names, looked up by name in `apps` unless it is one already.
pub(crate) fn bundle_id_in(apps: &[idevice::core_device::AppListEntry], app: &str) -> Res<String> {
    if is_bundle_id(app) {
        return Ok(app.to_string());
    }
    let want = nfc(app).to_lowercase();
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
    async fn locate(&mut self, targets: &[Target], fresh: Fresh) -> Res<(Screen, Vec<Spot>)> {
        let need_ids = targets
            .iter()
            .any(|t| matches!(t, Target::Selector(s) if s.needs_identifier()));
        let (_, screen) = self.screen(fresh, need_ids, BuildOptions::default()).await?;
        if screen.width <= 0.0 || screen.height <= 0.0 {
            return Err(Fail::not_sent("no_screen_size", "the reading has no application frame"));
        }
        let mut spots = Vec::with_capacity(targets.len());
        for target in targets {
            spots.push(match target {
                Target::Selector(sel) => {
                    let el = resolve_one(sel, &screen)?;
                    let at = tap_point(el, &screen)?;
                    Spot { at, on: screen.canvas(screen.space_of(el)), element: Some(el.clone()) }
                }
                Target::Point(x, y) => {
                    let on = screen.canvas(screen.point_space());
                    let (w, h) = on.size;
                    if *x < 0.0 || *y < 0.0 || *x > w || *y > h {
                        return Err(Fail::invalid(format!("point {x},{y} is outside the {w}x{h} screen")));
                    }
                    Spot { at: (*x, *y), on, element: screen.element_at(*x, *y).cloned() }
                }
            });
        }
        Ok((screen, spots))
    }

    /// Locate the targets on the latest reading when it is too old to reuse
    /// as it is (an agent's turn takes seconds) but the phone confirms it:
    /// every application still shows the same visible children (nothing came
    /// up or went away) and every target is still where it was. Each question
    /// takes tens of milliseconds; a full reading takes over a second. `None`
    /// sends the caller to a full reading.
    async fn locate_confirmed(&mut self, targets: &[Target]) -> Option<(Screen, Vec<Spot>)> {
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
        for spot in &located.1 {
            let el = spot.element.as_ref()?;
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
    ///
    /// On a phone iph has not touched before, input comes first: the reading
    /// depends on whether the phone has more than one display, and an input
    /// session is what tells.
    async fn locate_ready(&mut self, targets: &[Target]) -> Res<(Screen, Vec<Spot>)> {
        if self.displays_unknown() {
            self.ensure_input().await?;
        }
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
        let (screen, mut spots) = match self.locate_ready(std::slice::from_ref(&target)).await {
            Ok(located) => located,
            Err(read_failed) => return self.tap_unread(&target, &args, read_failed).await,
        };
        let Spot { at, on, element } = spots.remove(0);
        let (x, y) = at;
        let element_json = element.as_ref().map(brief);

        let before = screen.signature();
        let input_began = Instant::now();
        let gesture = if args.double {
            Gesture::DoubleTap { on, at }
        } else {
            Gesture::Tap { on, at, hold: Duration::from_millis(args.long_ms.unwrap_or(50)) }
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

    /// The last known screen size, when `read_failed` is the phone's
    /// accessibility daemon giving no reading on an unlocked phone. Control
    /// Center does that while a screen recording runs, until it is closed
    /// (2026-11-12, over Wi-Fi and USB alike), and closing it takes a tap, a
    /// swipe or a button: input that needs no element then goes out on this
    /// size, unverified.
    fn unread_size(&self, read_failed: &Fail) -> Option<(f64, f64)> {
        if !daemon_silent(read_failed) {
            return None;
        }
        self.screen_size
    }

    /// A tap on a point while the screen cannot be read (`unread_size`).
    /// Nothing is read afterwards either: that would only wait out another
    /// timeout.
    async fn tap_unread(&mut self, target: &Target, args: &TapArgs, read_failed: Fail) -> Res {
        let (Target::Point(x, y), Some((w, h))) = (target, self.unread_size(&read_failed)) else {
            return Err(read_failed);
        };
        let (x, y) = (*x, *y);
        if x < 0.0 || y < 0.0 || x > w || y > h {
            return Err(Fail::invalid(format!("point {x},{y} is outside the {w}x{h} screen")));
        }
        let (on, at) = (Canvas { space: Space::Screen, size: (w, h) }, (x, y));
        let gesture = if args.double {
            Gesture::DoubleTap { on, at }
        } else {
            Gesture::Tap { on, at, hold: Duration::from_millis(args.long_ms.unwrap_or(50)) }
        };
        self.send_gesture(gesture).await?;
        Ok(fields! {
            "sent": true,
            "verified": false,
            "screen_changed": false,
            "x": (x * 10.0).round() / 10.0,
            "y": (y * 10.0).round() / 10.0,
            "note": UNREAD_NOTE,
        })
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
        let (screen, spots) = self.locate_ready(&parsed).await?;
        let before = screen.signature();
        let tapped: Vec<Value> = spots
            .iter()
            .map(|spot| spot.element.as_ref().map(brief).unwrap_or(Value::Null))
            .collect();
        let input_began = Instant::now();
        for (i, spot) in spots.iter().enumerate() {
            let tap = Gesture::Tap { on: spot.on, at: spot.at, hold: Duration::from_millis(50) };
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

    /// Drag between two points of `space` and report whether the screen
    /// changed. `None`: points given by hand, in the space of the app in
    /// front.
    pub(crate) async fn drag_points(
        &mut self,
        from: (f64, f64),
        to: (f64, f64),
        space: Option<Space>,
        duration: Duration,
        motion: Motion,
    ) -> Res<(bool, Option<Screen>)> {
        let (_, screen) = self
            .screen(Fresh::MaxAge(reuse()), false, BuildOptions::default())
            .await?;
        if screen.width <= 0.0 || screen.height <= 0.0 {
            return Err(Fail::not_sent("no_screen_size", "the reading has no application frame"));
        }
        let on = screen.canvas(space.unwrap_or_else(|| screen.point_space()));
        let (w, h) = on.size;
        for (x, y) in [from, to] {
            if x < 0.0 || y < 0.0 || x > w || y > h {
                return Err(Fail::invalid(format!("point {x},{y} is outside the {w}x{h} screen")));
            }
        }
        let before = screen.signature();
        let input_began = Instant::now();
        self.send_gesture(Gesture::Drag { on, from, to, duration, motion }).await?;
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
        let read = self
            .screen(Fresh::MaxAge(reuse()), false, BuildOptions::default())
            .await;
        let (w, h) = match &read {
            Ok((_, screen)) => (screen.width, screen.height),
            Err(read_failed) => self.unread_size(read_failed).ok_or_else(|| read_failed.clone())?,
        };
        // A direction is laid out on the screen as turned, whatever is in front.
        let space = args.direction.as_ref().map(|_| Space::Screen);
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
        if read.is_err() {
            // `unread_size`: drag on the last known size, read nothing after.
            for (x, y) in [from, to] {
                if x < 0.0 || y < 0.0 || x > w || y > h {
                    return Err(Fail::invalid(format!("point {x},{y} is outside the {w}x{h} screen")));
                }
            }
            let on = Canvas { space: Space::Screen, size: (w, h) };
            self.send_gesture(Gesture::Drag { on, from, to, duration, motion }).await?;
            return Ok(fields! {
                "sent": true,
                "verified": false,
                "screen_changed": false,
                "from": [from.0, from.1],
                "to": [to.0, to.1],
                "note": UNREAD_NOTE,
            });
        }
        let (changed, after) = self.drag_points(from, to, space, duration, motion).await?;
        Ok(fields! {
            "sent": true,
            "verified": after.is_some(),
            "screen_changed": changed,
            "from": [from.0, from.1],
            "to": [to.0, to.1],
        })
    }

    /// Two fingers pinching, spreading or turning around a point. Arguments
    /// that cannot make a gesture, or a point with no room for one, fail
    /// before anything is sent. `screen_changed: false` is a fair answer on a
    /// screen that does not zoom.
    pub async fn cmd_pinch(&mut self, args: PinchArgs) -> Res {
        let (scale, rotate) = args.motion()?;
        let duration = Duration::from_millis(args.duration_ms.unwrap_or(500).clamp(100, 10_000));
        let target = (!args.at.is_null()).then(|| parse_target(&args.at)).transpose()?;
        let read = match &target {
            Some(target) => self.locate_ready(std::slice::from_ref(target)).await.map(|(screen, mut spots)| {
                let spot = spots.remove(0);
                (screen, Some((spot.at, spot.on)))
            }),
            None => self
                .screen(Fresh::MaxAge(reuse()), false, BuildOptions::default())
                .await
                .map(|(_, screen)| (screen, None)),
        };
        let (on, at, before) = match &read {
            Ok((screen, _)) if screen.width <= 0.0 || screen.height <= 0.0 => {
                return Err(Fail::not_sent("no_screen_size", "the reading has no application frame"));
            }
            Ok((screen, Some((at, on)))) => (*on, Some(*at), Some(screen.signature())),
            Ok((screen, None)) => (screen.canvas(Space::Screen), None, Some(screen.signature())),
            Err(read_failed) => {
                // `unread_size`: only the middle or a point can be pinched without a reading.
                let size = self.unread_size(read_failed).ok_or_else(|| read_failed.clone())?;
                let at = match &target {
                    None => None,
                    Some(Target::Point(x, y)) => Some((*x, *y)),
                    Some(Target::Selector(_)) => return Err(read_failed.clone()),
                };
                (Canvas { space: Space::Screen, size }, at, None)
            }
        };
        let size = on.size;
        let center = at.unwrap_or((size.0 / 2.0, size.1 / 2.0));
        let plan = pinch::plan(&pinch::Spec { screen: size, center, scale, rotate, duration }).map_err(Fail::invalid)?;
        let span = [(plan.span.0 * 10.0).round() / 10.0, (plan.span.1 * 10.0).round() / 10.0];
        let input_began = Instant::now();
        self.send_gesture(Gesture::Pinch { on, plan }).await?;
        let mut out = fields! {
            "sent": true,
            "center": [(center.0 * 10.0).round() / 10.0, (center.1 * 10.0).round() / 10.0],
            "scale": scale,
            "rotate": rotate,
            "span": span,
        };
        let Some(before) = before else {
            out.insert("verified".into(), json!(false));
            out.insert("screen_changed".into(), json!(false));
            out.insert("note".into(), json!(UNREAD_NOTE));
            return Ok(out);
        };
        let after = self.observe(input_began, AFTER_DRAG).await.map(|(_, screen)| screen);
        out.insert("verified".into(), json!(after.is_some()));
        out.insert("screen_changed".into(), json!(after.is_some_and(|s| s.signature() != before)));
        Ok(out)
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
        self.drag_points(from, to, Some(Space::Screen), Duration::from_millis(500), Motion::Place)
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
        // A button needs no screen size; the reading is only to compare with.
        let before = match self
            .screen(Fresh::MaxAge(reuse()), false, BuildOptions::default())
            .await
        {
            Ok((_, screen)) => Some(screen.signature()),
            Err(read_failed) if daemon_silent(&read_failed) => None,
            Err(read_failed) => return Err(read_failed),
        };
        let input_began = Instant::now();
        self.send_gesture(Gesture::Button { page, usage, hold: Duration::from_millis(hold) }).await?;
        let changed = match &before {
            Some(sig) => self.observe(input_began, AFTER_LAUNCH).await.map(|(_, sc)| sc.signature() != *sig),
            None => None,
        };
        let mut out = fields! {
            "sent": true,
            "verified": changed.is_some(),
            "screen_changed": changed.unwrap_or(false),
            "button": args.button,
        };
        if before.is_none() {
            out.insert("note".into(), json!(UNREAD_NOTE));
        }
        Ok(out)
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

    pub(crate) async fn resolve_bundle(&mut self, app: &str) -> Res<String> {
        let apps = if is_bundle_id(app) { Vec::new() } else { self.list_apps(false).await? };
        bundle_id_in(&apps, app)
    }

    pub async fn cmd_launch(&mut self, args: LaunchArgs) -> Res {
        args.validate()?;
        let bundle = self.resolve_bundle(&args.app).await?;
        let mut svc = self.app_service().await?;
        let arguments: Vec<&str> = args.arguments.iter().map(String::as_str).collect();
        let launch_began = Instant::now();
        // From here the launch request may have reached the phone.
        let launched = tokio::time::timeout(
            Duration::from_secs(30),
            svc.launch_application(
                bundle.as_str(),
                &arguments,
                args.kill_existing(),
                false,
                args.environment_dictionary(),
                None,
                None,
            ),
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
        let mut out = fields! {
            "sent": true,
            "verified": foreground,
            "foreground": foreground,
            "bundle_id": bundle,
            "pid": pid,
            "restart": args.kill_existing(),
        };
        if args.has_launch_options() {
            out.insert("restart_implied".into(), json!(!args.restart));
            out.insert("arguments".into(), json!(args.arguments));
            out.insert("environment_keys".into(), json!(args.environment.keys().collect::<Vec<_>>()));
        }
        Ok(out)
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
    /// every 0.1 s until it differs from before or
    /// `PASTE_SHOWS` has passed. `None` when the phone does not answer, and
    /// the caller reads the whole screen instead.
    async fn field_value(&mut self, field: &Element) -> Option<String> {
        let key = field.key?;
        let before = field.value.as_deref();
        let began = Instant::now();
        loop {
            let value = self.ax.as_mut()?.ask(key, attrs::VALUE, ASK_WAIT).await?;
            let now = crate::ax::tree::value_text(Some(&value))?;
            if Some(now.as_str()) != before || began.elapsed() >= PASTE_SHOWS {
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
        let (_, screen) = self
            .screen(Fresh::Force, false, BuildOptions::default())
            .await?;
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

        // The paste keys have gone out, so the clipboard can go back while
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
            // Focus may have moved to a permission dialog or another field.
            // Only the original field can tell us what this paste changed.
            screen.by_id(&field.id).and_then(|e| e.value.clone())
        };
        let (restored, value) = tokio::join!(restoring, reading);
        trace(|| format!("type: read back={} clipboard_restored={restored}", value.is_some()));
        let observation = TypeObservation::new(&args, field.value.clone(), value);

        let mut submitted = Some(false);
        let mut submit_error = None;
        if args.submit && observation.can_submit() {
            if let Some(input) = self.input.as_mut() {
                match input.keyboard_press(&[(crate::keys::KEY_RETURN, vec![])]).await {
                    Ok(()) => submitted = Some(true),
                    Err(e) => {
                        submitted = None;
                        submit_error = Some(format!("{e:#}"));
                    }
                }
            } else {
                submit_error = Some("the input session closed before Return could be sent".into());
            }
        }
        if let Some(input) = self.input.as_mut()
            && input.keyboard_detach().await.is_err() {
                self.drop_input();
            }
        // The on-screen keyboard comes back now; readings from before show
        // the screen without it.
        self.invalidate_readings();

        if submit_error.is_some() {
            self.drop_input();
        }
        // Compare complete readings after the temporary keyboard is gone.
        // A field read alone cannot tell whether the rest of the screen moved.
        let screen_changed = self
            .observe(keys_began, AFTER_PASTE)
            .await
            .map(|(_, after)| after.signature() != screen.signature());
        let mut result = observation.result(
            &args,
            &field,
            restored,
            submitted,
            screen_changed,
            submit_error,
        );
        if !restored && saved.unreadable() {
            let out = match &mut result {
                Ok(out) => out,
                Err(fail) => &mut fail.extra,
            };
            out.insert(
                "clipboard_note".into(),
                json!(t(
                    "폰 클립보드에 앱에서 복사한 내용이 있었는데, 그 내용은 컴퓨터에서 읽을 수 없어(읽으면 폰 클립보드가 1분쯤 멈춤) 되돌리지 못했습니다. 입력한 글은 클립보드에서 지웠습니다.",
                    "The phone's clipboard held something copied in an app, which cannot be read from here (reading it stalls the phone's clipboard for about a minute), so it could not be put back. The typed text was cleared from the clipboard.",
                )),
            );
        }
        result
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
        adapter: &crate::tcp::handle::AdapterHandle,
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
    async fn peek(adapter: &crate::tcp::handle::AdapterHandle, port: u16) -> Peek {
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

    async fn set_text(adapter: &crate::tcp::handle::AdapterHandle, port: u16, text: &str) -> Result<(), String> {
        let mut clip = Self::connect(adapter, port).await?;
        let general = idevice::core_device::GENERAL_PASTEBOARD;
        match tokio::time::timeout(Duration::from_secs(8), clip.set_text(text, general)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("set clipboard: {e:?}")),
            Err(_) => Err("set clipboard: no answer".into()),
        }
    }

    async fn save(adapter: &crate::tcp::handle::AdapterHandle, port: u16) -> Clipboard {
        Clipboard { before: Self::peek(adapter, port).await }
    }

    /// True when what was on the clipboard could not be read and is lost
    /// once `iph type` puts its text there.
    fn unreadable(&self) -> bool {
        matches!(self.before, Peek::AppContent | Peek::Unknown)
    }

    /// Put the old content back. True when the clipboard is known to hold
    /// what it held before.
    async fn restore(&self, adapter: &crate::tcp::handle::AdapterHandle, port: u16, typed: &str) -> bool {
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
mod type_tests {
    use super::*;
    use crate::reply::envelope;

    fn field(before: Option<&str>) -> Element {
        serde_json::from_value(json!({
            "id": "original", "role": "text_field", "value": before, "focused": true,
        }))
        .unwrap()
    }

    fn result(
        args: &TypeArgs,
        before: Option<&str>,
        after: Option<&str>,
        screen: Option<bool>,
    ) -> Value {
        let field = field(before);
        let observed = TypeObservation::new(args, field.value.clone(), after.map(String::from));
        let submit = args.submit && observed.can_submit();
        envelope(
            observed.result(args, &field, true, Some(submit), screen, None),
            Instant::now(),
        )
    }

    #[test]
    fn changed_values_are_observations_not_exact_text_matches() {
        for value in ["30000", "₩ 30,000", "•••••", "corrected text", " \n "] {
            let args = TypeArgs {
                text: "30000".into(),
                submit: true,
                ..Default::default()
            };
            let out = result(&args, Some(""), Some(value), Some(true));
            assert_eq!(out["ok"], true, "{out}");
            assert_eq!(out["outcome"], "done");
            assert_eq!(out["verified"], true);
            assert_eq!(out["value_before"], "");
            assert_eq!(out["value"], value);
            assert_eq!(out["value_changed"], true);
            assert_eq!(out["screen_changed"], true);
            assert_eq!(out["submit_sent"], true);
            assert!(out.get("warning").is_none());
        }
    }

    #[test]
    fn unchanged_paste_warns_without_claiming_permission_was_denied() {
        let args = TypeArgs {
            text: "테스트 문장".into(),
            ..Default::default()
        };
        for value in ["", "old text", "•••••"] {
            let out = result(&args, Some(value), Some(value), Some(false));
            assert_eq!(out["ok"], true);
            assert_eq!(out["outcome"], "done");
            assert_eq!(out["sent"], true);
            assert_eq!(out["verified"], true, "verified still means read back");
            assert_eq!(out["value_changed"], false);
            assert_eq!(out["screen_changed"], false);
            assert_eq!(out["warning"]["code"], "value_unchanged");
            assert!(
                out["warning"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("may not")
            );
            assert!(out["warning"]["hint"].as_str().is_some());
        }
        // A dialog or another part of the screen may change while the field does not.
        let out = result(&args, Some(""), Some(""), Some(true));
        assert_eq!(out["value_changed"], false);
        assert_eq!(out["screen_changed"], true);
        assert!(out.get("warning").is_some());
    }

    #[test]
    fn unreadable_and_uncomparable_values_do_not_become_empty_strings() {
        let args = TypeArgs {
            text: "hello".into(),
            ..Default::default()
        };
        let out = result(&args, Some("old"), None, None);
        assert_eq!(out["ok"], true);
        assert_eq!(out["verified"], false);
        assert!(out["value"].is_null());
        assert!(out["value_changed"].is_null());
        assert!(out["screen_changed"].is_null());
        assert_eq!(out["warning"]["code"], "value_unreadable");
        let out = result(&args, None, Some("hello"), Some(true));
        assert_eq!(out["verified"], true);
        assert!(out["value_before"].is_null());
        assert!(out["value_changed"].is_null());
        assert_eq!(out["warning"]["code"], "value_change_unknown");
        // A readable field does not imply a complete screen comparison succeeded.
        let out = result(&args, Some(""), Some("hello"), None);
        assert_eq!(out["verified"], true);
        assert!(out["screen_changed"].is_null());
    }

    #[test]
    fn submit_is_withheld_when_the_input_effect_needs_review() {
        let args = TypeArgs {
            text: "hello".into(),
            submit: true,
            ..Default::default()
        };
        for (before, after, code) in [
            (Some(""), Some(""), "value_unchanged"),
            (Some("old"), Some("old"), "value_unchanged"),
            (Some("old"), None, "value_unreadable"),
            (None, Some("hello"), "value_change_unknown"),
            (Some("old"), Some(""), "value_empty"),
        ] {
            let out = result(&args, before, after, Some(false));
            assert_eq!(out["ok"], false, "{out}");
            assert_eq!(out["outcome"], "unknown");
            assert_eq!(out["sent"], true);
            assert_eq!(out["submit_sent"], false);
            assert_eq!(out["error"]["code"], "type_not_verified");
            assert_eq!(out["warning"]["code"], code);
            assert_eq!(out["value"], json!(after));
            assert_eq!(out["clipboard_restored"], true);
        }
    }

    #[test]
    fn clear_and_identical_replacement_are_reported_separately() {
        let mut args = TypeArgs {
            clear: true,
            submit: true,
            ..Default::default()
        };
        for before in [Some(""), Some("old"), None] {
            let out = result(&args, before, Some(""), Some(false));
            assert_eq!(out["ok"], true);
            assert_eq!(out["submit_sent"], true);
            assert!(out.get("warning").is_none());
        }
        let out = result(&args, Some("old"), Some("old"), Some(false));
        assert_eq!(out["warning"]["code"], "clear_not_observed");
        assert_eq!(out["submit_sent"], false);
        // Replacing with identical text can be intentional, but it does not
        // establish whether paste worked. The caller must inspect before Return.
        args.text = "same".into();
        let out = result(&args, Some("same"), Some("same"), Some(false));
        assert_eq!(out["outcome"], "unknown");
        assert_eq!(out["warning"]["code"], "value_unchanged");
    }

    #[test]
    fn failed_return_keeps_observations_and_does_not_claim_it_was_unsent() {
        let args = TypeArgs {
            text: "hello".into(),
            submit: true,
            ..Default::default()
        };
        let field = field(Some(""));
        let observed = TypeObservation::new(&args, field.value.clone(), Some("hello".into()));
        let out = envelope(
            observed.result(
                &args,
                &field,
                false,
                None,
                None,
                Some("write failed".into()),
            ),
            Instant::now(),
        );
        assert_eq!(out["outcome"], "unknown");
        assert_eq!(out["error"]["code"], "input_failed");
        assert!(out["submit_sent"].is_null());
        assert_eq!(out["value_changed"], true);
        assert_eq!(out["clipboard_restored"], false);
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

#[cfg(test)]
mod launch_tests {
    use super::LaunchArgs;
    use crate::actions::parse_args;
    use serde_json::json;

    fn parse(v: serde_json::Value) -> Result<LaunchArgs, crate::reply::Fail> {
        parse_args::<LaunchArgs>(&v)
    }

    #[test]
    fn a_plain_launch_sends_nothing_extra() {
        let a = parse(json!({"app": "x"})).unwrap();
        assert!(!a.kill_existing());
        assert!(a.environment_dictionary().is_none());
    }

    #[test]
    fn arguments_or_environment_imply_a_restart() {
        let a = parse(json!({"app": "x", "arguments": ["-k", "v"]})).unwrap();
        assert!(a.kill_existing() && !a.restart);
        let b = parse(json!({"app": "x", "environment": {"TZ": "UTC"}})).unwrap();
        assert!(b.kill_existing());
        let dict = b.environment_dictionary().unwrap();
        assert_eq!(dict.get("TZ").and_then(|v| v.as_string()), Some("UTC"));
    }

    #[test]
    fn environment_values_may_hold_an_equals_sign() {
        let a = parse(json!({"app": "x", "environment": {"K": "a=b"}})).unwrap();
        assert!(a.validate().is_ok());
    }

    #[test]
    fn environment_names_are_checked() {
        for name in ["", "A=B"] {
            let a = parse(json!({"app": "x", "environment": {name: "v"}})).unwrap();
            assert_eq!(a.validate().unwrap_err().code, "invalid_argument");
        }
    }

    #[test]
    fn a_non_string_value_is_rejected_at_the_boundary() {
        assert_eq!(parse(json!({"app": "x", "environment": {"A": 1}})).unwrap_err().code, "invalid_argument");
        assert_eq!(parse(json!({"app": "x", "environment": ["A=1"]})).unwrap_err().code, "invalid_argument");
        assert_eq!(parse(json!({"app": "x", "arguments": "-a"})).unwrap_err().code, "invalid_argument");
        assert_eq!(parse(json!({"app": "x", "arguments": [1]})).unwrap_err().code, "invalid_argument");
    }
}

#[cfg(test)]
mod pinch_tests {
    use super::*;
    use crate::{config::Config, reply::Outcome};

    fn args(v: Value) -> PinchArgs {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_direction_means_a_scale() {
        assert_eq!(args(json!({"direction": "in"})).motion().unwrap(), (0.5, 0.0));
        assert_eq!(args(json!({"direction": "out"})).motion().unwrap(), (2.0, 0.0));
        assert_eq!(args(json!({"direction": "out", "rotate": -30})).motion().unwrap(), (2.0, -30.0));
        assert_eq!(args(json!({"scale": 1.5, "rotate": 90})).motion().unwrap(), (1.5, 90.0));
        assert_eq!(args(json!({"rotate": 45})).motion().unwrap(), (1.0, 45.0));
    }

    #[test]
    fn arguments_that_cannot_make_a_gesture_are_invalid() {
        for bad in [
            json!({}),
            json!({"scale": 1}),
            json!({"direction": "out", "scale": 2}),
            json!({"direction": "sideways"}),
            json!({"scale": 0.2}),
            json!({"scale": 5}),
            json!({"rotate": 181}),
            json!({"direction": "in", "rotate": -181}),
        ] {
            let fail = args(bad.clone()).motion().unwrap_err();
            assert_eq!((fail.code.as_str(), fail.outcome), ("invalid_argument", Outcome::NotSent), "{bad}");
        }
        assert!(serde_json::from_value::<PinchArgs>(json!({"scale": 2, "zoom": 1})).is_err());
    }

    #[tokio::test]
    async fn a_bad_request_fails_before_the_phone_is_asked_for_anything() {
        let mut s = Session::new("00000000-TESTONLY-NOPHONE".into(), Config::default());
        for bad in [json!({}), json!({"direction": "in", "scale": 2}), json!({"scale": 9})] {
            let fail = s.cmd_pinch(args(bad.clone())).await.unwrap_err();
            assert_eq!((fail.code.as_str(), fail.outcome), ("invalid_argument", Outcome::NotSent), "{bad}");
        }
        let fail = s.cmd_pinch(args(json!({"direction": "in", "at": 7}))).await.unwrap_err();
        assert_eq!(fail.outcome, Outcome::NotSent);
    }
}
