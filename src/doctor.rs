use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

struct Context {
    config: Option<PathBuf>,
    claude_logs: Option<PathBuf>,
    codex_logs: Option<PathBuf>,
    socket_path: PathBuf,
    path: Vec<PathBuf>,
    term: Option<String>,
    colors: Option<u32>,
    nested_mux: Option<&'static str>,
}

pub fn run() -> i32 {
    let config = crate::config::config_path();
    let socket_path = PathBuf::from(crate::ipc::socket_path(&crate::config::Config::default()));
    let context = Context {
        config,
        claude_logs: crate::claude_log::projects_dir(None),
        codex_logs: crate::codex_log::sessions_dir(None),
        socket_path,
        path: std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect(),
        term: std::env::var("TERM").ok(),
        colors: terminal_colors(),
        nested_mux: if std::env::var_os("TMUX").is_some() {
            Some("tmux")
        } else if std::env::var_os("STY").is_some() {
            Some("screen")
        } else {
            None
        },
    };
    run_with(&context, &mut std::io::stdout())
}

fn run_with(context: &Context, out: &mut dyn Write) -> i32 {
    let mut failures = 0;
    let mut report = |name: &str, level: Level, detail: String| {
        if matches!(level, Level::Fail) {
            failures += 1;
        }
        let _ = writeln!(out, "{:<5} {:<18} {}", level.label(), name, detail);
    };

    match context.config.as_deref() {
        Some(path) if path.exists() => report(
            "config file",
            Level::Ok,
            format!("found {}", path.display()),
        ),
        Some(path) => report(
            "config file",
            Level::Warn,
            format!("not found; defaults used (expected {})", path.display()),
        ),
        None => report(
            "config file",
            Level::Fail,
            "HOME is unset; set HOME to your home directory".into(),
        ),
    }
    for command in ["claude", "codex"] {
        match find_command(command, &context.path) {
            Some(path) => report(
                &format!("{command} CLI"),
                Level::Ok,
                format!("found {}", path.display()),
            ),
            None => report(
                &format!("{command} CLI"),
                Level::Warn,
                format!("not on PATH; install {command} or update PATH"),
            ),
        }
    }
    check_dir(
        "claude JSONL logs",
        context.claude_logs.as_deref(),
        "token/cost tracking disabled",
        &mut report,
    );
    check_dir(
        "codex JSONL logs",
        context.codex_logs.as_deref(),
        "token/context tracking disabled",
        &mut report,
    );

    let parent = context.socket_path.parent().unwrap_or(Path::new("."));
    match socket_dir_status(parent) {
        Ok(None) => report(
            "socket dir",
            Level::Ok,
            format!("{} is writable and private", parent.display()),
        ),
        Ok(Some(remedy)) => report("socket dir", Level::Warn, remedy),
        Err(error) => report("socket dir", Level::Fail, error),
    }

    match context.term.as_deref() {
        None | Some("") | Some("dumb") => report(
            "terminal",
            Level::Fail,
            "TERM is unset or dumb; use a terminal with 256-color support".into(),
        ),
        Some(term) if context.colors.unwrap_or(0) < 256 => report(
            "terminal",
            Level::Warn,
            format!("TERM={term}, but fewer than 256 colors detected"),
        ),
        Some(term) => {
            let suffix = context
                .nested_mux
                .map(|mux| {
                    format!("; inside {mux}, so Alt chords may need passthrough configuration")
                })
                .unwrap_or_default();
            let level = if context.nested_mux.is_some() {
                Level::Warn
            } else {
                Level::Ok
            };
            report(
                "terminal",
                level,
                format!(
                    "TERM={term}, {} colors{suffix}",
                    context.colors.unwrap_or(0)
                ),
            );
        }
    }

    match context.config.as_deref() {
        Some(path) if path.exists() => match crate::config::load_strict(path) {
            Ok(_) => report(
                "config validity",
                Level::Ok,
                "TOML and profile references are valid".into(),
            ),
            Err(error) => report(
                "config validity",
                Level::Fail,
                format!("{error}; fix {}", path.display()),
            ),
        },
        _ => report(
            "config validity",
            Level::Ok,
            "no config file; defaults are valid".into(),
        ),
    }
    if failures == 0 {
        0
    } else {
        1
    }
}

fn check_dir(
    name: &str,
    path: Option<&Path>,
    consequence: &str,
    report: &mut impl FnMut(&str, Level, String),
) {
    match path {
        Some(path) if path.is_dir() => {
            report(name, Level::Ok, format!("{} present", path.display()))
        }
        Some(path) => report(
            name,
            Level::Warn,
            format!("{} missing; {consequence}", path.display()),
        ),
        None => report(
            name,
            Level::Warn,
            format!("home directory unavailable; {consequence}"),
        ),
    }
}

