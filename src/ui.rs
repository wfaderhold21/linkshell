use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

use crate::app::{App, AppMode, NewSessionField, Selection};
use crate::session::{SessionKind, SessionState};
use vt100::Screen;

#[derive(Default)]
pub struct LayoutInfo {
    pub output_area: Rect,
    pub session_bar_area: Rect,
    pub session_slot_areas: Vec<Rect>,
}

/// Strip ANSI/VT escape sequences and prepare a line for safe ratatui rendering.
///
/// If raw escape sequences reach ratatui they get forwarded to the terminal,
/// where the terminal interprets them (e.g. \x1b[2J clears the screen mid-draw).
/// This strips CSI, OSC, and other escape sequences, expands tabs, and drops
/// remaining non-printable control characters.
fn prepare_display(s: &str) -> String {
    // Phase 1: strip escape sequences
    let mut stripped = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            stripped.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                // CSI — consume parameter + intermediate bytes, then final byte (0x40–0x7E)
                for nc in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&nc) { break; }
                }
            }
            Some(']') => {
                // OSC — consume until BEL or ST (ESC \)
                loop {
                    match chars.next() {
                        Some('\x07') | None => break,
                        Some('\x1b') => { chars.next(); break; }
                        _ => {}
                    }
                }
            }
            Some('(') | Some(')') | Some('*') | Some('+') => {
                chars.next(); // charset designation: skip one more char
            }
            _ => {} // other two-char sequences: already consumed
        }
    }

    // Phase 2: expand tabs, drop remaining control chars
    let mut out = String::with_capacity(stripped.len());
    let mut col: usize = 0;
    for c in stripped.chars() {
        match c {
            '\t' => {
                let spaces = 8 - (col % 8);
                for _ in 0..spaces { out.push(' '); }
                col += spaces;
            }
            c if (c as u32) < 0x20 || c == '\x7f' => {} // drop control chars
            c => { out.push(c); col += 1; }
        }
    }
    out
}

// ── Brand colours ──────────────────────────────────────────────────────────
const CLAUDE_COLOR: Color = Color::Rgb(255, 140, 0);  // orange
const CODEX_COLOR:  Color = Color::Rgb(64, 128, 255); // blue
const SHELL_COLOR:  Color = Color::White;
const CUSTOM_COLOR: Color = Color::Cyan;

fn kind_color(kind: &SessionKind) -> Color {
    match kind {
        SessionKind::Claude    => CLAUDE_COLOR,
        SessionKind::Codex     => CODEX_COLOR,
        SessionKind::Shell     => SHELL_COLOR,
        SessionKind::Custom(_) => CUSTOM_COLOR,
    }
}

fn state_border_style(state: &SessionState, active: bool) -> Style {
    let base = match state {
        SessionState::Waiting => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        SessionState::Error   => Style::default().fg(Color::Red).add_modifier(Modifier::RAPID_BLINK),
        SessionState::Dead    => Style::default().fg(Color::DarkGray),
        _ if active           => Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        _                     => Style::default().fg(Color::DarkGray),
    };
    base
}

pub fn draw(f: &mut Frame<'_>, app: &App) -> LayoutInfo {
    let size = f.size();

    // ── Top-level vertical split ───────────────────────────────────────────
    // main output | session bar | status panel
    let status_rows = app.sessions.len().max(1) as u16 + 2; // border + rows
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),          // main output
            Constraint::Length(3),       // session bar
            Constraint::Length(status_rows), // status panel
        ])
        .split(size);

    draw_main_output(f, app, chunks[0]);
    let slot_areas = draw_session_bar(f, app, chunks[1]);
    draw_status_panel(f, app, chunks[2]);

    // ── Overlays ───────────────────────────────────────────────────────────
    match &app.mode {
        AppMode::NewSession => draw_new_session_dialog(f, app, size),
        AppMode::CommandBar => draw_command_bar(f, app, size),
        AppMode::Help       => draw_help(f, size),
        AppMode::Normal     => {}
    }

    LayoutInfo {
        output_area:        chunks[0],
        session_bar_area:   chunks[1],
        session_slot_areas: slot_areas,
    }
}

