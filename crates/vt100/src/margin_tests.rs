//! Tests for DECLRMM / DECSLRM (left and right margins).

use crate::Parser;
use core::num::NonZeroU16;

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap()
}

fn term(rows: u16, cols: u16) -> Parser {
    Parser::new(nz(rows), nz(cols), 0)
}

/// Fills row `r` with the letter 'A' + r across all columns.
fn fill(p: &mut Parser, rows: u16, cols: u16) {
    for r in 0..rows {
        let letter = char::from(b'A' + u8::try_from(r).unwrap());
        let line: String = core::iter::repeat_n(letter, cols.into()).collect();
        p.process(format!("\x1b[{};1H{line}", r + 1).as_bytes());
    }
}

fn rows(p: &Parser) -> Vec<String> {
    p.screen().rows(0, p.screen().size().1.get()).collect()
}

/// 5x10 screen filled with A..E, DECLRMM on and margins at columns 3..6
/// (1-based), cursor homed.
fn margined() -> Parser {
    let mut p = term(5, 10);
    fill(&mut p, 5, 10);
    p.process(b"\x1b[?69h\x1b[3;6s");
    p
}

#[test]
fn su_scrolls_only_margin_columns() {
    let mut p = margined();
    p.process(b"\x1b[S");
    assert_eq!(
        rows(&p),
        ["AABBBBAAAA", "BBCCCCBBBB", "CCDDDDCCCC", "DDEEEEDDDD", "EE    EEEE"]
    );
}

#[test]
fn sd_scrolls_only_margin_columns() {
    let mut p = margined();
    p.process(b"\x1b[T");
    assert_eq!(
        rows(&p),
        ["AA    AAAA", "BBAAAABBBB", "CCBBBBCCCC", "DDCCCCDDDD", "EEDDDDEEEE"]
    );
}

#[test]
fn su_respects_top_and_bottom_margins_too() {
    let mut p = margined();
    p.process(b"\x1b[2;4r\x1b[S");
    assert_eq!(
        rows(&p),
        ["AAAAAAAAAA", "BBCCCCBBBB", "CCDDDDCCCC", "DD    DDDD", "EEEEEEEEEE"]
    );
}

#[test]
fn lf_at_bottom_scrolls_margin_columns() {
    let mut p = term(3, 10);
    fill(&mut p, 3, 10);
    p.process(b"\x1b[?69h\x1b[3;6s\x1b[3;3H\n");
    assert_eq!(rows(&p), ["AABBBBAAAA", "BBCCCCBBBB", "CC    CCCC"]);
    assert_eq!(p.screen().cursor_position(), (2, 2));
}

#[test]
fn lf_outside_margins_does_not_scroll() {
    let mut p = term(3, 10);
    fill(&mut p, 3, 10);
    p.process(b"\x1b[?69h\x1b[3;6s\x1b[3;1H\n");
    assert_eq!(rows(&p), ["AAAAAAAAAA", "BBBBBBBBBB", "CCCCCCCCCC"]);
}

#[test]
fn ri_at_top_scrolls_margin_columns() {
    let mut p = term(3, 10);
    fill(&mut p, 3, 10);
    p.process(b"\x1b[?69h\x1b[3;6s\x1b[1;3H\x1bM");
    assert_eq!(rows(&p), ["AA    AAAA", "BBAAAABBBB", "CCBBBBCCCC"]);
}

#[test]
fn ri_outside_margins_does_not_scroll() {
    let mut p = term(3, 10);
    fill(&mut p, 3, 10);
    p.process(b"\x1b[?69h\x1b[3;6s\x1b[1;1H\x1bM");
    assert_eq!(rows(&p), ["AAAAAAAAAA", "BBBBBBBBBB", "CCCCCCCCCC"]);
}

#[test]
fn nel_goes_to_left_margin_and_index_scrolls() {
    // NEL is not parsed separately by vt100 (ESC E is unhandled); CR + LF
    // is the equivalent.
    let mut p = term(3, 10);
    fill(&mut p, 3, 10);
    p.process(b"\x1b[?69h\x1b[3;6s\x1b[3;5H\r\n");
    assert_eq!(rows(&p), ["AABBBBAAAA", "BBCCCCBBBB", "CC    CCCC"]);
    assert_eq!(p.screen().cursor_position(), (2, 2));
}

#[test]
fn il_shifts_margin_columns_down() {
    let mut p = margined();
    p.process(b"\x1b[2;3H\x1b[L");
    assert_eq!(
        rows(&p),
        ["AAAAAAAAAA", "BB    BBBB", "CCBBBBCCCC", "DDCCCCDDDD", "EEDDDDEEEE"]
    );
    assert_eq!(p.screen().cursor_position(), (1, 2));
}

