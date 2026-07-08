use std::collections::HashMap;

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Config {
    /// Chat-addressable local LLM agents (OpenAI-compatible endpoints such as
    /// llama.cpp server, Ollama, vLLM, LM Studio):
    ///
    ///   [agents.qwen]
    ///   endpoint = "http://localhost:8080/v1"
    ///   model = "qwen3.6-27b"
    ///   system = "You are a concise coding assistant."
    ///   # api_key = "..."          # optional; sent as Bearer if set
    pub agents: HashMap<String, LocalAgent>,
    pub general: GeneralConfig,
    pub socket: SocketConfig,
    pub sessions: SessionsConfig,
    pub pipe: PipeConfig,
    pub pricing: PricingConfig,
    pub keybindings: KeybindingsConfig,
    pub profiles: Vec<Profile>,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    pub sessions: Vec<ProfileSession>,
    #[serde(default)]
    pub pipes: Vec<ProfilePipe>,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct ProfileSession {
    pub kind: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub group: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct ProfilePipe {
    pub source: String,
    pub dest: String,
    #[serde(default = "default_trigger")]
    pub trigger: String,
    #[serde(default = "default_extract")]
    pub extract: String,
    #[serde(default)]
    pub prefix: Option<String>,
}

fn default_trigger() -> String { "on_ready".into() }
fn default_extract() -> String { "last_block".into() }

// ── [general] ─────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
#[serde(default)]
pub struct GeneralConfig {
    pub max_ipc_message_bytes: usize,
    pub scroll_buffer_lines: usize,
    pub tick_interval_ms: u64,
    pub ipc_state_override_timeout_secs: u64,
    pub menu_key: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            max_ipc_message_bytes: 0,
            scroll_buffer_lines: 2000,
            tick_interval_ms: 100,
            ipc_state_override_timeout_secs: 60,
            menu_key: "ctrl+space".to_string(),
        }
    }
}

// ── [socket] ──────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
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

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, Default)]
#[serde(default)]
pub struct SessionsConfig {
    pub default_cwd: String,
    pub commands: SessionCommandsConfig,
    /// Map a command basename to a claude/codex identity, for wrapper scripts
    /// and shell aliases the classifier can't see through:
    ///
    ///   [sessions.aliases.claude-work]
    ///   kind = "claude"
    ///   config_dir = "~/.claude-work"   # exported as CLAUDE_CONFIG_DIR
    ///
    ///   [sessions.aliases.cx]
    ///   kind = "codex"
    ///   config_dir = "~/.codex-personal"  # exported as CODEX_HOME
    pub aliases: HashMap<String, SessionAlias>,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct LocalAgent {
    /// Base URL of an OpenAI-compatible server (with or without trailing /v1).
    pub endpoint: String,
    pub model: String,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct SessionAlias {
    /// "claude" or "codex"
    pub kind: String,
    /// Optional config home for this identity. Injected into the session's
    /// environment (CLAUDE_CONFIG_DIR / CODEX_HOME) and used by the JSONL
    /// watcher to find the right log directory. Supports a leading `~`.
    #[serde(default)]
    pub config_dir: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
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

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, Default)]
#[serde(default)]
pub struct PipeConfig {
    pub summarize: SummarizeConfig,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
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

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
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

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, Default)]
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
#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, Default)]
#[serde(default)]
pub struct KeybindingsConfig {
    /// Named modifier aliases, e.g. META = "alt".
    pub vars: HashMap<String, String>,
    /// chord → action, e.g. "$META+n" = "new_session".
    pub bind: HashMap<String, String>,
}

// ── Load ──────────────────────────────────────────────────────────────────

/// Location of the user config file: ~/.config/linkshell/config.toml
pub fn config_path() -> Option<std::path::PathBuf> {
    std::env::var("HOME").ok().map(|h| {
        std::path::PathBuf::from(h)
            .join(".config")
            .join("linkshell")
            .join("config.toml")
    })
}

