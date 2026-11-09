//! Drawing. Reads `App`, never changes it.

use crate::app::{App, Entry, Info, Mode, PaneView, fmt_dur, shorten_home};
use crate::forms::{InputPurpose, SpawnField, SpawnForm, TextInput};
use hc_client::AgentState;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Top bar, banner, body, bottom bar.
fn frame_layout(area: Rect, banner: bool) -> [Rect; 4] {
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(banner as u16),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(area)
}

/// The cells an in-board attached agent gets, for a screen of `area`.
pub fn pane_inner(area: Rect, banner: bool) -> Rect {
    let [_, _, body, _] = frame_layout(area, banner);
    Block::new().borders(Borders::ALL).inner(body)
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let [top, banner_area, body, bottom] = frame_layout(area, app.disconnected.is_some());

    draw_top(f, app, top);
    if let Some(err) = &app.disconnected {
        let msg = if app.connected_once {
            format!(" ✗ lost herdr-core: {err} — reconnecting… ")
        } else {
            format!(" ✗ can't reach herdr-core: {err} — retrying… ")
        };
        f.render_widget(Paragraph::new(msg).style(Style::new().bg(Color::Red).fg(Color::White)), banner_area);
    }

    if let (Mode::Pane, Some(pane)) = (&app.mode, &app.pane) {
        draw_pane(f, pane, body);
        draw_bottom(f, app, bottom);
        return;
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

    match &app.mode {
        Mode::Help => draw_help(f, area),
        Mode::Spawn(form) => draw_spawn(f, form, area),
        Mode::Input(d) => {
            let mut hint = d.hint.clone();
            if matches!(d.purpose, InputPurpose::Send(_)) && d.force {
                hint = format!("FORCE ON · {hint}");
            }
            draw_prompt(f, area, &d.title, &d.input, &hint);
        }
        Mode::Confirm(c) => draw_confirm(f, area, &c.message),
        Mode::Templates(v) => draw_templates(f, area, v),
        Mode::Tasks(v) => draw_tasks(f, app, area, v.selected),
        Mode::Review(v) => draw_review(f, app, area, v),
        Mode::Pipelines(v) => draw_pipelines(f, app, area, v),
        Mode::Launch(form) => draw_launch(f, area, form),
        _ => {}
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
                    Cell::from(Span::styled(
                        if i.depth > 0 { format!("{}└ {}", "  ".repeat(i.depth as usize - 1), i.name) } else { i.name.clone() },
                        name_style,
                    )),
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
    if let Some(task) = &info.task {
        title.push(Span::styled(format!(" · task: {task}"), Style::new().fg(Color::Magenta)));
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
    if let Some(fl) = &app.flash {
        let style = if fl.error { Style::new().fg(Color::Red).bold() } else { Style::new().fg(Color::Yellow) };
        f.render_widget(Line::from(Span::styled(format!(" {}", fl.msg), style)), area);
        return;
    }
    let line = match &app.mode {
        Mode::Search => Line::from(vec![
            Span::styled(" / ", Style::new().fg(Color::Black).bg(Color::Yellow)),
            Span::raw(format!(" {}", app.query)),
            Span::styled("█", Style::new().fg(Color::Yellow)),
            Span::styled("   enter keep · esc clear", Style::new().dim()),
        ]),
        Mode::Pane => hints(&[("ctrl+\\", "back to board"), ("", "every other key goes to the agent")]),
        _ => hints(&[
            ("c", "new"),
            ("s", "send"),
            ("a", "attach"),
            ("o", "open here"),
            ("i", "interrupt"),
            ("K", "kill"),
            ("n", "next waiting"),
            ("x", "dismiss"),
            ("/", "search"),
            ("v", "review"),
            ("T", "tasks"),
            ("w", "pipelines"),
            ("B", "broadcast"),
            ("?", "all keys"),
            ("q", "quit"),
        ]),
    };
    f.render_widget(line, area);
}

fn hints(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (k, v) in keys {
        spans.push(Span::styled(k.to_string(), Style::new().fg(Color::Cyan)));
        spans.push(Span::styled(format!(" {v}  "), Style::new().dim()));
    }
    Line::from(spans)
}

/// A centered box of `w`×`h` cells, cleared.
fn popup(f: &mut Frame, area: Rect, w: u16, h: u16, title: &str) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    let r = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h);
    f.render_widget(Clear, r);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Cyan))
        .title(Span::styled(format!(" {title} "), Style::new().bold()));
    let inner = block.inner(r);
    f.render_widget(block, r);
    inner
}

