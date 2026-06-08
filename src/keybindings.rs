use std::collections::HashMap;
use crossterm::event::{KeyCode, KeyModifiers};
use crate::config::KeybindingsConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    NewSession,
    KillSession,
    CommandBar,
    Help,
    Quit,
    PrevSession,
    NextSession,
    SwitchSession(usize), // 0-indexed
    ScrollUpPage,
    ScrollDownPage,
    ScrollUpLine,
    ScrollDownLine,
}

pub type Keymap = HashMap<(KeyModifiers, KeyCode), Action>;

pub fn build_keymap(cfg: &KeybindingsConfig) -> Keymap {
    let mut map = default_keymap();

    for (chord_str, action_str) in &cfg.bind {
        let mut resolved = chord_str.clone();
        for (var, val) in &cfg.vars {
            resolved = resolved.replace(&format!("${}", var), val);
        }
        if let (Some(chord), Some(action)) = (parse_chord(&resolved), parse_action(action_str)) {
            map.insert(chord, action);
        } else {
            eprintln!("[linkshell] unknown keybind: '{}' = '{}'", chord_str, action_str);
        }
    }

    map
}

fn default_keymap() -> Keymap {
    let mut m = HashMap::new();
    let alt   = KeyModifiers::ALT;
    let ctrl  = KeyModifiers::CONTROL;

    m.insert((alt,   KeyCode::Char('n')),  Action::NewSession);
    m.insert((alt,   KeyCode::Char('c')),  Action::CommandBar);
    m.insert((alt,   KeyCode::Char('x')),  Action::KillSession);
    m.insert((alt,   KeyCode::Char('h')),  Action::Help);
    m.insert((ctrl,  KeyCode::Char('q')),  Action::Quit);
    m.insert((alt,   KeyCode::Left),       Action::PrevSession);
    m.insert((alt,   KeyCode::Right),      Action::NextSession);
    m.insert((alt,   KeyCode::Tab),        Action::NextSession);
    m.insert((alt,   KeyCode::BackTab),    Action::PrevSession);
    let alt_shift = KeyModifiers::ALT | KeyModifiers::SHIFT;
    m.insert((alt_shift, KeyCode::PageUp),   Action::ScrollUpPage);
    m.insert((alt_shift, KeyCode::PageDown), Action::ScrollDownPage);
    m.insert((alt_shift, KeyCode::Up),       Action::ScrollUpLine);
    m.insert((alt_shift, KeyCode::Down),     Action::ScrollDownLine);

    for i in 1u32..=8 {
        let c = char::from_digit(i, 10).unwrap();
        m.insert((alt, KeyCode::Char(c)), Action::SwitchSession((i - 1) as usize));
    }

    m
}

fn parse_chord(s: &str) -> Option<(KeyModifiers, KeyCode)> {
    let s = s.trim().to_lowercase();
    let parts: Vec<&str> = s.split('+').collect();
    if parts.is_empty() { return None; }

    let (mod_parts, key_parts) = parts.split_at(parts.len() - 1);
    let key_str = key_parts[0];

    let mut mods = KeyModifiers::NONE;
    for m in mod_parts {
        match *m {
            "alt"              => mods |= KeyModifiers::ALT,
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "shift"            => mods |= KeyModifiers::SHIFT,
            _                  => return None,
        }
    }

    let code = match key_str {
        "left"      => KeyCode::Left,
        "right"     => KeyCode::Right,
        "up"        => KeyCode::Up,
        "down"      => KeyCode::Down,
        "tab"       => KeyCode::Tab,
        "backtab"   => KeyCode::BackTab,
        "enter"     => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "pageup"    => KeyCode::PageUp,
        "pagedown"  => KeyCode::PageDown,
        "home"      => KeyCode::Home,
        "end"       => KeyCode::End,
        "delete"    => KeyCode::Delete,
        "backspace" => KeyCode::Backspace,
        s if s.chars().count() == 1 => KeyCode::Char(s.chars().next().unwrap()),
        _ => return None,
    };

    Some((mods, code))
}

fn parse_action(s: &str) -> Option<Action> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("switch_") {
        let n: usize = rest.parse().ok()?;
        if (1..=8).contains(&n) { return Some(Action::SwitchSession(n - 1)); }
        return None;
    }
    match s {
        "new_session"      => Some(Action::NewSession),
        "kill_session"     => Some(Action::KillSession),
        "command_bar"      => Some(Action::CommandBar),
        "help"             => Some(Action::Help),
        "quit"             => Some(Action::Quit),
        "prev_session"     => Some(Action::PrevSession),
        "next_session"     => Some(Action::NextSession),
        "scroll_up_page"   => Some(Action::ScrollUpPage),
        "scroll_down_page" => Some(Action::ScrollDownPage),
        "scroll_up_line"   => Some(Action::ScrollUpLine),
        "scroll_down_line" => Some(Action::ScrollDownLine),
        _                  => None,
    }
}
