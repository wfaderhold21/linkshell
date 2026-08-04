//! Read-only filesystem tools for planning threads.
//!
//! Planning agents ground their plans in the repository, so they need to read
//! it — but a planning thread must never mutate the workspace. That guarantee
//! is structural rather than prompted: this module defines the *entire* tool
//! surface a planning thread has, and there is no write, edit, or shell
//! primitive in it. A model that asks for one gets "no such tool".
//!
//! `bash` is deliberately absent. It is a write primitive wearing a search
//! costume: `rg pattern` and `sh -c 'rm -rf'` travel the same channel. `grep`
//! here is a real implementation over `walkdir`-style recursion with a
//! compiled `regex`, never a shell string.
//!
//! Every path the model supplies is resolved against the thread's pinned
//! scope root and rejected if it escapes — including via `..`, an absolute
//! path, or a symlink pointing outside. Without that check a plan asking
//! about "your config" walks straight into `~/.ssh` or the linkshell config
//! holding API keys.

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// Largest single file read returned to the model, in bytes. Reads past this
/// are truncated with a marker; the model can ask for a later offset.
const MAX_READ_BYTES: usize = 64 * 1024;
/// Largest file `grep` will scan. Bigger files are almost always data.
const MAX_GREP_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Cap on grep hits returned in one call.
const MAX_GREP_HITS: usize = 200;
/// Cap on entries returned by `list_dir`.
const MAX_LIST_ENTRIES: usize = 500;
/// Directory names never descended into during recursive search.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    "dist",
    "build",
    ".next",
    ".cargo",
];

/// A resolved, in-scope path plus the identity of what was read, so the
/// thread can tell later whether its grounding has gone stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadRecord {
    /// Path relative to the scope root, as stored in the thread.
    pub rel: String,
    /// Content hash at read time (FNV-1a, 64-bit — this is a change detector,
    /// not a security primitive).
    pub hash: u64,
    /// Modification time as seconds since the Unix epoch, when available.
    pub mtime: Option<u64>,
}

/// Cheap, dependency-free content hash. Used only to detect that a file
/// changed under a plan; collisions here cost a spurious "unchanged", not a
/// security boundary.
pub fn content_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

fn mtime_secs(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// Lexically normalize a path, resolving `.` and `..` without touching the
/// filesystem. Used *before* canonicalization so that a path escaping the
/// root is rejected even when the target does not exist (canonicalize fails
/// on missing files, which would otherwise leave a hole).
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve a model-supplied path against the scope root.
///
/// Rejects absolute paths, `..` escapes, and symlinks whose target lands
/// outside the root. `root` must already be canonical (see
/// [`canonical_root`]).
pub fn resolve_in_root(root: &Path, input: &str) -> anyhow::Result<PathBuf> {
    let raw = input.trim();
    if raw.is_empty() {
        anyhow::bail!("empty path");
    }
    let candidate = Path::new(raw);
    // An absolute path is only acceptable if it is already inside the root;
    // models often echo back a full path we handed them.
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };

    let lexical = lexical_normalize(&joined);
    if !lexical.starts_with(root) {
        anyhow::bail!("path escapes the planning scope root: {}", input);
    }

    // Canonicalize what exists so symlinks cannot tunnel out. Missing files
    // fall back to the (already-checked) lexical form so the caller reports
    // "not found" rather than a confusing resolution error.
    match lexical.canonicalize() {
        Ok(real) => {
            if !real.starts_with(root) {
                anyhow::bail!("path resolves outside the planning scope root: {}", input);
            }
            Ok(real)
        }
        Err(_) => Ok(lexical),
    }
}

/// Canonical form of a scope root, resolved once when a thread is opened.
pub fn canonical_root(root: &Path) -> anyhow::Result<PathBuf> {
    root.canonicalize()
        .map_err(|e| anyhow::anyhow!("planning root {}: {}", root.display(), e))
}

/// Display form of a path relative to the root, for thread records and model
/// output. Falls back to the full path if it is somehow not under the root.
pub fn rel_to_root(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string())
}

