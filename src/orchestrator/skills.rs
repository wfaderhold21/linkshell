//! Orchestrator skills: markdown files the agent pulls in on demand.
//!
//! A skill is one `.md` file in the configured skills directory. Only the
//! name + description are put in the prompt; the full text is loaded when
//! the agent decides it is relevant (API class: `use_skill` tool; CLI class:
//! the briefing lists the file paths and the CLI reads them itself).

use std::path::{Path, PathBuf};

pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// Skills shipped with linkshell, installed into the skills directory the
/// first time an orchestrator starts. Each is written only when a file of
/// that name is absent, so a user's edits are never clobbered. A deleted
/// default is restored on next start; to drop one for good, empty the file
/// or override skills_dir.
pub const DEFAULT_SKILLS: &[(&str, &str)] = &[
    (
        "install-approval",
        "---\n\
description: A session wants to install software or run a destructive command\n\
---\n\
\n\
# Install approval\n\
\n\
A session has stopped on a prompt that would change what is installed on this\n\
machine or in the project: a package manager confirmation (apt, dnf, brew,\n\
pip, cargo, npm, go get), a toolchain download, a container pull, or an\n\
agent asking permission to add a dependency.\n\
\n\
You do not approve these. The user does.\n\
\n\
1. `read_output` on the session to capture the exact prompt and the exact\n\
   command or package list it is about to act on.\n\
2. Tell the user in one or two lines: which session (display number), what it\n\
   wants to install, and what it will do to the system if allowed.\n\
3. Stop. Do not type `y`, `yes`, `1`, Enter, or anything else into that\n\
   session, and do not run the install yourself in a shell session.\n\
4. Wait for the user to say so explicitly. \"Go ahead\", \"approve it\", or a\n\
   direct instruction naming the session counts; silence and general\n\
   encouragement do not.\n\
5. When approved, send exactly the response the prompt expects, then confirm\n\
   what happened.\n\
\n\
If several sessions are blocked on installs at once, list them all and let the\n\
user decide in one pass rather than asking repeatedly.\n\
\n\
Uninstalls, version downgrades, and anything touching a system-wide path get\n\
the same treatment. Reading a lockfile or asking what version is installed\n\
does not — that is inspection, not change.\n\
\n\
## Destructive commands\n\
\n\
Same rule, same procedure, for anything hard to undo: `rm -rf`, `git reset\n\
--hard`, `git clean -fdx`, force pushes, deleting or resetting a branch,\n\
dropping a table, truncating a file, overwriting a config the user maintains.\n\
\n\
Quote the exact command and name what it destroys — not \"cleaning up the\n\
build directory\" but the path it will delete and whether anything there is\n\
untracked. The user can only approve what they can see. If a session is\n\
already blocked on such a command, do not confirm it for them; if you were\n\
about to run one yourself in a shell session, describe it instead and wait.\n",
    ),
    (
        "uncertain-input",
        "---\n\
description: You cannot tell whether text you sent to a session was received\n\
---\n\
\n\
# Uncertain input\n\
\n\
You typed into a session and cannot confirm it landed: the output is\n\
unchanged, the state never moved off READY, the pane is showing a pager or a\n\
full-screen editor, or the session was mid-render when you wrote.\n\
\n\
Do not guess, and do not retype. A blind resend can double-execute a command,\n\
answer a prompt you never saw, or dump text into an editor buffer.\n\
\n\
1. `read_output` once more on that session — a slow session often just needed\n\
   a moment.\n\
2. If the output shows your text, carry on normally.\n\
3. If it does not, say so plainly: which session, what you tried to send, and\n\
   that you are not sure it was received. One or two lines.\n\
4. Move on to the rest of the user's request. An unconfirmed input is not a\n\
   reason to stall everything else, and it is not a reason to keep polling the\n\
   same session in a loop.\n\
5. Let the user decide whether to resend. If they ask you to, send it once.\n\
\n\
The same applies when a session's state is ambiguous — READY but with a\n\
half-drawn prompt, or output that stopped mid-line. Report the ambiguity\n\
instead of resolving it by assumption.\n",
    ),
];

/// Write the shipped defaults into `dir`, skipping any that already exist.
/// Best-effort and idempotent.
pub fn install_defaults(dir: &Path) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    for (name, body) in DEFAULT_SKILLS {
        let path = dir.join(format!("{name}.md"));
        if !path.exists() {
            let _ = std::fs::write(&path, body);
        }
    }
}

