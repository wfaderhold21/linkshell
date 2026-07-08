mod agent_llm;
mod app;
mod auth;
mod claude_log;
mod codex_log;
mod config;
mod council;
mod events;
mod ipc;
mod keybindings;
mod patterns;
mod pipe;
mod protocol;
mod session;
mod ui;

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;
use tokio::time;

use app::{App, AppMode, NewSessionField};
use events::AppEvent;
use keybindings::Action;
use ui::FILE_BROWSER_VISIBLE_ROWS;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Arc::new(config::load());
    let profile = match parse_profile_flag() {
        Some(name) => match config.profiles.iter().find(|profile| profile.name == name) {
            Some(profile) => Some(profile.clone()),
            None => {
                let available = config.profiles.iter().map(|p| p.name.as_str()).collect::<Vec<_>>();
                eprintln!(
                    "linkshell: unknown profile '{}'; available profiles: {}",
                    name,
                    if available.is_empty() { "(none)".into() } else { available.join(", ") }
                );
                std::process::exit(1);
            }
        },
        None => None,
    };

    // Load and validate any council config *before* raw mode so parse errors
    // print as normal terminal output instead of corrupting the TUI.
    let council_cfg = match parse_council_flag() {
        Some(path) => match council::load_config_file(&path) {
            Ok(cfg) => Some(cfg),
            Err(e) => {
                eprintln!("linkshell: --council {}: {}", path, e);
                std::process::exit(1);
            }
        },
        None => None,
    };

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    // Enable kitty keyboard protocol so the outer terminal (e.g. iTerm) sends
    // extended sequences for keys like Shift+Enter that would otherwise be
    // indistinguishable from plain Enter.
    let kitty_supported = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
    )
    .is_ok();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (tx, mut rx) = mpsc::channel::<AppEvent>(256);
    let mut app = App::new(tx.clone(), Arc::clone(&config));
    // Seed pty_size from actual terminal dimensions (best-effort; refined on first draw)
    if let Ok((term_cols, term_rows)) = crossterm::terminal::size() {
        // Reserve rows for session bar (3) + status panel (sessions+3, assume 1 session = 4)
        let chrome_rows = 3 + 4;
        let rows = term_rows.saturating_sub(chrome_rows).max(1);
        let cols = term_cols.saturating_sub(2).max(1);
        app.pty_size = (rows, cols);
    }
    if let Some(profile) = profile {
        if let Err(error) = app.apply_profile(&profile) {
            disable_raw_mode()?;
            eprintln!("linkshell: profile '{}': {}", profile.name, error);
            std::process::exit(1);
        }
    }

    // Launch a council supplied via --council. Config was validated before raw
    // mode; failures here (e.g. session limit) surface in the command result bar.
    if let Some(cfg) = council_cfg {
        if let Err(e) = app.launch_council(cfg) {
            app.command_result = format!("council: {}", e);
            app.mode = AppMode::CommandResult;
        }
    }

    // Input reader task — forwards key and mouse events
    let key_tx = tx.clone();
    tokio::spawn(async move {
        loop {
            if event::poll(Duration::from_millis(50)).unwrap_or(false) {
                match event::read() {
                    Ok(Event::Key(key)) if key_tx.send(AppEvent::Key(key)).await.is_err() => {
                        break;
                    }
                    Ok(Event::Mouse(mouse))
                        if key_tx.send(AppEvent::Mouse(mouse)).await.is_err() =>
                    {
                        break;
                    }
                    Ok(Event::Paste(text))
                        if key_tx.send(AppEvent::Paste(text.clone())).await.is_err() =>
                    {
                        break;
                    }
                    _ => {}
                }
            } else if key_tx.is_closed() {
                break;
            }
        }
    });

    // IPC listener — external orchestrators connect to the per-instance socket.
    ipc::spawn_listener(tx.clone(), Arc::clone(&config));

    // Optional TCP agent listener — enabled with --tcp [PORT] (default 7373).
    let tcp_port = parse_tcp_flag();
    if let Some(port) = tcp_port {
        ipc::spawn_tcp_listener(tx.clone(), port, Arc::clone(&config));
    }

    // Tick task
    let tick_tx = tx.clone();
    let tick_ms = config.general.tick_interval_ms;
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_millis(tick_ms));
        loop {
            interval.tick().await;
            if tick_tx.send(AppEvent::Tick).await.is_err() {
                break;
            }
        }
    });

    let frame_cap = Duration::from_millis(16);
    let mut last_render = Instant::now() - frame_cap;
    loop {
        if app.needs_redraw && last_render.elapsed() >= frame_cap {
            let mut layout = ui::LayoutInfo::default();
            terminal.draw(|f| {
                layout = ui::draw(f, &app);
            })?;
            app.output_area = layout.output_area;
            app.session_bar_area = layout.session_bar_area;
            app.session_slot_areas = layout.session_slot_areas;
            app.status_row_areas = layout.status_row_areas;
            app.new_session_area = layout.new_session_area;
            app.browse_button_area = layout.browse_button_area;
            app.file_browser_area = layout.file_browser_area;
            app.command_bar_area = layout.command_bar_area;
            app.help_area = layout.help_area;
            app.chat_area = layout.chat_area;
            app.menu_bar_area = layout.menu_bar_area;
            app.menu_item_areas = layout.menu_item_areas;
            app.menu_submenu_area = layout.menu_submenu_area;
            app.menu_submenu_item_areas = layout.menu_submenu_item_areas;
            app.needs_redraw = false;
            last_render = Instant::now();

            // Resize PTYs to match the output pane (inner area = subtract 2 for borders)
            let pty_rows = layout.output_area.height.saturating_sub(2).max(1);
            let pty_cols = layout.output_area.width.saturating_sub(2).max(1);
            let old_pty_size = app.pty_size;
            app.handle_resize(pty_rows, pty_cols);
            if app.pty_size != old_pty_size {
                app.needs_redraw = true;
            }
        }

        if app.should_quit {
            break;
        }

        let wait = if app.needs_redraw {
            frame_cap.saturating_sub(last_render.elapsed())
        } else {
            Duration::from_millis(config.general.tick_interval_ms)
        };

        tokio::select! {
            event = rx.recv() => {
                if let Some(event) = event {
                    handle_event(&mut app, event);
                } else {
                    break;
                }
            }
            _ = time::sleep(wait) => {}
        }
    }

    ipc::cleanup(&config);
    disable_raw_mode()?;
    if kitty_supported {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;

    Ok(())
}

