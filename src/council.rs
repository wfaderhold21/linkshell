// Council integration: `App::launch_council` is the activation point for this entire
// module but has no callers in the binary yet. A config-file loader or CLI flag will
// wire it up once that feature lands; until then everything here is dead from the
// binary's perspective.
#![allow(dead_code)]

use serde::de::{self, Deserializer, Visitor};
use std::collections::HashMap;
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

#[derive(serde::Deserialize, Clone, Copy, PartialEq, Default, Debug)]
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

// ── Config loading ────────────────────────────────────────────────────────────

pub fn load_config_file(path: &str) -> anyhow::Result<CouncilConfig> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {}", path, e))?;
    parse_config(&text)
}

pub fn parse_config(text: &str) -> anyhow::Result<CouncilConfig> {
    let cfg: CouncilConfig =
        toml::from_str(text).map_err(|e| anyhow::anyhow!("invalid council config: {}", e))?;
    if cfg.agent.is_empty() {
        return Err(anyhow::anyhow!(
            "council config defines no [[agent]] entries"
        ));
    }
    if cfg.route.is_empty() {
        return Err(anyhow::anyhow!(
            "council config defines no [[route]] entries"
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for a in &cfg.agent {
        if !seen.insert(a.name.as_str()) {
            return Err(anyhow::anyhow!("duplicate agent name '{}'", a.name));
        }
    }
    Ok(cfg)
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
        // Council-level termination: any member emitting the done_signal ends
        // the council, independent of per-route `unless_signal` markers.
        if let Some(sig) = &self.done_signal {
            if session_recent_text(sessions, sid).contains(sig.as_str()) {
                self.complete = true;
                return vec![];
            }
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

            let payload =
                crate::pipe::extract_from_session(sessions, sid, &route.extract).or_else(|| {
                    // LastBlock only matches ``` fenced output. Agents frequently
                    // answer in plain prose; rather than silently dropping the
                    // turn (which stalls the council), fall back to the tail of
                    // the transcript.
                    if matches!(route.extract, ExtractMode::LastBlock) {
                        crate::pipe::extract_from_session(sessions, sid, &ExtractMode::LastN(40))
                    } else {
                        None
                    }
                });
            let Some(payload) = payload else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, SessionKind, PTY_COLS, PTY_ROWS};

    const SAMPLE: &str = r#"
[council]
name = "review"
task = "Review the diff"
max_rounds = 3
done_signal = "LGTM"

[[agent]]
name = "author"
kind = "claude"
system = "You write code."

[[agent]]
name = "critic"
kind = "codex"

[[route]]
from = "author"
to = ["critic"]

[[route]]
from = ["author", "critic"]
to = "author"
on = "waiting"
join = "all"
extract = "last-n=10"
unless_signal = "LGTM"
"#;

    fn session_with_lines(id: usize, lines: &[&str]) -> Session {
        let mut s = Session::new(
            id,
            format!("s{id}"),
            SessionKind::Shell,
            "/tmp".into(),
            PTY_ROWS,
            PTY_COLS,
            2000,
        );
        for line in lines {
            s.push_output_line((*line).to_string());
        }
        s
    }

    fn route(from: &str, to: &str, on: &str, join: JoinMode) -> RouteSpec {
        RouteSpec {
            from: vec![from.to_string()],
            to: vec![to.to_string()],
            on: on.to_string(),
            join,
            extract: None,
            prefix: None,
            unless_signal: None,
        }
    }

    #[test]
    fn shipped_example_config_parses() {
        let cfg = parse_config(include_str!("../examples/council.toml")).unwrap();
        assert_eq!(cfg.council.name, "review");
        assert_eq!(cfg.agent.len(), 2);
        assert_eq!(cfg.route.len(), 2);
        assert_eq!(cfg.route[1].unless_signal.as_deref(), Some("LGTM"));
    }

    #[test]
    fn parse_config_accepts_one_or_many_and_defaults() {
        let cfg = parse_config(SAMPLE).unwrap();
        assert_eq!(cfg.council.max_rounds, 3);
        assert_eq!(cfg.council.done_signal.as_deref(), Some("LGTM"));
        assert_eq!(cfg.agent.len(), 2);
        // string form → single-element vec
        assert_eq!(cfg.route[0].from, vec!["author"]);
        assert_eq!(cfg.route[0].on, "ready"); // default
        assert_eq!(cfg.route[0].join, JoinMode::Any); // default
                                                      // list form preserved
        assert_eq!(cfg.route[1].from, vec!["author", "critic"]);
        assert_eq!(cfg.route[1].join, JoinMode::All);
    }

    #[test]
    fn parse_config_rejects_empty_and_duplicate_definitions() {
        assert!(parse_config("[council]\nname='x'\ntask='y'").is_err());
        let dup = SAMPLE.replace("name = \"critic\"", "name = \"author\"");
        assert!(parse_config(&dup).is_err());
    }

    #[test]
    fn parse_extract_handles_all_modes_with_fallback() {
        assert!(matches!(parse_extract("diff"), ExtractMode::Diff));
        assert!(matches!(parse_extract("last-n=7"), ExtractMode::LastN(7)));
        assert!(matches!(
            parse_extract("last-n=bad"),
            ExtractMode::LastN(20)
        ));
        assert!(matches!(
            parse_extract("summarize=99"),
            ExtractMode::Summarize(99)
        ));
        assert!(matches!(
            parse_extract("last-block"),
            ExtractMode::LastBlock
        ));
        assert!(matches!(parse_extract("nonsense"), ExtractMode::LastBlock));
    }

    #[test]
    fn any_route_relays_to_all_destinations_on_ready() {
        let meta = CouncilMeta {
            name: "c".into(),
            task: "t".into(),
            max_rounds: 5,
            done_signal: None,
        };
        let mut router = CouncilRouter::new(&meta, "c");
        let mut spec = route("a", "b", "ready", JoinMode::Any);
        spec.extract = Some("last-n=10".into()); // default last-block needs ``` fences
        router.add_route(vec![1], vec![2, 3], &spec);

        let sessions = vec![
            session_with_lines(1, &["hello", "world"]),
            session_with_lines(2, &[]),
            session_with_lines(3, &[]),
        ];
        let out = router.on_state(&sessions, 1, &SessionState::Ready);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, 2);
        assert_eq!(out[1].0, 3);
        // Waiting state must not fire an on-ready route.
        assert!(router
            .on_state(&sessions, 1, &SessionState::Waiting)
            .is_empty());
        // Non-member session must not fire.
        assert!(router
            .on_state(&sessions, 2, &SessionState::Ready)
            .is_empty());
    }

    #[test]
    fn all_join_waits_for_every_source_then_completes_at_max_rounds() {
        let meta = CouncilMeta {
            name: "c".into(),
            task: "t".into(),
            max_rounds: 1,
            done_signal: None,
        };
        let mut router = CouncilRouter::new(&meta, "c");
        let spec = RouteSpec {
            from: vec!["a".into(), "b".into()],
            to: vec!["c".into()],
            on: "ready".into(),
            join: JoinMode::All,
            extract: Some("last-n=10".into()),
            prefix: Some("combined:".into()),
            unless_signal: None,
        };
        router.add_route(vec![1, 2], vec![3], &spec);

        let sessions = vec![
            session_with_lines(1, &["alpha output"]),
            session_with_lines(2, &["beta output"]),
            session_with_lines(3, &[]),
        ];
        // First source ready → held, nothing relayed yet.
        assert!(router
            .on_state(&sessions, 1, &SessionState::Ready)
            .is_empty());
        // Second source ready → combined relay fires.
        let out = router.on_state(&sessions, 2, &SessionState::Ready);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 3);
        assert!(out[0].1.starts_with("combined:"));
        assert!(out[0].1.contains("[s1]"));
        assert!(out[0].1.contains("[s2]"));
        // max_rounds = 1 → council is now complete and inert.
        assert!(router.complete);
        assert!(router
            .on_state(&sessions, 1, &SessionState::Ready)
            .is_empty());
    }

    #[test]
    fn unless_signal_terminates_the_council() {
        let meta = CouncilMeta {
            name: "c".into(),
            task: "t".into(),
            max_rounds: 10,
            done_signal: Some("LGTM".into()),
        };
        let mut router = CouncilRouter::new(&meta, "c");
        let mut spec = route("a", "b", "ready", JoinMode::Any);
        spec.extract = Some("last-n=10".into());
        spec.unless_signal = Some("LGTM".into());
        router.add_route(vec![1], vec![2], &spec);

        let sessions = vec![
            session_with_lines(1, &["looks good", "LGTM"]),
            session_with_lines(2, &[]),
        ];
        let out = router.on_state(&sessions, 1, &SessionState::Ready);
        assert!(out.is_empty());
        assert!(router.complete);
    }
}