/// One text field; puts the cursor in it when focused.
fn draw_input(f: &mut Frame, area: Rect, input: &TextInput, focused: bool) {
    let style = if focused { Style::new().bg(Color::Rgb(40, 44, 52)) } else { Style::new().dim() };
    let text = input.value();
    // Scroll horizontally so the cursor stays visible.
    let before: String = text.chars().take(input.cursor()).collect();
    let before_w = Span::raw(before.as_str()).width() as u16;
    let skip = before_w.saturating_sub(area.width.saturating_sub(1));
    f.render_widget(Paragraph::new(text).style(style).scroll((0, skip)), area);
    if focused {
        f.set_cursor_position((area.x + before_w - skip, area.y));
    }
}

fn draw_spawn(f: &mut Frame, form: &SpawnForm, area: Rect) {
    let inner = popup(f, area, 78, 16, "new agent");
    let rows = Layout::vertical([
        Constraint::Length(1), // agent
        Constraint::Length(1), // agent detail
        Constraint::Length(1),
        Constraint::Length(1), // folder
        Constraint::Length(1), // worktree
        Constraint::Length(1),
        Constraint::Length(1), // task
        Constraint::Length(1),
        Constraint::Length(1), // name
        Constraint::Length(1),
        Constraint::Length(1), // error
        Constraint::Min(1),    // hint
    ])
    .split(inner);
    let label = |f: &mut Frame, r: Rect, text: &str, focused: bool| -> Rect {
        let [l, v] = Layout::horizontal([Constraint::Length(9), Constraint::Min(1)]).areas(r);
        let style = if focused { Style::new().fg(Color::Cyan).bold() } else { Style::new().dim() };
        f.render_widget(Span::styled(text.to_string(), style), l);
        v
    };

    let focused = form.field == SpawnField::Agent;
    let v = label(f, rows[0], "agent", focused);
    let mut spans = Vec::new();
    for (i, a) in form.agents.iter().enumerate() {
        let style = if i == form.agent {
            if focused { Style::new().fg(Color::Black).bg(Color::Cyan).bold() } else { Style::new().bold().underlined() }
        } else {
            Style::new().dim()
        };
        spans.push(Span::styled(format!(" {} ", a.label), style));
        spans.push(Span::raw(" "));
    }
    f.render_widget(Line::from(spans), v);
    if let Some(a) = form.selected_agent() {
        let [_, d] = Layout::horizontal([Constraint::Length(9), Constraint::Min(1)]).areas(rows[1]);
        let mut detail = a.detail.clone();
        if focused {
            detail.push_str("   ← → to choose");
        }
        f.render_widget(Span::styled(detail, Style::new().dim().italic()), d);
    }

    if form.machines.len() > 1 {
        let focused = form.field == SpawnField::Machine;
        let v = label(f, rows[2], "machine", focused);
        let mut spans = Vec::new();
        for (i, m) in form.machines.iter().enumerate() {
            let style = if i == form.machine {
                if focused { Style::new().fg(Color::Black).bg(Color::Cyan).bold() } else { Style::new().bold().underlined() }
            } else {
                Style::new().dim()
            };
            spans.push(Span::styled(format!(" {m} "), style));
            spans.push(Span::raw(" "));
        }
        f.render_widget(Line::from(spans), v);
    }
    let v = label(f, rows[3], "folder", form.field == SpawnField::Folder);
    draw_input(f, v, &form.folder, form.field == SpawnField::Folder);
    let wt_focused = form.field == SpawnField::Worktree;
    let v = label(f, rows[4], "worktree", wt_focused);
    let (mark, text) = if form.worktree {
        ("[x]", format!(" new branch hiero/{} in its own folder", form.slug()))
    } else {
        ("[ ]", " run in the folder itself".to_string())
    };
    let mark_style = if wt_focused { Style::new().fg(Color::Black).bg(Color::Cyan).bold() } else { Style::new().bold() };
    let mut spans = vec![Span::styled(mark, mark_style), Span::styled(text, Style::new().dim())];
    if wt_focused {
        spans.push(Span::styled("   space toggles", Style::new().dim().italic()));
    }
    f.render_widget(Line::from(spans), v);

    let v = label(f, rows[6], "task", form.field == SpawnField::Task);
    draw_input(f, v, &form.task, form.field == SpawnField::Task);
    let v = label(f, rows[8], "name", form.field == SpawnField::Name);
    draw_input(f, v, &form.name, form.field == SpawnField::Name);
    if form.name.value().is_empty() && form.field != SpawnField::Name {
        f.render_widget(Span::styled("optional", Style::new().dim().italic()), v);
    }

    if let Some(e) = &form.error {
        f.render_widget(Span::styled(format!("✗ {e}"), Style::new().fg(Color::Red)), rows[10]);
    }
    f.render_widget(
        Paragraph::new("tab/↓ next field · enter on name or ctrl+s start · esc cancel").dim(),
        rows[11],
    );
}

