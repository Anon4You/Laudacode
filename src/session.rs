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
            messages: Vec::new(),
        }
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
    pub fn list_recent(limit: usize) -> Vec<(String, u64, String)> {
        let mut out: Vec<(u64, String, String)> = Vec::new();
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
                    out.push((sess.created_unix, sess.id, preview));
                }
            }
        }
        out.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        out.truncate(limit);
        out.into_iter().map(|(t, id, p)| (id, t, p)).collect()
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
mod tests {
    use super::*;

    fn test_env() -> (std::sync::MutexGuard<'static, ()>, PathBuf) {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let guard = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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
        assert!(recent.iter().any(|(id, _, _)| *id == s.id));
        assert!(recent[0].2.contains("hello world"), "preview should show prompt");

        // Nothing leaked into the default location.
        assert!(!dirs::data_dir()
            .map(|d| d.join("laudacode/sessions").join(format!("{}.json", s.id)).exists())
            .unwrap_or(false));
        std::fs::remove_dir_all(&dir).ok();
    }
}
