use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

use crate::app::{App, AppMode, FileBrowserState, MenuAction, NewSessionField, Selection};
use crate::layout::LayoutTree;
use crate::session::{SessionKind, SessionState};
use crate::theme::Theme;
use vt100::Screen;

#[derive(Default)]
pub struct LayoutInfo {
    pub output_areas: Vec<Rect>,
    pub session_bar_area: Rect,
    pub session_slot_areas: Vec<Rect>,
    pub status_row_areas: Vec<Rect>,
    pub new_session_area: Rect,
    pub browse_button_area: Rect,
    pub file_browser_area: Rect,
    pub command_bar_area: Rect,
    pub help_area: Rect,
    pub chat_area: Rect,
    pub chat_transcript_area: Rect,
    pub chat_scroll_max: usize,
    pub chat_visible_lines: Vec<String>,
    pub menu_bar_area: Rect,
    pub menu_item_areas: Vec<Rect>,
    pub menu_submenu_area: Rect,
    pub menu_submenu_item_areas: Vec<Rect>,
}

/// Strip ANSI/VT escape sequences and prepare a line for safe ratatui rendering.
///
/// If raw escape sequences reach ratatui they get forwarded to the terminal,
/// where the terminal interprets them (e.g. \x1b[2J clears the screen mid-draw).
/// This strips CSI, OSC, and other escape sequences, expands tabs, and drops
/// remaining non-printable control characters.
// Only called from #[cfg(test)]; kept here (not inside the mod) so it can test internal logic.
#[cfg_attr(not(test), allow(dead_code))]
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

fn kind_color(t: &Theme, kind: &SessionKind) -> Color {
    match kind {
        SessionKind::Claude => t.kind_claude,
        SessionKind::Codex => t.kind_codex,
        SessionKind::OpenCode => t.kind_opencode,
        SessionKind::OhMyPi => t.kind_ohmypi,
        SessionKind::Aider => t.kind_aider,
        SessionKind::Shell => t.kind_shell,
        SessionKind::Custom(_) => t.kind_custom,
    }
}

fn state_border_style(t: &Theme, state: &SessionState, active: bool) -> Style {
    match state {
        SessionState::Waiting => Style::default().fg(t.warn).add_modifier(Modifier::BOLD),
        SessionState::Error => Style::default()
            .fg(t.err)
            .add_modifier(Modifier::RAPID_BLINK),
        SessionState::Dead => Style::default().fg(t.text_dim),
        _ if active => Style::default()
            .fg(t.text_bright)
            .add_modifier(Modifier::BOLD),
        _ => Style::default().fg(t.text_dim),
    }
}

/// Smallest terminal the main layout can be solved for: the vertical split
/// below asks for a 5-row main pane, a 3-row session bar and a status panel of
/// at least 4 rows. Under that, ratatui's solver starts handing back
/// zero-height rects and the geometry arithmetic downstream has nothing valid
/// to work from.
const MIN_ROWS: u16 = 12;
const MIN_COLS: u16 = 20;

pub fn draw(f: &mut Frame<'_>, app: &App) -> LayoutInfo {
    let size = f.size();
    if size.height < MIN_ROWS || size.width < MIN_COLS {
        draw_too_small(f, &app.theme, size);
        // An empty LayoutInfo is safe: every consumer either iterates these
        // vectors or length-checks before indexing, so hit-testing simply finds
        // nothing until the terminal is large enough to lay out again.
        return LayoutInfo::default();
    }
    let menu_open = matches!(app.mode, AppMode::Menu { .. });
    let mut menu_bar_area = Rect::default();
    let mut menu_item_areas = Vec::new();
    let mut menu_submenu_area = Rect::default();
    let mut menu_submenu_item_areas = Vec::new();
    let body = if menu_open {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(size);
        menu_bar_area = split[0];
        split[1]
    } else {
        size
    };

    // ── Top-level vertical split ───────────────────────────────────────────
    // main output | session bar | status panel
    let orch_row = if app.orchestrator.is_some() || app.orchestrator_session_id.is_some() {
        1u16
    } else {
        0
    };
    let desired_status_rows = app.visible_indices().len().max(1) as u16 + 4 + orch_row;
    let capped = desired_status_rows.min((body.height / 3).max(4));
    // Hysteresis so a changing row count can't oscillate the pane layout
    // (and with it the sessions' PTY sizes).
    let status_rows = app
        .stabilized_status_rows(capped)
        .min((body.height / 3).max(4));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),              // main output
            Constraint::Length(3),           // session bar
            Constraint::Length(status_rows), // status panel
        ])
        .split(body);

    let mut chat_area = Rect::default();
    let mut chat_layout = ChatLayout::default();

    // Fullscreen planning claims the whole output region, but only that
    // region: the session bar and status panel stay, so the sessions you are
    // planning against remain visible and their state keeps updating.
    let planning_fullscreen = app.planning_fullscreen && app.planning_docked.is_some();
    let output_areas = if planning_fullscreen {
        draw_planning_in(f, app, chunks[0], true);
        // No pane rects: hit-testing must find nothing where no session was
        // drawn, and the PTYs keep the size they last laid out at.
        Vec::new()
    } else {
        split_output_areas(chunks[0], &app.tree)
    };
    for (pane_idx, area) in output_areas.iter().copied().enumerate() {
        if app.chat_docked == Some(pane_idx) && output_areas.len() > 1 {
            chat_layout = draw_chat_in(f, app, area, pane_idx == app.focused_pane);
            chat_area = chat_layout.area;
        } else if app.planning_docked == Some(pane_idx) && output_areas.len() > 1 {
            draw_planning_in(f, app, area, pane_idx == app.focused_pane);
        } else {
            draw_pane_output(f, app, area, pane_idx, pane_idx == app.focused_pane);
        }
    }
    let slot_areas = draw_session_bar(f, app, chunks[1]);
    let status_row_areas = draw_status_panel(f, app, chunks[2]);

    // ── Overlays ───────────────────────────────────────────────────────────
    let mut new_session_area = Rect::default();
    let mut browse_button_area = Rect::default();
    let mut file_browser_area = Rect::default();
    let mut command_bar_area = Rect::default();
    let mut help_area = Rect::default();

    match &app.mode {
        AppMode::NewSession => {
            let ns_result = draw_new_session_dialog(f, app, size);
            new_session_area = ns_result.0;
            browse_button_area = ns_result.1;
        }
        AppMode::FileBrowser => {
            let ns_result = draw_new_session_dialog(f, app, size);
            new_session_area = ns_result.0;
            browse_button_area = ns_result.1;
            file_browser_area = draw_file_browser(f, &app.theme, &app.file_browser_state, size);
        }
        AppMode::CommandBar => {
            command_bar_area = draw_command_bar(f, app, size);
        }
        AppMode::CommandResult => {
            draw_command_result(f, app, size);
        }
        AppMode::Help => {
            help_area = draw_help(f, &app.theme, size);
        }
        AppMode::PipeList => {
            help_area = draw_pipe_list(f, app, size);
        }
        AppMode::Chat => {
            chat_layout = draw_chat(f, app, size);
            chat_area = chat_layout.area;
        }
        AppMode::OrchestratorModel { selected } => {
            help_area = draw_orchestrator_model_picker(f, app, size, *selected);
        }
        AppMode::Menu { .. } => {
            let menu = draw_menu_bar(f, app, menu_bar_area);
            menu_item_areas = menu.0;
            menu_submenu_area = menu.1;
            menu_submenu_item_areas = menu.2;
        }
        AppMode::Search { .. } => {
            help_area = draw_search_overlay(f, app, size);
        }
        AppMode::Settings => {
            help_area = draw_settings_overlay(f, app, size);
        }
        AppMode::Normal => {}
    }

    LayoutInfo {
        output_areas,
        session_bar_area: chunks[1],
        session_slot_areas: slot_areas,
        status_row_areas,
        new_session_area,
        browse_button_area,
        file_browser_area,
        command_bar_area,
        help_area,
        chat_area,
        chat_transcript_area: chat_layout.transcript_area,
        chat_scroll_max: chat_layout.scroll_max,
        chat_visible_lines: chat_layout.visible_lines,
        menu_bar_area,
        menu_item_areas,
        menu_submenu_area,
        menu_submenu_item_areas,
    }
}

fn split_output_areas(area: Rect, tree: &LayoutTree) -> Vec<Rect> {
    tree.rects(area)
}

// ── Main output zone ───────────────────────────────────────────────────────

