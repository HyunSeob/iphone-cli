//! `iph dnd`: Do Not Disturb on the phone.
//!
//! A notification banner drops over the top of the screen and takes the tap
//! meant for whatever is under it. The accessibility tree of the app in front
//! does not show it, so it cannot be seen coming (2026-11-10: a banner took a
//! tap on a back button and opened another app). `iph dnd on` keeps banners
//! away while iph works, and `iph dnd off` puts back the Focus that was on.
//!
//! No service on the phone offers this. iph does what a person does: pull
//! Control Center down, hold the Focus control, pick the mode from the list,
//! press Home twice. Control Center belongs to SpringBoard and shows in the
//! accessibility tree; Settings has no switch for it (iOS 27.0.1).

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::{
    actions::{AFTER_DRAG, AFTER_TAP},
    ax::tree::{BuildOptions, Rect, Screen},
    config::DndConfig,
    device::{
        ax::trace,
        input::{Motion, normalize},
        session::{Fresh, Gesture, Session},
    },
    fields,
    i18n::t,
    paths,
    reply::{Fail, Res},
    clock::now_utc,
};

/// Label of the Focus control in Control Center, by phone language.
const CONTROL_LABELS: [&str; 2] = ["집중 모드", "Focus"];
/// Name of Do Not Disturb in the list of modes, by phone language.
const MODE_LABELS: [&str; 2] = ["방해금지 모드", "Do Not Disturb"];
/// Holding the control opens the list of modes whatever the control's size;
/// a short tap on a small one would switch the last mode on or off instead.
const HOLD: Duration = Duration::from_millis(800);
const AFTER_HOME: Duration = Duration::from_millis(500);
/// Between two presses of Home, for the first panel to finish closing.
const BETWEEN_HOME: Duration = Duration::from_millis(600);
/// How long to keep reading for the screen an input should bring up.
const WAIT: Duration = Duration::from_secs(5);
fn nfc(s: &str) -> String {
    s.nfc().collect()
}

/// `label` is `name`, or `name` followed by more ("방해금지 모드, 켬").
fn named(label: &str, name: &str) -> bool {
    label == name || label.strip_prefix(name).is_some_and(|rest| rest.starts_with(", "))
}

/// The words to look for on the phone.
#[derive(Debug, Clone)]
pub struct Labels {
    pub control: Vec<String>,
    pub mode: Vec<String>,
}

impl Labels {
    pub fn new(config: &DndConfig) -> Self {
        let mut control: Vec<String> = CONTROL_LABELS.iter().map(|s| s.to_string()).collect();
        control.extend(config.control.as_deref().map(nfc));
        let mode = match &config.focus {
            Some(name) => vec![nfc(name)],
            None => MODE_LABELS.iter().map(|s| s.to_string()).collect(),
        };
        Self { control, mode }
    }