/// Outcome of a tool call: text for the model, plus any read to record.
pub struct ToolOutcome {
    pub text: String,
    pub read: Option<ReadRecord>,
}

impl ToolOutcome {
    fn plain(text: impl Into<String>) -> ToolOutcome {
        ToolOutcome {
            text: text.into(),
            read: None,
        }
    }
}

/// Execute one read-only tool call. Unknown names return an error string
/// rather than failing the turn — the model recovers better from "no such
/// tool" than from a dropped conversation.
pub fn exec(root: &Path, name: &str, args: &serde_json::Value) -> ToolOutcome {
    let result = match name {
        "read_file" => read_file(root, args),
        "list_dir" => list_dir(root, args),
        "grep" => grep(root, args),
        other => Err(anyhow::anyhow!(
            "no such tool: {} (planning threads are read-only; available: \
             read_file, list_dir, grep)",
            other
        )),
    };
    match result {
        Ok(outcome) => outcome,
        Err(e) => ToolOutcome::plain(format!("[error] {}", e)),
    }
}

fn arg_str(args: &serde_json::Value, key: &str) -> anyhow::Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("missing required argument: {}", key))
}

fn read_file(root: &Path, args: &serde_json::Value) -> anyhow::Result<ToolOutcome> {
    let input = arg_str(args, "path")?;
    let path = resolve_in_root(root, &input)?;
    let meta = fs::metadata(&path).map_err(|e| anyhow::anyhow!("cannot read {}: {}", input, e))?;
    if meta.is_dir() {
        anyhow::bail!("{} is a directory (use list_dir)", input);
    }

    let mut bytes = Vec::new();
    fs::File::open(&path)?
        .take((MAX_READ_BYTES * 4) as u64)
        .read_to_end(&mut bytes)?;
    let hash = content_hash(&bytes);

    if bytes.contains(&0) {
        return Ok(ToolOutcome {
            text: format!("[{} is binary, {} bytes — not shown]", input, meta.len()),
            read: Some(ReadRecord {
                rel: rel_to_root(root, &path),
                hash,
                mtime: mtime_secs(&path),
            }),
        });
    }

    let full = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = full.lines().collect();
    let offset = args
        .get("offset")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .min(lines.len() as u64) as usize;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(u64::MAX);

    let mut out = String::new();
    let mut shown = 0usize;
    let mut truncated = false;
    for (i, line) in lines.iter().enumerate().skip(offset) {
        if shown as u64 >= limit {
            truncated = true;
            break;
        }
        if out.len() + line.len() + 12 > MAX_READ_BYTES {
            truncated = true;
            break;
        }
        out.push_str(&format!("{:>6}\t{}\n", i + 1, line));
        shown += 1;
    }
    if truncated {
        out.push_str(&format!(
            "[truncated at line {} of {} — call read_file again with offset={}]\n",
            offset + shown,
            lines.len(),
            offset + shown
        ));
    }

    Ok(ToolOutcome {
        text: out,
        read: Some(ReadRecord {
            rel: rel_to_root(root, &path),
            hash,
            mtime: mtime_secs(&path),
        }),
    })
}

fn list_dir(root: &Path, args: &serde_json::Value) -> anyhow::Result<ToolOutcome> {
    let input = args
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(".")
        .to_string();
    let path = resolve_in_root(root, &input)?;
    let entries =
        fs::read_dir(&path).map_err(|e| anyhow::anyhow!("cannot list {}: {}", input, e))?;

    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir && SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        names.push(if is_dir { format!("{}/", name) } else { name });
    }
    names.sort();
    let total = names.len();
    names.truncate(MAX_LIST_ENTRIES);
    let mut text = names.join("\n");
    if total > MAX_LIST_ENTRIES {
        text.push_str(&format!(
            "\n[{} more entries not shown]",
            total - MAX_LIST_ENTRIES
        ));
    }
    if text.is_empty() {
        text = "[empty directory]".to_string();
    }
    Ok(ToolOutcome::plain(text))
}

