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
    pub theme: ThemeConfig,
    pub socket: SocketConfig,
    pub sessions: SessionsConfig,
    pub pipe: PipeConfig,
    pub pricing: PricingConfig,
    pub keybindings: KeybindingsConfig,
    pub notifications: NotificationsConfig,
    pub orchestrator: OrchestratorConfig,
    pub chat: ChatConfig,
    pub planning: PlanningConfig,
    pub profiles: Vec<Profile>,
    /// User-defined personas; a name matching a builtin replaces it.
    #[serde(default)]
    pub personas: Vec<Persona>,
}

// ── [orchestrator] ────────────────────────────────────────────────────────

/// The resident orchestrator agent: monitors sessions, chats via the chat
/// pane, starts sessions and routes work on the user's behalf.
///
///   [orchestrator]
///   enabled = true
///   provider = "anthropic"     # anthropic | openai | lmstudio (API class)
///                              # claude | codex | opencode | omp (CLI class)
///   model = "claude-opus-4-8"  # API class only
#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
#[serde(default)]
pub struct OrchestratorConfig {
    pub enabled: bool,
    pub provider: String,
    /// Chat target name (`@agent ...`) and CLI-class session name.
    pub name: String,
    /// API class: model id.
    pub model: String,
    /// openai/lmstudio base URL; also overrides the anthropic base URL.
    /// Anthropic falls back to the ANTHROPIC_BASE_URL env var when empty.
    pub endpoint: String,
    /// Falls back to ANTHROPIC_API_KEY / OPENAI_API_KEY env vars when empty.
    pub api_key: String,
    /// Anthropic bearer token (`Authorization: Bearer ...`), used instead of
    /// the x-api-key header. Some gateways (e.g. NVIDIA) require this. Takes
    /// precedence over `api_key`; falls back to the ANTHROPIC_AUTH_TOKEN env
    /// var when empty.
    pub auth_token: String,
    /// Appended to the built-in system prompt / CLI briefing.
    pub system: String,
    /// Directory of skill files (*.md) the orchestrator can pull in on
    /// demand. Each file is one skill: the file stem is the skill name, the
    /// description comes from a `description:` line in leading `---`
    /// frontmatter (or the first non-empty line). API-class orchestrators
    /// load skills through the `use_skill` tool; CLI-class orchestrators get
    /// the file paths in their briefing. Empty = ~/.config/linkshell/skills
    /// when that directory exists.
    pub skills_dir: String,
    /// Persistent agent memory: one markdown file injected into the
    /// orchestrator's prompt every turn and appended to by its `remember`
    /// tool. The whole file goes in verbatim — keep it concise; entries the
    /// agent adds are dated bullets you can prune by hand. Empty =
    /// ~/.config/linkshell/memory.md (created on first orchestrator start).
    pub memory_file: String,
    /// CLI class: working directory for the orchestrator session.
    pub cwd: String,
    /// CLI class: keep the orchestrator session out of the session bar and
    /// session switching; interact with it through the chat pane instead.
    /// `/orchestrator show` / `hide` toggles it at runtime.
    pub hidden: bool,
    /// CLI class: how much the orchestrator CLI may do without asking.
    /// "accept-edits" (default) starts the CLI with safe auto-approval flags
    /// (claude: `--permission-mode acceptEdits`, codex: `--full-auto`);
    /// "default" leaves the CLI's own prompting untouched. Anything that
    /// would bypass the CLI's sandboxing entirely is rejected.
    pub permission_mode: String,
    /// Session states that proactively wake the orchestrator.
    pub events: Vec<String>,
    /// Minimum seconds between events for the same (session, state).
    pub event_cooldown_secs: u64,
    /// "auto" (default): tools run immediately. "propose": tool calls not in
    /// auto_approve are held as proposals in the chat pane until /approve or
    /// /deny; the model's turn blocks on the verdict, so its context stays
    /// coherent — from the model's perspective approval is just a slow tool.
    pub approval: String,
    /// Tools that skip the propose gate. Defaults to the read-only set.
    /// kill_session always uses its own /confirm-kill flow and is never
    /// double-gated here.
    pub auto_approve: Vec<String>,
    /// Propose mode: seconds before an unanswered proposal resolves as
    /// denied ("no response from user") and the turn continues.
    pub approval_timeout_secs: u64,
    pub max_history_turns: usize,
    pub max_tokens: u32,
    pub max_tool_iterations: usize,
    pub input_wait_timeout_secs: u64,
    /// Soft token budget for the conversation history, estimated at ~4
    /// chars/token. When the estimate exceeds this, oldest turns are dropped
    /// even if max_history_turns hasn't been reached. 0 disables.
    pub max_context_tokens: usize,
    /// How many recent user turns keep their tool results verbatim. Tool
    /// results older than this are replaced with a short elision stub (the
    /// model can re-run the tool if it still needs the data). 0 disables.
    pub tool_result_keep_turns: usize,
    /// Lines of session output inlined into a [linkshell event]
    /// notification. The orchestrator can always read_output for more.
    pub event_tail_lines: usize,
    /// Tool names the orchestrator may call. Empty means the full set. This
    /// is the mechanical half of a persona: a system-prompt instruction to be
    /// cautious is a suggestion to a small local model, whereas omitting
    /// `send_input` from the schema is a guarantee.
    pub allowed_tools: Vec<String>,
    /// Extra text appended to the system prompt by the active persona.
    pub persona_note: String,
    /// Name of the active persona (informational; shown in the status row).
    pub persona: String,
    /// Seconds during which an identical (tool, arguments) call is answered
    /// with a duplicate_call error instead of being re-executed. Bounds the
    /// cross-turn loops that max_tool_iterations (per-turn) cannot see.
    /// 0 disables.
    pub tool_dedup_secs: u64,
    /// Cap on lines returned by send_input wait_ready / `input --wait`.
    /// Longer replies are truncated to the last N lines with a marker.
    /// 0 disables.
    pub wait_ready_max_lines: usize,
    /// Models offered by the Orchestrator menu's Model row. The menu cycles
    /// this list; it does not query the provider, because the endpoint may be
    /// a local server whose loaded model set changes independently of what is
    /// worth switching between.
    pub models: Vec<String>,
    /// Providers offered by the menu's Provider row. Empty = the built-in set.
    pub providers: Vec<String>,
    /// Context budgets offered by the menu. Empty = a built-in ladder.
    pub context_choices: Vec<usize>,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "anthropic".to_string(),
            name: "agent".to_string(),
            model: String::new(),
            endpoint: String::new(),
            api_key: String::new(),
            auth_token: String::new(),
            system: String::new(),
            skills_dir: String::new(),
            memory_file: String::new(),
            cwd: String::new(),
            hidden: true,
            permission_mode: "accept-edits".to_string(),
            // "ready" is deliberately absent: sessions going idle is the most
            // frequent and least actionable transition, and each event costs
            // a full orchestrator turn (expensive on local models). Add
            // "ready" to [orchestrator].events to opt back in.
            events: vec!["waiting".into(), "error".into(), "dead".into()],
            event_cooldown_secs: 30,
            approval: "auto".to_string(),
            auto_approve: vec![
                "list_sessions".into(),
                "read_output".into(),
                "use_skill".into(),
                // Writes only to the agent's own memory file.
                "remember".into(),
            ],
            approval_timeout_secs: 600,
            max_history_turns: 40,
            max_tokens: 4096,
            max_tool_iterations: 12,
            input_wait_timeout_secs: 180,
            max_context_tokens: 60_000,
            tool_result_keep_turns: 3,
            event_tail_lines: 5,
            allowed_tools: Vec::new(),
            persona_note: String::new(),
            persona: String::new(),
            tool_dedup_secs: 45,
            wait_ready_max_lines: 80,
            models: Vec::new(),
            providers: Vec::new(),
            context_choices: Vec::new(),
        }
    }
}

