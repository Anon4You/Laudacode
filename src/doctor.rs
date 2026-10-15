//! `/doctor`: environment diagnostics for setup debugging.
//!
//! No network, no tokens spent. Provider health is reported from config
//! (endpoint, model, key source) — actually probing it would burn tokens,
//! so that stays a manual step.

use std::path::PathBuf;

/// True when `bin` resolves through PATH. No spawn, so it is safe to call
/// for every tool on every `/doctor` run.
pub fn have(bin: &str) -> bool {
    if bin.contains('/') || bin.contains('\\') {
        return PathBuf::from(bin).is_file();
    }
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat", ""] } else { &[""] };
    path.split(':').any(|dir| {
        exts.iter().any(|ext| {
            let p = PathBuf::from(dir).join(format!("{bin}{ext}"));
            p.is_file()
        })
    })
}

/// True on Termux/Android (`sh -c` instead of bash, `$TMPDIR` not `/tmp`).
pub fn is_termux() -> bool {
    std::env::var("TERMUX_VERSION").is_ok()
}

fn ok(b: bool) -> &'static str {
    if b {
        "ok"
    } else {
        "MISSING"
    }
}

/// One `(label, value)` per line; the worker prefixes app-specific rows
/// (provider, MCP/LSP) that need live state.
pub fn env_rows() -> Vec<(String, String)> {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".into());
    let sessions = crate::session::Session::dir();
    let sessions_state = if sessions.is_dir() {
        "ok"
    } else if sessions.exists() {
        "exists but not a dir"
    } else {
        "not created yet (first save makes it)"
    };
    vec![
        ("binary".into(), format!("{} ({})", env!("CARGO_PKG_VERSION"), exe)),
        ("config".into(), crate::config::Config::toml_path().display().to_string()),
        ("sessions".into(), format!("{} · {}", sessions.display(), sessions_state)),
        ("platform".into(), platform()),
        (
            "tools".into(),
            format!(
                "git:{} tar:{} sha256:{} curl:{}",
                ok(have("git")),
                ok(have("tar")),
                ok(have("sha256sum") || have("shasum")),
                ok(have("curl")),
            ),
        ),
    ]
}

fn platform() -> String {
    let mut s = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    if is_termux() {
        s.push_str(" · termux");
    }
    if let Ok(tmp) = std::env::var("TMPDIR") {
        s.push_str(&format!(" · TMPDIR={tmp}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn have_finds_path_tools_without_spawning() {
        // `sh` exists on every unix this ships on; a nonsense name never does.
        assert!(have("sh") || cfg!(windows));
        assert!(!have("definitely-not-a-real-binary-xyz"));
        // Explicit paths are checked directly, never through PATH.
        assert!(!have("/nonexistent-dir-xyz/nope"));
    }

    #[test]
    fn env_rows_always_cover_the_basics() {
        let rows: Vec<String> = env_rows().into_iter().map(|(k, _)| k).collect();
        for key in ["binary", "config", "sessions", "platform", "tools"] {
            assert!(rows.contains(&key.to_string()), "missing row: {key}");
        }
    }

    #[test]
    fn termux_detection_follows_the_env_marker() {
        // Cannot flip the real env safely in parallel tests; just assert the
        // function agrees with a direct lookup.
        assert_eq!(is_termux(), std::env::var("TERMUX_VERSION").is_ok());
    }
}
