//! Scripts and the safety guard, on the made-up checkout screen. No phone is
//! involved: taps "land" on the fixture, after the same two checks a tap on a
//! phone goes through (`RunState::prepare`, then `Session::guard_tap`), with
//! the real operation record in a throwaway folder.

#![allow(clippy::result_large_err)]

use std::{
    sync::Once,
    time::{Duration, Instant},
};

use iphone_cli::{
    actions::{brief, resolve_one},
    ax::tree::Screen,
    config::Config,
    device::session::Session,
    fields,
    reply::{Fail, Outcome, Res, envelope},
    safety,
    script::{self, Limits, Prepared, RunState, ScriptHost},
    selector::Selector,
};
use serde_json::{Map, Value, json};

const NO_SUCH_PHONE: &str = "00000000-TESTONLY-NOPHONE";

fn isolate() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("iph-script-{}", uuid::Uuid::new_v4()));
        unsafe {
            std::env::set_var("IPH_STATE_DIR", &dir);
            std::env::set_var("IPH_CONFIG_DIR", dir.join("config"));
        }
    });
}

fn session() -> Session {
    isolate();
    Session::new(NO_SUCH_PHONE.into(), Config::default())
}

/// A phone that always shows the checkout screen and remembers what was pressed.
struct Checkout {
    session: Session,
    screen: Screen,
    state: RunState,
    pressed: Vec<String>,
    /// Give this outcome to the next tap that gets past the guard.
    next_tap: Option<Outcome>,
}

impl Checkout {
    fn new(confirm: &[&str]) -> Self {
        Self {
            session: session(),
            screen: serde_json::from_str(include_str!("fixtures/checkout-screen.json")).unwrap(),
            state: RunState::new(confirm.iter().map(|s| s.to_string()).collect()),
            pressed: Vec::new(),
            next_tap: None,
        }
    }

    fn act(&mut self, cmd: &str, p: Prepared) -> Res {
        match cmd {
            "read" => Ok(fields! { "count": self.screen.elements.len(), "elements": self.screen.elements }),
            "tap" => {
                let sel = Selector::from_value(&p.args["target"]).map_err(|e| Fail::invalid(e.to_string()))?;
                let el = resolve_one(&sel, &self.screen)?;
                // What `tap_impl` asks before it touches the phone.
                let op = self.session.guard_tap(Some(el), p.confirm.as_deref(), "tap", p.marked)?;
                let op_id = op.as_ref().map(|o| o.op_id.clone());
                if let Some(outcome) = self.next_tap.take() {
                    Session::finish_op(op, "unknown");
                    return Err(Fail::new(outcome, "input_failed", "the connection closed while sending"));
                }
                self.pressed.push(el.label.clone().unwrap_or_default());
                Session::finish_op(op, "done");
                Ok(fields! { "sent": true, "element": brief(el), "op_id": op_id })
            }
            other => Err(Fail::invalid(format!("the fixture has no `{other}`"))),
        }
    }

    fn run(&mut self, code: &str) -> Res {
        let limits = Limits { timeout: Duration::from_secs(10), memory: 32 << 20 };
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let ended = rt.block_on(script::run(self, code, &Map::new(), &limits));
        script::finish(ended, &self.state)
    }
}

impl ScriptHost for Checkout {
    async fn call(&mut self, cmd: &str, args: Value) -> Value {
        let started = Instant::now();
        let result = self.state.prepare(cmd, args).and_then(|p| self.act(cmd, p));
        let value = envelope(result, started);
        self.state.note(cmd, &value);
        value
    }
    fn must_stop(&self) -> bool {
        self.state.unknown.is_some()
    }
    fn cut_off(&mut self, _cmd: &str) {}
}

fn states(op_id: &str) -> Vec<String> {
    safety::lookup(op_id).unwrap().into_iter().map(|r| r.state).collect()
}

