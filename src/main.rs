mod app;
mod events;
mod patterns;
mod session;
mod ui;

use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
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
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (tx, mut rx) = mpsc::channel::<AppEvent>(256);
    let mut app = App::new(tx.clone());

    // Key reader task
    let key_tx = tx.clone();
    tokio::spawn(async move {
        loop {
            if event::poll(Duration::from_millis(50)).unwrap_or(false) {
                if let Ok(Event::Key(key)) = event::read() {
                    if key_tx.send(AppEvent::Key(key)).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

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
        terminal.draw(|f| ui::draw(f, &app))?;

        if let Some(event) = rx.recv().await {
            handle_event(&mut app, event);
        }

        if app.should_quit {
            break;
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
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
        AppEvent::SessionDied { session_id } => {
            app.handle_session_died(session_id);
        }
        AppEvent::Key(key) => handle_key(app, key),
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
            if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Tab {
                app.next_session();
                return;
            }
            if key.modifiers == KeyModifiers::SHIFT && key.code == KeyCode::BackTab {
                app.prev_session();
                return;
            }
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('q') {
                app.should_quit = true;
                return;
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
