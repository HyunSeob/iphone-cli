//! The result shape every command shares:
//! `{ok, outcome, error?: {code, message, hint}, elapsed_ms, …}`.

use std::time::Instant;

use serde::Serialize;
use serde_json::{Map, Value, json};

/// What happened to the input, not whether the command "worked".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The command ran to the end.
    Done,
    /// Nothing reached the phone. Safe to try again.
    NotSent,
    /// Input may have reached the phone. Look at the screen; do not resend.
    Unknown,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Done => "done",
            Outcome::NotSent => "not_sent",
            Outcome::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Fail {
    pub outcome: Outcome,
    pub code: String,
    pub message: String,
    pub hint: Option<String>,
    /// Extra top-level fields for the result (candidates, element, …).
    pub extra: Map<String, Value>,
}

pub type Fields = Map<String, Value>;
pub type Res<T = Fields> = Result<T, Fail>;

impl Fail {
    pub fn new(outcome: Outcome, code: &str, message: impl Into<String>) -> Self {
        Self {
            outcome,
            code: code.to_string(),
            message: message.into(),
            hint: None,
            extra: Map::new(),
        }
    }

    pub fn not_sent(code: &str, message: impl Into<String>) -> Self {
        Self::new(Outcome::NotSent, code, message)
    }

    pub fn unknown(code: &str, message: impl Into<String>) -> Self {
        Self::new(Outcome::Unknown, code, message)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::not_sent("invalid_argument", message)
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.extra.insert(key.to_string(), value.into());
        self
    }

    pub fn error_json(&self) -> Value {
        let mut e = json!({ "code": self.code, "message": self.message });
        if let Some(h) = &self.hint {
            e["hint"] = json!(h);
        }
        e
    }
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// Build the fields of a successful result, in `json!` object syntax.
#[macro_export]
macro_rules! fields {
    ($($tt:tt)*) => {{
        match ::serde_json::json!({ $($tt)* }) {
            ::serde_json::Value::Object(m) => m,
            _ => unreachable!(),
        }
    }};
}

/// Wrap a command's result in the common envelope.
pub fn envelope(result: Res, started: Instant) -> Value {
    let elapsed = started.elapsed().as_millis() as u64;
    let mut out = Map::new();
    match result {
        Ok(fields) => {
            out.insert("ok".into(), json!(true));
            out.insert("outcome".into(), json!(Outcome::Done.as_str()));
            out.insert("elapsed_ms".into(), json!(elapsed));
            for (k, v) in fields {
                out.insert(k, v);
            }
        }
        Err(fail) => {
            out.insert("ok".into(), json!(false));
            out.insert("outcome".into(), json!(fail.outcome.as_str()));
            out.insert("error".into(), fail.error_json());
            out.insert("elapsed_ms".into(), json!(elapsed));
            for (k, v) in fail.extra {
                out.insert(k, v);
            }
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_shapes() {
        let ok = envelope(Ok(fields! {"sent": true}), Instant::now());
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["outcome"], "done");
        assert_eq!(ok["sent"], true);
        assert!(ok.get("error").is_none());

        let fail = envelope(
            Err(Fail::not_sent("element_not_found", "no such element")
                .hint("read the screen")
                .with("candidates", "일반")),
            Instant::now(),
        );
        assert_eq!(fail["ok"], false);
        assert_eq!(fail["outcome"], "not_sent");
        assert_eq!(fail["error"]["code"], "element_not_found");
        assert_eq!(fail["error"]["hint"], "read the screen");
        assert_eq!(fail["candidates"], "일반");
    }
}
