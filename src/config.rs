use std::collections::HashMap;

#[derive(serde::Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub socket: SocketConfig,
    pub sessions: SessionsConfig,
    pub pipe: PipeConfig,
    pub pricing: PricingConfig,
    pub keybindings: KeybindingsConfig,
    pub chat: ChatConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            socket: SocketConfig::default(),
            sessions: SessionsConfig::default(),
            pipe: PipeConfig::default(),
            pricing: PricingConfig::default(),
            keybindings: KeybindingsConfig::default(),
            chat: ChatConfig::default(),
        }
    }
}

// ── [general] ─────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, Clone, Debug)]
#[serde(default)]
pub struct GeneralConfig {
    pub max_ipc_message_bytes: usize,
    pub scroll_buffer_lines: usize,
    pub tick_interval_ms: u64,
    pub ipc_state_override_timeout_secs: u64,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            max_ipc_message_bytes: 0,
            scroll_buffer_lines: 2000,
            tick_interval_ms: 500,
            ipc_state_override_timeout_secs: 60,
        }
    }
}

// ── [socket] ──────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, Clone, Debug)]
#[serde(default)]
pub struct SocketConfig {
    pub path: String,
}

impl Default for SocketConfig {
    fn default() -> Self {
        Self {
            path: "/tmp/linkshell-{pid}.sock".to_string(),
        }
    }
}

// ── [sessions] ────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct SessionsConfig {
    pub default_cwd: String,
    pub commands: SessionCommandsConfig,
}

#[derive(serde::Deserialize, Clone, Debug)]
#[serde(default)]
pub struct SessionCommandsConfig {
    pub claude: String,
    pub codex: String,
    /// Empty string means use $SHELL.
    pub shell: String,
}

impl Default for SessionCommandsConfig {
    fn default() -> Self {
        Self {
            claude: "claude".to_string(),
            codex: "codex".to_string(),
            shell: String::new(),
        }
    }
}

// ── [pipe] ────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct PipeConfig {
    pub summarize: SummarizeConfig,
}

#[derive(serde::Deserialize, Clone, Debug)]
#[serde(default)]
pub struct SummarizeConfig {
    pub model: String,
    pub max_tokens: u32,
    pub system: String,
    pub cooldown_secs: u64,
}

impl Default for SummarizeConfig {
    fn default() -> Self {
        Self {
            model: "claude-haiku-4-5-20251001".to_string(),
            max_tokens: 150,
            system: "Extract only the concrete output, code, or decision from this text. Be terse. No preamble.".to_string(),
            cooldown_secs: 2,
        }
    }
}

// ── [pricing] ─────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, Clone, Debug)]
#[serde(default)]
pub struct PricingConfig {
    pub claude: HashMap<String, ModelRate>,
    pub codex: HashMap<String, ModelRate>,
}

impl Default for PricingConfig {
    fn default() -> Self {
        // Rates as of 2026-06. Update from:
        //   https://www.anthropic.com/pricing
        //   https://openai.com/api/pricing
        let mut claude = HashMap::new();
        claude.insert(
            "claude-opus".into(),
            ModelRate {
                input: 15.00,
                cache_write: 18.75,
                cache_read: 1.50,
                output: 75.00,
            },
        );
        claude.insert(
            "claude-sonnet".into(),
            ModelRate {
                input: 3.00,
                cache_write: 3.75,
                cache_read: 0.30,
                output: 15.00,
            },
        );
        claude.insert(
            "claude-haiku".into(),
            ModelRate {
                input: 0.80,
                cache_write: 1.00,
                cache_read: 0.08,
                output: 4.00,
            },
        );

        let mut codex = HashMap::new();
        // Codex rates are credits per 1M tokens, not USD. `cache_read` is the
        // cached-input token rate from the Codex token-based rate card.
        codex.insert(
            "gpt-5.5".into(),
            ModelRate {
                input: 125.00,
                cache_write: 0.0,
                cache_read: 12.500,
                output: 750.00,
            },
        );
        codex.insert(
            "gpt-5.4-mini".into(),
            ModelRate {
                input: 18.75,
                cache_write: 0.0,
                cache_read: 1.875,
                output: 113.00,
            },
        );
        codex.insert(
            "gpt-5.4".into(),
            ModelRate {
                input: 62.50,
                cache_write: 0.0,
                cache_read: 6.250,
                output: 375.00,
            },
        );
        codex.insert(
            "gpt-5.3-codex".into(),
            ModelRate {
                input: 43.75,
                cache_write: 0.0,
                cache_read: 4.375,
                output: 350.00,
            },
        );
        codex.insert(
            "gpt-5.2-codex".into(),
            ModelRate {
                input: 43.75,
                cache_write: 0.0,
                cache_read: 4.375,
                output: 350.00,
            },
        );
        codex.insert(
            "gpt-5.2".into(),
            ModelRate {
                input: 43.75,
                cache_write: 0.0,
                cache_read: 4.375,
                output: 350.00,
            },
        );
        codex.insert(
            "unknown".into(),
            ModelRate {
                input: 0.00,
                cache_write: 0.0,
                cache_read: 0.000,
                output: 0.00,
            },
        );

        Self { claude, codex }
    }
}

#[derive(serde::Deserialize, Clone, Debug, Default)]
pub struct ModelRate {
    pub input: f64,
    #[serde(default)]
    pub cache_write: f64,
    #[serde(default)]
    pub cache_read: f64,
    pub output: f64,
}

