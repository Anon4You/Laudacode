//! Small filesystem helpers shared by the edit and patch paths.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Refuse a final-component symlink. The caller's containment check and the
/// write would otherwise be a TOCTOU pair an attacker can win by swapping the
/// link between them.
pub fn no_symlink(path: &Path) -> Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            bail!(
                "refusing to write through symlink {} — replace it with a normal file first",
                path.display()
            );
        }
    }
    Ok(())
}

/// Like [`write`], but pins the parent directory before creating the temp file
/// so a symlink swap between containment check and write cannot redirect it.
/// On Linux/Android the opened directory is addressed through `/proc/self/fd`.
pub fn write_in(root: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    no_symlink(path)?;
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.permissions().readonly() {
            bail!("{} is read-only", path.display());
        }
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating {}", parent.display()))?;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::fd::AsRawFd;
        let dir = std::fs::File::open(parent)
            .with_context(|| format!("opening {}", parent.display()))?;
        let fd = dir.as_raw_fd();
        let real = std::fs::read_link(format!("/proc/self/fd/{fd}"))
            .with_context(|| format!("resolving {}", parent.display()))?;
        let root_real =
            std::fs::canonicalize(root).with_context(|| format!("resolving {}", root.display()))?;
        if !real.starts_with(&root_real) {
            bail!(
                "refusing to write {}: its parent resolved outside {}",
                path.display(),
                root.display()
            );
        }
        let name = path
            .file_name()
            .with_context(|| format!("{} has no file name", path.display()))?;
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_name = format!(".laudacode-write-{}-{seq}", std::process::id());
        let base = PathBuf::from(format!("/proc/self/fd/{fd}"));
        let tmp = base.join(&tmp_name);
        let result = (|| -> Result<()> {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(bytes)
                .with_context(|| format!("writing {}", tmp.display()))?;
            drop(f);
            if let Ok(meta) = std::fs::metadata(path) {
                let _ = std::fs::set_permissions(&tmp, meta.permissions());
            }
            std::fs::rename(&tmp, base.join(name))
                .with_context(|| format!("replacing {}", path.display()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_preserves_existing_permissions() {
        let dir = std::env::temp_dir().join(format!("lc-fsutil-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("run.sh");
        std::fs::write(&p, "old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        write_in(&dir, &p, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o755);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn write_refuses_a_parent_symlink_outside_root() {
        let base = std::env::temp_dir().join(format!("lc-fsutil-link-{}", std::process::id()));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let err = write_in(&root, &root.join("link/x.txt"), b"x").unwrap_err();
        assert!(format!("{err:#}").contains("outside"), "{err:#}");
        assert!(!outside.join("x.txt").exists());
        std::fs::remove_dir_all(&base).ok();
    }
}
