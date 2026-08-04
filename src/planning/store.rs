//! Persistence for planning threads and committed plans.
//!
//! Threads are global — discoverable from anywhere, listed in one place —
//! while the directory a thread was grounded in is a *property* of the
//! thread, recorded once and reused on reopen. A thread must never silently
//! re-ground against whatever directory it happened to be opened from; that
//! would quietly invalidate every file citation in it.
//!
//! Storage is markdown plus a sidecar JSON:
//!
//! ```text
//! ~/.local/share/linkshell/planning/
//!   threads/<id>.md     conversation body — readable, greppable, editable
//!   threads/<id>.json   metadata: per-message provider/model, file reads
//!   plans/<id>/001.md   committed plan revisions (never overwritten)
//!   plans/<id>/001.json provenance for each revision
//! ```
//!
//! The markdown is the source of truth for message *text*, so fixing a bad
//! turn by editing the file in $EDITOR works. The JSON is the source of truth
//! for metadata, which has no natural home in markdown — per-message
//! front-matter gets ugly fast, and HTML comments make the readable file less
//! readable. When the two disagree (a hand edit added or removed a section),
//! the markdown wins and the missing metadata degrades to "unknown" rather
//! than the load failing.
//!
//! Tool results are deliberately *not* persisted. A thread records that a
//! file was read, with its hash and mtime, and re-materializes contents on
//! demand for the live request. That keeps threads small across backends with
//! wildly different context windows, and means a reopened thread reads the
//! current file rather than a stale snapshot.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::tools::ReadRecord;

/// Who produced a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }

    fn parse(s: &str) -> Option<Role> {
        match s.trim() {
            "user" => Some(Role::User),
            "assistant" => Some(Role::Assistant),
            _ => None,
        }
    }
}

/// One turn in a planning thread.
///
/// `backend`/`model` are recorded per message, not per thread: switching from
/// a local 27B to a frontier model mid-thread is expected, and you need to
/// see where the seam is to judge whether earlier turns deserve a re-run.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub text: String,
    /// Configured backend name (`[planning.backends]` key) that produced this
    /// message. Empty for user turns and for hand-edited additions.
    pub backend: String,
    /// Provider family: "anthropic", "openai", "lmstudio", "llamacpp".
    pub provider: String,
    /// Concrete model id as sent to the endpoint.
    pub model: String,
    /// Unix seconds.
    pub at: u64,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Message {
        Message {
            role: Role::User,
            text: text.into(),
            backend: String::new(),
            provider: String::new(),
            model: String::new(),
            at: now_secs(),
        }
    }

    pub fn assistant(text: impl Into<String>, backend: &super::Backend) -> Message {
        Message {
            role: Role::Assistant,
            text: text.into(),
            backend: backend.name.clone(),
            provider: backend.provider.clone(),
            model: backend.model.clone(),
            at: now_secs(),
        }
    }

    /// Short "who said this" label for the transcript gutter.
    pub fn attribution(&self) -> String {
        match self.role {
            Role::User => "you".to_string(),
            Role::Assistant => {
                if self.model.is_empty() {
                    self.backend.clone()
                } else {
                    format!("{} · {}", self.backend, self.model)
                }
            }
        }
    }
}

/// A planning conversation.
#[derive(Debug, Clone)]
pub struct Thread {
    pub id: String,
    pub title: String,
    /// Canonical scope root. Pinned at creation; tools resolve against it.
    pub root: PathBuf,
    pub created: u64,
    pub updated: u64,
    pub messages: Vec<Message>,
    /// Every file the thread has read, latest record per path.
    pub reads: HashMap<String, ReadRecord>,
    /// Number of plan revisions committed from this thread.
    pub revisions: usize,
    /// Decisions pinned out of the conversation, newest last. What you scroll
    /// back for is usually "what did we decide about X", and a short list is
    /// cheaper to scan than the transcript.
    pub decisions: Vec<String>,
}

impl Thread {
    pub fn new(title: &str, root: PathBuf) -> Thread {
        let at = now_secs();
        Thread {
            id: new_id(),
            title: if title.trim().is_empty() {
                "untitled".to_string()
            } else {
                title.trim().to_string()
            },
            root,
            created: at,
            updated: at,
            messages: Vec::new(),
            reads: HashMap::new(),
            revisions: 0,
            decisions: Vec::new(),
        }
    }