// ── [chat] ────────────────────────────────────────────────────────────────

/// The chat pane overlay (Alt+T).
#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
#[serde(default)]
pub struct ChatConfig {
    /// Popup width as a percentage of the terminal width (20–95).
    pub width_pct: u16,
    /// Popup height as a percentage of the terminal height (20–95).
    pub height_pct: u16,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            width_pct: 60,
            height_pct: 60,
        }
    }
}

// ── [planning] ────────────────────────────────────────────────────────────

/// The planning pane: a persistent, read-only, single-agent design chat.
///
/// Backends are selected at runtime from the pane, so several are configured
/// and none is privileged. A thread can be built cheaply on a local model and
/// distilled by a frontier one.
///
///   [planning]
///   default_backend = "local"
///   distill_backend = "opus"      # falls back to default_backend
///
///   [planning.backends.local]
///   provider = "lmstudio"
///   endpoint = "http://localhost:1234/v1"
///   model = "qwen3.6-27b"
///   max_context_tokens = 28000
///
///   [planning.backends.opus]
///   provider = "anthropic"
///   model = "claude-opus-4-8"
///   max_context_tokens = 180000
#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, Default)]
#[serde(default)]
pub struct PlanningConfig {
    /// Backend selected when a pane opens. Empty = first by name.
    pub default_backend: String,
    /// Backend used by the commit step. Empty = `default_backend`.
    pub distill_backend: String,
    /// Sidebar width as a percentage of the pane (15-50).
    pub sidebar_pct: u16,
    pub backends: HashMap<String, crate::planning::Backend>,
    /// Backends inferred from model endpoints configured elsewhere —
    /// `[agents.*]` and an API-class `[orchestrator]` (see
    /// `Config::derive_planning_backends`). Without these a config that has
    /// never named `[planning.backends.*]` offers nothing to pick, and the
    /// pane's whole point is picking. Explicit entries shadow them by name.
    ///
    /// Skipped by serde in both directions: they are not the user's config
    /// and must not be written back by `save()` as though they were.
    #[serde(skip)]
    pub derived: HashMap<String, crate::planning::Backend>,
}

