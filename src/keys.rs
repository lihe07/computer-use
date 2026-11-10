//! evdev keycodes and a US-layout character map.

use anyhow::{Result, bail};

pub const SHIFT: u32 = 42;

/// Canonical spelling: "Page_Down", "page-down" and "PageDown" all become "pagedown";
/// common aliases map to the names below ("arrowup" -> "up", "esc" stays "esc").
fn canonical(part: &str) -> String {
    let p = part.trim().to_lowercase();
    if p.chars().count() == 1 {
        return p; // "-", "_", "a": the character itself
    }
    let p: String = p.chars().filter(|c| !matches!(c, '_' | '-' | ' ')).collect();
    let p = p.strip_prefix("arrow").filter(|r| matches!(*r, "up" | "down" | "left" | "right")).map(str::to_owned).unwrap_or(p);
    match p.as_str() {
        "lctrl" | "controlleft" | "ctrlleft" => "leftctrl".into(),
        "rctrl" | "controlright" | "ctrlright" => "rightctrl".into(),
        "lshift" | "shiftleft" => "leftshift".into(),
        "rshift" | "shiftright" => "rightshift".into(),
        "lalt" | "altleft" | "option" | "opt" => "alt".into(),
        "ralt" | "altright" => "rightalt".into(),
        "lsuper" | "superleft" | "logo" | "windows" | "command" | "metaleft" => "super".into(),
        "rsuper" | "superright" | "metaright" => "rightmeta".into(),
        "ret" | "newline" | "kpenter" => "enter".into(),
        "bksp" | "backspace" => "backspace".into(),
        "spacebar" => "space".into(),
        "prtsc" | "prtscr" | "printscr" => "print".into(),
        "ins" => "insert".into(),
        "pageup" | "prior" => "pageup".into(),
        "pagedown" | "next_page" => "pagedown".into(),
        "contextmenu" | "apps" | "application" => "menu".into(),
        "plus" => "+".into(),
        "underscore" => "_".into(),
        "hyphen" | "dash" => "-".into(),
        "lessthan" | "less" => "<".into(),
        "greaterthan" | "greater" => ">".into(),
        "question" | "questionmark" => "?".into(),
        "exclamation" | "exclam" | "bang" => "!".into(),
        "at" => "@".into(),
        "hash" | "numbersign" | "pound" => "#".into(),
        "dollar" => "$".into(),
        "percent" => "%".into(),
        "caret" | "circumflex" => "^".into(),
        "ampersand" => "&".into(),
        "asterisk" | "star" => "*".into(),
        "colon" => ":".into(),
        "quote" | "doublequote" => "\"".into(),
        "pipe" | "bar" => "|".into(),
        "tilde" => "~".into(),
        "backtick" | "backquote" => "`".into(),
        "leftparen" | "parenleft" => "(".into(),
        "rightparen" | "parenright" => ")".into(),
        "leftbracket" | "bracketleft" => "[".into(),
        "rightbracket" | "bracketright" => "]".into(),
        "leftcurly" | "braceleft" => "{".into(),
        "rightcurly" | "braceright" => "}".into(),
        _ => p,
    }
}

pub fn named(name: &str) -> Option<u32> {
    Some(match name {
        "esc" | "escape" => 1,
        "minus" => 12,
        "equal" => 13,
        "backspace" => 14,
        "tab" => 15,
        "leftbrace" => 26,
        "rightbrace" => 27,
        "enter" | "return" => 28,
        "ctrl" | "control" | "leftctrl" => 29,
        "semicolon" => 39,
        "apostrophe" => 40,
        "grave" => 41,
        "shift" | "leftshift" => 42,
        "backslash" => 43,
        "comma" => 51,
        "dot" | "period" => 52,
        "slash" => 53,
        "rightshift" => 54,
        "alt" | "leftalt" => 56,
        "space" => 57,
        "capslock" => 58,
        "f1" => 59,
        "f2" => 60,
        "f3" => 61,
        "f4" => 62,
        "f5" => 63,
        "f6" => 64,
        "f7" => 65,
        "f8" => 66,
        "f9" => 67,
        "f10" => 68,
        "f11" => 87,
        "f12" => 88,
        "rightctrl" => 97,
        "print" | "printscreen" => 99,
        "rightalt" | "altgr" => 100,
        "home" => 102,
        "up" => 103,
        "pageup" | "pgup" => 104,
        "left" => 105,
        "right" => 106,
        "end" => 107,
        "down" => 108,
        "pagedown" | "pgdn" => 109,
        "insert" => 110,
        "delete" | "del" => 111,
        "mute" => 113,
        "volumedown" => 114,
        "volumeup" => 115,
        "super" | "meta" | "win" | "cmd" | "leftmeta" => 125,
        "rightmeta" => 126,
        "menu" | "compose" => 139,
        "pause" | "break" => 119,
        "scrolllock" => 70,
        "numlock" => 69,
        "sysrq" => 99,
        "f13" => 183,
        "f14" => 184,
        "f15" => 185,
        "f16" => 186,
        "f17" => 187,
        "f18" => 188,
        "f19" => 189,
        "f20" => 190,
        "f21" => 191,
        "f22" => 192,
        "f23" => 193,
        "f24" => 194,
        "playpause" => 164,
        "nextsong" | "next" => 163,
        "previoussong" | "prev" | "previous" => 165,
        "stop" => 166,
        _ => return None,
    })
}