fn draw_prompt(f: &mut Frame, area: Rect, title: &str, input: &TextInput, hint: &str) {
    let inner = popup(f, area, 72, 5, title);
    let [i, _, h] = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)]).areas(inner);
    draw_input(f, i, input, true);
    f.render_widget(Paragraph::new(hint.to_string()).dim(), h);
}

fn draw_confirm(f: &mut Frame, area: Rect, message: &str) {
    let w = (message.chars().count() as u16 + 6).max(30);
    let inner = popup(f, area, w, 3, "confirm");
    f.render_widget(Paragraph::new(message.to_string()).yellow(), inner);
}

fn draw_templates(f: &mut Frame, area: Rect, v: &crate::forms::TemplatesView) {
    let h = (v.items.len() as u16 + 5).clamp(8, 24);
    let inner = popup(f, area, 80, h, "templates");
    let [list, path, keys] = Layout::vertical([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)]).areas(inner);
    if v.items.is_empty() {
        f.render_widget(Paragraph::new("No templates yet. Press n to make one.").dim(), list);
    } else {
        let rows: Vec<Row> = v
            .items
            .iter()
            .map(|t| {
                Row::new(vec![
                    Cell::from(Span::styled(t.name.clone(), Style::new().bold().fg(Color::Cyan))),
                    Cell::from(Span::styled(t.description.clone().unwrap_or_default(), Style::new().dim())),
                ])
            })
            .collect();
        let table = Table::new(rows, [Constraint::Length(18), Constraint::Min(10)])
            .row_highlight_style(Style::new().bg(Color::DarkGray))
            .highlight_symbol("▌");
        let mut state = TableState::default().with_selected(Some(v.selected));
        f.render_stateful_widget(table, list, &mut state);
    }
    f.render_widget(
        Paragraph::new(shorten_home(&crate::forms::templates_dir().display().to_string())).dim().italic(),
        path,
    );
    f.render_widget(
        hints(&[("r", "run"), ("e", "edit"), ("n", "new"), ("D", "delete"), ("esc", "back")]),
        keys,
    );
}

fn task_style(status: crate::tasks::TaskStatus) -> (&'static str, Style) {
    use crate::tasks::TaskStatus::*;
    match status {
        Running => ("●", Style::new().fg(Color::Yellow)),
        Merged => ("✓", Style::new().fg(Color::Green)),
        Pr => ("⇡", Style::new().fg(Color::Cyan)),
        Kept => ("■", Style::new().fg(Color::Blue)),
        Discarded => ("✗", Style::new().dim()),
    }
}

