//! Drawing the workspace screen: sidebar, pane area, bottom bar, dialogs.

use crate::ui::{draw_input, draw_vt, popup};
use crate::workspace::WsStatus;
use crate::ws_app::{Dialog, Focus, Form, Row, WsApp};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

const SIDEBAR_W: u16 = 26;

struct Areas {
    banner: Rect,
    sidebar: Rect,
    work: Rect,
    bottom: Rect,
}

fn areas(area: Rect, banner: bool) -> Areas {
    let [banner_row, body, bottom] =
        Layout::vertical([Constraint::Length(banner as u16), Constraint::Min(3), Constraint::Length(1)]).areas(area);
    let sb = if area.width >= 60 { SIDEBAR_W } else { area.width / 3 };
    let [sidebar, work] = Layout::horizontal([Constraint::Length(sb), Constraint::Min(1)]).areas(body);
    Areas { banner: banner_row, sidebar, work, bottom }
}

/// The cells the pane's program gets, for a screen of `area`.
pub fn pane_inner(area: Rect, banner: bool) -> Rect {
    let inner = Block::new().borders(Borders::ALL).inner(areas(area, banner).work);
    Rect::new(0, 0, inner.width.max(1), inner.height.max(1))
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
    let a = areas(f.area(), app.disconnected.is_some());
    if let Some(err) = &app.disconnected {
        let msg = if app.connected_once {
            format!(" ✗ lost herdr-core: {err} — reconnecting… ")
        } else {
            format!(" ✗ can't reach herdr-core: {err} — is it running? (hc server status) ")
        };
        f.render_widget(Paragraph::new(msg).style(Style::new().bg(Color::Red).fg(Color::White).bold()), a.banner);
    }
    draw_sidebar(f, app, a.sidebar);
    draw_work(f, app, a.work);
    draw_bottom(f, app, a.bottom);
    match &app.dialog {
        Some(Dialog::Help) => draw_help(f, f.area(), &app.prefix.label()),
        Some(Dialog::Form(form)) => draw_form(f, form, f.area()),
        Some(Dialog::Confirm { message, .. }) => {
            let w = (message.chars().count() as u16 + 4).min(f.area().width.saturating_sub(4)).max(30);
            let h = 2 + (message.chars().count() as u16 / w.saturating_sub(2).max(1));
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
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = app.rows();
    if rows.is_empty() {
        f.render_widget(Paragraph::new("no projects yet\n\nA adds one").dim().wrap(Wrap { trim: false }), inner);
        return;
    }
    let cursor = rows.iter().position(|r| r.id() == app.sel).unwrap_or(0);
    let h = inner.height as usize;
    let top = (cursor + 1).saturating_sub(h);
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

fn draw_work(f: &mut Frame, app: &WsApp, area: Rect) {
    let focused = app.focus == Focus::Workspace && app.dialog.is_none();
    let mut title = vec![Span::raw(" ")];
    if let Some((p, w, tab, panes)) = app.open_title() {
        title.push(Span::styled(format!("{p} › {w} › {tab}"), Style::new().bold()));
        if panes > 1 {
            title.push(Span::styled(format!(" (1 of {panes} panes)"), Style::new().dim()));
        }
        if let Some(view) = &app.pane {
            title.push(Span::styled(format!(" · {}", view.addr), Style::new().dim()));
        }
        title.push(Span::raw(" "));
    } else {
        title.push(Span::styled("no workspace open ", Style::new().dim()));
    }
    let mut color = border(focused);
    if let Some(why) = app.pane.as_ref().and_then(|p| p.ended.as_ref()) {
        title.push(Span::styled(format!("· ended: {why} "), Style::new().fg(Color::Red)));
        color = Style::new().fg(Color::Red);
    }
    let mut block = Block::new()
        .borders(Borders::ALL)
        .border_type(if focused { BorderType::Thick } else { BorderType::Plain })
        .border_style(color)
        .title(Line::from(title));
    if app.pane.as_ref().is_some_and(|p| p.ended.is_some()) {
        block = block.title_bottom(Line::from(Span::styled(" enter: start a new shell here ", Style::new().dim())));
    }
    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Some(view) = &app.pane {
        draw_vt(f, &view.parser, inner, focused && view.ended.is_none());
        return;
    }
    let label = app.prefix.label();
    let text = if let Some(err) = &app.pane_error {
        vec![Line::from(Span::styled(format!("can't open this workspace: {err}"), Style::new().fg(Color::Red))), Line::from(""), Line::from("enter tries again").dim()]
    } else if app.open.is_some() {
        vec![Line::from("opening…").dim()]
    } else if app.doc.projects.is_empty() {
        vec![Line::from("no projects yet"), Line::from(format!("press {label} N to add one")).dim()]
    } else {
        vec![Line::from("no workspace open"), Line::from(format!("press {label} w to pick a workspace")).dim()]
    };
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}

fn draw_bottom(f: &mut Frame, app: &WsApp, area: Rect) {
    let label = app.prefix.label();
    let mut spans: Vec<Span> = Vec::new();
    if app.armed {
        spans.push(Span::styled(" prefix ", Style::new().bg(Color::Yellow).fg(Color::Black).bold()));
        spans.push(Span::raw(" w sidebar · n new workspace · N new project · ? help · q quit · esc cancel"));
    } else if let Some((msg, error)) = app.current_flash() {
        let style = if error { Style::new().fg(Color::Red) } else { Style::new().fg(Color::Green) };
        spans.push(Span::styled(format!(" {msg}"), style));
    } else if app.focus == Focus::Sidebar {
        spans.push(Span::styled(
            " j/k move · enter open · a add · A project · r rename · D delete · J/K reorder · esc back",
            Style::new().dim(),
        ));
    } else {
        spans.push(Span::styled(format!(" {label}"), Style::new().bold()));
        spans.push(Span::styled(" then  w sidebar · n new · ? help · q quit", Style::new().dim()));
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
    let h = form.fields.len() as u16 * 2 + 3;
    let inner = popup(f, area, 64, h + 2, &form.title);
    let mut constraints: Vec<Constraint> = form.fields.iter().flat_map(|_| [Constraint::Length(1), Constraint::Length(1)]).collect();
    constraints.extend([Constraint::Length(1), Constraint::Min(1)]);
    let rows = Layout::vertical(constraints).split(inner);
    for (i, field) in form.fields.iter().enumerate() {
        let [label, input] = Layout::horizontal([Constraint::Length(8), Constraint::Min(1)]).areas(rows[i * 2]);
        let focused = i == form.focus;
        let style = if focused { Style::new().bold() } else { Style::new().dim() };
        f.render_widget(Paragraph::new(field.label).style(style), label);
        draw_input(f, input, &field.input, focused);
    }
    let n = form.fields.len() * 2;
    if let Some(e) = &form.error {
        f.render_widget(Paragraph::new(e.clone()).red().wrap(Wrap { trim: false }), rows[n]);
    }
    f.render_widget(Paragraph::new("tab next · enter ok · esc cancel").dim(), rows[n + 1]);
}

fn draw_help(f: &mut Frame, area: Rect, prefix: &str) {
    let text = vec![
        Line::from(format!("Prefix: {prefix}, then").bold()),
        Line::from("  w        sidebar / back to the workspace"),
        Line::from("  n        new workspace in this project"),
        Line::from("  N        new project"),
        Line::from("  ?        this help"),
        Line::from("  q        quit Hiero (terminals keep running)"),
        Line::from(format!("  {prefix}   send the prefix key itself to the program")),
        Line::from("  esc      cancel"),
        Line::from(""),
        Line::from("Sidebar".bold()),
        Line::from("  j k, ↑ ↓   move              enter   open workspace / fold project"),
        Line::from("  a          add workspace     A       add project"),
        Line::from("  r          rename            D       delete (terminals keep running)"),
        Line::from("  J K        move down / up    z       fold project"),
        Line::from("  esc, l, tab  back to the workspace          q   quit"),
        Line::from(""),
        Line::from(Span::styled("status: ● working  ◐ needs you  ✓ done  ○ idle  · no terminal", Style::new().dim())),
        Line::from(Span::styled("press any key to close", Style::new().dim())),
    ];
    let w = 76.min(area.width.saturating_sub(4));
    let h = (text.len() as u16 + 2).min(area.height.saturating_sub(2));
    let r = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(Block::new().borders(Borders::ALL).border_type(BorderType::Rounded).title(" keys ")),
        r,
    );
}
