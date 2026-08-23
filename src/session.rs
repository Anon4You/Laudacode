use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::api::Message;

/// A persisted conversation, auto-saved to the local data directory.
#[derive(Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub created_unix: u64,
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
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("laudacode")
            .join("sessions")
    }

    pub fn path_for(id: &str) -> PathBuf {
        Self::dir().join(format!("{id}.json"))
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::dir();
        fs::create_dir_all(&dir).context("creating sessions dir")?;
        let path = Self::path_for(&self.id);
        let raw = serde_json::to_string_pretty(self)?;
        fs::write(path, raw)?;
        Ok(())
    }

    /// Load the most recent session (for `--continue`).
    pub fn latest() -> Option<Session> {
        let dir = Self::dir();
        let mut best: Option<(u64, PathBuf)> = None;
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(s) = fs::read_to_string(&p) {
                if let Ok(sess) = serde_json::from_str::<Session>(&s) {
                    if best.as_ref().map(|(t, _)| sess.created_unix > *t).unwrap_or(true) {
                        best = Some((sess.created_unix, p));
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
    // Cheap unique suffix without pulling uuid's v4 RNG path twice.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ d.as_secs() as u32)
        .unwrap_or(0);
    format!("{nanos:08x}")
}