#[test]
fn a_script_cannot_press_a_guarded_button_on_its_own() {
    // No --confirm on the run. The script tries every way it has.
    let mut phone = Checkout::new(&[]);
    let code = r#"
        const pay = "label~=결제하기";
        const tried = {};
        const attempt = (name, fn) => { try { fn(); tried[name] = "pressed" } catch (e) { tried[name] = e.code } };

        attempt("plain", () => tap(pay));
        attempt("marked", () => tap(pay, { confirm: true }));
        attempt("made up", () => tap(pay, { confirm: "selfmade-001" }));
        // Whatever a blocked result holds is no use either.
        let blocked;
        try { tap(pay) } catch (e) { blocked = e.result }
        const scraps = [blocked.op_id, blocked.matched_word, blocked.element.id, JSON.stringify(blocked)];
        scraps.forEach((scrap, i) => attempt("from result " + i, () => tap(pay, { confirm: scrap || "x" })));
        // Going around the functions to the door behind them changes nothing.
        attempt("raw", () => {
            const r = JSON.parse(__iph("tap", JSON.stringify({ target: pay, confirm: "selfmade-002" })));
            if (!r.ok) { const e = new Error(r.error.message); e.code = r.error.code; throw e; }
        });
        attempt("other pay button", () => tap("label~=\"Pay ₩\"", { confirm: "selfmade-003" }));
        return { tried, word: blocked.matched_word };
    "#;
    let out = phone.run(code).unwrap();
    let tried = out["outputs"]["tried"].as_object().unwrap();
    assert_eq!(tried["plain"], "confirm_required");
    assert_eq!(tried["marked"], "confirm_required");
    for (name, code) in tried.iter().filter(|(name, _)| !matches!(name.as_str(), "plain" | "marked")) {
        assert_eq!(code, "confirm_not_allowed", "{name}");
    }
    assert_eq!(tried.len(), 9);
    assert_eq!(out["outputs"]["word"], "결제하기");
    assert!(phone.pressed.is_empty(), "{:?}", phone.pressed);
    assert_eq!(out["inputs_sent"], 0);
    // Nothing was claimed in the operation record under the made-up ids.
    for id in ["selfmade-001", "selfmade-002", "selfmade-003"] {
        assert!(states(id).is_empty(), "{id}");
    }
}

#[test]
fn an_uncaught_guarded_tap_ends_the_run_as_blocked() {
    let mut phone = Checkout::new(&[]);
    let fail = phone.run("tap('취소');\ntap('label~=결제하기');\ntap('뒤로');").unwrap_err();
    assert_eq!((fail.outcome, fail.code.as_str()), (Outcome::Blocked, "script_failed"));
    assert_eq!(fail.extra["reason"]["code"], "confirm_required");
    assert_eq!(fail.extra["failed_call"], json!({"cmd": "tap", "line": 2, "outcome": "blocked"}));
    // The person is shown what would be pressed.
    assert_eq!(fail.extra["matched_word"], "결제하기");
    assert_eq!(fail.extra["element"]["label"], "3,000원 결제하기");
    assert_eq!(fail.extra["inputs_sent"], 1);
    assert_eq!(phone.pressed, ["취소"]);
}

#[test]
fn with_the_persons_op_id_a_marked_tap_presses_once() {
    let code = "tap('label~=결제하기', { confirm: true }); return { pressed: true }";
    let mut phone = Checkout::new(&["fixture-script-001"]);
    let out = phone.run(code).unwrap();
    assert_eq!(out["outputs"]["pressed"], true);
    assert_eq!(phone.pressed, ["3,000원 결제하기"]);
    assert_eq!(states("fixture-script-001"), ["started", "done"]);

    // The same id on a second run presses nothing.
    let mut phone = Checkout::new(&["fixture-script-001"]);
    let fail = phone.run(code).unwrap_err();
    assert_eq!(fail.outcome, Outcome::Blocked);
    assert_eq!(fail.extra["reason"]["code"], "op_already_used");
    assert!(phone.pressed.is_empty());
}

#[test]
fn an_op_id_does_not_cover_a_tap_the_script_did_not_mark() {
    // The person approved one marked press; the script presses another
    // guarded button without marking it.
    let mut phone = Checkout::new(&["fixture-script-002"]);
    let fail = phone.run("tap('label~=\"Pay ₩\"');").unwrap_err();
    assert_eq!(fail.outcome, Outcome::Blocked);
    assert_eq!(fail.extra["reason"]["code"], "confirm_required");
    assert!(phone.pressed.is_empty());
    assert!(states("fixture-script-002").is_empty(), "the id stays unspent");
}