    fn is_mode(&self, name: &str) -> bool {
        self.mode.iter().any(|m| m == name)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub label: String,
    pub selected: bool,
    pub frame: Rect,
}

/// What the screen shows, as far as Focus goes.
#[derive(Debug, Clone, PartialEq)]
pub enum View {
    /// Control Center with its Focus control; `active` is the Focus that is on.
    Controls { control: Rect, active: Option<String> },
    /// The list of modes.
    List { rows: Vec<Row> },
    Other,
}

/// The home screen, Control Center and the system's other panels have an
/// application element without a name; apps have one.
fn system_ui(screen: &Screen) -> bool {
    screen.apps.first().is_some_and(|a| a.name.is_none())
}

pub fn view(screen: &Screen, labels: &Labels) -> View {
    if !system_ui(screen) {
        return View::Other;
    }
    let buttons: Vec<(Row, Option<String>)> = screen
        .elements
        .iter()
        .filter(|e| e.role == "button" && e.on_screen)
        .filter_map(|e| {
            let row = Row { label: nfc(e.label.as_deref()?), selected: e.selected, frame: e.frame? };
            Some((row, e.value.as_deref().map(nfc)))
        })
        .collect();
    let mode = buttons
        .iter()
        .find(|(b, _)| labels.mode.iter().any(|m| named(&b.label, m)))
        .map(|(b, _)| b.frame);
    if let Some(mode) = mode {
        // The modes stand in one column; the button under it does not.
        let rows = buttons
            .into_iter()
            .map(|(b, _)| b)
            .filter(|b| (b.frame.x - mode.x).abs() < 2.0 && (b.frame.w - mode.w).abs() < 2.0)
            .collect();
        return View::List { rows };
    }
    match buttons.into_iter().find(|(b, _)| labels.control.contains(&b.label)) {
        Some((b, value)) => View::Controls { control: b.frame, active: value.filter(|_| b.selected) },
        None => View::Other,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Want {
    /// Only look.
    Status,
    /// Do Not Disturb on.
    Quiet,
    /// Do Not Disturb off again, and this Focus back on if there was one.
    Back(Option<String>),
}

fn mode_row<'a>(rows: &'a [Row], labels: &Labels) -> Option<&'a Row> {
    rows.iter().find(|r| labels.mode.iter().any(|m| named(&r.label, m)))
}

/// The row to tap, or `None` when the list already shows what is wanted.
pub fn pick<'a>(rows: &'a [Row], labels: &Labels, want: &Want) -> Option<&'a Row> {
    let mode = mode_row(rows, labels)?;
    match want {
        Want::Status => None,
        Want::Quiet => (!mode.selected).then_some(mode),
        // Someone chose another Focus in the meantime: theirs stays.
        Want::Back(_) if !mode.selected => None,
        Want::Back(Some(name)) => {
            let earlier = rows
                .iter()
                .find(|r| r.label == *name)
                .or_else(|| rows.iter().find(|r| named(&r.label, name)))
                .filter(|r| !std::ptr::eq(*r, mode));
            // A Focus that no longer exists cannot come back; quiet still ends.
            Some(earlier.unwrap_or(mode))
        }
        Want::Back(None) => Some(mode),
    }
}

/// Whether the list shows what was wanted.
pub fn reached(rows: &[Row], labels: &Labels, want: &Want) -> bool {
    let quiet = mode_row(rows, labels).is_some_and(|m| m.selected);
    match want {
        Want::Status => true,
        Want::Quiet => quiet,
        Want::Back(_) => !quiet,
    }
}

/// What `iph dnd on` replaced with Do Not Disturb, for `iph dnd off` to put
/// back. Kept in a file so that it outlives the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The Focus that was on, if any.
    pub previous: Option<String>,
    pub at: String,
}