impl PlanningConfig {
    /// Backend names in a stable display order for the picker.
    pub fn backend_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.backends.keys().cloned().collect();
        for name in self.derived.keys() {
            if !self.backends.contains_key(name) {
                names.push(name.clone());
            }
        }
        names.sort();
        names
    }

    /// Look up a backend, stamping its config key into `name` — the key is
    /// what gets recorded per message, so it must not be lost in
    /// deserialization.
    pub fn backend(&self, name: &str) -> Option<crate::planning::Backend> {
        self.backends.get(name).or(self.derived.get(name)).map(|b| {
            let mut b = b.clone();
            b.name = name.to_string();
            b
        })
    }

    /// Backend a newly opened pane starts on.
    pub fn default_backend(&self) -> Option<crate::planning::Backend> {
        self.backend(&self.default_backend)
            .or_else(|| self.backend_names().first().and_then(|n| self.backend(n)))
    }

    /// Backend the commit step distills with. Chosen independently so the
    /// artifact that actually gets handed to an agent can use a better model
    /// than the conversation did.
    pub fn distill_backend(&self) -> Option<crate::planning::Backend> {
        self.backend(&self.distill_backend)
            .or_else(|| self.default_backend())
    }

    /// Clamped sidebar width.
    pub fn sidebar_width_pct(&self) -> u16 {
        if self.sidebar_pct == 0 {
            28
        } else {
            self.sidebar_pct.clamp(15, 50)
        }
    }
}

pub enum ApiProvider {
    Anthropic,
    OpenAi, // also serves LM Studio
}