/// Load skill metadata (not the bodies) from `dir`, sorted by name.
pub fn load_skills(dir: &Path) -> Vec<Skill> {
    let mut skills: Vec<Skill> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
        .filter_map(|path| {
            let name = path.file_stem()?.to_str()?.to_string();
            let content = std::fs::read_to_string(&path).ok()?;
            Some(Skill {
                name,
                description: describe(&content),
                path,
            })
        })
        .collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Description of a skill: the `description:` field of leading `---`
/// frontmatter if present, else the first non-empty line (stripped of
/// leading `#` heading markers).
fn describe(content: &str) -> String {
    let mut lines = content.lines();
    if lines.next().map(str::trim) == Some("---") {
        for line in lines.by_ref() {
            let line = line.trim();
            if line == "---" {
                break;
            }
            if let Some(desc) = line.strip_prefix("description:") {
                return desc.trim().trim_matches('"').to_string();
            }
        }
    }
    content
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty() && *l != "---")
        .unwrap_or_default()
        .to_string()
}

/// Full text of the named skill. The name must match a skill in `dir`
/// (no paths accepted), so the agent can't read arbitrary files with it.
pub fn read_skill(dir: &Path, name: &str) -> anyhow::Result<String> {
    let skill = load_skills(dir)
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| anyhow::anyhow!("no skill named '{}' in {}", name, dir.display()))?;
    Ok(std::fs::read_to_string(&skill.path)?)
}

/// "- name: description" lines for the system prompt / briefing.
/// `with_paths` appends the file path (CLI class reads the files itself).
pub fn skill_list(skills: &[Skill], with_paths: bool) -> String {
    skills
        .iter()
        .map(|s| {
            if with_paths {
                format!("- {}: {} ({})", s.name, s.description, s.path.display())
            } else {
                format!("- {}: {}", s.name, s.description)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_skills_dir(files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "linkshell-skills-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, content) in files {
            std::fs::write(dir.join(name), content).unwrap();
        }
        dir
    }

    #[test]
    fn loads_md_skills_with_frontmatter_or_first_line_descriptions() {
        let dir = temp_skills_dir(&[
            (
                "review.md",
                "---\ndescription: How to run a code review\n---\nSteps...",
            ),
            ("deploy.md", "# Deploy checklist\n\n1. build\n"),
            ("notes.txt", "not a skill"),
        ]);
        let skills = load_skills(&dir);
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "deploy");
        assert_eq!(skills[0].description, "Deploy checklist");
        assert_eq!(skills[1].name, "review");
        assert_eq!(skills[1].description, "How to run a code review");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_skill_returns_body_and_rejects_unknown_names() {
        let dir = temp_skills_dir(&[("deploy.md", "# Deploy\ncontent")]);
        assert!(read_skill(&dir, "deploy").unwrap().contains("content"));
        assert!(read_skill(&dir, "missing").is_err());
        assert!(read_skill(&dir, "../etc/passwd").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skill_list_formats_with_and_without_paths() {
        let skills = vec![Skill {
            name: "deploy".into(),
            description: "Deploy checklist".into(),
            path: PathBuf::from("/tmp/deploy.md"),
        }];
        assert_eq!(skill_list(&skills, false), "- deploy: Deploy checklist");
        assert!(skill_list(&skills, true).contains("(/tmp/deploy.md)"));
    }

    #[test]
    fn install_defaults_seeds_without_clobbering_user_edits() {
        let dir = temp_skills_dir(&[]);
        install_defaults(&dir);
        let loaded = load_skills(&dir);
        assert_eq!(loaded.len(), DEFAULT_SKILLS.len());
        // Descriptions come from the frontmatter, so they show up in prompts.
        assert!(loaded.iter().all(|s| !s.description.is_empty()));

        // A user edit survives a second install.
        let edited = dir.join("install-approval.md");
        std::fs::write(&edited, "---\ndescription: mine\n---\nbody").unwrap();
        std::fs::remove_file(dir.join("uncertain-input.md")).unwrap();
        install_defaults(&dir);
        // A deleted default is restored.
        assert_eq!(
            std::fs::read_to_string(&edited).unwrap(),
            "---\ndescription: mine\n---\nbody"
        );
        assert!(dir.join("uncertain-input.md").exists());
    }
}