fn draw_tasks(f: &mut Frame, app: &App, area: Rect, selected: usize) {
    let list = app.task_list();
    let h = (list.len() as u16 + 5).clamp(8, area.height.saturating_sub(4));
    let inner = popup(f, area, 110, h, "tasks");
    let [body, keys] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    if list.is_empty() {
        f.render_widget(
            Paragraph::new("No tasks yet. Press c on the board, keep worktree on, and give it a task.").dim(),
            body,
        );
    } else {
        let now = crate::app::now_ms();
        let rows: Vec<Row> = list
            .iter()
            .map(|t| {
                let (sym, style) = task_style(t.status);
                let agent = match t.terminal.as_deref().and_then(|a| app.set.get(a)) {
                    Some(term) if term.is_live() => format!("{} {}", t.agent, term.address),
                    Some(_) => format!("{} (exited)", t.agent),
                    None => t.agent.clone(),
                };
                Row::new(vec![
                    Cell::from(Span::styled(sym, style)),
                    Cell::from(Span::styled(t.title.clone(), Style::new().bold())),
                    Cell::from(Span::styled(t.status.word(), style)),
                    Cell::from(Span::styled(t.branch.clone(), Style::new().fg(Color::Green))),
                    Cell::from(Span::styled(agent, Style::new().fg(Color::Cyan))),
                    Cell::from(Span::styled(fmt_dur(now.saturating_sub(t.created_ms)), Style::new().dim())),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Min(20),
                Constraint::Length(10),
                Constraint::Length(26),
                Constraint::Length(16),
                Constraint::Length(6),
            ],
        )
        .header(Row::new(["", "TASK", "STATUS", "BRANCH", "AGENT", "AGE"]).style(Style::new().dim()))
        .column_spacing(1)
        .row_highlight_style(Style::new().bg(Color::DarkGray))
        .highlight_symbol("▌");
        let mut state = TableState::default().with_selected(Some(selected.min(list.len() - 1)));
        f.render_stateful_widget(table, body, &mut state);
    }
    f.render_widget(
        hints(&[("enter", "review"), ("g", "go to agent"), ("D", "forget finished"), ("esc", "back")]),
        keys,
    );
}

fn draw_review(f: &mut Frame, app: &App, area: Rect, v: &crate::app::ReviewView) {
    let Some(task) = app.tasks.tasks.get(&v.task_id) else { return };
    let w = area.width.saturating_sub(4);
    let h = area.height.saturating_sub(2);
    let inner = popup(f, area, w, h, &format!("review · {}", task.title));
    let [head, body, keys] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(3), Constraint::Length(1)]).areas(inner);

    // Header: branch, status, numbers, timeline tail.
    let (sym, style) = task_style(task.status);
    let mut line1 = vec![
        Span::styled(format!("{sym} {} ", task.status.word()), style),
        Span::styled(format!(" {} ", task.branch), Style::new().fg(Color::Green).bold()),
        Span::styled(format!("→ {}", task.base_branch), Style::new().dim()),
        Span::styled(format!("   {}", shorten_home(&task.worktree)), Style::new().dim()),
    ];
    if let Some(url) = &task.pr_url {
        line1.push(Span::styled(format!("   {url}"), Style::new().fg(Color::Cyan)));
    }
    let line2 = match (&v.data, &v.error, v.loading) {
        (_, Some(e), _) => Line::from(Span::styled(format!("✗ {e}"), Style::new().fg(Color::Red))),
        (None, None, _) | (_, None, true) if v.data.is_none() => Line::from(Span::styled("reading changes…", Style::new().dim())),
        (Some(d), None, _) => {
            let (add, del) = d.files.iter().fold((0, 0), |(a, r), f| (a + f.added.unwrap_or(0), r + f.removed.unwrap_or(0)));
            let mut sp = vec![
                Span::raw(format!("{} file{} ", d.files.len(), if d.files.len() == 1 { "" } else { "s" })),
                Span::styled(format!("+{add} "), Style::new().fg(Color::Green)),
                Span::styled(format!("-{del}"), Style::new().fg(Color::Red)),
                Span::styled(
                    format!(
                        "   {} commit{} ahead of {}",
                        d.commits_ahead,
                        if d.commits_ahead == 1 { "" } else { "s" },
                        &d.base[..d.base.len().min(7)]
                    ),
                    Style::new().dim(),
                ),
            ];
            if d.dirty {
                sp.push(Span::styled("   uncommitted changes", Style::new().fg(Color::Yellow)));
            }
            if v.loading {
                sp.push(Span::styled("   refreshing…", Style::new().dim()));
            }
            Line::from(sp)
        }
        _ => Line::default(),
    };
    let last = task
        .timeline
        .iter()
        .rev()
        .take(4)
        .map(|e| e.what.clone())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("  ›  ");
    let line3 = Line::from(Span::styled(format!("timeline: {last}"), Style::new().dim()));
    f.render_widget(Paragraph::new(vec![Line::from(line1), line2, line3]), head);

    let [files_area, diff_area] = Layout::horizontal([Constraint::Percentage(28), Constraint::Percentage(72)]).areas(body);
    let files_block = Block::new().borders(Borders::ALL).border_type(BorderType::Rounded).title(" files ");
    let diff_block = Block::new().borders(Borders::ALL).border_type(BorderType::Rounded);
    let Some(d) = &v.data else {
        f.render_widget(files_block, files_area);
        f.render_widget(diff_block, diff_area);
        f.render_widget(hints(&[("r", "refresh"), ("esc", "back")]), keys);
        return;
    };
    if d.files.is_empty() {
        f.render_widget(Paragraph::new("no changes yet").dim().block(files_block), files_area);
    } else {
        let rows: Vec<Row> = d
            .files
            .iter()
            .map(|fc| {
                let st = match fc.status {
                    'A' | '?' => Style::new().fg(Color::Green),
                    'D' => Style::new().fg(Color::Red),
                    _ => Style::new().fg(Color::Yellow),
                };
                let nums = match (fc.added, fc.removed) {
                    (Some(a), Some(r)) => format!("+{a} -{r}"),
                    _ => "bin".into(),
                };
                Row::new(vec![
                    Cell::from(Span::styled(if fc.status == '?' { "N".to_string() } else { fc.status.to_string() }, st)),
                    Cell::from(fc.path.clone()),
                    Cell::from(Span::styled(nums, Style::new().dim())),
                ])
            })
            .collect();
        let table = Table::new(rows, [Constraint::Length(1), Constraint::Min(8), Constraint::Length(9)])
            .block(files_block)
            .row_highlight_style(Style::new().bg(Color::DarkGray))
            .highlight_symbol("▌");
        let mut state = TableState::default().with_selected(Some(v.file));
        f.render_stateful_widget(table, files_area, &mut state);
    }

    let path = d.files.get(v.file).map(|f| f.path.as_str()).unwrap_or("");
    let text = d.diff_for(path);
    let lines: Vec<Line> = text
        .lines()
        .skip(v.scroll as usize)
        .map(|l| {
            let style = if l.starts_with("+++") || l.starts_with("---") || l.starts_with("diff ") || l.starts_with("index ") || l.starts_with("new file") {
                Style::new().dim()
            } else if l.starts_with('+') {
                Style::new().fg(Color::Green)
            } else if l.starts_with('-') {
                Style::new().fg(Color::Red)
            } else if l.starts_with("@@") {
                Style::new().fg(Color::Cyan)
            } else {
                Style::new()
            };
            Line::from(Span::styled(l.to_string(), style))
        })
        .collect();
    f.render_widget(Paragraph::new(lines).block(diff_block.title(format!(" {path} "))), diff_area);

    let mut k = vec![("j/k", "file"), ("PgUp/PgDn", "scroll"), ("m", "merge"), ("P", "pull request"), ("K", "keep"), ("D", "discard"), ("r", "refresh"), ("esc", "back")];
    if task.status != crate::tasks::TaskStatus::Running && task.status != crate::tasks::TaskStatus::Kept {
        k.retain(|(key, _)| *key != "K");
    }
    f.render_widget(hints(&k), keys);
}