#[test]
fn dl_shifts_margin_columns_up() {
    let mut p = margined();
    p.process(b"\x1b[2;3H\x1b[M");
    assert_eq!(
        rows(&p),
        ["AAAAAAAAAA", "BBCCCCBBBB", "CCDDDDCCCC", "DDEEEEDDDD", "EE    EEEE"]
    );
}

#[test]
fn il_dl_outside_margins_do_nothing() {
    let mut p = margined();
    p.process(b"\x1b[2;1H\x1b[L\x1b[M\x1b[2;9H\x1b[L\x1b[M");
    assert_eq!(
        rows(&p),
        ["AAAAAAAAAA", "BBBBBBBBBB", "CCCCCCCCCC", "DDDDDDDDDD", "EEEEEEEEEE"]
    );
}

fn digits() -> Parser {
    let mut p = term(2, 10);
    p.process(b"\x1b[H0123456789\x1b[?69h\x1b[3;6s");
    p
}

#[test]
fn ich_stops_at_right_margin() {
    let mut p = digits();
    p.process(b"\x1b[1;3H\x1b[@");
    assert_eq!(rows(&p)[0], "01 2346789");
    p.process(b"\x1b[1;4H\x1b[2@");
    assert_eq!(rows(&p)[0], "01   26789");
}

#[test]
fn dch_stops_at_right_margin() {
    let mut p = digits();
    p.process(b"\x1b[1;3H\x1b[P");
    assert_eq!(rows(&p)[0], "01345 6789");
}

#[test]
fn ich_dch_outside_margins_do_nothing() {
    let mut p = digits();
    p.process(b"\x1b[1;8H\x1b[@\x1b[P\x1b[1;2H\x1b[@\x1b[P");
    assert_eq!(rows(&p)[0], "0123456789");
}

#[test]
fn ech_ignores_margins() {
    let mut p = digits();
    p.process(b"\x1b[1;5H\x1b[10X");
    assert_eq!(rows(&p)[0], "0123");
}

#[test]
fn text_wraps_at_right_margin_to_left_margin() {
    let mut p = term(4, 10);
    p.process(b"\x1b[?69h\x1b[3;6s\x1b[1;3Habcdef");
    assert_eq!(rows(&p), ["  abcd", "  ef", "", ""]);
    assert_eq!(p.screen().cursor_position(), (1, 4));
    assert!(!p.screen().row_wrapped(0));
}

#[test]
fn text_wrap_at_bottom_margin_scrolls_margin_columns() {
    let mut p = term(3, 10);
    fill(&mut p, 3, 10);
    p.process(b"\x1b[?69h\x1b[3;6s\x1b[3;3Hwxyzv");
    assert_eq!(rows(&p), ["AABBBBAAAA", "BBwxyzBBBB", "CCv   CCCC"]);
}

#[test]
fn text_right_of_right_margin_runs_to_the_screen_edge() {
    let mut p = term(2, 10);
    p.process(b"\x1b[?69h\x1b[3;6s\x1b[1;8Habcd");
    assert_eq!(rows(&p), ["       abc", "d"]);
}

#[test]
fn wide_char_that_does_not_fit_wraps() {
    let mut p = term(3, 10);
    p.process("\x1b[?69h\x1b[3;6s\x1b[1;6H\u{3042}".as_bytes());
    assert_eq!(rows(&p), ["", "  \u{3042}", ""]);
}

#[test]
fn wide_char_cut_by_scroll_is_cleared() {
    let mut p = term(3, 10);
    // cols 6-7 (1-based): a wide char straddling the margin set below
    p.process("\x1b[2;6H\u{3042}".as_bytes());
    p.process(b"\x1b[?69h\x1b[3;6s\x1b[S");
    let screen = p.screen();
    assert!(!screen.cell(0, 5).unwrap().is_wide());
    assert!(!screen.cell(1, 6).unwrap().is_wide_continuation());
    assert_eq!(screen.contents(), "");
}

#[test]
fn cr_goes_to_left_margin() {
    let mut p = margined();
    p.process(b"\x1b[1;5H\r");
    assert_eq!(p.screen().cursor_position(), (0, 2));
    p.process(b"\x1b[1;2H\r");
    assert_eq!(p.screen().cursor_position(), (0, 0));
    p.process(b"\x1b[1;8H\r");
    assert_eq!(p.screen().cursor_position(), (0, 2));
}