fn handle_event(app: &mut App, event: AppEvent) {
    match event {
        AppEvent::ChatReply { from, text } => {
            app.handle_chat_reply(from, text);
        }
        AppEvent::Tick => app.handle_tick(),
        AppEvent::SessionBytes { session_id, data } => {
            app.handle_session_bytes(session_id, data);
        }
        AppEvent::SessionOutput { session_id, line } => {
            app.handle_session_output(session_id, line);
        }
        AppEvent::SessionCurrentLine { session_id, text } => {
            app.handle_session_current_line(session_id, text);
        }
        AppEvent::SessionWriter {
            session_id,
            writer_tx,
        } => {
            app.handle_session_writer(session_id, writer_tx);
        }
        AppEvent::SessionResizer {
            session_id,
            resizer_tx,
        } => {
            app.handle_session_resizer(session_id, resizer_tx);
        }
        AppEvent::SessionDied { session_id } => {
            app.handle_session_died(session_id);
        }
        AppEvent::SessionStats { session_id, stats } => {
            app.handle_session_stats(session_id, stats);
        }
        AppEvent::SessionBillingKnown { session_id, is_pro } => {
            if let Some(s) = app.sessions.iter_mut().find(|s| s.id == session_id) {
                s.pro_sub = is_pro;
            }
        }
        AppEvent::PipeRelay { dest_id, message } => {
            app.handle_pipe_relay(dest_id, message);
        }
        AppEvent::AgentDirectMessage {
            from_session_id,
            dest_name,
            message,
            reply_tx,
        } => {
            app.handle_agent_direct_message(from_session_id, &dest_name, &message, reply_tx);
        }
        AppEvent::IpcFirePipe { source, dest } => {
            app.fire_manual_pipes(source, dest);
        }
        AppEvent::IpcGroupFire { source_group } => {
            app.handle_group_fire(&source_group);
        }
        AppEvent::IpcBroadcast { group, msg } => {
            app.handle_broadcast(&group, msg);
        }
        AppEvent::IpcNamedAction { session_name, msg } => {
            app.handle_named_action(session_name, msg);
        }
        AppEvent::IpcPipeAdd {
            source,
            dest,
            trigger,
            extract,
            prefix,
        } => {
            app.handle_ipc_pipe_add(source, dest, &trigger, &extract, prefix);
        }
        AppEvent::IpcPipeRemove { source, dest } => {
            app.handle_ipc_pipe_remove(source, dest);
        }
        AppEvent::IpcStateOverride { session_id, state } => {
            app.handle_ipc_state(session_id, state);
        }
        AppEvent::IpcTokenUpdate { session_id, stats } => {
            app.handle_ipc_tokens(session_id, stats);
        }
        AppEvent::IpcQuery {
            payload,
            response_tx,
        } => {
            app.handle_ipc_query(payload, response_tx);
        }
        AppEvent::IpcAgentConnected {
            session_id,
            agent_tx,
        } => {
            app.handle_ipc_agent_connected(session_id, agent_tx);
        }
        AppEvent::IpcAgentDisconnected { session_id } => {
            app.handle_ipc_agent_disconnected(session_id);
        }
        AppEvent::Key(key) => handle_key(app, key),
        AppEvent::Mouse(ev) => app.handle_mouse(ev),
        AppEvent::Paste(text) => handle_paste(app, text),
        AppEvent::Authenticate {
            token,
            transport,
            name,
            group,
            response_tx,
        } => {
            app.handle_authenticate(token, transport, name, group, response_tx);
        }
    }
    app.needs_redraw = true;
}