    /// Files whose content has changed since the thread read them.
    ///
    /// A plan grounded in a file read three days ago may be grounded in
    /// fiction. This is what makes handoff to an implementation session
    /// trustworthy: you can tell whether the brief still describes the repo.
    pub fn stale_reads(&self) -> Vec<String> {
        let mut stale: Vec<String> = self
            .reads
            .iter()
            .filter(|(rel, rec)| {
                let path = self.root.join(rel);
                match fs::read(&path) {
                    Ok(bytes) => super::tools::content_hash(&bytes) != rec.hash,
                    // A file that vanished is at least as stale as one that changed.
                    Err(_) => true,
                }
            })
            .map(|(rel, _)| rel.clone())
            .collect();
        stale.sort();
        stale
    }

    pub fn record_read(&mut self, rec: ReadRecord) {
        self.reads.insert(rec.rel.clone(), rec);
    }
}

// ── Paths ─────────────────────────────────────────────────────────────────

/// Base directory for planning state: `$XDG_DATA_HOME/linkshell/planning`,
/// falling back to `~/.local/share/linkshell/planning`.
pub fn base_dir() -> anyhow::Result<PathBuf> {
    let data = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local").join("share"))
        })
        .ok_or_else(|| anyhow::anyhow!("neither XDG_DATA_HOME nor HOME is set"))?;
    Ok(data.join("linkshell").join("planning"))
}

fn threads_dir() -> anyhow::Result<PathBuf> {
    Ok(base_dir()?.join("threads"))
}

/// Directory holding committed plan revisions for a thread.
pub fn plans_dir(thread_id: &str) -> anyhow::Result<PathBuf> {
    Ok(base_dir()?.join("plans").join(thread_id))
}

fn new_id() -> String {
    // Timestamp prefix keeps the threads directory sorted by age; the
    // nanosecond tail disambiguates threads created in the same second.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{:010}-{:06}",
        now.as_secs(),
        now.subsec_nanos() % 1_000_000
    )
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── Serialization ─────────────────────────────────────────────────────────

/// Markdown section header separating messages. Chosen so that a message body
/// containing its own `## ` headings does not confuse the parser: only a
/// heading matching this exact shape starts a new message.
const ROLE_PREFIX: &str = "## ";

fn render_markdown(thread: &Thread) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", thread.title));
    out.push_str(&format!(
        "<!-- linkshell planning thread {} -->\n",
        thread.id
    ));
    out.push_str(&format!("<!-- root: {} -->\n\n", thread.root.display()));
    for m in &thread.messages {
        out.push_str(&format!("{}{}\n\n", ROLE_PREFIX, m.role.as_str()));
        out.push_str(m.text.trim_end());
        out.push_str("\n\n");
    }
    out
}