#[test]
fn cursor_forward_and_back_clamp_at_margins() {
    let mut p = margined();
    p.process(b"\x1b[1;3H\x1b[20C");
    assert_eq!(p.screen().cursor_position(), (0, 5));
    p.process(b"\x1b[20D");
    assert_eq!(p.screen().cursor_position(), (0, 2));
    // outside the margins, the screen edge applies
    p.process(b"\x1b[1;8H\x1b[20C");
    assert_eq!(p.screen().cursor_position(), (0, 9));
    p.process(b"\x1b[1;2H\x1b[20C");
    assert_eq!(p.screen().cursor_position(), (0, 5));
    p.process(b"\x1b[1;1H\x1b[20D");
    assert_eq!(p.screen().cursor_position(), (0, 0));
}

#[test]
fn decslrm_homes_the_cursor() {
    let mut p = term(5, 10);
    p.process(b"\x1b[?69h\x1b[4;5H\x1b[3;6s");
    assert_eq!(p.screen().cursor_position(), (0, 0));
}

#[test]
fn decslrm_in_origin_mode_homes_to_the_left_margin() {
    let mut p = term(5, 10);
    p.process(b"\x1b[?69h\x1b[?6h\x1b[3;6s\x1b[1;1Hx\x1b[1;9Hy");
    assert_eq!(rows(&p)[0], "  x  y");
    assert_eq!(p.screen().cursor_position(), (0, 6));
}

#[test]
fn invalid_margins_are_ignored() {
    let mut p = term(3, 10);
    fill(&mut p, 3, 10);
    p.process(b"\x1b[?69h\x1b[6;3s\x1b[S");
    assert_eq!(rows(&p), ["BBBBBBBBBB", "CCCCCCCCCC", ""]);
}

#[test]
fn margins_default_to_the_full_width() {
    let mut p = term(3, 10);
    fill(&mut p, 3, 10);
    p.process(b"\x1b[?69h\x1b[3;6s\x1b[s\x1b[S");
    assert_eq!(rows(&p), ["BBBBBBBBBB", "CCCCCCCCCC", ""]);
}

#[test]
fn declrmm_off_resets_margins() {
    let mut p = term(3, 10);
    fill(&mut p, 3, 10);
    p.process(b"\x1b[?69h\x1b[3;6s\x1b[?69l\x1b[S");
    assert_eq!(rows(&p), ["BBBBBBBBBB", "CCCCCCCCCC", ""]);
    // and DECSLRM is not honoured while the mode is off
    let mut p = term(3, 10);
    fill(&mut p, 3, 10);
    p.process(b"\x1b[3;6s\x1b[S");
    assert_eq!(rows(&p), ["BBBBBBBBBB", "CCCCCCCCCC", ""]);
}

#[test]
fn csi_s_saves_the_cursor_when_declrmm_is_off() {
    let mut p = term(5, 10);
    p.process(b"\x1b[3;4H\x1b[s\x1b[1;1H\x1b8");
    assert_eq!(p.screen().cursor_position(), (2, 3));
}

#[test]
fn resize_resets_margins() {
    let mut p = term(3, 10);
    fill(&mut p, 3, 10);
    p.process(b"\x1b[?69h\x1b[3;6s");
    p.set_size(nz(3), nz(12));
    p.process(b"\x1b[S");
    let r = rows(&p);
    assert_eq!(r[0], "BBBBBBBBBB");
    assert_eq!(r[1], "CCCCCCCCCC");
    // DECLRMM is off again, so DECSLRM is SCOSC
    p.process(b"\x1b[3;6s\x1b[S");
    assert_eq!(rows(&p)[0], "CCCCCCCCCC");
}

#[test]
fn partial_margin_scroll_does_not_touch_scrollback() {
    let mut p = Parser::new(nz(3), nz(10), 10);
    fill(&mut p, 3, 10);
    p.process(b"\x1b[?69h\x1b[3;6s\x1b[S");
    assert_eq!(p.screen().scrollback_len(), 0);
}

#[derive(Default)]
struct Unhandled(Vec<String>);

impl crate::Callbacks for Unhandled {
    fn unhandled_csi(
        &mut self,
        _: &mut crate::Screen,
        i1: Option<u8>,
        _: Option<u8>,
        _: &[&[u16]],
        c: char,
    ) {
        self.0.push(format!("{i1:?}{c}"));
    }
}

#[test]
fn margin_sequences_are_handled() {
    let mut p = Parser::new_with_callbacks(
        nz(3),
        nz(10),
        0,
        Unhandled::default(),
    );
    p.process(b"\x1b[?69h\x1b[1;5s\x1b[?69l\x1b[s");
    assert_eq!(p.callbacks().0, Vec::<String>::new());
}

#[test]
fn in_band_resize_mode_follows_decset_2048() {
    let mut p = term(5, 10);
    assert!(!p.screen().in_band_resize());
    p.process(b"\x1b[?2004;2048h");
    assert!(p.screen().in_band_resize());
    p.process(b"\x1b[?2048l");
    assert!(!p.screen().in_band_resize());
}