fn handle_paste(app: &mut App, text: String) {
    match app.mode {
        AppMode::Normal => {
            // Forward to PTY wrapped in bracketed paste sequences so the inner
            // program can distinguish pasted text from typed input.
            let mut bytes = Vec::with_capacity(text.len() + 12);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(text.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            app.write_to_active(&bytes);
        }
        AppMode::CommandBar => {
            for c in text.chars() {
                if !c.is_control() {
                    app.command_input_char(c);
                }
            }
        }
        AppMode::NewSession => {
            for c in text.chars() {
                if !c.is_control() {
                    app.new_session_input(c);
                }
            }
        }
        _ => {}
    }
}

fn handle_key(app: &mut App, key: crossterm::event::KeyEvent) {
    match app.mode {
        AppMode::Normal => {
            if let Some(action) = app.keymap.get(&(key.modifiers, key.code)).cloned() {
                match action {
                    Action::NewSession => app.open_new_session(),
                    Action::KillSession => app.kill_active_session(),
                    Action::CommandBar => app.open_command_bar(),
                    Action::Help => app.mode = AppMode::Help,
                    Action::Quit => app.should_quit = true,
                    Action::PrevSession => app.prev_session(),
                    Action::NextSession => app.next_session(),
                    Action::SwitchSession(i) => app.switch_to(i),
                    Action::ScrollUpPage => app.scroll_up(20),
                    Action::ScrollDownPage => app.scroll_down(20),
                    Action::ScrollUpLine => app.scroll_up(3),
                    Action::ScrollDownLine => app.scroll_down(3),
                    Action::OpenMenu => app.open_menu(),
                    Action::ToggleChat => app.toggle_chat(),
                }
                return;
            }
            // Pass through to PTY
            let bytes = key_to_bytes(&key);
            if !bytes.is_empty() {
                app.write_to_active(&bytes);
            }
        }

        AppMode::NewSession => match key.code {
            KeyCode::Esc => {
                app.mode = AppMode::Normal;
            }
            KeyCode::Tab => {
                app.new_session_tab();
            }
            KeyCode::Char('b')
                if key.modifiers == crossterm::event::KeyModifiers::ALT
                    && app.new_session_state.active_field == NewSessionField::Cwd =>
            {
                app.open_file_browser();
            }
            KeyCode::Enter => {
                let _ = app.confirm_new_session();
            }
            KeyCode::Left if app.new_session_state.active_field == NewSessionField::Kind => {
                app.new_session_select_kind(-1);
            }
            KeyCode::Left => {
                app.new_session_cursor_left();
            }
            KeyCode::Right if app.new_session_state.active_field == NewSessionField::Kind => {
                app.new_session_select_kind(1);
            }
            KeyCode::Right => {
                app.new_session_cursor_right();
            }
            KeyCode::Home => {
                app.new_session_cursor_home();
            }
            KeyCode::End => {
                app.new_session_cursor_end();
            }
            KeyCode::Delete => {
                app.new_session_delete();
            }
            KeyCode::Char('1') if app.new_session_state.active_field == NewSessionField::Kind => {
                app.new_session_state.selected_kind = 0;
            }
            KeyCode::Char('2') if app.new_session_state.active_field == NewSessionField::Kind => {
                app.new_session_state.selected_kind = 1;
            }
            KeyCode::Char('3') if app.new_session_state.active_field == NewSessionField::Kind => {
                app.new_session_state.selected_kind = 2;
            }
            KeyCode::Char('4') if app.new_session_state.active_field == NewSessionField::Kind => {
                app.new_session_state.selected_kind = 3;
            }
            KeyCode::Backspace => {
                app.new_session_backspace();
            }
            KeyCode::Char(c) => {
                app.new_session_input(c);
            }
            _ => {}
        },

        AppMode::FileBrowser => match key.code {
            KeyCode::Esc => {
                app.file_browser_cancel();
            }
            KeyCode::Up => {
                app.file_browser_up();
            }
            KeyCode::Down => {
                app.file_browser_down(FILE_BROWSER_VISIBLE_ROWS);
            }
            KeyCode::Enter => {
                app.file_browser_enter();
            }
            KeyCode::Char(' ') => {
                app.file_browser_select();
            }
            _ => {}
        },

        AppMode::CommandBar => match key.code {
            KeyCode::Esc => {
                app.mode = AppMode::Normal;
                app.command_input.clear();
                app.command_cursor = 0;
            }
            KeyCode::Enter => {
                app.execute_command();
            }
            KeyCode::Backspace => {
                app.command_backspace();
            }
            KeyCode::Left => {
                app.command_cursor_left();
            }
            KeyCode::Right => {
                app.command_cursor_right();
            }
            KeyCode::Home => {
                app.command_cursor_home();
            }
            KeyCode::End => {
                app.command_cursor_end();
            }
            KeyCode::Char(c) => {
                app.command_input_char(c);
            }
            _ => {}
        },

        AppMode::Chat => {
            app.chat_key(key);
        }

        AppMode::Help | AppMode::CommandResult => {
            app.mode = AppMode::Normal; // any key dismisses
        }

        AppMode::Menu { .. } => {
            if app.keymap.get(&(key.modifiers, key.code)) == Some(&Action::OpenMenu) {
                app.mode = AppMode::Normal;
                return;
            }
            match key.code {
                KeyCode::Left => app.menu_move_top(-1),
                KeyCode::Right => app.menu_move_top(1),
                KeyCode::Down => app.menu_open_submenu(),
                KeyCode::Up => app.menu_close_submenu(),
                KeyCode::Enter => app.execute_selected_menu_action(),
                KeyCode::Esc => app.mode = AppMode::Normal,
                KeyCode::Char(c) => match c.to_ascii_lowercase() {
                    's' => {
                        app.mode = AppMode::Menu {
                            selected_top: 0,
                            selected_sub: Some(0),
                        }
                    }
                    'v' => {
                        app.mode = AppMode::Menu {
                            selected_top: 1,
                            selected_sub: Some(0),
                        }
                    }
                    'p' => {
                        app.mode = AppMode::Menu {
                            selected_top: 2,
                            selected_sub: Some(0),
                        }
                    }
                    'h' => {
                        app.mode = AppMode::Menu {
                            selected_top: 3,
                            selected_sub: Some(0),
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
}

fn key_to_bytes(key: &crossterm::event::KeyEvent) -> Vec<u8> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers == KeyModifiers::CONTROL && c.is_ascii_alphabetic() {
                let b = (c.to_ascii_lowercase() as u8) - b'a' + 1;
                return vec![b];
            }
            let mut buf = [0u8; 4];
            c.encode_utf8(&mut buf).as_bytes().to_vec()
        }
        // Shift+Enter: ESC [ 13 ; 2 u (kitty/xterm extended)
        KeyCode::Enter => {
            if shift {
                vec![27, b'[', b'1', b'3', b';', b'2', b'u']
            } else {
                vec![b'\r']
            }
        }
        KeyCode::Backspace => vec![127],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![27, b'[', b'Z'],
        KeyCode::Esc => vec![27],
        KeyCode::Up => {
            if shift {
                vec![27, b'[', b'1', b';', b'2', b'A']
            } else {
                vec![27, b'[', b'A']
            }
        }
        KeyCode::Down => {
            if shift {
                vec![27, b'[', b'1', b';', b'2', b'B']
            } else {
                vec![27, b'[', b'B']
            }
        }
        KeyCode::Right => {
            if shift {
                vec![27, b'[', b'1', b';', b'2', b'C']
            } else {
                vec![27, b'[', b'C']
            }
        }
        KeyCode::Left => {
            if shift {
                vec![27, b'[', b'1', b';', b'2', b'D']
            } else {
                vec![27, b'[', b'D']
            }
        }
        KeyCode::Home => {
            if shift {
                vec![27, b'[', b'1', b';', b'2', b'H']
            } else {
                vec![27, b'[', b'H']
            }
        }
        KeyCode::End => {
            if shift {
                vec![27, b'[', b'1', b';', b'2', b'F']
            } else {
                vec![27, b'[', b'F']
            }
        }
        KeyCode::Delete => {
            if shift {
                vec![27, b'[', b'3', b';', b'2', b'~']
            } else {
                vec![27, b'[', b'3', b'~']
            }
        }
        KeyCode::PageUp => {
            if shift {
                vec![27, b'[', b'5', b';', b'2', b'~']
            } else {
                vec![27, b'[', b'5', b'~']
            }
        }
        KeyCode::PageDown => {
            if shift {
                vec![27, b'[', b'6', b';', b'2', b'~']
            } else {
                vec![27, b'[', b'6', b'~']
            }
        }
        _ => vec![],
    }
}

fn parse_council_flag() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--council" {
            return args.get(i + 1).cloned().or_else(|| {
                eprintln!("linkshell: --council requires a path to a council.toml");
                std::process::exit(1);
            });
        }
        if let Some(p) = args[i].strip_prefix("--council=") {
            return Some(p.to_string());
        }
        i += 1;
    }
    None
}

fn parse_profile_flag() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--profile" {
            return args.get(i + 1).cloned().or_else(|| {
                eprintln!("linkshell: --profile requires a name");
                std::process::exit(2);
            });
        }
        if let Some(name) = args[i].strip_prefix("--profile=") {
            if name.is_empty() {
                eprintln!("linkshell: --profile requires a name");
                std::process::exit(2);
            }
            return Some(name.to_string());
        }
        i += 1;
    }
    None
}

