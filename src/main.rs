mod app;
mod claude_log;
mod events;
mod ipc;
mod patterns;
mod pipe;
mod session;
mod ui;

use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers, EnableMouseCapture, DisableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;
use tokio::time;

use app::{App, AppMode, NewSessionField};
use events::AppEvent;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (tx, mut rx) = mpsc::channel::<AppEvent>(256);
    let mut app = App::new(tx.clone());
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
                        if key_tx.send(AppEvent::Key(key)).await.is_err() { break; }
                    }
                    Ok(Event::Mouse(mouse)) => {
                        if key_tx.send(AppEvent::Mouse(mouse)).await.is_err() { break; }
                    }
                    _ => {}
                }
            } else if key_tx.is_closed() {
                break;
            }
        }
    });

    // IPC listener — external orchestrators connect to /tmp/linkshell.sock.
    // Session 0 is the default target; orchestrators can override per message.
    ipc::spawn_listener(tx.clone());

    // Optional TCP agent listener — enabled with --tcp [PORT] (default 7373).
    let tcp_port = parse_tcp_flag();
    if let Some(port) = tcp_port {
        ipc::spawn_tcp_listener(tx.clone(), port);
    }

    // Tick task
    let tick_tx = tx.clone();
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_millis(500));
        loop {
            interval.tick().await;
            if tick_tx.send(AppEvent::Tick).await.is_err() {
                break;
            }
        }
    });

    loop {
        let mut layout = ui::LayoutInfo::default();
        terminal.draw(|f| { layout = ui::draw(f, &app); })?;
        app.output_area        = layout.output_area;
        app.session_bar_area   = layout.session_bar_area;
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
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
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
        AppEvent::SessionWriter { session_id, writer_tx } => {
            app.handle_session_writer(session_id, writer_tx);
        }
        AppEvent::SessionResizer { session_id, resizer_tx } => {
            app.handle_session_resizer(session_id, resizer_tx);
        }
        AppEvent::SessionDied { session_id } => {
            app.handle_session_died(session_id);
        }
        AppEvent::SessionStats { session_id, stats } => {
            app.handle_session_stats(session_id, stats);
        }
        AppEvent::PipeRelay { dest_id, message } => {
            // Push to a connected remote agent if registered for this session.
            if let Some(agent_tx) = app.agent_writers.get(&dest_id) {
                let relay = serde_json::json!({"type": "relay", "content": message.clone()});
                let line = serde_json::to_string(&relay).unwrap_or_default() + "\n";
                let _ = agent_tx.try_send(line);
            }
            // Write into the PTY if the session has one.
            if let Some(session) = app.sessions.iter().find(|s| s.id == dest_id) {
                session.write_bytes(message.into_bytes());
            }
        }
        AppEvent::IpcFirePipe { source, dest } => {
            app.fire_manual_pipes(source, dest);
        }
        AppEvent::IpcStateOverride { session_id, state } => {
            app.handle_ipc_state(session_id, state);
        }
        AppEvent::IpcTokenUpdate { session_id, stats } => {
            app.handle_ipc_tokens(session_id, stats);
        }
        AppEvent::IpcQuery { payload, response_tx } => {
            app.handle_ipc_query(payload, response_tx);
        }
        AppEvent::IpcAgentConnected { session_id, agent_tx } => {
            app.handle_ipc_agent_connected(session_id, agent_tx);
        }
        AppEvent::IpcAgentDisconnected { session_id } => {
            app.handle_ipc_agent_disconnected(session_id);
        }
        AppEvent::IpcSend { session_id, message } => {
            app.handle_ipc_send(session_id, message);
        }
        AppEvent::Key(key)   => handle_key(app, key),
        AppEvent::Mouse(ev)  => app.handle_mouse(ev),
    }
}

