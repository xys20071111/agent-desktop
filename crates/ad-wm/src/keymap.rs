//! 键盘映射：键名/字符 → evdev keycode + 修饰键（US 布局约定）。
//!
//! 注入侧（agentdesk）把 evdev 码 +8 后喂给 smithay（xkb keycode 约定），
//! 客户端按合成键盘的 keymap（强制 us 布局）解析回 keysym。
//! 非 US 可打印区字符（如 CJK）v1 不支持：见 docs/DESIGN.md 风险 R6。

/// linux/input-event-codes.h 中的常用 keycode（evdev 约定）。
pub mod evdev {
    pub const KEY_ESC: u32 = 1;
    pub const KEY_1: u32 = 2;
    pub const KEY_2: u32 = 3;
    pub const KEY_3: u32 = 4;
    pub const KEY_4: u32 = 5;
    pub const KEY_5: u32 = 6;
    pub const KEY_6: u32 = 7;
    pub const KEY_7: u32 = 8;
    pub const KEY_8: u32 = 9;
    pub const KEY_9: u32 = 10;
    pub const KEY_0: u32 = 11;
    pub const KEY_MINUS: u32 = 12;
    pub const KEY_EQUAL: u32 = 13;
    pub const KEY_BACKSPACE: u32 = 14;
    pub const KEY_TAB: u32 = 15;
    pub const KEY_Q: u32 = 16;
    pub const KEY_W: u32 = 17;
    pub const KEY_E: u32 = 18;
    pub const KEY_R: u32 = 19;
    pub const KEY_T: u32 = 20;
    pub const KEY_Y: u32 = 21;
    pub const KEY_U: u32 = 22;
    pub const KEY_I: u32 = 23;
    pub const KEY_O: u32 = 24;
    pub const KEY_P: u32 = 25;
    pub const KEY_LEFTBRACE: u32 = 26;
    pub const KEY_RIGHTBRACE: u32 = 27;
    pub const KEY_ENTER: u32 = 28;
    pub const KEY_LEFTCTRL: u32 = 29;
    pub const KEY_A: u32 = 30;
    pub const KEY_S: u32 = 31;
    pub const KEY_D: u32 = 32;
    pub const KEY_F: u32 = 33;
    pub const KEY_G: u32 = 34;
    pub const KEY_H: u32 = 35;
    pub const KEY_J: u32 = 36;
    pub const KEY_K: u32 = 37;
    pub const KEY_L: u32 = 38;
    pub const KEY_SEMICOLON: u32 = 39;
    pub const KEY_APOSTROPHE: u32 = 40;
    pub const KEY_GRAVE: u32 = 41;
    pub const KEY_LEFTSHIFT: u32 = 42;
    pub const KEY_BACKSLASH: u32 = 43;
    pub const KEY_Z: u32 = 44;
    pub const KEY_X: u32 = 45;
    pub const KEY_C: u32 = 46;
    pub const KEY_V: u32 = 47;
    pub const KEY_B: u32 = 48;
    pub const KEY_N: u32 = 49;
    pub const KEY_M: u32 = 50;
    pub const KEY_COMMA: u32 = 51;
    pub const KEY_DOT: u32 = 52;
    pub const KEY_SLASH: u32 = 53;
    pub const KEY_LEFTALT: u32 = 56;
    pub const KEY_SPACE: u32 = 57;
    pub const KEY_F1: u32 = 59;
    pub const KEY_F2: u32 = 60;
    pub const KEY_F3: u32 = 61;
    pub const KEY_F4: u32 = 62;
    pub const KEY_F5: u32 = 63;
    pub const KEY_F6: u32 = 64;
    pub const KEY_F7: u32 = 65;
    pub const KEY_F8: u32 = 66;
    pub const KEY_F9: u32 = 67;
    pub const KEY_F10: u32 = 68;
    pub const KEY_F11: u32 = 87;
    pub const KEY_F12: u32 = 88;
    pub const KEY_RIGHTCTRL: u32 = 97;
    pub const KEY_HOME: u32 = 102;
    pub const KEY_UP: u32 = 103;
    pub const KEY_PAGEUP: u32 = 104;
    pub const KEY_LEFT: u32 = 105;
    pub const KEY_RIGHT: u32 = 106;
    pub const KEY_END: u32 = 107;
    pub const KEY_DOWN: u32 = 108;
    pub const KEY_PAGEDOWN: u32 = 109;
    pub const KEY_INSERT: u32 = 110;
    pub const KEY_DELETE: u32 = 111;
    pub const KEY_LEFTMETA: u32 = 125;
}