pub enum OrchestratorClass {
    Api(ApiProvider),
    /// Session-kind name for `SessionKind::from_name`.
    Cli(&'static str),
}

impl OrchestratorConfig {
    /// True if this tool call must be approved by the human before running.
    /// Provider names the menu cycles through.
    pub fn provider_choices(&self) -> Vec<String> {
        if !self.providers.is_empty() {
            return self.providers.clone();
        }
        [
            "anthropic",
            "openai",
            "lmstudio",
            "claude",
            "codex",
            "opencode",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// Models the menu cycles through. Always includes the configured model,
    /// so cycling from a hand-edited value can return to it.
    pub fn model_choices(&self) -> Vec<String> {
        let mut out = self.models.clone();
        if !self.model.is_empty() && !out.contains(&self.model) {
            out.insert(0, self.model.clone());
        }
        out
    }

    /// Context budgets the menu cycles through, in tokens. Always includes
    /// the configured value so cycling is reversible, and 0 (unlimited) so
    /// compaction can be turned off from the menu.
    pub fn context_choices(&self) -> Vec<usize> {
        let mut out = if self.context_choices.is_empty() {
            vec![0, 8_000, 16_000, 32_000, 60_000, 100_000, 180_000]
        } else {
            self.context_choices.clone()
        };
        if !out.contains(&self.max_context_tokens) {
            out.push(self.max_context_tokens);
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    pub fn approval_required(&self, tool: &str) -> bool {
        if self.approval != "propose" {
            return false;
        }
        // kill_session has its own confirmation flow (/confirm-kill).
        if tool == "kill_session" {
            return false;
        }
        !self.auto_approve.iter().any(|t| t == tool)
    }

    pub fn class(&self) -> anyhow::Result<OrchestratorClass> {
        use OrchestratorClass::*;
        Ok(match self.provider.as_str() {
            "anthropic" => Api(ApiProvider::Anthropic),
            "openai" | "lmstudio" => Api(ApiProvider::OpenAi),
            "claude" => Cli("claude"),
            "codex" => Cli("codex"),
            "opencode" => Cli("opencode"),
            "omp" | "oh-my-pi" | "ohmypi" => Cli("oh-my-pi"),
            other => anyhow::bail!("unknown orchestrator provider: {}", other),
        })
    }

    /// CLI class: extra flags implementing `permission_mode` for the given
    /// session-kind name. None when the mode is "default" or the CLI has no
    /// safe auto-approval flag (opencode / oh-my-pi: configure the tool's own
    /// permission settings instead).
    pub fn cli_permission_args(&self, kind: &str) -> anyhow::Result<Option<&'static str>> {
        match self.permission_mode.as_str() {
            "" | "default" => Ok(None),
            "accept-edits" | "acceptEdits" | "auto" => Ok(match kind {
                "claude" => Some("--permission-mode acceptEdits"),
                "codex" => Some("--full-auto"),
                _ => None,
            }),
            other => anyhow::bail!(
                "unknown orchestrator permission_mode '{}' (use \"accept-edits\" or \"default\")",
                other
            ),
        }
    }

    /// Effective skills directory: the configured path (with `~` expansion),
    /// or the default ~/.config/linkshell/skills when it exists
    /// (ensure_agent_files creates it when the orchestrator starts).
    pub fn skills_path(&self) -> Option<std::path::PathBuf> {
        if !self.skills_dir.is_empty() {
            return Some(std::path::PathBuf::from(expand_tilde(&self.skills_dir)));
        }
        let default = config_path()?.parent()?.join("skills");
        default.is_dir().then_some(default)
    }

    /// Effective memory file: the configured path (with `~` expansion), or
    /// the default ~/.config/linkshell/memory.md. Unlike skills_path this
    /// returns the default even before the file exists, so the `remember`
    /// tool and the scaffolding know where to write.
    pub fn memory_path(&self) -> Option<std::path::PathBuf> {
        if !self.memory_file.is_empty() {
            return Some(std::path::PathBuf::from(expand_tilde(&self.memory_file)));
        }
        Some(config_path()?.parent()?.join("memory.md"))
    }

    /// Create the default agent locations so they work out of the box:
    /// the skills directory, and a memory.md seeded with a short template
    /// explaining the contract. Idempotent and best-effort; called when an
    /// orchestrator starts.
    pub fn ensure_agent_files(&self) {
        // Skills: create the directory and drop in the shipped defaults.
        // install_defaults never overwrites an existing file, so a default
        // the user has edited stays edited.
        let skills_dir = if self.skills_dir.is_empty() {
            config_path().and_then(|p| p.parent().map(|d| d.join("skills")))
        } else {
            Some(std::path::PathBuf::from(expand_tilde(&self.skills_dir)))
        };
        if let Some(dir) = skills_dir {
            crate::orchestrator::install_default_skills(&dir);
        }
        if let Some(path) = self.memory_path() {
            if !path.exists() {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(
                    &path,
                    "# Agent memory\n\n\
                     Durable notes the orchestrator carries between sessions. The whole file\n\
                     is injected into its prompt every turn, so keep it concise — prune freely.\n\
                     The agent appends dated bullets below via its `remember` tool.\n",
                );
            }
        }
    }

    /// Effective model id (API class).
    pub fn model_id(&self) -> String {
        if !self.model.is_empty() {
            return self.model.clone();
        }
        match self.provider.as_str() {
            "anthropic" => "claude-opus-4-8".to_string(),
            _ => String::new(),
        }
    }

    /// Effective endpoint (API class).
    pub fn endpoint_url(&self) -> String {
        if !self.endpoint.is_empty() {
            return self.endpoint.clone();
        }
        match self.provider.as_str() {
            "anthropic" => anthropic_base_url(),
            "lmstudio" => "http://localhost:1234/v1".to_string(),
            _ => String::new(),
        }
    }

    /// Effective API key (API class): config value or provider env var.
    pub fn resolve_api_key(&self) -> Option<String> {
        if !self.api_key.is_empty() {
            return Some(self.api_key.clone());
        }
        let var = match self.provider.as_str() {
            "anthropic" => "ANTHROPIC_API_KEY",
            "openai" => "OPENAI_API_KEY",
            _ => return None,
        };
        std::env::var(var).ok().filter(|k| !k.is_empty())
    }

    /// Effective Anthropic credentials (API class). A bearer auth token — from
    /// `auth_token` or ANTHROPIC_AUTH_TOKEN — takes precedence; otherwise the
    /// x-api-key from `api_key` or ANTHROPIC_API_KEY.
    pub fn resolve_anthropic_auth(&self) -> Option<AnthropicAuth> {
        if !self.auth_token.is_empty() {
            return Some(AnthropicAuth::BearerToken(self.auth_token.clone()));
        }
        if !self.api_key.is_empty() {
            return Some(AnthropicAuth::ApiKey(self.api_key.clone()));
        }
        AnthropicAuth::from_env()
    }
}

/// How to authenticate against an Anthropic-compatible endpoint.
pub enum AnthropicAuth {
    /// Standard Anthropic API key, sent as the `x-api-key` header.
    ApiKey(String),
    /// Bearer token, sent as `Authorization: Bearer ...`. Required by some
    /// gateways (e.g. NVIDIA) that front the Anthropic API.
    BearerToken(String),
}

impl AnthropicAuth {
    /// Resolve from env vars: ANTHROPIC_AUTH_TOKEN (preferred) then
    /// ANTHROPIC_API_KEY.
    pub fn from_env() -> Option<AnthropicAuth> {
        if let Some(tok) = std::env::var("ANTHROPIC_AUTH_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
        {
            return Some(AnthropicAuth::BearerToken(tok));
        }
        std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .map(AnthropicAuth::ApiKey)
    }

    /// Attach the appropriate auth header to a request.
    pub fn apply(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            AnthropicAuth::ApiKey(k) => req.header("x-api-key", k),
            AnthropicAuth::BearerToken(t) => req.header("authorization", format!("Bearer {t}")),
        }
    }
}

/// Anthropic base URL: ANTHROPIC_BASE_URL env var or the public default.
pub fn anthropic_base_url() -> String {
    std::env::var("ANTHROPIC_BASE_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "https://api.anthropic.com".to_string())
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
#[serde(default)]
pub struct NotificationsConfig {
    pub enabled: bool,
    pub on_states: Vec<String>,
    pub method: crate::notify::Method,
    pub min_session_age_secs: u64,
    pub debounce_secs: u64,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            on_states: vec!["waiting".into(), "error".into()],
            method: crate::notify::Method::Auto,
            min_session_age_secs: 10,
            debounce_secs: 30,
        }
    }
}

/// A named behavioural preset layered over `[orchestrator]`.
///
/// Personas modulate *autonomy and eagerness*, not correctness: the loop
/// suppressor, the elision stubs and the send_input evidence are
/// unconditional. Every field is optional, and `None` means "inherit from
/// `[orchestrator]`" — an explicit setting there still wins unless the
/// persona overrides it.
///
///   [[personas]]
///   name = "assistant"
///   events = []
///   approval = "propose"
///   allowed_tools = ["list_sessions", "read_output", "use_skill", "remember"]
///   max_tool_iterations = 4
///   tool_dedup_secs = 300
///   note = "You observe and advise. You do not drive sessions."
#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Persona {
    pub name: String,
    pub events: Option<Vec<String>>,
    pub event_cooldown_secs: Option<u64>,
    pub approval: Option<String>,
    pub auto_approve: Option<Vec<String>>,
    pub allowed_tools: Option<Vec<String>>,
    pub max_tool_iterations: Option<usize>,
    pub tool_dedup_secs: Option<u64>,
    pub max_context_tokens: Option<usize>,
    pub event_tail_lines: Option<usize>,
    /// Appended to the system prompt.
    pub note: String,
}

impl Persona {
    /// Layer this persona over a base orchestrator config.
    pub fn apply(&self, base: &OrchestratorConfig) -> OrchestratorConfig {
        let mut cfg = base.clone();
        if let Some(v) = &self.events {
            cfg.events = v.clone();
        }
        if let Some(v) = self.event_cooldown_secs {
            cfg.event_cooldown_secs = v;
        }
        if let Some(v) = &self.approval {
            cfg.approval = v.clone();
        }
        if let Some(v) = &self.auto_approve {
            cfg.auto_approve = v.clone();
        }
        if let Some(v) = &self.allowed_tools {
            cfg.allowed_tools = v.clone();
        }
        if let Some(v) = self.max_tool_iterations {
            cfg.max_tool_iterations = v;
        }
        if let Some(v) = self.tool_dedup_secs {
            cfg.tool_dedup_secs = v;
        }
        if let Some(v) = self.max_context_tokens {
            cfg.max_context_tokens = v;
        }
        if let Some(v) = self.event_tail_lines {
            cfg.event_tail_lines = v;
        }
        cfg.persona_note = self.note.clone();
        cfg.persona = self.name.clone();
        cfg
    }
}

/// The three shipped personas, used when no `[[personas]]` entry matches.
/// Ordered by autonomy: assistant looks, monitor reports, orchestrator acts.
pub fn builtin_personas() -> Vec<Persona> {
    let read_only = vec![
        "list_sessions".to_string(),
        "read_output".to_string(),
        "use_skill".to_string(),
        "remember".to_string(),
    ];
    vec![
        Persona {
            name: "assistant".into(),
            events: Some(Vec::new()),
            approval: Some("propose".into()),
            allowed_tools: Some(read_only.clone()),
            max_tool_iterations: Some(4),
            tool_dedup_secs: Some(300),
            note: "You are a reactive assistant. You answer when spoken to. You can \
                   inspect sessions but cannot drive them; if something needs doing, \
                   say so and let the user do it."
                .into(),
            ..Default::default()
        },
        Persona {
            name: "monitor".into(),
            events: Some(vec!["waiting".into(), "error".into(), "dead".into()]),
            event_cooldown_secs: Some(60),
            approval: Some("propose".into()),
            allowed_tools: None, // full set, but writes are gated by propose
            auto_approve: Some(read_only),
            max_tool_iterations: Some(8),
            tool_dedup_secs: Some(120),
            note: "You watch sessions and report. Investigate freely with read-only \
                   tools; anything that changes a session is proposed for approval \
                   first. Prefer one clear report over a stream of updates."
                .into(),
            ..Default::default()
        },
        Persona {
            name: "orchestrator".into(),
            events: Some(vec![
                "ready".into(),
                "waiting".into(),
                "error".into(),
                "dead".into(),
            ]),
            event_cooldown_secs: Some(15),
            approval: Some("auto".into()),
            max_tool_iterations: Some(12),
            tool_dedup_secs: Some(45),
            note: "You actively route work between sessions. Act without asking for \
                   routine steps. Before repeating an action, check whether the \
                   previous one had an effect; if you cannot tell, say so rather \
                   than trying again."
                .into(),
            ..Default::default()
        },
    ]
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

fn default_trigger() -> String {
    "on_ready".into()
}
fn default_extract() -> String {
    "last_block".into()
}

// ── [theme] ───────────────────────────────────────────────────────────────

/// Palette selection and per-field overrides:
///
///   [theme]
///   base = "dark"          # "classic" | "dark" | "ansi16"
///   accent = "#5fb3d4"     # any field overridable as #rrggbb
///
/// `base` unset auto-detects from `COLORTERM`. See `theme.rs`.
#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, Default)]
#[serde(default)]
pub struct ThemeConfig {
    pub base: Option<String>,
    pub bg: Option<String>,
    pub surface: Option<String>,
    pub chrome: Option<String>,
    pub text: Option<String>,
    pub text_dim: Option<String>,
    pub text_bright: Option<String>,
    pub accent: Option<String>,
    pub warn: Option<String>,
    pub err: Option<String>,
    pub ok: Option<String>,
    pub info: Option<String>,
    pub ctx: Option<String>,
    pub cost: Option<String>,
    pub pipe: Option<String>,
    pub on_accent: Option<String>,
    pub sel_bg: Option<String>,
    pub kind_claude: Option<String>,
    pub kind_codex: Option<String>,
    pub kind_opencode: Option<String>,
    pub kind_ohmypi: Option<String>,
    pub kind_aider: Option<String>,
    pub kind_shell: Option<String>,
    pub kind_custom: Option<String>,
    pub kind_orch: Option<String>,
}

// ── [general] ─────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
#[serde(default)]
pub struct GeneralConfig {
    pub max_ipc_message_bytes: usize,
    pub scroll_buffer_lines: usize,
    pub tick_interval_ms: u64,
    pub ipc_state_override_timeout_secs: u64,
    pub menu_key: String,
    /// Where the status panel lives:
    ///
    /// - "left" (default) — a permanent sidebar beside the output. Costs
    ///   columns instead of rows, and unlike the bottom region it has the
    ///   full terminal height, so it doesn't run out of room at 4 sessions.
    /// - "bottom" — the always-on region below the output.
    /// - "overlay" — not docked; alt-s opens it centered over the output.
    /// - "off" — never shown.
    ///
    /// In "left" and "bottom", alt-s hides and shows the panel at runtime.
    pub status_panel: String,
    /// Columns claimed by the left sidebar. Below `status_panel_width` +
    /// 60 columns of terminal the sidebar collapses to a rail (see
    /// `ui::SIDEBAR_RAIL_COLS`) rather than starving the output pane.
    pub status_panel_width: u16,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            max_ipc_message_bytes: 0,
            scroll_buffer_lines: 2000,
            tick_interval_ms: 100,
            ipc_state_override_timeout_secs: 60,
            menu_key: "ctrl+space".to_string(),
            status_panel: "left".to_string(),
            status_panel_width: 28,
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
    pub opencode: String,
    /// Oh My Pi ships its CLI as `omp`.
    pub ohmypi: String,
    pub aider: String,
    /// Empty string means use $SHELL.
    pub shell: String,
}

impl Default for SessionCommandsConfig {
    fn default() -> Self {
        Self {
            claude: "claude".to_string(),
            codex: "codex".to_string(),
            opencode: "opencode".to_string(),
            ohmypi: "omp".to_string(),
            aider: "aider".to_string(),
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
        // cache_write is the 5-minute-TTL rate (1.25x input); the log watcher
        // prices 1-hour cache writes at 1.6x this (i.e. 2x input), matching
        // Claude Code's own /cost accounting.
        claude.insert(
            "claude-fable".into(),
            ModelRate {
                input: 10.00,
                cache_write: 12.50,
                cache_read: 1.00,
                output: 50.00,
            },
        );
        claude.insert(
            "claude-mythos".into(),
            ModelRate {
                input: 10.00,
                cache_write: 12.50,
                cache_read: 1.00,
                output: 50.00,
            },
        );
        // Opus 4.5 and later are $5/$25; older opus (4.0/4.1/3) stays at
        // $15/$75 via the shorter "claude-opus" prefix below.
        for m in [
            "claude-opus-4-5",
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-4-8",
        ] {
            claude.insert(
                m.into(),
                ModelRate {
                    input: 5.00,
                    cache_write: 6.25,
                    cache_read: 0.50,
                    output: 25.00,
                },
            );
        }
        claude.insert(
            "claude-haiku-4".into(),
            ModelRate {
                input: 1.00,
                cache_write: 1.25,
                cache_read: 0.10,
                output: 5.00,
            },
        );
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
/// Expand a leading `~`/`~/` to $HOME; other paths pass through unchanged.
fn expand_tilde(path: &str) -> String {
    if path == "~" || path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{}{}", home, &path[1..]);
        }
    }
    path.to_string()
}

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
        || !profile
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!("profile name may contain only letters, numbers, '-' and '_'");
    }
    #[derive(serde::Serialize)]
    struct ProfilesFile<'a> {
        profiles: [&'a Profile; 1],
    }
    let base = config_path().ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    let dir = base
        .parent()
        .expect("config path has parent")
        .join("profiles.d");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.toml", profile.name));
    std::fs::write(
        &path,
        toml::to_string_pretty(&ProfilesFile {
            profiles: [profile],
        })?,
    )?;
    Ok(path)
}

pub fn parse(content: &str) -> anyhow::Result<Config> {
    let mut cfg: Config = toml::from_str(content)?;
    validate_profiles(&cfg)?;
    cfg.derive_planning_backends();
    Ok(cfg)
}

impl Config {
    /// Offer the model endpoints already configured elsewhere as planning
    /// backends, so the picker is usable without a second copy of the same
    /// endpoint under `[planning.backends.*]`. Explicit entries always win —
    /// a derived one is a starting point, not an override.
    pub fn derive_planning_backends(&mut self) {
        let mut derived: HashMap<String, crate::planning::Backend> = HashMap::new();

        for (name, agent) in &self.agents {
            derived.insert(
                name.clone(),
                crate::planning::Backend {
                    name: name.clone(),
                    // `[agents.*]` is defined as OpenAI-compatible.
                    provider: "openai".to_string(),
                    endpoint: agent.endpoint.clone(),
                    model: agent.model.clone(),
                    api_key: agent.api_key.clone().unwrap_or_default(),
                    ..Default::default()
                },
            );
        }

        // Only the API-class orchestrator has an endpoint of its own; the CLI
        // class is a subprocess with no HTTP surface to borrow.
        let orch = &self.orchestrator;
        if matches!(orch.class(), Ok(OrchestratorClass::Api(_))) && !orch.model.is_empty() {
            let name = if orch.name.is_empty() {
                "orchestrator".to_string()
            } else {
                orch.name.clone()
            };
            let mut backend = crate::planning::Backend {
                name: name.clone(),
                provider: orch.provider.clone(),
                // Resolved, not raw: `provider = "lmstudio"` with no endpoint
                // means localhost:1234, and the planning wire has no notion of
                // an lmstudio default of its own.
                endpoint: orch.endpoint_url(),
                model: orch.model.clone(),
                api_key: orch.api_key.clone(),
                auth_token: orch.auth_token.clone(),
                ..Default::default()
            };
            if orch.max_context_tokens > 0 {
                backend.max_context_tokens = orch.max_context_tokens;
            }
            if orch.max_tool_iterations > 0 {
                backend.max_tool_iterations = orch.max_tool_iterations;
            }
            derived.entry(name).or_insert(backend);
        }

        self.planning.derived = derived;
    }
}

pub fn save(config: &Config) -> anyhow::Result<()> {
    let path = config_path().ok_or_else(|| anyhow::anyhow!("cannot determine config path"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = toml::to_string_pretty(config)?;
    std::fs::write(&path, content)?;
    Ok(())
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
            if !matches!(
                session.kind.as_str(),
                "claude" | "codex" | "shell" | "custom"
            ) {
                anyhow::bail!(
                    "profile '{}': unknown session kind '{}'",
                    profile.name,
                    session.kind
                );
            }
            if session.name.is_empty() {
                anyhow::bail!("profile '{}': sessions require a name", profile.name);
            }
            if !session_names.insert(session.name.as_str()) {
                anyhow::bail!(
                    "profile '{}': duplicate session name '{}'",
                    profile.name,
                    session.name
                );
            }
            if session.kind == "custom" {
                if session.command.is_empty() {
                    anyhow::bail!(
                        "profile '{}': custom session '{}' requires command",
                        profile.name,
                        session.name
                    );
                }
                validate_command(&session.command).map_err(anyhow::Error::msg)?;
            }
        }
        for pipe in &profile.pipes {
            if !session_names.contains(pipe.source.as_str())
                || !session_names.contains(pipe.dest.as_str())
            {
                anyhow::bail!(
                    "profile '{}': pipe references undefined session",
                    profile.name
                );
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
    fn orchestrator_provider_classes_resolve_with_sane_defaults() {
        let mut cfg = OrchestratorConfig::default();
        assert!(!cfg.enabled);
        assert!(matches!(
            cfg.class(),
            Ok(OrchestratorClass::Api(ApiProvider::Anthropic))
        ));
        assert_eq!(cfg.model_id(), "claude-opus-4-8");
        assert_eq!(cfg.endpoint_url(), "https://api.anthropic.com");

        cfg.provider = "lmstudio".into();
        assert!(matches!(
            cfg.class(),
            Ok(OrchestratorClass::Api(ApiProvider::OpenAi))
        ));
        assert_eq!(cfg.endpoint_url(), "http://localhost:1234/v1");

        for (provider, kind) in [
            ("claude", "claude"),
            ("codex", "codex"),
            ("opencode", "opencode"),
            ("omp", "oh-my-pi"),
        ] {
            cfg.provider = provider.into();
            match cfg.class() {
                Ok(OrchestratorClass::Cli(k)) => assert_eq!(k, kind),
                _ => panic!("{} should be CLI class", provider),
            }
        }

        cfg.provider = "gpt5".into();
        assert!(cfg.class().is_err());

        // Explicit values win over defaults
        cfg.provider = "anthropic".into();
        cfg.model = "claude-haiku-4-5-20251001".into();
        cfg.endpoint = "http://localhost:9999".into();
        assert_eq!(cfg.model_id(), "claude-haiku-4-5-20251001");
        assert_eq!(cfg.endpoint_url(), "http://localhost:9999");
    }

    #[test]
    fn orchestrator_permission_mode_maps_safe_flags_per_cli() {
        let mut cfg = OrchestratorConfig::default();
        // Default reduces prompting via each CLI's safe auto-approval flags
        assert_eq!(cfg.permission_mode, "accept-edits");
        assert_eq!(
            cfg.cli_permission_args("claude").unwrap(),
            Some("--permission-mode acceptEdits")
        );
        assert_eq!(
            cfg.cli_permission_args("codex").unwrap(),
            Some("--full-auto")
        );
        // CLIs without a safe flag get none
        assert_eq!(cfg.cli_permission_args("opencode").unwrap(), None);
        assert_eq!(cfg.cli_permission_args("oh-my-pi").unwrap(), None);

        cfg.permission_mode = "default".into();
        assert_eq!(cfg.cli_permission_args("claude").unwrap(), None);

        // Anything unrecognized (including bypass-style modes) is rejected
        cfg.permission_mode = "bypass".into();
        assert!(cfg.cli_permission_args("claude").is_err());
    }

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
        assert_eq!(
            decoded.profiles[0].sessions[0].group.as_deref(),
            Some("council")
        );
        assert_eq!(decoded.profiles[0].sessions[1].command, "qwen-agent");
        assert_eq!(decoded.profiles[0].pipes[0].trigger, "manual");
        assert_eq!(decoded.profiles[0].pipes[0].extract, "summarize:500");
        assert_eq!(
            decoded.profiles[0].pipes[0].prefix.as_deref(),
            Some("Review this:")
        );
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
        assert!(cfg.notifications.enabled);
        assert_eq!(cfg.notifications.on_states, vec!["waiting", "error"]);
    }

    #[test]
    fn notifications_config_parses_method_states_age_and_debounce() {
        let cfg = parse(
            r#"
[notifications]
enabled = false
on_states = ["error"]
method = "bell"
min_session_age_secs = 5
debounce_secs = 12
"#,
        )
        .unwrap();
        assert!(!cfg.notifications.enabled);
        assert_eq!(cfg.notifications.method, crate::notify::Method::Bell);
        assert_eq!(cfg.notifications.min_session_age_secs, 5);
        assert_eq!(cfg.notifications.debounce_secs, 12);
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
    fn planning_backends_are_derived_from_agents_and_the_orchestrator() {
        let cfg = parse(
            r#"
[orchestrator]
name = "agent"
provider = "lmstudio"
model = "qwen3.6-27b"
max_context_tokens = 131072

[agents.qwen]
endpoint = "http://localhost:8080/v1"
model = "qwen3.6-8b"
"#,
        )
        .unwrap();
        assert_eq!(cfg.planning.backend_names(), vec!["agent", "qwen"]);
        let agent = cfg.planning.backend("agent").unwrap();
        // An lmstudio orchestrator with no explicit endpoint must not land on
        // api.openai.com.
        assert_eq!(agent.endpoint, "http://localhost:1234/v1");
        assert_eq!(agent.max_context_tokens, 131072);
        assert_eq!(cfg.planning.backend("qwen").unwrap().provider, "openai");
        // The picker opens on one of them rather than on nothing.
        assert!(cfg.planning.default_backend().is_some());
    }

    #[test]
    fn explicit_planning_backends_shadow_derived_ones() {
        let cfg = parse(
            r#"
[orchestrator]
name = "agent"
provider = "lmstudio"
model = "from-orchestrator"

[planning.backends.agent]
provider = "anthropic"
model = "from-planning"
"#,
        )
        .unwrap();
        assert_eq!(cfg.planning.backend_names(), vec!["agent"]);
        assert_eq!(
            cfg.planning.backend("agent").unwrap().model,
            "from-planning"
        );
    }

    #[test]
    fn derived_backends_are_not_written_back_to_disk() {
        let cfg = parse("[agents.qwen]\nendpoint = \"http://x/v1\"\nmodel = \"m\"\n").unwrap();
        assert!(!cfg.planning.derived.is_empty());
        let round_tripped = parse(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        // Derived again from [agents.*], never persisted as [planning.backends.*].
        assert!(round_tripped.planning.backends.is_empty());
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

    #[test]
    fn orchestrator_parses_auth_token_and_base_url() {
        let toml = r#"
[orchestrator]
provider = "anthropic"
endpoint = "https://integrate.api.nvidia.com"
auth_token = "nvapi-secret"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.orchestrator.auth_token, "nvapi-secret");
        assert_eq!(
            cfg.orchestrator.endpoint_url(),
            "https://integrate.api.nvidia.com"
        );
        match cfg.orchestrator.resolve_anthropic_auth() {
            Some(AnthropicAuth::BearerToken(t)) => assert_eq!(t, "nvapi-secret"),
            _ => panic!("expected bearer token auth"),
        }
    }

    #[test]
    fn anthropic_auth_prefers_token_over_api_key() {
        let cfg = OrchestratorConfig {
            api_key: "sk-key".into(),
            auth_token: "tok".into(),
            ..Default::default()
        };
        match cfg.resolve_anthropic_auth() {
            Some(AnthropicAuth::BearerToken(t)) => assert_eq!(t, "tok"),
            _ => panic!("auth_token should take precedence over api_key"),
        }

        let cfg = OrchestratorConfig {
            api_key: "sk-key".into(),
            ..Default::default()
        };
        match cfg.resolve_anthropic_auth() {
            Some(AnthropicAuth::ApiKey(k)) => assert_eq!(k, "sk-key"),
            _ => panic!("expected x-api-key auth"),
        }
    }
}
