use std::collections::HashMap;
use serde::de::{self, Deserializer, Visitor};
use std::fmt;

use crate::pipe::{ExtractMode, PipeTrigger};
use crate::session::{Session, SessionState};

// ── Config types ──────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct CouncilConfig {
    pub council: CouncilMeta,
    pub agent: Vec<AgentSpec>,
    pub route: Vec<RouteSpec>,
}

#[derive(serde::Deserialize)]
pub struct CouncilMeta {
    pub name: String,
    pub task: String,
    #[serde(default = "default_one")]
    pub max_rounds: u32,
    #[serde(default)]
    pub done_signal: Option<String>,
}

fn default_one() -> u32 {
    1
}

fn default_on_ready() -> String {
    "ready".to_string()
}

#[derive(serde::Deserialize)]
pub struct AgentSpec {
    pub name: String,
    pub kind: String, // claude | codex | shell | custom cmd
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct RouteSpec {
    #[serde(deserialize_with = "one_or_many")]
    pub from: Vec<String>,
    #[serde(deserialize_with = "one_or_many")]
    pub to: Vec<String>,
    #[serde(default = "default_on_ready")]
    pub on: String,
    #[serde(default)]
    pub join: JoinMode,
    #[serde(default)]
    pub extract: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub unless_signal: Option<String>,
}

#[derive(serde::Deserialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum JoinMode {
    #[default]
    Any,
    All,
}

// ── one_or_many deserializer ──────────────────────────────────────────────────

struct OneOrManyVisitor;

impl<'de> Visitor<'de> for OneOrManyVisitor {
    type Value = Vec<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a string or a list of strings")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(vec![value.to_string()])
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
        Ok(vec![value])
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(val) = seq.next_element::<String>()? {
            out.push(val);
        }
        Ok(out)
    }
}

fn one_or_many<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    d.deserialize_any(OneOrManyVisitor)
}

// ── Runtime types ─────────────────────────────────────────────────────────────

struct JoinRoute {
    from: Vec<usize>,
    to: Vec<usize>,
    trigger: PipeTrigger,
    extract: ExtractMode,
    join: JoinMode,
    prefix: Option<String>,
    unless_signal: Option<String>,
}

pub struct CouncilRouter {
    pub group: String,
    pub max_rounds: u32,
    pub round: u32,
    pub done_signal: Option<String>,
    pub complete: bool,
    routes: Vec<JoinRoute>,
    /// route_idx -> (source_id -> captured payload) accumulated this cycle
    pending: HashMap<usize, HashMap<usize, String>>,
}

impl CouncilRouter {
    pub fn new(meta: &CouncilMeta, group: &str) -> Self {
        Self {
            group: group.to_string(),
            max_rounds: meta.max_rounds,
            round: 0,
            done_signal: meta.done_signal.clone(),
            complete: false,
            routes: Vec::new(),
            pending: HashMap::new(),
        }
    }

    pub fn add_route(&mut self, from: Vec<usize>, to: Vec<usize>, spec: &RouteSpec) {
        let trigger = match spec.on.as_str() {
            "waiting" | "on_waiting" => PipeTrigger::OnWaiting,
            "manual" => PipeTrigger::Manual,
            _ => PipeTrigger::OnReady,
        };
        let extract = parse_extract(spec.extract.as_deref().unwrap_or("last-block"));
        self.routes.push(JoinRoute {
            from,
            to,
            trigger,
            extract,
            join: spec.join,
            prefix: spec.prefix.clone(),
            unless_signal: spec.unless_signal.clone(),
        });
    }

    /// Called from App whenever a session changes state. Returns relays to send.
    pub fn on_state(
        &mut self,
        sessions: &[Session],
        sid: usize,
        state: &SessionState,
    ) -> Vec<(usize, String)> {
        if self.complete {
            return vec![];
        }
        let mut out = Vec::new();

        for (idx, route) in self.routes.iter().enumerate() {
            let fires = matches!(
                (route.trigger, state),
                (PipeTrigger::OnReady, SessionState::Ready)
                    | (PipeTrigger::OnWaiting, SessionState::Waiting)
            );
            if !fires || !route.from.contains(&sid) {
                continue;
            }

            // termination: did this source emit the done signal?
            if let Some(sig) = &route.unless_signal {
                if session_recent_text(sessions, sid).contains(sig.as_str()) {
                    self.complete = true;
                    return out;
                }
            }

            let Some(payload) = crate::pipe::extract_from_session(sessions, sid, &route.extract)
            else {
                continue;
            };

            match route.join {
                JoinMode::Any => {
                    for &d in &route.to {
                        out.push((d, decorate(&route.prefix, &payload)));
                    }
                }
                JoinMode::All => {
                    let slot = self.pending.entry(idx).or_default();
                    slot.insert(sid, payload);
                    if route.from.iter().all(|s| slot.contains_key(s)) {
                        let combined = route
                            .from
                            .iter()
                            .filter_map(|s| {
                                slot.get(s)
                                    .map(|p| format!("[{}]\n{}", name_of(sessions, *s), p))
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        for &d in &route.to {
                            out.push((d, decorate(&route.prefix, &combined)));
                        }
                        slot.clear();
                        self.round += 1;
                        if self.round >= self.max_rounds {
                            self.complete = true;
                        }
                    }
                }
            }
        }
        out
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn parse_extract(s: &str) -> ExtractMode {
    if s == "diff" {
        ExtractMode::Diff
    } else if let Some(n) = s.strip_prefix("last-n=") {
        ExtractMode::LastN(n.parse().unwrap_or(20))
    } else if let Some(n) = s.strip_prefix("summarize=") {
        ExtractMode::Summarize(n.parse().unwrap_or(150))
    } else {
        ExtractMode::LastBlock
    }
}

fn decorate(prefix: &Option<String>, payload: &str) -> String {
    match prefix {
        Some(p) => format!("{}\n{}", p, payload),
        None => payload.to_string(),
    }
}

fn name_of(sessions: &[Session], sid: usize) -> &str {
    sessions
        .iter()
        .find(|s| s.id == sid)
        .map(|s| s.name.as_str())
        .unwrap_or("unknown")
}

fn session_recent_text(sessions: &[Session], sid: usize) -> String {
    sessions
        .iter()
        .find(|s| s.id == sid)
        .map(|s| {
            s.output_lines
                .iter()
                .rev()
                .take(50)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}
