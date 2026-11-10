//! Selectors: how a caller names an element on screen.
//!
//! Text form (CLI): `label="정확히"`, `label~="포함"`, `label/정규식/`,
//! `role=button`, `id=…`, `value~=…`, `within=…`, `index=N`, combined with
//! spaces. `label`, `value` and `id` all take `=`, `~=` and `/regex/`. A
//! string with none of these is an exact label.
//!
//! Structured form (recipes, MCP, batch): `{label, label_contains,
//! label_regex, value, value_contains, value_regex, role, id, id_contains,
//! id_regex, within, index}`.

use anyhow::{Result, anyhow, bail};
use regex::Regex;
use serde_json::Value;
use unicode_normalization::UnicodeNormalization;

use crate::ax::tree::{Element, Screen};

#[derive(Debug, Clone)]
pub enum TextMatch {
    Exact(String),
    Contains(String),
    Regex(Regex),
}

fn nfc(s: &str) -> String {
    s.nfc().collect::<String>().trim().to_string()
}

impl TextMatch {
    fn matches(&self, text: Option<&str>, first_segment_counts: bool) -> bool {
        let Some(text) = text else { return false };
        let text = nfc(text);
        match self {
            TextMatch::Exact(want) => {
                // Rows are often labelled "name, value" ("Wi-Fi, HomeNet"); the
                // name alone is what a caller knows in advance.
                text == *want
                    || (first_segment_counts
                        && text.split_once(", ").is_some_and(|(head, _)| head == want))
            }
            TextMatch::Contains(want) => text.to_lowercase().contains(&want.to_lowercase()),
            TextMatch::Regex(re) => re.is_match(&text),
        }
    }