fn handle_key(app: &mut App, key: crossterm::event::KeyEvent) {
    match app.mode {
        AppMode::Normal => {
            if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Char('n') {
                app.open_new_session();
                return;
            }
            if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Char('c') {
                app.open_command_bar();
                return;
            }
            if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Char('x') {
                app.kill_active_session();
                return;
            }
            if key.modifiers == KeyModifiers::ALT {
                if let KeyCode::Char(c) = key.code {
                    if let Some(digit) = c.to_digit(10) {
                        if digit >= 1 && digit <= 8 {
                            app.switch_to((digit - 1) as usize);
                            return;
                        }
                    }
                }
            }
            if key.modifiers == KeyModifiers::ALT {
                match key.code {
                    KeyCode::Left  => { app.prev_session(); return; }
                    KeyCode::Right => { app.next_session(); return; }
                    _ => {}
                }
            }
            if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Tab {
                app.next_session();
                return;
            }
            if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::BackTab {
                app.prev_session();
                return;
            }
            if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Char('h') {
                app.mode = AppMode::Help;
                return;
            }
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('q') {
                app.should_quit = true;
                return;
            }
            // Scrollback
            match key.code {
                KeyCode::PageUp => { app.scroll_up(20); return; }
                KeyCode::PageDown => { app.scroll_down(20); return; }
                _ => {}
            }
            if key.modifiers == KeyModifiers::SHIFT {
                match key.code {
                    KeyCode::Up   => { app.scroll_up(3); return; }
                    KeyCode::Down => { app.scroll_down(3); return; }
                    _ => {}
                }
            }
            // Pass through to PTY
            let bytes = key_to_bytes(&key);
            if !bytes.is_empty() {
                app.write_to_active(&bytes);
            }
        }

        AppMode::NewSession => {
            match key.code {
                KeyCode::Esc => { app.mode = AppMode::Normal; }
                KeyCode::Tab => { app.new_session_tab(); }
                KeyCode::Enter => { let _ = app.confirm_new_session(); }
                KeyCode::Left
                    if app.new_session_state.active_field == NewSessionField::Kind =>
                {
                    app.new_session_select_kind(-1);
                }
                KeyCode::Right
                    if app.new_session_state.active_field == NewSessionField::Kind =>
                {
                    app.new_session_select_kind(1);
                }
                KeyCode::Char('1')
                    if app.new_session_state.active_field == NewSessionField::Kind =>
                {
                    app.new_session_state.selected_kind = 0;
                }
                KeyCode::Char('2')
                    if app.new_session_state.active_field == NewSessionField::Kind =>
                {
                    app.new_session_state.selected_kind = 1;
                }
                KeyCode::Char('3')
                    if app.new_session_state.active_field == NewSessionField::Kind =>
                {
                    app.new_session_state.selected_kind = 2;
                }
                KeyCode::Char('4')
                    if app.new_session_state.active_field == NewSessionField::Kind =>
                {
                    app.new_session_state.selected_kind = 3;
                }
                KeyCode::Backspace => { app.new_session_backspace(); }
                KeyCode::Char(c) => { app.new_session_input(c); }
                _ => {}
            }
        }

        AppMode::CommandBar => {
            match key.code {
                KeyCode::Esc => {
                    app.mode = AppMode::Normal;
                    app.command_input.clear();
                }
                KeyCode::Enter => { app.execute_command(); }
                KeyCode::Backspace => { app.command_backspace(); }
                KeyCode::Char(c) => { app.command_input_char(c); }
                _ => {}
            }
        }

        AppMode::Help => {
            app.mode = AppMode::Normal; // any key dismisses
        }
    }
}

fn key_to_bytes(key: &crossterm::event::KeyEvent) -> Vec<u8> {
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
        KeyCode::Enter     => vec![b'\r'],
        KeyCode::Backspace => vec![127],
        KeyCode::Tab       => vec![b'\t'],
        KeyCode::Esc       => vec![27],
        KeyCode::Up        => vec![27, b'[', b'A'],
        KeyCode::Down      => vec![27, b'[', b'B'],
        KeyCode::Right     => vec![27, b'[', b'C'],
        KeyCode::Left      => vec![27, b'[', b'D'],
        KeyCode::Home      => vec![27, b'[', b'H'],
        KeyCode::End       => vec![27, b'[', b'F'],
        KeyCode::Delete    => vec![27, b'[', b'3', b'~'],
        _                  => vec![],
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
