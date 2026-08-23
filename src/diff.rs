//! Minimal unified-diff engine (no external crates).
//!
//! Produces colored-renderable diffs for every code-mutating tool
//! (`edit_file`, `write_file`, `apply_patch`) so edits appear as green/red
//! hunks in the TUI instead of opaque "edited N lines" lines.

/// One rendered diff line. `text` keeps its leading sign/space marker.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DiffLine {
    pub kind: LineKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LineKind {
    /// Added line (`+`).
    Add,
    /// Removed line (`-`).
    Del,
    /// Context line (` `).
    Ctx,
    /// Hunk header / metadata (`@@`).
    Meta,
}

/// A per-file diff summary handed to the UI layer.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileDiff {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub lines: Vec<DiffLine>,
}

impl FileDiff {
    /// True when nothing actually changed.
    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0
    }
}

const MAX_DIFF_LINES: usize = 240;
/// Above this product the LCS matrix gets too fat for a phone — fall back to
/// a coarse "whole region replaced" diff instead of grinding memory.
const MAX_LCS_CELLS: usize = 400_000;

fn split_lines(s: &str) -> Vec<&str> {
    let mut v: Vec<&str> = s.split('\n').collect();
    if v.last() == Some(&"") {
        v.pop();
    }
    v
}

/// Longest-common-subsequence based unified diff with grouped hunks,
/// `@@ -a,b +c,d @@` headers and `context` lines of surrounding code.
pub fn unified_diff(path: &str, old: &str, new: &str, context: usize) -> FileDiff {
    let a = split_lines(old);
    let b = split_lines(new);

    // Op stream over the WHOLE file so context lines exist between changes.
    // The expensive LCS only runs on the prefix/suffix-trimmed middle.
    let mut start = 0usize;
    while start < a.len() && start < b.len() && a[start] == b[start] {
        start += 1;
    }
    let mut end_a = a.len();
    let mut end_b = b.len();
    while end_a > start && end_b > start && a[end_a - 1] == b[end_b - 1] {
        end_a -= 1;
        end_b -= 1;
    }
    let mid_a = &a[start..end_a];
    let mid_b = &b[start..end_b];

    if mid_a.is_empty() && mid_b.is_empty() {
        return FileDiff { path: path.into(), added: 0, removed: 0, lines: vec![] };
    }

    let mut ops: Vec<(LineKind, &str)> = Vec::with_capacity(a.len().max(b.len()));
    for l in &a[..start] {
        ops.push((LineKind::Ctx, l));
    }
    if mid_a.len() * mid_b.len() <= MAX_LCS_CELLS {
        ops.extend(lcs_ops(mid_a, mid_b));
    } else {
        // Too big for the matrix — coarse replace of the whole region.
        for l in mid_a {
            ops.push((LineKind::Del, l));
        }
        for l in mid_b {
            ops.push((LineKind::Add, l));
        }
    }
    for l in &a[end_a..] {
        ops.push((LineKind::Ctx, l));
    }

    let mut out: Vec<DiffLine> = vec![
        DiffLine { kind: LineKind::Meta, text: format!("--- a/{path}") },
        DiffLine { kind: LineKind::Meta, text: format!("+++ b/{path}") },
    ];
    let mut added = 0usize;
    let mut removed = 0usize;
    // 1-based line cursors into the old/new files.
    let (mut cur_old, mut cur_new) = (1usize, 1usize);

    let mut i = 0usize;
    let mut truncated = false;
    while i < ops.len() {
        // Find this change cluster: consecutive changes bridged by short
        // equal runs (<= 2*context).
        let mut j = i;
        let mut last_change = i;
        while j < ops.len() {
            if ops[j].0 != LineKind::Ctx {
                last_change = j;
                j += 1;
            } else {
                let gap_start = j;
                let mut k = j;
                while k < ops.len() && ops[k].0 == LineKind::Ctx {
                    k += 1;
                }
                if k < ops.len() && k - gap_start <= context * 2 {
                    j = k;
                } else {
                    break;
                }
            }
        }
        let cluster_end = (last_change + 1).min(ops.len());
        let range_start = i.saturating_sub(context);
        let range_end = (cluster_end + context).min(ops.len());

        // Hunk header from the pre-computed range.
        let old_first = cur_old;
        let new_first = cur_new;
        let mut old_cnt = 0usize;
        let mut new_cnt = 0usize;
        for op in &ops[range_start..range_end] {
            match op.0 {
                LineKind::Del => old_cnt += 1,
                LineKind::Add => new_cnt += 1,
                _ => {
                    old_cnt += 1;
                    new_cnt += 1;
                }
            }
        }
        out.push(DiffLine {
            kind: LineKind::Meta,
            text: format!("@@ -{old_first},{old_cnt} +{new_first},{new_cnt} @@"),
        });
        for op in &ops[range_start..range_end] {
            match op.0 {
                LineKind::Add => added += 1,
                LineKind::Del => removed += 1,
                _ => {}
            }
            out.push(DiffLine {
                kind: op.0,
                text: match op.0 {
                    LineKind::Add => format!("+{}", op.1),
                    LineKind::Del => format!("-{}", op.1),
                    LineKind::Ctx => format!(" {}", op.1),
                    LineKind::Meta => op.1.to_string(),
                },
            });
            match op.0 {
                LineKind::Del => cur_old += 1,
                LineKind::Add => cur_new += 1,
                _ => {
                    cur_old += 1;
                    cur_new += 1;
                }
            }
        }

        i = range_end;
        // Skip untouched gap before the next cluster.
        while i < ops.len() && ops[i].0 == LineKind::Ctx {
            i += 1;
        }
        if i < ops.len() && out.len() > MAX_DIFF_LINES {
            truncated = true;
            break;
        }
    }
    let _ = truncated;

    if added == 0 && removed == 0 {
        return FileDiff { path: path.into(), added: 0, removed: 0, lines: vec![] };
    }
    if out.len() > MAX_DIFF_LINES + 2 {
        out.truncate(MAX_DIFF_LINES + 2);
        out.push(DiffLine { kind: LineKind::Meta, text: "… [diff truncated]".into() });
    }
    FileDiff { path: path.into(), added, removed, lines: out }
}

