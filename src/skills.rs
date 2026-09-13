//! Skills — reusable instruction packs discovered from disk.
//!
//! A skill lives at `~/.config/laudacode/skills/<name>/SKILL.md` (global) or
//! `<project>/.laudacode/skills/<name>/SKILL.md` (project), following the
//! same convention popular agent CLIs use. The file is markdown with
//! optional frontmatter:
//!
//! ```md
//! ---
//! name: my-skill
//! description: Short one-liner shown to the agent in the system prompt.
//! ---
//! Full instructions …
//! ```
//!
//! Progressive disclosure: only `name` + `description` are injected into the
//! system prompt (a few tokens each); the agent reads the full file with
//! `read_file` when the skill is relevant to the task at hand.

use std::path::{Path, PathBuf};

/// One discovered skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Slash-command-safe skill id (directory name or frontmatter override).
    pub name: String,
    /// One-line description from frontmatter (or the first body line).
    pub description: String,
    /// Absolute path to the SKILL.md file.
    pub path: PathBuf,
}

impl Skill {
    /// Frontmatter-derived name is preferred; the directory name is the
    /// fallback (and what `/skills` invocation uses).
    pub fn dir_name(path: &Path) -> String {
        path
            .parent()
            .and_then(Path::file_name)
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    }
}

/// Where skills are looked up, in precedence order (later wins).
/// 1. `~/.config/laudacode/skills/` (global, all projects)
/// 2. `<cwd>/.laudacode/skills/` (project-local overrides)
pub fn skill_dirs(cwd: &Path) -> Vec<PathBuf> {
    // Test/power-user override is respected via Config::dir(); the real
    // location is ~/.config/laudacode/skills/.
    vec![
        crate::config::Config::dir().join("skills"),
        cwd.join(".laudacode").join("skills"),
    ]
}

/// Scan all skill directories. Project entries override global ones with the
/// same name (they are appended last and dedupe keeps the last occurrence).
pub fn discover(cwd: &Path) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    for dir in skill_dirs(cwd) {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path().join("SKILL.md");
            if !path.is_file() {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else { continue };
            let (fm, body) = split_frontmatter(&raw);
            let name = fm
                .get("name")
                .cloned()
                .unwrap_or_else(|| Skill::dir_name(&path))
                .trim()
                .to_string();
            if name.is_empty() {
                continue;
            }
            let description = fm
                .get("description")
                .cloned()
                .unwrap_or_else(|| first_body_line(&body))
                .trim()
                .to_string();
            // Later directory wins (project overrides global).
            out.retain(|s: &Skill| s.name != name);
            out.push(Skill { name, description, path });
        }
    }
    out
}

/// The block injected into the system prompt: one bullet per skill. Only
/// names + descriptions (a handful of tokens) — the agent reads the full
/// SKILL.md itself when a task matches, keeping context lean.
pub fn prompt_block(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "Skills available — reusable instruction packs. When a task matches a skill, \
         FIRST read its SKILL.md with read_file, then follow it:\n",
    );
    for sk in skills {
        let desc = if sk.description.is_empty() {
            String::from("(no description)")
        } else {
            sk.description.chars().take(160).collect()
        };
        s.push_str(&format!(
            "- {} — {} (read {} for full instructions)\n",
            sk.name,
            desc,
            sk.path.display()
        ));
    }
    s.push('\n');
    s
}

/// Parse simple `key: value` frontmatter (no nesting, no quotes needed).
fn split_frontmatter(raw: &str) -> (std::collections::BTreeMap<String, String>, String) {
    let mut map = std::collections::BTreeMap::new();
    let trimmed = raw.trim_start();
    let Some(rest) = trimmed.strip_prefix("---") else {
        return (map, raw.to_string());
    };
    let Some((fm_raw, body)) = rest.split_once("---") else {
        return (map, raw.to_string());
    };
    for line in fm_raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            map.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    (map, body.trim_start().to_string())
}

/// First non-empty markdown line, stripped of heading marks.
fn first_body_line(body: &str) -> String {
    for line in body.lines() {
        let l = line.trim().trim_start_matches('#').trim();
        if !l.is_empty() {
            return l.to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &Path, name: &str, description: &str) {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n# {name}\nBody instructions.\n"),
        )
        .unwrap();
    }

    /// Isolated temp cwd + config dir so tests never touch the real ones.
    struct Sandbox {
        cwd: PathBuf,
        #[allow(dead_code)]
        home: PathBuf,
    }
    impl Sandbox {
        fn new() -> Self {
            let base = std::env::temp_dir().join(format!(
                "laudacode-skills-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let home = base.join("home");
            let cwd = base.join("project");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::create_dir_all(&cwd).unwrap();
            std::env::set_var("LAUDACODE_CONFIG_DIR", home.join("config"));
            Self { cwd, home }
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            std::env::remove_var("LAUDACODE_CONFIG_DIR");
            let _ = std::fs::remove_dir_all(self.cwd.parent().unwrap());
        }
    }

    #[test]
    fn discovers_global_and_project_skills_with_override() {
        let sb = Sandbox::new();
        let global = crate::config::Config::dir().join("skills");
        write_skill(&global, "deploy", "ship the app");
        write_skill(&sb.cwd.join(".laudacode").join("skills"), "deploy", "project deploy variant");
        write_skill(&sb.cwd.join(".laudacode").join("skills"), "lint", "lint the code");

        let skills = discover(&sb.cwd);
        let deploy = skills.iter().find(|s| s.name == "deploy").expect("deploy present");
        assert_eq!(deploy.description, "project deploy variant", "project wins");
        assert!(skills.iter().any(|s| s.name == "lint"));
        // Paths point at the SKILL.md files.
        assert!(deploy.path.ends_with("SKILL.md"));
        assert_eq!(Skill::dir_name(&deploy.path), "deploy");
    }

    #[test]
    fn frontmatter_optional_and_description_falls_back_to_body() {
        let sb = Sandbox::new();
        let d = sb.cwd.join(".laudacode").join("skills").join("bare");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            "# Commit style guide\nUse conventional commits.\n",
        )
        .unwrap();
        let skills = discover(&sb.cwd);
        let bare = skills.iter().find(|s| s.name == "bare").expect("bare present");
        assert_eq!(bare.description, "Commit style guide");
        assert_eq!(Skill::dir_name(&bare.path), "bare");
    }

    #[test]
    fn prompt_block_lists_skills_with_paths() {
        let sb = Sandbox::new();
        write_skill(&sb.cwd.join(".laudacode").join("skills"), "pdf", "generate pdfs");
        let skills = discover(&sb.cwd);
        let block = prompt_block(&skills);
        assert!(block.contains("pdf"));
        assert!(block.contains("generate pdfs"));
        assert!(block.contains("SKILL.md"), "must point at the file");
        // No skills → empty block (nothing injected).
        assert!(prompt_block(&[]).is_empty());
    }

    #[test]
    fn empty_skill_dirs_yield_no_skills() {
        let sb = Sandbox::new();
        assert!(discover(&sb.cwd).is_empty());
    }
}
