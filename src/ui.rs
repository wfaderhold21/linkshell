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
                    if ('\x40'..='\x7e').contains(&nc) {
                        break;
                    }
                }
            }
            Some(']') => {
                // OSC — consume until BEL or ST (ESC \)
                loop {
                    match chars.next() {
                        Some('\x07') | None => break,
                        Some('\x1b') => {
                            chars.next();
                            break;
                        }
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
                for _ in 0..spaces {
                    out.push(' ');
                }
                col += spaces;
            }
            c if (c as u32) < 0x20 || c == '\x7f' => {} // drop control chars
            c => {
                out.push(c);
                col += 1;
            }
        }
    }
    out
}

// ── Brand colours ──────────────────────────────────────────────────────────
const CLAUDE_COLOR: Color = Color::Rgb(255, 140, 0); // orange
const CODEX_COLOR: Color = Color::Rgb(64, 128, 255); // blue
const SHELL_COLOR: Color = Color::White;
const CUSTOM_COLOR: Color = Color::Cyan;

fn kind_color(kind: &SessionKind) -> Color {
    match kind {
        SessionKind::Claude => CLAUDE_COLOR,
        SessionKind::Codex => CODEX_COLOR,
        SessionKind::Shell => SHELL_COLOR,
        SessionKind::Custom(_) => CUSTOM_COLOR,
    }
}

fn state_border_style(state: &SessionState, active: bool) -> Style {
    let base = match state {
        SessionState::Waiting => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        SessionState::Error => Style::default()
            .fg(Color::Red)
            .add_modifier(Modifier::RAPID_BLINK),
        SessionState::Dead => Style::default().fg(Color::DarkGray),
        _ if active => Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
        _ => Style::default().fg(Color::DarkGray),
    };
    base
}

pub fn draw(f: &mut Frame<'_>, app: &App) -> LayoutInfo {
    let size = f.size();

    // ── Top-level vertical split ───────────────────────────────────────────
    // main output | session bar | status panel
    let status_rows = app.sessions.len().max(1) as u16 + 4; // border + header + rows + socket footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),              // main output
            Constraint::Length(3),           // session bar
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
        AppMode::Help => draw_help(f, size),
        AppMode::Normal => {}
    }

    LayoutInfo {
        output_area: chunks[0],
        session_bar_area: chunks[1],
        session_slot_areas: slot_areas,
    }
}

// ── Main output zone ───────────────────────────────────────────────────────

fn draw_main_output(f: &mut Frame<'_>, app: &App, area: Rect) {
    let (title, lines, border_style) = if let Some(idx) = app.active_idx {
        let session = &app.sessions[idx];
        let screen = session.screen.screen();
        let (screen_rows, screen_cols) = screen.size();
        let display_rows = area.height.saturating_sub(2) as u16;
        let scroll_offset = app.scroll_offset() as u16;
        // vt100 handles the scrollback offset internally via set_scrollback;
        // just display the bottom display_rows rows of the virtual screen.
        let start_row = screen_rows.saturating_sub(display_rows);
        let end_row = screen_rows;
        let scroll_indicator = if scroll_offset > 0 {
            format!(" ↑{} ", scroll_offset)
        } else {
            String::new()
        };
        let title = format!(
            " {} [{}] {}{}",
            idx + 1,
            session.kind.label().to_uppercase(),
            session.name,
            scroll_indicator,
        );
        let sel = app.selection.as_ref();
        let cursor = screen.cursor_position();
        let items: Vec<ListItem> = (start_row..end_row)
            .enumerate()
            .map(|(disp_row, vt_row)| {
                let disp_row = disp_row as u16;
                // Only show cursor when at the live tail
                let cursor_col = if scroll_offset == 0 && vt_row == cursor.0 {
                    Some(cursor.1)
                } else {
                    None
                };
                build_row(screen, vt_row, screen_cols, disp_row, sel, cursor_col)
            })
            .collect();
        let style = state_border_style(&session.state, true);
        (title, items, style)
    } else {
        let items = vec![ListItem::new(Line::from(
            " No sessions. Press alt-n to create one.",
        ))];
        (
            " linkshell ".to_string(),
            items,
            Style::default().fg(Color::DarkGray),
        )
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
    let outer = Block::default().borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM);
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
            SessionState::Waiting => Span::styled("⚡", Style::default().fg(Color::Yellow)),
            SessionState::Error => Span::styled("✗", Style::default().fg(Color::Red)),
            SessionState::Thinking => Span::styled("…", Style::default().fg(CLAUDE_COLOR)),
            SessionState::Running => Span::styled("▶", Style::default().fg(Color::Green)),
            SessionState::Ready => Span::styled("●", Style::default().fg(Color::Green)),
            SessionState::Dead => Span::styled("✗", Style::default().fg(Color::DarkGray)),
            SessionState::Starting => Span::styled("○", Style::default().fg(Color::Gray)),
        };

        let para = Paragraph::new(Line::from(vec![state_dot]))
            .block(block)
            .alignment(Alignment::Center);

        f.render_widget(para, slot);
    }

    (0..n)
        .map(|i| Rect {
            x: offset_x + i as u16 * slot_w,
            y: inner.y,
            width: slot_w,
            height: inner.height,
        })
        .collect()
}

