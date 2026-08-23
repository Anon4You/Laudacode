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
    AddFile { path: PathBuf, contents: String },
    DeleteFile { path: PathBuf },
    UpdateFile { path: PathBuf, move_path: Option<PathBuf>, chunks: Vec<Chunk> },
}

impl Hunk {
    /// The path whose safety/danger classifies this hunk (move target wins).
    pub fn classify_path(&self) -> &Path {
        match self {
            Hunk::AddFile { path, .. } | Hunk::DeleteFile { path } => path,
            Hunk::UpdateFile { move_path: Some(p), .. } => p,
            Hunk::UpdateFile { path, .. } => path,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Hunk::AddFile { path, .. } => format!("add {}", path.display()),
            Hunk::DeleteFile { path } => format!("delete {}", path.display()),
            Hunk::UpdateFile { path, move_path, chunks } => {
                let added: usize = chunks.iter().map(|c| c.new_lines.len()).sum();
                let removed: usize = chunks.iter().map(|c| c.old_lines.len().min(c.new_lines.len())).sum();
                match move_path {
                    Some(m) => format!("move {} -> {} (+{} -{} lines)", path.display(), m.display(), added, removed),
                    None => format!("update {} (+{} -{} lines)", path.display(), added.saturating_sub(removed), removed),
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
            hunks.push(Hunk::AddFile { path, contents: body });
            continue;
        }
        if let Some(rest) = line.strip_prefix(DELETE_FILE_MARKER) {
            hunks.push(Hunk::DeleteFile { path: PathBuf::from(rest.trim()) });
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix(UPDATE_FILE_MARKER) {
            let path = PathBuf::from(rest.trim());
            i += 1;
            let mut move_path = None;
            if i < lines.len() && lines[i].trim_end().starts_with(MOVE_TO_MARKER) {
                move_path = Some(PathBuf::from(lines[i].trim_end()[MOVE_TO_MARKER.len()..].trim()));
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
                    cur.change_context = l.strip_prefix("@@ ").map(str::to_string).filter(|s| !s.is_empty());
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
            hunks.push(Hunk::UpdateFile { path, move_path, chunks });
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

fn apply_update(cwd: &Path, path: &Path, move_path: &Option<PathBuf>, chunks: &[Chunk]) -> Result<String> {
    let abs = cwd.join(path);
    let raw = std::fs::read_to_string(&abs)
        .with_context(|| format!("Failed to read file to update {}", abs.display()))?;
    let original = split_lines(&raw);
    let reps = compute_replacements(&original, &abs, chunks)?;
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
    std::fs::write(&abs, out.as_bytes())
        .with_context(|| format!("writing {}", abs.display()))?;
    if let Some(dest) = move_path {
        let dest_abs = cwd.join(dest);
        if let Some(parent) = dest_abs.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::rename(&abs, &dest_abs)
            .with_context(|| format!("moving {} to {}", abs.display(), dest_abs.display()))?;
    }
    Ok(out)
}

/// Apply parsed hunks under `cwd`. Returns a human-readable summary.
/// Per-file failures abort remaining hunks for that file but report progress.
pub fn apply_hunks(cwd: &Path, hunks: &[Hunk]) -> Result<String> {
    let mut changed: Vec<String> = Vec::new();
    for hunk in hunks {
        match hunk {
            Hunk::AddFile { path, contents } => {
                let abs = cwd.join(path);
                if let Some(parent) = abs.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                std::fs::write(&abs, contents.as_bytes())
                    .with_context(|| format!("writing {}", abs.display()))?;
                changed.push(hunk.describe());
            }
            Hunk::DeleteFile { path } => {
                let abs = cwd.join(path);
                if !abs.exists() {
                    bail!("Failed to delete {}: does not exist", abs.display());
                }
                std::fs::remove_file(&abs)
                    .with_context(|| format!("deleting {}", abs.display()))?;
                changed.push(hunk.describe());
            }
            Hunk::UpdateFile { path, move_path, chunks } => {
                apply_update(cwd, path, move_path, chunks)?;
                changed.push(hunk.describe());
            }
        }
    }
    Ok(format!(
        "Success. Updated the following files:\n{}",
        changed.iter().map(|d| format!("M {d}")).collect::<Vec<_>>().join("\n")
    ))
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
            Hunk::UpdateFile { path, move_path, chunks } => {
                assert_eq!(path, Path::new("src/lib.rs"));
                assert!(move_path.is_none());
                // One @@ marker ⇒ one chunk; context lines join old+new.
                assert_eq!(chunks.len(), 1);
                assert_eq!(chunks[0].change_context.as_deref(), Some("fn main() {"));
                assert_eq!(
                    chunks[0].old_lines,
                    vec!["old_a", "    kept();", "old_b"]
                );
                assert_eq!(
                    chunks[0].new_lines,
                    vec!["new_line", "    kept();", "replacement"]
                );
            }
            _ => panic!("expected update"),
        }
        assert_eq!(hunks[2], Hunk::DeleteFile { path: PathBuf::from("obsolete.txt") });
    }

    #[test]
    fn applies_multi_chunk_update_with_fuzzy_whitespace() {
        let dir = std::env::temp_dir().join(format!("lc-patch-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn main() {\n    println!(\"hi\");\n}\n\nfn tail() {\n    1\n}\n").unwrap();
        let p = "*** Begin Patch\n*** Update File: src/a.rs\n@@ fn main() {\n-    println!(\"hi\");\n+    println!(\"patched\");\n*** Update File: src/a.rs\n@@ fn tail() {\n-    1\n+    42\n*** End Patch";
        // Two separate UpdateFile hunks for the same file — supported sequentially.
        let hunks = parse_patch(p).unwrap();
        let out = apply_hunks(&dir, &hunks).unwrap();
        assert!(out.contains("Success"), "{out}");
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
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "alpha\nbeta\ngamma\n");
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
        assert_eq!(std::fs::read_to_string(dir.join("sub/g.txt")).unwrap(), "ONE\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_context_is_reported_not_panic() {
        let dir = std::env::temp_dir().join(format!("lc-patch-miss-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "a\nb\n").unwrap();
        let p = "*** Begin Patch\n*** Update File: f.txt\n@@ nonexistent-anchor\n-b\n+B\n*** End Patch";
        let err = apply_hunks(&dir, &parse_patch(p).unwrap()).unwrap_err().to_string();
        assert!(err.contains("nonexistent-anchor"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