fn draw_pane_output(f: &mut Frame<'_>, app: &App, area: Rect, pane_idx: usize, focused: bool) {
    let t = &app.theme;
    let (title, lines, border_style) = if let Some(idx) = app.panes[pane_idx] {
        let session = &app.sessions[idx];
        let screen = session.screen.screen();
        let (screen_rows, screen_cols) = screen.size();
        let display_rows = area.height.saturating_sub(2);
        let scroll_offset = screen.scrollback().max(session.history_scroll) as u16;
        // vt100 handles the scrollback offset internally via set_scrollback;
        // just display the bottom display_rows rows of the virtual screen.
        let start_row = screen_rows.saturating_sub(display_rows);
        let end_row = screen_rows;
        let scroll_indicator = if scroll_offset > 0 {
            format!(" ↑{}", scroll_offset)
        } else {
            String::new()
        };
        let title = format!(
            " {} [{}] {}{} ",
            idx + 1,
            session.kind.label().to_uppercase(),
            session.name,
            scroll_indicator,
        );
        let sel = if focused {
            app.selection.as_ref()
        } else {
            None
        };
        let cursor = screen.cursor_position();
        let items: Vec<ListItem> = if session.history_scroll > 0 {
            // Alternate-screen apps have no vt100 scrollback; show a window of
            // our captured line history instead. history_scroll counts lines
            // up from the tail of output_lines.
            let total = session.output_lines.len();
            let end = total.saturating_sub(session.history_scroll);
            let start = end.saturating_sub(display_rows as usize);
            session
                .output_lines
                .iter()
                .skip(start)
                .take(end - start)
                .map(|l| {
                    ListItem::new(Line::from(Span::styled(
                        l.clone(),
                        Style::default().fg(t.text),
                    )))
                })
                .collect()
        } else {
            (start_row..end_row)
                .enumerate()
                .map(|(disp_row, vt_row)| {
                    let disp_row = disp_row as u16;
                    // Only show cursor when at the live tail
                    let cursor_col = if scroll_offset == 0 && vt_row == cursor.0 {
                        Some(cursor.1)
                    } else {
                        None
                    };
                    build_row(t, screen, vt_row, screen_cols, disp_row, sel, cursor_col)
                })
                .collect()
        };
        let style = state_border_style(t, &session.state, focused);
        (title, items, style)
    } else {
        let message = if app.sessions.is_empty() {
            " No sessions. Press alt-n to create one."
        } else {
            " No session in this pane. Use a session switch key."
        };
        let items = vec![ListItem::new(Line::from(message))];
        (
            if app.sessions.is_empty() {
                " linkshell ".to_string()
            } else {
                " no session ".to_string()
            },
            items,
            Style::default().fg(t.text_dim),
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
    let t = &app.theme;
    // Slots (and the returned click rects) cover visible sessions only;
    // mouse handling maps a slot position back through visible_to_idx.
    let visible = app.visible_indices();
    let n = visible.len();

    // outer block
    let outer = Block::default().borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM);
    f.render_widget(outer, area);

    // Broadcast mode indicator
    if app.broadcast_mode {
        let indicator = Rect {
            x: area.x + 1,
            y: area.y,
            width: 13,
            height: 1,
        };
        f.render_widget(
            Paragraph::new("[BROADCAST]")
                .style(Style::default().fg(t.err).add_modifier(Modifier::BOLD)),
            indicator,
        );
    }

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

    for (i, &idx) in visible.iter().enumerate() {
        let session = &app.sessions[idx];
        let slot = Rect {
            x: offset_x + i as u16 * slot_w,
            y: inner.y,
            width: slot_w,
            height: inner.height,
        };

        let is_active = app.active_idx() == Some(idx);
        let is_visible = app.panes.contains(&Some(idx));
        let label = format!("{} {}", i + 1, session.kind.label());
        let color = kind_color(t, &session.kind);
        let border_style = state_border_style(t, &session.state, is_active);

        let title_style = if is_active {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        } else if is_visible {
            Style::default()
                .fg(color)
                .add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default().fg(color)
        };

        let block = Block::default()
            .title(Span::styled(label, title_style))
            .borders(Borders::ALL)
            .border_style(border_style);

        // State dot inside slot
        let state_dot = if session.paused {
            Span::styled("⏸", Style::default().fg(t.text_dim))
        } else {
            match session.state {
                SessionState::Waiting => Span::styled("⚡", Style::default().fg(t.warn)),
                SessionState::Error => Span::styled("✗", Style::default().fg(t.err)),
                SessionState::Thinking => Span::styled("…", Style::default().fg(t.kind_claude)),
                SessionState::Running => Span::styled("▶", Style::default().fg(t.ok)),
                SessionState::Ready => Span::styled("●", Style::default().fg(t.ok)),
                SessionState::Dead => Span::styled("✗", Style::default().fg(t.text_dim)),
                SessionState::Starting => Span::styled("○", Style::default().fg(t.text)),
            }
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

/// Shorten a model ID for the 12-char status column: drop the "claude-"
/// prefix and any trailing -YYYYMMDD date stamp, then truncate.
fn model_display(model: Option<&str>) -> String {
    let Some(model) = model else {
        return "-".to_string();
    };
    let mut m = model.strip_prefix("claude-").unwrap_or(model);
    if let Some(idx) = m.rfind('-') {
        let tail = &m[idx + 1..];
        if tail.len() == 8 && tail.chars().all(|c| c.is_ascii_digit()) {
            m = &m[..idx];
        }
    }
    m.chars().take(12).collect()
}

/// Status-panel row data for the orchestrator agent, whichever flavor runs.
struct OrchRow {
    dot: &'static str,
    dot_style: Style,
    name: String,
    model: Option<String>,
    state: String,
    state_style: Style,
    tokens: String,
    ctx: String,
    cost: String,
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn orchestrator_row(app: &App) -> Option<OrchRow> {
    let t = &app.theme;
    let green = Style::default().fg(t.ok);
    let red = Style::default().fg(t.err).add_modifier(Modifier::BOLD);

    if let Some(h) = &app.orchestrator {
        // API-class: in-process task. Failed when its channel is gone.
        let alive = !h.tx.is_closed();
        let busy = app.orchestrator_status.is_some();
        let stats = &app.orchestrator_stats;
        let total = stats.input_tokens + stats.output_tokens;
        return Some(OrchRow {
            dot: "●",
            dot_style: if alive { green } else { red },
            name: h.name.clone(),
            model: Some(app.config.orchestrator.model.clone()).filter(|m| !m.is_empty()),
            state: if !alive {
                "DEAD".into()
            } else if app.orchestrator_paused {
                "PAUSED".into()
            } else if busy {
                "BUSY".into()
            } else {
                "IDLE".into()
            },
            state_style: if !alive {
                red
            } else if app.orchestrator_paused {
                Style::default().fg(t.text_dim).add_modifier(Modifier::BOLD)
            } else if busy {
                Style::default().fg(t.kind_claude)
            } else {
                Style::default().fg(t.text_dim)
            },
            tokens: if total == 0 {
                "—".into()
            } else {
                crate::session::fmt_count(total)
            },
            ctx: match (stats.context_tokens, app.orchestrator_ctx_max) {
                (0, None) => "—".into(),
                (c, None) => crate::session::fmt_count(c),
                (c, Some(m)) => format!(
                    "{}/{}",
                    crate::session::fmt_count(c),
                    crate::session::fmt_count(m)
                ),
            },
            cost: if stats.total_cost_usd > 0.0 {
                format!("${:.3}", stats.total_cost_usd)
            } else {
                "—".into()
            },
        });
    }

    // CLI-class: a (usually hidden) session drives a real CLI.
    let sid = app.orchestrator_session_id?;
    let s = app.sessions.iter().find(|s| s.id == sid)?;
    let failed = matches!(s.state, SessionState::Error | SessionState::Dead);
    Some(OrchRow {
        dot: "●",
        dot_style: if failed { red } else { green },
        name: s.name.clone(),
        model: s.model.clone(),
        state: s.state_label().to_string(),
        state_style: if failed {
            red
        } else if s.paused {
            Style::default().fg(t.text_dim).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t.ok)
        },
        tokens: s.tokens_display(),
        ctx: s.context_display(),
        cost: s.cost_display(),
    })
}

fn draw_status_panel(f: &mut Frame<'_>, app: &App, area: Rect) -> Vec<Rect> {
    let t = &app.theme;
    let title = match &app.council {
        Some(r) if r.complete => format!(" Status ── council '{}' done ", r.group),
        Some(r) => format!(
            " Status ── council '{}' round {}/{} ",
            r.group, r.round, r.max_rounds
        ),
        None => " Status ".to_string(),
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM);

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Header row
    if inner.height > 0 {
        let hdr_style = Style::default().fg(t.text_dim).add_modifier(Modifier::BOLD);
        let header_spans = vec![
            Span::styled("  ⏻ ", hdr_style),
            Span::styled("│ ", hdr_style),
            Span::styled(format!("{:<8} ", "Kind"), hdr_style),
            Span::styled("│ ", hdr_style),
            Span::styled(format!("{:<12} ", "Model"), hdr_style),
            Span::styled("│ ", hdr_style),
            Span::styled(format!("{:<20} ", "Pipe"), hdr_style),
            Span::styled("│ ", hdr_style),
            Span::styled(format!("{:<8} ", "State"), hdr_style),
            Span::styled("│ ", hdr_style),
            Span::styled(format!("{:>6}  ", "Time"), hdr_style),
            Span::styled("│ ", hdr_style),
            Span::styled(format!("{:>6}  ", "Tokens"), hdr_style),
            Span::styled("│ ", hdr_style),
            Span::styled(format!("{:>13}  ", "Ctx"), hdr_style),
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

    let mut row_areas = Vec::new();
    let mut row_y = inner.y + 1;
    // Rows (and the returned click rects) cover visible sessions only, in
    // the same order as the session bar slots.
    for idx in app.visible_indices() {
        let session = &app.sessions[idx];
        if row_y >= inner.y + inner.height {
            break;
        }

        let row = Rect {
            x: inner.x,
            y: row_y,
            width: inner.width,
            height: 1,
        };
        row_y += 1;
        row_areas.push(row);

        // Health indicator: red when the agent has failed (Error/Dead),
        // green once it is connected and healthy again.
        let (health_dot, health_style) = match session.state {
            SessionState::Error | SessionState::Dead => {
                ("●", Style::default().fg(t.err).add_modifier(Modifier::BOLD))
            }
            SessionState::Starting => ("○", Style::default().fg(t.text)),
            _ => ("●", Style::default().fg(t.ok)),
        };
        let kind_style = Style::default().fg(kind_color(t, &session.kind));
        let state_style = if session.paused {
            Style::default().fg(t.text_dim).add_modifier(Modifier::BOLD)
        } else {
            match session.state {
                SessionState::Waiting => Style::default().fg(t.warn).add_modifier(Modifier::BOLD),
                SessionState::Error => Style::default().fg(t.err).add_modifier(Modifier::BOLD),
                SessionState::Thinking => Style::default().fg(t.kind_claude),
                SessionState::Running => Style::default().fg(t.ok),
                SessionState::Ready => Style::default().fg(t.text_dim),
                SessionState::Dead => Style::default().fg(t.text_dim),
                SessionState::Starting => Style::default().fg(t.text),
            }
        };

        let tokens = session.tokens_display();
        let context = session.context_display();
        let cost = session.cost_display();
        let elapsed = session.elapsed_display();

        let glyphs = app.pipe_summary_for(session.id, std::time::Instant::now());
        let mut labels: Vec<String> = glyphs
            .iter()
            .take(2)
            .map(|glyph| {
                format!(
                    "{}{}{}",
                    if glyph.outgoing { "→" } else { "←" },
                    glyph.peer,
                    if glyph.recent { " ●" } else { "" }
                )
            })
            .collect();
        if glyphs.len() > 2 {
            labels.push(format!("+{}", glyphs.len() - 2));
        }
        let pipe_label = labels.join(",");
        let pipe_recently_fired = glyphs.iter().any(|glyph| glyph.recent);
        let all_inactive = !glyphs.is_empty() && glyphs.iter().all(|glyph| !glyph.active);

        let spans = vec![
            Span::styled(format!("  {health_dot} "), health_style),
            Span::raw("│ "),
            Span::styled(format!("{:<8} ", session.kind.label()), kind_style),
            Span::raw("│ "),
            Span::styled(
                format!("{:<12} ", model_display(session.model.as_deref())),
                Style::default().fg(t.text),
            ),
            Span::raw("│ "),
            Span::styled(format!("{:<20} ", pipe_label), {
                let s = Style::default().fg(t.info);
                if all_inactive {
                    s.add_modifier(Modifier::DIM)
                } else if pipe_recently_fired {
                    s.add_modifier(Modifier::BOLD)
                } else {
                    s
                }
            }),
            Span::raw("│ "),
            Span::styled(format!("{:<8} ", session.state_label()), state_style),
            Span::raw("│ "),
            Span::styled(format!("{:>6}  ", elapsed), Style::default().fg(t.text)),
            Span::raw("│ "),
            Span::styled(format!("{:>6}  ", tokens), Style::default().fg(t.info)),
            Span::raw("│ "),
            Span::styled(format!("{:>13}  ", context), Style::default().fg(t.ctx)),
            Span::raw("│ "),
            Span::styled(format!("{:>7}", cost), Style::default().fg(t.ok)),
        ];

        let line = Paragraph::new(Line::from(spans));
        f.render_widget(line, row);
    }

    // Orchestrator agent row — after the session rows so the returned click
    // rects still map 1:1 onto visible sessions. Covers both flavors: the
    // in-process API-class handle and the hidden CLI-class session.
    let orch = orchestrator_row(app);
    if let Some(o) = orch {
        if row_y < inner.y + inner.height {
            let row = Rect {
                x: inner.x,
                y: row_y,
                width: inner.width,
                height: 1,
            };
            let spans = vec![
                Span::styled(format!("  {} ", o.dot), o.dot_style),
                Span::raw("│ "),
                Span::styled(
                    format!("{:<8} ", truncate(&o.name, 8)),
                    Style::default()
                        .fg(t.kind_orch)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("│ "),
                Span::styled(
                    format!("{:<12} ", model_display(o.model.as_deref())),
                    Style::default().fg(t.text),
                ),
                Span::raw("│ "),
                Span::styled(
                    format!("{:<20} ", "orchestrator"),
                    Style::default().fg(t.text_dim),
                ),
                Span::raw("│ "),
                Span::styled(format!("{:<8} ", o.state), o.state_style),
                Span::raw("│ "),
                Span::styled(format!("{:>6}  ", ""), Style::default().fg(t.text)),
                Span::raw("│ "),
                Span::styled(format!("{:>6}  ", o.tokens), Style::default().fg(t.info)),
                Span::raw("│ "),
                Span::styled(format!("{:>13}  ", o.ctx), Style::default().fg(t.ctx)),
                Span::raw("│ "),
                Span::styled(format!("{:>7}", o.cost), Style::default().fg(t.ok)),
            ];
            f.render_widget(Paragraph::new(Line::from(spans)), row);
        }
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
            Span::styled("  sock: ", Style::default().fg(t.text_dim)),
            Span::styled(
                sock,
                Style::default().fg(t.text_dim).add_modifier(Modifier::DIM),
            ),
        ]));
        f.render_widget(footer, footer_row);
    }

    row_areas
}

// ── New session dialog ─────────────────────────────────────────────────────

/// Returns (popup_rect, browse_button_rect).
pub fn draw_new_session_dialog(f: &mut Frame<'_>, app: &App, area: Rect) -> (Rect, Rect) {
    let t = &app.theme;
    let ns = &app.new_session_state;
    let height = if ns.is_custom() { 16 } else { 13 };
    let popup = centered_rect(50, height, area);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .title(" New Session ")
        .borders(Borders::ALL);
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    // Kind dropdown field (closed state; the expanded list is drawn last so
    // it overlays the fields below).
    let kind_active = ns.active_field == NewSessionField::Kind;
    let kind_style = if kind_active {
        Style::default()
            .fg(t.text_bright)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t.text_dim)
    };
    let kind_label = crate::session::KIND_LABELS
        .get(ns.selected_kind)
        .copied()
        .unwrap_or("?");
    let arrow = if ns.kind_dropdown_open { "▴" } else { "▾" };
    let kind_field = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 3,
    };
    let kind_block = Block::default()
        .title(" Kind ")
        .borders(Borders::ALL)
        .border_style(kind_style);
    let kind_text = Line::from(vec![
        Span::styled(format!(" {}", kind_label), Style::default().fg(t.warn)),
        Span::styled(format!("  {}", arrow), Style::default().fg(t.text_dim)),
    ]);
    f.render_widget(Paragraph::new(kind_text).block(kind_block), kind_field);

    let fields_y = inner.y + 3;

    // Name field
    draw_input_field(
        f,
        t,
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

    // CWD field — input shrunk to leave room for [Browse] button
    const BROWSE_BTN_W: u16 = 10;
    let cwd_input_w = inner.width.saturating_sub(BROWSE_BTN_W);
    draw_input_field(
        f,
        t,
        "CWD",
        &ns.cwd,
        ns.cwd_cursor,
        Rect {
            x: inner.x,
            y: fields_y + 3,
            width: cwd_input_w,
            height: 3,
        },
        ns.active_field == NewSessionField::Cwd,
    );

    // [Browse] button
    let browse_area = Rect {
        x: inner.x + cwd_input_w,
        y: fields_y + 3,
        width: BROWSE_BTN_W,
        height: 3,
    };
    let browse_style = Style::default().fg(t.info);
    let browse_block = Block::default()
        .borders(Borders::ALL)
        .border_style(browse_style);
    let browse_btn = Paragraph::new("Browse")
        .block(browse_block)
        .alignment(Alignment::Center);
    f.render_widget(browse_btn, browse_area);

    // Custom cmd field (only when Custom selected)
    if ns.is_custom() {
        draw_input_field(
            f,
            t,
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
    let hint_text = if ns.kind_dropdown_open {
        " ↑/↓: choose kind  Enter: select  Esc: close "
    } else {
        " Tab: next field  Alt+B: browse  Enter: create  Esc: cancel "
    };
    let hint = Paragraph::new(hint_text)
        .style(Style::default().fg(t.text_dim))
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

    // Expanded dropdown list, drawn last so it overlays the fields below.
    if ns.kind_dropdown_open {
        let list = kind_dropdown_list_rect(popup).intersection(area);
        f.render_widget(Clear, list);
        let list_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(t.text_bright));
        let list_inner = list_block.inner(list);
        f.render_widget(list_block, list);
        for (i, label) in crate::session::KIND_LABELS.iter().enumerate() {
            let y = list_inner.y + i as u16;
            if y >= list_inner.y + list_inner.height {
                break;
            }
            let style = if i == ns.selected_kind {
                Style::default()
                    .fg(t.on_accent)
                    .bg(t.text_bright)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.text)
            };
            let row = Rect {
                x: list_inner.x,
                y,
                width: list_inner.width,
                height: 1,
            };
            f.render_widget(Paragraph::new(format!(" {}", label)).style(style), row);
        }
    }

    (popup, browse_area)
}

/// Screen rect of the expanded Kind dropdown list, anchored below the Kind
/// field. Shared with the mouse handler in `app.rs` so hit-testing and
/// rendering can never drift apart.
pub fn kind_dropdown_list_rect(popup: Rect) -> Rect {
    let inner_x = popup.x + 1;
    let inner_y = popup.y + 1;
    let inner_w = popup.width.saturating_sub(2);
    Rect {
        x: inner_x,
        y: inner_y + 2, // overlap the Kind field's bottom border
        width: inner_w,
        height: crate::session::SessionKind::COUNT as u16 + 2,
    }
}

// ── File browser overlay ───────────────────────────────────────────────────

pub const FILE_BROWSER_VISIBLE_ROWS: usize = 16;

pub fn draw_file_browser(
    f: &mut Frame<'_>,
    t: &Theme,
    state: &FileBrowserState,
    area: Rect,
) -> Rect {
    const VISIBLE_ROWS: u16 = FILE_BROWSER_VISIBLE_ROWS as u16;
    let popup = centered_rect(60, VISIBLE_ROWS + 6, area);
    f.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Browse Directory ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t.info));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    // Current path line
    let path_str = state.current_dir.to_string_lossy();
    let path_line = Paragraph::new(path_str.as_ref()).style(Style::default().fg(t.warn));
    f.render_widget(
        path_line,
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        },
    );

    // Separator
    let sep =
        Paragraph::new("─".repeat(inner.width as usize)).style(Style::default().fg(t.text_dim));
    f.render_widget(
        sep,
        Rect {
            x: inner.x,
            y: inner.y + 1,
            width: inner.width,
            height: 1,
        },
    );

    // Directory list
    let list_area = Rect {
        x: inner.x,
        y: inner.y + 2,
        width: inner.width,
        height: VISIBLE_ROWS,
    };

    let scroll = state.scroll_offset;
    let visible = VISIBLE_ROWS as usize;
    let items: Vec<ListItem> = (scroll..state.entries.len().min(scroll + visible))
        .map(|i| {
            let label = state.entry_label(i);
            let style = if i == state.selected {
                Style::default()
                    .fg(t.on_accent)
                    .bg(t.info)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.text_bright)
            };
            ListItem::new(format!(" {label}")).style(style)
        })
        .collect();

    let list = List::new(items);
    f.render_widget(list, list_area);

    // Footer
    let footer =
        Paragraph::new(" ↑↓: navigate  Enter: open dir  Space: select current  Esc: cancel ")
            .style(Style::default().fg(t.text_dim))
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

    popup
}