// ── Status panel ───────────────────────────────────────────────────────────

fn draw_status_panel(f: &mut Frame<'_>, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Status ")
        .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM);

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Header row
    if inner.height > 0 {
        let hdr_style = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD);
        let header_spans = vec![
            Span::styled("  # ", hdr_style),
            Span::styled("│ ", hdr_style),
            Span::styled(format!("{:<6} ", "Kind"), hdr_style),
            Span::styled("│ ", hdr_style),
            Span::styled(format!("{:<5} ", "Pipe"), hdr_style),
            Span::styled("│ ", hdr_style),
            Span::styled(format!("{:<8} ", "State"), hdr_style),
            Span::styled("│ ", hdr_style),
            Span::styled(format!("{:>6}  ", "Time"), hdr_style),
            Span::styled("│ ", hdr_style),
            Span::styled(format!("{:>8}  ", "Tokens"), hdr_style),
            Span::styled("│ ", hdr_style),
            Span::styled(format!("{:>8}  ", "Ctx"), hdr_style),
            Span::styled("│ ", hdr_style),
            Span::styled(format!("{:>7}", "Cost"), hdr_style),
        ];
        let header_row = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        };
        f.render_widget(Paragraph::new(Line::from(header_spans)), header_row);
    }

    for (i, session) in app.sessions.iter().enumerate() {
        if i + 2 >= inner.height as usize {
            break;
        }

        let row = Rect {
            x: inner.x,
            y: inner.y + 1 + i as u16,
            width: inner.width,
            height: 1,
        };

        let num_style = Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD);
        let kind_style = Style::default().fg(kind_color(&session.kind));
        let state_style = match session.state {
            SessionState::Waiting => Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            SessionState::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            SessionState::Thinking => Style::default().fg(CLAUDE_COLOR),
            SessionState::Running => Style::default().fg(Color::Green),
            SessionState::Ready => Style::default().fg(Color::DarkGray),
            SessionState::Dead => Style::default().fg(Color::DarkGray),
            SessionState::Starting => Style::default().fg(Color::Gray),
        };

        let tokens = session.tokens_display();
        let context = session.context_display();
        let cost = session.cost_display();
        let elapsed = session.elapsed_display();

        // Pipe destinations: "→2,→3" or blank; bold for 1s after firing
        let (pipe_label, pipe_recently_fired) = {
            let mut fired = false;
            let dests: Vec<String> = app
                .pipes
                .iter()
                .filter(|p| p.source == session.id && p.active)
                .filter_map(|p| {
                    if p.last_fired
                        .map(|t| t.elapsed().as_millis() < 1000)
                        .unwrap_or(false)
                    {
                        fired = true;
                    }
                    app.sessions
                        .iter()
                        .position(|s| s.id == p.dest)
                        .map(|idx| format!("→{}", idx + 1))
                })
                .collect();
            let label = if dests.is_empty() {
                String::new()
            } else {
                dests.join(",")
            };
            (label, fired)
        };

        let spans = vec![
            Span::styled(format!(" {:1} ", i + 1), num_style),
            Span::raw("│ "),
            Span::styled(format!("{:<6} ", session.kind.label()), kind_style),
            Span::raw("│ "),
            Span::styled(format!("{:<5} ", pipe_label), {
                let s = Style::default().fg(Color::Cyan);
                if pipe_recently_fired {
                    s.add_modifier(Modifier::BOLD)
                } else {
                    s
                }
            }),
            Span::raw("│ "),
            Span::styled(format!("{:<8} ", session.state.label()), state_style),
            Span::raw("│ "),
            Span::styled(
                format!("{:>6}  ", elapsed),
                Style::default().fg(Color::Gray),
            ),
            Span::raw("│ "),
            Span::styled(format!("{:>8}  ", tokens), Style::default().fg(Color::Cyan)),
            Span::raw("│ "),
            Span::styled(
                format!("{:>8}  ", context),
                Style::default().fg(Color::Magenta),
            ),
            Span::raw("│ "),
            Span::styled(format!("{:>7}", cost), Style::default().fg(Color::Green)),
        ];

        let line = Paragraph::new(Line::from(spans));
        f.render_widget(line, row);
    }

    // Socket path footer — always at the last row of inner area
    if inner.height > 0 {
        let sock = crate::ipc::socket_path(&app.config);
        let footer_row = Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        };
        let footer = Paragraph::new(Line::from(vec![
            Span::styled("  sock: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                sock,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
        ]));
        f.render_widget(footer, footer_row);
    }
}

