//! Key and hardware-button names → HID usages.

/// HID keyboard page usages.
pub const KEY_RETURN: u64 = 0x28;
pub const KEY_ESCAPE: u64 = 0x29;
pub const KEY_BACKSPACE: u64 = 0x2A;
pub const KEY_TAB: u64 = 0x2B;
pub const KEY_SPACE: u64 = 0x2C;
pub const KEY_DELETE_FORWARD: u64 = 0x4C;
pub const MOD_CONTROL: u64 = 0xE0;
pub const MOD_SHIFT: u64 = 0xE1;
pub const MOD_OPTION: u64 = 0xE2;
pub const MOD_COMMAND: u64 = 0xE3;

/// Usage of a letter or digit key on a US layout.
pub fn usage(c: char) -> Option<u64> {
    Some(match c.to_ascii_lowercase() {
        c @ 'a'..='z' => 0x04 + (c as u64 - 'a' as u64),
        c @ '1'..='9' => 0x1E + (c as u64 - '1' as u64),
        '0' => 0x27,
        _ => return None,
    })
}

fn named(name: &str) -> Option<u64> {
    Some(match name {
        "return" | "enter" => KEY_RETURN,
        "esc" | "escape" => KEY_ESCAPE,
        "backspace" => KEY_BACKSPACE,
        "delete" | "forward_delete" => KEY_DELETE_FORWARD,
        "tab" => KEY_TAB,
        "space" => KEY_SPACE,
        "right" => 0x4F,
        "left" => 0x50,
        "down" => 0x51,
        "up" => 0x52,
        "home" => 0x4A,
        "end" => 0x4D,
        "pageup" => 0x4B,
        "pagedown" => 0x4E,
        "-" => 0x2D,
        "=" => 0x2E,
        "[" => 0x2F,
        "]" => 0x30,
        "\\" => 0x31,
        ";" => 0x33,
        "'" => 0x34,
        "`" => 0x35,
        "," => 0x36,
        "." => 0x37,
        "/" => 0x38,
        _ => return None,
    })
}

fn modifier(name: &str) -> Option<u64> {
    Some(match name {
        "cmd" | "command" => MOD_COMMAND,
        "ctrl" | "control" => MOD_CONTROL,
        "opt" | "option" | "alt" => MOD_OPTION,
        "shift" => MOD_SHIFT,
        _ => return None,
    })
}

/// Parse `return`, `a`, `cmd+v`, `cmd+shift+z` or `0x28` into (key, modifiers).
pub fn parse_combo(input: &str) -> Result<(u64, Vec<u64>), String> {
    let lower = input.trim().to_lowercase();
    if lower.is_empty() {
        return Err("empty key".into());
    }
    let parts: Vec<&str> = if lower == "+" { vec!["+"] } else { lower.split('+').collect() };
    let (key, mods) = parts.split_last().unwrap();
    let mut modifiers = Vec::new();
    for m in mods {
        modifiers.push(modifier(m.trim()).ok_or_else(|| {
            format!("unknown modifier `{m}` (cmd, ctrl, opt, shift)")
        })?);
    }
    let key = key.trim();
    let usage = if let Some(u) = named(key) {
        u
    } else if let Some(hex) = key.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).map_err(|_| format!("bad key usage `{key}`"))?
    } else if key.chars().count() == 1 {
        usage(key.chars().next().unwrap()).ok_or_else(|| format!("unknown key `{key}`"))?
    } else {
        return Err(format!(
            "unknown key `{key}` (return, esc, tab, space, backspace, delete, up, down, left, right, home, end, pageup, pagedown, a letter, a digit, or 0xNN)"
        ));
    };
    if usage == 0 || usage > 0xE7 {
        return Err(format!("key usage {usage:#x} is out of range"));
    }
    Ok((usage, modifiers))
}

/// Hardware button → (usage page, usage, hold in ms).
pub fn button(name: &str) -> Result<(u64, u64, u64), String> {
    Ok(match name.trim().to_lowercase().replace('-', "_").as_str() {
        "home" => (0x0C, 0x40, 80),
        "lock" | "power" => (0x0C, 0x30, 200),
        "volume_up" => (0x0C, 0xE9, 80),
        "volume_down" => (0x0C, 0xEA, 80),
        other => {
            return Err(format!(
                "unknown button `{other}` (home, lock, volume_up, volume_down)"
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos() {
        assert_eq!(parse_combo("return").unwrap(), (0x28, vec![]));
        assert_eq!(parse_combo("Cmd+V").unwrap(), (0x19, vec![MOD_COMMAND]));
        assert_eq!(
            parse_combo("cmd+shift+z").unwrap(),
            (0x1D, vec![MOD_COMMAND, MOD_SHIFT])
        );
        assert_eq!(parse_combo("0x28").unwrap(), (0x28, vec![]));
        assert_eq!(parse_combo("a").unwrap(), (0x04, vec![]));
        assert!(parse_combo("hyper+a").is_err());
        assert!(parse_combo("nope").is_err());
        assert!(parse_combo("").is_err());
    }

    #[test]
    fn buttons() {
        assert_eq!(button("home").unwrap(), (0x0C, 0x40, 80));
        assert_eq!(button("volume-up").unwrap().1, 0xE9);
        assert!(button("siri").is_err());
    }
}