/// Parse the markdown body back into (title, message role/text pairs).
fn parse_markdown(text: &str) -> (String, Vec<(Role, String)>) {
    let mut title = String::new();
    let mut messages: Vec<(Role, String)> = Vec::new();
    let mut current: Option<(Role, String)> = None;

    for line in text.lines() {
        if title.is_empty() && line.starts_with("# ") && current.is_none() {
            title = line[2..].trim().to_string();
            continue;
        }
        // A heading only starts a new message if it names a known role;
        // `## Design notes` inside an assistant turn stays part of the body.
        if let Some(rest) = line.strip_prefix(ROLE_PREFIX) {
            if let Some(role) = Role::parse(rest) {
                if let Some((r, body)) = current.take() {
                    messages.push((r, body.trim().to_string()));
                }
                current = Some((role, String::new()));
                continue;
            }
        }
        if let Some((_, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some((r, body)) = current.take() {
        messages.push((r, body.trim().to_string()));
    }
    (title, messages)
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct MessageMeta {
    #[serde(default)]
    backend: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    at: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ReadMeta {
    hash: u64,
    #[serde(default)]
    mtime: Option<u64>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Sidecar {
    id: String,
    root: String,
    created: u64,
    updated: u64,
    #[serde(default)]
    revisions: usize,
    #[serde(default)]
    decisions: Vec<String>,
    #[serde(default)]
    messages: Vec<MessageMeta>,
    #[serde(default)]
    reads: HashMap<String, ReadMeta>,
}

/// Write a thread to disk. Both files are written via a temp-and-rename so a
/// crash mid-write cannot leave a half-parsed thread behind.
pub fn save(thread: &Thread) -> anyhow::Result<()> {
    let dir = threads_dir()?;
    fs::create_dir_all(&dir)?;

    let sidecar = Sidecar {
        id: thread.id.clone(),
        root: thread.root.to_string_lossy().to_string(),
        created: thread.created,
        updated: thread.updated,
        revisions: thread.revisions,
        decisions: thread.decisions.clone(),
        messages: thread
            .messages
            .iter()
            .map(|m| MessageMeta {
                backend: m.backend.clone(),
                provider: m.provider.clone(),
                model: m.model.clone(),
                at: m.at,
            })
            .collect(),
        reads: thread
            .reads
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    ReadMeta {
                        hash: v.hash,
                        mtime: v.mtime,
                    },
                )
            })
            .collect(),
    };

    write_atomic(
        &dir.join(format!("{}.md", thread.id)),
        &render_markdown(thread),
    )?;
    write_atomic(
        &dir.join(format!("{}.json", thread.id)),
        &serde_json::to_string_pretty(&sidecar)?,
    )?;
    Ok(())
}

fn write_atomic(path: &Path, content: &str) -> anyhow::Result<()> {
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default()
    ));
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Load one thread by id.
pub fn load(id: &str) -> anyhow::Result<Thread> {
    let dir = threads_dir()?;
    let md = fs::read_to_string(dir.join(format!("{}.md", id)))
        .map_err(|e| anyhow::anyhow!("thread {}: {}", id, e))?;
    let (title, parsed) = parse_markdown(&md);

    let sidecar: Option<Sidecar> = fs::read_to_string(dir.join(format!("{}.json", id)))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());

    // Markdown wins on text and count; metadata is zipped in positionally and
    // degrades to empty when a hand edit changed the message count.
    let metas = sidecar
        .as_ref()
        .map(|s| s.messages.as_slice())
        .unwrap_or(&[]);
    let aligned = metas.len() == parsed.len();
    let messages = parsed
        .into_iter()
        .enumerate()
        .map(|(i, (role, text))| {
            let m = if aligned { Some(&metas[i]) } else { None };
            Message {
                role,
                text,
                backend: m.map(|m| m.backend.clone()).unwrap_or_default(),
                provider: m.map(|m| m.provider.clone()).unwrap_or_default(),
                model: m.map(|m| m.model.clone()).unwrap_or_default(),
                at: m.map(|m| m.at).unwrap_or(0),
            }
        })
        .collect();

    let root = sidecar
        .as_ref()
        .map(|s| PathBuf::from(&s.root))
        .or_else(|| {
            // Fall back to the root comment in the markdown so a thread whose
            // sidecar was lost still knows what it was grounded in.
            md.lines()
                .find_map(|l| l.strip_prefix("<!-- root: "))
                .and_then(|l| l.strip_suffix(" -->"))
                .map(PathBuf::from)
        })
        .ok_or_else(|| anyhow::anyhow!("thread {} has no recorded scope root", id))?;

    Ok(Thread {
        id: id.to_string(),
        title,
        root,
        created: sidecar.as_ref().map(|s| s.created).unwrap_or(0),
        updated: sidecar.as_ref().map(|s| s.updated).unwrap_or(0),
        messages,
        reads: sidecar
            .as_ref()
            .map(|s| {
                s.reads
                    .iter()
                    .map(|(rel, r)| {
                        (
                            rel.clone(),
                            ReadRecord {
                                rel: rel.clone(),
                                hash: r.hash,
                                mtime: r.mtime,
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
        revisions: sidecar.as_ref().map(|s| s.revisions).unwrap_or(0),
        decisions: sidecar
            .as_ref()
            .map(|s| s.decisions.clone())
            .unwrap_or_default(),
    })
}

/// Summary row for the thread list in the left sidebar.
#[derive(Debug, Clone)]
pub struct ThreadSummary {
    pub id: String,
    pub title: String,
    pub root: PathBuf,
    pub updated: u64,
    pub messages: usize,
}

/// All threads, most recently updated first.
pub fn list() -> anyhow::Result<Vec<ThreadSummary>> {
    let dir = threads_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out: Vec<ThreadSummary> = Vec::new();
    for entry in fs::read_dir(&dir)?.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e != "md").unwrap_or(true) {
            continue;
        }
        let id = match path.file_stem().map(|s| s.to_string_lossy().to_string()) {
            Some(s) => s,
            None => continue,
        };
        // A malformed thread should not take the whole list down with it.
        if let Ok(t) = load(&id) {
            out.push(ThreadSummary {
                id: t.id,
                title: t.title,
                root: t.root,
                updated: t.updated,
                messages: t.messages.len(),
            });
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.updated));
    Ok(out)
}

/// Delete a thread and its sidecar. Committed plans are left in place — they
/// are the durable artifact and outlive the conversation that produced them.
pub fn delete(id: &str) -> anyhow::Result<()> {
    let dir = threads_dir()?;
    let _ = fs::remove_file(dir.join(format!("{}.json", id)));
    fs::remove_file(dir.join(format!("{}.md", id)))?;
    Ok(())
}

// ── Committed plans ───────────────────────────────────────────────────────

/// A distilled plan written to linkshell's own store.
#[derive(Debug, Clone)]
pub struct PlanRevision {
    pub path: PathBuf,
    pub revision: usize,
}

/// Write a new plan revision. Revisions are never overwritten: plans get
/// revised, and being able to diff two distillations is most of the value of
/// committing at all.
pub fn write_plan(
    thread: &Thread,
    body: &str,
    distiller: &super::Backend,
) -> anyhow::Result<PlanRevision> {
    let dir = plans_dir(&thread.id)?;
    fs::create_dir_all(&dir)?;
    let revision = thread.revisions + 1;
    let path = dir.join(format!("{:03}.md", revision));

    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", thread.title));
    out.push_str(&format!(
        "<!-- linkshell plan · thread {} · revision {} -->\n",
        thread.id, revision
    ));
    out.push_str(&format!("<!-- root: {} -->\n", thread.root.display()));
    out.push_str(&format!(
        "<!-- distilled by: {} ({}) -->\n\n",
        distiller.model, distiller.name
    ));
    out.push_str(body.trim());
    out.push('\n');
    write_atomic(&path, &out)?;

    let meta = serde_json::json!({
        "thread": thread.id,
        "revision": revision,
        "root": thread.root.to_string_lossy(),
        "distilled_by": {
            "backend": distiller.name,
            "provider": distiller.provider,
            "model": distiller.model,
        },
        "at": now_secs(),
        "source_messages": thread.messages.len(),
        "grounded_in": thread.reads.keys().collect::<Vec<_>>(),
        "stale_at_commit": thread.stale_reads(),
    });
    write_atomic(
        &dir.join(format!("{:03}.json", revision)),
        &serde_json::to_string_pretty(&meta)?,
    )?;

    Ok(PlanRevision { path, revision })
}

/// Path of the most recent committed plan for a thread, if any.
pub fn latest_plan(thread_id: &str) -> Option<PathBuf> {
    let dir = plans_dir(thread_id).ok()?;
    let mut revs: Vec<PathBuf> = fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "md").unwrap_or(false))
        .collect();
    revs.sort();
    revs.pop()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point the store at a scratch directory for the duration of a test.
    fn with_temp_home<T>(f: impl FnOnce() -> T) -> T {
        // Tests touching process env must not interleave.
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "linkshell-store-test-{}-{}",
            std::process::id(),
            now_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", &dir);
        let out = f();
        match prev {
            Some(p) => std::env::set_var("XDG_DATA_HOME", p),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        let _ = fs::remove_dir_all(&dir);
        out
    }

    fn now_nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn backend(name: &str, model: &str) -> super::super::Backend {
        super::super::Backend {
            name: name.to_string(),
            provider: "anthropic".to_string(),
            model: model.to_string(),
            ..Default::default()
        }
    }

    fn sample() -> Thread {
        let mut t = Thread::new("Planning pane", PathBuf::from("/tmp/repo"));
        t.messages.push(Message::user("How does layout work?"));
        t.messages.push(Message::assistant(
            "It is a binary tree.",
            &backend("local", "qwen3-27b"),
        ));
        t.messages.push(Message::user("And panes?"));
        t.messages.push(Message::assistant(
            "Parallel vectors indexed by leaf order.",
            &backend("opus", "claude-opus-4"),
        ));
        t
    }

    #[test]
    fn round_trips_through_markdown_and_sidecar() {
        with_temp_home(|| {
            let t = sample();
            save(&t).unwrap();
            let back = load(&t.id).unwrap();
            assert_eq!(back.title, "Planning pane");
            assert_eq!(back.root, PathBuf::from("/tmp/repo"));
            assert_eq!(back.messages.len(), 4);
            assert_eq!(back.messages[0].text, "How does layout work?");
            assert_eq!(
                back.messages[3].text,
                "Parallel vectors indexed by leaf order."
            );
        });
    }

    #[test]
    fn per_message_model_survives_a_mid_thread_switch() {
        with_temp_home(|| {
            let t = sample();
            save(&t).unwrap();
            let back = load(&t.id).unwrap();
            assert_eq!(back.messages[1].model, "qwen3-27b");
            assert_eq!(back.messages[3].model, "claude-opus-4");
            assert!(back.messages[3].attribution().contains("claude-opus-4"));
        });
    }

    #[test]
    fn hand_edited_markdown_wins_over_stale_metadata() {
        with_temp_home(|| {
            let t = sample();
            save(&t).unwrap();
            // Simulate fixing a turn in $EDITOR, adding a message.
            let dir = threads_dir().unwrap();
            let p = dir.join(format!("{}.md", t.id));
            let mut md = fs::read_to_string(&p).unwrap();
            md.push_str("## user\n\nOne more question.\n\n");
            fs::write(&p, md).unwrap();

            let back = load(&t.id).unwrap();
            assert_eq!(back.messages.len(), 5, "the edit is honored");
            assert_eq!(back.messages[4].text, "One more question.");
            // Counts no longer align, so metadata degrades rather than lying.
            assert_eq!(back.messages[1].model, "");
        });
    }

    #[test]
    fn message_bodies_may_contain_their_own_headings() {
        with_temp_home(|| {
            let mut t = Thread::new("headings", PathBuf::from("/tmp/repo"));
            t.messages.push(Message::assistant(
                "## Design notes\n\nSome detail.\n\n## Risks\n\nMore detail.",
                &backend("local", "qwen"),
            ));
            save(&t).unwrap();
            let back = load(&t.id).unwrap();
            assert_eq!(
                back.messages.len(),
                1,
                "inner headings must not split turns"
            );
            assert!(back.messages[0].text.contains("## Risks"));
        });
    }

    #[test]
    fn thread_list_is_newest_first_and_survives_a_corrupt_entry() {
        with_temp_home(|| {
            let mut a = sample();
            a.title = "older".into();
            a.updated = 1_000;
            save(&a).unwrap();
            let mut b = sample();
            b.title = "newer".into();
            b.updated = 2_000;
            save(&b).unwrap();
            // A junk file in the directory must not take the list down.
            fs::write(threads_dir().unwrap().join("garbage.md"), "not a thread").unwrap();

            let list = list().unwrap();
            let titles: Vec<&str> = list.iter().map(|s| s.title.as_str()).collect();
            assert_eq!(&titles[..2], &["newer", "older"]);
        });
    }

    #[test]
    fn stale_reads_flags_changed_and_missing_files() {
        with_temp_home(|| {
            let root = std::env::temp_dir().join(format!("ls-stale-{}", now_nanos()));
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("a.rs"), "original").unwrap();
            fs::write(root.join("b.rs"), "stable").unwrap();
            let root = root.canonicalize().unwrap();

            let mut t = Thread::new("stale", root.clone());
            for name in ["a.rs", "b.rs"] {
                let bytes = fs::read(root.join(name)).unwrap();
                t.record_read(ReadRecord {
                    rel: name.to_string(),
                    hash: super::super::tools::content_hash(&bytes),
                    mtime: None,
                });
            }
            t.record_read(ReadRecord {
                rel: "gone.rs".to_string(),
                hash: 42,
                mtime: None,
            });
            assert_eq!(t.stale_reads(), vec!["gone.rs".to_string()]);

            fs::write(root.join("a.rs"), "changed under the plan").unwrap();
            assert_eq!(
                t.stale_reads(),
                vec!["a.rs".to_string(), "gone.rs".to_string()]
            );
        });
    }

    #[test]
    fn plan_revisions_accumulate_instead_of_clobbering() {
        with_temp_home(|| {
            let mut t = sample();
            let d = backend("opus", "claude-opus-4");
            let r1 = write_plan(&t, "First cut.", &d).unwrap();
            assert_eq!(r1.revision, 1);
            t.revisions = 1;
            let r2 = write_plan(&t, "Revised after review.", &d).unwrap();
            assert_eq!(r2.revision, 2);
            assert!(r1.path.exists() && r2.path.exists());
            assert!(fs::read_to_string(&r1.path).unwrap().contains("First cut."));
            assert_eq!(latest_plan(&t.id).unwrap(), r2.path);
        });
    }

    #[test]
    fn deleting_a_thread_keeps_its_committed_plans() {
        with_temp_home(|| {
            let t = sample();
            save(&t).unwrap();
            let plan = write_plan(&t, "durable", &backend("opus", "claude-opus-4")).unwrap();
            delete(&t.id).unwrap();
            assert!(load(&t.id).is_err());
            assert!(plan.path.exists(), "plans outlive the conversation");
        });
    }
}