fn draw_input_field(
    f: &mut Frame<'_>,
    t: &Theme,
    label: &str,
    value: &str,
    cursor_pos: usize,
    area: Rect,
    active: bool,
) {
    let style = if active {
        Style::default().fg(t.info)
    } else {
        Style::default().fg(t.text_dim)
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
        let cursor_style = Style::default().bg(t.info).fg(t.on_accent);
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

fn draw_help(f: &mut Frame<'_>, t: &Theme, area: Rect) -> Rect {
    const BINDINGS: &[(&str, &str)] = &[
        ("alt-n", "New session dialog"),
        ("alt-tab", "Next session"),
        ("alt-shift-tab", "Previous session"),
        ("alt-1 … alt-8", "Switch to session by number"),
        ("alt-← / alt-→", "Cycle sessions in focused pane"),
        ("alt-shift-arrows", "Focus pane in direction"),
        ("alt-x", "Kill active session"),
        ("alt-c", "Open command bar"),
        ("alt-t", "Toggle agent chat pane"),
        ("alt-g", "Dock/undock chat as a split pane"),
        ("alt-\\ / alt--", "Split focused pane right / down"),
        ("alt-w / alt-r / alt-o", "Close / rotate / focus next pane"),
        ("alt-shift-pgup/pgdn", "Scroll output (any session)"),
        (
            "alt-p / alt-shift-p",
            "Planning pane: dock / fill output area",
        ),
        ("alt-h", "Show this help"),
        ("ctrl-space", "Toggle menu bar"),
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

    let key_style = Style::default().fg(t.warn).add_modifier(Modifier::BOLD);
    let desc_style = Style::default();
    let sep_style = Style::default().fg(t.text_dim);

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
        .style(Style::default().fg(t.text_dim))
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

    popup
}

fn draw_pipe_list(f: &mut Frame<'_>, app: &App, area: Rect) -> Rect {
    let t = &app.theme;
    let height = (app.pipes.len() as u16 + 4).clamp(6, 18);
    let popup = centered_rect(90, height, area);
    f.render_widget(Clear, popup);
    let rows: Vec<Line<'_>> = if app.pipes.is_empty() {
        vec![Line::from("No pipes configured")]
    } else {
        app.pipes
            .iter()
            .enumerate()
            .map(|(index, pipe)| {
                let name = |id| {
                    app.sessions
                        .iter()
                        .find(|session| session.id == id)
                        .map(|session| session.name.as_str())
                        .unwrap_or("?")
                };
                let trigger = match pipe.trigger {
                    crate::pipe::PipeTrigger::OnReady => "on_ready",
                    crate::pipe::PipeTrigger::OnWaiting => "on_waiting",
                    crate::pipe::PipeTrigger::Manual => "manual",
                };
                let extract = match pipe.extract {
                    crate::pipe::ExtractMode::LastBlock => "last_block".into(),
                    crate::pipe::ExtractMode::LastN(n) => format!("last:{n}"),
                    crate::pipe::ExtractMode::Diff => "diff".into(),
                    crate::pipe::ExtractMode::Summarize(n) => format!("summarize:{n}"),
                };
                let fired = pipe
                    .last_fired
                    .map(|instant| format!("{}s ago", instant.elapsed().as_secs()))
                    .unwrap_or_else(|| "never".into());
                let text = format!(
                    "{} → {} │ {} │ {} │ {} │ {} │ {}",
                    name(pipe.source),
                    name(pipe.dest),
                    trigger,
                    extract,
                    pipe.prefix.as_deref().unwrap_or("—"),
                    fired,
                    if pipe.active { "active" } else { "paused" },
                );
                let style = if index == app.pipe_list_selected {
                    Style::default().fg(t.on_accent).bg(t.info)
                } else if !pipe.active {
                    Style::default().fg(t.text).add_modifier(Modifier::DIM)
                } else {
                    Style::default()
                };
                Line::styled(text, style)
            })
            .collect()
    };
    let widget = Paragraph::new(rows).block(
        Block::default()
            .title(" Pipes — ↑/↓ select  Space toggle  Enter fire  d delete ")
            .borders(Borders::ALL),
    );
    f.render_widget(widget, popup);
    popup
}

// ── Chat pane ───────────────────────────────────────────────────────────────

/// Greedy word wrap; falls back to hard breaks for unbroken runs.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    for raw_line in text.lines() {
        let mut line = String::new();
        for word in raw_line.split(' ') {
            let candidate_len = if line.is_empty() {
                word.chars().count()
            } else {
                line.chars().count() + 1 + word.chars().count()
            };
            if candidate_len <= width {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(word);
            } else {
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                // Hard-break words longer than the width
                let mut w: Vec<char> = word.chars().collect();
                while w.len() > width {
                    out.push(w.drain(..width).collect());
                }
                line = w.into_iter().collect();
            }
        }
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn chat_from_style(t: &Theme, from: &str) -> Style {
    if from.starts_with("you") {
        Style::default().fg(t.info).add_modifier(Modifier::BOLD)
    } else if from == "linkshell" {
        Style::default().fg(t.text_dim)
    } else {
        Style::default().fg(t.warn).add_modifier(Modifier::BOLD)
    }
}

#[derive(Default)]
struct ChatLayout {
    area: Rect,
    transcript_area: Rect,
    scroll_max: usize,
    visible_lines: Vec<String>,
}

/// Overlay REVERSED onto the char columns of `line` covered by the selection
/// on this row. `from`/`to` are inclusive char columns; `None` means the
/// selection extends past that edge of the row.
fn highlight_line(line: Line<'static>, from: Option<usize>, to: Option<usize>) -> Line<'static> {
    let mut spans: Vec<Span> = Vec::new();
    let mut col = 0usize;
    for span in line.spans {
        let style = span.style;
        for c in span.content.chars() {
            let selected =
                from.map(|s| col >= s).unwrap_or(true) && to.map(|e| col <= e).unwrap_or(true);
            let style = if selected {
                style.add_modifier(Modifier::REVERSED)
            } else {
                style
            };
            match spans.last_mut() {
                Some(last) if last.style == style => last.content.to_mut().push(c),
                _ => spans.push(Span::styled(c.to_string(), style)),
            }
            col += 1;
        }
    }
    Line::from(spans)
}

fn draw_chat(f: &mut Frame<'_>, app: &App, area: Rect) -> ChatLayout {
    // Configurable via [chat] width_pct / height_pct (defaults 60×60).
    // centered_rect takes height in rows, so convert the percentage here —
    // the old hardcoded (86, 86) passed 86 *rows*, which clamped to full
    // height on any normal-sized terminal.
    let width_pct = app.config.chat.width_pct.clamp(20, 95);
    let height_pct = app.config.chat.height_pct.clamp(20, 95);
    let height_rows = (area.height as u32 * height_pct as u32 / 100).max(8) as u16;
    let popup = centered_rect(width_pct, height_rows, area);
    draw_chat_in(f, app, popup, true)
}

/// Render the chat into an exact rect — used by both the centered overlay
/// and the docked split pane. `focused` drives the border colour.
fn draw_chat_in(f: &mut Frame<'_>, app: &App, popup: Rect, focused: bool) -> ChatLayout {
    let t = &app.theme;
    f.render_widget(Clear, popup);

    let target = app
        .chat
        .target
        .as_deref()
        .map(|t| format!("@{}", t))
        .unwrap_or_else(|| "no target".to_string());
    let block = Block::default()
        .title(format!(
            " Chat ─ {}  (@name msg · /cmd · /agents · esc) ",
            target
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { t.info } else { t.text_dim }));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let width = inner.width as usize;

    // The input grows with its content: wrap "❯ " + text into rows, capped at
    // half the pane (max 8 rows); the transcript yields the space. Long input
    // scrolls so the cursor row stays visible.
    let mut pos = app.chat.cursor.min(app.chat.input.len());
    while pos > 0 && !app.chat.input.is_char_boundary(pos) {
        pos -= 1;
    }
    let (before, after) = app.chat.input.split_at(pos);
    let (cursor_ch, after) = match after.chars().next() {
        Some(ch) => (&after[..ch.len_utf8()], &after[ch.len_utf8()..]),
        None => (" ", ""),
    };
    let prompt_style = Style::default().fg(t.info);
    let cursor_style = Style::default().fg(t.on_accent).bg(t.text_bright);
    // Pasted newlines stay in the string but render as a single '⏎' glyph so
    // the char↔cell math below holds.
    let display = |c: char, style: Style| {
        if c == '\n' {
            ('⏎', style.patch(Style::default().fg(t.text_dim)))
        } else {
            (c, style)
        }
    };
    let mut input_chars: Vec<(char, Style)> = Vec::new();
    input_chars.extend("❯ ".chars().map(|c| (c, prompt_style)));
    input_chars.extend(before.chars().map(|c| display(c, Style::default())));
    input_chars.extend(cursor_ch.chars().map(|c| display(c, cursor_style)));
    input_chars.extend(after.chars().map(|c| display(c, Style::default())));

    let cols = width.max(1);
    let total_rows = input_chars.len().div_ceil(cols).max(1);
    let max_input_rows = ((inner.height as usize) / 2).clamp(1, 8);
    let input_rows = total_rows.min(max_input_rows);
    let cursor_row = (2 + before.chars().count()) / cols;
    let first_row = cursor_row
        .saturating_sub(input_rows - 1)
        .min(total_rows - input_rows);

    let transcript_h = inner.height.saturating_sub(1 + input_rows as u16) as usize;

    // Flatten messages into wrapped, styled lines
    let mut lines: Vec<Line> = Vec::new();
    for m in &app.chat.messages {
        let prefix = format!("{}: ", m.from);
        let indent = " ".repeat(prefix.chars().count().min(width / 2));
        let body_width = width.saturating_sub(indent.chars().count()).max(8);
        for (i, l) in wrap_text(&m.text, body_width).into_iter().enumerate() {
            if i == 0 {
                lines.push(Line::from(vec![
                    Span::styled(prefix.clone(), chat_from_style(t, &m.from)),
                    Span::raw(l),
                ]));
            } else {
                lines.push(Line::from(vec![Span::raw(indent.clone()), Span::raw(l)]));
            }
        }
    }
    if !app.chat.pending.is_empty() {
        let waiting: Vec<&str> = app.chat.pending.iter().map(|p| p.name.as_str()).collect();
        lines.push(Line::from(Span::styled(
            format!("… awaiting {}", waiting.join(", ")),
            Style::default().fg(t.text_dim),
        )));
    }
    if let Some((status, since)) = &app.orchestrator_status {
        // Braille spinner keyed off wall time; handle_tick keeps the pane
        // redrawing while a turn is in flight.
        const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let frame = FRAMES[(since.elapsed().as_millis() / 120) as usize % FRAMES.len()];
        lines.push(Line::from(Span::styled(
            format!(
                "{} {}{}: {}",
                frame,
                app.config.orchestrator.name,
                if app.orchestrator_persona.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", app.orchestrator_persona)
                },
                status
            ),
            Style::default().fg(t.warn),
        )));
    }
    if let Some(p) = &app.pending_proposal {
        lines.push(Line::from(Span::styled(
            format!(
                "⏸ {} proposes {}: {}  (/approve · /deny [reason])",
                app.config.orchestrator.name, p.tool, p.detail
            ),
            Style::default().fg(t.ctx).add_modifier(Modifier::BOLD),
        )));
    }

    // Window: scroll counts lines up from the tail
    let total = lines.len();
    let scroll_max = total.saturating_sub(transcript_h);
    let end = total.saturating_sub(app.chat.scroll.min(scroll_max));
    let start = end.saturating_sub(transcript_h);
    let visible_lines: Vec<String> = lines[start..end]
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect();
    // Mouse selection highlight (content coordinates within the window)
    let sel = app.chat_selection.as_ref().map(|s| s.normalized());
    let items: Vec<ListItem> = lines[start..end]
        .iter()
        .cloned()
        .enumerate()
        .map(|(row, line)| {
            let row = row as u16;
            match sel {
                Some(((min_row, min_col), (max_row, max_col)))
                    if row >= min_row && row <= max_row =>
                {
                    let from = (row == min_row).then_some(min_col as usize);
                    let to = (row == max_row).then_some(max_col as usize);
                    ListItem::new(highlight_line(line, from, to))
                }
                _ => ListItem::new(line),
            }
        })
        .collect();
    let transcript = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: transcript_h as u16,
    };
    f.render_widget(List::new(items), transcript);

    // Separator + growable input with cursor overlay
    let sep = Rect {
        x: inner.x,
        y: inner.y + inner.height.saturating_sub(1 + input_rows as u16),
        width: inner.width,
        height: 1,
    };
    let sep_text = if app.chat.scroll > 0 {
        let label = format!(
            "─ ↓ {} more line(s) below ",
            app.chat.scroll.min(scroll_max)
        );
        let pad = (inner.width as usize).saturating_sub(label.chars().count());
        format!("{}{}", label, "─".repeat(pad))
    } else {
        "─".repeat(inner.width as usize)
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            sep_text,
            Style::default().fg(t.text_dim),
        ))),
        sep,
    );

    let input_area = Rect {
        x: inner.x,
        y: inner.y + inner.height.saturating_sub(input_rows as u16),
        width: inner.width,
        height: input_rows as u16,
    };
    let input_lines: Vec<Line> = (first_row..first_row + input_rows)
        .map(|row| {
            let start = row * cols;
            let end = (start + cols).min(input_chars.len());
            let mut spans: Vec<Span> = Vec::new();
            // Group consecutive same-styled chars into one span per run.
            for &(c, style) in input_chars.get(start..end).unwrap_or(&[]) {
                match spans.last_mut() {
                    Some(last) if last.style == style => last.content.to_mut().push(c),
                    _ => spans.push(Span::styled(c.to_string(), style)),
                }
            }
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(input_lines), input_area);

    // Slash-command completion popup: overlays the transcript just above the
    // separator while the input starts with '/'.
    if focused && !app.chat.palette.matches.is_empty() {
        let count = (app.chat.palette.matches.len().min(8) as u16).min(transcript_h as u16);
        if count > 0 {
            let popup = Rect {
                x: inner.x,
                y: sep.y.saturating_sub(count),
                width: inner.width,
                height: count,
            };
            f.render_widget(Clear, popup);
            let lines: Vec<Line<'_>> = app
                .chat
                .palette
                .matches
                .iter()
                .take(count as usize)
                .enumerate()
                .map(|(index, entry)| {
                    let style = if index == app.chat.palette.selected {
                        Style::default().fg(t.on_accent).bg(t.info)
                    } else {
                        Style::default().fg(t.text_bright).bg(t.text_dim)
                    };
                    Line::from(vec![
                        Span::styled(format!(" {:<30}", entry.template), style),
                        Span::styled(entry.summary.clone(), style.add_modifier(Modifier::DIM)),
                    ])
                })
                .collect();
            f.render_widget(Paragraph::new(lines), popup);
        }
    }

    ChatLayout {
        area: popup,
        transcript_area: transcript,
        scroll_max,
        visible_lines,
    }
}