    fn describe(&self, key: &str) -> String {
        match self {
            TextMatch::Exact(s) => format!("{key}=\"{s}\""),
            TextMatch::Contains(s) => format!("{key}~=\"{s}\""),
            TextMatch::Regex(r) => format!("{key}/{}/", r.as_str()),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Selector {
    pub label: Option<TextMatch>,
    pub value: Option<TextMatch>,
    pub role: Option<String>,
    /// accessibilityIdentifier, or (exact form only) the `id` handle from
    /// `iph read`.
    pub id: Option<TextMatch>,
    pub within: Option<Box<Selector>>,
    pub index: Option<usize>,
}

const KEYS: &[&str] = &["label", "value", "role", "id", "within", "index"];

/// Split a selector string into `key op value` terms. Returns None when the
/// string is not in selector syntax at all (then it is a plain label).
fn terms(input: &str) -> Option<Vec<(String, char, String)>> {
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let start = i;
        while i < chars.len() && chars[i].is_ascii_alphabetic() {
            i += 1;
        }
        let key: String = chars[start..i].iter().collect();
        if !KEYS.contains(&key.as_str()) || i >= chars.len() {
            return None;
        }
        // Operator: `=`, `~=` or `/regex/`.
        let op = match chars[i] {
            '=' => {
                i += 1;
                '='
            }
            '~' if chars.get(i + 1) == Some(&'=') => {
                i += 2;
                '~'
            }
            '/' => '/',
            _ => return None,
        };
        let mut val = String::new();
        if op == '/' {
            i += 1;
            let mut closed = false;
            while i < chars.len() {
                if chars[i] == '\\' && chars.get(i + 1) == Some(&'/') {
                    val.push('/');
                    i += 2;
                    continue;
                }
                if chars[i] == '/' {
                    closed = true;
                    i += 1;
                    break;
                }
                val.push(chars[i]);
                i += 1;
            }
            if !closed {
                return None;
            }
        } else if chars.get(i) == Some(&'"') || chars.get(i) == Some(&'\'') {
            let quote = chars[i];
            i += 1;
            let mut closed = false;
            while i < chars.len() {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    val.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                if chars[i] == quote {
                    closed = true;
                    i += 1;
                    break;
                }
                val.push(chars[i]);
                i += 1;
            }
            if !closed {
                return None;
            }
        } else {
            while i < chars.len() && !chars[i].is_whitespace() {
                val.push(chars[i]);
                i += 1;
            }
        }
        out.push((key, op, val));
    }
    (!out.is_empty()).then_some(out)
}

fn text_match(op: char, val: &str) -> Result<TextMatch> {
    Ok(match op {
        '=' => TextMatch::Exact(nfc(val)),
        '~' => TextMatch::Contains(nfc(val)),
        _ => TextMatch::Regex(Regex::new(val).map_err(|e| anyhow!("bad regex /{val}/: {e}"))?),
    })
}

impl Selector {
    pub fn parse(input: &str) -> Result<Selector> {
        let input = input.trim();
        if input.is_empty() {
            bail!("empty selector");
        }
        let Some(terms) = terms(input) else {
            return Ok(Selector {
                label: Some(TextMatch::Exact(nfc(input))),
                ..Default::default()
            });
        };
        let mut sel = Selector::default();
        for (key, op, val) in terms {
            match key.as_str() {
                "label" => sel.label = Some(text_match(op, &val)?),
                "value" => sel.value = Some(text_match(op, &val)?),
                "role" if op == '=' => sel.role = Some(val.to_lowercase()),
                "id" => sel.id = Some(text_match(op, &val)?),
                "within" if op == '=' => sel.within = Some(Box::new(Selector::parse(&val)?)),
                "index" if op == '=' => {
                    sel.index = Some(val.parse().map_err(|_| anyhow!("index must be a number"))?)
                }
                _ => bail!("`{key}` does not support this operator"),
            }
        }
        Ok(sel)
    }

    /// Accepts a selector string or a structured object.
    pub fn from_value(v: &Value) -> Result<Selector> {
        match v {
            Value::String(s) => Selector::parse(s),
            Value::Object(map) => {
                let mut sel = Selector::default();
                for (key, val) in map {
                    let s = || -> Result<String> {
                        match val {
                            Value::String(s) => Ok(s.clone()),
                            Value::Number(n) => Ok(n.to_string()),
                            Value::Bool(b) => Ok(b.to_string()),
                            _ => bail!("selector field `{key}` must be text"),
                        }
                    };
                    match key.as_str() {
                        "label" => sel.label = Some(TextMatch::Exact(nfc(&s()?))),
                        "label_contains" => sel.label = Some(TextMatch::Contains(nfc(&s()?))),
                        "label_regex" => sel.label = Some(text_match('/', &s()?)?),
                        "value" => sel.value = Some(TextMatch::Exact(nfc(&s()?))),
                        "value_contains" => sel.value = Some(TextMatch::Contains(nfc(&s()?))),
                        "value_regex" => sel.value = Some(text_match('/', &s()?)?),
                        "role" => sel.role = Some(s()?.to_lowercase()),
                        "id" | "identifier" => sel.id = Some(TextMatch::Exact(nfc(&s()?))),
                        "id_contains" => sel.id = Some(TextMatch::Contains(nfc(&s()?))),
                        "id_regex" => sel.id = Some(text_match('/', &s()?)?),
                        "within" => sel.within = Some(Box::new(Selector::from_value(val)?)),
                        "index" => {
                            sel.index = Some(
                                val.as_u64()
                                    .ok_or_else(|| anyhow!("index must be a number"))?
                                    as usize,
                            )
                        }
                        other => bail!("unknown selector field `{other}`"),
                    }
                }
                if sel.is_empty() {
                    bail!("empty selector");
                }
                Ok(sel)
            }
            _ => bail!("a selector is a string or an object"),
        }
    }

    fn is_empty(&self) -> bool {
        self.label.is_none()
            && self.value.is_none()
            && self.role.is_none()
            && self.id.is_none()
            && self.within.is_none()
            && self.index.is_none()
    }

    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(l) = &self.label {
            parts.push(l.describe("label"));
        }
        if let Some(v) = &self.value {
            parts.push(v.describe("value"));
        }
        if let Some(r) = &self.role {
            parts.push(format!("role={r}"));
        }
        if let Some(i) = &self.id {
            parts.push(i.describe("id"));
        }
        if let Some(w) = &self.within {
            parts.push(format!("within=\"{}\"", w.describe().replace('"', "\\\"")));
        }
        if let Some(i) = self.index {
            parts.push(format!("index={i}"));
        }
        parts.join(" ")
    }

    /// True when the selector reads the identifier, which only the slower
    /// second pass of a reading carries.
    pub fn needs_identifier(&self) -> bool {
        self.id.is_some() || self.within.as_ref().is_some_and(|w| w.needs_identifier())
    }

    fn matches_own(&self, el: &Element) -> bool {
        if el.role == "application" && self.role.as_deref() != Some("application") {
            return false;
        }
        if let Some(l) = &self.label {
            if !l.matches(el.label.as_deref(), true) {
                return false;
            }
        }
        if let Some(v) = &self.value {
            if !v.matches(el.value.as_deref(), false) {
                return false;
            }
        }
        if let Some(r) = &self.role {
            if el.role != *r {
                return false;
            }
        }
        if let Some(id) = &self.id {
            let by_handle = matches!(id, TextMatch::Exact(want) if el.id == *want);
            if !by_handle && !id.matches(el.identifier.as_deref(), false) {
                return false;
            }
        }
        true
    }

    /// All matches in reading order. `index` picks one of them.
    pub fn find<'a>(&self, screen: &'a Screen) -> Vec<&'a Element> {
        let containers: Option<Vec<&Element>> = self.within.as_ref().map(|w| w.find(screen));
        let mut out: Vec<&Element> = screen
            .elements
            .iter()
            .filter(|el| self.matches_own(el))
            .filter(|el| match &containers {
                None => true,
                Some(cs) => cs.iter().any(|c| is_inside(el, c, screen)),
            })
            .collect();
        if let Some(i) = self.index {
            out = out.get(i).map(|e| vec![*e]).unwrap_or_default();
        }
        out
    }
}

