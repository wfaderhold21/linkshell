//! Colour palette for the TUI.
//!
//! Colour used to be decided inline at ~170 call sites in `ui.rs`, as a mix of
//! `Color::` literals and a pair of ad-hoc helpers. That made restyling a
//! find-and-replace across the file and made a terminal-specific fallback
//! impossible to express at all. Every colour now comes from one `Theme`,
//! carried on `App` so the `draw_*` fns reach it through the `&App` they
//! already take.
//!
//! Three bases:
//!
//! - `classic` — the palette linkshell has always shipped. Truecolor default,
//!   so adopting this module changed nothing on screen.
//! - `ansi16` — named ANSI colours only, for terminals that don't report
//!   truecolor. Auto-selected when `COLORTERM` says nothing.
//! - `dark` — the quieter restyle palette: one accent, reserved for focus.
//!
//! `[theme]` in linkshell.toml picks the base and overrides any field with a
//! hex string.

use ratatui::style::Color;

use crate::config::ThemeConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Terminal default background. Overlays and the footer paint `surface`
    /// over it; everything else leaves it alone.
    pub bg: Color,
    /// Fill for the footer strip and overlay bodies.
    pub surface: Color,
    /// Rules, separators, inactive glyphs — structure you should not read.
    pub chrome: Color,
    pub text: Color,
    pub text_dim: Color,
    pub text_bright: Color,
    /// Focus and active state, and nothing else. The restyle's whole premise
    /// is that this colour means one thing.
    pub accent: Color,
    /// WAITING, context pressure.
    pub warn: Color,
    /// ERROR.
    pub err: Color,
    /// READY, healthy.
    pub ok: Color,
    /// Token counters.
    pub info: Color,
    /// Context-window counters.
    pub ctx: Color,
    /// Money.
    pub cost: Color,
    /// Pipe glyphs and pipe labels.
    pub pipe: Color,
    /// Foreground for text sitting on an `accent`/`warn` fill.
    pub on_accent: Color,
    /// Fill behind a mouse text selection.
    pub sel_bg: Color,

    pub kind_claude: Color,
    pub kind_codex: Color,
    pub kind_opencode: Color,
    pub kind_ohmypi: Color,
    pub kind_aider: Color,
    pub kind_shell: Color,
    pub kind_custom: Color,
    pub kind_orch: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self::classic()
    }
}

impl Theme {
    /// The palette linkshell shipped before the restyle. Kept as the default
    /// so the theme refactor was a no-op on screen.
    pub fn classic() -> Self {
        Self {
            bg: Color::Reset,
            surface: Color::Reset,
            chrome: Color::DarkGray,
            text: Color::Gray,
            text_dim: Color::DarkGray,
            text_bright: Color::White,
            accent: Color::White,
            warn: Color::Yellow,
            err: Color::Red,
            ok: Color::Green,
            info: Color::Cyan,
            ctx: Color::Magenta,
            cost: Color::Green,
            pipe: Color::Cyan,
            on_accent: Color::Black,
            sel_bg: Color::Blue,
            kind_claude: Color::Rgb(255, 140, 0),
            kind_codex: Color::Rgb(64, 128, 255),
            kind_opencode: Color::Rgb(80, 200, 120),
            kind_ohmypi: Color::Rgb(200, 120, 255),
            kind_aider: Color::Rgb(0, 180, 180),
            kind_shell: Color::White,
            kind_custom: Color::Cyan,
            kind_orch: Color::Rgb(255, 215, 0),
        }
    }

    /// Named ANSI colours only — no `Color::Rgb` anywhere, so a 16- or
    /// 256-colour terminal renders the palette the user's scheme defines
    /// rather than a truecolor approximation of it.
    pub fn ansi16() -> Self {
        Self {
            bg: Color::Reset,
            surface: Color::Black,
            chrome: Color::DarkGray,
            text: Color::Gray,
            text_dim: Color::DarkGray,
            text_bright: Color::White,
            accent: Color::Cyan,
            warn: Color::Yellow,
            err: Color::Red,
            ok: Color::Green,
            info: Color::Cyan,
            ctx: Color::Magenta,
            cost: Color::Green,
            pipe: Color::Cyan,
            on_accent: Color::Black,
            sel_bg: Color::Blue,
            kind_claude: Color::Yellow,
            kind_codex: Color::Blue,
            kind_opencode: Color::Green,
            kind_ohmypi: Color::Magenta,
            kind_aider: Color::Cyan,
            kind_shell: Color::White,
            kind_custom: Color::Cyan,
            kind_orch: Color::LightYellow,
        }
    }

    /// The restyle palette: desaturated chrome, one accent.
    pub fn dark() -> Self {
        Self {
            bg: Color::Reset,
            surface: Color::Rgb(24, 26, 31),
            chrome: Color::Rgb(58, 63, 74),
            text: Color::Rgb(168, 176, 190),
            text_dim: Color::Rgb(106, 114, 128),
            text_bright: Color::Rgb(226, 232, 240),
            accent: Color::Rgb(95, 179, 212),
            warn: Color::Rgb(224, 168, 84),
            err: Color::Rgb(224, 108, 117),
            ok: Color::Rgb(126, 186, 132),
            info: Color::Rgb(122, 178, 200),
            ctx: Color::Rgb(178, 148, 214),
            cost: Color::Rgb(126, 186, 132),
            pipe: Color::Rgb(122, 178, 200),
            on_accent: Color::Rgb(16, 18, 22),
            sel_bg: Color::Rgb(48, 70, 96),
            kind_claude: Color::Rgb(214, 138, 74),
            kind_codex: Color::Rgb(108, 146, 214),
            kind_opencode: Color::Rgb(112, 178, 128),
            kind_ohmypi: Color::Rgb(174, 138, 208),
            kind_aider: Color::Rgb(96, 166, 166),
            kind_shell: Color::Rgb(168, 176, 190),
            kind_custom: Color::Rgb(122, 178, 200),
            kind_orch: Color::Rgb(212, 178, 92),
        }
    }

