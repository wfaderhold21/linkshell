mod app;
mod claude_log;
mod codex_log;
mod config;
mod events;
mod ipc;
mod keybindings;
mod patterns;
mod pipe;
mod session;
mod ui;

use std::sync::Arc;
use std::time::Duration;

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Arc::new(config::load());

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
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
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

    // Input reader task — forwards key and mouse events
    let key_tx = tx.clone();
    tokio::spawn(async move {
        loop {
            if event::poll(Duration::from_millis(50)).unwrap_or(false) {
                match event::read() {
                    Ok(Event::Key(key)) => {
                        if key_tx.send(AppEvent::Key(key)).await.is_err() {
                            break;
                        }
                    }
                    Ok(Event::Mouse(mouse)) => {
                        if key_tx.send(AppEvent::Mouse(mouse)).await.is_err() {
                            break;
                        }
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

    loop {
        let mut layout = ui::LayoutInfo::default();
        terminal.draw(|f| {
            layout = ui::draw(f, &app);
        })?;
        app.output_area = layout.output_area;
        app.session_bar_area = layout.session_bar_area;
        app.session_slot_areas = layout.session_slot_areas;
        // Resize PTYs to match the output pane (inner area = subtract 2 for borders)
        let pty_rows = layout.output_area.height.saturating_sub(2).max(1);
        let pty_cols = layout.output_area.width.saturating_sub(2).max(1);
        app.handle_resize(pty_rows, pty_cols);

        if let Some(event) = rx.recv().await {
            handle_event(&mut app, event);
        }

        if app.should_quit {
            break;
        }
    }

    disable_raw_mode()?;
    if kitty_supported {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}

fn handle_event(app: &mut App, event: AppEvent) {
    match event {
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
        AppEvent::IpcSend {
            session_id,
            message,
        } => {
            app.handle_ipc_send(session_id, message);
        }
        AppEvent::ChatInbound {
            from_session_id,
            text,
        } => {
            app.handle_chat_inbound(from_session_id, text);
        }
        AppEvent::ChatOutbound { text } => {
            app.handle_chat_outbound(text);
        }
        AppEvent::Key(key) => handle_key(app, key),
        AppEvent::Mouse(ev) => app.handle_mouse(ev),
    }
}

fn handle_key(app: &mut App, key: crossterm::event::KeyEvent) {
    match app.mode {
        AppMode::Normal => {
            // When chat is focused, route keys to chat input.
            if app.chat.focused {
                match key.code {
                    KeyCode::Esc => {
                        app.chat.focused = false;
                    }
                    _ if app.keymap.get(&(key.modifiers, key.code)) == Some(&Action::ToggleChat) => {
                        app.chat.focused = false;
                        return;
                    }
                    KeyCode::Enter => {
                        let text = app.chat.input.clone();
                        app.chat.input.clear();
                        app.chat.input_cursor = 0;
                        app.chat.scroll_offset = 0;
                        let _ = app.event_tx.try_send(AppEvent::ChatOutbound { text });
                    }
                    KeyCode::Backspace => app.chat_input_backspace(),
                    KeyCode::Left => app.chat_cursor_left(),
                    KeyCode::Right => app.chat_cursor_right(),
                    KeyCode::Home => app.chat_cursor_home(),
                    KeyCode::End => app.chat_cursor_end(),
                    KeyCode::PageUp => app.chat_scroll_up(5),
                    KeyCode::PageDown => app.chat_scroll_down(5),
                    KeyCode::Char(c) if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT => {
                        app.chat_input_char(c);
                    }
                    _ => {}
                }
                return;
            }

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
                    Action::ToggleChat => app.chat.focused = !app.chat.focused,
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

        AppMode::CommandBar => match key.code {
            KeyCode::Esc => {
                app.mode = AppMode::Normal;
                app.command_input.clear();
            }
            KeyCode::Enter => {
                app.execute_command();
            }
            KeyCode::Backspace => {
                app.command_backspace();
            }
            KeyCode::Char(c) => {
                app.command_input_char(c);
            }
            _ => {}
        },

        AppMode::Help => {
            app.mode = AppMode::Normal; // any key dismisses
        }
    }
}

fn key_to_bytes(key: &crossterm::event::KeyEvent) -> Vec<u8> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers == KeyModifiers::CONTROL {
                if c.is_ascii_alphabetic() {
                    let b = (c.to_ascii_lowercase() as u8) - b'a' + 1;
                    return vec![b];
                }
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
        assert_eq!(key_to_bytes(&key(KeyCode::Enter, KeyModifiers::NONE)), vec![b'\r']);
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