// ── New session dialog ─────────────────────────────────────────────────────

pub fn draw_new_session_dialog(f: &mut Frame<'_>, app: &App, area: Rect) {
    let ns = &app.new_session_state;
    let height = if ns.selected_kind == 3 { 16 } else { 13 };
    let popup = centered_rect(50, height, area);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .title(" New Session ")
        .borders(Borders::ALL);
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
        .split(Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 3,
        });

    for (i, label) in types.iter().enumerate() {
        let selected = ns.selected_kind == i;
        let is_active_field = ns.active_field == NewSessionField::Kind;
        let style = if selected && is_active_field {
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD)
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
        ns.name_cursor,
        Rect {
            x: inner.x,
            y: fields_y,
            width: inner.width,
            height: 3,
        },
        ns.active_field == NewSessionField::Name,
    );

    // CWD field
    draw_input_field(
        f,
        "CWD",
        &ns.cwd,
        ns.cwd_cursor,
        Rect {
            x: inner.x,
            y: fields_y + 3,
            width: inner.width,
            height: 3,
        },
        ns.active_field == NewSessionField::Cwd,
    );

    // Custom cmd field (only when Custom selected)
    if ns.selected_kind == 3 {
        draw_input_field(
            f,
            "Command",
            &ns.custom_cmd,
            ns.custom_cmd_cursor,
            Rect {
                x: inner.x,
                y: fields_y + 6,
                width: inner.width,
                height: 3,
            },
            ns.active_field == NewSessionField::CustomCmd,
        );
    }

    // Footer hint
    let hint = Paragraph::new(" Tab: next field  Enter: create  Esc: cancel ")
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    f.render_widget(
        hint,
        Rect {
            x: inner.x,
            y: popup.y + popup.height - 2,
            width: inner.width,
            height: 1,
        },
    );
}

fn draw_input_field(
    f: &mut Frame<'_>,
    label: &str,
    value: &str,
    cursor_pos: usize,
    area: Rect,
    active: bool,
) {
    let style = if active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .title(format!(" {} ", label))
        .borders(Borders::ALL)
        .border_style(style);
    let p = if active {
        let mut pos = cursor_pos.min(value.len());
        while pos > 0 && !value.is_char_boundary(pos) {
            pos -= 1;
        }
        let before = &value[..pos];
        let (cursor_ch, after) = if pos < value.len() {
            let ch = value[pos..].chars().next().unwrap();
            let end = pos + ch.len_utf8();
            (&value[pos..end], &value[end..])
        } else {
            (" ", "")
        };
        let cursor_style = Style::default().bg(Color::Cyan).fg(Color::Black);
        let line = Line::from(vec![
            Span::raw(before),
            Span::styled(cursor_ch, cursor_style),
            Span::raw(after),
        ]);
        Paragraph::new(line).block(block)
    } else {
        Paragraph::new(value.to_string()).block(block)
    };
    f.render_widget(p, area);
}

