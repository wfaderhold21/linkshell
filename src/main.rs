mod agent_llm;
mod app;
mod auth;
mod claude_log;
mod codex_log;
mod config;
mod council;
mod ctx_probe;
mod doctor;
mod events;
mod ipc;
mod keybindings;
mod notify;
mod opencode_log;
mod orchestrator;
mod patterns;
mod pipe;
mod protocol;
mod reattach;
mod session;
mod ui;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyCode, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;
use tokio::time;

use app::{App, AppMode, NewSessionField};
use events::AppEvent;
use keybindings::Action;
use reattach::{SwappableWriter, WriterBox};
use ui::FILE_BROWSER_VISIBLE_ROWS;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── Subcommands that don't enter the TUI ──────────────────────────────
    if std::env::args().nth(1).as_deref() == Some("doctor") {
        std::process::exit(doctor::run());
    }
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-r" || a == "--reattach") {
        return reattach::run_relay_client().await;
    }
    if args.iter().any(|a| a == "--server") {
        return run_server().await;
    }

    // ── Default path: client/server split (tmux-style) ─────────────────────
    // `linkshell` is a thin launcher: make sure a detached server exists,
    // then run the relay client in the foreground. Detaching just exits the
    // client — the shell comes back immediately and the server keeps running.
    launch_and_attach().await
}

/// Ensure a linkshell server is running (spawning one detached if needed),
/// then attach the relay client to it in the foreground.
async fn launch_and_attach() -> anyhow::Result<()> {
    let already_running = reattach::server_alive();
    if !already_running {
        spawn_server_detached()?;
        // Wait for the server to write its reattach info file and bind the
        // socket. The server does this early in startup, so 5s is generous.
        // The socket path must come from the info file, not be recomputed
        // here: the default path embeds the server's pid, and the info file
        // is written before the socket is bound — waiting for both avoids
        // racing the bind.
        let mut ready = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Some(sock) = reattach::recorded_reattach_socket() {
                if std::path::Path::new(&sock).exists() {
                    ready = true;
                    break;
                }
            }
        }
        if !ready {
            anyhow::bail!(
                "linkshell server did not start within 5s\n  \
                 (run `linkshell --server` in the foreground to see errors, \
                 or check {})",
                reattach::server_log_path_display()
            );
        }
    } else {
        eprintln!("[linkshell] attaching to existing session");
    }

    let result = reattach::run_relay_client().await;
    if result.is_ok() {
        println!("[linkshell] detached — sessions keep running; run `linkshell` to reattach");
    }
    result
}

/// Spawn `linkshell --server` as a daemon: new session (setsid) so it has no
/// controlling terminal and survives SSH drops, stdio detached from our tty.
/// stderr goes to a log file so server-side startup errors are diagnosable.
/// All CLI flags (--profile, --council, --tcp, ...) are forwarded verbatim.
fn spawn_server_detached() -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe()?;
    let stderr = reattach::open_server_log()
        .map(Stdio::from)
        .unwrap_or_else(Stdio::null);
    let mut cmd = Command::new(exe);
    cmd.arg("--server")
        .args(std::env::args().skip(1))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    unsafe {
        cmd.pre_exec(|| {
            // Detach from the launcher's session and controlling terminal,
            // then fork once more so the daemon is NOT a session leader: a
            // session leader that opens a pty slave (pty-process opens the
            // pts without O_NOCTTY) acquires it as its controlling terminal,
            // which makes the session child's TIOCSCTTY fail with EPERM and
            // delivers SIGHUP (→ Detach) to the server when that pty closes.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            match libc::fork() {
                -1 => Err(std::io::Error::last_os_error()),
                0 => Ok(()),         // grandchild continues to exec the server
                _ => libc::_exit(0), // intermediate exits immediately
            }
        });
    }
    let mut child = cmd.spawn()?;
    // The direct child is only the short-lived intermediate fork; reap it so
    // it doesn't linger as a zombie. The real server (grandchild) outlives
    // the launcher and is reparented to init.
    let _ = child.wait();
    Ok(())
}

