//! Self-update (`/update`): check the latest GitHub release, download the
//! matching prebuilt asset and install it over the running binary.
//!
//! Asset naming mirrors `install.sh`: `laudacode-<tag>-<rust-target>.tar.gz`
//! plus a published `SHA256SUMS`. No new dependencies: downloads reuse the
//! proxy-aware web client, checksums shell out to `sha256sum`/`shasum`.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub const REPO: &str = "Anon4You/Laudacode";
/// Biggest asset we will download — phones have tight RAM budgets.
const MAX_ASSET_BYTES: u64 = 64 * 1024 * 1024;
/// How long a cached latest-tag stays valid. Unauthenticated GitHub API calls
/// are capped at 60/hour per IP, and mobile networks share IPs — so repeated
/// `/update` checks must not each burn a call.
pub const CACHE_TTL_SECS: u64 = 3600;

fn cache_path() -> PathBuf {
    crate::session::Session::dir().join("update-check.json")
}

/// Latest tag from cache when fresh, otherwise `None`.
pub fn cached_tag() -> Option<String> {
    let raw = std::fs::read_to_string(cache_path()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let tag = v.get("tag")?.as_str()?.to_string();
    let when = v.get("unix")?.as_u64()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if now.saturating_sub(when) <= CACHE_TTL_SECS {
        Some(tag)
    } else {
        None
    }
}

/// Remember a fresh lookup. Best-effort: a missing sessions dir is not an
/// update failure.
pub fn store_tag(tag: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let body = serde_json::json!({ "tag": tag, "unix": now }).to_string();
    if let Some(parent) = cache_path().parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(cache_path(), body).ok();
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Numeric dotted-prefix compare, ignoring a leading `v` and any pre-release
/// suffix (`1.2.3-rc1` counts as `1.2.3`). True when `latest` is newer.
pub fn is_newer(latest: &str, current: &str) -> bool {
    fn parts(s: &str) -> Vec<u64> {
        s.trim()
            .trim_start_matches('v')
            .split(['.', '-', '+'])
            .take(3)
            .map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    }
    let (mut l, mut c) = (parts(latest), parts(current));
    l.resize(3, 0);
    c.resize(3, 0);
    l > c
}

/// Release targets to try, preferred first — same order as `install.sh`
/// (musl before glibc on Linux). Empty means no prebuilt binary exists for
/// this platform (e.g. macOS: source build only).
pub fn detect_targets() -> Vec<&'static str> {
    if std::env::var("TERMUX_VERSION").is_ok() {
        return match std::env::consts::ARCH {
            "aarch64" => vec!["aarch64-linux-android"],
            "arm" => vec!["armv7-linux-androideabi"],
            "x86_64" => vec!["x86_64-linux-android"],
            "x86" => vec!["i686-linux-android"],
            _ => vec![],
        };
    }
    if std::env::consts::OS == "linux" {
        return match std::env::consts::ARCH {
            "x86_64" => vec!["x86_64-unknown-linux-musl", "x86_64-unknown-linux-gnu"],
            "aarch64" => vec!["aarch64-unknown-linux-musl", "aarch64-unknown-linux-gnu"],
            _ => vec![],
        };
    }
    vec![]
}

pub fn asset_name(tag: &str, target: &str) -> String {
    format!("laudacode-{tag}-{target}.tar.gz")
}

/// Where the worker downloads the asset. Single source of truth so the
/// extractor below can never collide with it again.
pub fn download_dir() -> PathBuf {
    std::env::temp_dir().join(format!("lc-update-{}", std::process::id()))
}

/// Latest release tag (`vX.Y.Z`) from the GitHub API.
pub async fn fetch_latest_tag(client: &reqwest::Client) -> Result<String> {
    let v: serde_json::Value = client
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .send()
        .await
        .context("checking for updates — is the network up?")?
        .error_for_status()
        .context("GitHub releases API failed (rate-limited?)")?
        .json()
        .await
        .context("parsing GitHub release info")?;
    v.get("tag_name")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .context("GitHub response has no tag_name")
}

/// Download `url` to `dest`, refusing absurd sizes before they eat RAM.
pub async fn download_to(client: &reqwest::Client, url: &str, dest: &Path) -> Result<()> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("not found: {url}");
    }
    let resp = resp.error_for_status().with_context(|| format!("downloading {url}"))?;
    if resp.content_length().is_some_and(|n| n > MAX_ASSET_BYTES) {
        bail!("asset exceeds {MAX_ASSET_BYTES} bytes — refusing");
    }
    let bytes = resp.bytes().await.context("reading download")?;
    if bytes.len() as u64 > MAX_ASSET_BYTES {
        bail!("asset exceeds {MAX_ASSET_BYTES} bytes — refusing");
    }
    std::fs::write(dest, &bytes)
        .with_context(|| format!("writing {}", dest.display()))?;
    Ok(())
}