fn grep(root: &Path, args: &serde_json::Value) -> anyhow::Result<ToolOutcome> {
    let pattern = arg_str(args, "pattern")?;
    let re = regex::Regex::new(&pattern)
        .map_err(|e| anyhow::anyhow!("invalid regex {:?}: {}", pattern, e))?;
    let start_input = args
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(".")
        .to_string();
    let start = resolve_in_root(root, &start_input)?;
    // Simple extension filter; a full glob engine is more than this needs.
    let ext_filter: Option<String> = args
        .get("ext")
        .and_then(|v| v.as_str())
        .map(|s| s.trim_start_matches('.').to_string());

    let mut hits: Vec<String> = Vec::new();
    let mut stack = vec![start];
    while let Some(dir) = stack.pop() {
        if hits.len() >= MAX_GREP_HITS {
            break;
        }
        let meta = match fs::metadata(&dir) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_file() {
            scan_file(root, &dir, &re, ext_filter.as_deref(), &mut hits);
            continue;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                if SKIP_DIRS.contains(&name.as_str()) || name.starts_with('.') {
                    continue;
                }
                stack.push(p);
            } else {
                scan_file(root, &p, &re, ext_filter.as_deref(), &mut hits);
                if hits.len() >= MAX_GREP_HITS {
                    break;
                }
            }
        }
    }

    let text = if hits.is_empty() {
        format!("[no matches for {:?}]", pattern)
    } else {
        let capped = hits.len() >= MAX_GREP_HITS;
        let mut t = hits.join("\n");
        if capped {
            t.push_str("\n[hit limit reached — narrow the pattern or path]");
        }
        t
    };
    Ok(ToolOutcome::plain(text))
}

fn scan_file(
    root: &Path,
    path: &Path,
    re: &regex::Regex,
    ext_filter: Option<&str>,
    hits: &mut Vec<String>,
) {
    if let Some(want) = ext_filter {
        let ok = path
            .extension()
            .map(|e| e.to_string_lossy() == want)
            .unwrap_or(false);
        if !ok {
            return;
        }
    }
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return,
    };
    if meta.len() > MAX_GREP_FILE_BYTES {
        return;
    }
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(_) => return,
    };
    if bytes.contains(&0) {
        return;
    }
    let text = String::from_utf8_lossy(&bytes);
    let rel = rel_to_root(root, path);
    for (i, line) in text.lines().enumerate() {
        if hits.len() >= MAX_GREP_HITS {
            return;
        }
        if re.is_match(line) {
            let shown: String = line.chars().take(240).collect();
            hits.push(format!("{}:{}:{}", rel, i + 1, shown.trim_end()));
        }
    }
}

// ── Tool schemas ──────────────────────────────────────────────────────────

/// Anthropic Messages API tool definitions.
pub fn anthropic_tools() -> serde_json::Value {
    serde_json::json!([
        {
            "name": "read_file",
            "description": "Read a text file inside the planning scope root. \
                            Returns numbered lines. Long files are truncated; \
                            call again with a larger offset for more.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the scope root."},
                    "offset": {"type": "integer", "description": "0-based first line to return."},
                    "limit": {"type": "integer", "description": "Maximum lines to return."}
                },
                "required": ["path"]
            }
        },
        {
            "name": "list_dir",
            "description": "List the entries of a directory inside the scope root. \
                            Directory names end with '/'. Build and VCS directories are skipped.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory relative to the scope root. Defaults to the root."}
                }
            }
        },
        {
            "name": "grep",
            "description": "Search file contents by regular expression, recursively. \
                            Returns path:line:text. Use this to locate code before reading it.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Rust regex syntax."},
                    "path": {"type": "string", "description": "Directory or file to search. Defaults to the root."},
                    "ext": {"type": "string", "description": "Only search files with this extension, e.g. 'rs'."}
                },
                "required": ["pattern"]
            }
        }
    ])
}

