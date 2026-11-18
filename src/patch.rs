//! V4A patch format (`*** Begin Patch` … `*** End Patch`).
//!
//! Parser grammar + fuzzy sequence matching + chunk replacement algorithm,
//! simplified to synchronous std::fs with no external deps.
//!
//! Grammar (lenient):
//! ```text
//! start: "*** Begin Patch" LF hunk+ "*** End Patch"
//! hunk: add_hunk | delete_hunk | update_hunk
//! add_hunk:    "*** Add File: " filename LF ("+" line LF)+
//! delete_hunk: "*** Delete File: " filename LF
//! update_hunk: "*** Update File: " filename LF change_move? change+
//! change_move: "*** Move to: " filename LF
//! change:      ("@@" context? LF)? ((" "|"-"|"+") line LF)+ "*** End of File"?
//! ```

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

const BEGIN_MARKER: &str = "*** Begin Patch";
const END_MARKER: &str = "*** End Patch";
const ADD_FILE_MARKER: &str = "*** Add File: ";
const DELETE_FILE_MARKER: &str = "*** Delete File: ";
const UPDATE_FILE_MARKER: &str = "*** Update File: ";
const MOVE_TO_MARKER: &str = "*** Move to: ";
pub const EOF_MARKER: &str = "*** End of File";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Chunk {
    /// Single context line anchoring this chunk (usually a fn/class header).
    pub change_context: Option<String>,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    pub is_end_of_file: bool,
}

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::enum_variant_names)] // Add*/Update*/Delete* naming is part of the format
pub enum Hunk {
    AddFile {
        path: PathBuf,
        contents: String,
    },
    DeleteFile {
        path: PathBuf,
    },
    UpdateFile {
        path: PathBuf,
        move_path: Option<PathBuf>,
        chunks: Vec<Chunk>,
    },
}

impl Hunk {
    /// Every path this hunk can read or write. A move affects both the source
    /// and the destination, so checking only the destination is a bypass.
    pub fn paths(&self) -> Vec<&Path> {
        match self {
            Hunk::AddFile { path, .. } | Hunk::DeleteFile { path } => vec![path],
            Hunk::UpdateFile { path, move_path, .. } => {
                let mut out = vec![path.as_path()];
                if let Some(dest) = move_path {
                    out.push(dest.as_path());
                }
                out
            }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Hunk::AddFile { path, .. } => format!("add {}", path.display()),
            Hunk::DeleteFile { path } => format!("delete {}", path.display()),
            Hunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => {
                let added: usize = chunks.iter().map(|c| c.new_lines.len()).sum();
                let removed: usize = chunks
                    .iter()
                    .map(|c| c.old_lines.len().min(c.new_lines.len()))
                    .sum();
                match move_path {
                    Some(m) => format!(
                        "move {} -> {} (+{} -{} lines)",
                        path.display(),
                        m.display(),
                        added,
                        removed
                    ),
                    None => format!(
                        "update {} (+{} -{} lines)",
                        path.display(),
                        added.saturating_sub(removed),
                        removed
                    ),
                }
            }
        }
    }
}

