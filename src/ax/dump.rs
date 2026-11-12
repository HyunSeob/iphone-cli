//! One accessibility "tree dump" as the phone sends it.
//!
//! A dump is a flat list of answers to the questions the phone's accessibility
//! translator asked each element: "who is the application element", "give me
//! this batch of attributes", "give me this one attribute". Elements are named
//! by (pid, objectID). Attributes are numbered; `attrs` documents the ones this
//! tool understands.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, anyhow, bail};

use super::archive::{self, Ka};

/// Request kinds seen in `associatedRequestType`.
pub mod request {
    /// A single global value (attribute 129 in every dump seen so far).
    pub const GLOBAL: i64 = 2;
    /// One attribute of one element.
    pub const SINGLE: i64 = 3;
    /// The application element itself.
    pub const APPLICATION: i64 = 4;
    /// A batch of attributes of one element, as a dictionary keyed by number.
    pub const BATCH: i64 = 5;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjKey {
    pub pid: i64,
    pub object_id: u64,
}

impl ObjKey {
    pub fn from_ka(v: &Ka) -> Option<Self> {
        if v.class() != Some("AXPTranslationObject") {
            return None;
        }
        Some(Self {
            pid: v.get("pid")?.as_i64()?,
            object_id: v.get("objectID")?.as_u64()?,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct RawElement {
    pub is_application: bool,
    /// Attributes that arrived in a batch (request kind 5).
    pub attrs: BTreeMap<i64, Ka>,
    /// Attributes that arrived one by one (request kind 3). Kept apart because
    /// the same number does not always carry the same kind of value there.
    pub singles: BTreeMap<i64, Ka>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DumpKind {
    /// The quick first pass: labels, frames, roles, traits, children.
    Initial,
    /// The slower second pass with identifiers and the rest.
    Additional,
    Other(String),
}

/// What one message from the accessibility service carried.
#[derive(Debug, Clone)]
pub enum Payload {
    Dump(Dump),
    /// A short notice about one element (an announcement, a screen change)
    /// with the phone's notification number. Not a reading.
    Notification(i64),
    /// The answer to one translator request (`request::attribute`).
    Answer(Answer),
}

/// One attribute of one element, as the phone answered a request for it.
#[derive(Debug, Clone)]
pub struct Answer {
    pub key: Option<ObjKey>,
    pub attribute: i64,
    /// `Ka::Null` when the element has no such attribute or no longer exists.
    pub value: Ka,
}

#[derive(Debug, Clone)]
pub struct Dump {
    pub kind: DumpKind,
    /// Application elements in the order the phone listed them.
    pub applications: Vec<ObjKey>,
    pub elements: HashMap<ObjKey, RawElement>,
    /// Elements in the order their first answer appeared.
    pub order: Vec<ObjKey>,
}

/// Peel the outer archive layers (the payload is an archived NSData holding
/// another archive) until the translator response appears.
fn unwrap_layers(bytes: &[u8]) -> Result<Ka> {
    let mut current = archive::decode(bytes)?;
    for _ in 0..4 {
        match current {
            Ka::Data(ref inner) if archive::looks_like_archive(inner) => {
                current = archive::decode(inner)?;
            }
            _ => return Ok(current),
        }
    }
    bail!("too many nested archive layers")
}

impl Payload {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let root = unwrap_layers(bytes)?;
        let result = root
            .get("resultData")
            .ok_or_else(|| anyhow!("payload is not a translator response"))?;
        if result.get("treeDump").is_none() {
            let notification = root.get("notification").and_then(|n| n.as_i64()).unwrap_or(0);
            if notification != 0 {
                return Ok(Payload::Notification(notification));
            }
            return Ok(Payload::Answer(Answer {
                key: root.get("associatedTranslationObject").and_then(ObjKey::from_ka),
                attribute: root.get("attribute").and_then(|a| a.as_i64()).unwrap_or(0),
                value: result.clone(),
            }));
        }
        Dump::from_response(result).map(Payload::Dump)
    }
}

impl Dump {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        match Payload::parse(bytes)? {
            Payload::Dump(d) => Ok(d),
            Payload::Notification(n) => bail!("payload is notification {n}, not a tree dump"),
            Payload::Answer(_) => bail!("payload is a translator answer, not a tree dump"),
        }
    }

    fn from_response(result: &Ka) -> Result<Self> {
        let entries = result
            .get("treeDump")
            .and_then(|t| t.as_array())
            .ok_or_else(|| anyhow!("payload has no treeDump"))?;
        let kind = match result.get("treeDumpType").and_then(|t| t.as_str()) {
            Some("AXPTreeDumpTypeInitialDump") => DumpKind::Initial,
            Some("AXPTreeDumpTypeAdditionalData") => DumpKind::Additional,
            Some(other) => DumpKind::Other(other.to_string()),
            None => DumpKind::Other(String::new()),
        };

        let mut dump = Dump {
            kind,
            applications: Vec::new(),
            elements: HashMap::new(),
            order: Vec::new(),
        };
        for entry in entries {
            let request = entry
                .get("associatedRequestType")
                .and_then(|v| v.as_i64())
                .unwrap_or(-1);
            let attribute = entry.get("attribute").and_then(|v| v.as_i64()).unwrap_or(0);
            let data = entry.get("resultData").unwrap_or(&Ka::Null);
            let subject = entry
                .get("associatedTranslationObject")
                .and_then(ObjKey::from_ka);
            match request {
                request::APPLICATION => {
                    if let Some(key) = ObjKey::from_ka(data) {
                        dump.touch(key).is_application = true;
                        if !dump.applications.contains(&key) {
                            dump.applications.push(key);
                        }
                    }
                }
                request::BATCH => {
                    let (Some(key), Ka::Dict(pairs)) = (subject, data) else {
                        continue;
                    };
                    let el = dump.touch(key);
                    for (k, v) in pairs {
                        if let Some(n) = k.as_i64() {
                            el.attrs.insert(n, v.clone());
                        }
                    }
                }
                request::SINGLE => {
                    if let Some(key) = subject {
                        dump.touch(key).singles.insert(attribute, data.clone());
                    }
                }
                _ => {}
            }
        }
        Ok(dump)
    }

    fn touch(&mut self, key: ObjKey) -> &mut RawElement {
        if !self.elements.contains_key(&key) {
            self.order.push(key);
        }
        self.elements.entry(key).or_default()
    }

    /// Copy attributes this dump lacks from `extra` (the additional pass of the
    /// same screen). Elements that are not in this dump are ignored.
    pub fn absorb(&mut self, extra: &Dump) {
        for (key, more) in &extra.elements {
            if let Some(el) = self.elements.get_mut(key) {
                for (n, v) in &more.attrs {
                    el.attrs.entry(*n).or_insert_with(|| v.clone());
                }
                for (n, v) in &more.singles {
                    el.singles.entry(*n).or_insert_with(|| v.clone());
                }
            }
        }
    }

    /// True when `other` describes the same set of elements.
    pub fn same_elements(&self, other: &Dump) -> bool {
        self.elements.len() == other.elements.len()
            && self.elements.keys().all(|k| other.elements.contains_key(k))
    }
}