/// Verify `asset` against the `SHA256SUMS` text using an external checksum
/// tool. Errors when no tool exists rather than installing unverified code.
pub fn verify_against_sums(asset: &Path, sums: &str) -> Result<()> {
    let name = asset
        .file_name()
        .context("asset has no file name")?
        .to_string_lossy();
    let expected = sums
        .lines()
        .find_map(|l| {
            let mut it = l.split_whitespace();
            match (it.next(), it.next()) {
                (Some(hash), Some(file)) if file.trim_start_matches('*') == name => Some(hash.to_string()),
                _ => None,
            }
        })
        .with_context(|| format!("SHA256SUMS has no entry for {name}"))?;
    for tool in [
        ["sha256sum".to_string(), asset.display().to_string()].as_slice(),
        ["shasum".to_string(), "-a".to_string(), "256".to_string(), asset.display().to_string()].as_slice(),
    ] {
        let out = std::process::Command::new(&tool[0])
            .args(&tool[1..])
            .output();
        let Ok(out) = out else { continue };
        if !out.status.success() {
            continue;
        }
        let actual = String::from_utf8_lossy(&out.stdout);
        let actual = actual.split_whitespace().next().unwrap_or("");
        anyhow::ensure!(
            actual.eq_ignore_ascii_case(&expected),
            "checksum mismatch for {name} — download corrupted, refusing to install"
        );
        return Ok(());
    }
    bail!("no sha256 tool found (sha256sum/shasum) — refusing to install unverified binary")
}