fn parse_tcp_flag() -> Option<u16> {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--tcp" {
            let port = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(7373);
            return Some(port);
        }
        if let Some(p) = args[i].strip_prefix("--tcp=") {
            return Some(p.parse().unwrap_or(7373));
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn key_to_bytes_maps_printable_control_and_navigation_keys() {
        assert_eq!(
            key_to_bytes(&key(KeyCode::Char('a'), KeyModifiers::NONE)),
            b"a".to_vec()
        );
        assert_eq!(
            key_to_bytes(&key(KeyCode::Char('C'), KeyModifiers::CONTROL)),
            vec![3]
        );
        assert_eq!(
            key_to_bytes(&key(KeyCode::Enter, KeyModifiers::NONE)),
            vec![b'\r']
        );
        assert_eq!(
            key_to_bytes(&key(KeyCode::Left, KeyModifiers::NONE)),
            vec![27, b'[', b'D']
        );
    }

    #[test]
    fn key_to_bytes_emits_extended_sequences_for_shifted_keys() {
        assert_eq!(
            key_to_bytes(&key(KeyCode::Enter, KeyModifiers::SHIFT)),
            vec![27, b'[', b'1', b'3', b';', b'2', b'u']
        );
        assert_eq!(
            key_to_bytes(&key(KeyCode::Delete, KeyModifiers::SHIFT)),
            vec![27, b'[', b'3', b';', b'2', b'~']
        );
    }
}