pub fn load() -> Config {
    let path = match config_path() {
        Some(p) => p,
        None => return Config::default(),
    };

    let content = std::fs::read_to_string(&path).unwrap_or_default();

    match parse(&content) {
        Ok(mut cfg) => {
            if let Some(dir) = path.parent().map(|p| p.join("profiles.d")) {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        if entry.path().extension().and_then(|e| e.to_str()) != Some("toml") {
                            continue;
                        }
                        match std::fs::read_to_string(entry.path())
                            .map_err(anyhow::Error::from)
                            .and_then(|text| parse(&text))
                        {
                            Ok(fragment) => cfg.profiles.extend(fragment.profiles),
                            Err(error) => eprintln!(
                                "[linkshell] profile parse error ({}): {}",
                                entry.path().display(),
                                error
                            ),
                        }
                    }
                }
            }
            if let Err(error) = validate_profiles(&cfg) {
                eprintln!("[linkshell] config validation error: {}", error);
                Config::default()
            } else {
                cfg
            }
        }
        Err(e) => {
            eprintln!("[linkshell] config parse error ({}): {}", path.display(), e);
            Config::default()
        }
    }
}

/// Load and validate the primary config without silently falling back to defaults.
/// Used by diagnostics where hiding an invalid file would defeat the check.
pub fn load_strict(path: &std::path::Path) -> anyhow::Result<Config> {
    let content = std::fs::read_to_string(path)?;
    let mut cfg = parse(&content)?;
    if let Some(dir) = path.parent().map(|parent| parent.join("profiles.d")) {
        if dir.exists() {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                if entry.path().extension().and_then(|ext| ext.to_str()) != Some("toml") {
                    continue;
                }
                let content = std::fs::read_to_string(entry.path())?;
                cfg.profiles.extend(parse(&content)?.profiles);
            }
        }
    }
    validate_profiles(&cfg)?;
    Ok(cfg)
}

pub fn save_profile(profile: &Profile) -> anyhow::Result<std::path::PathBuf> {
    if profile.name.is_empty()
        || !profile.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!("profile name may contain only letters, numbers, '-' and '_'");
    }
    #[derive(serde::Serialize)]
    struct ProfilesFile<'a> {
        profiles: [&'a Profile; 1],
    }
    let base = config_path().ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    let dir = base.parent().expect("config path has parent").join("profiles.d");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.toml", profile.name));
    std::fs::write(
        &path,
        toml::to_string_pretty(&ProfilesFile { profiles: [profile] })?,
    )?;
    Ok(path)
}

pub fn parse(content: &str) -> anyhow::Result<Config> {
    let cfg: Config = toml::from_str(content)?;
    validate_profiles(&cfg)?;
    Ok(cfg)
}