// ── Command bar ────────────────────────────────────────────────────────────

/// How many rows the command palette popup may occupy: at most 8 entries, and
/// never more than the rows left above the one-row command bar.
///
/// The clamp against `area_height` is the load-bearing part. Without it the
/// caller's `area.y + area.height - 1 - match_count` wrapped the u16 on any
/// terminal shorter than the match list (8 matches needed 9 rows), producing a
/// y of ~65530 and a panic inside ratatui's buffer indexing.
fn palette_popup_rows(match_len: usize, area_height: u16) -> u16 {
    let rows_above_bar = area_height.saturating_sub(1);
    (match_len.min(8) as u16).min(rows_above_bar)
}

fn draw_command_bar(f: &mut Frame<'_>, app: &App, area: Rect) -> Rect {
    let t = &app.theme;
    if area.height == 0 || area.width == 0 {
        return Rect::default();
    }
    let match_count = palette_popup_rows(app.palette.matches.len(), area.height);
    if match_count > 0 {
        let popup = Rect {
            x: area.x,
            y: area.y + area.height - 1 - match_count,
            width: area.width,
            height: match_count,
        };
        f.render_widget(Clear, popup);
        let lines: Vec<Line<'_>> = app
            .palette
            .matches
            .iter()
            .take(match_count as usize)
            .enumerate()
            .map(|(index, entry)| {
                let style = if index == app.palette.selected {
                    Style::default().fg(t.on_accent).bg(t.info)
                } else {
                    Style::default().fg(t.text_bright).bg(t.text_dim)
                };
                Line::from(vec![
                    Span::styled(format!(" {:<38}", entry.template), style),
                    Span::styled(entry.summary.clone(), style.add_modifier(Modifier::DIM)),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(lines), popup);
    }
    let bar = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    f.render_widget(Clear, bar);
    // Clamp to a char boundary — split_at panics mid-UTF-8. Same defensive
    // walk-back draw_input_field uses, so both editors behave identically.
    let mut pos = app.command_cursor.min(app.command_input.len());
    while pos > 0 && !app.command_input.is_char_boundary(pos) {
        pos -= 1;
    }
    let (before, after) = app.command_input.split_at(pos);
    // Render the cursor over the character under it (matching draw_input_field)
    // instead of inserting a block that visually shifts the tail of the line.
    let (cursor_ch, after) = match after.chars().next() {
        Some(ch) => (&after[..ch.len_utf8()], &after[ch.len_utf8()..]),
        None => (" ", ""),
    };
    let line = Line::from(vec![
        Span::raw("> "),
        Span::raw(before.to_string()),
        Span::styled(
            cursor_ch.to_string(),
            Style::default().fg(t.on_accent).bg(t.text_bright),
        ),
        Span::raw(after.to_string()),
    ]);
    let p = Paragraph::new(line).style(Style::default().fg(t.text_bright).bg(t.text_dim));
    f.render_widget(p, bar);
    bar
}

fn draw_command_result(f: &mut Frame<'_>, app: &App, area: Rect) {
    let t = &app.theme;
    let bar = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    f.render_widget(Clear, bar);
    let line = Line::from(vec![
        Span::styled("» ", Style::default().fg(t.warn).bg(t.text_dim)),
        Span::raw(app.command_result.clone()),
        Span::styled(
            "  [any key to close]",
            Style::default().fg(t.text).bg(t.text_dim),
        ),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().fg(t.text_bright).bg(t.text_dim)),
        bar,
    );
}

fn draw_menu_bar(f: &mut Frame<'_>, app: &App, area: Rect) -> (Vec<Rect>, Rect, Vec<Rect>) {
    let t = &app.theme;
    let (selected_top, selected_sub) = match app.mode {
        AppMode::Menu {
            selected_top,
            selected_sub,
        } => (selected_top, selected_sub),
        _ => return (Vec::new(), Rect::default(), Vec::new()),
    };

    // Rebuilt every frame so labels reflect live state — "Stop" vs "Start",
    // the current model, whether a restart is pending.
    let sections = app.menu();
    if sections.is_empty() {
        return (Vec::new(), Rect::default(), Vec::new());
    }
    let selected_top = selected_top.min(sections.len() - 1);

    let mut spans = Vec::new();
    let mut item_areas = Vec::new();
    let mut x = area.x;
    for (idx, section) in sections.iter().enumerate() {
        let text = format!(" {} ", section.title);
        let width = text.chars().count() as u16;
        item_areas.push(Rect {
            x,
            y: area.y,
            width,
            height: 1,
        });
        x = x.saturating_add(width);
        let style = if idx == selected_top {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        spans.push(Span::styled(text, style));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(t.text_dim)),
        area,
    );

    let Some(sub_idx) = selected_sub else {
        return (item_areas, Rect::default(), Vec::new());
    };

    let entries = &sections[selected_top].items;
    if entries.is_empty() {
        return (item_areas, Rect::default(), Vec::new());
    }
    let sub_idx = sub_idx.min(entries.len() - 1);

    // Width fits the widest label+detail pair, then is clamped so a long
    // model id cannot push the popup off the right edge of the terminal.
    let content_width = entries.iter().map(|e| e.width()).max().unwrap_or(0) as u16;
    let width = (content_width + 4).min(area.width.max(8));
    let x = item_areas
        .get(selected_top)
        .map(|r| r.x)
        .unwrap_or(area.x)
        .min(area.x + area.width.saturating_sub(width));
    let popup = Rect {
        x,
        y: area.y + 1,
        width,
        height: entries.len() as u16 + 2,
    };
    f.render_widget(Clear, popup);
    let block = Block::default().borders(Borders::ALL);
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let rows: Vec<ListItem> = entries
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            if item.action == MenuAction::Separator {
                return ListItem::new(Line::from(Span::styled(
                    "─".repeat(inner.width as usize),
                    Style::default().fg(t.text_dim),
                )));
            }
            let selected = idx == sub_idx;
            let base = if !item.enabled {
                Style::default().fg(t.text_dim)
            } else if selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            // Pad between label and detail so values right-align into a
            // readable column rather than trailing each label.
            let label_w = item.label.chars().count();
            let detail_w = item.detail.chars().count();
            let avail = inner.width as usize;
            let mut spans = vec![Span::styled(format!(" {}", item.label), base)];
            if detail_w > 0 {
                let used = label_w + detail_w + 2;
                let pad = avail.saturating_sub(used).max(1);
                spans.push(Span::styled(" ".repeat(pad), base));
                let detail_style = if selected || !item.enabled {
                    base
                } else {
                    base.fg(t.info)
                };
                spans.push(Span::styled(format!("{} ", item.detail), detail_style));
            } else if selected {
                // Extend the highlight across the row so selection reads as a
                // bar rather than stopping at the end of the text.
                let pad = avail.saturating_sub(label_w + 1);
                spans.push(Span::styled(" ".repeat(pad), base));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    f.render_widget(List::new(rows), inner);

    let item_rows = (0..entries.len())
        .map(|i| Rect {
            x: inner.x,
            y: inner.y + i as u16,
            width: inner.width,
            height: 1,
        })
        .collect();

    (item_areas, popup, item_rows)
}

// ── Search overlay ─────────────────────────────────────────────────────────

fn draw_search_overlay(f: &mut Frame<'_>, app: &App, area: Rect) -> Rect {
    let t = &app.theme;
    let (query, cursor, matches, selected) = match &app.mode {
        AppMode::Search {
            query,
            cursor,
            matches,
            selected,
        } => (query.clone(), *cursor, matches.clone(), *selected),
        _ => return Rect::default(),
    };

    let visible_matches = 14usize;
    let height = (visible_matches + 4).min(area.height as usize) as u16;
    let popup = centered_rect(70, height, area);
    f.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Search (↑↓ navigate  Enter jump  Esc cancel) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t.info));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    // Query input line
    if inner.height == 0 {
        return popup;
    }
    let input_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 1,
    };

    // Render query with cursor
    let mut pos = cursor.min(query.len());
    while pos > 0 && !query.is_char_boundary(pos) {
        pos -= 1;
    }
    let before = &query[..pos];
    let (cursor_ch, after) = if pos < query.len() {
        let ch = query[pos..].chars().next().unwrap();
        let end = pos + ch.len_utf8();
        (&query[pos..end], &query[end..])
    } else {
        (" ", "")
    };
    let cursor_style = Style::default().bg(t.info).fg(t.on_accent);
    let input_line = Line::from(vec![
        Span::styled("> ", Style::default().fg(t.warn)),
        Span::raw(before.to_string()),
        Span::styled(cursor_ch.to_string(), cursor_style),
        Span::raw(after.to_string()),
    ]);
    f.render_widget(Paragraph::new(input_line), input_area);

    // Separator
    if inner.height < 2 {
        return popup;
    }
    let sep_area = Rect {
        x: inner.x,
        y: inner.y + 1,
        width: inner.width,
        height: 1,
    };
    f.render_widget(
        Paragraph::new("─".repeat(inner.width as usize)).style(Style::default().fg(t.text_dim)),
        sep_area,
    );

    // Match list
    let list_area = Rect {
        x: inner.x,
        y: inner.y + 2,
        width: inner.width,
        height: inner.height.saturating_sub(3),
    };
    let session_lines: Vec<String> = if let Some(session) = app.active_session() {
        session.output_lines.iter().cloned().collect()
    } else {
        vec![]
    };

    let items: Vec<ListItem> = matches
        .iter()
        .enumerate()
        .take(visible_matches)
        .map(|(i, &line_idx)| {
            let text = session_lines.get(line_idx).cloned().unwrap_or_default();
            let style = if i == selected {
                Style::default().fg(t.on_accent).bg(t.info)
            } else {
                Style::default().fg(t.text)
            };
            ListItem::new(format!(" {:4}: {}", line_idx + 1, text)).style(style)
        })
        .collect();
    f.render_widget(List::new(items), list_area);

    // Footer: match count
    if inner.height > 0 {
        let footer_area = Rect {
            x: inner.x,
            y: popup.y + popup.height - 2,
            width: inner.width,
            height: 1,
        };
        let count_text = if query.is_empty() {
            "type to search".to_string()
        } else {
            format!(
                "{} match{}",
                matches.len(),
                if matches.len() == 1 { "" } else { "es" }
            )
        };
        f.render_widget(
            Paragraph::new(count_text)
                .style(Style::default().fg(t.text_dim).add_modifier(Modifier::DIM))
                .alignment(Alignment::Center),
            footer_area,
        );
    }

    popup
}

// ── Settings overlay ────────────────────────────────────────────────────────

fn draw_settings_overlay(f: &mut Frame<'_>, app: &App, area: Rect) -> Rect {
    let t = &app.theme;
    let ss = &app.settings_state;
    let n_fields = ss.fields.len() as u16;
    let height = (n_fields + 6).min(area.height);
    let popup = centered_rect(70, height, area);
    f.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Settings (↑↓ navigate  Enter edit  s save  Esc cancel) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t.warn));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let list_height = inner.height.saturating_sub(1);
    let list_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: list_height,
    };

    let items: Vec<ListItem> = ss
        .fields
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let is_selected = i == ss.selected;
            let value_str = if is_selected && ss.editing {
                // Show edit buffer with cursor
                let cursor = ss.edit_cursor.min(ss.edit_buf.len());
                let before = &ss.edit_buf[..cursor];
                let (cursor_ch, after) = if cursor < ss.edit_buf.len() {
                    let ch = ss.edit_buf[cursor..].chars().next().unwrap();
                    let end = cursor + ch.len_utf8();
                    (&ss.edit_buf[cursor..end], &ss.edit_buf[end..])
                } else {
                    (" ", "")
                };
                format!("{}{}{}|", before, cursor_ch, after)
            } else {
                field.value.clone()
            };

            let row_style = if is_selected {
                Style::default().fg(t.on_accent).bg(t.warn)
            } else {
                Style::default().fg(t.text_bright)
            };
            let text = format!(" {:<28} │  {}", field.label, value_str);
            ListItem::new(text).style(row_style)
        })
        .collect();

    f.render_widget(List::new(items), list_area);

    // Footer
    if inner.height > 0 {
        let footer_area = Rect {
            x: inner.x,
            y: popup.y + popup.height - 2,
            width: inner.width,
            height: 1,
        };
        f.render_widget(
            Paragraph::new("Changes are saved to ~/.config/linkshell/config.toml")
                .style(Style::default().fg(t.text_dim).add_modifier(Modifier::DIM))
                .alignment(Alignment::Center),
            footer_area,
        );
    }

    popup
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
    t: &Theme,
    screen: &Screen,
    vt_row: u16,
    screen_cols: u16,
    disp_row: u16,
    sel: Option<&Selection>,
    cursor_col: Option<u16>,
) -> ListItem<'static> {
    ListItem::new(build_row_line(
        t,
        screen,
        vt_row,
        screen_cols,
        disp_row,
        sel,
        cursor_col,
    ))
}

