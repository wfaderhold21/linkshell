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
}
