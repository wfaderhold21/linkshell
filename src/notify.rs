use std::io::Write;

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Method {
    #[default]
    Auto,
    Osc9,
    NotifySend,
    Bell,
    None,
}

pub fn notify(method: Method, title: &str, body: &str) {
    let method = if method == Method::Auto {
        auto_method()
    } else {
        method
    };
    match method {
        Method::Osc9 => {
            let title = sanitize(title);
            let body = sanitize(body);
            let mut output = std::io::stdout().lock();
            let _ = write!(output, "\x1b]9;{title}: {body}\x07");
            let _ = output.flush();
        }
        Method::NotifySend => {
            let _ = std::process::Command::new("notify-send")
                .arg("--app-name=linkshell")
                .arg(title)
                .arg(body)
                .spawn();
        }
        Method::Bell => {
            let mut output = std::io::stdout().lock();
            let _ = output.write_all(b"\x07");
            let _ = output.flush();
        }
        Method::None | Method::Auto => {}
    }
}

pub fn auto_method() -> Method {
    let graphical =
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    if graphical && command_on_path("notify-send") {
        Method::NotifySend
    } else {
        Method::Osc9
    }
}

fn command_on_path(command: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join(command).is_file())
    })
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}