/// Assemble the styled spans for one row. Split out from `build_row` so tests
/// can inspect the rendered text directly.
fn build_row_line(
    t: &Theme,
    screen: &Screen,
    vt_row: u16,
    screen_cols: u16,
    disp_row: u16,
    sel: Option<&Selection>,
    cursor_col: Option<u16>,
) -> Line<'static> {
    let sel_style = Style::default().bg(t.sel_bg).fg(t.text_bright);
    let cursor_style = Style::default().add_modifier(Modifier::REVERSED);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut cur_style: Option<Style> = None;

    for col in 0..screen_cols {
        let is_sel = sel.is_some_and(|s| s.contains(disp_row, col));
        let is_cursor = cursor_col == Some(col);
        let (content, style) = match screen.cell(vt_row, col) {
            Some(cell) => {
                // The second cell of a double-width glyph (emoji, CJK) is a
                // continuation marker with empty contents. Rendering it as a
                // space would shift the rest of the line right by one column
                // per wide char — skip it; the wide glyph already covers it.
                if cell.is_wide_continuation() {
                    continue;
                }
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

    Line::from(spans)
}

/// Fallback frame for terminals below the minimum layout size. Deliberately
/// uses no arithmetic on the area beyond ratatui's own clipping.
fn draw_too_small(f: &mut Frame<'_>, t: &Theme, size: Rect) {
    f.render_widget(Clear, size);
    let text = vec![
        Line::from(Span::styled(
            "terminal too small",
            Style::default().fg(t.warn).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!(
                "{}x{} — need {}x{}",
                size.width, size.height, MIN_COLS, MIN_ROWS
            ),
            Style::default().fg(t.text_dim),
        )),
    ];
    f.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        size,
    );
}

fn centered_rect(percent_x: u16, height: u16, r: Rect) -> Rect {
    // Widen to u32 for the percentage: `r.width * percent_x` overflows u16 past
    // 655 columns, which is reachable on a wide display at a small font size.
    let w = ((r.width as u32 * percent_x as u32) / 100).min(r.width as u32) as u16;
    let x = r.x + (r.width - w) / 2;
    let y = r.y + (r.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width: w,
        height: height.min(r.height),
    }
}

// ── Planning pane ─────────────────────────────────────────────────────────

/// Contract `$HOME` to `~` for display.
fn contract_home(path: &std::path::Path) -> String {
    let s = path.to_string_lossy().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && s.starts_with(&home) => {
            format!("~{}", &s[home.len()..])
        }
        _ => s,
    }
}

