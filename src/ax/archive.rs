//! Minimal NSKeyedArchiver reader.
//!
//! The accessibility service sends binary property lists written by
//! NSKeyedArchiver: a flat `$objects` table whose entries point at each other
//! with UIDs. This turns one into a plain value tree, keeping unknown classes
//! as generic objects so nothing is lost while the format is being explored.

use std::io::Cursor;

use anyhow::{Result, anyhow, bail};
use plist::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Ka {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Real(f64),
    Str(String),
    Data(Vec<u8>),
    Array(Vec<Ka>),
    Dict(Vec<(Ka, Ka)>),
    Obj {
        class: String,
        fields: Vec<(String, Ka)>,
    },
}

impl Ka {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Ka::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Ka::Bool(b) => Some(*b),
            Ka::Int(i) => Some(*i != 0),
            Ka::UInt(u) => Some(*u != 0),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Ka::UInt(u) => Some(*u),
            Ka::Int(i) => Some(*i as u64),
            Ka::Bool(b) => Some(*b as u64),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Ka::Int(i) => Some(*i),
            Ka::UInt(u) => i64::try_from(*u).ok(),
            Ka::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Ka::Real(r) => Some(*r),
            Ka::Int(i) => Some(*i as f64),
            Ka::UInt(u) => Some(*u as f64),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Ka]> {
        match self {
            Ka::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn class(&self) -> Option<&str> {
        match self {
            Ka::Obj { class, .. } => Some(class),
            _ => None,
        }
    }

    /// Field of an archived object, or the value under a string key of a
    /// dictionary.
    pub fn get(&self, name: &str) -> Option<&Ka> {
        match self {
            Ka::Obj { fields, .. } => fields.iter().find(|(k, _)| k == name).map(|(_, v)| v),
            Ka::Dict(entries) => entries
                .iter()
                .find(|(k, _)| k.as_str() == Some(name))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// Plain text of a string or an attributed string.
    pub fn text(&self) -> Option<String> {
        match self {
            Ka::Str(s) => Some(s.clone()),
            Ka::Obj { class, .. } if class.contains("AttributedString") => {
                self.get("NSString").and_then(|s| s.text())
            }
            _ => None,
        }
    }

    /// Debug JSON for `--raw` output and for exploring unknown attributes.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::{Value as J, json};
        match self {
            Ka::Null => J::Null,
            Ka::Bool(b) => json!(b),
            Ka::Int(i) => json!(i),
            Ka::UInt(u) => json!(u),
            Ka::Real(r) => serde_json::Number::from_f64(*r).map(J::Number).unwrap_or(J::Null),
            Ka::Str(s) => json!(s),
            Ka::Data(d) => json!({ "$data": d.len() }),
            Ka::Array(a) => J::Array(a.iter().map(|x| x.to_json()).collect()),
            Ka::Dict(entries) => {
                let mut m = serde_json::Map::new();
                for (k, v) in entries {
                    let key = match k {
                        Ka::Str(s) => s.clone(),
                        Ka::Int(i) => i.to_string(),
                        Ka::UInt(u) => u.to_string(),
                        other => other.to_json().to_string(),
                    };
                    m.insert(key, v.to_json());
                }
                J::Object(m)
            }
            Ka::Obj { class, fields } => {
                let mut m = serde_json::Map::new();
                m.insert("$class".into(), json!(class));
                for (k, v) in fields {
                    m.insert(k.clone(), v.to_json());
                }
                J::Object(m)
            }
        }
    }
}

/// Most object expansions one archive may take. A full screen needs a few
/// thousand; the cap stops an archive whose objects point at each other in
/// many ways from being unfolded forever.
pub const DEFAULT_BUDGET: usize = 400_000;

/// Decode a keyed archive and return its root object.
pub fn decode(bytes: &[u8]) -> Result<Ka> {
    decode_with_budget(bytes, DEFAULT_BUDGET).map(|(root, _)| root)
}

/// Like [`decode`], with an explicit cap. Also returns how many object
/// expansions were used.
pub fn decode_with_budget(bytes: &[u8], budget: usize) -> Result<(Ka, usize)> {
    let top = Value::from_reader(Cursor::new(bytes)).map_err(|e| anyhow!("not a plist: {e}"))?;
    let dict = top
        .as_dictionary()
        .ok_or_else(|| anyhow!("archive is not a dictionary"))?;
    let objects = dict
        .get("$objects")
        .and_then(|o| o.as_array())
        .ok_or_else(|| anyhow!("archive has no $objects"))?;
    let root = dict
        .get("$top")
        .and_then(|t| t.as_dictionary())
        .and_then(|t| t.get("root").or_else(|| t.values().next()))
        .ok_or_else(|| anyhow!("archive has no $top root"))?;
    let mut r = Resolver {
        objects,
        active: vec![false; objects.len()],
        depth: 0,
        left: budget,
    };
    let root = r.value(root)?;
    Ok((root, budget - r.left))
}

/// True when `bytes` look like a keyed archive (a nested layer).
pub fn looks_like_archive(bytes: &[u8]) -> bool {
    bytes.starts_with(b"bplist00")
}

struct Resolver<'a> {
    objects: &'a [Value],
    /// Objects currently being expanded, to cut reference cycles.
    active: Vec<bool>,
    depth: usize,
    /// Object expansions left before giving up.
    left: usize,
}

const MAX_DEPTH: usize = 200;

impl Resolver<'_> {
    fn value(&mut self, v: &Value) -> Result<Ka> {
        Ok(match v {
            Value::Uid(u) => return self.object(u.get() as usize),
            Value::Boolean(b) => Ka::Bool(*b),
            Value::Integer(i) => match i.as_signed() {
                Some(s) => Ka::Int(s),
                None => Ka::UInt(i.as_unsigned().unwrap_or(0)),
            },
            Value::Real(r) => Ka::Real(*r),
            Value::String(s) => Ka::Str(s.clone()),
            Value::Data(d) => Ka::Data(d.clone()),
            Value::Array(a) => Ka::Array(a.iter().map(|x| self.value(x)).collect::<Result<_>>()?),
            Value::Dictionary(d) => Ka::Dict(
                d.iter()
                    .map(|(k, x)| Ok((Ka::Str(k.clone()), self.value(x)?)))
                    .collect::<Result<_>>()?,
            ),
            Value::Date(d) => Ka::Str(d.to_xml_format()),
            _ => Ka::Null,
        })
    }

    fn object(&mut self, index: usize) -> Result<Ka> {
        if index == 0 {
            return Ok(Ka::Null);
        }
        let Some(obj) = self.objects.get(index) else {
            bail!("archive points at missing object {index}");
        };
        let Some(dict) = obj.as_dictionary().filter(|d| d.contains_key("$class")) else {
            return self.value(obj);
        };
        if self.active[index] || self.depth >= MAX_DEPTH {
            return Ok(Ka::Null);
        }
        if self.left == 0 {
            bail!("archive unfolds into too many objects");
        }
        self.left -= 1;
        self.active[index] = true;
        self.depth += 1;
        let out = self.instance(dict);
        self.depth -= 1;
        self.active[index] = false;
        out
    }

    fn class_name(&self, dict: &plist::Dictionary) -> String {
        dict.get("$class")
            .and_then(|c| match c {
                Value::Uid(u) => self.objects.get(u.get() as usize),
                _ => None,
            })
            .and_then(|c| c.as_dictionary())
            .and_then(|c| c.get("$classname"))
            .and_then(|n| n.as_string())
            .unwrap_or("")
            .to_string()
    }

    fn list(&mut self, dict: &plist::Dictionary, key: &str) -> Result<Vec<Ka>> {
        match dict.get(key).and_then(|v| v.as_array()) {
            Some(items) => items.iter().map(|x| self.value(x)).collect(),
            None => Ok(Vec::new()),
        }
    }

    fn instance(&mut self, dict: &plist::Dictionary) -> Result<Ka> {
        let class = self.class_name(dict);
        match class.as_str() {
            "NSArray" | "NSMutableArray" | "NSSet" | "NSMutableSet" | "NSOrderedSet"
            | "NSMutableOrderedSet" => Ok(Ka::Array(self.list(dict, "NS.objects")?)),
            "NSDictionary" | "NSMutableDictionary" => {
                let keys = self.list(dict, "NS.keys")?;
                let values = self.list(dict, "NS.objects")?;
                Ok(Ka::Dict(keys.into_iter().zip(values).collect()))
            }
            "NSString" | "NSMutableString" => {
                if let Some(s) = dict.get("NS.string").and_then(|s| s.as_string()) {
                    Ok(Ka::Str(s.to_string()))
                } else if let Some(b) = dict.get("NS.bytes").and_then(|b| b.as_data()) {
                    Ok(Ka::Str(String::from_utf8_lossy(b).into_owned()))
                } else {
                    Ok(Ka::Str(String::new()))
                }
            }
            "NSData" | "NSMutableData" => Ok(Ka::Data(
                dict.get("NS.data")
                    .and_then(|d| d.as_data())
                    .map(|d| d.to_vec())
                    .unwrap_or_default(),
            )),
            _ => {
                let mut fields = Vec::with_capacity(dict.len());
                for (k, v) in dict.iter() {
                    if k == "$class" {
                        continue;
                    }
                    fields.push((k.clone(), self.value(v)?));
                }
                Ok(Ka::Obj { class, fields })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plist::{Dictionary, Uid};

    fn archive(objects: Vec<Value>) -> Vec<u8> {
        let mut top = Dictionary::new();
        top.insert("root".into(), Value::Uid(Uid::new(1)));
        let mut d = Dictionary::new();
        d.insert("$archiver".into(), "NSKeyedArchiver".into());
        d.insert("$version".into(), Value::Integer(100000.into()));
        d.insert("$top".into(), Value::Dictionary(top));
        d.insert("$objects".into(), Value::Array(objects));
        let mut out = Vec::new();
        Value::Dictionary(d).to_writer_binary(&mut out).unwrap();
        out
    }

    fn class(name: &str) -> Value {
        let mut d = Dictionary::new();
        d.insert("$classname".into(), name.into());
        Value::Dictionary(d)
    }

    #[test]
    fn decodes_dictionary_with_numeric_keys_and_custom_objects() {
        // 1: NSDictionary {3: "설정", 21: NSValue rect}
        let mut dict = Dictionary::new();
        dict.insert("$class".into(), Value::Uid(Uid::new(6)));
        dict.insert(
            "NS.keys".into(),
            Value::Array(vec![Value::Uid(Uid::new(2)), Value::Uid(Uid::new(3))]),
        );
        dict.insert(
            "NS.objects".into(),
            Value::Array(vec![Value::Uid(Uid::new(4)), Value::Uid(Uid::new(5))]),
        );
        let mut rect = Dictionary::new();
        rect.insert("$class".into(), Value::Uid(Uid::new(7)));
        rect.insert("NS.rectval".into(), "{{0, 0}, {430, 932}}".into());
        let bytes = archive(vec![
            "$null".into(),
            Value::Dictionary(dict),
            Value::Integer(3.into()),
            Value::Integer(21.into()),
            "설정".into(),
            Value::Dictionary(rect),
            class("NSDictionary"),
            class("NSValue"),
        ]);
        let root = decode(&bytes).unwrap();
        let Ka::Dict(entries) = &root else {
            panic!("not a dict: {root:?}")
        };
        assert_eq!(entries[0].0, Ka::Int(3));
        assert_eq!(entries[0].1.as_str(), Some("설정"));
        assert_eq!(entries[1].1.class(), Some("NSValue"));
        assert_eq!(
            entries[1].1.get("NS.rectval").and_then(|v| v.as_str()),
            Some("{{0, 0}, {430, 932}}")
        );
    }

    #[test]
    fn null_reference_and_cycles_do_not_loop() {
        // 1: Thing {next -> 1 (itself), nothing -> $null}
        let mut thing = Dictionary::new();
        thing.insert("$class".into(), Value::Uid(Uid::new(2)));
        thing.insert("next".into(), Value::Uid(Uid::new(1)));
        thing.insert("nothing".into(), Value::Uid(Uid::new(0)));
        let bytes = archive(vec!["$null".into(), Value::Dictionary(thing), class("Thing")]);
        let root = decode(&bytes).unwrap();
        assert_eq!(root.class(), Some("Thing"));
        assert_eq!(root.get("next"), Some(&Ka::Null));
        assert_eq!(root.get("nothing"), Some(&Ka::Null));
    }

    #[test]
    fn densely_linked_objects_hit_the_budget_instead_of_running_forever() {
        // 12 objects that each point at all the others: unfolding every path
        // would take 12! steps.
        let n = 12;
        let mut objects: Vec<Value> = vec!["$null".into()];
        for _ in 0..n {
            let mut d = Dictionary::new();
            d.insert("$class".into(), Value::Uid(Uid::new(n as u64 + 1)));
            d.insert(
                "others".into(),
                Value::Array((1..=n as u64).map(|i| Value::Uid(Uid::new(i))).collect()),
            );
            objects.push(Value::Dictionary(d));
        }
        objects.push(class("Thing"));
        let bytes = archive(objects);
        let started = std::time::Instant::now();
        let err = decode_with_budget(&bytes, 50_000).unwrap_err();
        assert!(err.to_string().contains("too many objects"));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn rejects_non_archives() {
        assert!(decode(b"hello").is_err());
    }
}
