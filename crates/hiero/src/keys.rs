//! Turn crossterm key events back into the bytes a terminal would send,
//! for typing into an agent from inside the board.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton};
use vt100::MouseProtocolEncoding;

/// `app_cursor`: the program asked for application cursor keys (DECCKM).
pub fn encode(key: KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let m = key.modifiers;
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let alt = m.contains(KeyModifiers::ALT);
    let shift = m.contains(KeyModifiers::SHIFT);

    // xterm modifier parameter: 1 + shift + 2·alt + 4·ctrl.
    let modp = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;

    let csi_letter = |letter: char| -> Vec<u8> {
        if modp > 1 {
            format!("\x1b[1;{modp}{letter}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[{letter}").into_bytes()
        }
    };
    let csi_tilde = |n: u8| -> Vec<u8> {
        if modp > 1 {
            format!("\x1b[{n};{modp}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };

    let bytes = match key.code {
        KeyCode::Char(c) => {
            let mut out = Vec::new();
            if alt {
                out.push(0x1b);
            }
            if ctrl {
                out.push(ctrl_byte(c)?);
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            out
        }
        KeyCode::Enter => with_alt(alt, b"\r"),
        KeyCode::Tab => with_alt(alt, b"\t"),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => {
            if ctrl {
                with_alt(alt, b"\x08")
            } else {
                with_alt(alt, b"\x7f")
            }
        }
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Up => csi_letter('A'),
        KeyCode::Down => csi_letter('B'),
        KeyCode::Right => csi_letter('C'),
        KeyCode::Left => csi_letter('D'),
        KeyCode::Home => csi_letter('H'),
        KeyCode::End => csi_letter('F'),
        KeyCode::Insert => csi_tilde(2),
        KeyCode::Delete => csi_tilde(3),
        KeyCode::PageUp => csi_tilde(5),
        KeyCode::PageDown => csi_tilde(6),
        KeyCode::F(n) => match n {
            1..=4 => {
                let letter = b"PQRS"[n as usize - 1] as char;
                if modp > 1 {
                    format!("\x1b[1;{modp}{letter}").into_bytes()
                } else {
                    format!("\x1bO{letter}").into_bytes()
                }
            }
            5 => csi_tilde(15),
            6 => csi_tilde(17),
            7 => csi_tilde(18),
            8 => csi_tilde(19),
            9 => csi_tilde(20),
            10 => csi_tilde(21),
            11 => csi_tilde(23),
            12 => csi_tilde(24),
            _ => return None,
        },
        _ => return None,
    };
    Some(bytes)
}

fn with_alt(alt: bool, b: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(b.len() + 1);
    if alt {
        v.push(0x1b);
    }
    v.extend_from_slice(b);
    v
}

fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        ' ' | '@' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '-' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

/// The keys that leave an attached view: ctrl+\ (like hc) and ctrl+].
pub fn is_detach(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('\\') | KeyCode::Char('4') | KeyCode::Char(']') | KeyCode::Char('5'))
}

/// Find a detach key in raw input bytes: 0x1c / 0x1d, or their kitty-protocol
/// forms (`CSI 92;5u`, `CSI 93;5u`) when the program turned that on.
/// Returns the bytes before it.
pub fn find_detach(bytes: &[u8]) -> Option<usize> {
    if let Some(i) = bytes.iter().position(|&b| b == 0x1c || b == 0x1d) {
        return Some(i);
    }
    for pat in [b"\x1b[92;5u".as_slice(), b"\x1b[93;5u".as_slice()] {
        if let Some(i) = bytes.windows(pat.len()).position(|w| w == pat) {
            return Some(i);
        }
    }
    None
}

/// What the pointer did, for [`encode_mouse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAct {
    Press(MouseButton),
    Release(MouseButton),
    /// Moved with a button held.
    Drag(MouseButton),
    /// Moved with no button held.
    Move,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}

/// The bytes that report a pointer event to a program that turned mouse
/// reporting on. `x` and `y` are 0-based cells inside the pane (1-based on
/// the wire). `None` when the encoding can't express the position (the
/// default one stops at column/row 223, UTF-8 at 2015).
pub fn encode_mouse(act: MouseAct, m: KeyModifiers, x: u16, y: u16, enc: MouseProtocolEncoding) -> Option<Vec<u8>> {
    let base = |b: MouseButton| match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let sgr = enc == MouseProtocolEncoding::Sgr;
    let (code, release) = match act {
        MouseAct::Press(b) => (base(b), false),
        // Only SGR says which button let go; the others just say "released".
        MouseAct::Release(b) => (if sgr { base(b) } else { 3 }, true),
        MouseAct::Drag(b) => (base(b) + 32, false),
        MouseAct::Move => (3 + 32, false),
        MouseAct::WheelUp => (64, false),
        MouseAct::WheelDown => (65, false),
        MouseAct::WheelLeft => (66, false),
        MouseAct::WheelRight => (67, false),
    };
    let code = code
        + if m.contains(KeyModifiers::SHIFT) { 4 } else { 0 }
        + if m.contains(KeyModifiers::ALT) { 8 } else { 0 }
        + if m.contains(KeyModifiers::CONTROL) { 16 } else { 0 };
    let (x, y) = (x as u32 + 1, y as u32 + 1);
    match enc {
        MouseProtocolEncoding::Sgr => Some(format!("\x1b[<{code};{x};{y}{}", if release { 'm' } else { 'M' }).into_bytes()),
        MouseProtocolEncoding::Default => {
            if x > 223 || y > 223 {
                return None;
            }
            Some(vec![0x1b, b'[', b'M', 32 + code, 32 + x as u8, 32 + y as u8])
        }
        MouseProtocolEncoding::Utf8 => {
            if x > 2015 || y > 2015 {
                return None;
            }
            let mut out = vec![0x1b, b'[', b'M'];
            for v in [code as u32, x, y] {
                let c = char::from_u32(32 + v)?;
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            Some(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn encodes_common_keys() {
        let none = KeyModifiers::NONE;
        assert_eq!(encode(k(KeyCode::Char('a'), none), false).unwrap(), b"a");
        assert_eq!(encode(k(KeyCode::Char('é'), none), false).unwrap(), "é".as_bytes());
        assert_eq!(encode(k(KeyCode::Char('c'), KeyModifiers::CONTROL), false).unwrap(), b"\x03");
        assert_eq!(encode(k(KeyCode::Char('x'), KeyModifiers::ALT), false).unwrap(), b"\x1bx");
        assert_eq!(encode(k(KeyCode::Enter, none), false).unwrap(), b"\r");
        assert_eq!(encode(k(KeyCode::Backspace, none), false).unwrap(), b"\x7f");
        assert_eq!(encode(k(KeyCode::Up, none), false).unwrap(), b"\x1b[A");
        assert_eq!(encode(k(KeyCode::Up, none), true).unwrap(), b"\x1bOA");
        assert_eq!(encode(k(KeyCode::Right, KeyModifiers::CONTROL), false).unwrap(), b"\x1b[1;5C");
        assert_eq!(encode(k(KeyCode::Delete, none), false).unwrap(), b"\x1b[3~");
        assert_eq!(encode(k(KeyCode::F(5), none), false).unwrap(), b"\x1b[15~");
    }

    #[test]
    fn finds_detach() {
        assert_eq!(find_detach(b"abc\x1cdef"), Some(3));
        assert_eq!(find_detach(b"x\x1b[92;5u"), Some(1));
        assert_eq!(find_detach(b"plain"), None);
        assert!(is_detach(&k(KeyCode::Char('\\'), KeyModifiers::CONTROL)));
    }

    fn mouse(act: MouseAct, m: KeyModifiers, x: u16, y: u16, enc: MouseProtocolEncoding) -> Option<Vec<u8>> {
        encode_mouse(act, m, x, y, enc)
    }

    #[test]
    fn sgr_mouse() {
        use MouseProtocolEncoding::Sgr;
        let n = KeyModifiers::NONE;
        let l = MouseButton::Left;
        assert_eq!(mouse(MouseAct::Press(l), n, 0, 0, Sgr).unwrap(), b"\x1b[<0;1;1M");
        assert_eq!(mouse(MouseAct::Release(l), n, 49, 9, Sgr).unwrap(), b"\x1b[<0;50;10m");
        assert_eq!(mouse(MouseAct::Press(MouseButton::Middle), n, 1, 2, Sgr).unwrap(), b"\x1b[<1;2;3M");
        assert_eq!(mouse(MouseAct::Release(MouseButton::Right), n, 1, 2, Sgr).unwrap(), b"\x1b[<2;2;3m");
        assert_eq!(mouse(MouseAct::Drag(l), n, 51, 9, Sgr).unwrap(), b"\x1b[<32;52;10M");
        assert_eq!(mouse(MouseAct::Move, n, 0, 0, Sgr).unwrap(), b"\x1b[<35;1;1M");
        assert_eq!(mouse(MouseAct::WheelUp, n, 4, 4, Sgr).unwrap(), b"\x1b[<64;5;5M");
        assert_eq!(mouse(MouseAct::WheelDown, n, 4, 4, Sgr).unwrap(), b"\x1b[<65;5;5M");
        assert_eq!(mouse(MouseAct::WheelLeft, n, 4, 4, Sgr).unwrap(), b"\x1b[<66;5;5M");
        assert_eq!(mouse(MouseAct::WheelRight, n, 4, 4, Sgr).unwrap(), b"\x1b[<67;5;5M");
        // Large positions are fine in SGR.
        assert_eq!(mouse(MouseAct::Press(l), n, 999, 499, Sgr).unwrap(), b"\x1b[<0;1000;500M");
    }

    #[test]
    fn mouse_modifiers_add_to_the_button_code() {
        use MouseProtocolEncoding::Sgr;
        let l = MouseButton::Left;
        let m = |m| mouse(MouseAct::Press(l), m, 0, 0, Sgr).unwrap();
        assert_eq!(m(KeyModifiers::SHIFT), b"\x1b[<4;1;1M");
        assert_eq!(m(KeyModifiers::ALT), b"\x1b[<8;1;1M");
        assert_eq!(m(KeyModifiers::CONTROL), b"\x1b[<16;1;1M");
        assert_eq!(m(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL), b"\x1b[<28;1;1M");
        let drag = mouse(MouseAct::Drag(l), KeyModifiers::CONTROL, 0, 0, Sgr).unwrap();
        assert_eq!(drag, b"\x1b[<48;1;1M");
        let wheel = mouse(MouseAct::WheelUp, KeyModifiers::SHIFT, 0, 0, Sgr).unwrap();
        assert_eq!(wheel, b"\x1b[<68;1;1M");
    }

    #[test]
    fn x10_mouse() {
        use MouseProtocolEncoding::Default;
        let n = KeyModifiers::NONE;
        let l = MouseButton::Left;
        assert_eq!(mouse(MouseAct::Press(l), n, 0, 0, Default).unwrap(), [0x1b, b'[', b'M', 32, 33, 33]);
        // A release has no button: code 3.
        assert_eq!(mouse(MouseAct::Release(l), n, 9, 4, Default).unwrap(), [0x1b, b'[', b'M', 35, 42, 37]);
        assert_eq!(mouse(MouseAct::Press(MouseButton::Right), n, 0, 0, Default).unwrap()[3], 34);
        assert_eq!(mouse(MouseAct::Drag(l), n, 0, 0, Default).unwrap()[3], 64);
        assert_eq!(mouse(MouseAct::WheelDown, n, 0, 0, Default).unwrap()[3], 97);
        assert_eq!(mouse(MouseAct::Press(l), KeyModifiers::CONTROL, 0, 0, Default).unwrap()[3], 48);
        // The last expressible cell is 223 (1-based): 32 + 223 = 255.
        assert_eq!(mouse(MouseAct::Press(l), n, 222, 222, Default).unwrap(), [0x1b, b'[', b'M', 32, 255, 255]);
        assert_eq!(mouse(MouseAct::Press(l), n, 223, 0, Default), None);
        assert_eq!(mouse(MouseAct::Press(l), n, 0, 223, Default), None);
    }

    #[test]
    fn utf8_mouse() {
        use MouseProtocolEncoding::Utf8;
        let n = KeyModifiers::NONE;
        let l = MouseButton::Left;
        assert_eq!(mouse(MouseAct::Press(l), n, 0, 0, Utf8).unwrap(), [0x1b, b'[', b'M', 32, 33, 33]);
        // Column 100 (1-based) is 132: two bytes in UTF-8.
        let b = mouse(MouseAct::Press(l), n, 99, 0, Utf8).unwrap();
        assert_eq!(b, [&[0x1b, b'[', b'M', 32][..], "\u{84}".as_bytes(), &[33]].concat());
        // Beyond 223 it still works, up to 2015.
        let b = mouse(MouseAct::Press(l), n, 299, 0, Utf8).unwrap();
        assert_eq!(&b[4..], ["\u{14c}".as_bytes(), &[33]].concat());
        assert!(mouse(MouseAct::Press(l), n, 2014, 0, Utf8).is_some());
        assert_eq!(mouse(MouseAct::Press(l), n, 2015, 0, Utf8), None);
    }
}
