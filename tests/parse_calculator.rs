//! The two passes of one real reading (Calculator showing 408, iOS 27.0.1)
//! through the whole pipeline: archive layers → dump → element list.

use iphone_cli::{
    ax::{
        dump::{Dump, DumpKind, Payload},
        tree::{BuildOptions, Screen},
    },
    selector::Selector,
};

fn initial() -> Dump {
    Dump::parse(include_bytes!("fixtures/calculator-initial.bin")).unwrap()
}

fn additional() -> Dump {
    Dump::parse(include_bytes!("fixtures/calculator-additional.bin")).unwrap()
}

#[test]
fn first_pass_has_labels_frames_and_roles() {
    let dump = initial();
    assert_eq!(dump.kind, DumpKind::Initial);
    assert_eq!(dump.applications.len(), 1);
    let screen = Screen::build(&dump, BuildOptions::default());
    assert_eq!((screen.width, screen.height), (430.0, 932.0));
    assert_eq!(screen.apps[0].name.as_deref(), Some("계산기"));
    assert_eq!(screen.elements.len(), 25);
    assert_eq!(screen.elements[0].role, "application");

    let seven = Selector::parse("7").unwrap().find(&screen);
    assert_eq!(seven.len(), 1);
    assert_eq!(seven[0].role, "key");
    let f = seven[0].frame.unwrap();
    assert_eq!((f.x, f.y, f.w, f.h), (16.0, 486.0, 93.7, 90.0));
    assert!(seven[0].enabled);
    assert_eq!(seven[0].parent.as_deref(), Some(screen.elements[0].id.as_str()));
    assert_eq!(seven[0].depth, 1);

    let negate = Selector::parse("부호 변경").unwrap().find(&screen);
    assert_eq!(negate[0].hint.as_deref(), Some("특정 값의 부호를 변경합니다."));
    // Identifiers come with the second pass only.
    assert!(screen.elements.iter().all(|e| e.identifier.is_none()));
}

#[test]
fn second_pass_adds_identifiers() {
    let mut dump = initial();
    let extra = additional();
    assert_eq!(extra.kind, DumpKind::Additional);
    assert!(dump.same_elements(&extra));
    dump.absorb(&extra);
    let screen = Screen::build(&dump, BuildOptions::default());
    let equals = Selector::parse("id=Equals").unwrap().find(&screen);
    assert_eq!(equals.len(), 1);
    assert_eq!(equals[0].label.as_deref(), Some("등호"));
    // The display has no label; its value sits in the identifier.
    let display = Selector::parse("id/^StandardInputView/").unwrap().find(&screen);
    assert_eq!(display.len(), 1);
    assert_eq!(display[0].identifier.as_deref(), Some("StandardInputView;value:408"));
    assert_eq!(display[0].label, None);
}

#[test]
fn handles_stay_the_same_between_passes_and_are_unique() {
    let a = Screen::build(&initial(), BuildOptions::default());
    let mut merged = initial();
    merged.absorb(&additional());
    let b = Screen::build(&merged, BuildOptions::default());
    let ids_a: Vec<&str> = a.elements.iter().map(|e| e.id.as_str()).collect();
    let ids_b: Vec<&str> = b.elements.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids_a, ids_b);
    let mut unique = ids_a.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), ids_a.len());
    // An element can be found again by its handle.
    let first_key = &a.elements[5];
    let again = Selector::parse(&format!("id={}", first_key.id)).unwrap().find(&a);
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].label, first_key.label);
}

#[test]
fn raw_option_exposes_numbered_attributes() {
    let screen = Screen::build(&initial(), BuildOptions { include_offscreen: false, raw: true });
    let raw = screen.elements[1].raw.as_ref().unwrap();
    assert!(raw.get("21").is_some(), "frame attribute");
    assert!(raw.get("77").is_some(), "traits attribute");
}

#[test]
fn point_lookup_finds_the_key_under_a_point() {
    let screen = Screen::build(&initial(), BuildOptions::default());
    let hit = screen.element_at(367.0, 825.0).unwrap();
    assert_eq!(hit.label.as_deref(), Some("등호"));
    assert!(screen.element_at(215.0, 20.0).is_none());
}

#[test]
fn garbage_and_truncated_payloads_are_errors_not_panics() {
    assert!(Payload::parse(b"").is_err());
    assert!(Payload::parse(b"bplist00").is_err());
    let bytes = include_bytes!("fixtures/calculator-initial.bin");
    assert!(Payload::parse(&bytes[..bytes.len() / 2]).is_err());
}