/// Coarse relative age: the sidebar only needs enough to order things by feel.
fn relative_age(then: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now.saturating_sub(then);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        86400..=172_799 => "yesterday".to_string(),
        _ => format!("{}d", secs / 86400),
    }
}

/// Truncate to `width` columns, ellipsizing when it doesn't fit.
fn ellipsize(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if s.chars().count() <= width {
        return s.to_string();
    }
    let keep = width.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn spinner_frame() -> char {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    SPINNER[(ms / 120) as usize % SPINNER.len()]
}

/// Render the planning pane into a split leaf.
///
/// The pane is a document, not a log: a thread list on the left, and on the
/// right a header that says what the thread is grounded in, the transcript,
/// an input that grows with the paragraph you're writing, and a status row.
fn draw_planning_in(f: &mut Frame<'_>, app: &App, area: Rect, focused: bool) {
    let t = &app.theme;
    use crate::app::PlanningFocus;

    f.render_widget(Clear, area);

    let block = Block::default()
        .title(" Planning ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { t.info } else { t.text_dim }));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Sidebar collapses to zero width, giving the transcript the whole pane.
    let (sidebar_area, main_area) = if app.planning.sidebar_collapsed {
        (Rect { width: 0, ..inner }, inner)
    } else {
        let pct = app.config.planning.sidebar_width_pct();
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(pct), Constraint::Min(20)])
            .split(inner);
        (cols[0], cols[1])
    };

    if sidebar_area.width > 2 {
        draw_planning_sidebar(f, app, sidebar_area);
    }

    let Some(thread) = app.planning.thread.as_ref() else {
        let hint = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  no thread — n to start one",
                Style::default().fg(t.text_dim),
            )),
        ]);
        f.render_widget(hint, main_area);
        // The overlays still have to draw: deleting from the list and
        // switching backend are both things you do with no thread open.
        draw_planning_overlays(f, app, area);
        return;
    };

    // The input grows with its content; the overflow strip and status row take
    // fixed rows off the top and bottom of what's left for the transcript.
    let input_rows = {
        let cols = main_area.width.max(1) as usize;
        let chars = app.planning.input.chars().count() + 3;
        (chars.div_ceil(cols)).clamp(1, 5) as u16
    };
    let overflow_rows = if app.planning.overflow { 2 } else { 0 };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),             // header
            Constraint::Min(3),                // transcript
            Constraint::Length(overflow_rows), // overflow prompt
            Constraint::Length(input_rows),    // input
            Constraint::Length(1),             // status
        ])
        .split(main_area);

    draw_planning_header(f, app, thread, rows[0]);
    draw_planning_transcript(f, app, thread, rows[1]);
    if overflow_rows > 0 {
        draw_planning_overflow(f, app, rows[2]);
    }
    draw_planning_input(
        f,
        app,
        rows[3],
        focused && app.planning.focus == PlanningFocus::Transcript,
    );
    draw_planning_status(f, app, thread, rows[4]);

    draw_planning_overlays(f, app, area);
}

/// Picker and delete confirmation, drawn over the pane in either state.
fn draw_planning_overlays(f: &mut Frame<'_>, app: &App, area: Rect) {
    if app.planning.picker.is_some() {
        draw_planning_picker(f, app, area);
    }
    if app.planning.confirm_delete.is_some() {
        draw_planning_delete_confirm(f, app, area);
    }
    if app.planning.handoff.is_some() {
        draw_planning_handoff(f, app, area);
    }
}

/// Pick the session a committed plan is handed to as work.
fn draw_planning_handoff(f: &mut Frame<'_>, app: &App, area: Rect) {
    let t = &app.theme;
    let targets = app.planning_handoff_targets();
    let sel = app.planning.handoff.unwrap_or(0);
    let height = (targets.len() as u16 + 2).min(area.height).max(3);
    let popup = centered_rect(70, height, area);
    f.render_widget(Clear, popup);

    let items: Vec<ListItem> = targets
        .iter()
        .enumerate()
        .map(|(i, (id, name))| {
            let style = if i == sel {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {}", name), style),
                Span::styled(format!("  #{}", id), Style::default().fg(t.text_dim)),
            ]))
        })
        .collect();

    f.render_widget(
        List::new(items).block(
            Block::default()
                .title(" Hand plan to ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(t.ok)),
        ),
        popup,
    );
}

/// Thread list: two lines per row so a title has room to breathe.
fn draw_planning_sidebar(f: &mut Frame<'_>, app: &App, area: Rect) {
    let t = &app.theme;
    use crate::app::PlanningFocus;

    let open_id = app.planning.thread.as_ref().map(|t| t.id.as_str());
    let focused_list = app.planning.focus == PlanningFocus::Sidebar;
    let width = area.width.saturating_sub(2) as usize;

    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        "Threads",
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    lines.push(Line::from(""));

    // Reserve the bottom rows for pinned decisions and the count footer.
    let decisions = app
        .planning
        .thread
        .as_ref()
        .map(|t| t.decisions.clone())
        .unwrap_or_default();
    let decisions_rows = if decisions.is_empty() {
        0
    } else {
        (decisions.len() + 2).min(area.height as usize / 3)
    };
    let list_budget = (area.height as usize)
        .saturating_sub(3 + decisions_rows)
        .max(2);

    for (i, thread) in app.planning.threads.iter().enumerate() {
        if lines.len() + 2 > list_budget + 2 {
            break;
        }
        let is_open = open_id == Some(thread.id.as_str());
        let is_sel = i == app.planning.list_selected;
        let marker = if is_open { "▸ " } else { "  " };
        // The open thread stays marked even when focus is elsewhere; the
        // selection highlight only means something while the list has focus.
        let title_style = if is_open || (is_sel && focused_list) {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(
                ellipsize(&thread.title, width.saturating_sub(2)),
                title_style,
            ),
        ]));
        lines.push(Line::from(Span::styled(
            format!(
                "   {} · {} msg",
                relative_age(thread.updated),
                thread.messages
            ),
            Style::default().fg(t.text_dim),
        )));
    }

    // Pinned decisions: what you scroll back for is usually "what did we
    // decide about X", and a list of those is cheaper to scan than the
    // transcript.
    if !decisions.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Decisions",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for d in decisions.iter().take(decisions_rows.saturating_sub(2)) {
            lines.push(Line::from(Span::styled(
                format!("· {}", ellipsize(d, width.saturating_sub(2))),
                Style::default().fg(t.ok),
            )));
        }
    }

    f.render_widget(Paragraph::new(lines), area);

    // Count footer pinned to the last row.
    if area.height >= 2 {
        let footer = Rect {
            x: area.x,
            y: area.y + area.height - 1,
            width: area.width,
            height: 1,
        };
        let n = app.planning.threads.len();
        f.render_widget(
            Paragraph::new(Span::styled(
                format!(" {} thread{}", n, if n == 1 { "" } else { "s" }),
                Style::default().fg(t.text_dim),
            )),
            footer,
        );
    }
}

/// Title, scope root, and grounding state.
///
/// Staleness lives in the persistent chrome rather than a status flash: it is
/// what tells you whether the brief still describes the repo.
fn draw_planning_header(
    f: &mut Frame<'_>,
    app: &App,
    thread: &crate::planning::store::Thread,
    area: Rect,
) {
    let t = &app.theme;
    let stale = thread.stale_reads();
    let reads = thread.reads.len();
    let mut scope: Vec<Span> = vec![Span::styled(
        format!(" {} · {} files read", contract_home(&thread.root), reads),
        Style::default().fg(t.text_dim),
    )];
    if !stale.is_empty() {
        scope.push(Span::styled(
            format!(" · {} changed since", stale.len()),
            Style::default().fg(t.warn),
        ));
    }
    let lines = vec![
        Line::from(Span::styled(
            format!(" {}", thread.title),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(scope),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

/// The transcript, oldest first, with a rule wherever the model changed.
fn draw_planning_transcript(
    f: &mut Frame<'_>,
    app: &App,
    thread: &crate::planning::store::Thread,
    area: Rect,
) {
    let t = &app.theme;
    use crate::planning::store::Role;

    let width = area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();
    let mut prev_model: Option<String> = None;

    for m in &thread.messages {
        // Show the model seam: per-message provider/model is recorded so you
        // can see where a thread switched from a local model to a frontier
        // one, which is how you judge whether earlier turns deserve a re-run.
        if m.role == Role::Assistant && !m.model.is_empty() {
            let changed = prev_model.as_deref() != Some(m.model.as_str());
            if changed && prev_model.is_some() {
                let label = format!(" {} ", m.attribution());
                let rule_w = width.saturating_sub(label.chars().count() + 2);
                lines.push(Line::from(Span::styled(
                    format!("{}{}──", "─".repeat(rule_w), label),
                    Style::default().fg(t.text_dim),
                )));
            }
            prev_model = Some(m.model.clone());
        }

        let attribution = m.attribution();
        // Hand-edited messages lose their metadata by design — markdown wins
        // on text and metadata degrades rather than lying — so an empty
        // attribution renders as a generic label, not a blank gutter.
        let (label, style) = match m.role {
            Role::User => ("you".to_string(), Style::default().fg(t.text_dim)),
            Role::Assistant if attribution.trim().is_empty() => {
                ("assistant".to_string(), Style::default().fg(t.text_dim))
            }
            Role::Assistant => (attribution, Style::default().fg(t.info)),
        };
        lines.push(Line::from(Span::styled(format!(" {}", label), style)));
        for l in wrap_text(&m.text, width.saturating_sub(2).max(8)) {
            lines.push(Line::from(format!("   {}", l)));
        }
        lines.push(Line::from(""));
    }

    if app.planning.busy && !app.planning.status.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" {} {}", spinner_frame(), app.planning.status),
            Style::default().fg(t.warn),
        )));
    }

    // Scroll counts lines up from the tail; 0 pins to the bottom.
    let h = area.height as usize;
    let total = lines.len();
    let scroll_max = total.saturating_sub(h);
    let end = total.saturating_sub(app.planning.scroll.min(scroll_max));
    let start = end.saturating_sub(h);
    let window: Vec<Line> = lines[start..end].to_vec();
    f.render_widget(Paragraph::new(window), area);
}

