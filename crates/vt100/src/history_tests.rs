//! Tests for addressing lines of history plus screen.

use crate::Parser;
use core::num::NonZeroU16;

fn term(rows: u16, cols: u16, back: usize) -> Parser {
    Parser::new(NonZeroU16::new(rows).unwrap(), NonZeroU16::new(cols).unwrap(), back)
}

fn lines(p: &mut Parser, from: usize, to: usize) {
    for i in from..=to {
        p.process(format!("l{i}\r\n").as_bytes());
    }
}

#[test]
fn lines_count_history_then_screen() {
    let mut p = term(3, 10, 5);
    lines(&mut p, 1, 4);
    let s = p.screen();
    // l1 l2 went up; the screen holds l3 l4 and the cursor's empty row.
    assert_eq!((s.scrollback_len(), s.lines_scrolled(), s.line_count()), (2, 2, 5));
    let all: Vec<String> = (0..s.line_count()).map(|i| s.line_text(i, 0, 10).unwrap()).collect();
    assert_eq!(all, ["l1", "l2", "l3", "l4", ""]);
    assert_eq!(s.line_text(5, 0, 10), None);
    assert_eq!(s.line_text(0, 1, 2).as_deref(), Some("1"));
}

#[test]
fn the_count_keeps_going_when_the_history_is_full() {
    let mut p = term(3, 10, 2);
    lines(&mut p, 1, 10);
    let s = p.screen();
    assert_eq!((s.scrollback_len(), s.lines_scrolled()), (2, 8));
    // Line id 7 (l8) is index lines_scrolled - len + i.
    let id = 7;
    let i = id - (s.lines_scrolled() - s.scrollback_len());
    assert_eq!(s.line_text(i, 0, 10).as_deref(), Some("l8"));
}

#[test]
fn wrapped_lines_say_so() {
    let mut p = term(3, 4, 5);
    p.process(b"abcdefg\r\nx\r\n\r\n\r\n");
    let s = p.screen();
    assert!(s.line_wrapped(0));
    assert!(!s.line_wrapped(1));
    assert_eq!(s.line_text(1, 0, 4).as_deref(), Some("efg"));
}
