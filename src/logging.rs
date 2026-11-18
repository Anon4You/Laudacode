//! Optional structured (JSONL) run log.
//!
//! Off by default. When `[logging] enabled = true` with a `file`, every
//! notable event is appended as one JSON object per line, which is easy to
//! `grep`/`jq` and cheap to tail. Never logs message bodies, API keys or
//! file contents — only metadata (tool names, token counts, outcomes).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::style::Stylize;
use serde_json::json;

use crate::config::Logging;

/// Expand a leading `~` to the user's home directory so config files can use
/// the familiar shell-style shorthand. Only a bare `~` or `~/...` expands —
/// `~user` would need a passwd lookup. Anything unresolvable is left as-is
/// rather than silently writing into a directory literally named "~".
fn expand_tilde(path: &str) -> String {
    let rest = match path.strip_prefix('~') {
        Some("") => "",
        Some(r) if r.starts_with('/') || r.starts_with('\\') => r,
        _ => return path.to_string(),
    };
    match dirs::home_dir() {
        Some(home) => format!("{}{rest}", home.display()),
        None => path.to_string(),
    }
}

pub struct Logger {
    file: Option<Mutex<File>>,
    stderr: bool,
}

impl Logger {
    /// Build a logger from config. A bad path degrades to stderr instead of
    /// taking the session down — logging must never break the agent.
    pub fn from_config(cfg: &Logging) -> Self {
        if !cfg.is_active() {
            return Self {
                file: None,
                stderr: false,
            };
        }
        // No file configured means stderr; `stderr = true` additionally
        // mirrors the file. `enabled` is already known true here.
        let to_stderr = cfg.stderr || !cfg.has_file();
        let file = if cfg.has_file() {
            let raw = cfg.file.as_deref().unwrap_or("").trim();
            let expanded = expand_tilde(raw);
            let p = if Path::new(&expanded).is_absolute() {
                Path::new(&expanded).to_path_buf()
            } else {
                std::env::current_dir().unwrap_or_default().join(&expanded)
            };
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let mut opts = OpenOptions::new();
            opts.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            match opts.open(&p) {
                Ok(f) => {
                    // `mode` only applies on create; tighten an existing file too.
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
                    }
                    Some(Mutex::new(f))
                }
                Err(e) => {
                    // Never fail the session over the log file, but do say so.
                    eprintln!(
                        "{} [logging] cannot open {}: {e} — falling back to stderr",
                        "warning:".yellow().bold(),
                        p.display()
                    );
                    None
                }
            }
        } else {
            None
        };
        Self {
            file,
            stderr: to_stderr,
        }
    }

    pub fn is_active(&self) -> bool {
        self.file.is_some() || self.stderr
    }

    /// Append one event. `fields` are merged into the top-level object.
    pub fn event(&self, kind: &str, fields: serde_json::Value) {
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let mut obj = json!({ "ts_ms": ts_ms, "event": kind });
        if let (Some(map), serde_json::Value::Object(extra)) = (obj.as_object_mut(), fields) {
            for (k, v) in extra {
                map.insert(k, v);
            }
        }
        let line = obj.to_string();
        if self.stderr {
            eprintln!("{line}");
        }
        if let Some(f) = &self.file {
            if let Ok(mut file) = f.lock() {
                let _ = writeln!(file, "{line}");
            }
        }
    }

    pub fn tool(&self, name: &str, ok: bool) {
        // Tool output is file contents or command output and can contain
        // secrets; the outcome and name are all that belongs in a run log.
        self.event("tool", json!({ "tool": name, "ok": ok }));
    }

    /// `prompt`/`completion` are cumulative session totals; `cost` is the
    /// session cost for the model those totals were billed at.
    pub fn usage(&self, model: &str, prompt: u64, completion: u64, cost: f64) {
        self.event(
            "usage",
            json!({
                "model": model,
                "prompt_tokens": prompt,
                "completion_tokens": completion,
                "est_cost_usd": cost,
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_logger_is_inert() {
        let l = Logger::from_config(&Logging::default());
        assert!(!l.is_active());
        l.tool("grep", true); // must not panic or write anywhere
    }

    #[test]
    fn writes_jsonl_and_never_secret_fields() {
        let dir = std::env::temp_dir().join(format!("lc-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("run.jsonl");
        let cfg = Logging {
            enabled: true,
            file: Some(path.display().to_string()),
            stderr: false,
        };
        let l = Logger::from_config(&cfg);
        assert!(l.is_active());
        l.usage("gpt-x", 100, 20, 0.09);
        l.tool("read_file", true);
        let raw = std::fs::read_to_string(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "logs may contain command summaries");
        }
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["event"], "usage");
        assert_eq!(first["prompt_tokens"], 100);
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["tool"], "read_file");
        assert!(
            second.get("detail").is_none(),
            "tool output must not be logged: {second}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bad_path_degrades_quietly() {
        let cfg = Logging {
            enabled: true,
            // A directory can never be opened for append.
            file: Some("/definitely/not/a/dir/run.jsonl".into()),
            stderr: false,
        };
        let l = Logger::from_config(&cfg);
        l.event("noop", json!({})); // must not panic
    }

    #[test]
    fn enabled_without_a_file_logs_to_stderr() {
        // `enabled = true` with no `file` used to be silently inert.
        let cfg = Logging {
            enabled: true,
            file: None,
            stderr: false,
        };
        assert!(cfg.is_active());
        let l = Logger::from_config(&cfg);
        assert!(l.is_active());
        assert!(l.file.is_none(), "no file sink expected");
        l.event("noop", json!({})); // writes to stderr, must not panic
    }

    #[test]
    fn disabled_even_with_a_file_stays_off() {
        let cfg = Logging {
            enabled: false,
            file: Some("/tmp/should-never-be-created.jsonl".into()),
            stderr: true,
        };
        assert!(!cfg.is_active());
        let l = Logger::from_config(&cfg);
        assert!(!l.is_active());
        assert!(!std::path::Path::new("/tmp/should-never-be-created.jsonl").exists());
    }

    #[test]
    fn blank_file_falls_back_to_stderr() {
        let cfg = Logging {
            enabled: true,
            file: Some("   ".into()),
            stderr: false,
        };
        assert!(!cfg.has_file());
        assert!(cfg.is_active());
        assert!(Logger::from_config(&cfg).file.is_none());
    }

    #[test]
    fn tilde_expands_to_home() {
        let home = dirs::home_dir().expect("home dir");
        assert_eq!(expand_tilde("~"), home.to_string_lossy());
        assert_eq!(
            expand_tilde("~/.config/laudacode/run.jsonl"),
            home.join(".config/laudacode/run.jsonl").to_string_lossy()
        );
        // `~user` needs a passwd lookup and relative paths are left alone.
        assert_eq!(expand_tilde("~other/x"), "~other/x");
        assert_eq!(expand_tilde("relative/x.jsonl"), "relative/x.jsonl");
        assert_eq!(expand_tilde("/abs/x.jsonl"), "/abs/x.jsonl");
    }
}
