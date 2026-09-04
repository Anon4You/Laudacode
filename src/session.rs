use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::api::Message;

/// A persisted conversation, auto-saved to the local data directory.
/// Sessions carry a unique id (`<unix>-<rand>`) so they can be resumed
/// explicitly (`laudacode resume <id>` or `/resume` in the TUI).
#[derive(Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub created_unix: u64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub messages: Vec<Message>,
}

impl Session {
    pub fn new() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            id: format!("{}-{}", now, uuid_short()),
            created_unix: now,
            name: None,
            messages: Vec::new(),
        }
    }

    /// Assign (or replace) the session's friendly name and persist it.
    pub fn set_name(&mut self, name: String) -> Result<()> {
        let name = name.trim().to_string();
        self.name = if name.is_empty() { None } else { Some(name) };
        self.save()
    }

    /// Restore a session's conversation (skipping its system prompt —
    /// the caller re-seeds a fresh system message).
    pub fn restore(&self) -> Vec<Message> {
        self.messages.clone()
    }

    pub fn dir() -> PathBuf {
        // Override keeps tests away from real user data.
        if let Ok(p) = std::env::var("LAUDACODE_SESSIONS_DIR") {
            return PathBuf::from(p);
        }
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("laudacode")
            .join("sessions")
    }

    pub fn path_for(id: &str) -> PathBuf {
        Self::dir().join(format!("{id}.json"))
    }

    /// Load one session by its unique id.
    pub fn load(id: &str) -> Result<Self> {
        let raw = fs::read_to_string(Self::path_for(id))
            .with_context(|| format!("loading session '{id}'"))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing session '{id}'"))
    }

    /// Look up a session whose friendly `name` matches (case-insensitive).
    pub fn load_by_name(name: &str) -> Option<Self> {
        let needle = name.trim().to_lowercase();
        if needle.is_empty() {
            return None;
        }
        for entry in fs::read_dir(Self::dir()).ok()?.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let raw = fs::read_to_string(&p).ok()?;
            if let Ok(sess) = serde_json::from_str::<Session>(&raw) {
                if sess
                    .name
                    .as_deref()
                    .map(|n| n.to_lowercase() == needle)
                    .unwrap_or(false)
                {
                    return Some(sess);
                }
            }
        }
        None
    }

    /// Resolve a session from either its unique id or its friendly name.
    pub fn resolve(id_or_name: &str) -> Option<Self> {
        if let Ok(s) = Self::load(id_or_name) {
            return Some(s);
        }
        Self::load_by_name(id_or_name)
    }

    /// Delete a session by id or name. Returns what was removed.
    pub fn delete(id_or_name: &str) -> Result<Option<String>> {
        // Prefer an exact id match on disk, then a name match.
        let target = if Self::path_for(id_or_name).exists() {
            id_or_name.to_string()
        } else {
            match Self::load_by_name(id_or_name) {
                Some(s) => s.id,
                None => return Ok(None),
            }
        };
        let path = Self::path_for(&target);
        if path.exists() {
            let _ = fs::remove_file(&path);
        }
        Ok(Some(target))
    }

    /// Sessions whose id or name contain `kw` (case-insensitive), newest
    /// first, paired with a preview of the first user prompt.
    pub fn find_by_keyword(kw: &str, limit: usize) -> Vec<(Session, String)> {
        let kw = kw.trim().to_lowercase();
        let mut hits: Vec<(u64, Session, String)> = Vec::new();
        for entry in fs::read_dir(Self::dir()).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(s) = fs::read_to_string(&p) {
                if let Ok(sess) = serde_json::from_str::<Session>(&s) {
                    let id_matches = sess.id.to_lowercase().contains(&kw);
                    let name_matches = sess
                        .name
                        .as_deref()
                        .map(|n| n.to_lowercase().contains(&kw))
                        .unwrap_or(false);
                    if kw.is_empty() || id_matches || name_matches {
                        let preview = sess
                            .messages
                            .iter()
                            .find(|m| m.role == "user")
                            .and_then(|m| m.content.clone())
                            .map(|c| c.replace('\n', " ").chars().take(48).collect::<String>())
                            .unwrap_or_else(|| "(no prompt)".into());
                        hits.push((sess.created_unix, sess, preview));
                    }
                }
            }
        }
        hits.sort_by_key(|a| std::cmp::Reverse(a.0));
        hits.truncate(limit);
        hits.into_iter().map(|(_, s, p)| (s, p)).collect()
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::dir();
        fs::create_dir_all(&dir).context("creating sessions dir")?;
        let path = Self::path_for(&self.id);
        let raw = serde_json::to_string_pretty(self)?;
        // Transcripts can embed sensitive material discussed in-session.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::write(&path, &raw).and_then(|_| {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            });
        }
        #[cfg(not(unix))]
        fs::write(&path, raw)?;
        Ok(())
    }

    /// Most recent sessions, newest first, with a short preview of the first
    /// user prompt for the /resume picker. Skips unreadable entries silently.
    /// Entry: `(id, name, created_unix, preview)`.
    pub fn list_recent(limit: usize) -> Vec<(String, Option<String>, u64, String)> {
        let mut out: Vec<(u64, String, Option<String>, String)> = Vec::new();
        for entry in fs::read_dir(Self::dir()).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(s) = fs::read_to_string(&p) {
                if let Ok(sess) = serde_json::from_str::<Session>(&s) {
                    let preview = sess
                        .messages
                        .iter()
                        .find(|m| m.role == "user")
                        .and_then(|m| m.content.clone())
                        .map(|c| c.replace('\n', " ").chars().take(64).collect::<String>())
                        .unwrap_or_else(|| "(no prompt)".into());
                    out.push((sess.created_unix, sess.id, sess.name, preview));
                }
            }
        }
        out.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        out.truncate(limit);
        out.into_iter().map(|(t, id, name, p)| (id, name, t, p)).collect()
    }

    /// Load the most recent session (for `--continue`). Ties on created_unix
    /// are broken by id so same-second saves resolve newest-first.
    pub fn latest() -> Option<Session> {
        let dir = Self::dir();
        let mut best: Option<(u64, String)> = None;
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(s) = fs::read_to_string(&p) {
                if let Ok(sess) = serde_json::from_str::<Session>(&s) {
                    let newer = match &best {
                        Some((t, id)) => {
                            sess.created_unix > *t
                                || (sess.created_unix == *t && sess.id > *id)
                        }
                        None => true,
                    };
                    if newer {
                        best = Some((sess.created_unix, p.to_string_lossy().to_string()));
                    }
                }
            }
        }
        let (_, path) = best?;
        let raw = fs::read_to_string(path).ok()?;
        serde_json::from_str(&raw).ok()
    }
}