// ── Main output zone ───────────────────────────────────────────────────────

fn draw_main_output(f: &mut Frame<'_>, app: &App, area: Rect) {
    let (title, lines, border_style) = if let Some(idx) = app.active_idx {
        let session = &app.sessions[idx];
        let title = format!(
            " {} [{}] {} ",
            idx + 1,
            session.kind.label().to_uppercase(),
            session.name
        );
        let screen = session.screen.screen();
        let (screen_rows, screen_cols) = screen.size();
        let display_rows = area.height.saturating_sub(2) as u16;
        let start_row = screen_rows.saturating_sub(display_rows);
        let sel = app.selection.as_ref();
        let items: Vec<ListItem> = (start_row..screen_rows)
            .enumerate()
            .map(|(disp_row, vt_row)| {
                let disp_row = disp_row as u16;
                build_row(screen, vt_row, screen_cols, disp_row, sel)
            })
            .collect();
        let style = state_border_style(&session.state, true);
        (title, items, style)
    } else {
        let items = vec![ListItem::new(Line::from(
            " No sessions. Press alt-n to create one.",
        ))];
        (" linkshell ".to_string(), items, Style::default().fg(Color::DarkGray))
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);

    let list = List::new(lines).block(block);
    f.render_widget(list, area);
}

// ── Session bar ────────────────────────────────────────────────────────────

fn draw_session_bar(f: &mut Frame<'_>, app: &App, area: Rect) -> Vec<Rect> {
    let n = app.sessions.len();

    // outer block
    let outer = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM);
    f.render_widget(outer, area);

    if n == 0 {
        return vec![];
    }

    // inner area (remove outer border)
    let inner = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    };

    // Center the slots
    let slot_w = (inner.width as usize / n).min(16) as u16;
    let total_w = slot_w * n as u16;
    let offset_x = inner.x + (inner.width.saturating_sub(total_w)) / 2;

    for (i, session) in app.sessions.iter().enumerate() {
        let slot = Rect {
            x: offset_x + i as u16 * slot_w,
            y: inner.y,
            width: slot_w,
            height: inner.height,
        };

        let is_active = app.active_idx == Some(i);
        let label = format!("{} {}", i + 1, session.kind.label());
        let color = kind_color(&session.kind);
        let border_style = state_border_style(&session.state, is_active);

        let title_style = if is_active {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(color)
        };

        let block = Block::default()
            .title(Span::styled(label, title_style))
            .borders(Borders::ALL)
            .border_style(border_style);

        // State dot inside slot
        let state_dot = match session.state {
            SessionState::Waiting  => Span::styled("⚡", Style::default().fg(Color::Yellow)),
            SessionState::Error    => Span::styled("✗", Style::default().fg(Color::Red)),
            SessionState::Thinking => Span::styled("…", Style::default().fg(CLAUDE_COLOR)),
            SessionState::Running  => Span::styled("▶", Style::default().fg(Color::Green)),
            SessionState::Ready    => Span::styled("●", Style::default().fg(Color::Green)),
            SessionState::Dead     => Span::styled("✗", Style::default().fg(Color::DarkGray)),
            SessionState::Starting => Span::styled("○", Style::default().fg(Color::Gray)),
        };

        let para = Paragraph::new(Line::from(vec![state_dot]))
            .block(block)
            .alignment(Alignment::Center);

        f.render_widget(para, slot);
    }

    (0..n).map(|i| Rect {
        x: offset_x + i as u16 * slot_w,
        y: inner.y,
        width: slot_w,
        height: inner.height,
    }).collect()
}

// ── Status panel ───────────────────────────────────────────────────────────