// ── Help overlay ───────────────────────────────────────────────────────────

fn draw_help(f: &mut Frame<'_>, area: Rect) {
    const BINDINGS: &[(&str, &str)] = &[
        ("alt-n", "New session dialog"),
        ("alt-tab", "Next session"),
        ("alt-shift-tab", "Previous session"),
        ("alt-1 … alt-8", "Switch to session by number"),
        ("alt-← / alt-→", "Cycle sessions"),
        ("alt-x", "Kill active session"),
        ("alt-c", "Open command bar"),
        ("alt-h", "Show this help"),
        ("ctrl-q", "Quit"),
        ("esc", "Dismiss overlay"),
    ];

    let height = BINDINGS.len() as u16 + 4; // borders + title + footer
    let popup = centered_rect(52, height, area);
    f.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Keyboard Shortcuts ")
        .borders(Borders::ALL);
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let key_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let desc_style = Style::default();
    let sep_style = Style::default().fg(Color::DarkGray);

    let rows: Vec<ListItem> = BINDINGS
        .iter()
        .map(|(key, desc)| {
            ListItem::new(Line::from(vec![
                Span::styled(format!("  {:<18}", key), key_style),
                Span::styled("│  ", sep_style),
                Span::styled(*desc, desc_style),
            ]))
        })
        .collect();

    let list = List::new(rows);
    f.render_widget(list, inner);

    let footer = Paragraph::new(" press any key to close ")
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    f.render_widget(
        footer,
        Rect {
            x: inner.x,
            y: popup.y + popup.height - 2,
            width: inner.width,
            height: 1,
        },
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

fn vt100_color(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(n) => Some(Color::Indexed(n)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut s = Style::default();
    if let Some(fg) = vt100_color(cell.fgcolor()) {
        s = s.fg(fg);
    }
    if let Some(bg) = vt100_color(cell.bgcolor()) {
        s = s.bg(bg);
    }
    if cell.bold() {
        s = s.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        s = s.add_modifier(Modifier::REVERSED);
    }
    s
}

fn style_preserves_spaces(style: Style) -> bool {
    style.bg.is_some() || style.add_modifier.contains(Modifier::REVERSED)
}

/// Build one display row, applying per-cell colors, selection highlight, and cursor.
fn build_row(
    screen: &Screen,
    vt_row: u16,
    screen_cols: u16,
    disp_row: u16,
    sel: Option<&Selection>,
    cursor_col: Option<u16>,
) -> ListItem<'static> {
    let sel_style = Style::default().bg(Color::Blue).fg(Color::White);
    let cursor_style = Style::default().add_modifier(Modifier::REVERSED);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut cur_style: Option<Style> = None;

    for col in 0..screen_cols {
        let is_sel = sel.map_or(false, |s| s.contains(disp_row, col));
        let is_cursor = cursor_col == Some(col);
        let (content, style) = match screen.cell(vt_row, col) {
            Some(cell) => {
                let s = cell.contents();
                let content = if s.is_empty() { " ".to_string() } else { s };
                let style = if is_sel {
                    sel_style
                } else if is_cursor {
                    cursor_style
                } else {
                    cell_style(cell)
                };
                (content, style)
            }
            None => {
                let style = if is_cursor {
                    cursor_style
                } else {
                    Style::default()
                };
                (" ".to_string(), style)
            }
        };

        if Some(style) != cur_style {
            if !run.is_empty() {
                spans.push(Span::styled(run.clone(), cur_style.unwrap_or_default()));
                run.clear();
            }
            cur_style = Some(style);
        }
        run.push_str(&content);
    }

    let final_style = cur_style.unwrap_or_default();
    let trimmed = if style_preserves_spaces(final_style) {
        run
    } else {
        run.trim_end().to_string()
    };
    if !trimmed.is_empty() {
        spans.push(Span::styled(trimmed, final_style));
    }

    ListItem::new(Line::from(spans))
}

fn centered_rect(percent_x: u16, height: u16, r: Rect) -> Rect {
    let w = r.width * percent_x / 100;
    let x = r.x + (r.width - w) / 2;
    let y = r.y + (r.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width: w,
        height: height.min(r.height),
    }
}