fn uuid_short() -> String {
    // Nanos + pid + ASLR-ish stack address entropy; collisions across
    // simultaneously-starting processes are practically impossible.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u32;
    let stack = &nanos as *const u32 as usize as u32;
    format!("{:08x}{:04x}", nanos ^ stack.rotate_left(7), pid & 0xffff)
}

#[cfg(test)]
pub(crate) mod test_sync {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Serializes tests that mutate process-wide env vars — cargo runs test
    /// threads in parallel and env races made this suite flaky.
    pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_env() -> (std::sync::MutexGuard<'static, ()>, PathBuf) {
        // Shared with every other env-flipping test (see repl::history).
        let guard = test_sync::env_lock();
        let dir = std::env::temp_dir().join(format!(
            "lc-sessions-test-{}-{}",
            std::process::id(),
            uuid_short()
        ));
        std::env::set_var("LAUDACODE_SESSIONS_DIR", &dir);
        (guard, dir)
    }

    #[test]
    fn session_ids_are_unique_within_a_millisecond() {
        let a = Session::new();
        let b = Session::new();
        assert_ne!(a.id, b.id, "ids built from distinct nanos must differ");
        assert!(a.id.contains('-'));
    }

    #[test]
    fn save_and_load_roundtrip_is_isolated_from_real_data() {
        let (_g, dir) = test_env();
        let mut s = Session::new();
        s.messages.push(Message::user("hello world"));
        s.save().unwrap();

        // Loaded by unique id…
        let loaded = Session::load(&s.id).expect("session should load by id");
        assert!(loaded.messages.iter().any(|m| m.content.as_deref() == Some("hello world")));
        // …and appears in the recent list for /resume.
        let recent = Session::list_recent(10);
        assert!(recent.iter().any(|(id, _, _, _)| *id == s.id));
        assert!(recent[0].3.contains("hello world"), "preview should show prompt");

        // Nothing leaked into the default location.
        assert!(!dirs::data_dir()
            .map(|d| d.join("laudacode/sessions").join(format!("{}.json", s.id)).exists())
            .unwrap_or(false));
        std::fs::remove_dir_all(&dir).ok();
    }
}