/// 解析出的按键：先按住 mods，再 tap/按住 evdev 键。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedKey {
    pub mods: Vec<Modifier>,
    pub evdev: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Modifier {
    Ctrl,
    Shift,
    Alt,
    Super,
}

impl Modifier {
    pub fn evdev(self) -> u32 {
        match self {
            Modifier::Ctrl => evdev::KEY_LEFTCTRL,
            Modifier::Shift => evdev::KEY_LEFTSHIFT,
            Modifier::Alt => evdev::KEY_LEFTALT,
            Modifier::Super => evdev::KEY_LEFTMETA,
        }
    }
}

/// 键名（xkb keysym 名，大小写不敏感）→ 解析。
pub fn resolve_key_name(name: &str) -> Option<ResolvedKey> {
    use evdev::*;
    let k = |evdev: u32| Some(ResolvedKey { mods: Vec::new(), evdev });
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "return" | "enter" => k(KEY_ENTER),
        "tab" => k(KEY_TAB),
        "escape" | "esc" => k(KEY_ESC),
        "space" => k(KEY_SPACE),
        "backspace" => k(KEY_BACKSPACE),
        "delete" | "del" => k(KEY_DELETE),
        "insert" => k(KEY_INSERT),
        "home" => k(KEY_HOME),
        "end" => k(KEY_END),
        "pageup" => k(KEY_PAGEUP),
        "pagedown" => k(KEY_PAGEDOWN),
        "left" => k(KEY_LEFT),
        "right" => k(KEY_RIGHT),
        "up" => k(KEY_UP),
        "down" => k(KEY_DOWN),
        "ctrl" | "leftctrl" => k(KEY_LEFTCTRL),
        "shift" | "leftshift" => k(KEY_LEFTSHIFT),
        "alt" | "leftalt" => k(KEY_LEFTALT),
        "super" | "meta" | "leftmeta" => k(KEY_LEFTMETA),
        "f1" => k(KEY_F1),
        "f2" => k(KEY_F2),
        "f3" => k(KEY_F3),
        "f4" => k(KEY_F4),
        "f5" => k(KEY_F5),
        "f6" => k(KEY_F6),
        "f7" => k(KEY_F7),
        "f8" => k(KEY_F8),
        "f9" => k(KEY_F9),
        "f10" => k(KEY_F10),
        "f11" => k(KEY_F11),
        "f12" => k(KEY_F12),
        _ => {
            // 单字符：小写字母 / 数字 / 标点 / 大写或上档符号（+shift）
            let mut chars = lower.chars();
            if let (Some(c), None) = (chars.next(), chars.next()) {
                resolve_char(c)
            } else {
                None
            }
        }
    }
}

/// 字母 → evdev 码（QWERTY 物理布局顺序，非字母序！）
fn letter_code(c: u8) -> Option<u32> {
    use evdev::*;
    Some(match c {
        b'q' => KEY_Q,
        b'w' => KEY_W,
        b'e' => KEY_E,
        b'r' => KEY_R,
        b't' => KEY_T,
        b'y' => KEY_Y,
        b'u' => KEY_U,
        b'i' => KEY_I,
        b'o' => KEY_O,
        b'p' => KEY_P,
        b'a' => KEY_A,
        b's' => KEY_S,
        b'd' => KEY_D,
        b'f' => KEY_F,
        b'g' => KEY_G,
        b'h' => KEY_H,
        b'j' => KEY_J,
        b'k' => KEY_K,
        b'l' => KEY_L,
        b'z' => KEY_Z,
        b'x' => KEY_X,
        b'c' => KEY_C,
        b'v' => KEY_V,
        b'b' => KEY_B,
        b'n' => KEY_N,
        b'm' => KEY_M,
        _ => return None,
    })
}

