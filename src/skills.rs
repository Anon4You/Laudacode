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
    /// fallback.
    pub fn dir_name(path: &Path) -> String {
        path.parent()
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
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path().join("SKILL.md");
            if !path.is_file() {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
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
            out.push(Skill {
                name,
                description,
                path,
            });
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
            clip(&sk.description, 160)
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

/// `/skills` picker rows: `name — description`. An empty description gets a
/// hint. The REPL handles the chosen row (stages it into the composer).
pub fn picker_items(cwd: &Path) -> Vec<String> {
    picker_items_for(&discover(cwd))
}

fn picker_items_for(skills: &[Skill]) -> Vec<String> {
    skills
        .iter()
        .map(|s| {
            let desc = if s.description.is_empty() {
                "(no description)"
            } else {
                &clip(&s.description, 90)
            };
            format!("{} — {}", s.name, desc)
        })
        .collect()
}

/// Parse `key: value` frontmatter.
///
/// Real skill files write long descriptions as a YAML block scalar:
///
/// ```md
/// description: >
///   Prose that wraps across
///   several lines.
/// ```
///
/// A line-by-line read takes the description to be the lone `>` and throws the
/// prose away, so every such skill shows up in the picker as ">". Folded (`>`,
/// spaces) and literal (`|`, newlines) scalars are both handled, with the
/// `-`/`+` chomping indicators tolerated.
pub(crate) fn split_frontmatter(
    raw: &str,
) -> (std::collections::BTreeMap<String, String>, String) {
    let mut map = std::collections::BTreeMap::new();
    let trimmed = raw.trim_start();
    let Some(rest) = trimmed.strip_prefix("---") else {
        return (map, raw.to_string());
    };
    let Some((fm_raw, body)) = rest.split_once("---") else {
        return (map, raw.to_string());
    };
    let lines: Vec<&str> = fm_raw.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_end();
        i += 1;
        if line.trim().is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim().to_lowercase();
        let val = v.trim();
        // `key: >` / `key: >-` / `key: |+` → the value is the indented block
        // that follows, not the marker.
        let folded = val.starts_with('>');
        if !(folded || val.starts_with('|')) {
            map.insert(key, val.to_string());
            continue;
        }
        // Continuation lines are blank or indented past the key's own column.
        let indent = line.len() - line.trim_start().len();
        let mut block: Vec<String> = Vec::new();
        while i < lines.len() {
            let nxt = lines[i];
            if !nxt.trim().is_empty() && nxt.len() - nxt.trim_start().len() <= indent {
                break; // dedented: a new key
            }
            block.push(nxt.trim().to_string());
            i += 1;
        }
        while block.last().is_some_and(|l| l.is_empty()) {
            block.pop(); // trailing blank lines are not content
        }
        let joined = if folded {
            block.join(" ")
        } else {
            block.join("\n")
        };
        // A marker with no block body is a mistake, not an empty description:
        // fall back to the marker-free body line rather than showing ">" .
        if joined.is_empty() {
            map.insert(key, val.to_string());
        } else {
            map.insert(key, joined);
        }
    }
    (map, body.trim_start().to_string())
}

/// Clip `s` to `max` characters on a word boundary, marking the cut with an
/// ellipsis so a truncated description never reads as a complete one.
fn clip(s: &str, max: usize) -> String {
    let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= max {
        return t;
    }
    let mut out: String = t.chars().take(max).collect();
    if let Some(cut) = out.rfind(' ') {
        out.truncate(cut);
    }
    out.push('…');
    out
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

    #[test]
    fn picker_rows_use_name_and_description() {
        assert!(picker_items_for(&[]).is_empty(), "no skills → empty picker");
        let skills = vec![
            Skill {
                name: "release-notes".into(),
                description: "Write release notes".into(),
                path: PathBuf::from("global/skills/release-notes/SKILL.md"),
            },
            Skill {
                name: "minimal".into(),
                description: String::new(),
                path: PathBuf::from("global/skills/minimal/SKILL.md"),
            },
        ];
        let items = picker_items_for(&skills);
        assert_eq!(
            items,
            vec![
                "release-notes — Write release notes",
                "minimal — (no description)"
            ]
        );
    }

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
        write_skill(
            &sb.cwd.join(".laudacode").join("skills"),
            "deploy",
            "project deploy variant",
        );
        write_skill(
            &sb.cwd.join(".laudacode").join("skills"),
            "lint",
            "lint the code",
        );

        let skills = discover(&sb.cwd);
        let deploy = skills
            .iter()
            .find(|s| s.name == "deploy")
            .expect("deploy present");
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
        let bare = skills
            .iter()
            .find(|s| s.name == "bare")
            .expect("bare present");
        assert_eq!(bare.description, "Commit style guide");
        assert_eq!(Skill::dir_name(&bare.path), "bare");
    }

    /// The `description: >` folded scalar used by real skill files (the
    /// opencode format). A line-by-line read yields ">" as the description
    /// and silently discards the prose — the picker then shows every skill
    /// as ">".
    #[test]
    fn folded_block_scalar_descriptions_are_joined() {
        let raw = "---\nname: ponytail-help\ndescription: >\n  Quick-reference card for all modes.\n  One-shot display, not a persistent mode.\n---\n\n# Ponytail\n";
        let (fm, body) = split_frontmatter(raw);
        assert_eq!(fm.get("name").unwrap(), "ponytail-help");
        assert_eq!(
            fm.get("description").unwrap(),
            "Quick-reference card for all modes. One-shot display, not a persistent mode."
        );
        assert!(body.starts_with("# Ponytail"), "body was {body:?}");
    }

    /// A literal `|` scalar keeps its newlines, and a single-line value
    /// after it must not be swallowed into the description.
    #[test]
    fn literal_block_scalars_keep_line_breaks() {
        let raw = "---\nname: x\ndescription: |\n  line one\n  line two\nother: kept\n---\nbody\n";
        let (fm, _body) = split_frontmatter(raw);
        assert_eq!(fm.get("description").unwrap(), "line one\nline two");
        // A sibling key after the block is not swallowed.
        assert_eq!(fm.get("other").unwrap(), "kept");
    }

    /// Long real descriptions must not eat the whole picker row.
    #[test]
    fn long_descriptions_are_clipped_for_the_picker_and_the_prompt() {
        let long = "word ".repeat(200);
        let skills = vec![Skill {
            name: "verbose".into(),
            description: long.clone(),
            path: PathBuf::from("x/SKILL.md"),
        }];
        let row = &picker_items_for(&skills)[0];
        assert!(
            row.chars().count() < 200,
            "picker row not clipped: {}",
            row.chars().count()
        );
        let block = prompt_block(&skills);
        assert!(block.chars().count() < 400, "prompt block not clipped");
        // The clip must not end mid-word with no hint that it was cut.
        assert!(!row.contains("wor "), "row should not end mid-word");
    }

    #[test]
    fn prompt_block_lists_skills_with_paths() {
        let sb = Sandbox::new();
        write_skill(
            &sb.cwd.join(".laudacode").join("skills"),
            "pdf",
            "generate pdfs",
        );
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