fn draw_status_panel(f: &mut Frame<'_>, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Status ")
        .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM);

    let inner = block.inner(area);
    f.render_widget(block, area);

    for (i, session) in app.sessions.iter().enumerate() {
        if i >= inner.height as usize {
            break;
        }

        let row = Rect {
            x: inner.x,
            y: inner.y + i as u16,
            width: inner.width,
            height: 1,
        };

        let num_style   = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
        let kind_style  = Style::default().fg(kind_color(&session.kind));
        let state_style = match session.state {
            SessionState::Waiting  => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            SessionState::Error    => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            SessionState::Thinking => Style::default().fg(CLAUDE_COLOR),
            SessionState::Running  => Style::default().fg(Color::Green),
            SessionState::Ready    => Style::default().fg(Color::DarkGray),
            SessionState::Dead     => Style::default().fg(Color::DarkGray),
            SessionState::Starting => Style::default().fg(Color::Gray),
        };

        let tokens = session.tokens_display();
        let cost   = session.cost_display();
        let elapsed = session.elapsed_display();

        let spans = vec![
            Span::styled(format!(" {:1} ", i + 1), num_style),
            Span::raw("│ "),
            Span::styled(format!("{:<6} ", session.kind.label()), kind_style),
            Span::raw("│ "),
            Span::styled(format!("{:<8} ", session.state.label()), state_style),
            Span::raw("│ "),
            Span::styled(format!("{:>6}  ", elapsed), Style::default().fg(Color::Gray)),
            Span::raw("│ "),
            Span::styled(format!("{:>8}  ", tokens), Style::default().fg(Color::Cyan)),
            Span::raw("│ "),
            Span::styled(format!("{:>7}", cost), Style::default().fg(Color::Green)),
        ];

        let line = Paragraph::new(Line::from(spans));
        f.render_widget(line, row);
    }
}

// ── New session dialog ─────────────────────────────────────────────────────

