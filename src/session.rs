use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use crate::api::Message;

const SESSION_SCHEMA_VERSION: u32 = 2;

/// A named snapshot of the conversation, used as a branch point. The
/// snapshot is an immutable copy of the message list at the moment it was
/// taken, so later turns can never mutate it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub created_unix: u64,
    /// How many messages were captured (system prompt excluded).
    #[serde(default)]
    pub message_count: usize,
    pub messages: Vec<Message>,
}

/// A persisted conversation, auto-saved to the local data directory.
/// Sessions carry a unique id (`<unix>-<rand>`) so they can be resumed
/// explicitly (`laudacode resume <id>` or `/resume` in the TUI).
#[derive(Debug, Serialize, Deserialize)]
pub struct Session {
    #[serde(default = "session_schema_version")]
    pub schema_version: u32,
    pub id: String,
    #[serde(default)]
    pub created_unix: u64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub messages: Vec<Message>,
    /// Branch points recorded with `/checkpoint`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<Checkpoint>,
    /// Checkpoint this session was branched from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branched_from: Option<String>,
    /// Cumulative billable prompt tokens for the session, so `[limits]`
    /// ceilings and `/status` do not silently reset to zero on resume.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub prompt_tokens: u64,
    /// Cumulative completion tokens for the session.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub completion_tokens: u64,
}

fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

fn session_schema_version() -> u32 {
    SESSION_SCHEMA_VERSION
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains('\0')
}

fn normalize_session_ref(value: &str) -> String {
    value
        .trim()
        .trim_end_matches('…')
        .strip_suffix(".json")
        .unwrap_or_else(|| value.trim().trim_end_matches('…'))
        .to_string()
}