/// Classic LCS backtrace producing ordered Add/Del/Ctx ops.
fn lcs_ops<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<(LineKind, &'a str)> {
    let n = a.len();
    let m = b.len();
    // dp[i][j] = LCS length of a[i..], b[j..]
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut out = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push((LineKind::Ctx, a[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push((LineKind::Del, a[i]));
            i += 1;
        } else {
            out.push((LineKind::Add, b[j]));
            j += 1;
        }
    }
    while i < n {
        out.push((LineKind::Del, a[i]));
        i += 1;
    }
    while j < m {
        out.push((LineKind::Add, b[j]));
        j += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_added_removed_and_context() {
        let old = "fn a() {\n    one();\n    two();\n}\n";
        let new = "fn a() {\n    one();\n    two_changed();\n    three();\n}\n";
        let d = unified_diff("src/x.rs", old, new, 2);
        assert_eq!(d.path, "src/x.rs");
        assert_eq!(d.removed, 1, "{d:#?}");
        assert_eq!(d.added, 2, "{d:#?}");
        let texts: Vec<&str> = d.lines.iter().map(|l| l.text.as_str()).collect();
        assert!(texts.contains(&"-    two();"));
        assert!(texts.contains(&"+    two_changed();"));
        assert!(texts.contains(&"+    three();"));
        assert!(texts.contains(&"     one();"), "context preserved: {texts:?}");
        assert!(d.lines.iter().any(|l| l.kind == LineKind::Meta && l.text.contains("@@")));
    }

    #[test]
    fn identical_files_produce_empty_diff() {
        let d = unified_diff("f.txt", "same\nlines\n", "same\nlines\n", 3);
        assert!(d.is_empty());
        assert!(d.lines.is_empty());
    }

    #[test]
    fn brand_new_file_is_all_additions() {
        let d = unified_diff("new.rs", "", "alpha\nbeta\n", 2);
        assert_eq!(d.added, 2);
        assert_eq!(d.removed, 0);
        assert!(d.lines.iter().all(|l| l.kind == LineKind::Add || l.kind == LineKind::Meta));
    }

    #[test]
    fn deleted_file_is_all_removals() {
        let d = unified_diff("old.rs", "x\ny\nz\n", "", 2);
        assert_eq!(d.removed, 3);
        assert_eq!(d.added, 0);
    }

    #[test]
    fn far_apart_changes_make_separate_hunks() {
        let filler = (0..30).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        let old = format!("{filler}\ntail\n");
        let edited = filler
            .replace("line5", "line5-edited")
            .replace("line25", "line25-edited");
        let new = format!("{edited}\ntail\n");
        let d = unified_diff("big.txt", &old, &new, 3);
        let meta_count = d.lines.iter().filter(|l| l.text.starts_with("@@")).count();
        assert_eq!(meta_count, 2, "expected two hunks, got {meta_count}: {}", 
            d.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("|"));
        // Unchanged filler lines must NOT appear between distant hunks.
        assert!(!d.lines.iter().any(|l| l.text == " line12"));
    }

    #[test]
    fn huge_files_fall_back_to_coarse_diff_without_panic() {
        let big_a = (0..5000).map(|i| format!("old{i}")).collect::<Vec<_>>().join("\n");
        let big_b = (0..5000).map(|i| format!("new{i}")).collect::<Vec<_>>().join("\n");
        let d = unified_diff("huge.txt", &big_a, &big_b, 2);
        assert!(d.removed > 0 && d.added > 0);
        assert!(d.lines.len() <= MAX_DIFF_LINES + 3, "capped: {}", d.lines.len());
        assert!(d.lines.last().unwrap().text.contains("[diff truncated]"));
    }

    #[test]
    fn diff_serializes_for_json_output() {
        let d = unified_diff("a", "x\n", "y\n", 1);
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["path"], "a");
        // lines: ---/+++/@@ header, then -x, +y
        let kinds: Vec<&str> = v["lines"].as_array().unwrap()
            .iter().map(|l| l["kind"].as_str().unwrap()).collect();
        assert!(kinds.contains(&"del"), "{kinds:?}");
        assert!(kinds.contains(&"add"), "{kinds:?}");
    }
}
