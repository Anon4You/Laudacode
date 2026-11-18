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

/// Ordered map of glob pattern -> rule for one tool. Order matters: later
/// wins, so this is a `Vec` rather than a `BTreeMap` (whose sort order is
/// alphabetical and would silently reverse the documented contract).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PatternMap(pub Vec<(String, Rule)>);

impl PatternMap {
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Rule)> {
        self.0.iter().map(|(pat, rule)| (pat.as_str(), rule))
    }
}

impl<const N: usize> From<[(String, Rule); N]> for PatternMap {
    fn from(entries: [(String, Rule); N]) -> Self {
        Self(entries.into_iter().collect())
    }
}

impl Serialize for PatternMap {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (pat, rule) in &self.0 {
            map.serialize_entry(pat, rule)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for PatternMap {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = PatternMap;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a table of permission patterns to rules")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<PatternMap, A::Error> {
                let mut entries = Vec::new();
                while let Some((pat, rule)) = map.next_entry::<String, Rule>()? {
                    entries.push((pat, rule));
                }
                Ok(PatternMap(entries))
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

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
    /// MCP tools keyed by qualified name (`mcp__<server>__<tool>`). MCP tools
    /// are third-party code, so this is the only way to deny or prompt for one
    /// without giving up on the server entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<PatternMap>,
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
    /// - mcp: the qualified tool name
    pub fn resolve(&self, tool: &str, input: &str) -> Option<Rule> {
        self.resolve_with_pattern(tool, input).map(|(_, rule)| rule)
    }

    /// Like [`resolve`](Self::resolve), but also returns the pattern that won.
    /// Callers need it to tell an explicit allow from a blanket `"*"`.
    pub fn resolve_with_pattern(&self, tool: &str, input: &str) -> Option<(&str, Rule)> {
        let map = match tool {
            "bash" => self.bash.as_ref()?,
            "edit" => self.edit.as_ref()?,
            "read" => self.read.as_ref()?,
            "webfetch" => self.webfetch.as_ref()?,
            "mcp" => self.mcp.as_ref()?,
            _ => return None,
        };
        // Last matching pattern wins (documented contract).
        let mut matched: Option<(&str, Rule)> = None;
        for (pat, rule) in map.iter() {
            if glob_match(pat, input) {
                matched = Some((pat, *rule));
            }
        }
        matched
    }

    /// The first path that looks like a secret and is not allowed by a
    /// non-blanket read rule. A bare `"*" = "allow"` must not expose `.env`;
    /// `".env" = "allow"` or `"*.env" = "allow"` does.
    pub fn secret_blocked<'a>(&self, paths: &'a [String]) -> Option<&'a str> {
        for p in paths {
            if Self::secret_guard(p) != Some(Rule::Deny) {
                continue;
            }
            // Per-path, not per-action: an allow for `src/.env` must not also
            // expose a root `.env` touched by the same patch.
            let explicitly_allowed = matches!(
                self.resolve_with_pattern("read", p),
                Some((pat, Rule::Allow)) if pat != "*" && pat != "**"
            );
            if !explicitly_allowed {
                return Some(p);
            }
        }
        None
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
    fn later_wildcard_beats_an_earlier_specific_pattern() {
        // Alphabetical (BTreeMap) order would keep the earlier `z*` deny last.
        let p = perms("[read]\n\"z*\" = \"deny\"\n\"*\" = \"allow\"\n");
        assert_eq!(p.resolve("read", "zeta"), Some(Rule::Allow));
        let round = perms(&toml::to_string(&p).unwrap());
        assert_eq!(round.resolve("read", "zeta"), Some(Rule::Allow));
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
"*" = "ask"
"https://docs.example.com/*" = "allow"
"#,
        );
        assert_eq!(p.resolve("edit", "src/main.rs"), Some(Rule::Allow));
        assert_eq!(p.resolve("edit", "/etc/passwd"), Some(Rule::Deny));
        assert_eq!(p.resolve("read", "anything.txt"), Some(Rule::Allow));
        assert_eq!(
            p.resolve("webfetch", "https://docs.example.com/api"),
            Some(Rule::Allow)
        );
        assert_eq!(
            p.resolve("webfetch", "https://evil.example.net"),
            Some(Rule::Ask)
        );
    }

    #[test]
    fn question_mark_matches_single_char() {
        let p = perms(
            r#"
[bash]
"sh?-c" = "deny"
"#,
        );
        assert_eq!(
            p.resolve("bash", "shx-c"),
            Some(Rule::Deny),
            "? matches one char"
        );
        assert_eq!(p.resolve("bash", "shxy-c"), None, "? is exactly one char");
    }

    #[test]
    fn env_files_denied_by_default_except_example() {
        assert_eq!(Permissions::secret_guard(".env"), Some(Rule::Deny));
        assert_eq!(Permissions::secret_guard(".env.local"), Some(Rule::Deny));
        assert_eq!(
            Permissions::secret_guard("src/.env.production"),
            Some(Rule::Deny)
        );
        assert_eq!(Permissions::secret_guard(".env.example"), None);
        assert_eq!(Permissions::secret_guard("main.rs"), None);
    }

    #[test]
    fn secret_allow_is_per_path() {
        let p = perms("[read]\n\"src/.env\" = \"allow\"");
        let paths = vec!["src/.env".to_string(), ".env".to_string()];
        assert_eq!(p.secret_blocked(&paths), Some(".env"));
        assert_eq!(p.secret_blocked(&["src/.env".to_string()]), None);

        let blanket = perms("[read]\n\"*\" = \"allow\"");
        assert_eq!(blanket.secret_blocked(&[".env".to_string()]), Some(".env"));
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
        let cfg: crate::config::Config =
            toml::from_str("[agents.security]\nprompt = \"audit deps\"\nread_only = true").unwrap();
        assert_eq!(cfg.agents["security"].prompt, "audit deps");
        assert!(cfg.agents["security"].read_only);
    }
}