/// Parse a V4A patch body. Accepts input with or without the Begin/End
/// markers (markers are required by the spec but we tolerate bare hunks).
pub fn parse_patch(raw: &str) -> Result<Vec<Hunk>> {
    let lines: Vec<&str> = raw.lines().collect();
    let mut i = 0usize;
    // Skip anything before the Begin marker.
    if let Some(pos) = lines.iter().position(|l| l.trim() == BEGIN_MARKER) {
        i = pos + 1;
    }
    let mut hunks = Vec::new();
    while i < lines.len() {
        let line = lines[i].trim_end();
        if line.trim() == END_MARKER {
            break;
        }
        if let Some(rest) = line.strip_prefix(ADD_FILE_MARKER) {
            let path = PathBuf::from(rest.trim());
            i += 1;
            let mut contents: Vec<String> = Vec::new();
            while i < lines.len() {
                let l = lines[i];
                if l.starts_with("*** ") || l.trim() == END_MARKER {
                    break;
                }
                if let Some(body) = l.strip_prefix('+') {
                    contents.push(body.to_string());
                } else if l.trim().is_empty() && contents.is_empty() {
                    // tolerate stray blank line before content
                } else {
                    bail!("invalid patch: add-file line {} must start with '+'", i + 1);
                }
                i += 1;
            }
            let mut body = contents.join("\n");
            if !body.is_empty() {
                body.push('\n');
            }
            hunks.push(Hunk::AddFile {
                path,
                contents: body,
            });
            continue;
        }
        if let Some(rest) = line.strip_prefix(DELETE_FILE_MARKER) {
            hunks.push(Hunk::DeleteFile {
                path: PathBuf::from(rest.trim()),
            });
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix(UPDATE_FILE_MARKER) {
            let path = PathBuf::from(rest.trim());
            i += 1;
            let mut move_path = None;
            if i < lines.len() && lines[i].trim_end().starts_with(MOVE_TO_MARKER) {
                move_path = Some(PathBuf::from(
                    lines[i].trim_end()[MOVE_TO_MARKER.len()..].trim(),
                ));
                i += 1;
            }
            let mut chunks: Vec<Chunk> = Vec::new();
            let mut cur = Chunk {
                change_context: None,
                old_lines: vec![],
                new_lines: vec![],
                is_end_of_file: false,
            };
            let flush = |cur: &mut Chunk, chunks: &mut Vec<Chunk>| {
                // A chunk counts when it has any anchor/context/eof intent.
                if cur.change_context.is_some()
                    || !cur.old_lines.is_empty()
                    || !cur.new_lines.is_empty()
                    || cur.is_end_of_file
                {
                    chunks.push(std::mem::take(cur));
                }
            };
            while i < lines.len() {
                let l = lines[i].trim_end();
                if l.starts_with("*** ") && l.trim() != EOF_MARKER {
                    break;
                }
                if l.trim() == EOF_MARKER {
                    // Flag the current/next chunk to anchor at end-of-file;
                    // the marker may precede or follow its +/- lines.
                    cur.is_end_of_file = true;
                    i += 1;
                    continue;
                }
                if l == "@@" || l.starts_with("@@ ") {
                    flush(&mut cur, &mut chunks);
                    cur.change_context = l
                        .strip_prefix("@@ ")
                        .map(str::to_string)
                        .filter(|s| !s.is_empty());
                    i += 1;
                    continue;
                }
                if let Some(body) = l.strip_prefix('+') {
                    cur.new_lines.push(body.to_string());
                } else if let Some(body) = l.strip_prefix('-') {
                    cur.old_lines.push(body.to_string());
                } else if let Some(body) = l.strip_prefix(' ') {
                    cur.old_lines.push(body.to_string());
                    cur.new_lines.push(body.to_string());
                } else if l.is_empty() {
                    // Bare empty line == context line with empty content.
                    cur.old_lines.push(String::new());
                    cur.new_lines.push(String::new());
                } else {
                    bail!(
                        "invalid patch: line {} ('{}') must start with '+', '-', ' ', '@@' or '***'",
                        i + 1,
                        l
                    );
                }
                i += 1;
            }
            flush(&mut cur, &mut chunks);
            hunks.push(Hunk::UpdateFile {
                path,
                move_path,
                chunks,
            });
            continue;
        }
        bail!("invalid patch: unexpected line {}: '{}'", i + 1, line);
    }
    if hunks.is_empty() {
        bail!("empty patch — expected hunks between '{BEGIN_MARKER}' and '{END_MARKER}'");
    }
    Ok(hunks)
}

/// Fuzzy sequence search: exact, then
/// rstrip, then full-trim comparison. `eof` prefers matching at end-of-file.
fn seek_sequence(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let search_start = if eof && lines.len() >= pattern.len() {
        (lines.len() - pattern.len()).max(start)
    } else {
        start
    };
    let last = lines.len() - pattern.len();
    // Pass 1: exact.
    for i in search_start..=last {
        if &lines[i..i + pattern.len()] == pattern {
            return Some(i);
        }
    }
    // Pass 2: ignore trailing whitespace.
    for i in search_start..=last {
        if (0..pattern.len()).all(|j| lines[i + j].trim_end() == pattern[j].trim_end()) {
            return Some(i);
        }
    }
    // Pass 3: ignore leading + trailing whitespace.
    for i in search_start..=last {
        if (0..pattern.len()).all(|j| lines[i + j].trim() == pattern[j].trim()) {
            return Some(i);
        }
    }
    None
}

/// Compute `(start_index, old_len, new_lines)` replacements for one file.
fn compute_replacements(
    original: &[String],
    path: &Path,
    chunks: &[Chunk],
) -> Result<Vec<(usize, usize, Vec<String>)>> {
    let mut reps: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut line_index = 0usize;
    for chunk in chunks {
        if let Some(ctx) = &chunk.change_context {
            match seek_sequence(original, std::slice::from_ref(ctx), line_index, false) {
                Some(idx) => line_index = idx + 1,
                None => bail!("Failed to find context '{ctx}' in {}", path.display()),
            }
        }
        if chunk.old_lines.is_empty() {
            // Pure insertion: at EOF when flagged, else after the last
            // anchored position (append for context-free chunks).
            let insertion_idx = if chunk.is_end_of_file {
                original.len()
            } else {
                line_index.min(original.len())
            };
            reps.push((insertion_idx, 0, chunk.new_lines.clone()));
            continue;
        }
        let mut pattern: &[String] = &chunk.old_lines;
        let mut new_slice: &[String] = &chunk.new_lines;
        let mut found = seek_sequence(original, pattern, line_index, chunk.is_end_of_file);
        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            // Trailing "" represents the final newline — retry without it.
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }
            found = seek_sequence(original, pattern, line_index, chunk.is_end_of_file);
        }
        match found {
            Some(start_idx) => {
                reps.push((start_idx, pattern.len(), new_slice.to_vec()));
                line_index = start_idx + pattern.len();
            }
            None => bail!(
                "Failed to find expected lines in {}:\n{}",
                path.display(),
                chunk.old_lines.join("\n")
            ),
        }
    }
    reps.sort_by_key(|(i, _, _)| *i);
    Ok(reps)
}