/// Multi-line input. Enter sends; Alt+Enter inserts a newline, because a
/// planning message is usually a paragraph.
fn draw_planning_input(f: &mut Frame<'_>, app: &App, area: Rect, focused: bool) {
    let t = &app.theme;
    let dim = app.planning.busy;
    let mut pos = app.planning.cursor.min(app.planning.input.len());
    while pos > 0 && !app.planning.input.is_char_boundary(pos) {
        pos -= 1;
    }
    let (before, after) = app.planning.input.split_at(pos);
    let (cursor_ch, rest) = match after.chars().next() {
        Some(ch) => (&after[..ch.len_utf8()], &after[ch.len_utf8()..]),
        None => (" ", ""),
    };
    let base = if dim {
        Style::default().fg(t.text_dim)
    } else {
        Style::default()
    };
    let mut spans = vec![Span::styled(" > ", Style::default().fg(t.info))];
    spans.push(Span::styled(before.replace('\n', "⏎"), base));
    if focused && !dim {
        spans.push(Span::styled(
            cursor_ch.replace('\n', "⏎"),
            Style::default().fg(t.on_accent).bg(t.text_bright),
        ));
    } else {
        spans.push(Span::styled(cursor_ch.replace('\n', "⏎"), base));
    }
    spans.push(Span::styled(rest.replace('\n', "⏎"), base));
    f.render_widget(
        Paragraph::new(Line::from(spans)).wrap(Wrap { trim: false }),
        area,
    );
}

/// Backend, context usage, and whichever of error/status/last-plan matters.
/// Token counts for the context meter. Sub-1k values keep their digits: the
/// old `used / 1000` rendered every short thread as "0k", which reads as a
/// meter that is not measuring anything rather than one reporting a small
/// number.
fn fmt_tokens(n: usize) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{}k", n / 1000)
    }
}

fn draw_planning_status(
    f: &mut Frame<'_>,
    app: &App,
    thread: &crate::planning::store::Thread,
    area: Rect,
) {
    let t = &app.theme;
    let mut spans: Vec<Span> = Vec::new();

    let label = app.planning.backend_label();
    let backend_style = if app.planning.backend.is_none() {
        Style::default().fg(t.err)
    } else {
        Style::default().fg(t.text_dim)
    };
    spans.push(Span::styled(format!(" {}", label), backend_style));

    // Context usage earns permanent space: it is the number whose meaning
    // changes when you switch models.
    if let Some(b) = app.planning.backend.as_ref() {
        if b.max_context_tokens > 0 {
            let used = crate::planning::estimate_tokens(&thread.messages, &app.planning.input);
            // Two different numbers, and conflating them is what made this
            // meter look broken: `used` is what the *next* turn starts from
            // (the transcript, which never carries tool traffic), while the
            // peak is what the last turn actually sent — file contents and
            // all. A turn that reads a codebase moves only the second one.
            let peak = app.planning.last_peak_tokens;
            let worst = used.max(peak);
            let pct = worst * 100 / b.max_context_tokens.max(1);
            let style = if pct >= 90 {
                Style::default().fg(t.err)
            } else if pct >= 75 {
                Style::default().fg(t.warn)
            } else {
                Style::default().fg(t.text_dim)
            };
            spans.push(Span::styled(
                format!(
                    "    ~{}/{}",
                    fmt_tokens(used),
                    fmt_tokens(b.max_context_tokens)
                ),
                style,
            ));
            if peak > used {
                spans.push(Span::styled(format!(" (peak {})", fmt_tokens(peak)), style));
            }
        }
    }

    if !app.planning.error.is_empty() {
        spans.push(Span::styled(
            format!("    {}", app.planning.error),
            Style::default().fg(t.err),
        ));
    } else if app.planning.busy {
        spans.push(Span::styled(
            format!("    {} {}", spinner_frame(), app.planning.status),
            Style::default().fg(t.warn),
        ));
    } else if !app.planning.status.is_empty() {
        // Commit and handoff both land here: they finish by clearing `busy`
        // and leaving a confirmation behind, which would otherwise never be
        // shown because the spinner branch above is the only other reader.
        spans.push(Span::styled(
            format!("    {}", app.planning.status),
            Style::default().fg(t.ok),
        ));
    } else if let Some(p) = crate::planning::store::latest_plan(&thread.id) {
        spans.push(Span::styled(
            format!("    {}", contract_home(&p)),
            Style::default().fg(t.text_dim),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The one place the design refuses to be silent. Automatic compaction would
/// eat the early turns, and in a planning thread those are usually the design
/// premises everything downstream rests on.
fn draw_planning_overflow(f: &mut Frame<'_>, app: &App, area: Rect) {
    let t = &app.theme;
    let label = app.planning.backend_label();
    let limit = app
        .planning
        .backend
        .as_ref()
        .map(|b| b.max_context_tokens)
        .unwrap_or(0);
    let used = app
        .planning
        .thread
        .as_ref()
        .map(|t| crate::planning::estimate_tokens(&t.messages, &app.planning.input))
        .unwrap_or(0);
    let lines = vec![
        Line::from(Span::styled(
            format!(
                " thread is ~{}k tokens, over {}'s {}k limit",
                used / 1000,
                label,
                limit / 1000
            ),
            Style::default().fg(t.warn),
        )),
        Line::from(Span::styled(
            " [c] compact   [b] switch backend   [Esc] dismiss",
            Style::default().fg(t.text_dim),
        )),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

/// Backend picker, reusing the new-session kind-selector idiom rather than
/// inventing a second dropdown.
/// The orchestrator's model list, as reported by its endpoint.
///
/// A full overlay rather than a menu row: a local server serves dozens of
/// models, and stepping through those one Enter at a time is not choosing.
fn draw_orchestrator_model_picker(f: &mut Frame<'_>, app: &App, area: Rect, sel: usize) -> Rect {
    let t = &app.theme;
    let models = &app.orchestrator_models;
    let height = (models.len() as u16 + 2)
        .min(area.height.saturating_sub(4))
        .max(3);
    let popup = centered_rect(60, height, area);
    f.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Orchestrator model · ↵ select · r reprobe · esc ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t.info));

    if models.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                " endpoint reported no models",
                Style::default().fg(t.err),
            ))
            .block(block),
            popup,
        );
        return popup;
    }

    // Keep the highlighted row on screen when the server serves more models
    // than the popup has rows.
    let rows = popup.height.saturating_sub(2) as usize;
    let first = sel.saturating_sub(rows.saturating_sub(1));
    let current = &app.config.orchestrator.model;
    let items: Vec<ListItem> = models
        .iter()
        .enumerate()
        .skip(first)
        .take(rows)
        .map(|(i, m)| {
            let style = if i == sel {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let marker = if m == current { "●" } else { " " };
            ListItem::new(Line::from(vec![Span::styled(
                format!(" {} {}", marker, m),
                style,
            )]))
        })
        .collect();

    f.render_widget(List::new(items).block(block), popup);
    popup
}

fn draw_planning_picker(f: &mut Frame<'_>, app: &App, area: Rect) {
    let t = &app.theme;
    let names = app.config.planning.backend_names();
    let sel = app.planning.picker.unwrap_or(0);
    if let Some(msel) = app.planning.picker_model {
        draw_planning_model_picker(
            f,
            app,
            area,
            names.get(sel).cloned().unwrap_or_default(),
            msel,
        );
        return;
    }
    let height = (names.len() as u16 + 2).min(area.height).max(3);
    let popup = centered_rect(70, height, area);
    f.render_widget(Clear, popup);

    if names.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                " no backends — configure [agents.*] or [planning.backends.*]",
                Style::default().fg(t.err),
            ))
            .block(
                Block::default()
                    .title(" Backend ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(t.info)),
            ),
            popup,
        );
        return;
    }

    let items: Vec<ListItem> = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let b = app.config.planning.backend(n);
            let detail = b
                .as_ref()
                .map(|b| {
                    // Three states worth telling apart: probed with a list,
                    // probed and told nothing, and never asked.
                    let probe = match app.planning.model_cache.get(n) {
                        Some(m) if !m.is_empty() => format!(" · {} models ›", m.len()),
                        Some(_) => " · endpoint unreachable".to_string(),
                        None if b.is_probeable() => " · probing…".to_string(),
                        None => String::new(),
                    };
                    format!(
                        "{} · {} · {}k{}",
                        b.provider,
                        if b.model.is_empty() { "—" } else { &b.model },
                        b.max_context_tokens / 1000,
                        probe
                    )
                })
                .unwrap_or_default();
            let style = if i == sel {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {:<12}", n), style),
                Span::styled(format!("  {}", detail), Style::default().fg(t.text_dim)),
            ]))
        })
        .collect();

    f.render_widget(
        List::new(items).block(
            Block::default()
                .title(" Backend · ↵/→ models · r reprobe ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(t.info)),
        ),
        popup,
    );
}

/// The models a backend's endpoint says it is serving right now. Second level
/// of the picker: an endpoint is not a choice of model, and on a local server
/// the loaded set changes without the config knowing.
fn draw_planning_model_picker(
    f: &mut Frame<'_>,
    app: &App,
    area: Rect,
    backend: String,
    sel: usize,
) {
    let t = &app.theme;
    let models = app
        .planning
        .model_cache
        .get(&backend)
        .cloned()
        .unwrap_or_default();
    let current = app.config.planning.backend(&backend).map(|b| b.model);

    // Keep the highlighted row on screen for a server serving more models
    // than the popup has rows.
    let height = (models.len() as u16 + 2).min(area.height).max(3);
    let popup = centered_rect(70, height, area);
    let rows = popup.height.saturating_sub(2) as usize;
    let first = sel.saturating_sub(rows.saturating_sub(1));
    f.render_widget(Clear, popup);

    let items: Vec<ListItem> = models
        .iter()
        .enumerate()
        .skip(first)
        .take(rows)
        .map(|(i, m)| {
            let style = if i == sel {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let marker = if current.as_deref() == Some(m.as_str()) {
                "●"
            } else {
                " "
            };
            ListItem::new(Line::from(vec![Span::styled(
                format!(" {} {}", marker, m),
                style,
            )]))
        })
        .collect();

    f.render_widget(
        List::new(items).block(
            Block::default()
                .title(format!(" Model · {} · ↵ select · ← back ", backend))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(t.info)),
        ),
        popup,
    );
}

