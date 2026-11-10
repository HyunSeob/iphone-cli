//! What the numbered accessibility attributes mean.
//!
//! Found by experiment on iOS 27.0.1 with Settings, Calculator and Notes
//! (2026-11-10): read a screen, compare the numbers with what is visibly
//! there, change one thing, read again. Numbers not listed here are passed
//! through untouched in `iph read --raw`.
//!
//! Seen but not used: 1 activation point, 13 custom actions, 48 and 43 other
//! frames, 61 a second role-like number, 73 language, 78 trait descriptions
//! ("버튼"), 87/88 alternative labels, 128 a pointer string.

/// Text VoiceOver would speak for the element. String or attributed string.
pub const LABEL: i64 = 3;
/// Every child element, on screen or not.
pub const CHILDREN: i64 = 8;
/// Children the phone considers visible.
pub const VISIBLE_CHILDREN: i64 = 9;
/// `{{x, y}, {w, h}}` in logical points, screen coordinates.
pub const FRAME: i64 = 21;
/// Developer-assigned accessibilityIdentifier. Only in the additional pass.
pub const IDENTIFIER: i64 = 25;
/// Whether this is an accessibility element (false for plain containers).
pub const IS_ELEMENT: i64 = 26;
/// Role number, see [`role_name`].
pub const ROLE: i64 = 45;
/// Free-text role description, e.g. "back button".
pub const ROLE_DESCRIPTION: i64 = 46;
/// Current value: text of a field, "1"/"0" of a switch, percentage of a slider.
pub const VALUE: i64 = 53;
/// Bit set of UIAccessibilityTraits, see [`trait_names`].
pub const TRAITS: i64 = 77;
/// Hint text VoiceOver adds after the label ("changes the sign of a value").
pub const HINT: i64 = 23;
/// The label as plain text (number 3 can be an attributed string).
pub const LABEL_PLAIN: i64 = 33;

/// Public UIAccessibilityTraits bits, plus private ones. Seen on device:
/// text_entry and editing (Notes body while typing), search_field (Settings),
/// back_button (navigation bars), toggle (Settings switches), not_enabled
/// (Notes share button on an empty note), keyboard_key (keyboard, Calculator).
/// Not yet seen on device: secure_text, tab_button.
pub mod traits {
    pub const BUTTON: u64 = 1 << 0;
    pub const LINK: u64 = 1 << 1;
    pub const IMAGE: u64 = 1 << 2;
    pub const SELECTED: u64 = 1 << 3;
    pub const PLAYS_SOUND: u64 = 1 << 4;
    pub const KEYBOARD_KEY: u64 = 1 << 5;
    pub const STATIC_TEXT: u64 = 1 << 6;
    pub const SUMMARY: u64 = 1 << 7;
    pub const NOT_ENABLED: u64 = 1 << 8;
    pub const UPDATES_FREQUENTLY: u64 = 1 << 9;
    pub const SEARCH_FIELD: u64 = 1 << 10;
    pub const STARTS_MEDIA: u64 = 1 << 11;
    pub const ADJUSTABLE: u64 = 1 << 12;
    pub const DIRECT_INTERACTION: u64 = 1 << 13;
    pub const CAUSES_PAGE_TURN: u64 = 1 << 14;
    pub const TAB_BAR: u64 = 1 << 15;
    pub const HEADER: u64 = 1 << 16;
    /// Editable text (text field or text view).
    pub const TEXT_ENTRY: u64 = 1 << 18;
    /// The text element that currently has the keyboard.
    pub const IS_EDITING: u64 = 1 << 21;
    pub const SECURE_TEXT: u64 = 1 << 24;
    pub const BACK_BUTTON: u64 = 1 << 27;
    pub const TAB_BUTTON: u64 = 1 << 28;
    pub const TOGGLE: u64 = 1 << 53;
}

