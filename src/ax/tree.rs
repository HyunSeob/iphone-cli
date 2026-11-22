//! Turn a raw dump into the element list `iph read` prints.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::{
    archive::Ka,
    attrs::{self, traits},
    dump::{Dump, ObjKey, RawElement},
};
use crate::device::display::{ActiveDisplay, Canvas, Space};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }

    pub fn area(&self) -> f64 {
        self.w.max(0.0) * self.h.max(0.0)
    }

    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        let x1 = self.x.max(other.x);
        let y1 = self.y.max(other.y);
        let x2 = (self.x + self.w).min(other.x + other.w);
        let y2 = (self.y + self.h).min(other.y + other.h);
        (x2 > x1 && y2 > y1).then_some(Rect {
            x: x1,
            y: y1,
            w: x2 - x1,
            h: y2 - y1,
        })
    }

    /// Parse `{{x, y}, {w, h}}`.
    pub fn parse(s: &str) -> Option<Rect> {
        let nums: Vec<f64> = s
            .split(|c: char| c == '{' || c == '}' || c == ',' || c.is_whitespace())
            .filter(|p| !p.is_empty())
            .map(|p| p.parse::<f64>())
            .collect::<Result<_, _>>()
            .ok()?;
        if nums.len() != 4 || nums.iter().any(|n| !n.is_finite()) {
            return None;
        }
        Some(Rect {
            x: round1(nums[0]),
            y: round1(nums[1]),
            w: round1(nums[2]),
            h: round1(nums[3]),
        })
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Element {
    /// Short handle for this element, stable while the element lives.
    pub id: String,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame: Option<Rect>,
    #[serde(default)]
    pub traits: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub selected: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub focused: bool,
    /// `id` of the parent element, absent for an application.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default)]
    pub depth: u32,
    #[serde(default)]
    pub pid: i64,
    /// False when the element lies outside the screen.
    #[serde(default = "yes")]
    pub on_screen: bool,
    /// Every numbered attribute, only with `--raw`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
    /// The phone's name for the element, to ask it about one attribute.
    #[serde(skip)]
    pub key: Option<ObjKey>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppInfo {
    pub pid: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Screen {
    /// Logical screen size in points.
    pub width: f64,
    pub height: f64,
    pub apps: Vec<AppInfo>,
    pub elements: Vec<Element>,
    /// The display in use when the reading was taken, on a phone with more
    /// than one.
    #[serde(skip)]
    pub display: Option<ActiveDisplay>,
}

/// Role, label, value and rounded frame of one element.
pub type ElementSignature = (String, Option<String>, Option<String>, Option<(i64, i64, i64, i64)>);

#[derive(Debug, Clone, Copy, Default)]
pub struct BuildOptions {
    /// Keep elements that lie outside the screen and walk hidden children.
    pub include_offscreen: bool,
    /// Attach every numbered attribute to each element.
    pub raw: bool,
}

fn handle(key: &ObjKey) -> String {
    // Fold the 64-bit object id and the pid into 32 bits; unique enough within
    // one screen, and `Screen::build` de-duplicates the rare collision.
    let mut h = key.object_id ^ (key.object_id >> 32) ^ ((key.pid as u64) << 17);
    h = h.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("{:08x}", (h >> 32) as u32)
}

/// A readable value, including an empty or whitespace-only string. Missing
/// attributes and unsupported values are not evidence of an empty field.
pub(crate) fn value_text(v: Option<&Ka>) -> Option<String> {
    Some(match v? {
        Ka::Str(s) => s.clone(),
        other @ Ka::Obj { .. } => other.text()?,
        Ka::Int(i) => i.to_string(),
        Ka::UInt(u) => u.to_string(),
        Ka::Real(r) => r.to_string(),
        _ => return None,
    })
}

pub(crate) fn text(v: Option<&Ka>) -> Option<String> {
    let s = value_text(v)?;
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn frame_of(raw: &RawElement) -> Option<Rect> {
    raw.attrs
        .get(&attrs::FRAME)
        .and_then(|v| v.get("NS.rectval"))
        .and_then(|v| v.as_str())
        .and_then(Rect::parse)
}

pub(crate) fn children_of(raw: &RawElement, include_offscreen: bool) -> Vec<ObjKey> {
    let pick = |n: i64| -> Vec<ObjKey> {
        raw.attrs
            .get(&n)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(ObjKey::from_ka).collect())
            .unwrap_or_default()
    };
    let (first, second) = if include_offscreen {
        (attrs::CHILDREN, attrs::VISIBLE_CHILDREN)
    } else {
        (attrs::VISIBLE_CHILDREN, attrs::CHILDREN)
    };
    let list = pick(first);
    if list.is_empty() { pick(second) } else { list }
}

impl Screen {
    pub fn build(dump: &Dump, opts: BuildOptions) -> Screen {
        Self::build_on(dump, opts, None)
    }

    /// `display`: the display in use on a phone with more than one. It gives
    /// the size, which the application frames there do not: the home
    /// screen's says the cover display's size also when the inner one is in
    /// use.
    pub fn build_on(dump: &Dump, opts: BuildOptions, display: Option<ActiveDisplay>) -> Screen {
        let mut screen = Screen {
            width: 0.0,
            height: 0.0,
            apps: Vec::new(),
            elements: Vec::new(),
            display,
        };
        // The screen is as large as the largest application frame.
        for key in &dump.applications {
            if let Some(f) = dump.elements.get(key).and_then(frame_of) {
                screen.width = screen.width.max(f.x + f.w);
                screen.height = screen.height.max(f.y + f.h);
            }
        }
        if let Some(display) = &screen.display {
            (screen.width, screen.height) = display.size(Space::Screen);
        }

        let mut seen: HashSet<ObjKey> = HashSet::new();
        let mut used_ids: HashSet<String> = HashSet::new();
        // Depth-first from each application, in the phone's own child order.
        let mut stack: Vec<(ObjKey, Option<String>, u32)> = dump
            .applications
            .iter()
            .rev()
            .map(|k| (*k, None, 0))
            .collect();
        // Elements the phone described but no application reaches (rare).
        let mut orphans: Vec<ObjKey> = Vec::new();

        loop {
            let Some((key, parent, depth)) = stack.pop() else {
                if orphans.is_empty() {
                    orphans = dump
                        .order
                        .iter()
                        .filter(|k| !seen.contains(k))
                        .copied()
                        .collect();
                    if orphans.is_empty() {
                        break;
                    }
                    for k in orphans.iter().rev() {
                        stack.push((*k, None, 1));
                    }
                    continue;
                }
                break;
            };
            if !seen.insert(key) {
                continue;
            }
            let Some(raw) = dump.elements.get(&key) else {
                continue;
            };
            let bits = raw
                .attrs
                .get(&attrs::TRAITS)
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let role_num = raw.attrs.get(&attrs::ROLE).and_then(|v| v.as_i64());
            let frame = frame_of(raw);

            let mut id = handle(&key);
            while !used_ids.insert(id.clone()) {
                id.push('x');
            }
            for child in children_of(raw, opts.include_offscreen).into_iter().rev() {
                stack.push((child, Some(id.clone()), depth + 1));
            }
            if raw.is_application {
                screen.apps.push(AppInfo {
                    pid: key.pid,
                    name: text(raw.attrs.get(&attrs::LABEL))
                        .or_else(|| text(raw.attrs.get(&attrs::LABEL_PLAIN))),
                    bundle_id: None,
                });
            }
            let (w, h) = screen.size(screen.space_of_pid(key.pid));
            let bounds = Rect { x: 0.0, y: 0.0, w, h };
            let on_screen = match (&frame, bounds.area() > 0.0) {
                (Some(f), true) => f.intersection(&bounds).is_some(),
                (Some(f), false) => f.area() > 0.0,
                (None, _) => raw.is_application,
            };
            if !on_screen && !opts.include_offscreen && !raw.is_application {
                continue;
            }
            let label = text(raw.attrs.get(&attrs::LABEL))
                .or_else(|| text(raw.attrs.get(&attrs::LABEL_PLAIN)));
            let value = value_text(raw.attrs.get(&attrs::VALUE));
            let has_text = label.is_some() || value.as_ref().is_some_and(|v| !v.trim().is_empty());
            screen.elements.push(Element {
                id,
                role: attrs::role_name(role_num, bits, raw.is_application, has_text).to_string(),
                label,
                value,
                identifier: text(raw.attrs.get(&attrs::IDENTIFIER)),
                hint: text(raw.attrs.get(&attrs::HINT)),
                frame,
                traits: attrs::trait_names(bits)
                    .into_iter()
                    .map(String::from)
                    .collect(),
                enabled: bits & traits::NOT_ENABLED == 0,
                selected: bits & traits::SELECTED != 0,
                focused: bits & traits::IS_EDITING != 0,
                parent,
                depth,
                pid: key.pid,
                key: Some(key),
                on_screen,
                raw: opts.raw.then(|| {
                    let mut m = serde_json::Map::new();
                    for (n, v) in &raw.attrs {
                        m.insert(n.to_string(), v.to_json());
                    }
                    for (n, v) in &raw.singles {
                        m.insert(format!("single_{n}"), v.to_json());
                    }
                    serde_json::Value::Object(m)
                }),
            });
        }
        screen
    }

    /// What a comparison of two readings looks at: everything a user could
    /// see or an agent could act on, without handles.
    pub fn signature(&self) -> Vec<ElementSignature> {
        self.elements
            .iter()
            .map(|e| {
                (
                    e.role.clone(),
                    e.label.clone(),
                    e.value.clone(),
                    e.frame.map(|f| {
                        (
                            f.x.round() as i64,
                            f.y.round() as i64,
                            f.w.round() as i64,
                            f.h.round() as i64,
                        )
                    }),
                )
            })
            .collect()
    }

    pub fn by_id(&self, id: &str) -> Option<&Element> {
        self.elements.iter().find(|e| e.id == id)
    }

    /// The space the frames of process `pid` are in. The system's own
    /// surfaces are the applications without a name. With one display there
    /// is one space.
    fn space_of_pid(&self, pid: i64) -> Space {
        let system = self.apps.iter().any(|a| a.pid == pid && a.name.is_none());
        match &self.display {
            Some(_) if system => Space::Panel,
            _ => Space::Screen,
        }
    }

    /// The space `el`'s frame is in.
    pub fn space_of(&self, el: &Element) -> Space {
        self.space_of_pid(el.pid)
    }

    /// The space of a point given by hand: that of the app in front.
    pub fn point_space(&self) -> Space {
        self.apps.first().map_or(Space::Screen, |a| self.space_of_pid(a.pid))
    }

    /// The size of `space` in points.
    pub fn size(&self, space: Space) -> (f64, f64) {
        match &self.display {
            Some(display) => display.size(space),
            None => (self.width, self.height),
        }
    }

    pub fn canvas(&self, space: Space) -> Canvas {
        Canvas { space, size: self.size(space), display: self.display.clone() }
    }

    /// A point of `space` on the panel, where frames of both spaces compare.
    fn on_panel(&self, space: Space, p: (f64, f64)) -> (f64, f64) {
        match &self.display {
            Some(display) => display.on_panel(space, p),
            None => p,
        }
    }

    /// `el`'s frame on the panel.
    pub fn panel_frame(&self, el: &Element) -> Option<Rect> {
        let f = el.frame?;
        let Some(display) = &self.display else {
            return Some(f);
        };
        let space = self.space_of(el);
        let (x1, y1) = display.on_panel(space, (f.x, f.y));
        let (x2, y2) = display.on_panel(space, (f.x + f.w, f.y + f.h));
        Some(Rect { x: x1.min(x2), y: y1.min(y2), w: (x2 - x1).abs(), h: (y2 - y1).abs() })
    }

    /// The smallest on-screen element whose frame contains the point, a point
    /// given by hand (see [`point_space`](Self::point_space)).
    pub fn element_at(&self, x: f64, y: f64) -> Option<&Element> {
        self.element_in(self.point_space(), x, y)
    }

    /// The smallest on-screen element whose frame contains the point, a point
    /// of `space`.
    pub fn element_in(&self, space: Space, x: f64, y: f64) -> Option<&Element> {
        let (x, y) = self.on_panel(space, (x, y));
        self.elements
            .iter()
            .filter(|e| e.role != "application")
            .filter_map(|e| self.panel_frame(e).map(|f| (e, f)))
            .filter(|(_, f)| f.contains(x, y))
            .min_by(|a, b| a.1.area().total_cmp(&b.1.area()))
            .map(|(e, _)| e)
    }

    /// A short description of the screen for error reports: app names and the
    /// first few labels.
    pub fn summary(&self, max_labels: usize) -> serde_json::Value {
        let labels: Vec<&str> = self
            .elements
            .iter()
            .filter(|e| e.role != "application")
            .filter_map(|e| e.label.as_deref())
            .take(max_labels)
            .collect();
        serde_json::json!({
            "apps": self.apps.iter().filter_map(|a| a.name.clone()).collect::<Vec<_>>(),
            "element_count": self.elements.len(),
            "labels": labels,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_values_keep_empty_and_whitespace_distinct_from_missing() {
        for (raw, expected) in [
            (Ka::Str(String::new()), Some("")),
            (Ka::Str(" \n ".into()), Some(" \n ")),
            (Ka::Str("  hello  ".into()), Some("  hello  ")),
            (Ka::Int(0), Some("0")),
            (Ka::Null, None),
            (Ka::Bool(false), None),
            (
                Ka::Obj {
                    class: "Unknown".into(),
                    fields: vec![],
                },
                None,
            ),
        ] {
            assert_eq!(value_text(Some(&raw)).as_deref(), expected);
        }
        assert_eq!(value_text(None), None);
        // Labels keep their existing whitespace normalization.
        assert_eq!(
            text(Some(&Ka::Str("  hello  ".into()))).as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn screen_build_preserves_an_empty_field_value() {
        use crate::ax::dump::DumpKind;
        let key = ObjKey {
            pid: 10,
            object_id: 1,
        };
        let mut dump = Dump {
            kind: DumpKind::Initial,
            applications: vec![key],
            elements: [(
                key,
                RawElement {
                    is_application: true,
                    ..Default::default()
                },
            )]
            .into(),
            order: vec![key],
        };
        let missing = Screen::build(&dump, BuildOptions::default());
        dump.elements
            .get_mut(&key)
            .unwrap()
            .attrs
            .insert(attrs::VALUE, Ka::Str(String::new()));
        let empty = Screen::build(&dump, BuildOptions::default());
        assert_eq!(missing.elements[0].value, None);
        assert_eq!(empty.elements[0].value.as_deref(), Some(""));
        assert_ne!(missing.signature(), empty.signature());
    }

    #[test]
    fn parses_rect_strings() {
        let r = Rect::parse("{{20, 59}, {44, 44}}").unwrap();
        assert_eq!((r.x, r.y, r.w, r.h), (20.0, 59.0, 44.0, 44.0));
        let r = Rect::parse("{{168.66666666666669, 70.666666666666657}, {92.3, 20.67}}").unwrap();
        assert_eq!(r.x, 168.7);
        assert_eq!(r.center().0, 168.7 + 92.3 / 2.0);
        assert!(Rect::parse("{{nan, 0}, {1, 1}}").is_none());
        assert!(Rect::parse("{1, 2}").is_none());
    }

    #[test]
    fn intersection_detects_offscreen() {
        let screen = Rect { x: 0.0, y: 0.0, w: 430.0, h: 932.0 };
        let below = Rect { x: 20.0, y: 1000.0, w: 390.0, h: 50.0 };
        let partly = Rect { x: 20.0, y: 900.0, w: 390.0, h: 50.0 };
        assert!(below.intersection(&screen).is_none());
        assert!(partly.intersection(&screen).is_some());
    }

    /// An iPhone Duo unfolded, the inner display at rot270, with the home
    /// screen (no name) and an app: the home screen's application frame says
    /// the cover display's size, as measured.
    fn duo_dump(home_in_front: bool) -> Dump {
        use crate::ax::dump::DumpKind;
        let rect = |x: f64, y: f64, w: f64, h: f64| Ka::Obj {
            class: "NSValue".into(),
            fields: vec![("NS.rectval".into(), Ka::Str(format!("{{{{{x}, {y}}}, {{{w}, {h}}}}}")))],
        };
        let element = |label: Option<&str>, frame: Ka, is_application: bool| {
            let mut raw = RawElement { is_application, ..Default::default() };
            raw.attrs.insert(attrs::FRAME, frame);
            if let Some(label) = label {
                raw.attrs.insert(attrs::LABEL, Ka::Str(label.into()));
            }
            raw
        };
        let key = |pid: i64, object_id: u64| ObjKey { pid, object_id };
        let (home, app) = (key(50, 1), key(60, 1));
        let elements = [
            (home, element(None, rect(0.0, 0.0, 466.0, 678.0), true)),
            (app, element(Some("Settings"), rect(0.0, 0.0, 951.0, 669.0), true)),
            (key(50, 2), element(Some("Settings icon"), rect(600.0, 900.0, 60.0, 60.0), false)),
            (key(60, 2), element(Some("General"), rect(700.0, 500.0, 200.0, 40.0), false)),
            (key(60, 3), element(Some("Below"), rect(700.0, 700.0, 200.0, 40.0), false)),
        ];
        let order: Vec<ObjKey> = elements.iter().map(|(k, _)| *k).collect();
        Dump {
            kind: DumpKind::Initial,
            applications: if home_in_front { vec![home, app] } else { vec![app, home] },
            elements: elements.into(),
            order,
        }
    }

    fn inner_rot270() -> ActiveDisplay {
        use crate::device::display::Rotation;
        ActiveDisplay { unique_id: "123-456-789".into(), display_id: Some(2), panel: (669.0, 951.0), rotation: Rotation::R270 }
    }

    fn labelled<'a>(screen: &'a Screen, label: &str) -> Option<&'a Element> {
        screen.elements.iter().find(|e| e.label.as_deref() == Some(label))
    }

    #[test]
    fn on_a_turned_inner_display_each_element_is_clipped_to_its_own_space() {
        let screen = Screen::build_on(&duo_dump(true), BuildOptions::default(), Some(inner_rot270()));
        assert_eq!((screen.width, screen.height), (951.0, 669.0));
        let icon = labelled(&screen, "Settings icon").expect("the home screen's icon is inside the panel");
        assert_eq!(screen.space_of(icon), Space::Panel);
        assert_eq!(screen.size(Space::Panel), (669.0, 951.0));
        let row = labelled(&screen, "General").expect("the app's row is inside the turned screen");
        assert_eq!(screen.space_of(row), Space::Screen);
        assert!(labelled(&screen, "Below").is_none(), "below the turned screen");
        assert_eq!(screen.point_space(), Space::Panel, "the home screen is in front");
        let app_in_front = Screen::build_on(&duo_dump(false), BuildOptions::default(), Some(inner_rot270()));
        assert_eq!(app_in_front.point_space(), Space::Screen);
    }

    #[test]
    fn without_a_display_the_size_comes_from_the_application_frames_as_before() {
        let screen = Screen::build(&duo_dump(true), BuildOptions::default());
        assert_eq!((screen.width, screen.height), (951.0, 678.0));
        assert!(labelled(&screen, "Settings icon").is_none());
        let row = labelled(&screen, "General").unwrap();
        assert_eq!(screen.space_of(row), Space::Screen);
        assert_eq!(screen.panel_frame(row), row.frame);
        assert_eq!(screen.element_in(Space::Screen, 800.0, 520.0).map(|e| e.id.as_str()), Some(row.id.as_str()));
    }

    #[test]
    fn the_points_of_a_reading_are_on_the_display_it_was_read_on() {
        let on = Screen::build_on(&duo_dump(true), BuildOptions::default(), Some(inner_rot270()));
        for space in [Space::Screen, Space::Panel] {
            assert_eq!(on.canvas(space), inner_rot270().canvas(space), "{space:?}");
        }
        let unaware = Screen::build(&duo_dump(true), BuildOptions::default());
        assert_eq!(unaware.canvas(Space::Screen).display, None);
    }

    #[test]
    fn frames_of_both_spaces_meet_on_the_panel() {
        let screen = Screen::build_on(&duo_dump(true), BuildOptions::default(), Some(inner_rot270()));
        let row = labelled(&screen, "General").unwrap();
        // rot270 puts (u, v) of the screen at (1 - v, u) of the panel.
        let f = screen.panel_frame(row).unwrap();
        let near = |a: f64, b: f64| (a - b).abs() < 1e-6;
        assert!(near(f.x, 669.0 - 540.0) && near(f.y, 700.0) && near(f.w, 40.0) && near(f.h, 200.0), "{f:?}");
        assert_eq!(screen.element_in(Space::Screen, 800.0, 520.0).map(|e| e.id.as_str()), Some(row.id.as_str()));
        assert_eq!(screen.element_in(Space::Panel, 149.0, 800.0).map(|e| e.id.as_str()), Some(row.id.as_str()));
        let icon = labelled(&screen, "Settings icon").unwrap();
        // The icon's panel point (630, 930) seen from the turned screen.
        assert_eq!(screen.element_in(Space::Screen, 930.0, 39.0).map(|e| e.id.as_str()), Some(icon.id.as_str()));
    }
}
