//! One question to the phone's accessibility translator: "what is attribute N
//! of this element?". Sent with `processDataFromHost:`, answered within tens of
//! milliseconds, without the 0.75 s the phone waits before a whole-screen dump.
//!
//! The archive is the one the Mac's own `AXPHostCacheManager` writes for an
//! `AXPTranslatorRequest` (macOS 27, captured 2026-11-12), wrapped in an
//! archived NSData as every DTX object argument is.

use plist::{Dictionary, Uid, Value};

use super::dump::ObjKey;

/// `AXPTranslatorRequest.requestType` for one attribute of one element. (The
/// phone files the answer under its own numbering, which differs.)
const ATTRIBUTE_REQUEST: u64 = 2;

fn class(name: &str, chain: &[&str]) -> Value {
    let mut d = Dictionary::new();
    d.insert("$classname".into(), Value::String(name.into()));
    d.insert(
        "$classes".into(),
        Value::Array(chain.iter().map(|c| Value::String((*c).into())).collect()),
    );
    Value::Dictionary(d)
}

fn archive(objects: Vec<Value>) -> Vec<u8> {
    let mut top = Dictionary::new();
    top.insert("root".into(), Value::Uid(Uid::new(1)));
    let mut d = Dictionary::new();
    d.insert("$version".into(), Value::Integer(100_000.into()));
    d.insert("$archiver".into(), Value::String("NSKeyedArchiver".into()));
    d.insert("$top".into(), Value::Dictionary(top));
    d.insert("$objects".into(), Value::Array(objects));
    let mut out = Vec::new();
    Value::Dictionary(d)
        .to_writer_binary(&mut out)
        .expect("writing a plist to memory cannot fail");
    out
}

/// The bytes to send as the one argument of `processDataFromHost:`.
pub fn attribute(key: ObjKey, attribute: i64) -> Vec<u8> {
    let null = Value::String("$null".into());
    let mut req = Dictionary::new();
    req.insert("requestType".into(), Value::Integer(ATTRIBUTE_REQUEST.into()));
    req.insert("attributeType".into(), Value::Integer(attribute.into()));
    req.insert("actionType".into(), Value::Integer(0.into()));
    req.insert("clientType".into(), Value::Integer(0.into()));
    req.insert("translation".into(), Value::Uid(Uid::new(2)));
    req.insert("parameters".into(), Value::Uid(Uid::new(0)));
    req.insert("$class".into(), Value::Uid(Uid::new(5)));
    let mut obj = Dictionary::new();
    obj.insert("pid".into(), Value::Integer(key.pid.into()));
    obj.insert("objectID".into(), Value::Uid(Uid::new(3)));
    obj.insert("isApplicationElement".into(), Value::Boolean(false));
    obj.insert("didPopuldateAppInfo".into(), Value::Boolean(true));
    obj.insert("rawElementData".into(), Value::Uid(Uid::new(0)));
    obj.insert("bridgeDelegateToken".into(), Value::Uid(Uid::new(0)));
    obj.insert("$class".into(), Value::Uid(Uid::new(4)));
    let inner = archive(vec![
        null.clone(),
        Value::Dictionary(req),
        Value::Dictionary(obj),
        Value::Integer(key.object_id.into()),
        class("AXPTranslationObject", &["AXPTranslationObject", "NSObject"]),
        class("AXPTranslatorRequest", &["AXPTranslatorRequest", "NSObject"]),
    ]);
    let mut data = Dictionary::new();
    data.insert("NS.data".into(), Value::Data(inner));
    data.insert("$class".into(), Value::Uid(Uid::new(2)));
    archive(vec![
        null,
        Value::Dictionary(data),
        class("NSMutableData", &["NSMutableData", "NSData", "NSObject"]),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ax::archive::{self, Ka};

    #[test]
    fn decodes_back_to_the_request() {
        let key = ObjKey { pid: 2383, object_id: 13_597_625_945_791_341_321 };
        let outer = archive::decode(&attribute(key, 21)).unwrap();
        let Ka::Data(inner) = outer else { panic!("outer layer is not data: {outer:?}") };
        let req = archive::decode(&inner).unwrap();
        assert_eq!(req.class(), Some("AXPTranslatorRequest"));
        assert_eq!(req.get("requestType").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(req.get("attributeType").and_then(|v| v.as_i64()), Some(21));
        let t = req.get("translation").unwrap();
        assert_eq!(ObjKey::from_ka(t), Some(key));
    }
}
