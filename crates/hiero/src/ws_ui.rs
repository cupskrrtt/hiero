//! Drawing the workspace screen: sidebar, pane area, bottom bar, dialogs.

use crate::widgets::{draw_input, draw_multiline, draw_vt, popup};
use crate::workspace::{Workspace, WsStatus};
use crate::ws_app::{Dialog, Focus, Form, Row, WsApp};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

/// Default sidebar width, and the range `<` `>` keep it in.
pub const SIDEBAR_W: u16 = 26;
pub const SIDEBAR_MIN: u16 = 16;
pub const SIDEBAR_MAX: u16 = 60;

pub struct Areas {
    pub banner: Rect,
    pub sidebar: Rect,
    /// The tabs bar, one line.
    pub tabs: Rect,
    /// Where the panes of the active tab go.
    pub panes: Rect,
    pub bottom: Rect,
}

/// `sidebar` is the wanted width in columns; 0 hides it. On a narrow screen
/// it gives way so the workspace keeps at least half.
pub fn areas(area: Rect, banner: bool, sidebar: u16) -> Areas {
    let [banner_row, body, bottom] =
        Layout::vertical([Constraint::Length(banner as u16), Constraint::Min(3), Constraint::Length(1)]).areas(area);
    let sb = if sidebar == 0 { 0 } else { sidebar.min(area.width / 2) };
    let [sidebar, work] = Layout::horizontal([Constraint::Length(sb), Constraint::Min(1)]).areas(body);
    let [tabs, panes] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(work);
    Areas { banner: banner_row, sidebar, tabs, panes, bottom }
}

/// The rows of the sidebar list (inside its border).
pub fn sidebar_inner(area: Rect) -> Rect {
    Block::new().borders(Borders::ALL).inner(area)
}

/// The first row shown: the list scrolls just enough to keep the cursor in view.
pub fn sidebar_top(cursor: usize, height: usize) -> usize {
    (cursor + 1).saturating_sub(height)
}

/// The label of each tab in the tabs bar.
fn tab_label(i: usize, name: &str) -> String {
    format!(" {} {} ", i + 1, name)
}

/// Where each tab sits in the tabs bar: (first column, width), relative to
/// the bar's left edge. One blank cell leads the bar and one follows each tab.
pub fn tab_cells(w: &Workspace) -> Vec<(u16, u16)> {
    let mut x = 1u16;
    w.tabs
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let width = Span::raw(tab_label(i, &t.name)).width() as u16;
            let cell = (x, width);
            x = x.saturating_add(width + 1);
            cell
        })
        .collect()
}

fn dot(s: WsStatus) -> Span<'static> {
    match s {
        WsStatus::Attention => Span::styled("◐", Style::new().fg(Color::Red).bold()),
        WsStatus::Working => Span::styled("●", Style::new().fg(Color::Yellow)),
        WsStatus::Done => Span::styled("✓", Style::new().fg(Color::Green)),
        WsStatus::Idle => Span::styled("○", Style::new().dim()),
        WsStatus::None => Span::styled("·", Style::new().dim()),
    }
}

pub fn draw(f: &mut Frame, app: &WsApp) {
    let a = areas(f.area(), app.disconnected.is_some(), app.sidebar_cols());
    if let Some(err) = &app.disconnected {
        let msg = if app.connected_once {
            format!(" ✗ lost herdr-core: {err} — reconnecting… ")
        } else {
            format!(" ✗ can't reach herdr-core: {err} — is it running? (hc server status) ")
        };
        f.render_widget(Paragraph::new(msg).style(Style::new().bg(Color::Red).fg(Color::White).bold()), a.banner);
    }
    if a.sidebar.width > 0 {
        draw_sidebar(f, app, a.sidebar);
    }
    draw_tabs(f, app, a.tabs);
    draw_panes(f, app, a.panes);
    draw_bottom(f, app, a.bottom);
    match &app.dialog {
        Some(Dialog::Help(top)) => draw_help(f, f.area(), &app.prefix.label(), *top),
        Some(Dialog::Form(form)) => draw_form(f, form, f.area()),
        Some(Dialog::Confirm { message, .. }) => {
            let longest = message.lines().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
            let w = (longest + 4).min(f.area().width.saturating_sub(4)).max(30);
            let rows: usize = message.lines().map(|l| 1 + l.chars().count() / w.saturating_sub(2).max(1) as usize).sum();
            let h = 1 + rows as u16;
            let inner = popup(f, f.area(), w, h + 1, "confirm");
            f.render_widget(Paragraph::new(message.clone()).wrap(Wrap { trim: false }).yellow(), inner);
        }
        None => {}
    }
}