impl PricingConfig {
    /// Longest-prefix match on the Claude pricing table.
    /// Falls back to Sonnet rates if nothing matches.
    pub fn claude_rate(&self, model: &str) -> ModelRate {
        self.claude
            .iter()
            .filter(|(prefix, _)| model.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, r)| r.clone())
            .unwrap_or_else(|| ModelRate {
                input: 3.0,
                cache_write: 3.75,
                cache_read: 0.30,
                output: 15.0,
            })
    }

    /// Longest-prefix match on the Codex pricing table.
    /// Falls back to zero-cost "unknown" if nothing matches.
    pub fn codex_rate(&self, model: &str) -> ModelRate {
        let model = model.to_ascii_lowercase();
        self.codex
            .iter()
            .filter(|(prefix, _)| model.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, r)| r.clone())
            .unwrap_or_default()
    }
}

// ── [chat] ────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ChatConfig {
    pub enabled: bool,
    pub pane_height: u16,
    pub history_lines: usize,
    pub focus_key: String,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            pane_height: 8,
            history_lines: 500,
            focus_key: "ctrl+/".to_string(),
        }
    }
}

// ── [keybindings] ─────────────────────────────────────────────────────────

/// Variable substitutions and key→action bindings.
///
/// In config.toml:
///
///   [keybindings.vars]
///   META = "alt"
///
///   [keybindings.bind]
///   "$META+n" = "new_session"
///   "ctrl+q"  = "quit"
#[derive(serde::Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct KeybindingsConfig {
    /// Named modifier aliases, e.g. META = "alt".
    pub vars: HashMap<String, String>,
    /// chord → action, e.g. "$META+n" = "new_session".
    pub bind: HashMap<String, String>,
}

// ── Load ──────────────────────────────────────────────────────────────────

pub fn load() -> Config {
    let path = match std::env::var("HOME") {
        Ok(h) => std::path::PathBuf::from(h)
            .join(".config")
            .join("linkshell")
            .join("config.toml"),
        Err(_) => return Config::default(),
    };

    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Config::default(),
    };

    match toml::from_str::<Config>(&content) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("[linkshell] config parse error ({}): {}", path.display(), e);
            Config::default()
        }
    }
}

// ── Safety guard ──────────────────────────────────────────────────────────

const FORBIDDEN_FLAGS: &[&str] = &["--dangerously-skip-permissions"];

pub fn validate_command(cmd: &str) -> Result<(), String> {
    for flag in FORBIDDEN_FLAGS {
        if cmd.contains(flag) {
            return Err(format!(
                "linkshell refuses to spawn a session containing '{}'. \
                 Remove this flag from your config or command.",
                flag
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_contains_expected_safe_defaults() {
        let cfg = Config::default();

        assert_eq!(cfg.socket.path, "/tmp/linkshell-{pid}.sock");
        assert_eq!(cfg.general.scroll_buffer_lines, 2000);
        assert_eq!(cfg.general.tick_interval_ms, 500);
        assert_eq!(cfg.sessions.commands.claude, "claude");
        assert_eq!(cfg.sessions.commands.codex, "codex");
        assert_eq!(cfg.pipe.summarize.max_tokens, 150);
        assert_eq!(cfg.pipe.summarize.cooldown_secs, 2);
    }

    #[test]
    fn toml_overrides_only_specified_fields() {
        let cfg: Config = toml::from_str(
            r#"
            [general]
            tick_interval_ms = 100

            [sessions]
            default_cwd = "/work"

            [sessions.commands]
            shell = "/bin/zsh"

            [keybindings.vars]
            META = "ctrl"

            [keybindings.bind]
            "$META+n" = "new_session"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.general.tick_interval_ms, 100);
        assert_eq!(cfg.general.scroll_buffer_lines, 2000);
        assert_eq!(cfg.sessions.default_cwd, "/work");
        assert_eq!(cfg.sessions.commands.claude, "claude");
        assert_eq!(cfg.sessions.commands.shell, "/bin/zsh");
        assert_eq!(cfg.keybindings.vars["META"], "ctrl");
        assert_eq!(cfg.keybindings.bind["$META+n"], "new_session");
    }

    #[test]
    fn claude_rate_uses_longest_prefix_and_sonnet_fallback() {
        let mut pricing = PricingConfig::default();
        pricing.claude.insert(
            "claude-sonnet-special".into(),
            ModelRate {
                input: 9.0,
                cache_write: 10.0,
                cache_read: 1.0,
                output: 20.0,
            },
        );

        assert_eq!(pricing.claude_rate("claude-sonnet-special-2026").input, 9.0);
        assert_eq!(pricing.claude_rate("unlisted-model").input, 3.0);
    }

    #[test]
    fn codex_rate_is_case_insensitive_longest_prefix_with_zero_fallback() {
        let pricing = PricingConfig::default();

        assert_eq!(pricing.codex_rate("GPT-5.4-MINI-latest").input, 18.75);
        assert_eq!(pricing.codex_rate("gpt-5.4-mini-latest").cache_read, 1.875);
        assert_eq!(pricing.codex_rate("missing-model").output, 0.0);
    }

    #[test]
    fn validate_command_rejects_forbidden_flag_anywhere() {
        assert!(validate_command("claude").is_ok());
        let err = validate_command("claude --dangerously-skip-permissions").unwrap_err();
        assert!(err.contains("--dangerously-skip-permissions"));
    }
}
