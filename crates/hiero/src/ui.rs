//! Drawing. Reads `App`, never changes it.

use crate::app::{App, Entry, Info, Mode, fmt_dur};
use hc_client::AgentState;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let banner = app.disconnected.is_some() as u16;
    let [top, banner_area, body, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(banner),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_top(f, app, top);
    if let Some(err) = &app.disconnected {
        let msg = if app.connected_once {
            format!(" ✗ lost herdr-core: {err} — reconnecting… ")
        } else {
            format!(" ✗ can't reach herdr-core: {err} — retrying… ")
        };
        f.render_widget(Paragraph::new(msg).style(Style::new().bg(Color::Red).fg(Color::White)), banner_area);
    }

    // Side by side when wide, stacked when narrow.
    let (list_area, preview_area) = if body.width >= 110 {
        let [l, r] = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body);
        (l, r)
    } else {
        let [l, r] = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body);
        (l, r)
    };
    draw_list(f, app, list_area);
    draw_preview(f, app, preview_area);
    draw_bottom(f, app, bottom);

    if app.mode == Mode::Help {
        draw_help(f, area);
    }
}

fn draw_top(f: &mut Frame, app: &App, area: Rect) {
    let live = app.set.terminals.values().filter(|t| t.is_live()).count();
    let waiting = app.attention_count();
    let mut spans = vec![
        Span::styled(" hiero ", Style::new().bold().fg(Color::Black).bg(Color::Cyan)),
        Span::raw(" "),
    ];
    if !app.core_name.is_empty() {
        spans.push(Span::styled(format!("core {} · ", app.core_name), Style::new().dim()));
    }
    spans.push(Span::raw(format!("{live} live")));
    if waiting > 0 {
        spans.push(Span::raw(" · "));
        spans.push(Span::styled(
            format!("{waiting} need{} you", if waiting == 1 { "s" } else { "" }),
            Style::new().fg(Color::Magenta).bold(),
        ));
    }
    let right = Span::styled("? help ", Style::new().dim());
    let [l, r] = Layout::horizontal([Constraint::Min(0), Constraint::Length(right.width() as u16)]).areas(area);
    f.render_widget(Line::from(spans), l);
    f.render_widget(right, r);
}

fn state_style(i: &Info) -> (String, Style) {
    if !i.live {
        let style = match i.attention {
            Some(a) if a.word() == "failed" => Style::new().fg(Color::Red),
            _ => Style::new().dim(),
        };
        return ("■".into(), style);
    }
    match i.state {
        AgentState::Working => ("●".into(), Style::new().fg(Color::Yellow)),
        AgentState::Blocked => ("◐".into(), Style::new().fg(Color::Magenta).bold()),
        AgentState::Done => ("✓".into(), Style::new().fg(Color::Green).bold()),
        AgentState::Error => ("✗".into(), Style::new().fg(Color::Red).bold()),
        AgentState::Idle => ("○".into(), Style::new().fg(Color::Blue)),
        AgentState::Unknown => ("·".into(), Style::new().dim()),
    }
}

fn draw_list(f: &mut Frame, app: &App, area: Rect) {
    let mut rows = Vec::new();
    let mut selected_row = None;
    for e in &app.entries {
        match e {
            Entry::Header { title, count, attention } => {
                let style = if *attention {
                    Style::new().fg(Color::Magenta).bold()
                } else {
                    Style::new().fg(Color::Cyan).bold()
                };
                let label = if *attention { format!("▲ {} ({count})", title.to_uppercase()) } else { format!("{title} ({count})") };
                rows.push(Row::new(vec![Cell::from(""), Cell::from(Span::styled(label, style))]).height(1));
            }
            Entry::Item(i) => {
                if app.selected.as_ref() == Some(&i.addr) {
                    selected_row = Some(rows.len());
                }
                let (mut sym, sym_style) = state_style(i);
                if i.live && i.state == AgentState::Working {
                    sym = SPINNER[(app.tick as usize) % SPINNER.len()].into();
                }
                let when = match i.wait_ms {
                    Some(w) => Span::styled(format!("⏱ {}", fmt_dur(w)), Style::new().fg(Color::Magenta)),
                    None => Span::styled(fmt_dur(i.up_ms), Style::new().dim()),
                };
                let where_ = match &i.branch {
                    Some(b) => format!("{}  {}", i.project, b),
                    None => i.project.clone(),
                };
                let name_style = if i.live { Style::new().bold() } else { Style::new().dim() };
                rows.push(Row::new(vec![
                    Cell::from(Span::styled(sym, sym_style)),
                    Cell::from(Span::styled(i.name.clone(), name_style)),
                    Cell::from(Span::styled(i.agent.clone(), Style::new().fg(Color::Cyan))),
                    Cell::from(Span::styled(i.status.clone(), sym_style)),
                    Cell::from(Span::styled(where_, Style::new().dim())),
                    Cell::from(when),
                    Cell::from(Span::styled(i.title.clone(), Style::new().dim())),
                ]));
            }
        }
    }

    let header = Row::new(["", "NAME", "AGENT", "STATUS", "PROJECT  BRANCH", "TIME", "TITLE"])
        .style(Style::new().dim().add_modifier(Modifier::UNDERLINED));
    let widths = [
        Constraint::Length(1),
        Constraint::Length(16),
        Constraint::Length(10),
        Constraint::Length(12),
        Constraint::Length(22),
        Constraint::Length(7),
        Constraint::Min(8),
    ];
    let title = if app.query.is_empty() {
        " agents ".to_string()
    } else {
        format!(" agents · /{} ", app.query)
    };
    let block = Block::new().borders(Borders::ALL).border_type(BorderType::Rounded).title(title);

    if app.entries.is_empty() {
        let msg = if app.disconnected.is_some() && !app.connected_once {
            "waiting for herdr-core…"
        } else if !app.query.is_empty() {
            "nothing matches — esc clears the search"
        } else {
            "no terminals yet — start one with `hc run --bg <agent>`"
        };
        f.render_widget(Paragraph::new(msg).dim().block(block), area);
        return;
    }

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(Style::new().bg(Color::DarkGray))
        .highlight_symbol("▌");
    let mut state = TableState::default().with_selected(selected_row);
    f.render_stateful_widget(table, area, &mut state);
}

