use std::collections::HashMap;

#[derive(serde::Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub general:  GeneralConfig,
    pub socket:   SocketConfig,
    pub sessions: SessionsConfig,
    pub pipe:     PipeConfig,
    pub pricing:  PricingConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general:  GeneralConfig::default(),
            socket:   SocketConfig::default(),
            sessions: SessionsConfig::default(),
            pipe:     PipeConfig::default(),
            pricing:  PricingConfig::default(),
        }
    }
}

// ── [general] ─────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, Clone, Debug)]
#[serde(default)]
pub struct GeneralConfig {
    pub max_ipc_message_bytes:          usize,
    pub scroll_buffer_lines:            usize,
    pub tick_interval_ms:               u64,
    pub ipc_state_override_timeout_secs: u64,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            max_ipc_message_bytes:           0,
            scroll_buffer_lines:          2000,
            tick_interval_ms:              500,
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
        Self { path: "/tmp/linkshell-{pid}.sock".to_string() }
    }
}

// ── [sessions] ────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct SessionsConfig {
    pub default_cwd: String,
    pub commands:    SessionCommandsConfig,
}

#[derive(serde::Deserialize, Clone, Debug)]
#[serde(default)]
pub struct SessionCommandsConfig {
    pub claude: String,
    pub codex:  String,
    /// Empty string means use $SHELL.
    pub shell:  String,
}

impl Default for SessionCommandsConfig {
    fn default() -> Self {
        Self {
            claude: "claude".to_string(),
            codex:  "codex".to_string(),
            shell:  String::new(),
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
    pub model:        String,
    pub max_tokens:   u32,
    pub system:       String,
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
    pub codex:  HashMap<String, ModelRate>,
}

impl Default for PricingConfig {
    fn default() -> Self {
        let mut claude = HashMap::new();
        claude.insert("claude-opus".into(),   ModelRate { input: 15.00, cache_write: 18.75, cache_read: 1.50,  output: 75.00 });
        claude.insert("claude-sonnet".into(), ModelRate { input:  3.00, cache_write:  3.75, cache_read: 0.30,  output: 15.00 });
        claude.insert("claude-haiku".into(),  ModelRate { input:  0.80, cache_write:  1.00, cache_read: 0.08,  output:  4.00 });

        let mut codex = HashMap::new();
        codex.insert("codex-mini".into(), ModelRate { input: 1.50, cache_write: 0.0, cache_read: 0.0, output: 6.00 });
        codex.insert("o4-mini".into(),    ModelRate { input: 1.10, cache_write: 0.0, cache_read: 0.0, output: 4.40 });
        codex.insert("unknown".into(),    ModelRate { input: 0.00, cache_write: 0.0, cache_read: 0.0, output: 0.00 });

        Self { claude, codex }
    }
}

#[derive(serde::Deserialize, Clone, Debug, Default)]
pub struct ModelRate {
    pub input:       f64,
    #[serde(default)]
    pub cache_write: f64,
    #[serde(default)]
    pub cache_read:  f64,
    pub output:      f64,
}

impl PricingConfig {
    /// Longest-prefix match on the Claude pricing table.
    /// Falls back to Sonnet rates if nothing matches.
    pub fn claude_rate(&self, model: &str) -> ModelRate {
        self.claude.iter()
            .filter(|(prefix, _)| model.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, r)| r.clone())
            .unwrap_or_else(|| ModelRate { input: 3.0, cache_write: 3.75, cache_read: 0.30, output: 15.0 })
    }

    /// Longest-prefix match on the Codex pricing table.
    /// Falls back to zero-cost "unknown" if nothing matches.
    pub fn codex_rate(&self, model: &str) -> ModelRate {
        self.codex.iter()
            .filter(|(prefix, _)| model.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, r)| r.clone())
            .unwrap_or_default()
    }
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