/// OpenAI-compatible tool definitions, derived from the Anthropic schemas so
/// the two providers cannot drift apart.
pub fn openai_tools() -> serde_json::Value {
    let converted: Vec<serde_json::Value> = anthropic_tools()
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t["name"],
                            "description": t["description"],
                            "parameters": t["input_schema"],
                        }
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::Value::Array(converted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "linkshell-planning-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(base.join("src")).unwrap();
        fs::write(base.join("src/lib.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        fs::write(base.join("README.md"), "# hello\n").unwrap();
        base.canonicalize().unwrap()
    }

    #[test]
    fn resolves_paths_inside_the_root() {
        let root = tmp_root();
        let p = resolve_in_root(&root, "src/lib.rs").unwrap();
        assert!(p.starts_with(&root));
        assert_eq!(rel_to_root(&root, &p), "src/lib.rs");
    }

    #[test]
    fn rejects_parent_traversal_and_absolute_escapes() {
        let root = tmp_root();
        assert!(resolve_in_root(&root, "../etc/passwd").is_err());
        assert!(resolve_in_root(&root, "src/../../etc/passwd").is_err());
        assert!(resolve_in_root(&root, "/etc/passwd").is_err());
        // A missing file inside the root is fine — read_file reports not-found.
        assert!(resolve_in_root(&root, "src/nope.rs").is_ok());
        // …but a missing file *outside* it is still rejected.
        assert!(resolve_in_root(&root, "../nope.rs").is_err());
    }

    #[test]
    fn rejects_symlinks_pointing_outside_the_root() {
        let root = tmp_root();
        let link = root.join("escape");
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc", &link).unwrap();
        assert!(resolve_in_root(&root, "escape/passwd").is_err());
    }

    #[test]
    fn read_file_returns_numbered_lines_and_records_the_read() {
        let root = tmp_root();
        let out = exec(
            &root,
            "read_file",
            &serde_json::json!({"path": "src/lib.rs"}),
        );
        assert!(out.text.contains("fn alpha"));
        assert!(out.text.contains("     1\t"));
        let rec = out.read.expect("read recorded");
        assert_eq!(rec.rel, "src/lib.rs");
        assert_ne!(rec.hash, 0);
    }

    #[test]
    fn read_file_offset_and_limit_window_the_output() {
        let root = tmp_root();
        let out = exec(
            &root,
            "read_file",
            &serde_json::json!({"path": "src/lib.rs", "offset": 1, "limit": 1}),
        );
        assert!(out.text.contains("fn beta"));
        assert!(!out.text.contains("fn alpha"));
    }

    #[test]
    fn grep_finds_matches_with_path_and_line() {
        let root = tmp_root();
        let out = exec(&root, "grep", &serde_json::json!({"pattern": "fn beta"}));
        assert!(out.text.contains("src/lib.rs:2:"), "got: {}", out.text);
    }

    #[test]
    fn grep_ext_filter_excludes_other_files() {
        let root = tmp_root();
        let out = exec(
            &root,
            "grep",
            &serde_json::json!({"pattern": "hello", "ext": "rs"}),
        );
        assert!(out.text.starts_with("[no matches"), "got: {}", out.text);
    }

    #[test]
    fn list_dir_marks_directories() {
        let root = tmp_root();
        let out = exec(&root, "list_dir", &serde_json::json!({}));
        assert!(out.text.contains("src/"));
        assert!(out.text.contains("README.md"));
    }

    #[test]
    fn write_tools_do_not_exist() {
        let root = tmp_root();
        for name in [
            "write_file",
            "edit",
            "bash",
            "shell",
            "apply_patch",
            "remember",
        ] {
            let out = exec(
                &root,
                name,
                &serde_json::json!({"path": "x", "content": "y"}),
            );
            assert!(
                out.text.contains("no such tool"),
                "{} must not be callable, got: {}",
                name,
                out.text
            );
        }
        // And the file was not created by any of them.
        assert!(!root.join("x").exists());
    }

    #[test]
    fn tool_schemas_agree_across_providers() {
        let a = anthropic_tools();
        let o = openai_tools();
        let a_names: Vec<&str> = a
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        let o_names: Vec<&str> = o
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(a_names, o_names);
        assert_eq!(a_names, vec!["read_file", "list_dir", "grep"]);
    }
}