/// `el` is inside `container` when it descends from it or lies within its frame.
fn is_inside(el: &Element, container: &Element, screen: &Screen) -> bool {
    if el.id == container.id {
        return false;
    }
    let mut parent = el.parent.as_deref();
    while let Some(pid) = parent {
        if pid == container.id {
            return true;
        }
        parent = screen.by_id(pid).and_then(|p| p.parent.as_deref());
    }
    match (el.frame, container.frame) {
        (Some(a), Some(b)) => {
            a.x >= b.x - 0.5
                && a.y >= b.y - 0.5
                && a.x + a.w <= b.x + b.w + 0.5
                && a.y + a.h <= b.y + b.h + 0.5
        }
        _ => false,
    }
}

/// Reduce matches that are the same tap target (same label, frames nested or
/// equal) to one. Anything left over is a real ambiguity.
pub fn dedupe_targets<'a>(matches: Vec<&'a Element>) -> Vec<&'a Element> {
    let mut out: Vec<&Element> = Vec::new();
    'next: for m in matches {
        for kept in out.iter_mut() {
            let (Some(a), Some(b)) = (kept.frame, m.frame) else {
                continue;
            };
            let same_point = {
                let (ax, ay) = a.center();
                let (bx, by) = b.center();
                (ax - bx).abs() <= 2.0 && (ay - by).abs() <= 2.0
            };
            let nested = a.intersection(&b).is_some_and(|i| {
                (i.area() - a.area()).abs() < 1.0 || (i.area() - b.area()).abs() < 1.0
            });
            if kept.label == m.label && (same_point || nested) {
                // Prefer the element an agent would expect to act on.
                let rank = |e: &Element| match e.role.as_str() {
                    "button" | "switch" | "link" | "tab" | "text_field" | "search_field" => 0,
                    _ => 1,
                };
                if rank(m) < rank(kept) {
                    *kept = m;
                }
                continue 'next;
            }
        }
        out.push(m);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ax::tree::Rect;

    fn el(id: &str, role: &str, label: &str, frame: (f64, f64, f64, f64)) -> Element {
        Element {
            id: id.into(),
            role: role.into(),
            label: (!label.is_empty()).then(|| label.to_string()),
            value: None,
            identifier: None,
            hint: None,
            frame: Some(Rect { x: frame.0, y: frame.1, w: frame.2, h: frame.3 }),
            traits: vec![],
            enabled: true,
            selected: false,
            focused: false,
            parent: Some("app".into()),
            depth: 1,
            pid: 1,
            on_screen: true,
            raw: None,
        }
    }

    fn screen() -> Screen {
        let mut app = el("app", "application", "설정", (0.0, 0.0, 430.0, 932.0));
        app.parent = None;
        app.depth = 0;
        let mut wifi = el("b", "button", "Wi-Fi, HomeNet", (20.0, 538.0, 390.0, 49.0));
        wifi.identifier = Some("com.apple.settings.wifi".into());
        let mut airplane = el("c", "switch", "에어플레인 모드", (20.0, 489.0, 390.0, 49.0));
        airplane.value = Some("0".into());
        Screen {
            width: 430.0,
            height: 932.0,
            apps: vec![],
            elements: vec![
                app,
                el("a", "header", "설정", (20.0, 119.0, 54.0, 37.0)),
                airplane,
                wifi,
                el("d", "button", "일반", (20.0, 700.0, 390.0, 49.0)),
                el("e", "button", "일반 정보", (20.0, 750.0, 390.0, 49.0)),
            ],
        }
    }

    fn ids(sel: &str) -> Vec<String> {
        Selector::parse(sel)
            .unwrap()
            .find(&screen())
            .iter()
            .map(|e| e.id.clone())
            .collect()
    }

    #[test]
    fn plain_string_is_exact_label_and_skips_the_application() {
        assert_eq!(ids("일반"), vec!["d"]);
        assert_eq!(ids("설정"), vec!["a"]);
    }

    #[test]
    fn exact_label_accepts_the_name_part_of_a_row() {
        assert_eq!(ids("Wi-Fi"), vec!["b"]);
        assert_eq!(ids("label=\"Wi-Fi\""), vec!["b"]);
        assert!(ids("HomeNet").is_empty());
    }

    #[test]
    fn contains_regex_role_id_value_index() {
        assert_eq!(ids("label~=\"일반\""), vec!["d", "e"]);
        assert_eq!(ids("label~=wi-fi"), vec!["b"]);
        assert_eq!(ids("label/^일반$/"), vec!["d"]);
        assert_eq!(ids("role=switch"), vec!["c"]);
        assert_eq!(ids("id=com.apple.settings.wifi"), vec!["b"]);
        assert_eq!(ids("id=e"), vec!["e"]);
        assert_eq!(ids("id~=settings.wi"), vec!["b"]);
        assert_eq!(ids("id/^com\\.apple\\.settings\\./"), vec!["b"]);
        assert_eq!(ids("role=switch value=0"), vec!["c"]);
        assert_eq!(ids("label~=일반 index=1"), vec!["e"]);
        assert!(ids("label~=일반 index=5").is_empty());
    }

    #[test]
    fn within_uses_frames_when_the_tree_is_flat() {
        let mut s = screen();
        s.elements.push(el("row", "other", "행", (20.0, 800.0, 390.0, 60.0)));
        s.elements.push(el("inner", "button", "삭제", (350.0, 810.0, 40.0, 40.0)));
        s.elements.push(el("outer", "button", "삭제", (350.0, 870.0, 40.0, 40.0)));
        let sel = Selector::parse("label=\"삭제\" within=\"행\"").unwrap();
        let found: Vec<_> = sel.find(&s).iter().map(|e| e.id.clone()).collect();
        assert_eq!(found, vec!["inner"]);
    }

    #[test]
    fn structured_form_matches_text_form() {
        let v = serde_json::json!({"label_contains": "일반", "role": "button", "index": 0});
        let sel = Selector::from_value(&v).unwrap();
        assert_eq!(sel.find(&screen())[0].id, "d");
        assert!(Selector::from_value(&serde_json::json!({"nope": 1})).is_err());
        assert!(Selector::from_value(&serde_json::json!({})).is_err());
    }

    #[test]
    fn duplicate_targets_collapse_but_real_ambiguity_stays() {
        let a = el("t", "text", "메인", (20.0, 100.0, 390.0, 50.0));
        let b = el("b", "button", "메인", (20.0, 100.0, 390.0, 50.0));
        let c = el("c", "button", "메인", (20.0, 300.0, 390.0, 50.0));
        let one = dedupe_targets(vec![&a, &b]);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].id, "b");
        assert_eq!(dedupe_targets(vec![&a, &b, &c]).len(), 2);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Selector::parse("").is_err());
        assert!(Selector::parse("label/[/").is_err());
        assert!(Selector::parse("index=x").is_err());
    }
}