/// 单字符 → 解析（US 布局可打印区；返回的 mods 只含 Shift）。
pub fn resolve_char(c: char) -> Option<ResolvedKey> {
    use evdev::*;
    let plain = |evdev: u32| Some(ResolvedKey { mods: Vec::new(), evdev });
    let shifted = |evdev: u32| Some(ResolvedKey { mods: vec![Modifier::Shift], evdev });
    match c {
        'a'..='z' => letter_code(c as u8).and_then(plain),
        'A'..='Z' => letter_code(c.to_ascii_lowercase() as u8).and_then(shifted),
        '1'..='9' => plain(KEY_1 + (c as u32 - '1' as u32)),
        '0' => plain(KEY_0),
        ' ' => plain(KEY_SPACE),
        '`' => plain(KEY_GRAVE),
        '-' => plain(KEY_MINUS),
        '=' => plain(KEY_EQUAL),
        '[' => plain(KEY_LEFTBRACE),
        ']' => plain(KEY_RIGHTBRACE),
        '\\' => plain(KEY_BACKSLASH),
        ';' => plain(KEY_SEMICOLON),
        '\'' => plain(KEY_APOSTROPHE),
        ',' => plain(KEY_COMMA),
        '.' => plain(KEY_DOT),
        '/' => plain(KEY_SLASH),
        '~' => shifted(KEY_GRAVE),
        '!' => shifted(KEY_1),
        '@' => shifted(KEY_2),
        '#' => shifted(KEY_3),
        '$' => shifted(KEY_4),
        '%' => shifted(KEY_5),
        '^' => shifted(KEY_6),
        '&' => shifted(KEY_7),
        '*' => shifted(KEY_8),
        '(' => shifted(KEY_9),
        ')' => shifted(KEY_0),
        '_' => shifted(KEY_MINUS),
        '+' => shifted(KEY_EQUAL),
        '{' => shifted(KEY_LEFTBRACE),
        '}' => shifted(KEY_RIGHTBRACE),
        '|' => shifted(KEY_BACKSLASH),
        ':' => shifted(KEY_SEMICOLON),
        '"' => shifted(KEY_APOSTROPHE),
        '<' => shifted(KEY_COMMA),
        '>' => shifted(KEY_DOT),
        '?' => shifted(KEY_SLASH),
        '\t' => plain(KEY_TAB),
        '\n' | '\r' => plain(KEY_ENTER),
        _ => None, // 非 US 可打印区（CJK 等）：v1 不支持
    }
}

/// 鼠标按钮名/编号 → evdev 按钮码。
pub fn resolve_button(name: &str) -> Option<u32> {
    match name.to_ascii_lowercase().as_str() {
        "left" | "0" => Some(0x110),     // BTN_LEFT
        "right" | "1" => Some(0x111),    // BTN_RIGHT
        "middle" | "2" => Some(0x112),   // BTN_MIDDLE
        "back" | "3" => Some(0x113),     // BTN_SIDE
        "forward" | "4" => Some(0x114),  // BTN_EXTRA
        other => other.parse::<u32>().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names() {
        assert_eq!(resolve_key_name("Return").unwrap().evdev, 28);
        assert_eq!(resolve_key_name("ESC").unwrap().evdev, 1);
        assert_eq!(resolve_key_name("F5").unwrap().evdev, 63);
        assert_eq!(resolve_key_name("Left").unwrap().evdev, 105);
        assert_eq!(resolve_key_name("ctrl").unwrap().evdev, 29);
        assert_eq!(resolve_key_name("a").unwrap().evdev, 30);
    }

    #[test]
    fn chars() {
        assert_eq!(resolve_char('h').unwrap().evdev, 35);
        assert!(resolve_char('h').unwrap().mods.is_empty());
        assert_eq!(resolve_char('H').unwrap().evdev, 35);
        assert_eq!(resolve_char('H').unwrap().mods, vec![Modifier::Shift]);
        assert_eq!(resolve_char('!').unwrap().evdev, 2); // shift+1
        assert_eq!(resolve_char(' ').unwrap().evdev, 57);
        assert!(resolve_char('中').is_none(), "CJK v1 不支持");
    }

    #[test]
    fn buttons() {
        assert_eq!(resolve_button("left"), Some(0x110));
        assert_eq!(resolve_button("middle"), Some(0x112));
        assert_eq!(resolve_button("275"), Some(275));
        assert_eq!(resolve_button("nope"), None);
    }
}