    pub fn by_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "classic" => Some(Self::classic()),
            "ansi16" | "ansi" | "16" => Some(Self::ansi16()),
            "dark" => Some(Self::dark()),
            _ => None,
        }
    }

    /// Resolve the configured theme. `base` wins when set; otherwise the
    /// terminal's truecolor claim decides, because `Color::Rgb` on a terminal
    /// that can't render it gets quantized to whatever is nearest, which is
    /// how a carefully-picked "dim" ends up indistinguishable from "text".
    pub fn resolve(cfg: &ThemeConfig) -> Self {
        let base = match cfg.base.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(name) => Self::by_name(name).unwrap_or_else(|| {
                eprintln!("[linkshell] unknown theme base '{name}'; using 'classic'");
                Self::classic()
            }),
            None if terminal_has_truecolor() => Self::classic(),
            None => Self::ansi16(),
        };
        base.with_overrides(cfg)
    }

    fn with_overrides(mut self, cfg: &ThemeConfig) -> Self {
        macro_rules! overlay {
            ($($field:ident),* $(,)?) => {
                $(
                    if let Some(raw) = cfg.$field.as_deref() {
                        match parse_hex(raw) {
                            Some(c) => self.$field = c,
                            None => eprintln!(
                                "[linkshell] [theme] {} = '{}' is not a #rrggbb colour; ignored",
                                stringify!($field), raw
                            ),
                        }
                    }
                )*
            };
        }
        overlay!(
            bg,
            surface,
            chrome,
            text,
            text_dim,
            text_bright,
            accent,
            warn,
            err,
            ok,
            info,
            ctx,
            cost,
            pipe,
            on_accent,
            sel_bg,
            kind_claude,
            kind_codex,
            kind_opencode,
            kind_ohmypi,
            kind_aider,
            kind_shell,
            kind_custom,
            kind_orch,
        );
        self
    }
}

/// Whether the terminal advertises 24-bit colour. Only `COLORTERM` is
/// trustworthy here: `TERM` says `xterm-256color` on virtually everything,
/// truecolor-capable or not.
pub fn terminal_has_truecolor() -> bool {
    match std::env::var("COLORTERM") {
        Ok(v) => {
            let v = v.to_ascii_lowercase();
            v.contains("truecolor") || v.contains("24bit")
        }
        Err(_) => false,
    }
}

fn parse_hex(s: &str) -> Option<Color> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let n = u32::from_str_radix(h, 16).ok()?;
    Some(Color::Rgb(
        ((n >> 16) & 0xff) as u8,
        ((n >> 8) & 0xff) as u8,
        (n & 0xff) as u8,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parsing_accepts_both_forms_and_rejects_junk() {
        assert_eq!(parse_hex("#5fb3d4"), Some(Color::Rgb(0x5f, 0xb3, 0xd4)));
        assert_eq!(parse_hex("5FB3D4"), Some(Color::Rgb(0x5f, 0xb3, 0xd4)));
        assert_eq!(parse_hex("#fff"), None);
        assert_eq!(parse_hex("blue"), None);
        assert_eq!(parse_hex(""), None);
    }

    #[test]
    fn ansi16_uses_no_truecolor() {
        let t = Theme::ansi16();
        for c in [
            t.chrome,
            t.text,
            t.text_dim,
            t.text_bright,
            t.accent,
            t.warn,
            t.err,
            t.ok,
            t.info,
            t.ctx,
            t.cost,
            t.pipe,
            t.on_accent,
            t.sel_bg,
            t.kind_claude,
            t.kind_codex,
            t.kind_opencode,
            t.kind_ohmypi,
            t.kind_aider,
            t.kind_shell,
            t.kind_custom,
            t.kind_orch,
        ] {
            assert!(!matches!(c, Color::Rgb(..)), "{c:?} is truecolor");
        }
    }

    #[test]
    fn base_selects_palette_and_fields_override_it() {
        let cfg = ThemeConfig {
            base: Some("dark".into()),
            accent: Some("#ff0000".into()),
            ..Default::default()
        };
        let t = Theme::resolve(&cfg);
        assert_eq!(t.accent, Color::Rgb(0xff, 0, 0));
        assert_eq!(t.chrome, Theme::dark().chrome);
    }

    #[test]
    fn unknown_base_falls_back_to_classic() {
        let cfg = ThemeConfig {
            base: Some("neon".into()),
            ..Default::default()
        };
        assert_eq!(Theme::resolve(&cfg), Theme::classic());
    }

    #[test]
    fn a_bad_override_is_ignored_not_fatal() {
        let cfg = ThemeConfig {
            base: Some("classic".into()),
            warn: Some("not-a-colour".into()),
            ..Default::default()
        };
        assert_eq!(Theme::resolve(&cfg).warn, Theme::classic().warn);
    }
}