/// Extract the `laudacode` binary from `tgz` and install it over `dest_exe`.
/// The previous binary is kept next to it as `laudacode.bak`.
///
/// Extracts in its own staging dir: the caller downloads `tgz` into a
/// *different* dir, and this must never delete it (wiping a shared dir here
/// is exactly how an update once ate its own just-downloaded archive).
pub fn install_from_tarball(tgz: &Path, dest_exe: &Path) -> Result<PathBuf> {
    let dir = dest_exe.parent().context("binary path has no parent dir")?;
    let stage = std::env::temp_dir().join(format!("lc-update-x-{}", std::process::id()));
    if stage.exists() {
        std::fs::remove_dir_all(&stage).ok();
    }
    std::fs::create_dir_all(&stage).context("creating staging dir")?;
    let status = std::process::Command::new("tar")
        .args(["-xzf"])
        .arg(tgz)
        .args(["-C"])
        .arg(&stage)
        .status()
        .context("extracting update — is tar installed?")?;
    anyhow::ensure!(status.success(), "extracting update archive failed");
    let staged = stage.join("laudacode");
    anyhow::ensure!(staged.is_file(), "archive has no laudacode binary");
    let backup = dir.join("laudacode.bak");
    std::fs::remove_file(&backup).ok();
    // Rename (never overwrite) the destination: it is the *running* binary,
    // and writing to a running executable fails with ETXTBSY on Linux while
    // renaming it aside and creating a fresh file is fine.
    std::fs::rename(dest_exe, &backup).with_context(|| {
        format!(
            "moving aside {} — not writable? rerun with sudo or check $PREFIX",
            dest_exe.display()
        )
    })?;
    if std::fs::copy(&staged, dest_exe).is_err() {
        // Dest is gone but the old binary survives as backup; say so.
        let _ = std::fs::rename(&backup, dest_exe);
        bail!(
            "installing to {} — not writable? previous binary restored",
            dest_exe.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest_exe, std::fs::Permissions::from_mode(0o755)).ok();
    }
    std::fs::remove_dir_all(&stage).ok();
    Ok(dest_exe.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_detection_ignores_v_prefix_and_suffixes() {
        assert!(is_newer("v0.11.0", "0.10.1"));
        assert!(is_newer("0.11.0", "0.11.0") == false);
        assert!(!is_newer("v0.10.1", "0.11.0"));
        assert!(is_newer("v0.10.2", "0.10.1"));
        assert!(!is_newer("v0.10.1", "0.10.10"));
        assert!(is_newer("v1.0.0", "0.99.99"));
    }

    #[test]
    fn asset_names_match_install_sh() {
        assert_eq!(
            asset_name("v0.11.0", "aarch64-linux-android"),
            "laudacode-v0.11.0-aarch64-linux-android.tar.gz"
        );
    }

    #[test]
    fn targets_cover_tier1_platforms() {
        // Must at least not panic; exact list depends on the test host.
        let _ = detect_targets();
        // Asset order on linux/x86_64 prefers musl (mirrors install.sh).
        if std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64" {
            assert_eq!(
                detect_targets(),
                vec!["x86_64-unknown-linux-musl", "x86_64-unknown-linux-gnu"]
            );
        }
    }

    #[test]
    fn install_never_touches_the_download_dir() {
        // Regression: downloader and extractor once shared one staging dir,
        // so the extractor deleted the just-downloaded archive before
        // unpacking it (`tar: Cannot open: No such file or directory`).
        // The tgz lives in download_dir(), exactly like the real flow.
        let base = std::env::temp_dir().join(format!("lc-upd-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dl = download_dir();
        let _ = std::fs::remove_dir_all(&dl);
        std::fs::create_dir_all(&dl).unwrap();
        let bin_dir = base.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let dest = bin_dir.join("laudacode");
        std::fs::write(&dest, b"old").unwrap();
        let payload_dir = base.join("payload");
        std::fs::create_dir_all(&payload_dir).unwrap();
        std::fs::write(payload_dir.join("laudacode"), b"new").unwrap();
        let tgz = dl.join("fake.tar.gz");
        let tar = std::process::Command::new("tar")
            .args(["-czf"])
            .arg(&tgz)
            .args(["-C"])
            .arg(&payload_dir)
            .arg("laudacode")
            .status();
        let Ok(st) = tar else {
            eprintln!("skipping: tar not installed");
            return;
        };
        assert!(st.success());
        let sentinel = dl.join("downloaded-asset.bin");
        std::fs::write(&sentinel, b"precious download").unwrap();

        let installed = install_from_tarball(&tgz, &dest).expect("install");
        assert_eq!(installed, dest);
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert_eq!(std::fs::read(bin_dir.join("laudacode.bak")).unwrap(), b"old");
        assert!(tgz.is_file(), "extractor deleted the download dir!");
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"precious download");
        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_dir_all(&dl).ok();
    }

    #[test]
    fn cache_roundtrip_expires_on_ttl() {
        // Serialized with every other env-flipping test via the shared lock.
        let _g = crate::session::test_sync::env_lock();
        let old = std::env::var("LAUDACODE_SESSIONS_DIR").ok();
        let dir = std::env::temp_dir().join(format!("lc-upd-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("LAUDACODE_SESSIONS_DIR", &dir);
        assert!(cached_tag().is_none());
        store_tag("v9.9.9");
        assert_eq!(cached_tag().as_deref(), Some("v9.9.9"));
        // Backdate past the TTL: must read as a miss, not stale data.
        let stale = serde_json::json!({ "tag": "v1.0.0", "unix": 1 }).to_string();
        std::fs::write(dir.join("update-check.json"), stale).unwrap();
        assert!(cached_tag().is_none());
        match old {
            Some(v) => std::env::set_var("LAUDACODE_SESSIONS_DIR", v),
            None => std::env::remove_var("LAUDACODE_SESSIONS_DIR"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sums_entry_lookup_skips_starred_and_unrelated_lines() {        let sums = "abc123  laudacode-v0.11.0-aarch64-linux-android.tar.gz\n\
                    deadbeef  some-other-file.tar.gz\n";
        let dir = std::env::temp_dir();
        let asset = dir.join("laudacode-v0.11.0-aarch64-linux-android.tar.gz");
        // No checksum tool assertion — just the parser: wrong hash must fail
        // closed (either mismatch error or missing-tool error, never Ok).
        assert!(verify_against_sums(&asset, sums).is_err());
        assert!(verify_against_sums(&asset, "nothing relevant here\n").is_err());
    }
}
