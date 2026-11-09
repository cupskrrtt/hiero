//! Turn crossterm key events back into the bytes a terminal would send,
//! for typing into an agent from inside the board.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
}