fn find_command(command: &str, path: &[PathBuf]) -> Option<PathBuf> {
    path.iter().map(|dir| dir.join(command)).find(|candidate| {
        candidate.is_file()
            && candidate
                .metadata()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    })
}

fn socket_dir_status(path: &Path) -> Result<Option<String>, String> {
    if !path.is_dir() {
        return Err(format!(
            "{} does not exist; create it with mode 0700",
            path.display()
        ));
    }
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("{} contains a NUL byte", path.display()))?;
    if unsafe { libc::access(c_path.as_ptr(), libc::W_OK) } != 0 {
        return Err(format!(
            "{} is not writable; fix ownership or permissions",
            path.display()
        ));
    }
    let permissions = path
        .metadata()
        .map_err(|e| e.to_string())?
        .permissions()
        .mode();
    let mode = permissions & 0o777;
    if mode != 0o700 {
        if permissions & 0o1000 != 0 {
            return Ok(Some(format!(
                "{} is a shared sticky directory; set socket.path = \"default\" to use a private runtime directory",
                path.display()
            )));
        }
        return Ok(Some(format!(
            "{} has mode {mode:04o}; use chmod 700 {}",
            path.display(),
            path.display()
        )));
    }
    Ok(None)
}

fn terminal_colors() -> Option<u32> {
    std::process::Command::new(OsStr::new("tput"))
        .arg("colors")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(root: &Path) -> Context {
        let bin = root.join("bin");
        let claude_logs = root.join(".claude/projects");
        let codex_logs = root.join(".codex/sessions");
        let socket_dir = root.join("run");
        for dir in [&bin, &claude_logs, &codex_logs, &socket_dir] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::set_permissions(&socket_dir, fs::Permissions::from_mode(0o700)).unwrap();
        for command in ["claude", "codex"] {
            let path = bin.join(command);
            fs::write(&path, "#!/bin/sh\n").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let config = root.join("config.toml");
        fs::write(&config, "").unwrap();
        Context {
            config: Some(config),
            claude_logs: Some(claude_logs),
            codex_logs: Some(codex_logs),
            socket_path: socket_dir.join("linkshell.sock"),
            path: vec![bin],
            term: Some("xterm-256color".into()),
            colors: Some(256),
            nested_mux: None,
        }
    }

    #[test]
    fn healthy_environment_reports_all_checks_and_succeeds() {
        let temp = tempfile_dir("healthy");
        let mut output = Vec::new();
        assert_eq!(run_with(&fixture(&temp), &mut output), 0);
        let output = String::from_utf8(output).unwrap();
        for check in [
            "config file",
            "claude CLI",
            "codex CLI",
            "claude JSONL logs",
            "codex JSONL logs",
            "socket dir",
            "terminal",
            "config validity",
        ] {
            assert!(output.contains(check), "missing {check} in {output}");
        }
        assert!(!output.contains("fail "));
    }

    #[test]
    fn warnings_do_not_fail_but_invalid_config_does() {
        let temp = tempfile_dir("invalid");
        let mut context = fixture(&temp);
        context.path.clear();
        fs::write(context.config.as_ref().unwrap(), "[broken").unwrap();
        let mut output = Vec::new();
        assert_eq!(run_with(&context, &mut output), 1);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("warn  claude CLI"));
        assert!(output.contains("fail  config validity"));
        assert!(output.contains("fix "));
    }

    #[test]
    fn invalid_profile_fragment_fails_config_validity() {
        let temp = tempfile_dir("fragment");
        let context = fixture(&temp);
        let profiles = context
            .config
            .as_ref()
            .unwrap()
            .parent()
            .unwrap()
            .join("profiles.d");
        fs::create_dir(&profiles).unwrap();
        fs::write(profiles.join("broken.toml"), "[[profiles]\n").unwrap();
        let mut output = Vec::new();
        assert_eq!(run_with(&context, &mut output), 1);
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("fail  config validity"));
    }

    #[test]
    fn missing_home_and_bad_terminal_are_failures() {
        let temp = tempfile_dir("missing");
        let mut context = fixture(&temp);
        context.config = None;
        context.term = Some("dumb".into());
        let mut output = Vec::new();
        assert_eq!(run_with(&context, &mut output), 1);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("HOME is unset"));
        assert!(output.contains("TERM is unset or dumb"));
    }

    fn tempfile_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "linkshell-doctor-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }
}