fn draw_preview(f: &mut Frame, app: &App, area: Rect) {
    let Some(info) = app.selected_info() else {
        f.render_widget(Block::new().borders(Borders::ALL).border_type(BorderType::Rounded).title(" preview "), area);
        return;
    };

    let mut title = vec![
        Span::raw(" "),
        Span::styled(info.name.clone(), Style::new().bold()),
        Span::styled(format!(" {} · {}", info.addr, info.agent), Style::new().dim()),
    ];
    if let Some(b) = &info.branch {
        title.push(Span::styled(format!(" ·  {b}"), Style::new().fg(Color::Green)));
    }
    title.push(Span::raw(" "));
    let mut bottom = format!(" {} · turn {} ", shorten_home(&info.dir), info.turn);
    if app.preview.scroll > 0 {
        bottom = format!(" ↑{} {}", app.preview.scroll, bottom);
    }
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Line::from(title))
        .title_bottom(Line::from(Span::styled(bottom, Style::new().dim())));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Status message, if the agent gave one.
    let [msg_area, text_area] = Layout::vertical([
        Constraint::Length((!info.status_msg.is_empty()) as u16),
        Constraint::Min(0),
    ])
    .areas(inner);
    if !info.status_msg.is_empty() {
        let (_, style) = state_style(info);
        f.render_widget(Paragraph::new(format!("» {}", info.status_msg)).style(style), msg_area);
    }

    let p = &app.preview;
    if p.addr.as_deref() != Some(info.addr.as_str()) {
        f.render_widget(Paragraph::new("loading…").dim(), text_area);
        return;
    }
    if let Some(err) = &p.error {
        f.render_widget(Paragraph::new(format!("can't read screen: {err}")).red(), text_area);
        return;
    }
    // Show the bottom of the screen, scrolled up by `scroll` lines.
    let lines: Vec<&str> = p.text.lines().collect();
    let h = text_area.height as usize;
    let max_scroll = lines.len().saturating_sub(h);
    let scroll = (p.scroll as usize).min(max_scroll);
    let start = lines.len().saturating_sub(h + scroll);
    let shown: Vec<Line> = lines[start..lines.len() - scroll].iter().map(|l| Line::raw(*l)).collect();
    f.render_widget(Paragraph::new(shown), text_area);
}

fn draw_bottom(f: &mut Frame, app: &App, area: Rect) {
    let line = match app.mode {
        Mode::Search => Line::from(vec![
            Span::styled(" / ", Style::new().fg(Color::Black).bg(Color::Yellow)),
            Span::raw(format!(" {}", app.query)),
            Span::styled("█", Style::new().fg(Color::Yellow)),
            Span::styled("   enter keep · esc clear", Style::new().dim()),
        ]),
        _ => {
            if let Some((msg, _)) = &app.flash {
                Line::from(Span::styled(format!(" {msg}"), Style::new().fg(Color::Yellow)))
            } else {
                let keys = [
                    ("j/k", "move"),
                    ("n", "next waiting"),
                    ("x", "dismiss"),
                    ("/", "search"),
                    ("p", "group"),
                    ("f", "finished"),
                    ("J/K", "scroll"),
                    ("q", "quit"),
                ];
                let mut spans = vec![Span::raw(" ")];
                for (k, v) in keys {
                    spans.push(Span::styled(k, Style::new().fg(Color::Cyan)));
                    spans.push(Span::styled(format!(" {v}  "), Style::new().dim()));
                }
                Line::from(spans)
            }
        }
    };
    f.render_widget(line, area);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let text = vec![
        Line::from("Move".bold()),
        Line::from("  j / k, ↑ / ↓     select"),
        Line::from("  g / G            first / last"),
        Line::from("  n, tab           next agent that needs you"),
        Line::from(""),
        Line::from("Board".bold()),
        Line::from("  /                search (fuzzy)    esc  clear"),
        Line::from("  p                group by project on/off"),
        Line::from("  f                show finished on/off"),
        Line::from("  x                dismiss from NEEDS YOU"),
        Line::from(""),
        Line::from("Preview".bold()),
        Line::from("  J / K, PgDn/PgUp scroll"),
        Line::from("  r                refresh"),
        Line::from(""),
        Line::from("  q, ctrl+c        quit"),
        Line::from(""),
        Line::from(Span::styled("status: ● working  ◐ blocked  ✓ done  ✗ error  ○ idle  · none  ■ finished", Style::new().dim())),
        Line::from(Span::styled("press any key to close", Style::new().dim())),
    ];
    let w = 76.min(area.width.saturating_sub(4));
    let h = (text.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h);
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(Block::new().borders(Borders::ALL).border_type(BorderType::Rounded).title(" keys ")),
        popup,
    );
}

fn shorten_home(p: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && p.starts_with(&h) => format!("~{}", &p[h.len()..]),
        _ => p.to_string(),
    }
}
