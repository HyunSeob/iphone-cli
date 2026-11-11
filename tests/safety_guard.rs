//! The safety guard, on a made-up checkout screen. No phone is involved: the
//! session uses a UDID that does not exist.

use std::sync::Once;

use iphone_cli::{
    actions::resolve_one,
    ax::tree::Screen,
    config::{Config, SafetyConfig},
    device::session::Session,
    reply::Outcome,
    safety,
    selector::Selector,
    steps::RunOptions,
};
use serde_json::{Map, json};

const NO_SUCH_PHONE: &str = "00000000-TESTONLY-NOPHONE";

fn isolate() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("iph-safety-{}", uuid::Uuid::new_v4()));
        // The operation record of these tests goes to a throwaway folder.
        unsafe {
            std::env::set_var("IPH_STATE_DIR", &dir);
            std::env::set_var("IPH_CONFIG_DIR", dir.join("config"));
        }
    });
}

fn screen() -> Screen {
    serde_json::from_str(include_str!("fixtures/checkout-screen.json")).unwrap()
}

fn session() -> Session {
    isolate();
    Session::new(NO_SUCH_PHONE.into(), Config::default())
}

#[test]
fn a_pay_button_is_refused_without_confirm() {
    let s = session();
    let screen = screen();
    let el = resolve_one(&Selector::parse("label~=결제하기").unwrap(), &screen).unwrap();
    let fail = s.guard_tap(Some(el), None, "tap", false).unwrap_err();
    assert_eq!(fail.outcome, Outcome::Blocked);
    assert_eq!(fail.code, "confirm_required");
    assert_eq!(fail.extra["matched_word"], "결제하기");
    assert_eq!(fail.extra["element"]["label"], "3,000원 결제하기");
    assert!(fail.hint.is_some());
}

#[test]
fn english_words_guard_whole_words_only() {
    let s = session();
    let screen = screen();
    let pay = resolve_one(&Selector::parse("label~=\"Pay ₩\"").unwrap(), &screen).unwrap();
    assert_eq!(s.guard_tap(Some(pay), None, "tap", false).unwrap_err().code, "confirm_required");
    let methods = resolve_one(&Selector::parse("Payment methods").unwrap(), &screen).unwrap();
    assert!(s.guard_tap(Some(methods), None, "tap", false).unwrap().is_none());
}

#[test]
fn ordinary_buttons_pass() {
    let s = session();
    let screen = screen();
    for label in ["취소", "뒤로"] {
        let el = resolve_one(&Selector::parse(label).unwrap(), &screen).unwrap();
        assert!(s.guard_tap(Some(el), None, "tap", false).unwrap().is_none(), "{label}");
    }
}

#[test]
fn a_button_without_a_label_is_not_caught() {
    // The documented limit: the guard reads labels and nothing else.
    let s = session();
    let screen = screen();
    let icon = screen.by_id("icon0001").unwrap();
    assert!(s.guard_tap(Some(icon), None, "tap", false).unwrap().is_none());
}