fn border(focused: bool) -> Style {
    Style::new().fg(if focused { Color::Green } else { Color::DarkGray })
}

fn draw_sidebar(f: &mut Frame, app: &WsApp, area: Rect) {
    let focused = app.focus == Focus::Sidebar && app.dialog.is_none();
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(if focused { BorderType::Thick } else { BorderType::Plain })
        .border_style(border(focused))
        .title(Span::styled(" projects ", Style::new().bold()));
    let inner = sidebar_inner(area);
    f.render_widget(block, area);

    let rows = app.rows();
    if rows.is_empty() {
        f.render_widget(Paragraph::new("no projects yet\n\nA adds one").dim().wrap(Wrap { trim: false }), inner);
        return;
    }
    let cursor = rows.iter().position(|r| r.id() == app.sel).unwrap_or(0);
    let h = inner.height as usize;
    let top = sidebar_top(cursor, h);
    for (i, row) in rows.iter().enumerate().skip(top).take(h) {
        let line = match row {
            Row::Project(id) => {
                let Some(p) = app.doc.project(id) else { continue };
                let mut spans =
                    vec![Span::styled(if p.collapsed { "▸ " } else { "▾ " }, Style::new().dim()), Span::styled(p.name.clone(), Style::new().bold())];
                if p.collapsed {
                    spans.push(Span::raw(" "));
                    spans.push(dot(app.project_status(id)));
                }
                Line::from(spans)
            }
            Row::Workspace(id) => {
                let Some((_, w)) = app.doc.workspace(id) else { continue };
                let open = app.open.as_deref() == Some(id.as_str());
                let mark = if open { Span::styled("▶ ", Style::new().fg(Color::Green)) } else { Span::raw("  ") };
                let name = if open { Span::styled(w.name.clone(), Style::new().bold()) } else { Span::raw(w.name.clone()) };
                Line::from(vec![Span::raw("  "), mark, dot(app.ws_status(id)), Span::raw(" "), name])
            }
        };
        let y = inner.y + (i - top) as u16;
        let r = Rect::new(inner.x, y, inner.width, 1);
        let style = if i == cursor && focused { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
        if i == cursor && focused {
            f.render_widget(Clear, r);
        }
        f.render_widget(Paragraph::new(line).style(style), r);
    }
}

/// The tab names in order; the active one is highlighted. No status here:
/// the sidebar has the dots.
fn draw_tabs(f: &mut Frame, app: &WsApp, area: Rect) {
    let Some((w, active)) = app.active() else {
        f.render_widget(Paragraph::new(" no workspace open").dim(), area);
        return;
    };
    let mut spans = vec![Span::raw(" ")];
    for (i, t) in w.tabs.iter().enumerate() {
        let label = tab_label(i, &t.name);
        let style = if t.id == active.id {
            Style::new().bg(Color::Green).fg(Color::Black).bold()
        } else {
            Style::new().dim()
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
    }
    if app.zoomed() {
        spans.push(Span::styled(" zoom ", Style::new().fg(Color::Yellow)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Every pane of the active tab in its own bordered box.
fn draw_panes(f: &mut Frame, app: &WsApp, area: Rect) {
    let rects = app.pane_rects();
    if rects.is_empty() {
        let label = app.prefix.label();
        let text = if app.doc.projects.is_empty() {
            vec![Line::from("no projects yet"), Line::from(format!("press {label} N to add one")).dim()]
        } else {
            vec![Line::from("no workspace open"), Line::from(format!("press {label} w to pick a workspace")).dim()]
        };
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
        return;
    }
    let focused_id = app.focused_pane();
    let in_workspace = app.focus == Focus::Workspace && app.dialog.is_none();
    for (id, r) in rects {
        let is_focused = focused_id.as_deref() == Some(id.as_str());
        draw_pane(f, app, &id, r, is_focused, in_workspace);
    }
}

fn draw_pane(f: &mut Frame, app: &WsApp, id: &str, r: Rect, is_focused: bool, in_workspace: bool) {
    let Some((_, _, pane)) = app.doc.pane(id) else { return };
    let slot = app.slots.get(id);
    let view = slot.and_then(|s| s.view.as_ref());
    let ended = view.and_then(|v| v.ended.as_ref());

    let mut title = vec![Span::styled(
        format!(" {} ", pane.name.as_deref().or(pane.command.as_deref()).unwrap_or("shell")),
        if is_focused { Style::new().bold() } else { Style::new() },
    )];
    if let Some(v) = view {
        title.push(Span::styled(format!("{} ", v.addr), Style::new().dim()));
    }
    let mut color = if is_focused {
        Style::new().fg(if in_workspace { Color::Green } else { Color::LightGreen })
    } else {
        Style::new().fg(Color::DarkGray)
    };
    let copy = app.copy_of(id);
    if let (Some(c), Some(v)) = (copy, view) {
        let s = v.parser.screen();
        let up = c.lines_up(s);
        let what = if c.anchor.is_some() { "copy · selecting" } else { "copy" };
        title.push(Span::styled(
            format!(" {what} · {up}/{} up ", s.scrollback_len()),
            Style::new().bg(Color::Yellow).fg(Color::Black).bold(),
        ));
        title.push(Span::raw(" "));
    }
    if let Some(why) = ended {
        title.push(Span::styled(format!("· ended: {why} "), Style::new().fg(Color::Red)));
        color = Style::new().fg(Color::Red);
    }
    let mut block = Block::new()
        .borders(Borders::ALL)
        .border_type(if is_focused && in_workspace { BorderType::Thick } else { BorderType::Plain })
        .border_style(color)
        .title(Line::from(title));
    let idle = slot.is_some_and(|s| s.idle);
    if is_focused && (ended.is_some() || idle || slot.is_some_and(|s| s.error.is_some())) {
        let hint = if ended.is_some() {
            " enter: start a new shell here "
        } else if idle {
            " enter: start it "
        } else {
            " enter: try again "
        };
        block = block.title_bottom(Line::from(Span::styled(hint, Style::new().dim())));
    }
    let inner = block.inner(r);
    f.render_widget(block, r);

    if let Some(view) = view {
        draw_vt(f, &view.parser, inner, copy.is_none() && is_focused && in_workspace && ended.is_none());
        if let Some(c) = copy {
            draw_copy(f, c, view.parser.screen(), inner, in_workspace);
        }
        return;
    }
    let text = match slot.and_then(|s| s.error.as_ref()) {
        Some(err) => Line::from(Span::styled(format!("can't open this pane: {err}"), Style::new().fg(Color::Red))),
        None if slot.is_some_and(|s| s.idle) => Line::from("not running — Enter starts it").dim(),
        None => Line::from("opening…").dim(),
    };
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}

/// Copy mode over a pane: the selection reversed, the cursor on its cell.
fn draw_copy(f: &mut Frame, c: &crate::copy::Copy, s: &vt100::Screen, inner: Rect, show_cursor: bool) {
    let buf = f.buffer_mut();
    for r in 0..inner.height {
        let Some((from, to)) = c.selected(s, r) else { continue };
        for col in from..=to.min(inner.width.saturating_sub(1)) {
            if let Some(cell) = buf.cell_mut((inner.x + col, inner.y + r)) {
                cell.set_style(Style::new().bg(Color::Rgb(70, 90, 140)).fg(Color::White));
            }
        }
    }
    if let Some((col, r)) = c.cursor_cell(s)
        && col < inner.width
        && r < inner.height
    {
        let pos = (inner.x + col, inner.y + r);
        if let Some(cell) = buf.cell_mut(pos) {
            cell.set_style(Style::new().bg(Color::Yellow).fg(Color::Black));
        }
        if show_cursor && c.prompt.is_none() {
            f.set_cursor_position(pos);
        }
    }
}

fn draw_bottom(f: &mut Frame, app: &WsApp, area: Rect) {
    let label = app.prefix.label();
    let mut spans: Vec<Span> = Vec::new();
    if app.armed {
        spans.push(Span::styled(" prefix ", Style::new().bg(Color::Yellow).fg(Color::Black).bold()));
        spans.push(Span::raw(
            " t/T tab · v/s split (V/S ask) · hjkl move · HJKL size · 1-9 [ ] tab · c copy · r/R rename tab/pane · e env · x/X close · z zoom · w sidebar · b hide · < > size · ? help",
        ));
    } else if let Some(p) = app.copy.as_ref().and_then(|c| c.prompt.as_ref()) {
        let [label, input] = Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(area);
        f.render_widget(Paragraph::new(if p.forward { " /" } else { " ?" }).bold(), label);
        draw_input(f, input, &p.input, true);
        return;
    } else if let Some((msg, error)) = app.current_flash() {
        let style = if error { Style::new().fg(Color::Red) } else { Style::new().fg(Color::Green) };
        spans.push(Span::styled(format!(" {msg}"), style));
    } else if app.copy.is_some() {
        spans.push(Span::styled(" copy ", Style::new().bg(Color::Yellow).fg(Color::Black).bold()));
        spans.push(Span::styled(
            " hjkl w b move · ^u ^d g G scroll · v/V select · y copy · / ? search · n N next · q quit",
            Style::new().dim(),
        ));
    } else if app.focus == Focus::Sidebar {
        spans.push(Span::styled(
            format!(" j/k move · enter open · a add · A project · r rename · e env · D delete · J/K reorder · esc back · {label}"),
            Style::new().dim(),
        ));
    } else {
        spans.push(Span::styled(format!(" {label}"), Style::new().bold()));
        spans.push(Span::styled(" then  t tab · v s split · hjkl move · z zoom · c copy · ? help · q quit", Style::new().dim()));
    }
    let right = app.attention().map(|(p, w)| Span::styled(format!("◐ {p} › {w} needs you "), Style::new().fg(Color::Red).bold()));
    let rw = right.as_ref().map(|s| s.width() as u16).unwrap_or(0);
    let [left, rarea] = Layout::horizontal([Constraint::Min(1), Constraint::Length(rw.min(area.width / 2))]).areas(area);
    f.render_widget(Paragraph::new(Line::from(spans)), left);
    if let Some(r) = right {
        f.render_widget(Paragraph::new(Line::from(r)), rarea);
    }
}

fn draw_form(f: &mut Frame, form: &Form, area: Rect) {
    // A field of several lines gets a block of rows.
    let height = |f: &crate::ws_app::Field| if f.input.multiline { 10u16.min(area.height.saturating_sub(9)).max(3) } else { 1 };
    let h: u16 = form.fields.iter().map(|f| height(f) + 1).sum::<u16>() + 3;
    let inner = popup(f, area, 64, h + 2, &form.title);
    let mut constraints: Vec<Constraint> =
        form.fields.iter().flat_map(|fl| [Constraint::Length(height(fl)), Constraint::Length(1)]).collect();
    constraints.extend([Constraint::Length(1), Constraint::Min(1)]);
    let rows = Layout::vertical(constraints).split(inner);
    for (i, field) in form.fields.iter().enumerate() {
        let [label, input] = Layout::horizontal([Constraint::Length(8), Constraint::Min(1)]).areas(rows[i * 2]);
        let focused = i == form.focus;
        let style = if focused { Style::new().bold() } else { Style::new().dim() };
        f.render_widget(Paragraph::new(field.label).style(style), label);
        if field.input.multiline {
            draw_multiline(f, input, &field.input, focused);
        } else {
            draw_input(f, input, &field.input, focused);
        }
    }
    let n = form.fields.len() * 2;
    if let Some(e) = &form.error {
        f.render_widget(Paragraph::new(e.clone()).red().wrap(Wrap { trim: false }), rows[n]);
    }
    let hint = if form.fields.iter().any(|f| f.input.multiline) {
        "KEY=VALUE per line · # comment · enter new line · ctrl+s save · esc cancel"
    } else {
        "tab next · enter ok · esc cancel"
    };
    f.render_widget(Paragraph::new(hint).dim(), rows[n + 1]);
}

/// The help text. It is taller than a small screen, so it scrolls.
pub fn help_lines(prefix: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(format!("Prefix: {prefix}, then").bold()),
        Line::from("  t / T      new tab (T asks for a command first)"),
        Line::from("  v / s      split side by side / stacked (V / S ask for a command)"),
        Line::from("  h j k l    move to the pane left / below / above / right (arrows too)"),
        Line::from("  H J K L    narrower / taller / shorter / wider"),
        Line::from("  1 .. 9     go to tab N        [ ]   previous / next tab"),
        Line::from("  r          rename the tab"),
        Line::from("  R          rename the pane (empty: show what it runs)"),
        Line::from("  e          edit this project's env vars (offers to restart its terminals)"),
        Line::from("  x / X      close the pane / the tab (ends its programs)"),
        Line::from("  z          zoom the pane to the whole area, again to undo"),
        Line::from("  c, PgUp    copy mode: scroll back through the pane, select and copy"),
        Line::from("  w          sidebar / back to the workspace (shows a hidden sidebar)"),
        Line::from("  b          hide or show the sidebar"),
        Line::from("  < / >      narrower / wider sidebar"),
        Line::from("  n / N      new workspace in this project / new project"),
        Line::from("  ?          this help        q   quit Hiero (terminals keep running)"),
        Line::from(format!("  {prefix}   send the prefix key itself to the program")),
        Line::from("  esc        cancel"),
        Line::from(""),
        Line::from("Copy mode (the wheel over a shell starts it too)".bold()),
        Line::from("  h j k l    move the cursor    w b     next / previous word"),
        Line::from("  0 ^ $      line start / first text / end      H M L   top / middle / bottom"),
        Line::from("  ^u ^d      half a page        ^b ^f, PgUp PgDn   a page   ^y ^e  a line"),
        Line::from("  g / G      oldest line / back to the bottom"),
        Line::from("  v / V      select characters / whole lines (again: stop)"),
        Line::from("  y, enter   copy the selection (or the line) and leave"),
        Line::from("  / ?        search down / up (smart case)   n N   next / previous"),
        Line::from("  esc        drop the selection, then leave   q   leave"),
        Line::from("  mouse      click places the cursor, drag selects"),
        Line::from(""),
        Line::from("Sidebar".bold()),
        Line::from("  j k, ↑ ↓   move              enter   open workspace / fold project"),
        Line::from("  a          add workspace     A       add project"),
        Line::from("  r          rename            D       delete (asks about its terminals)"),
        Line::from("  e          edit the project's env vars (ctrl+s saves)"),
        Line::from("  J K        move down / up    z       fold project"),
        Line::from("  esc, l, tab  back to the workspace          q   quit"),
        Line::from(""),
        Line::from(Span::styled("status: ● working  ◐ needs you  ✓ done  ○ idle  · no terminal", Style::new().dim())),
    ]
}

fn draw_help(f: &mut Frame, area: Rect, prefix: &str, top: usize) {
    let text = help_lines(prefix);
    let w = 78.min(area.width.saturating_sub(4));
    let h = (text.len() as u16 + 2).min(area.height.saturating_sub(2));
    let r = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h);
    let visible = h.saturating_sub(2) as usize;
    let scrolls = text.len() > visible;
    let top = top.min(text.len().saturating_sub(visible));
    let title = if scrolls { " keys · ↑↓ scroll · any other key closes " } else { " keys · any key closes " };
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(text)
            .scroll((top as u16, 0))
            .block(Block::new().borders(Borders::ALL).border_type(BorderType::Rounded).title(title)),
        r,
    );
}