pub fn draw_new_session_dialog(f: &mut Frame<'_>, app: &App, area: Rect) {
    let popup = centered_rect(50, 14, area);
    f.render_widget(Clear, popup);

    let ns = &app.new_session_state;
    let block = Block::default()
        .title(" New Session ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    // Type selector row
    let types = ["Claude", "Codex", "Shell", "Custom"];
    let type_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(Rect { x: inner.x, y: inner.y, width: inner.width, height: 3 });

    for (i, label) in types.iter().enumerate() {
        let selected = ns.selected_kind == i;
        let is_active_field = ns.active_field == NewSessionField::Kind;
        let style = if selected && is_active_field {
            Style::default().fg(Color::Black).bg(Color::White).add_modifier(Modifier::BOLD)
        } else if selected {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let b = Block::default().borders(Borders::ALL).border_style(style);
        let p = Paragraph::new(*label).block(b).alignment(Alignment::Center);
        f.render_widget(p, type_chunks[i]);
    }

    let fields_y = inner.y + 3;

    // Name field
    draw_input_field(
        f,
        "Name",
        &ns.name,
        Rect { x: inner.x, y: fields_y, width: inner.width, height: 3 },
        ns.active_field == NewSessionField::Name,
    );

    // CWD field
    draw_input_field(
        f,
        "CWD",
        &ns.cwd,
        Rect { x: inner.x, y: fields_y + 3, width: inner.width, height: 3 },
        ns.active_field == NewSessionField::Cwd,
    );

    // Custom cmd field (only when Custom selected)
    if ns.selected_kind == 3 {
        draw_input_field(
            f,
            "Command",
            &ns.custom_cmd,
            Rect { x: inner.x, y: fields_y + 6, width: inner.width, height: 3 },
            ns.active_field == NewSessionField::CustomCmd,
        );
    }

    // Footer hint
    let hint = Paragraph::new(" Tab: next field  Enter: create  Esc: cancel ")
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    f.render_widget(
        hint,
        Rect { x: inner.x, y: popup.y + popup.height - 2, width: inner.width, height: 1 },
    );
}

fn draw_input_field(f: &mut Frame<'_>, label: &str, value: &str, area: Rect, active: bool) {
    let style = if active {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .title(format!(" {} ", label))
        .borders(Borders::ALL)
        .border_style(style);
    let cursor = if active { "█" } else { "" };
    let p = Paragraph::new(format!("{}{}", value, cursor)).block(block);
    f.render_widget(p, area);
}

// ── Help overlay ───────────────────────────────────────────────────────────

fn draw_help(f: &mut Frame<'_>, area: Rect) {
    const BINDINGS: &[(&str, &str)] = &[
        ("alt-n",          "New session dialog"),
        ("alt-tab",        "Next session"),
        ("alt-shift-tab",  "Previous session"),
        ("alt-1 … alt-8",  "Switch to session by number"),
        ("alt-← / alt-→",  "Cycle sessions"),
        ("alt-x",          "Kill active session"),
        ("alt-c",          "Open command bar"),
        ("alt-h",          "Show this help"),
        ("ctrl-q",         "Quit"),
        ("esc",            "Dismiss overlay"),
    ];

    let height = BINDINGS.len() as u16 + 4; // borders + title + footer
    let popup = centered_rect(52, height, area);
    f.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Keyboard Shortcuts ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let key_style   = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let desc_style  = Style::default().fg(Color::White);
    let sep_style   = Style::default().fg(Color::DarkGray);

    let rows: Vec<ListItem> = BINDINGS.iter().map(|(key, desc)| {
        ListItem::new(Line::from(vec![
            Span::styled(format!("  {:<18}", key), key_style),
            Span::styled("│  ", sep_style),
            Span::styled(*desc, desc_style),
        ]))
    }).collect();

    let list = List::new(rows);
    f.render_widget(list, inner);

    let footer = Paragraph::new(" press any key to close ")
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    f.render_widget(
        footer,
        Rect { x: inner.x, y: popup.y + popup.height - 2, width: inner.width, height: 1 },
    );
}

// ── Command bar ────────────────────────────────────────────────────────────

fn draw_command_bar(f: &mut Frame<'_>, app: &App, area: Rect) {
    let bar = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    f.render_widget(Clear, bar);
    let text = format!("> {}█", app.command_input);
    let p = Paragraph::new(text).style(Style::default().fg(Color::White).bg(Color::DarkGray));
    f.render_widget(p, bar);
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Build one display row, applying selection highlight where applicable.
fn build_row(
    screen: &Screen,
    vt_row: u16,
    screen_cols: u16,
    disp_row: u16,
    sel: Option<&Selection>,
) -> ListItem<'static> {
    let sel_style = Style::default().bg(Color::Blue).fg(Color::White);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut in_sel = false;

    for col in 0..screen_cols {
        let cell_sel = sel.map_or(false, |s| s.contains(disp_row, col));
        let content = screen.cell(vt_row, col)
            .map(|c| { let s = c.contents(); if s.is_empty() { " ".to_string() } else { s } })
            .unwrap_or_else(|| " ".to_string());

        if cell_sel != in_sel {
            if !run.is_empty() {
                spans.push(Span::styled(run.clone(), if in_sel { sel_style } else { Style::default() }));
                run.clear();
            }
            in_sel = cell_sel;
        }
        run.push_str(&content);
    }

    // Trim trailing spaces only when there's no trailing selection
    let trimmed = if in_sel { run } else { run.trim_end().to_string() };
    if !trimmed.is_empty() {
        spans.push(Span::styled(trimmed, if in_sel { sel_style } else { Style::default() }));
    }

    ListItem::new(Line::from(spans))
}

fn centered_rect(percent_x: u16, height: u16, r: Rect) -> Rect {
    let w = r.width * percent_x / 100;
    let x = r.x + (r.width - w) / 2;
    let y = r.y + (r.height.saturating_sub(height)) / 2;
    Rect { x, y, width: w, height: height.min(r.height) }
}