#[test]
fn an_op_id_confirms_once_and_is_refused_the_second_time() {
    let s = session();
    let screen = screen();
    let el = resolve_one(&Selector::parse("label~=결제하기").unwrap(), &screen).unwrap();

    let claimed = s.guard_tap(Some(el), Some("fixture-pay-001"), "tap", false).unwrap();
    let record = claimed.expect("a confirmed guarded tap is recorded");
    assert_eq!(record.state, "started");
    assert_eq!(record.matched_word.as_deref(), Some("결제하기"));
    Session::finish_op(Some(record), "done");

    let again = s.guard_tap(Some(el), Some("fixture-pay-001"), "tap", false).unwrap_err();
    assert_eq!(again.outcome, Outcome::Blocked);
    assert_eq!(again.code, "op_already_used");
    let states: Vec<&str> = again.extra["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["started", "done"]);

    // What `iph op fixture-pay-001` shows.
    let records = safety::lookup("fixture-pay-001").unwrap();
    assert_eq!(records.last().unwrap().state, "done");
    // A new id is accepted.
    assert!(s.guard_tap(Some(el), Some("fixture-pay-002"), "tap", false).unwrap().is_some());
}

#[test]
fn an_op_id_that_only_started_is_also_spent() {
    // The process died between "started" and the tap: the id must not work again.
    let s = session();
    let screen = screen();
    let el = resolve_one(&Selector::parse("label~=결제하기").unwrap(), &screen).unwrap();
    assert!(s.guard_tap(Some(el), Some("fixture-crash-001"), "tap", false).unwrap().is_some());
    let again = s.guard_tap(Some(el), Some("fixture-crash-001"), "tap", false).unwrap_err();
    assert_eq!(again.code, "op_already_used");
}

#[test]
fn bad_op_ids_are_rejected() {
    let s = session();
    let screen = screen();
    let el = resolve_one(&Selector::parse("label~=결제하기").unwrap(), &screen).unwrap();
    let fail = s.guard_tap(Some(el), Some("has space"), "tap", false).unwrap_err();
    assert_eq!(fail.code, "invalid_argument");
}

#[test]
fn a_swipe_that_stays_on_a_guarded_button_is_refused() {
    let s = session();
    let screen = screen();
    // The pay button spans x 225..410, y 780..836.
    let fail = s.guard_drag(&screen, (300.0, 800.0), (300.0, 800.0)).unwrap_err();
    assert_eq!((fail.outcome, fail.code.as_str()), (Outcome::Blocked, "confirm_required"));
    assert_eq!(fail.extra["matched_word"], "결제하기");
    assert!(s.guard_drag(&screen, (240.0, 790.0), (400.0, 830.0)).is_err());
    // Leaving the button, staying on an ordinary one, or crossing bare
    // screen is a swipe like any other.
    assert!(s.guard_drag(&screen, (300.0, 800.0), (300.0, 400.0)).is_ok());
    assert!(s.guard_drag(&screen, (40.0, 800.0), (180.0, 800.0)).is_ok());
    assert!(s.guard_drag(&screen, (215.0, 600.0), (215.0, 400.0)).is_ok());
}

#[test]
fn removed_words_stop_guarding_and_added_words_start() {
    isolate();
    let config = Config {
        safety: SafetyConfig {
            add: vec!["주문서".into()],
            remove: vec!["결제하기".into(), "결제".into()],
            ..Default::default()
        },
        ..Default::default()
    };
    let s = Session::new(NO_SUCH_PHONE.into(), config);
    let screen = screen();
    let pay = resolve_one(&Selector::parse("label~=결제하기").unwrap(), &screen).unwrap();
    assert!(s.guard_tap(Some(pay), None, "tap", false).unwrap().is_none());
    let header = screen.by_id("head0001").unwrap();
    assert_eq!(s.guard_tap(Some(header), None, "tap", false).unwrap_err().extra["matched_word"], "주문서");
}

#[tokio::test]
async fn a_batch_step_marked_confirm_stops_without_an_op_id() {
    let mut s = session();
    let steps = [json!({ "key": "return", "confirm": true })];
    let fail = s
        .run_steps(&steps, &Map::new(), &RunOptions { confirm: None, batch: false })
        .await
        .unwrap_err();
    assert_eq!(fail.outcome, Outcome::Blocked);
    assert_eq!(fail.code, "step_failed");
    assert_eq!(fail.extra["reason"]["code"], "confirm_required");
    assert_eq!(fail.extra["failed_step"]["index"], 1);
}

#[tokio::test]
async fn a_step_op_id_is_spent_even_when_the_step_could_not_be_sent() {
    let mut s = session();
    let steps = [json!({ "key": "return", "confirm": true })];
    let opts = RunOptions { confirm: Some("fixture-steps-001".into()), batch: false };
    // No such phone: the step fails before anything is sent …
    let first = s.run_steps(&steps, &Map::new(), &opts).await.unwrap_err();
    assert_eq!(first.outcome, Outcome::NotSent);
    // … and the same id is refused afterwards.
    let second = s.run_steps(&steps, &Map::new(), &opts).await.unwrap_err();
    assert_eq!(second.outcome, Outcome::Blocked);
    assert_eq!(second.extra["reason"]["code"], "op_already_used");
    let records = safety::lookup("fixture-steps-001#1").unwrap();
    assert_eq!(records.iter().map(|r| r.state.as_str()).collect::<Vec<_>>(), ["started", "not_sent"]);
}