const TRAIT_NAMES: &[(u64, &str)] = &[
    (traits::BUTTON, "button"),
    (traits::LINK, "link"),
    (traits::IMAGE, "image"),
    (traits::SELECTED, "selected"),
    (traits::PLAYS_SOUND, "plays_sound"),
    (traits::KEYBOARD_KEY, "keyboard_key"),
    (traits::STATIC_TEXT, "static_text"),
    (traits::SUMMARY, "summary"),
    (traits::NOT_ENABLED, "not_enabled"),
    (traits::UPDATES_FREQUENTLY, "updates_frequently"),
    (traits::SEARCH_FIELD, "search_field"),
    (traits::STARTS_MEDIA, "starts_media"),
    (traits::ADJUSTABLE, "adjustable"),
    (traits::DIRECT_INTERACTION, "direct_interaction"),
    (traits::CAUSES_PAGE_TURN, "causes_page_turn"),
    (traits::TAB_BAR, "tab_bar"),
    (traits::HEADER, "header"),
    (traits::TEXT_ENTRY, "text_entry"),
    (traits::IS_EDITING, "editing"),
    (traits::SECURE_TEXT, "secure_text"),
    (traits::BACK_BUTTON, "back_button"),
    (traits::TAB_BUTTON, "tab_button"),
    (traits::TOGGLE, "toggle"),
];

pub fn trait_names(bits: u64) -> Vec<&'static str> {
    TRAIT_NAMES
        .iter()
        .filter(|(bit, _)| bits & bit != 0)
        .map(|(_, name)| *name)
        .collect()
}

/// Role name for an element. The trait bits are the dependable source (they
/// are a public, stable set); the role number refines what traits cannot say.
pub fn role_name(role: Option<i64>, bits: u64, is_application: bool, has_text: bool) -> &'static str {
    if is_application || role == Some(1) {
        return "application";
    }
    if bits & traits::SECURE_TEXT != 0 {
        return "secure_text_field";
    }
    if bits & traits::SEARCH_FIELD != 0 {
        return "search_field";
    }
    if bits & traits::TEXT_ENTRY != 0 {
        return "text_field";
    }
    if bits & traits::TOGGLE != 0 || role == Some(3) {
        return "switch";
    }
    if bits & traits::KEYBOARD_KEY != 0 {
        return "key";
    }
    if bits & traits::TAB_BUTTON != 0 {
        return "tab";
    }
    if bits & traits::ADJUSTABLE != 0 {
        return "adjustable";
    }
    if bits & traits::LINK != 0 {
        return "link";
    }
    if bits & traits::BUTTON != 0 || role == Some(2) {
        return "button";
    }
    if bits & traits::HEADER != 0 {
        return "header";
    }
    if bits & traits::IMAGE != 0 {
        return "image";
    }
    if bits & traits::STATIC_TEXT != 0 {
        return "text";
    }
    if bits & traits::TAB_BAR != 0 {
        return "tab_bar";
    }
    // No telling trait: a row of plain text, or a container.
    if has_text { "text" } else { "other" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_traits_seen_on_device() {
        // Settings back button: button + back_button (+ an unnamed bit).
        assert_eq!(trait_names(8724152321), vec!["button", "back_button"]);
        // Settings airplane-mode row: button + static_text + toggle.
        assert_eq!(
            trait_names(9007207844675649),
            vec!["button", "static_text", "toggle"]
        );
    }

    #[test]
    fn roles_follow_traits() {
        assert_eq!(role_name(Some(1), 0, true, true), "application");
        assert_eq!(role_name(Some(3), 9007207844675649, false, true), "switch");
        assert_eq!(role_name(Some(15), 146029151232, false, false), "search_field");
        assert_eq!(role_name(Some(2), 8589934657, false, true), "button");
        assert_eq!(role_name(Some(6), 8590000192, false, true), "header");
        assert_eq!(role_name(Some(14), 146028888064, false, true), "text");
        assert_eq!(role_name(None, 0, false, false), "other");
    }
}