impl Session {
    pub fn new() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            id: format!("{}-{}", now, uuid_short()),
            created_unix: now,
            name: None,
            messages: Vec::new(),
            checkpoints: Vec::new(),
            branched_from: None,
            prompt_tokens: 0,
            completion_tokens: 0,
        }
    }

    /// Assign (or replace) the session's friendly name and persist it.
    pub fn set_name(&mut self, name: String) -> Result<()> {
        let name = name.trim().to_string();
        self.name = if name.is_empty() { None } else { Some(name) };
        self.save()
    }

    /// Snapshot the current conversation as a branch point and persist it.
    /// The snapshot never changes afterwards, so it stays a valid restore
    /// target no matter how far the live session moves on.
    pub fn create_checkpoint(&mut self, label: Option<String>) -> Result<Checkpoint> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let label = label
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty());
        let cp = Checkpoint {
            id: format!("{}-{}", now, uuid_short()),
            label,
            created_unix: now,
            message_count: self.messages.len(),
            messages: self.messages.clone(),
        };
        self.checkpoints.push(cp.clone());
        self.save()?;
        Ok(cp)
    }

    /// Find a checkpoint by full id, unique id prefix, or 1-based position
    /// (as shown by `/checkpoints`).
    pub fn find_checkpoint(&self, id_or_index: &str) -> Option<&Checkpoint> {
        let key = id_or_index.trim().trim_start_matches('#');
        if key.is_empty() {
            return None;
        }
        if let Ok(n) = key.parse::<usize>() {
            if n >= 1 && n <= self.checkpoints.len() {
                return self.checkpoints.get(n - 1);
            }
        }
        if let Some(cp) = self.checkpoints.iter().find(|c| c.id == key) {
            return Some(cp);
        }
        // Unique-prefix search (ids are `<unix>-<rand>`).
        let mut hit: Option<&Checkpoint> = None;
        for cp in &self.checkpoints {
            if cp.id.starts_with(key) {
                if hit.is_some() {
                    return None; // ambiguous
                }
                hit = Some(cp);
            }
        }
        hit
    }

    /// Branch a new session from a checkpoint. The new session is persisted
    /// and returned; the original session is left untouched. Works on an
    /// in-memory copy so callers can branch without saving first.
    pub fn branch_from(&self, id_or_index: &str) -> Result<Session> {
        let cp = self
            .find_checkpoint(id_or_index)
            .with_context(|| {
                format!(
                    "no checkpoint matching '{id_or_index}' in session {}",
                    self.id
                )
            })?
            .clone();
        let mut branched = Session::new();
        branched.messages = cp.messages.clone();
        branched.branched_from = Some(format!("{}#{}", self.id, cp.id));
        branched.name = cp.label.clone().map(|l| format!("{l} (branch)"));
        branched.save()?;
        Ok(branched)
    }

    /// One picker row per checkpoint: id first, so a selection can be fed
    /// straight back to `branch_from`. Shared by `/checkpoints` (picker) and
    /// `checkpoint_list` (plain text) so the two never drift.
    pub fn checkpoint_items(&self) -> Vec<String> {
        self.checkpoints
            .iter()
            .enumerate()
            .map(|(i, cp)| {
                format!(
                    "#{} {} · {} msg{} · {}{}",
                    i + 1,
                    cp.id,
                    cp.message_count,
                    if cp.message_count == 1 { "" } else { "s" },
                    fmt_day(cp.created_unix),
                    cp.label
                        .as_deref()
                        .map(|l| format!(" · {l}"))
                        .unwrap_or_default()
                )
            })
            .collect()
    }

    /// One-line-per-checkpoint listing for the CLI.
    pub fn checkpoint_list(&self) -> String {
        if self.checkpoints.is_empty() {
            return "no checkpoints yet — create one with /checkpoint [label]".into();
        }
        self.checkpoint_items().join("\n")
    }

    /// Restore a session's conversation (skipping its system prompt —
    /// the caller re-seeds a fresh system message).
    pub fn restore(&self) -> Vec<Message> {
        self.messages.clone()
    }

    /// `~/.local/share/laudacode` — the root for everything we persist
    /// outside the config file (sessions).
    pub fn data_dir() -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("laudacode")
    }

    pub fn dir() -> PathBuf {
        // Override keeps tests away from real user data.
        if let Ok(p) = std::env::var("LAUDACODE_SESSIONS_DIR") {
            return PathBuf::from(p);
        }
        Self::data_dir().join("sessions")
    }

    pub fn path_for(id: &str) -> PathBuf {
        Self::dir().join(format!("{id}.json"))
    }

    /// Load one session by its unique id.
    pub fn load(id: &str) -> Result<Self> {
        if !valid_session_id(id) {
            bail!("invalid session id '{id}'");
        }
        let raw = fs::read_to_string(Self::path_for(id))
            .with_context(|| format!("loading session '{id}'"))?;
        let session: Self =
            serde_json::from_str(&raw).with_context(|| format!("parsing session '{id}'"))?;
        if session.id != id {
            bail!("session file '{id}' contains id '{}' instead", session.id);
        }
        Ok(session)
    }

    /// Look up a session whose friendly `name` matches (case-insensitive).
    pub fn load_by_name(name: &str) -> Option<Self> {
        let needle = name.trim().to_lowercase();
        if needle.is_empty() {
            return None;
        }
        let mut entries: Vec<_> = fs::read_dir(Self::dir()).ok()?.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&p) else {
                continue;
            };
            if let Ok(sess) = serde_json::from_str::<Session>(&raw) {
                if sess
                    .name
                    .as_deref()
                    .map(|n| n.trim().to_lowercase() == needle)
                    .unwrap_or(false)
                {
                    return Some(sess);
                }
            }
        }
        None
    }

    /// Resolve a session from an exact id, a unique id prefix, or its name.
    pub fn resolve(id_or_name: &str) -> Option<Self> {
        let key = normalize_session_ref(id_or_name);
        if key.is_empty() {
            return None;
        }
        if let Ok(session) = Self::load(&key) {
            return Some(session);
        }

        let mut prefix_match: Option<Self> = None;
        if key.len() >= 6 {
            let mut entries: Vec<_> = fs::read_dir(Self::dir()).ok()?.flatten().collect();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let Ok(raw) = fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(session) = serde_json::from_str::<Session>(&raw) else {
                    continue;
                };
                if session.id.starts_with(&key) {
                    if prefix_match.is_some() {
                        prefix_match = None;
                        break;
                    }
                    prefix_match = Some(session);
                }
            }
        }
        if prefix_match.is_some() {
            return prefix_match;
        }
        Self::load_by_name(&key)
    }

    /// Delete a session by id, prefix, or name. Returns what was removed.
    pub fn delete(id_or_name: &str) -> Result<Option<String>> {
        let Some(session) = Self::resolve(id_or_name) else {
            return Ok(None);
        };
        let path = Self::path_for(&session.id);
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("removing session '{}'", session.id))?;
        }
        Ok(Some(session.id))
    }

    /// Parked sessions live here: invisible to resume/search (the scans only
    /// read top-level `.json` files) until moved back.
    pub fn archived_dir() -> PathBuf {
        Self::dir().join("archived")
    }

    /// Move a session aside by id, prefix, or name. Returns its id.
    pub fn archive(id_or_name: &str) -> Result<Option<String>> {
        let Some(session) = Self::resolve(id_or_name) else {
            return Ok(None);
        };
        let archived = Self::archived_dir();
        fs::create_dir_all(&archived).context("creating archive dir")?;
        fs::rename(Self::path_for(&session.id), archived.join(format!("{}.json", session.id)))
            .with_context(|| format!("archiving session '{}'", session.id))?;
        Ok(Some(session.id))
    }

    /// Ids + names of parked sessions, newest first.
    pub fn list_archived() -> Vec<(String, Option<String>)> {
        let mut out: Vec<(u64, String, Option<String>)> = Vec::new();
        for entry in fs::read_dir(Self::archived_dir()).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(raw) = fs::read_to_string(&p) {
                if let Ok(sess) = serde_json::from_str::<Session>(&raw) {
                    out.push((sess.created_unix, sess.id, sess.name));
                }
            }
        }
        out.sort_by_key(|a| std::cmp::Reverse(a.0));
        out.into_iter().map(|(_, id, name)| (id, name)).collect()
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
        if !valid_session_id(&self.id) {
            bail!("invalid session id '{}'", self.id);
        }
        let dir = Self::dir();
        fs::create_dir_all(&dir).context("creating sessions dir")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
                .context("restricting sessions directory permissions")?;
        }
        let path = Self::path_for(&self.id);
        let tmp = dir.join(format!(".{}.{}.tmp", self.id, uuid_short()));
        let result = (|| -> Result<()> {
            let raw = serde_json::to_string_pretty(self)?;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .with_context(|| format!("creating temporary session file {}", tmp.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(0o600))
                    .context("restricting temporary session permissions")?;
            }
            file.write_all(raw.as_bytes())
                .context("writing session data")?;
            file.sync_all().context("syncing session data")?;
            fs::rename(&tmp, &path)
                .with_context(|| format!("replacing session {}", path.display()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
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
        out.into_iter()
            .map(|(t, id, name, p)| (id, name, t, p))
            .collect()
    }

    /// Load the most recent session (for `--continue`). Ties on created_unix
    /// are broken by id so same-second saves resolve newest-first.
    pub fn latest() -> Option<Session> {
        let dir = Self::dir();
        let mut best: Option<(u64, String, Session)> = None;
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&p) else {
                continue;
            };
            let Ok(session) = serde_json::from_str::<Session>(&raw) else {
                continue;
            };
            let newer = match &best {
                Some((created, id, _)) => {
                    session.created_unix > *created
                        || (session.created_unix == *created && session.id > *id)
                }
                None => true,
            };
            if newer {
                best = Some((session.created_unix, session.id.clone(), session));
            }
        }
        best.map(|(_, _, session)| session)
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

/// `YYYY-MM-DD` for a unix timestamp (days-from-civil, proleptic Gregorian).
/// Kept local so session listings stay self-contained.
fn fmt_day(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y}-{m:02}-{d:02}")
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
    fn checkpoints_snapshot_and_branch_without_mutating_the_original() {
        let (_g, dir) = test_env();
        let mut s = Session::new();
        s.messages.push(Message::user("first"));
        s.save().unwrap();
        let cp1 = s.create_checkpoint(Some("before refactor".into())).unwrap();
        // Keep going after the checkpoint.
        s.messages.push(Message::assistant("second"));
        s.save().unwrap();

        // The snapshot is immutable: it still holds only "first".
        assert_eq!(cp1.messages.len(), 1);
        assert_eq!(cp1.message_count, 1);
        assert_eq!(cp1.label.as_deref(), Some("before refactor"));

        // Branch by index, by id and by unique prefix — all same point.
        for key in ["1", &cp1.id, &cp1.id[..8], "#1"] {
            let b = s
                .branch_from(key)
                .unwrap_or_else(|e| panic!("branch {key}: {e:#}"));
            assert_eq!(b.messages.len(), 1, "branch {key} should hold the snapshot");
            assert!(b.branched_from.as_deref().unwrap().contains(&cp1.id));
            // The branch is persisted and independently loadable.
            let reloaded = Session::load(&b.id).unwrap();
            assert_eq!(reloaded.messages.len(), 1);
            // The original is untouched.
            assert_eq!(s.messages.len(), 2, "original must keep both messages");
        }
        // Unknown refs fail cleanly.
        assert!(s.branch_from("999").is_err());
        assert!(s.branch_from("nope").is_err());
        // Empty label is normalized away.
        let cp2 = s.create_checkpoint(Some("   ".into())).unwrap();
        assert!(cp2.label.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn checkpoints_survive_a_reload() {
        let (_g, dir) = test_env();
        let mut s = Session::new();
        s.messages.push(Message::user("hello"));
        s.create_checkpoint(Some("cp".into())).unwrap();
        let loaded = Session::load(&s.id).unwrap();
        assert_eq!(loaded.checkpoints.len(), 1);
        assert_eq!(loaded.checkpoints[0].label.as_deref(), Some("cp"));
        assert!(loaded.checkpoint_list().contains("cp"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn old_sessions_without_checkpoints_still_load() {
        let (_g, dir) = test_env();
        // A pre-checkpoint (v1) session file: no checkpoints key at all.
        let raw = serde_json::json!({
            "id": "1700000000-legacy",
            "created_unix": 1_700_000_000u64,
            "messages": [{"role": "user", "content": "old"}]
        })
        .to_string();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(Session::path_for("1700000000-legacy"), raw).unwrap();
        let s = Session::load("1700000000-legacy").unwrap();
        assert!(s.checkpoints.is_empty());
        assert!(s.branched_from.is_none());
        assert!(s.checkpoint_list().contains("no checkpoints yet"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn session_ids_are_unique_within_a_millisecond() {
        let a = Session::new();
        let b = Session::new();
        assert_ne!(a.id, b.id, "ids built from distinct nanos must differ");
        assert!(a.id.contains('-'));
    }

    #[test]
    fn archive_parks_and_hides_from_resume() {
        let (_g, _dir) = test_env();
        let mut s = Session::new();
        s.messages.push(Message::user("park me"));
        s.save().unwrap();
        let id = s.id.clone();
        assert!(Session::resolve(&id).is_some());
        assert_eq!(Session::archive(&id).unwrap().as_deref(), Some(id.as_str()));
        // Gone from resume/search, listed under archived instead.
        assert!(Session::resolve(&id).is_none());
        assert!(Session::list_recent(10).iter().all(|(i, _, _, _)| *i != id));
        let archived = Session::list_archived();
        assert!(archived.iter().any(|(i, _)| *i == id), "{archived:?}");
        std::fs::remove_dir_all(Session::dir()).ok();
    }

    #[test]
    fn usage_totals_survive_a_save_and_load() {
        // Before this, a resume reset the token counters to zero, which handed
        // the user a fresh `max_cost_usd` allowance every time they resumed.
        let (_g, _dir) = test_env();
        let mut s = Session::new();
        s.prompt_tokens = 12_345;
        s.completion_tokens = 678;
        s.save().unwrap();
        let loaded = Session::load(&s.id).expect("loads");
        assert_eq!(loaded.prompt_tokens, 12_345);
        assert_eq!(loaded.completion_tokens, 678);
    }

    #[test]
    fn sessions_written_before_usage_existed_still_load() {
        // Both fields are `#[serde(default)]`, so an older session file is
        // valid input rather than a parse failure that loses the transcript.
        let v = serde_json::json!({
            "id": "1-abc",
            "messages": [{"role": "user", "content": "hi"}],
        });
        let s: Session = serde_json::from_value(v).expect("legacy session must parse");
        assert_eq!((s.prompt_tokens, s.completion_tokens), (0, 0));
    }

    #[test]
    fn save_and_load_roundtrip_is_isolated_from_real_data() {
        let (_g, dir) = test_env();
        let mut s = Session::new();
        s.messages.push(Message::user("hello world"));
        s.save().unwrap();

        // Loaded by unique id…
        let loaded = Session::load(&s.id).expect("session should load by id");
        assert!(loaded
            .messages
            .iter()
            .any(|m| m.content.as_deref() == Some("hello world")));
        // …and appears in the recent list for /resume.
        let recent = Session::list_recent(10);
        assert!(recent.iter().any(|(id, _, _, _)| *id == s.id));
        assert!(
            recent[0].3.contains("hello world"),
            "preview should show prompt"
        );

        // Nothing leaked into the default location.
        assert!(!dirs::data_dir()
            .map(|d| d
                .join("laudacode/sessions")
                .join(format!("{}.json", s.id))
                .exists())
            .unwrap_or(false));
        std::fs::remove_dir_all(&dir).ok();
    }
}