fn draw_planning_delete_confirm(f: &mut Frame<'_>, app: &App, area: Rect) {
    let t = &app.theme;
    let target = app.planning.confirm_delete.as_ref().and_then(|id| {
        app.planning
            .threads
            .iter()
            .find(|t| &t.id == id)
            .map(|t| (t.title.clone(), t.root.clone()))
    });
    let (title, root) = target.unwrap_or_default();
    let popup = centered_rect(60, 5, area);
    f.render_widget(Clear, popup);
    let lines = vec![
        Line::from(Span::raw(format!(" delete \"{}\"?", ellipsize(&title, 40)))),
        // Threads are global, so two repos can hold same-named threads. The
        // root is what tells them apart before an irreversible delete.
        Line::from(Span::styled(
            format!(" {}", ellipsize(&contract_home(&root), 50)),
            Style::default().fg(t.text_dim),
        )),
        Line::from(Span::styled(
            " [y] delete   [n/Esc] cancel",
            Style::default().fg(t.text_dim),
        )),
    ];
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" Confirm ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(t.err)),
        ),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Render `draw()` into an offscreen buffer and flatten it to rows of
    /// `(text, fg)` — a snapshot fine-grained enough to catch a colour
    /// regression, coarse enough not to churn on unrelated edits.
    fn render_rows(app: &App, width: u16, height: u16) -> Vec<(String, Vec<Color>)> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| {
                draw(f, app);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                let mut text = String::new();
                let mut fgs = Vec::new();
                for x in 0..width {
                    let cell = buffer.get(x, y);
                    text.push_str(cell.symbol());
                    fgs.push(cell.fg);
                }
                (text.trim_end().to_string(), fgs)
            })
            .collect()
    }

    fn snapshot_app() -> App {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        App::new(tx, std::sync::Arc::new(crate::config::Config::default()))
    }

    /// Pins the rendered frame so a restyle has to be a deliberate edit to
    /// this expectation rather than a silent change nobody reviewed. Sessions
    /// can't be spawned in a unit test (each needs a real PTY), so this covers
    /// the empty-workspace frame: the output pane, the session bar and the
    /// status panel chrome, which is where the theme refactor touched most.
    #[test]
    fn the_empty_frame_renders_from_the_theme_unchanged() {
        let app = snapshot_app();
        let rows = render_rows(&app, 60, 14);
        let text: Vec<&str> = rows.iter().map(|(t, _)| t.as_str()).collect();

        assert_eq!(
            text[0],
            "┌ linkshell ───────────────────────────────────────────────┐"
        );
        assert_eq!(
            text[1],
            "│ No sessions. Press alt-n to create one.                  │"
        );
        assert!(text[6].starts_with("└"), "{:?}", text[6]);
        // Session bar: bordered slot boxes, three rows of chrome.
        assert_eq!(
            text[9],
            "└──────────────────────────────────────────────────────────┘"
        );
        assert!(text[10].contains("Status"), "{:?}", text[10]);
        assert!(
            text[11].contains("Kind"),
            "status header missing: {:?}",
            text[11]
        );
        assert!(text[12].contains("sock:"), "{:?}", text[12]);

        // The empty-pane border is `text_dim`, not some other grey.
        let t = Theme::classic();
        assert_eq!(rows[0].1[0], t.text_dim);
    }

    #[test]
    fn a_theme_override_reaches_the_rendered_frame() {
        let mut config = crate::config::Config::default();
        config.theme.base = Some("classic".into());
        config.theme.text_dim = Some("#ff00ff".into());
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let app = App::new(tx, std::sync::Arc::new(config));

        let rows = render_rows(&app, 60, 14);
        assert_eq!(rows[0].1[0], Color::Rgb(0xff, 0, 0xff));
    }

    /// `used / 1000` rendered every thread under 1000 tokens as "0k", which
    /// reads as a meter that is not measuring anything.
    #[test]
    fn the_context_meter_keeps_digits_below_a_thousand_tokens() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(420), "420");
        assert_eq!(fmt_tokens(1_900), "1.9k");
        assert_eq!(fmt_tokens(47_000), "47k");
        assert_eq!(fmt_tokens(131_072), "131k");
    }

    #[test]
    fn palette_popup_never_claims_rows_it_does_not_have() {
        // Normal case: one row per match, bar keeps its own row.
        assert_eq!(palette_popup_rows(5, 40), 5);
        // Capped at 8 entries regardless of match count.
        assert_eq!(palette_popup_rows(200, 40), 8);
        // Exactly enough room for 8 matches plus the bar.
        assert_eq!(palette_popup_rows(8, 9), 8);
        // One row short: the bar wins, the popup gives one up.
        assert_eq!(palette_popup_rows(8, 8), 7);
        // Degenerate heights must not wrap or panic.
        assert_eq!(palette_popup_rows(8, 1), 0);
        assert_eq!(palette_popup_rows(8, 0), 0);
    }

    #[test]
    fn palette_popup_y_offset_stays_in_range_at_every_height() {
        // Guards the exact expression in draw_command_bar:
        //   y = area.y + area.height - 1 - match_count
        for height in 1..=64u16 {
            let count = palette_popup_rows(64, height);
            assert!(
                height > count,
                "height {} would underflow with {} popup rows",
                height,
                count
            );
        }
    }

    #[test]
    fn centered_rect_survives_terminals_wider_than_655_columns() {
        // r.width * percent_x overflowed u16 above 655 columns.
        let r = Rect::new(0, 0, 1200, 40);
        let full = centered_rect(100, 10, r);
        assert_eq!((full.x, full.width), (0, 1200));
        let half = centered_rect(50, 10, r);
        assert_eq!((half.x, half.width), (300, 600));
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn build_row_skips_wide_continuation_cells() {
        let t = Theme::classic();
        // A double-width glyph occupies two vt100 cells; the second is a
        // continuation marker. It must not render as an extra space, or every
        // wide char shifts the rest of the line right by one column.
        let mut parser = vt100::Parser::new(2, 20, 0);
        parser.process("\u{4e16}x plain".as_bytes()); // 世 then ASCII
        let screen = parser.screen();
        assert!(screen.cell(0, 0).unwrap().is_wide());
        assert!(screen.cell(0, 1).unwrap().is_wide_continuation());

        let line = build_row_line(&t, &screen, 0, 20, 0, None, None);
        assert_eq!(line_text(&line), "\u{4e16}x plain");
    }

    #[test]
    fn build_row_renders_plain_ascii_unchanged() {
        let t = Theme::classic();
        let mut parser = vt100::Parser::new(2, 20, 0);
        parser.process(b"hello world");
        let line = build_row_line(&t, parser.screen(), 0, 20, 0, None, None);
        assert_eq!(line_text(&line), "hello world");
    }

    #[test]
    fn model_display_shortens_ids_for_the_status_column() {
        assert_eq!(model_display(None), "-");
        assert_eq!(model_display(Some("claude-sonnet-4-6")), "sonnet-4-6");
        assert_eq!(
            model_display(Some("claude-haiku-4-5-20251001")),
            "haiku-4-5"
        );
        assert_eq!(model_display(Some("gpt-5.4-mini")), "gpt-5.4-mini");
        assert_eq!(
            model_display(Some("some-very-long-model-name")),
            "some-very-lo"
        );
    }

    #[test]
    fn status_header_and_row_number_columns_have_equal_width() {
        // Header uses "  # " (4 cols); rows must match so the │ separators align.
        let header = "  # ";
        let row = format!("  {:1} ", 1);
        assert_eq!(header.chars().count(), row.chars().count());
    }

    #[test]
    fn prepare_display_strips_escape_sequences_and_control_chars() {
        let raw = "ok\x1b[31m red\x1b[0m\x1b]0;title\x07\tend\x07";

        assert_eq!(prepare_display(raw), "ok red  end");
    }

    #[test]
    fn prepare_display_expands_tabs_to_eight_column_stops() {
        assert_eq!(prepare_display("a\tb"), "a       b");
        assert_eq!(prepare_display("12345678\tb"), "12345678        b");
    }

    #[test]
    fn kind_color_assigns_distinct_brand_colors() {
        let t = Theme::classic();
        assert_eq!(kind_color(&t, &SessionKind::Claude), t.kind_claude);
        assert_eq!(kind_color(&t, &SessionKind::Codex), t.kind_codex);
        assert_eq!(kind_color(&t, &SessionKind::OpenCode), t.kind_opencode);
        assert_eq!(kind_color(&t, &SessionKind::OhMyPi), t.kind_ohmypi);
        assert_eq!(kind_color(&t, &SessionKind::Aider), t.kind_aider);
        assert_eq!(kind_color(&t, &SessionKind::Shell), t.kind_shell);
        assert_eq!(
            kind_color(&t, &SessionKind::Custom("x".into())),
            t.kind_custom
        );
    }

    #[test]
    fn state_border_style_highlights_waiting_error_and_active_states() {
        let t = Theme::classic();
        assert_eq!(
            state_border_style(&t, &SessionState::Waiting, false).fg,
            Some(t.warn)
        );
        assert_eq!(
            state_border_style(&t, &SessionState::Error, false).fg,
            Some(t.err)
        );
        assert_eq!(
            state_border_style(&t, &SessionState::Ready, true).fg,
            Some(t.text_bright)
        );
        assert_eq!(
            state_border_style(&t, &SessionState::Ready, false).fg,
            Some(t.text_dim)
        );
    }

    #[test]
    fn style_preserves_spaces_for_background_or_reverse_styles_only() {
        let t = Theme::classic();
        assert!(style_preserves_spaces(Style::default().bg(t.sel_bg)));
        assert!(style_preserves_spaces(
            Style::default().add_modifier(Modifier::REVERSED)
        ));
        assert!(!style_preserves_spaces(Style::default().fg(t.ok)));
    }

    #[test]
    fn centered_rect_centers_width_and_clamps_height_to_area() {
        let r = Rect {
            x: 10,
            y: 5,
            width: 100,
            height: 20,
        };

        assert_eq!(
            centered_rect(50, 30, r),
            Rect {
                x: 35,
                y: 5,
                width: 50,
                height: 20,
            }
        );
    }
    #[test]
    fn wrap_text_wraps_words_and_hard_breaks_long_runs() {
        assert_eq!(wrap_text("a bb ccc", 5), vec!["a bb", "ccc"]);
        assert_eq!(wrap_text("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap_text("", 10), vec![""]);
        assert_eq!(wrap_text("one\ntwo", 10), vec!["one", "two"]);
    }

    #[test]
    fn ellipsize_only_truncates_when_it_has_to() {
        assert_eq!(ellipsize("short", 10), "short");
        assert_eq!(ellipsize("exactfit", 8), "exactfit");
        // The ellipsis is part of the budget: the result is `width` columns.
        assert_eq!(ellipsize("truncate me", 5), "trun…");
        assert_eq!(ellipsize("anything", 0), "");
    }

    #[test]
    fn ellipsize_counts_chars_not_bytes() {
        // A multi-byte title must not be cut mid-codepoint, and its width is
        // measured in columns the terminal draws, not bytes.
        assert_eq!(ellipsize("héllo wörld", 20), "héllo wörld");
        assert_eq!(ellipsize("héllo wörld", 5), "héll…");
        assert_eq!(ellipsize("héllo wörld", 5).chars().count(), 5);
    }

    #[test]
    fn relative_age_buckets_by_magnitude() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(relative_age(now), "just now");
        assert_eq!(relative_age(now - 120), "2m");
        assert_eq!(relative_age(now - 7200), "2h");
        assert_eq!(relative_age(now - 90_000), "yesterday");
        assert_eq!(relative_age(now - 3 * 86400), "3d");
        // A timestamp from the future must not underflow into a huge age.
        assert_eq!(relative_age(now + 500), "just now");
    }

    #[test]
    fn contract_home_only_rewrites_the_home_prefix() {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return;
        }
        let inside = std::path::PathBuf::from(&home).join("src/linkshell");
        assert_eq!(contract_home(&inside), "~/src/linkshell");
        assert_eq!(
            contract_home(std::path::Path::new("/etc/hosts")),
            "/etc/hosts"
        );
    }

    #[test]
    fn split_layout_returns_two_non_overlapping_output_areas() {
        let area = Rect::new(3, 4, 101, 20);

        use crate::layout::SplitDir;

        let single = split_output_areas(area, &LayoutTree::Leaf);

        let mut row = LayoutTree::Leaf;
        row.split_leaf(0, SplitDir::Row);
        let split = split_output_areas(area, &row);

        assert_eq!(single, vec![area]);
        assert_eq!(split.len(), 2);
        assert_eq!(split[0].x, area.x);
        assert_eq!(split[1].x, split[0].x + split[0].width);
        assert_eq!(split[0].width + split[1].width, area.width);
        assert_eq!(split[0].height, area.height);
        assert_eq!(split[1].height, area.height);

        let mut col = LayoutTree::Leaf;
        col.split_leaf(0, SplitDir::Col);
        let stacked = split_output_areas(area, &col);
        assert_eq!(stacked.len(), 2);
        assert_eq!(stacked[0].y, area.y);
        assert_eq!(stacked[1].y, stacked[0].y + stacked[0].height);
        assert_eq!(stacked[0].height + stacked[1].height, area.height);
        assert_eq!(stacked[0].width, area.width);
        assert_eq!(stacked[1].width, area.width);
    }
}