fn split_lines(contents: &str) -> Vec<String> {
    let mut v: Vec<String> = contents.split('\n').map(String::from).collect();
    if v.last().is_some_and(String::is_empty) {
        v.pop(); // final newline is implied on rejoin
    }
    v
}

fn updated_contents(raw: &str, path: &Path, chunks: &[Chunk]) -> Result<String> {
    let original = split_lines(raw);
    let reps = compute_replacements(&original, path, chunks)?;
    let mut lines = original;
    for (start_idx, old_len, new_segment) in reps.iter().rev() {
        for _ in 0..*old_len {
            if *start_idx < lines.len() {
                lines.remove(*start_idx);
            }
        }
        for (offset, l) in new_segment.iter().enumerate() {
            lines.insert(start_idx + offset, l.clone());
        }
    }
    let mut out = lines.join("\n");
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Resolve against `cwd` and remove `.`/`..` so two spellings of the same
/// path cannot stage as different files.
fn absolute(cwd: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut out = PathBuf::new();
    for comp in joined.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

#[derive(Debug, Clone)]
struct Staged {
    original: Option<Vec<u8>>,
    current: Option<Vec<u8>>,
}

fn stage<'a>(
    states: &'a mut std::collections::BTreeMap<PathBuf, Staged>,
    abs: &Path,
) -> Result<&'a mut Staged> {
    if !states.contains_key(abs) {
        if let Ok(meta) = std::fs::symlink_metadata(abs) {
            if meta.file_type().is_symlink() {
                bail!(
                    "refusing to patch through symlink {} — replace it with a normal file first",
                    abs.display()
                );
            }
        }
        let original = match std::fs::read(abs) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", abs.display()));
            }
        };
        states.insert(
            abs.to_path_buf(),
            Staged {
                current: original.clone(),
                original,
            },
        );
    }
    Ok(states.get_mut(abs).expect("just inserted"))
}

fn write_bytes(cwd: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    crate::fsutil::write_in(cwd, path, bytes)
}