#[test]
fn one_op_id_presses_one_thing() {
    let mut phone = Checkout::new(&["fixture-script-003"]);
    let fail = phone
        .run("for (let i = 0; i < 3; i++) { tap('label~=결제하기', { confirm: true }) }")
        .unwrap_err();
    assert_eq!(fail.outcome, Outcome::Blocked);
    assert_eq!(fail.extra["reason"]["code"], "confirm_required");
    assert_eq!(fail.extra["confirms_spent"], 1);
    assert_eq!(phone.pressed.len(), 1);

    // Two ids, two presses, and no third.
    let mut phone = Checkout::new(&["fixture-script-004", "fixture-script-005"]);
    let code = "let n = 0; try { for (;;) { tap('label~=결제하기', { confirm: true }); n++ } } catch (e) {} return { n }";
    assert_eq!(phone.run(code).unwrap()["outputs"]["n"], 2);
    assert_eq!(phone.pressed.len(), 2);
}

#[test]
fn a_press_that_may_have_gone_out_is_never_sent_again() {
    let mut phone = Checkout::new(&["fixture-script-006", "fixture-script-007"]);
    phone.next_tap = Some(Outcome::Unknown);
    // The script swallows the failure and tries again with the next id.
    let code = "for (let i = 0; i < 4; i++) { try { tap('label~=결제하기', { confirm: true }) } catch (e) {} }\nreturn 'done'";
    let fail = phone.run(code).unwrap_err();
    assert_eq!(fail.outcome, Outcome::Unknown);
    assert_eq!(fail.extra["reason"]["code"], "input_failed");
    assert_eq!(fail.extra["failed_call"]["cmd"], "tap");
    assert!(phone.pressed.is_empty(), "the second attempt never reached the phone");
    assert_eq!(states("fixture-script-006"), ["started", "unknown"]);
    assert!(states("fixture-script-007").is_empty());
}

// The same rules through the path a real phone takes (`Session::cmd_script`).
// There is no phone, so whatever gets past the checks fails as "not sent".

#[tokio::test]
async fn on_the_real_path_a_marked_call_stops_without_an_op_id() {
    let mut s = session();
    let fail = s.cmd_script("key('return', { confirm: true })", &Map::new(), vec![]).await.unwrap_err();
    assert_eq!((fail.outcome, fail.code.as_str()), (Outcome::Blocked, "script_failed"));
    assert_eq!(fail.extra["reason"]["code"], "confirm_required");
    assert_eq!(fail.extra["failed_call"]["line"], 1);
}

#[tokio::test]
async fn on_the_real_path_a_script_cannot_supply_an_op_id() {
    let mut s = session();
    let code = "try { tap('결제하기', { confirm: 'selfmade-101' }) } catch (e) { return { code: e.code, outcome: e.outcome } }";
    let out = s.cmd_script(code, &Map::new(), vec![]).await.unwrap();
    assert_eq!(out["outputs"], json!({"code": "confirm_not_allowed", "outcome": "blocked"}));
    assert!(states("selfmade-101").is_empty());
}

#[tokio::test]
async fn on_the_real_path_an_op_id_is_spent_even_when_nothing_could_be_sent() {
    let mut s = session();
    let code = "key('return', { confirm: true })";
    let first = s.cmd_script(code, &Map::new(), vec!["fixture-script-101".into()]).await.unwrap_err();
    assert_eq!(first.outcome, Outcome::NotSent, "{first}");
    let second = s.cmd_script(code, &Map::new(), vec!["fixture-script-101".into()]).await.unwrap_err();
    assert_eq!(second.outcome, Outcome::Blocked);
    assert_eq!(second.extra["reason"]["code"], "op_already_used");
    assert_eq!(states("fixture-script-101"), ["started", "not_sent"]);
}

#[tokio::test]
async fn a_request_carries_its_op_ids_and_the_saved_scripts_name() {
    let mut s = session();
    let req = script::request("return { n: Number(args.n) + 1 }", json!({"n": "41"}).as_object().unwrap(), &[], Some(("answer", "builtin:answer")));
    let out = s.serve_script(&req).await.unwrap();
    assert_eq!(out["outputs"], json!({"n": 42}));
    assert_eq!((out["script"].as_str(), out["source"].as_str()), (Some("answer"), Some("builtin:answer")));
    // A bad op_id is turned away before the script starts.
    let bad = json!({"code": "return 1", "confirm": ["ok-1", "not ok"]});
    assert_eq!(s.serve_script(&bad).await.unwrap_err().code, "invalid_argument");
    assert_eq!(s.serve_script(&json!({})).await.unwrap_err().code, "invalid_argument");
}