fn validate_profiles(cfg: &Config) -> anyhow::Result<()> {
    use std::collections::HashSet;
    let mut profile_names = HashSet::new();
    for profile in &cfg.profiles {
        if !profile_names.insert(profile.name.as_str()) {
            anyhow::bail!("duplicate profile name '{}'", profile.name);
        }
        let mut session_names = HashSet::new();
        for session in &profile.sessions {
            if !matches!(session.kind.as_str(), "claude" | "codex" | "shell" | "custom") {
                anyhow::bail!("profile '{}': unknown session kind '{}'", profile.name, session.kind);
            }
            if session.name.is_empty() {
                anyhow::bail!("profile '{}': sessions require a name", profile.name);
            }
            if !session_names.insert(session.name.as_str()) {
                anyhow::bail!("profile '{}': duplicate session name '{}'", profile.name, session.name);
            }
            if session.kind == "custom" {
                if session.command.is_empty() {
                    anyhow::bail!("profile '{}': custom session '{}' requires command", profile.name, session.name);
                }
                validate_command(&session.command).map_err(anyhow::Error::msg)?;
            }
        }
        for pipe in &profile.pipes {
            if !session_names.contains(pipe.source.as_str())
                || !session_names.contains(pipe.dest.as_str())
            {
                anyhow::bail!("profile '{}': pipe references undefined session", profile.name);
            }
            crate::pipe::PipeTrigger::parse(&pipe.trigger)?;
            crate::pipe::ExtractMode::parse(&pipe.extract)?;
        }
    }
    Ok(())
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
    fn profile_toml_round_trips_all_fields_and_defaults() {
        let input = r#"
[[profiles]]
name = "dev"

[[profiles.sessions]]
kind = "claude"
name = "reviewer"
cwd = "~/work"
group = "council"

[[profiles.sessions]]
kind = "custom"
command = "qwen-agent"
name = "local"

[[profiles.pipes]]
source = "reviewer"
dest = "local"
trigger = "manual"
extract = "summarize:500"
prefix = "Review this:"
"#;
        let cfg = parse(input).unwrap();
        let encoded = toml::to_string(&cfg).unwrap();
        let decoded = parse(&encoded).unwrap();

        assert_eq!(decoded.profiles[0].name, "dev");
        assert_eq!(decoded.profiles[0].sessions[0].group.as_deref(), Some("council"));
        assert_eq!(decoded.profiles[0].sessions[1].command, "qwen-agent");
        assert_eq!(decoded.profiles[0].pipes[0].trigger, "manual");
        assert_eq!(decoded.profiles[0].pipes[0].extract, "summarize:500");
        assert_eq!(decoded.profiles[0].pipes[0].prefix.as_deref(), Some("Review this:"));
    }

    #[test]
    fn profiles_reject_invalid_schema_references_and_commands() {
        for input in [
            "[[profiles]]\nname='x'\n[[profiles.sessions]]\nkind='bad'\nname='a'",
            "[[profiles]]\nname='x'\n[[profiles.sessions]]\nkind='shell'\nname='a'\n[[profiles.pipes]]\nsource='a'\ndest='missing'",
            "[[profiles]]\nname='x'\n[[profiles.sessions]]\nkind='custom'\nname='a'\ncommand='tool --dangerously-skip-permissions'",
            "[[profiles]]\nname='x'\n[[profiles.sessions]]\nkind='shell'\nname='a'\n[[profiles.pipes]]\nsource='a'\ndest='a'\ntrigger='sometimes'",
            "[[profiles]]\nname='x'\n[[profiles.sessions]]\nkind='shell'\nname='a'\n[[profiles.pipes]]\nsource='a'\ndest='a'\nextract='everything'",
            "[[profiles]]\nname='x'\n[[profiles]]\nname='x'",
        ] {
            assert!(parse(input).is_err(), "accepted invalid profile: {input}");
        }
    }

    #[test]
    fn default_config_contains_expected_safe_defaults() {
        let cfg = Config::default();

        assert_eq!(cfg.socket.path, "/tmp/linkshell-{pid}.sock");
        assert_eq!(cfg.general.scroll_buffer_lines, 2000);
        assert_eq!(cfg.general.tick_interval_ms, 100);
        assert_eq!(cfg.general.menu_key, "ctrl+space");
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
    #[test]
    fn sessions_aliases_parse_kind_and_config_dir() {
        let toml = r#"
[sessions.aliases.claude-work]
kind = "claude"
config_dir = "~/.claude-work"

[sessions.aliases.cx]
kind = "codex"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let a = &cfg.sessions.aliases["claude-work"];
        assert_eq!(a.kind, "claude");
        assert_eq!(a.config_dir.as_deref(), Some("~/.claude-work"));
        let b = &cfg.sessions.aliases["cx"];
        assert_eq!(b.kind, "codex");
        assert!(b.config_dir.is_none());
        // absent table defaults to empty
        let empty: Config = toml::from_str("").unwrap();
        assert!(empty.sessions.aliases.is_empty());
    }
    #[test]
    fn agents_table_parses_local_llm_endpoints() {
        let toml = r#"
[agents.qwen]
endpoint = "http://localhost:8080/v1"
model = "qwen3.6-27b"
system = "Be concise."
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let a = &cfg.agents["qwen"];
        assert_eq!(a.endpoint, "http://localhost:8080/v1");
        assert_eq!(a.model, "qwen3.6-27b");
        assert_eq!(a.system.as_deref(), Some("Be concise."));
        assert!(a.api_key.is_none());
    }
}