const ROWS: &[(&str, &str, &[u32])] = &[
    ("1234567890-=", "!@#$%^&*()_+", &[2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]),
    ("qwertyuiop[]", "QWERTYUIOP{}", &[16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27]),
    ("asdfghjkl;'`\\", "ASDFGHJKL:\"~|", &[30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 43]),
    ("zxcvbnm,./", "ZXCVBNM<>?", &[44, 45, 46, 47, 48, 49, 50, 51, 52, 53]),
];

/// (keycode, needs_shift) for a character on a US layout.
pub fn char_key(ch: char) -> Option<(u32, bool)> {
    match ch {
        ' ' => return Some((57, false)),
        '\t' => return Some((15, false)),
        '\n' => return Some((28, false)),
        _ => {}
    }
    for (lo, hi, codes) in ROWS {
        if let Some(i) = lo.chars().position(|c| c == ch) {
            return Some((codes[i], false));
        }
        if let Some(i) = hi.chars().position(|c| c == ch) {
            return Some((codes[i], true));
        }
    }
    None
}

/// "ctrl+shift+t" -> [29, 42, 20]. A shifted character adds shift: "ctrl+plus" ->
/// ctrl+shift+equal. A literal "+" key is written "plus" (or a trailing "+": "ctrl++").
pub fn parse_combo(combo: &str) -> Result<Vec<u32>> {
    let mut parts: Vec<String> = combo.split('+').map(canonical).collect();
    // "ctrl++" splits into ["ctrl", "", ""]: the empty tail is the "+" key itself.
    if combo.trim_end().ends_with("++") || combo.trim() == "+" {
        parts.retain(|p| !p.is_empty());
        parts.push("+".into());
    }
    let mut out = Vec::new();
    let push = |k: u32, out: &mut Vec<u32>| {
        if !out.contains(&k) {
            out.push(k);
        }
    };
    for part in parts.iter().filter(|p| !p.is_empty()) {
        if let Some(k) = named(part) {
            push(k, &mut out);
        } else if let (Some(ch), 1) = (part.chars().next(), part.chars().count()) {
            match char_key(ch) {
                Some((k, shift)) => {
                    if shift {
                        // Shift goes down before the key, like a person would press it.
                        let at = out.len();
                        if !out.contains(&SHIFT) {
                            out.insert(at, SHIFT);
                        }
                    }
                    push(k, &mut out);
                }
                None => bail!("unknown key {part:?}"),
            }
        } else {
            bail!(
                "unknown key {part:?}; use names like enter, tab, esc, backspace, delete, up, pagedown, home, f5, \
                 ctrl, shift, alt, super, or single characters"
            );
        }
    }
    if out.is_empty() {
        bail!("empty key combo");
    }
    Ok(out)
}

pub fn button(name: &str) -> Result<u32> {
    Ok(match name.trim().to_lowercase().as_str() {
        "left" | "primary" | "1" => 0x110,
        "right" | "secondary" | "context" | "3" => 0x111,
        "middle" | "wheel" | "2" => 0x112,
        "back" | "side" | "4" => 0x113,
        "forward" | "extra" | "5" => 0x114,
        _ => bail!("unknown button {name:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spellings_of_the_same_key_agree() {
        for c in ["pagedown", "page_down", "Page-Down", "PageDown", "pgdn"] {
            assert_eq!(parse_combo(c).unwrap(), vec![109], "{c}");
        }
        assert_eq!(parse_combo("ArrowUp").unwrap(), vec![103]);
        assert_eq!(parse_combo("Ctrl+Shift+T").unwrap(), vec![29, 42, 20]);
        assert_eq!(parse_combo("cmd+l").unwrap(), vec![125, 38]);
    }

    #[test]
    fn shifted_characters_add_shift() {
        assert_eq!(parse_combo("ctrl+plus").unwrap(), vec![29, SHIFT, 13]);
        assert_eq!(parse_combo("ctrl++").unwrap(), vec![29, SHIFT, 13]);
        assert_eq!(parse_combo("ctrl+shift+plus").unwrap(), vec![29, SHIFT, 13]);
        assert_eq!(parse_combo("?").unwrap(), vec![SHIFT, 53]);
        assert_eq!(parse_combo("ctrl+-").unwrap(), vec![29, 12]);
    }

    #[test]
    fn bad_keys_are_rejected() {
        assert!(parse_combo("").is_err());
        assert!(parse_combo("ctrl+nosuchkey").unwrap_err().to_string().contains("unknown key"));
        assert!(parse_combo("é").is_err());
    }

    #[test]
    fn every_ascii_printable_has_a_key() {
        for ch in (0x20u8..0x7f).map(char::from) {
            assert!(char_key(ch).is_some(), "{ch:?}");
        }
    }
}