/// Apply parsed hunks under `cwd`. Returns a human-readable summary plus a
/// per-file unified diff so the UI can render colored edits.
///
/// Every hunk is staged in memory first, so a bad hunk late in the patch
/// leaves every earlier file untouched. Add/move destinations that already
/// exist are refused rather than overwritten.
pub fn apply_hunks(cwd: &Path, hunks: &[Hunk]) -> Result<AppliedPatch> {
    let mut states: std::collections::BTreeMap<PathBuf, Staged> =
        std::collections::BTreeMap::new();
    let mut changed: Vec<String> = Vec::new();
    let mut files: Vec<crate::diff::FileDiff> = Vec::new();

    for hunk in hunks {
        match hunk {
            Hunk::AddFile { path, contents } => {
                let abs = absolute(cwd, path);
                if states.get(&abs).is_some_and(|s| s.current.is_some()) || exists(&abs) {
                    bail!("refusing to add {}: it already exists", abs.display());
                }
                let st = states.entry(abs.clone()).or_insert(Staged {
                    original: None,
                    current: None,
                });
                st.current = Some(contents.as_bytes().to_vec());
                files.push(crate::diff::unified_diff(
                    &path.display().to_string(),
                    "",
                    contents,
                    3,
                ));
                changed.push(hunk.describe());
            }
            Hunk::DeleteFile { path } => {
                let abs = absolute(cwd, path);
                let st = stage(&mut states, &abs)?;
                let old = st
                    .current
                    .take()
                    .with_context(|| format!("{} does not exist", abs.display()))?;
                let old = String::from_utf8(old)
                    .with_context(|| format!("{} is not UTF-8", abs.display()))?;
                files.push(crate::diff::unified_diff(
                    &path.display().to_string(),
                    &old,
                    "",
                    3,
                ));
                changed.push(hunk.describe());
            }
            Hunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => {
                let abs = absolute(cwd, path);
                let (old, new_contents) = {
                    let st = stage(&mut states, &abs)?;
                    let old_bytes = st
                        .current
                        .as_ref()
                        .with_context(|| format!("{} does not exist", abs.display()))?;
                    let old = String::from_utf8(old_bytes.clone())
                        .with_context(|| format!("{} is not UTF-8", abs.display()))?;
                    let new_contents = updated_contents(&old, &abs, chunks)?;
                    (old, new_contents)
                };

                let display = if let Some(dest) = move_path {
                    let dest_abs = absolute(cwd, dest);
                    if dest_abs == abs {
                        path.display().to_string()
                    } else {
                        if states.get(&dest_abs).is_some_and(|s| s.current.is_some())
                            || exists(&dest_abs)
                        {
                            bail!(
                                "refusing to move {} to {}: destination already exists",
                                abs.display(),
                                dest_abs.display()
                            );
                        }
                        states.get_mut(&abs).expect("staged").current = None;
                        let dest_st = states.entry(dest_abs).or_insert(Staged {
                            original: None,
                            current: None,
                        });
                        dest_st.current = Some(new_contents.as_bytes().to_vec());
                        display_for_move(path, dest)
                    }
                } else {
                    path.display().to_string()
                };
                let stays_put = match &move_path {
                    None => true,
                    Some(dest) => absolute(cwd, dest) == abs,
                };
                if stays_put {
                    states.get_mut(&abs).expect("staged").current =
                        Some(new_contents.as_bytes().to_vec());
                }
                files.push(crate::diff::unified_diff(&display, &old, &new_contents, 3));
                changed.push(hunk.describe());
            }
        }
    }

    // Write only after every hunk validated. Roll back on the first failure;
    // a multi-file patch must not leave a half-applied tree behind.
    if let Err(e) = commit(cwd, &states) {
        if let Err(rollback) = restore(cwd, &states) {
            bail!("{e:#}\nrollback also failed: {rollback:#}");
        }
        return Err(e);
    }

    Ok(AppliedPatch {
        summary: format!(
            "Success. Updated the following files:\n{}",
            changed
                .iter()
                .map(|d| format!("M {d}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        files,
    })
}

fn display_for_move(path: &Path, dest: &Path) -> String {
    format!("{} -> {}", path.display(), dest.display())
}

fn commit(cwd: &Path, states: &std::collections::BTreeMap<PathBuf, Staged>) -> Result<()> {
    for (path, st) in states {
        if st.original == st.current {
            continue;
        }
        if let Some(bytes) = &st.current {
            write_bytes(cwd, path, bytes)?;
        }
    }
    for (path, st) in states {
        if st.original == st.current {
            continue;
        }
        if st.current.is_none() && st.original.is_some() {
            std::fs::remove_file(path).with_context(|| format!("deleting {}", path.display()))?;
        }
    }
    Ok(())
}

fn restore(cwd: &Path, states: &std::collections::BTreeMap<PathBuf, Staged>) -> Result<()> {
    for (path, st) in states {
        if st.original == st.current {
            continue;
        }
        match &st.original {
            Some(bytes) => write_bytes(cwd, path, bytes)?,
            None => match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
            },
        }
    }
    Ok(())
}

/// Result of applying a whole patch: text summary for the model + diffs for the UI.
#[derive(Debug, Clone)]
pub struct AppliedPatch {
    pub summary: String,
    pub files: Vec<crate::diff::FileDiff>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_add_delete_update() {
        let p = r#"*** Begin Patch
*** Add File: hello.txt
+hello
+world
*** Update File: src/lib.rs
@@ fn main() {
-old_a
+new_line
     kept();
-old_b
+replacement
*** Delete File: obsolete.txt
*** End Patch"#;
        let hunks = parse_patch(p).unwrap();
        assert_eq!(hunks.len(), 3);
        match &hunks[0] {
            Hunk::AddFile { path, contents } => {
                assert_eq!(path, Path::new("hello.txt"));
                assert_eq!(contents, "hello\nworld\n");
            }
            _ => panic!("expected add"),
        }
        match &hunks[1] {
            Hunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => {
                assert_eq!(path, Path::new("src/lib.rs"));
                assert!(move_path.is_none());
                // One @@ marker ⇒ one chunk; context lines join old+new.
                assert_eq!(chunks.len(), 1);
                assert_eq!(chunks[0].change_context.as_deref(), Some("fn main() {"));
                assert_eq!(chunks[0].old_lines, vec!["old_a", "    kept();", "old_b"]);
                assert_eq!(
                    chunks[0].new_lines,
                    vec!["new_line", "    kept();", "replacement"]
                );
            }
            _ => panic!("expected update"),
        }
        assert_eq!(
            hunks[2],
            Hunk::DeleteFile {
                path: PathBuf::from("obsolete.txt")
            }
        );
    }

    #[test]
    fn applies_multi_chunk_update_with_fuzzy_whitespace() {
        let dir = std::env::temp_dir().join(format!("lc-patch-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/a.rs"),
            "fn main() {\n    println!(\"hi\");\n}\n\nfn tail() {\n    1\n}\n",
        )
        .unwrap();
        let p = "*** Begin Patch\n*** Update File: src/a.rs\n@@ fn main() {\n-    println!(\"hi\");\n+    println!(\"patched\");\n*** Update File: src/a.rs\n@@ fn tail() {\n-    1\n+    42\n*** End Patch";
        // Two separate UpdateFile hunks for the same file — supported sequentially.
        let hunks = parse_patch(p).unwrap();
        let out = apply_hunks(&dir, &hunks).unwrap();
        assert!(out.summary.contains("Success"), "{}", out.summary);
        assert_eq!(out.files.len(), 2, "one diff per update hunk");
        let text = std::fs::read_to_string(dir.join("src/a.rs")).unwrap();
        assert!(text.contains("\"patched\""), "{text}");
        assert!(text.contains("42"), "{text}");
        assert!(text.ends_with("}\n"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn eof_marker_appends_at_end() {
        let dir = std::env::temp_dir().join(format!("lc-patch-eof-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "alpha\nbeta\n").unwrap();
        let p = "*** Begin Patch\n*** Update File: f.txt\n*** End of File\n+gamma\n*** End Patch";
        let hunks = parse_patch(p).unwrap();
        apply_hunks(&dir, &hunks).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("f.txt")).unwrap(),
            "alpha\nbeta\ngamma\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn move_to_renames_file() {
        let dir = std::env::temp_dir().join(format!("lc-patch-move-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("f.txt"), "one\n").unwrap();
        let p = "*** Begin Patch\n*** Update File: f.txt\n*** Move to: sub/g.txt\n-one\n+ONE\n*** End Patch";
        let hunks = parse_patch(p).unwrap();
        apply_hunks(&dir, &hunks).unwrap();
        assert!(!dir.join("f.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("sub/g.txt")).unwrap(),
            "ONE\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failed_hunk_leaves_earlier_files_untouched() {
        let dir = std::env::temp_dir().join(format!("lc-patch-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "old\n").unwrap();
        std::fs::write(dir.join("b.txt"), "keep\n").unwrap();
        let p = "*** Begin Patch\n*** Update File: a.txt\n-old\n+new\n*** Update File: b.txt\n-missing\n+gone\n*** End Patch";
        let hunks = parse_patch(p).unwrap();
        assert!(apply_hunks(&dir, &hunks).is_err());
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "old\n");
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "keep\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_and_move_refuse_to_clobber() {
        let dir = std::env::temp_dir().join(format!("lc-patch-clobber-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        std::fs::write(dir.join("b.txt"), "b\n").unwrap();
        let add = "*** Begin Patch\n*** Add File: a.txt\n+x\n*** End Patch";
        assert!(apply_hunks(&dir, &parse_patch(add).unwrap()).is_err());
        let mv = "*** Begin Patch\n*** Update File: a.txt\n*** Move to: b.txt\n-a\n+z\n*** End Patch";
        assert!(apply_hunks(&dir, &parse_patch(mv).unwrap()).is_err());
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a\n");
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "b\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_context_is_reported_not_panic() {
        let dir = std::env::temp_dir().join(format!("lc-patch-miss-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "a\nb\n").unwrap();
        let p =
            "*** Begin Patch\n*** Update File: f.txt\n@@ nonexistent-anchor\n-b\n+B\n*** End Patch";
        let err = apply_hunks(&dir, &parse_patch(p).unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("nonexistent-anchor"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