/// The linkshell server: owns all sessions, renders headlessly until a relay
/// client attaches, and keeps running across attach/detach cycles. Never
/// touches its own stdio as a terminal — all terminal I/O goes over the
/// relay socket.
async fn run_server() -> anyhow::Result<()> {
    let config = Arc::new(config::load());
    let profile = match parse_profile_flag() {
        Some(name) => match config.profiles.iter().find(|profile| profile.name == name) {
            Some(profile) => Some(profile.clone()),
            None => {
                let available = config
                    .profiles
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>();
                eprintln!(
                    "linkshell: unknown profile '{}'; available profiles: {}",
                    name,
                    if available.is_empty() {
                        "(none)".into()
                    } else {
                        available.join(", ")
                    }
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

    // The server never renders to its own stdio: the writer starts as a sink
    // and is swapped to the relay socket when a client attaches. This process
    // typically has no controlling terminal at all (spawned via setsid).
    let (swappable, writer_handle) = SwappableWriter::with_sink();
    // SizedBackend answers ratatui's autoresize from this shared value instead
    // of an ioctl on a tty; the relay client's handshake dimensions win. The
    // placeholder size only matters until the first attach — nothing is
    // rendered to the sink anyway.
    let term_size: reattach::SharedSize = Arc::new(Mutex::new((80, 24)));
    let backend =
        reattach::SizedBackend::new(CrosstermBackend::new(swappable), Arc::clone(&term_size));
    let mut terminal = Terminal::new(backend)?;

    let (tx, mut rx) = mpsc::channel::<AppEvent>(256);
    let mut app = App::new(tx.clone(), Arc::clone(&config));
    if let Some(profile) = profile {
        if let Err(error) = app.apply_profile(&profile) {
            // stderr is the server log when daemonized.
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

    // Resident orchestrator agent ([orchestrator] in linkshell.toml).
    if config.orchestrator.enabled {
        if let Err(e) = app.start_orchestrator() {
            app.command_result = format!("orchestrator: {}", e);
            app.mode = AppMode::CommandResult;
        }
    }

    // No local stdin input reader: all input arrives as JSON relay events
    // from the attached client via the reattach socket.

    // Write the reattach info file so `linkshell -r` can find this session.
    // The minted token authenticates relay clients; it lives only in the
    // 0600 info file, so only the owning user can reattach.
    let ipc_socket_path = ipc::socket_path(&config);
    let reattach_token = auth::mint_token();
    reattach::write_reattach_info(std::process::id(), &ipc_socket_path, &reattach_token);

    // Reattach socket — accepts a single relay client (plain `linkshell` or
    // `linkshell -r`). One client at a time by design: a second connection
    // attempt is rejected at the handshake while `attached` is set.
    let attached = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reattach_socket_path = reattach::reattach_socket_from_ipc(&ipc_socket_path);
    spawn_reattach_listener(
        tx.clone(),
        reattach_socket_path.clone(),
        reattach_token,
        Arc::clone(&attached),
    );

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

    // SIGHUP → detach (only relevant when --server runs in a foreground tty).
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;

    let frame_cap = Duration::from_millis(16);
    let mut last_render = Instant::now() - frame_cap;
    // The server starts headless and becomes attached on the first relay
    // connection. relay_task/relay_kitty track the current connection so a
    // server-initiated detach (`:detach` / Alt+D) can restore the client's
    // terminal and drop the socket, which makes the client exit cleanly.
    let mut headless = true;
    let mut relay_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut relay_kitty = false;
    loop {
        if !headless && app.needs_redraw && last_render.elapsed() >= frame_cap {
            let mut layout = ui::LayoutInfo::default();
            if let Err(error) = terminal.draw(|f| {
                layout = ui::draw(f, &app);
            }) {
                if is_transient_terminal_error(&error) {
                    // A busy terminal or relay socket may briefly reject a
                    // frame. Keep the redraw pending and retry after the frame
                    // interval instead of terminating the whole application.
                    last_render = Instant::now();
                    continue;
                }
                return Err(error.into());
            }
            app.output_areas = layout.output_areas.clone();
            app.session_bar_area = layout.session_bar_area;
            app.session_slot_areas = layout.session_slot_areas;
            app.status_row_areas = layout.status_row_areas;
            app.new_session_area = layout.new_session_area;
            app.browse_button_area = layout.browse_button_area;
            app.file_browser_area = layout.file_browser_area;
            app.command_bar_area = layout.command_bar_area;
            app.help_area = layout.help_area;
            app.chat_area = layout.chat_area;
            app.chat_transcript_area = layout.chat_transcript_area;
            app.chat_scroll_max = layout.chat_scroll_max;
            app.chat_visible_lines = layout.chat_visible_lines;
            app.menu_bar_area = layout.menu_bar_area;
            app.menu_item_areas = layout.menu_item_areas;
            app.menu_submenu_area = layout.menu_submenu_area;
            app.menu_submenu_item_areas = layout.menu_submenu_item_areas;
            app.needs_redraw = false;
            last_render = Instant::now();

            let mut sizes = app.pane_sizes;
            for (idx, area) in layout.output_areas.iter().take(2).enumerate() {
                sizes[idx] = (
                    area.height.saturating_sub(2).max(1),
                    area.width.saturating_sub(2).max(1),
                );
            }
            let old_sizes = app.pane_sizes;
            app.handle_pane_resize(sizes);
            if app.pane_sizes != old_sizes {
                app.needs_redraw = true;
            }
        }

        if app.should_quit {
            // Stopped children can't see the PTY-close SIGHUP; continue them
            // so they exit instead of lingering as stopped orphans.
            for s in app.sessions.iter_mut() {
                let _ = s.set_paused(false);
            }
            break;
        }

        let wait = if app.needs_redraw {
            frame_cap.saturating_sub(last_render.elapsed())
        } else {
            Duration::from_millis(config.general.tick_interval_ms)
        };

        tokio::select! {
            event = rx.recv() => {
                match event {
                    None => break,
                    Some(AppEvent::Detach) => {
                        if !headless {
                            // Restore the *client's* terminal through the
                            // relay before dropping it, then close the
                            // connection: the client sees EOF and exits,
                            // returning the user's shell. Best-effort — the
                            // client restores its own terminal on exit too.
                            send_restore_sequences(&writer_handle, relay_kitty);
                            *writer_handle.lock().unwrap() = Box::new(std::io::sink());
                            if let Some(task) = relay_task.take() {
                                task.abort();
                            }
                            headless = true;
                            attached.store(false, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                    Some(AppEvent::Resize { cols, rows }) => {
                        // Feed the new dimensions to the backend; the next
                        // draw autoresizes ratatui and the post-draw pass
                        // propagates pane sizes to session PTYs.
                        *term_size.lock().unwrap() = (cols, rows);
                        app.needs_redraw = true;
                    }
                    Some(AppEvent::Reattach { stream, rows, cols, kitty }) => {
                        // Belt and braces: if a previous relay task is still
                        // around (client vanished without EOF), drop it.
                        if let Some(task) = relay_task.take() {
                            task.abort();
                        }
                        relay_task = do_reattach(
                            &mut terminal,
                            &writer_handle,
                            &tx,
                            stream,
                            kitty,
                        ).await;
                        relay_kitty = kitty;
                        attached.store(true, std::sync::atomic::Ordering::SeqCst);
                        *term_size.lock().unwrap() = (cols, rows);
                        headless = false;
                        app.needs_redraw = true;
                        // Resize all sessions to the relay client's dimensions.
                        let chrome_rows = 3 + (app.sessions.len().max(1) as u16 + 4);
                        let pty_rows = rows.saturating_sub(chrome_rows).max(1);
                        let pty_cols = cols.saturating_sub(2).max(1);
                        app.handle_pane_resize([(pty_rows, pty_cols); 2]);
                    }
                    Some(other) => handle_event(&mut app, other),
                }
            }
            _ = sighup.recv() => {
                // A daemonized server (setsid) never receives SIGHUP; this
                // only fires when run as `linkshell --server` in a foreground
                // terminal that closes. Treat it as a detach.
                let _ = tx.try_send(AppEvent::Detach);
            }
            _ = time::sleep(wait) => {}
        }
    }

    // Restore an attached client's terminal before the socket drops on exit.
    if !headless {
        send_restore_sequences(&writer_handle, relay_kitty);
    }
    if let Some(task) = relay_task.take() {
        task.abort();
    }
    reattach::clear_reattach_info();
    let _ = std::fs::remove_file(&reattach_socket_path);
    ipc::cleanup(&config);

    Ok(())
}

/// Write terminal-restore sequences to the current writer (the relay socket
/// while attached): pop kitty flags while still on the alternate screen, then
/// leave it. Best-effort; the relay client restores its own terminal on exit
/// as a second line of defense.
fn send_restore_sequences(writer_handle: &Arc<Mutex<WriterBox>>, kitty: bool) {
    let mut buf: Vec<u8> = Vec::new();
    if kitty {
        let _ = crossterm::execute!(&mut buf, PopKeyboardEnhancementFlags);
    }
    let _ = crossterm::execute!(
        &mut buf,
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste,
        crossterm::cursor::Show
    );
    let mut w = writer_handle.lock().unwrap();
    let _ = w.write_all(&buf);
    let _ = w.flush();
}

fn is_transient_terminal_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
    ) || error.raw_os_error() == Some(libc::EAGAIN)
}

fn handle_event(app: &mut App, event: AppEvent) {
    match event {
        AppEvent::ChatReply { from, text } => {
            app.handle_chat_reply(from, text);
        }
        AppEvent::Tick => app.handle_tick(),
        AppEvent::SessionBytes { session_id, data } => {
            // High-frequency path: full-screen TUIs stream bytes continuously.
            // Only flag a redraw when a visible session's frame actually
            // changed, and return early to skip the blanket needs_redraw below.
            if app.handle_session_bytes(session_id, data) {
                app.needs_redraw = true;
            }
            return;
        }
        AppEvent::SessionOutput { session_id, line } => {
            app.handle_session_output(session_id, line);
        }
        AppEvent::SessionCurrentLine { session_id, text } => {
            // ~20ms-per-session heartbeat: only a state change warrants a
            // redraw. Skip the blanket needs_redraw below.
            if app.handle_session_current_line(session_id, text) {
                app.needs_redraw = true;
            }
            return;
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
        AppEvent::SessionPid { session_id, pid } => {
            if let Some(s) = app.sessions.iter_mut().find(|s| s.id == session_id) {
                s.child_pid = Some(pid);
            }
        }
        AppEvent::SessionStats { session_id, stats } => {
            app.handle_session_stats(session_id, stats);
        }
        AppEvent::SessionBaseDetected { session_id, base } => {
            if let Some(s) = app.sessions.iter_mut().find(|s| s.id == session_id) {
                // Only upgrade unresolved identities; never override a base
                // that was already pinned at spawn time or by an alias.
                if matches!(
                    s.base,
                    session::BaseKind::Other | session::BaseKind::LocalAgent
                ) {
                    s.base = base;
                }
            }
        }
        AppEvent::SessionProvider {
            session_id,
            provider,
        } => {
            // A watcher resolved which backend the session's agent talks to
            // (e.g. opencode → lmstudio). Start the matching context-size
            // probe, once; switching providers mid-session keeps the first
            // probe — rare enough not to juggle probe lifetimes over.
            if let Some(s) = app.sessions.iter_mut().find(|s| s.id == session_id) {
                if !s.ctx_probe_spawned {
                    if let Some(backend) = ctx_probe::backend_for_provider(&provider) {
                        ctx_probe::spawn_probe(session_id, backend, app.event_tx.clone());
                        s.ctx_probe_spawned = true;
                    }
                }
            }
        }
        AppEvent::SessionContextMax { session_id, max } => {
            if let Some(s) = app.sessions.iter_mut().find(|s| s.id == session_id) {
                s.context_max = max;
            }
        }
        AppEvent::SessionModel { session_id, model } => {
            if let Some(s) = app.sessions.iter_mut().find(|s| s.id == session_id) {
                s.model = Some(model);
            }
        }
        AppEvent::SessionBillingKnown { session_id, is_pro } => {
            if let Some(s) = app.sessions.iter_mut().find(|s| s.id == session_id) {
                s.pro_sub = is_pro;
            }
        }
        AppEvent::PipeRelay { dest_id, message } => {
            app.handle_pipe_relay(dest_id, message);
        }
        AppEvent::OrchestratorRequest { req, response_tx } => {
            app.handle_orchestrator_request(req, response_tx);
        }
        AppEvent::OrchestratorProposal {
            tool,
            detail,
            response_tx,
        } => {
            app.handle_orchestrator_proposal(tool, detail, response_tx);
        }
        AppEvent::OrchestratorStatus(status) => {
            app.orchestrator_status = status.map(|s| (s, std::time::Instant::now()));
            app.needs_redraw = true;
        }
        AppEvent::OrchestratorUsage { input, output } => {
            app.handle_orchestrator_usage(input, output);
        }
        AppEvent::IpcChatPost {
            from_session_id,
            text,
        } => {
            app.handle_ipc_chat_post(from_session_id, text);
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
        // Intercepted in the main select! loop before handle_event is reached.
        AppEvent::Detach | AppEvent::Reattach { .. } | AppEvent::Resize { .. } => {}
    }
    app.needs_redraw = true;
}

fn handle_paste(app: &mut App, text: String) {
    match &app.mode {
        AppMode::Normal if app.chat_docked == Some(app.focused_pane) => app.chat_paste(&text),
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
        AppMode::Chat => app.chat_paste(&text),
        _ => {}
    }
}

fn handle_key(app: &mut App, key: crossterm::event::KeyEvent) {
    match app.mode.clone() {
        AppMode::Normal => {
            // Ctrl+F → enter search mode
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('f') {
                app.mode = AppMode::Search {
                    query: String::new(),
                    cursor: 0,
                    matches: vec![],
                    selected: 0,
                };
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
                    Action::SwitchSession(i) => {
                        // Alt+N addresses the N-th visible session; a hidden
                        // orchestrator session doesn't consume a digit.
                        if let Some(idx) = app.visible_to_idx(i) {
                            app.switch_to(idx);
                        }
                    }
                    Action::ScrollUpPage => app.scroll_up(20),
                    Action::ScrollDownPage => app.scroll_down(20),
                    Action::ScrollUpLine => app.scroll_up(3),
                    Action::ScrollDownLine => app.scroll_down(3),
                    Action::OpenMenu => app.open_menu(),
                    Action::ToggleChat => app.toggle_chat(),
                    Action::DockChat => {
                        if app.chat_docked.is_some() {
                            app.undock_chat();
                        } else {
                            app.dock_chat(None);
                        }
                    }
                    Action::ToggleSplit => app.toggle_split(),
                    Action::RotateSplit => app.rotate_split(),
                    Action::FocusNextPane => app.focus_next_pane(),
                    Action::BroadcastToggle => {
                        app.broadcast_mode = !app.broadcast_mode;
                        app.command_result = if app.broadcast_mode {
                            "Broadcast mode ON".to_string()
                        } else {
                            "Broadcast mode OFF".to_string()
                        };
                        app.mode = AppMode::CommandResult;
                    }
                    Action::Detach => {
                        let _ = app.event_tx.try_send(AppEvent::Detach);
                    }
                }
                return;
            }
            // Docked chat pane focused → keys drive the chat input
            if app.chat_docked == Some(app.focused_pane) {
                app.chat_key(key);
                return;
            }
            // Pass through to PTY
            let bytes = key_to_bytes(&key);
            if !bytes.is_empty() {
                app.write_to_active(&bytes);
            }
        }

        // Expanded Kind dropdown captures navigation keys until closed.
        AppMode::NewSession if app.new_session_state.kind_dropdown_open => match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') => {
                app.new_session_state.kind_dropdown_open = false;
            }
            KeyCode::Up => {
                app.new_session_select_kind(-1);
            }
            KeyCode::Down => {
                app.new_session_select_kind(1);
            }
            KeyCode::Char(c) => {
                if let Some(d) = c.to_digit(10) {
                    let idx = d as usize;
                    if (1..=session::SessionKind::COUNT).contains(&idx) {
                        app.new_session_state.selected_kind = idx - 1;
                        app.new_session_state.kind_dropdown_open = false;
                    }
                }
            }
            _ => {}
        },

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
            KeyCode::Enter | KeyCode::Char(' ')
                if app.new_session_state.active_field == NewSessionField::Kind =>
            {
                app.new_session_state.kind_dropdown_open = true;
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
            KeyCode::Char(c @ '1'..='9')
                if app.new_session_state.active_field == NewSessionField::Kind =>
            {
                let idx = c.to_digit(10).unwrap() as usize;
                if idx <= session::SessionKind::COUNT {
                    app.new_session_state.selected_kind = idx - 1;
                }
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
            KeyCode::Up => app.command_palette_move(-1),
            KeyCode::Down => app.command_palette_move(1),
            KeyCode::Tab => app.command_palette_insert_selected(),
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

        AppMode::PipeList => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => app.mode = AppMode::Normal,
            KeyCode::Up => app.pipe_list_move(-1),
            KeyCode::Down => app.pipe_list_move(1),
            KeyCode::Char(' ') => app.pipe_list_toggle(),
            KeyCode::Char('d') | KeyCode::Delete => app.pipe_list_delete(),
            KeyCode::Enter => app.pipe_list_fire(),
            _ => {}
        },

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

        AppMode::Search {
            mut query,
            mut cursor,
            matches,
            mut selected,
        } => {
            match key.code {
                KeyCode::Esc => {
                    app.mode = AppMode::Normal;
                    return;
                }
                KeyCode::Enter => {
                    if let Some(&line_idx) = matches.get(selected) {
                        if let Some(session) = app.active_session_mut() {
                            let total = session.output_lines.len();
                            let from_end = total.saturating_sub(line_idx + 1);
                            session.history_scroll = from_end;
                        }
                    }
                    app.mode = AppMode::Normal;
                    return;
                }
                KeyCode::Up => {
                    if !matches.is_empty() && selected > 0 {
                        selected -= 1;
                    }
                    app.mode = AppMode::Search {
                        query,
                        cursor,
                        matches,
                        selected,
                    };
                    return;
                }
                KeyCode::Down => {
                    if !matches.is_empty() && selected + 1 < matches.len() {
                        selected += 1;
                    }
                    app.mode = AppMode::Search {
                        query,
                        cursor,
                        matches,
                        selected,
                    };
                    return;
                }
                KeyCode::Backspace => {
                    if cursor > 0 {
                        let prev = query[..cursor]
                            .char_indices()
                            .next_back()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        query.remove(prev);
                        cursor = prev;
                    }
                }
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    query.insert(cursor, c);
                    cursor += c.len_utf8();
                }
                _ => {
                    app.mode = AppMode::Search {
                        query,
                        cursor,
                        matches,
                        selected,
                    };
                    return;
                }
            }
            // After query change, update matches
            let new_matches = app.search_compute_matches(&query);
            app.mode = AppMode::Search {
                query,
                cursor,
                matches: new_matches,
                selected: 0,
            };
        }

        AppMode::Settings => {
            match key.code {
                KeyCode::Esc => {
                    if app.settings_state.editing {
                        app.settings_state.editing = false;
                        app.settings_state.edit_buf.clear();
                        app.settings_state.edit_cursor = 0;
                    } else {
                        app.mode = AppMode::Normal;
                    }
                }
                KeyCode::Up if !app.settings_state.editing => {
                    app.settings_state.selected = app.settings_state.selected.saturating_sub(1);
                }
                KeyCode::Down if !app.settings_state.editing => {
                    let max = app.settings_state.fields.len().saturating_sub(1);
                    if app.settings_state.selected < max {
                        app.settings_state.selected += 1;
                    }
                }
                KeyCode::Enter => {
                    if app.settings_state.editing {
                        // Commit the edit
                        let sel = app.settings_state.selected;
                        let new_val = app.settings_state.edit_buf.clone();
                        if let Some(field) = app.settings_state.fields.get_mut(sel) {
                            field.value = new_val;
                        }
                        app.settings_state.editing = false;
                        app.settings_state.edit_buf.clear();
                        app.settings_state.edit_cursor = 0;
                    } else {
                        // Start editing
                        let sel = app.settings_state.selected;
                        if let Some(field) = app.settings_state.fields.get(sel) {
                            app.settings_state.edit_buf = field.value.clone();
                            app.settings_state.edit_cursor = field.value.len();
                            app.settings_state.editing = true;
                        }
                    }
                }
                KeyCode::Char('s') if !app.settings_state.editing => {
                    match app.apply_settings() {
                        Ok(()) => {
                            app.command_result =
                                "Saved. Some settings require restart.".to_string();
                        }
                        Err(e) => {
                            app.command_result = format!("Save failed: {}", e);
                        }
                    }
                    app.mode = AppMode::CommandResult;
                }
                KeyCode::Char(c) if app.settings_state.editing => {
                    let cursor = app.settings_state.edit_cursor;
                    app.settings_state.edit_buf.insert(cursor, c);
                    app.settings_state.edit_cursor += c.len_utf8();
                }
                KeyCode::Backspace if app.settings_state.editing => {
                    let cursor = app.settings_state.edit_cursor;
                    if cursor > 0 {
                        let prev = app.settings_state.edit_buf[..cursor]
                            .char_indices()
                            .next_back()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        app.settings_state.edit_buf.remove(prev);
                        app.settings_state.edit_cursor = prev;
                    }
                }
                KeyCode::Left if app.settings_state.editing => {
                    let cursor = app.settings_state.edit_cursor;
                    if cursor > 0 {
                        let prev = app.settings_state.edit_buf[..cursor]
                            .char_indices()
                            .next_back()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        app.settings_state.edit_cursor = prev;
                    }
                }
                KeyCode::Right if app.settings_state.editing => {
                    let cursor = app.settings_state.edit_cursor;
                    let len = app.settings_state.edit_buf.len();
                    if cursor < len {
                        let ch = app.settings_state.edit_buf[cursor..]
                            .chars()
                            .next()
                            .unwrap();
                        app.settings_state.edit_cursor += ch.len_utf8();
                    }
                }
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

// ── Reattach socket listener ───────────────────────────────────────────────

fn spawn_reattach_listener(
    tx: mpsc::Sender<AppEvent>,
    path: String,
    token: String,
    attached: Arc<std::sync::atomic::AtomicBool>,
) {
    tokio::spawn(async move {
        let _ = std::fs::remove_file(&path);
        let Ok(listener) = tokio::net::UnixListener::bind(&path) else {
            return;
        };
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        // Accept connections in a loop so a failed attempt doesn't kill
        // the listener. Only one relay client is active at a time, enforced
        // by the server-side headless flag.
        while let Ok((stream, _)) = listener.accept().await {
            #[cfg(target_os = "linux")]
            if ipc::peer_uid(&stream).ok() != Some(unsafe { libc::getuid() }) {
                continue;
            }
            let tx = tx.clone();
            let token = token.clone();
            let attached = Arc::clone(&attached);
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
                let mut reader = tokio::io::BufReader::new(stream);
                let mut line = String::new();
                // A client that never completes the handshake must not hold
                // the connection open indefinitely.
                let read =
                    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line)).await;
                if !matches!(read, Ok(Ok(n)) if n > 0) {
                    return;
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                    return;
                };
                if v["type"].as_str() != Some("reattach") {
                    return;
                }
                if v["token"].as_str() != Some(token.as_str()) {
                    let mut stream = reader.into_inner();
                    let _ = stream
                        .write_all(b"{\"ok\":false,\"error\":\"invalid reattach token\"}\n")
                        .await;
                    return;
                }
                // One client at a time, by design: reject while attached
                // instead of hijacking the existing client's session.
                if attached.load(std::sync::atomic::Ordering::SeqCst) {
                    let mut stream = reader.into_inner();
                    let _ = stream
                        .write_all(
                            b"{\"ok\":false,\"error\":\"session already attached; detach the other client first\"}\n",
                        )
                        .await;
                    return;
                }
                let rows = v["rows"].as_u64().unwrap_or(40) as u16;
                let cols = v["cols"].as_u64().unwrap_or(200) as u16;
                let kitty = v["kitty"].as_bool().unwrap_or(false);
                let mut stream = reader.into_inner();
                if stream.write_all(b"{\"ok\":true}\n").await.is_err() {
                    return;
                }
                let _ = tx
                    .send(AppEvent::Reattach {
                        stream,
                        rows,
                        cols,
                        kitty,
                    })
                    .await;
            });
        }
    });
}

// ── Reattach: swap the terminal backend and spawn the relay reader task ────

async fn do_reattach(
    terminal: &mut Terminal<reattach::SizedBackend>,
    writer_handle: &Arc<Mutex<WriterBox>>,
    event_tx: &mpsc::Sender<AppEvent>,
    stream: tokio::net::UnixStream,
    kitty_supported: bool,
) -> Option<tokio::task::JoinHandle<()>> {
    // Convert to std UnixStream so we can clone the fd for the sync writer.
    let Ok(std_stream) = stream.into_std() else {
        return None;
    };
    let _ = std_stream.set_nonblocking(false); // writer clone must be blocking
    let Ok(writer_clone) = std_stream.try_clone() else {
        return None;
    };
    let _ = std_stream.set_nonblocking(true); // back to non-blocking for tokio
    let Ok(relay_reader) = tokio::net::UnixStream::from_std(std_stream) else {
        return None;
    };

    // Point the ratatui backend at the relay socket.
    *writer_handle.lock().unwrap() = Box::new(std::io::BufWriter::new(writer_clone));

    // Re-issue terminal init sequences to the relay client: enter alternate
    // screen and re-enable mouse capture so the relay terminal is ready.
    // We buffer into Vec<u8> first because crossterm::execute! requires Sized
    // and our writer_handle holds a dyn Write trait object.
    {
        let mut buf: Vec<u8> = Vec::new();
        let _ = crossterm::execute!(
            &mut buf,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
        );
        if kitty_supported {
            let _ = crossterm::execute!(
                &mut buf,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
            );
        }
        let mut w = writer_handle.lock().unwrap();
        let _ = w.write_all(&buf);
        let _ = w.flush();
    }

    // Force ratatui to repaint everything on the relay client's terminal.
    let _ = terminal.clear();

    // Spawn relay reader: deserialize JSON events from the relay client and
    // inject them into the main event loop.
    let tx = event_tx.clone();
    let tx2 = event_tx.clone();
    let handle = tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(relay_reader);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Some(ev) = reattach::decode_relay_line(&line) {
                        if tx.send(ev).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
        // Relay client disconnected → go headless again.
        let _ = tx2.send(AppEvent::Detach).await;
    });
    // The caller updates the shared backend size from the handshake's
    // rows/cols; subsequent relay resize events carry their own dimensions.
    Some(handle)
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

    #[test]
    fn transient_terminal_errors_are_retried() {
        let would_block = std::io::Error::new(std::io::ErrorKind::WouldBlock, "busy");
        let interrupted = std::io::Error::new(std::io::ErrorKind::Interrupted, "signal");
        let eagain = std::io::Error::from_raw_os_error(libc::EAGAIN);
        let broken_pipe = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "closed");

        assert!(is_transient_terminal_error(&would_block));
        assert!(is_transient_terminal_error(&interrupted));
        assert!(is_transient_terminal_error(&eagain));
        assert!(!is_transient_terminal_error(&broken_pipe));
    }

    #[test]
    fn documentation_index_links_required_guides_and_recipes_cover_core_workflows() {
        let readme = include_str!("../README.md");
        let recipes = include_str!("../docs/recipes.md");
        let config = include_str!("../docs/config-reference.md");
        assert!(readme.contains("docs/recipes.md"));
        assert!(readme.contains("docs/config-reference.md"));
        for heading in [
            "Reviewer pipe",
            "Council quickstart",
            "Remote agent",
            "Local LLM",
        ] {
            assert!(recipes.contains(heading), "missing recipe: {heading}");
        }
        for section in [
            "[general]",
            "[socket]",
            "[sessions]",
            "[pipe.summarize]",
            "[pricing]",
            "[keybindings]",
            "[notifications]",
        ] {
            assert!(
                config.contains(section),
                "missing config section: {section}"
            );
        }
    }
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
