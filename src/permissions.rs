//! Permission rules — allow / ask / deny per tool with wildcard patterns,
//! evaluated last-match-wins (parity with modern agent CLIs).
//!
//! ```toml
//! [permission.bash]
//! "*" = "ask"
//! "git *" = "allow"
//! "rm *" = "deny"
//! ```

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rule {
    /// Run without prompting.
    Allow,
    /// Always route through the approval modal.
    Ask,
    /// Refuse before any side effect.
    Deny,
}

/// Map of glob pattern -> rule for one tool. Order matters: later wins.
pub type PatternMap = std::collections::BTreeMap<String, Rule>;

/// Full permission configuration. `None` maps fall back to the danger
/// heuristics; present maps override them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Permissions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bash: Option<PatternMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit: Option<PatternMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<PatternMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webfetch: Option<PatternMap>,
}

impl Permissions {
    /// Resolve the rule for one tool input. Returns None when no rule
    /// matched (caller falls back to danger-based behavior).
    ///
    /// `input` is:
    /// - bash: the full command line
    /// - edit: the target file path
    /// - read: the file path
    /// - webfetch: the URL
    pub fn resolve(&self, tool: &str, input: &str) -> Option<Rule> {
        let map = match tool {
            "bash" => self.bash.as_ref()?,
            "edit" => self.edit.as_ref()?,
            "read" => self.read.as_ref()?,
            "webfetch" => self.webfetch.as_ref()?,
            _ => return None,
        };
        // Last matching pattern wins (documented contract).
        let mut matched: Option<Rule> = None;
        for (pat, rule) in map {
            if glob_match(pat, input) {
                matched = Some(*rule);
            }
        }
        matched
    }

    /// Defaults applied even with an empty `[permission]` section: secret
    /// material must not be readable by the model unless explicitly allowed.
    pub fn secret_guard(path: &str) -> Option<Rule> {
        let name = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if name == ".env" || name.starts_with(".env.") {
            if name == ".env.example" {
                return None;
            }
            return Some(Rule::Deny);
        }
        None
    }
}

/// Wildcard matcher shared with the glob tool semantics: `*` crosses
/// everything here (permission patterns are flat strings, not paths).
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    fn inner(p: &[char], t: &[char]) -> bool {
        let (mut pi, mut ti) = (0usize, 0usize);
        let (mut star_p, mut star_t) = (usize::MAX, 0usize);
        while ti < t.len() {
            if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
                pi += 1;
                ti += 1;
            } else if pi < p.len() && p[pi] == '*' {
                star_p = pi;
                star_t = ti;
                pi += 1;
            } else if star_p != usize::MAX {
                pi = star_p + 1;
                star_t += 1;
                ti = star_t;
            } else {
                return false;
            }
        }
        while pi < p.len() && p[pi] == '*' {
            pi += 1;
        }
        pi == p.len()
    }
    inner(&p, &t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perms(toml_str: &str) -> Permissions {
        toml::from_str(toml_str).unwrap()
    }

    #[test]
    fn wildcard_rules_last_match_wins() {
        let p = perms(
            r#"
[bash]
"*" = "ask"
"git status" = "allow"
"git *" = "allow"
"git push*" = "deny"
"rm *" = "deny"
"#,
        );
        assert_eq!(p.resolve("bash", "cargo build"), Some(Rule::Ask));
        assert_eq!(p.resolve("bash", "git status"), Some(Rule::Allow));
        assert_eq!(p.resolve("bash", "git commit -m x"), Some(Rule::Allow));
        assert_eq!(p.resolve("bash", "git push origin main"), Some(Rule::Deny));
        assert_eq!(p.resolve("bash", "rm -rf build"), Some(Rule::Deny));
        // Catch-all "*" matches everything, so nothing falls back here…
        assert_eq!(p.resolve("bash", "ls"), Some(Rule::Ask));
        // …but a tool with NO section does fall back to heuristics.
        let bare = perms("");
        assert_eq!(bare.resolve("webfetch", "https://x"), None);
    }

    #[test]
    fn edit_and_read_and_webfetch_maps() {
        let p = perms(
            r#"
[edit]
"*" = "deny"
"src/*" = "allow"

[read]
"*" = "allow"

[webfetch]
"https://docs.example.com/*" = "allow"
"*" = "ask"
"#,
        );
        assert_eq!(p.resolve("edit", "src/main.rs"), Some(Rule::Allow));
        assert_eq!(p.resolve("edit", "/etc/passwd"), Some(Rule::Deny));
        assert_eq!(p.resolve("read", "anything.txt"), Some(Rule::Allow));
        assert_eq!(
            p.resolve("webfetch", "https://docs.example.com/api"),
            Some(Rule::Allow)
        );
        assert_eq!(p.resolve("webfetch", "https://evil.example.net"), Some(Rule::Ask));
    }

    #[test]
    fn question_mark_matches_single_char() {
        let p = perms(
            r#"
[bash]
"sh?-c" = "deny"
"#,
        );
        assert_eq!(p.resolve("bash", "shx-c"), Some(Rule::Deny), "? matches one char");
        assert_eq!(p.resolve("bash", "shxy-c"), None, "? is exactly one char");
    }

    #[test]
    fn env_files_denied_by_default_except_example() {
        assert_eq!(Permissions::secret_guard(".env"), Some(Rule::Deny));
        assert_eq!(Permissions::secret_guard(".env.local"), Some(Rule::Deny));
        assert_eq!(Permissions::secret_guard("src/.env.production"), Some(Rule::Deny));
        assert_eq!(Permissions::secret_guard(".env.example"), None);
        assert_eq!(Permissions::secret_guard("main.rs"), None);
    }

    #[test]
    fn empty_section_deserializes() {
        let p = perms("");
        assert_eq!(p.resolve("bash", "anything"), None);
    }

    /// Through the real Config nesting: [permission.bash].
    #[test]
    fn nested_config_sections_parse() {
        let cfg: crate::config::Config =
            toml::from_str("[permission.bash]\n\"git *\" = \"allow\"").unwrap();
        assert_eq!(cfg.permission.resolve("bash", "git log"), Some(Rule::Allow));
        // Custom agents round-trip too.
        let cfg: crate::config::Config = toml::from_str(
            "[agents.security]\nprompt = \"audit deps\"\nread_only = true",
        )
        .unwrap();
        assert_eq!(cfg.agents["security"].prompt, "audit deps");
        assert!(cfg.agents["security"].read_only);
    }
}