fn run_style(status: crate::pipeline::RunStatus) -> Style {
    use crate::pipeline::RunStatus::*;
    match status {
        Running => Style::new().fg(Color::Yellow),
        Done => Style::new().fg(Color::Green),
        Failed => Style::new().fg(Color::Red),
        Stopped => Style::new().dim(),
    }
}

fn draw_pipelines(f: &mut Frame, app: &App, area: Rect, v: &crate::app::PipelinesView) {
    let runs = app.run_list();
    let h = (v.specs.len() as u16 + runs.len() as u16 + v.errors.len() as u16 + 9).clamp(12, area.height.saturating_sub(2));
    let inner = popup(f, area, 116, h, "pipelines");
    let [top, mid, errs, path, keys] = Layout::vertical([
        Constraint::Length(v.specs.len() as u16 + 1),
        Constraint::Min(3),
        Constraint::Length(v.errors.len() as u16),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let mut rows: Vec<Row> = v
        .specs
        .iter()
        .map(|p| {
            let stages: Vec<String> = p
                .stages
                .iter()
                .map(|s| if s.members() > 1 { format!("{}×{}", s.name, s.members()) } else { s.name.clone() })
                .collect();
            Row::new(vec![
                Cell::from(Span::styled(p.name.clone(), Style::new().bold().fg(Color::Cyan))),
                Cell::from(Span::styled(stages.join(" → "), Style::new().fg(Color::Magenta))),
                Cell::from(Span::styled(p.description.clone(), Style::new().dim())),
            ])
        })
        .collect();
    let sel_spec = (v.selected < v.specs.len()).then_some(v.selected);
    let t = Table::new(std::mem::take(&mut rows), [Constraint::Length(22), Constraint::Length(34), Constraint::Min(10)])
        .header(Row::new(["PIPELINE", "STAGES", ""]).style(Style::new().dim()))
        .row_highlight_style(Style::new().bg(Color::DarkGray))
        .highlight_symbol("▌");
    f.render_stateful_widget(t, top, &mut TableState::default().with_selected(sel_spec));

    let now = crate::app::now_ms();
    let run_rows: Vec<Row> = runs
        .iter()
        .map(|r| {
            let style = run_style(r.status);
            let mut who = r.members.iter().map(|m| m.terminal.as_str()).collect::<Vec<_>>().join(" ");
            if r.status == crate::pipeline::RunStatus::Running && r.driver != app.driver_id {
                who.push_str("  (driven by another board — A adopts)");
            }
            Row::new(vec![
                Cell::from(Span::styled(r.progress(), style)),
                Cell::from(Span::styled(r.pipeline.clone(), Style::new().fg(Color::Cyan))),
                Cell::from(r.task.clone()),
                Cell::from(Span::styled(if r.note.is_empty() { who } else { r.note.clone() }, Style::new().dim())),
                Cell::from(Span::styled(fmt_dur(now.saturating_sub(r.started_ms)), Style::new().dim())),
            ])
        })
        .collect();
    let block = Block::new().borders(Borders::TOP).title(" runs ");
    if runs.is_empty() {
        f.render_widget(Paragraph::new("No runs yet. Pick a pipeline and press enter.").dim().block(block), mid);
    } else {
        let sel_run = v.selected.checked_sub(v.specs.len());
        let t = Table::new(
            run_rows,
            [Constraint::Length(22), Constraint::Length(18), Constraint::Min(16), Constraint::Length(36), Constraint::Length(6)],
        )
        .block(block)
        .row_highlight_style(Style::new().bg(Color::DarkGray))
        .highlight_symbol("▌");
        f.render_stateful_widget(t, mid, &mut TableState::default().with_selected(sel_run));
    }
    if !v.errors.is_empty() {
        let lines: Vec<Line> = v.errors.iter().map(|e| Line::from(Span::styled(format!("✗ {e}"), Style::new().fg(Color::Red)))).collect();
        f.render_widget(Paragraph::new(lines), errs);
    }
    f.render_widget(
        Paragraph::new(shorten_home(&crate::pipeline::pipelines_dir().display().to_string())).dim().italic(),
        path,
    );
    f.render_widget(
        hints(&[("enter", "launch / go to run"), ("e", "edit (copies the built-in)"), ("S", "stop run"), ("A", "adopt"), ("D", "forget run"), ("esc", "back")]),
        keys,
    );
}

fn draw_launch(f: &mut Frame, area: Rect, form: &crate::app::LaunchForm) {
    use crate::app::LaunchField;
    let inner = popup(f, area, 84, 11, &format!("launch {}", form.spec.name));
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .split(inner);
    let stages: Vec<String> = form
        .spec
        .stages
        .iter()
        .map(|s| {
            let what = s.agent.clone().or_else(|| s.command.clone().map(|c| format!("`{c}`"))).unwrap_or_default();
            let n = if s.members() > 1 { format!(" ×{}", s.members()) } else { String::new() };
            let wt = if s.worktree { " (worktrees)" } else { "" };
            format!("{}: {what}{n}{wt}", s.name)
        })
        .collect();
    f.render_widget(Paragraph::new(stages.join("  →  ")).fg(Color::Magenta), rows[0]);
    let field = |f: &mut Frame, r: Rect, label: &str, input: &TextInput, focused: bool| {
        let [l, v] = Layout::horizontal([Constraint::Length(8), Constraint::Min(1)]).areas(r);
        let style = if focused { Style::new().fg(Color::Cyan).bold() } else { Style::new().dim() };
        f.render_widget(Span::styled(label.to_string(), style), l);
        draw_input(f, v, input, focused);
    };
    field(f, rows[2], "task", &form.task, form.field == LaunchField::Task);
    field(f, rows[4], "folder", &form.folder, form.field == LaunchField::Folder);
    if let Some(e) = &form.error {
        f.render_widget(Span::styled(format!("✗ {e}"), Style::new().fg(Color::Red)), rows[6]);
    }
    f.render_widget(Paragraph::new("tab switch field · enter on folder or ctrl+s launch · esc back").dim(), rows[7]);
}

/// An agent attached inside the board, drawn from its vt100 screen.
fn draw_pane(f: &mut Frame, pane: &PaneView, area: Rect) {
    let mut title = vec![
        Span::raw(" "),
        Span::styled(pane.name.clone(), Style::new().bold()),
        Span::styled(format!(" {} · attached ", pane.addr), Style::new().dim()),
    ];
    let border = if let Some(why) = &pane.ended {
        title.push(Span::styled(format!("· ended: {why} "), Style::new().fg(Color::Red)));
        Color::Red
    } else {
        Color::Green
    };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::new().fg(border))
        .title(Line::from(title))
        .title_bottom(Line::from(Span::styled(" ctrl+\\ back to board ", Style::new().dim())));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let screen = pane.parser.screen();
    let buf = f.buffer_mut();
    let (rows, cols) = screen.size();
    for r in 0..rows.get().min(inner.height) {
        for c in 0..cols.get().min(inner.width) {
            let Some(cell) = screen.cell(r, c) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            let mut fg = vt_color(cell.fgcolor());
            let mut bg = vt_color(cell.bgcolor());
            if cell.inverse() {
                std::mem::swap(&mut fg, &mut bg);
                fg = fg.or(Some(Color::Black));
                bg = bg.or(Some(Color::White));
            }
            let mut style = Style::new();
            if let Some(c) = fg {
                style = style.fg(c);
            }
            if let Some(c) = bg {
                style = style.bg(c);
            }
            let mut m = Modifier::empty();
            if cell.bold() {
                m |= Modifier::BOLD;
            }
            if cell.dim() {
                m |= Modifier::DIM;
            }
            if cell.italic() {
                m |= Modifier::ITALIC;
            }
            if cell.underline() {
                m |= Modifier::UNDERLINED;
            }
            let pos = (inner.x + c, inner.y + r);
            if let Some(out) = buf.cell_mut(pos) {
                let text = cell.contents();
                out.set_symbol(if text.is_empty() { " " } else { text });
                out.set_style(style.add_modifier(m));
            }
        }
    }
    if pane.ended.is_none() && !screen.hide_cursor() {
        let (r, c) = screen.cursor_position();
        if r < inner.height && c < inner.width {
            f.set_cursor_position((inner.x + c, inner.y + r));
        }
    }
}

fn vt_color(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

fn draw_help(f: &mut Frame, area: Rect) {
    let text = vec![
        Line::from("Move".bold()),
        Line::from("  j / k, ↑ / ↓     select                g / G   first / last"),
        Line::from("  n, tab           next agent that needs you"),
        Line::from(""),
        Line::from("Agents".bold()),
        Line::from("  c                new agent             t       templates"),
        Line::from("  s                send a message        i       interrupt"),
        Line::from("  a, enter         attach full-screen    o       open inside the board"),
        Line::from("  K                kill                  d       remove finished"),
        Line::from("  e                keep on/off           R       rename"),
        Line::from("  T                tasks                 v       review this agent's task"),
        Line::from("  w                pipelines             B       broadcast to agents shown"),
        Line::from("  ctrl+\\          leave an attached agent"),
        Line::from(""),
        Line::from("Board".bold()),
        Line::from("  /                search (fuzzy)        esc     clear"),
        Line::from("  p                project / tree / flat f       show finished"),
        Line::from("  x                dismiss from NEEDS YOU"),
        Line::from("  PgUp/PgDn, ctrl+u/d  scroll preview    r       refresh preview"),
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
