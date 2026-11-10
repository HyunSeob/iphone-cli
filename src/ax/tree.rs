//! Turn a raw dump into the element list `iph read` prints.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::{
    archive::Ka,
    attrs::{self, traits},
    dump::{Dump, ObjKey, RawElement},
};

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
        (x2 > x1 && y2 > y1).then(|| Rect {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame: Option<Rect>,
    pub traits: Vec<String>,
    pub enabled: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub selected: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub focused: bool,
    /// `id` of the parent element, absent for an application.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub depth: u32,
    pub pid: i64,
    /// False when the element lies outside the screen.
    pub on_screen: bool,
    /// Every numbered attribute, only with `--raw`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
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
}

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

fn text(v: Option<&Ka>) -> Option<String> {
    let s = match v? {
        Ka::Str(s) => s.clone(),
        other @ Ka::Obj { .. } => other.text()?,
        Ka::Int(i) => i.to_string(),
        Ka::UInt(u) => u.to_string(),
        Ka::Real(r) => r.to_string(),
        _ => return None,
    };
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

fn children_of(raw: &RawElement, include_offscreen: bool) -> Vec<ObjKey> {
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
        let mut screen = Screen {
            width: 0.0,
            height: 0.0,
            apps: Vec::new(),
            elements: Vec::new(),
        };
        // The screen is as large as the largest application frame.
        for key in &dump.applications {
            if let Some(f) = dump.elements.get(key).and_then(frame_of) {
                screen.width = screen.width.max(f.x + f.w);
                screen.height = screen.height.max(f.y + f.h);
            }
        }
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: screen.width,
            h: screen.height,
        };

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
            let on_screen = match (&frame, bounds.area() > 0.0) {
                (Some(f), true) => f.intersection(&bounds).is_some(),
                (Some(f), false) => f.area() > 0.0,
                (None, _) => raw.is_application,
            };

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
                    name: text(raw.attrs.get(&attrs::LABEL)),
                    bundle_id: None,
                });
            }
            if !on_screen && !opts.include_offscreen && !raw.is_application {
                continue;
            }
            let label = text(raw.attrs.get(&attrs::LABEL));
            let value = text(raw.attrs.get(&attrs::VALUE));
            screen.elements.push(Element {
                id,
                role: attrs::role_name(role_num, bits, raw.is_application).to_string(),
                label,
                value,
                identifier: text(raw.attrs.get(&attrs::IDENTIFIER)),
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
    pub fn signature(&self) -> Vec<(String, Option<String>, Option<String>, Option<(i64, i64, i64, i64)>)> {
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

    /// The smallest on-screen element whose frame contains the point.
    pub fn element_at(&self, x: f64, y: f64) -> Option<&Element> {
        self.elements
            .iter()
            .filter(|e| e.role != "application")
            .filter_map(|e| e.frame.map(|f| (e, f)))
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
}