impl Record {
    pub fn load(udid: &str) -> Option<Record> {
        let text = std::fs::read_to_string(paths::dnd_record(udid)).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn save(&self, udid: &str) -> std::io::Result<()> {
        use std::io::Write;
        paths::ensure_private_dir(&paths::state_dir())?;
        let mut file = paths::private_file()
            .create(true)
            .write(true)
            .truncate(true)
            .open(paths::dnd_record(udid))?;
        file.write_all(serde_json::to_string(self).unwrap_or_default().as_bytes())
    }

    fn clear(udid: &str) {
        let _ = std::fs::remove_file(paths::dnd_record(udid));
    }
}

/// What the daemon remembers about Do Not Disturb between commands.
#[derive(Debug, Default)]
pub struct State {
    pub record: Option<Record>,
}

impl State {
    pub fn load(udid: &str) -> Self {
        Self { record: Record::load(udid) }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Switch {
    /// The Focus that was on when Control Center opened.
    pub before: Option<String>,
    /// The Focus that is on now.
    pub after: Option<String>,
    pub changed: bool,
    /// Something other than the app that was in front is in front now.
    pub app_changed: bool,
}

enum Touch {
    /// From the top right corner down: Control Center.
    Pull,
    Hold(f64, f64),
    Tap(f64, f64),
    Home,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DndArgs {
    /// "on", "off" or "status".
    pub action: String,
}

fn no_control() -> Fail {
    Fail::unknown("control_center_unavailable", "Control Center did not show a Focus control").hint(t(
        "제어 센터 첫 페이지에 '집중 모드' 항목이 있어야 하고, 앱을 쓰는 중에도 제어 센터가 열려야 합니다. 폰 언어가 한국어 · 영어가 아니면 config.toml의 [dnd] control에 그 항목의 이름을 적으세요.",
        "Control Center's first page must hold the Focus control, and Control Center must open while an app is in front. For a phone in a language other than Korean or English, put the control's label in [dnd] control in config.toml.",
    ))
}

fn no_list() -> Fail {
    Fail::unknown("focus_list_unavailable", "the list of Focus modes did not show Do Not Disturb").hint(t(
        "폰에 보이는 방해금지 모드의 이름을 config.toml의 [dnd] focus에 적으세요.",
        "Put the name the phone shows for Do Not Disturb in [dnd] focus in config.toml.",
    ))
}

impl Session {
    async fn dnd_touch(&mut self, size: (f64, f64), touch: Touch) -> Res<()> {
        let (w, h) = size;
        let at = |x: f64, y: f64| (normalize(x, w), normalize(y, h));
        let gesture = match touch {
            // Slowing to a stop keeps the pull on Control Center's first
            // page; a flick of the same length landed on its second.
            Touch::Pull => {
                let x = (w - 30.0).max(w * 0.9);
                Gesture::Drag {
                    from: at(x, 2.0),
                    to: at(x, h * 0.32),
                    duration: Duration::from_millis(350),
                    motion: Motion::Place,
                }
            }
            Touch::Hold(x, y) => {
                let (x, y) = at(x, y);
                Gesture::Tap { x, y, hold: HOLD }
            }
            Touch::Tap(x, y) => {
                let (x, y) = at(x, y);
                Gesture::Tap { x, y, hold: Duration::from_millis(50) }
            }
            Touch::Home => {
                let (page, usage, hold) = crate::keys::button("home").map_err(Fail::invalid)?;
                Gesture::Button { page, usage, hold: Duration::from_millis(hold) }
            }
        };
        self.send_gesture(gesture).await
    }

    async fn dnd_look(&mut self, delay: Duration, labels: &Labels) -> Res<(Screen, View)> {
        tokio::time::sleep(delay).await;
        let (_, screen) = self.screen(Fresh::Force, false, BuildOptions::default()).await?;
        let seen = view(&screen, labels);
        trace(|| match &seen {
            View::Controls { control, active } => format!("focus: control center, control at {control:?}, on: {active:?}"),
            View::List { rows } => format!("focus: list of {} modes", rows.len()),
            View::Other => format!("focus: something else ({} elements)", screen.elements.len()),
        });
        Ok((screen, seen))
    }

    /// Read until the screen is what `wanted` describes, for at most `WAIT`.
    async fn dnd_until(
        &mut self,
        delay: Duration,
        labels: &Labels,
        wanted: impl Fn(&View) -> bool,
    ) -> Res<(bool, Screen, View)> {
        let began = Instant::now();
        let mut delay = delay;
        loop {
            let (screen, seen) = self.dnd_look(delay, labels).await?;
            let ok = wanted(&seen);
            if ok || began.elapsed() >= WAIT {
                return Ok((ok, screen, seen));
            }
            delay = Duration::from_millis(200);
        }
    }

    /// Press Home until neither the list nor Control Center is in front, and
    /// return the screen that is. Home is only pressed for a panel that was
    /// seen: one press too many would leave the app.
    ///
    /// `start` is the screen from before the pull when `pulled`; a screen of
    /// the system's that differs from it then counts as a panel too.
    async fn dnd_close(
        &mut self,
        start: &Screen,
        pulled: bool,
        labels: &Labels,
        mut seen: Option<(Screen, View)>,
    ) -> Res<Screen> {
        let size = (start.width, start.height);
        // Home closes one panel at a time, and the list lies over Control
        // Center: two presses, without the reading in between (Control
        // Center takes 3 to 7 s to answer).
        let panels_of = |screen: &Screen, now: &View| match now {
            View::List { .. } => 2,
            View::Controls { .. } => 1,
            // Something of the system's that was not there at the start:
            // another page of Control Center.
            View::Other if pulled
                && system_ui(screen)
                && (!system_ui(start) || screen.signature() != start.signature()) => 1,
            View::Other => 0,
        };
        let mut presses = 0;
        loop {
            let (mut screen, mut now) = match seen.take() {
                Some(x) => x,
                None => self.dnd_look(AFTER_HOME, labels).await?,
            };
            let mut panels = panels_of(&screen, &now);
            if panels > 0 && presses > 0 {
                // A panel still up after Home was pressed for it: the reading
                // may be a late one from before the press. Only a second
                // reading that says the same is believed.
                (screen, now) = self.dnd_look(Duration::from_millis(300), labels).await?;
                panels = panels.min(panels_of(&screen, &now));
            }
            if panels == 0 || presses >= 4 {
                return Ok(screen);
            }
            for i in 0..panels {
                if i > 0 {
                    tokio::time::sleep(BETWEEN_HOME).await;
                }
                self.dnd_touch(size, Touch::Home).await?;
                presses += 1;
            }
        }
    }

    /// Open Control Center, bring Focus to what `want` asks for, and close
    /// it again.
    ///
    /// A failure carries `control_center: "may still be open"` when the
    /// panel could not be seen closed afterwards.
    pub(crate) async fn focus_switch(&mut self, want: Want) -> Res<Switch> {
        let labels = Labels::new(&self.config.dnd);
        let (_, mut start) = self.screen(Fresh::Force, false, BuildOptions::default()).await?;
        if start.width <= 0.0 || start.height <= 0.0 {
            return Err(Fail::not_sent("no_screen_size", "the reading has no application frame"));
        }
        // Nothing has been sent if this fails.
        self.ensure_input().await?;

        // Control Center left open by someone: start from what is under it.
        let seen = view(&start, &labels);
        if seen != View::Other {
            let open = start.clone();
            start = self.dnd_close(&open, false, &labels, Some((start, seen))).await?;
        }
        let result = self.focus_steps(&want, &labels, &start).await;
        // Later commands must not act on a reading of Control Center.
        self.invalidate_readings();
        let Err(fail) = result else {
            return result;
        };
        // Whatever is still up must not stay over the app.
        match self.dnd_close(&start, true, &labels, None).await {
            Ok(_) => Err(fail),
            Err(_) => Err(fail.with("control_center", "may still be open")),
        }
    }

    /// The steps of `focus_switch` from the pull to the closed panel. On
    /// failure the panel is left as it is, for the caller to close.
    async fn focus_steps(&mut self, want: &Want, labels: &Labels, start: &Screen) -> Res<Switch> {
        let size = (start.width, start.height);
        let front = start.apps.first().map(|a| a.pid);
        // A button of that name already on screen is something else (an app
        // icon); the control is the one that appears with the pull.
        let already: Vec<Rect> = start
            .elements
            .iter()
            .filter(|e| e.role == "button")
            .filter(|e| e.label.as_deref().is_some_and(|l| labels.control.contains(&nfc(l))))
            .filter_map(|e| e.frame)
            .collect();
        let control_of = |seen: &View| match seen {
            View::Controls { control, active } if !already.contains(control) => Some((*control, active.clone())),
            _ => None,
        };

        let mut last = None;
        for attempt in 0..2 {
            if attempt == 1 {
                // A pull that did nothing may never have arrived: once in
                // about a dozen new input sessions the phone took no touch
                // at all (cause unknown). Try again on a new session.
                self.close_input().await;
            }
            self.dnd_touch(size, Touch::Pull).await?;
            let (ok, screen, seen) = self
                .dnd_until(AFTER_DRAG, labels, |v| control_of(v).is_some() || matches!(v, View::List { .. }))
                .await?;
            last = Some((screen, seen));
            if ok {
                break;
            }
        }
        let (mut screen, mut seen) = last.expect("the loop ran");
        if matches!(seen, View::List { .. }) {
            // The pull met a list that was open; one step back are the controls.
            self.dnd_touch(size, Touch::Home).await?;
            (_, screen, seen) = self.dnd_until(AFTER_HOME, labels, |v| control_of(v).is_some()).await?;
        }
        let Some((control, active)) = control_of(&seen) else {
            return Err(no_control());
        };

        let quiet = active.as_deref().is_some_and(|a| labels.is_mode(a));
        let nothing_to_do = match want {
            Want::Status => true,
            Want::Quiet => quiet,
            Want::Back(_) => !quiet,
        };
        let mut after = active.clone();
        let mut changed = false;
        if !nothing_to_do {
            let (x, y) = control.center();
            self.dnd_touch(size, Touch::Hold(x, y)).await?;
            (_, screen, seen) = self
                .dnd_until(AFTER_TAP, labels, |v| matches!(v, View::List { .. }))
                .await?;
            let View::List { rows } = &seen else {
                return Err(no_list());
            };
            if let Some(row) = pick(rows, labels, want) {
                after = match (want, mode_row(rows, labels)) {
                    (Want::Quiet, _) => labels.mode.iter().find(|m| named(&row.label, m)).cloned(),
                    (Want::Back(earlier), Some(mode)) if !std::ptr::eq(row, mode) => earlier.clone(),
                    _ => None,
                };
                let (x, y) = row.frame.center();
                if *want == Want::Quiet {
                    // Written before the tap: should iph die right after it,
                    // the next run still knows what to put back.
                    let record = Record { previous: active.clone(), at: now_utc() };
                    record
                        .save(&self.udid)
                        .map_err(|e| Fail::unknown("state_write_failed", format!("{e}")))?;
                    self.dnd.record = Some(record);
                }
                self.dnd_touch(size, Touch::Tap(x, y)).await?;
                let ok;
                (ok, screen, seen) = self
                    .dnd_until(AFTER_TAP, labels, |v| {
                        matches!(v, View::List { rows } if reached(rows, labels, want))
                    })
                    .await?;
                if !ok {
                    return Err(Fail::unknown(
                        "focus_not_changed",
                        "the tap in the list of modes did not change Focus",
                    ));
                }
                changed = true;
            }
        }
        let end = self.dnd_close(start, true, labels, Some((screen, seen))).await?;
        let app_changed = end.apps.first().map(|a| a.pid) != front;
        trace(|| format!("focus: {active:?} -> {after:?} (changed {changed}, app changed {app_changed})"));
        Ok(Switch { before: active, after, changed, app_changed })
    }

    pub async fn cmd_dnd(&mut self, args: DndArgs) -> Res {
        let labels = Labels::new(&self.config.dnd);
        match args.action.as_str() {
            "on" => {
                let switch = self.focus_switch(Want::Quiet).await?;
                Ok(fields! {
                    "on": true,
                    "changed": switch.changed,
                    "focus": switch.after,
                    "previous": self.dnd.record.as_ref().and_then(|r| r.previous.clone()),
                    "set_by_iph": self.dnd.record.is_some(),
                })
            }
            "off" => {
                let earlier = self.dnd.record.as_ref().and_then(|r| r.previous.clone());
                let switch = self.focus_switch(Want::Back(earlier)).await?;
                Record::clear(&self.udid);
                self.dnd = State::default();
                Ok(fields! { "on": false, "changed": switch.changed, "focus": switch.after })
            }
            "status" => {
                let switch = self.focus_switch(Want::Status).await?;
                Ok(fields! {
                    "on": switch.before.as_deref().is_some_and(|f| labels.is_mode(f)),
                    "focus": switch.before,
                    "set_by_iph": self.dnd.record.is_some(),
                    "previous": self.dnd.record.as_ref().and_then(|r| r.previous.clone()),
                })
            }
            other => Err(Fail::invalid(format!("unknown action `{other}` (on, off, status)"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn labels() -> Labels {
        Labels::new(&DndConfig::default())
    }

    fn button(label: &str, value: Option<&str>, selected: bool, frame: [f64; 4]) -> Value {
        json!({
            "id": format!("{label}{}", frame[1]), "role": "button", "label": label, "value": value,
            "frame": { "x": frame[0], "y": frame[1], "w": frame[2], "h": frame[3] },
            "traits": ["button"], "enabled": true, "selected": selected, "focused": false,
            "depth": 1, "pid": 60, "on_screen": true,
        })
    }

    fn screen(app: Option<&str>, elements: Vec<Value>) -> Screen {
        serde_json::from_value(json!({
            "width": 400.0, "height": 900.0,
            "apps": [{ "pid": 60, "name": app }],
            "elements": elements,
        }))
        .unwrap()
    }

    fn controls(active: Option<&str>) -> Screen {
        screen(None, vec![
            button("카메라", None, false, [220.0, 490.0, 73.0, 73.0]),
            button("집중 모드", active, active.is_some(), [46.0, 490.0, 161.0, 73.0]),
        ])
    }

    /// The list as the phone shows it: the mode that is on gets ", 켬", some
    /// others carry a description after their name.
    fn list(on: Option<&str>) -> Screen {
        let names = ["방해금지 모드", "업무", "독서"];
        let mut rows: Vec<Value> = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let is_on = on == Some(*name);
                let label = match (is_on, *name) {
                    (true, n) => format!("{n}, 켬"),
                    (false, "방해금지 모드") => "방해금지 모드, 모든 알림을 무음으로 설정".to_string(),
                    (false, n) => n.to_string(),
                };
                button(&label, None, is_on, [44.0, 190.0 + 70.0 * i as f64, 342.0, 58.0])
            })
            .collect();
        rows.push(button("새로운 집중 모드", None, false, [178.0, 820.0, 73.0, 63.0]));
        screen(None, rows)
    }

    fn rows(s: &Screen) -> Vec<Row> {
        match view(s, &labels()) {
            View::List { rows } => rows,
            other => panic!("not a list: {other:?}"),
        }
    }

    #[test]
    fn control_center_shows_the_focus_that_is_on() {
        assert_eq!(
            view(&controls(Some("업무")), &labels()),
            View::Controls { control: Rect { x: 46.0, y: 490.0, w: 161.0, h: 73.0 }, active: Some("업무".into()) }
        );
        assert!(matches!(view(&controls(None), &labels()), View::Controls { active: None, .. }));
    }

    #[test]
    fn an_app_with_the_same_words_is_not_control_center() {
        // Settings has a row called 집중 모드 and a page that lists the modes.
        let settings = screen(Some("설정"), vec![
            button("집중 모드", None, false, [20.0, 740.0, 390.0, 49.0]),
            button("방해금지 모드", None, false, [20.0, 130.0, 390.0, 50.0]),
        ]);
        assert_eq!(view(&settings, &labels()), View::Other);
    }

    #[test]
    fn the_list_leaves_out_the_button_under_it() {
        let rows = rows(&list(Some("업무")));
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.label != "새로운 집중 모드"));
        assert!(rows.iter().any(|r| r.label == "업무, 켬" && r.selected));
    }

    #[test]
    fn going_quiet_taps_do_not_disturb_unless_it_is_on() {
        let l = labels();
        let off = rows(&list(Some("업무")));
        assert!(named(&pick(&off, &l, &Want::Quiet).unwrap().label, "방해금지 모드"));
        assert!(!reached(&off, &l, &Want::Quiet));
        let on = rows(&list(Some("방해금지 모드")));
        assert_eq!(pick(&on, &l, &Want::Quiet), None);
        assert!(reached(&on, &l, &Want::Quiet));
    }

    #[test]
    fn going_back_picks_the_earlier_focus() {
        let l = labels();
        let quiet = rows(&list(Some("방해금지 모드")));
        let back = Want::Back(Some("업무".into()));
        assert_eq!(pick(&quiet, &l, &back).unwrap().label, "업무");
        assert!(!reached(&quiet, &l, &back));
        assert!(reached(&rows(&list(Some("업무"))), &l, &back));
    }

    #[test]
    fn going_back_to_no_focus_turns_do_not_disturb_off() {
        let l = labels();
        let quiet = rows(&list(Some("방해금지 모드")));
        assert_eq!(pick(&quiet, &l, &Want::Back(None)).unwrap().label, "방해금지 모드, 켬");
        // A Focus that was deleted meanwhile cannot come back.
        assert_eq!(pick(&quiet, &l, &Want::Back(Some("없어진 모드".into()))).unwrap().label, "방해금지 모드, 켬");
        assert!(reached(&rows(&list(None)), &l, &Want::Back(None)));
    }

    #[test]
    fn a_focus_someone_else_chose_is_left_alone() {
        let l = labels();
        let theirs = rows(&list(Some("독서")));
        assert_eq!(pick(&theirs, &l, &Want::Back(Some("업무".into()))), None);
        assert_eq!(pick(&theirs, &l, &Want::Back(None)), None);
    }

    #[test]
    fn another_focus_can_stand_in_for_do_not_disturb() {
        let l = Labels::new(&DndConfig { focus: Some("독서".into()), ..Default::default() });
        let View::List { rows } = view(&list(Some("업무")), &l) else { panic!("not a list") };
        assert_eq!(pick(&rows, &l, &Want::Quiet).unwrap().label, "독서");
        assert!(l.is_mode("독서") && !l.is_mode("방해금지 모드"));
    }

    #[test]
    fn names_match_whole_or_before_a_comma() {
        assert!(named("방해금지 모드", "방해금지 모드"));
        assert!(named("방해금지 모드, 켬", "방해금지 모드"));
        assert!(!named("방해금지 모드 2", "방해금지 모드"));
        assert!(!named("업무", "방해금지 모드"));
    }

    #[test]
    fn the_record_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("iph-dnd-test-{}", std::process::id()));
        // SAFETY: this test is the only one in this binary that reads the variable.
        unsafe { std::env::set_var("IPH_STATE_DIR", &dir) };
        let record = Record { previous: Some("업무".into()), at: now_utc() };
        record.save("TEST-PHONE").unwrap();
        assert_eq!(State::load("TEST-PHONE").record, Some(record));
        Record::clear("TEST-PHONE");
        assert_eq!(Record::load("TEST-PHONE"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
