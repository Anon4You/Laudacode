use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::api::{FunctionDef, ToolDef};

const MAX_TOOL_OUTPUT: usize = 8 * 1024;
const MAX_READ_BYTES: u64 = 200 * 1024;
/// Inspection subcommands. Nothing here writes to the repository, the index,
/// the working tree or any remote, so these are allowed even in Plan mode.
const GIT_READ: &[&str] = &["status", "log", "diff", "show", "blame", "worktree"];

/// Repo-mutating subcommands, local only. Deliberately no `push`, no `fetch`,
/// no `reset`, no remote or PR operations — those stay behind `run_command`,
/// which is permission-gated and runs the dangerous-command detector.
///
/// `worktree` is read-only (`list`): add/remove stays behind `run_command`
/// until the multi-session work lands, so no stranded worktrees.
const GIT_WRITE: &[&str] = &["add", "commit", "branch", "checkout", "stash"];

/// Writes that discard or relocate work in ways the approval prompt cannot
/// show, so they ask even in full-auto regardless of the danger detector.
const GIT_ALWAYS_ASK: &[&str] = &["checkout", "stash"];

/// Every op the `lsp` tool accepts.
pub const LSP_OPS: [&str; 5] = [
    "diagnostics",
    "definition",
    "references",
    "hover",
    "symbols",
];

#[derive(Deserialize)]
struct LspArgs {
    op: String,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    column: Option<u64>,
    #[serde(default)]
    query: Option<String>,
}

/// Every subcommand the `git` tool accepts, reads first.
fn git_subcommands() -> Vec<&'static str> {
    GIT_READ.iter().chain(GIT_WRITE).copied().collect()
}

/// Actions the model can request. Parsed from tool-call arguments.
#[derive(Debug, Clone)]
pub enum Action {
    ListDir {
        path: String,
    },
    ReadFile {
        path: String,
        offset: Option<u64>,
        limit: Option<u64>,
    },
    ViewImage {
        path: String,
    },
    WriteFile {
        path: String,
        content: String,
    },
    EditFile {
        path: String,
        old: String,
        new: String,
    },
    /// V4A patch — multi-file add/update/delete in one call.
    ApplyPatch {
        patch: String,
    },
    RunCommand {
        command: String,
    },
    /// Start a long-lived managed process (dev server, watcher, REPL...).
    StartProcess {
        command: String,
    },
    /// Poll a managed process: status + buffered output since spawn.
    PollProcess {
        id: u64,
    },
    /// Send stdin to a managed process (eof=true closes stdin).
    WriteProcess {
        id: u64,
        input: String,
        eof: bool,
    },
    /// Kill + remove a managed process.
    StopProcess {
        id: u64,
    },
    FetchUrl {
        url: String,
    },
    WebSearch {
        query: String,
        max_results: usize,
    },
    Grep {
        pattern: String,
        path: Option<String>,
        ignore_case: bool,
        context: Option<u32>,
        /// Treat `pattern` as a regular expression instead of literal text.
        regex: bool,
    },
    Glob {
        pattern: String,
        path: Option<String>,
    },
    /// Language-server query. Read-only, but needs the agent's live LSP
    /// connections, so it is executed by the agent rather than standalone.
    Lsp {
        op: String,
        file: Option<String>,
        line: Option<u64>,
        column: Option<u64>,
        query: Option<String>,
    },
    /// Read-only git inspection (status, log, diff, show, blame).
    Git {
        subcommand: String,
        path: Option<String>,
        rev: Option<String>,
        limit: Option<u32>,
        /// Commit message; always set for `commit`, never for anything else.
        message: Option<String>,
    },
    UpdatePlan {
        todos: Vec<TodoItem>,
    },
}

/// Render a git action as a shell command line, in the shape a user would
/// actually type.
///
/// Used for the `[permission.bash]` key and the dangerous-command detector, so
/// a rule written for `git commit -m "…"` matches the action that really runs.
/// Deliberately unquoted: a rule has to be writable by hand, so the operands
/// are joined plainly. `Action::describe` formats its own display-oriented
/// variant (quoted message, `(max N)`) — that one is for humans, and never
/// feeds a pattern match.
pub fn git_command_line(
    subcommand: &str,
    rev: Option<&str>,
    path: Option<&str>,
    message: Option<&str>,
) -> String {
    let mut s = format!("git {subcommand}");
    match subcommand {
        // Operand is the message, not a revision. `commit` always has one:
        // parse_tool_action rejects the empty case.
        "commit" => {
            if let Some(m) = message {
                s.push_str(&format!(" -m {m}"));
            }
        }
        // No pathspec means the whole tree, deletions included. Report the
        // effective `-A` rather than the bare subcommand.
        "add" => {
            if let Some(p) = path {
                s.push_str(&format!(" -- {p}"));
            } else {
                s.push_str(" -A");
            }
        }
        // No rev and no path restores the whole tree, not "nothing".
        "checkout" => match (rev, path) {
            (Some(r), Some(p)) => s.push_str(&format!(" {r} -- {p}")),
            (Some(r), None) => s.push_str(&format!(" {r}")),
            (None, Some(p)) => s.push_str(&format!(" -- {p}")),
            (None, None) => s.push_str(" -- ."),
        },
        // `stash` with no rev is a push; `branch` with no rev lists.
        "stash" | "branch" => {
            if let Some(r) = rev {
                s.push(' ');
                s.push_str(r);
            }
        }
        // Read-only list; add/remove stays behind `run_command`.
        "worktree" => {
            s.push_str(" list");
        }
        _ => {
            if let Some(r) = rev {
                s.push(' ');
                s.push_str(r);
            }
            if let Some(p) = path {
                s.push_str(&format!(" -- {p}"));
            }
        }
    }
    s
}

/// One task in the shared todo list (surfaced live in the TUI).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    pub content: String,
    /// "pending" | "in_progress" | "completed"
    pub status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Danger {
    Safe,
    Moderate,
    High,
}

#[derive(Deserialize)]
struct ListDirArgs {
    #[serde(default)]
    path: Option<String>,
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    limit: Option<u64>,
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
}

#[derive(Deserialize)]
struct PatchArgs {
    patch: String,
}

#[derive(Deserialize)]
struct CmdArgs {
    command: String,
}

#[derive(Deserialize)]
struct StartProcessArgs {
    command: String,
}

#[derive(Deserialize)]
struct ProcessIdArgs {
    id: u64,
}

#[derive(Deserialize)]
struct WriteProcessArgs {
    id: u64,
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    eof: bool,
}

#[derive(Deserialize)]
struct FetchArgs {
    url: String,
}

#[derive(Deserialize)]
struct WebSearchArgs {
    query: String,
    #[serde(default = "default_max_results")]
    max_results: Option<usize>,
}

fn default_max_results() -> Option<usize> {
    Some(5)
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    context: Option<u32>,
    #[serde(default)]
    regex: bool,
}

#[derive(Deserialize)]
struct GitArgs {
    subcommand: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    rev: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    /// Commit message. Required for `commit`; `git` is spawned with a null
    /// stdin so it can never open an editor and hang until the timeout.
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Deserialize)]
struct TodoItemArgs {
    content: String,
    status: String,
}

#[derive(Deserialize)]
struct TodoWriteArgs {
    todos: Vec<TodoItemArgs>,
}

/// `update_plan` schema: {plan: [{step, status}]}.
#[derive(Deserialize)]
struct PlanStepArgs {
    step: String,
    status: String,
}

#[derive(Deserialize)]
struct UpdatePlanArgs {
    plan: Vec<PlanStepArgs>,
}

pub fn parse_tool_action(name: &str, arguments: &str) -> Result<Action> {
    fn inner(name: &str, v: serde_json::Value) -> Result<Action> {
        match name {
            "list_dir" => {
                let a: ListDirArgs = serde_json::from_value(v)?;
                Ok(Action::ListDir {
                    path: a.path.unwrap_or_else(|| ".".into()),
                })
            }
            "read_file" => {
                let a: ReadArgs = serde_json::from_value(v)?;
                Ok(Action::ReadFile {
                    path: a.path,
                    offset: a.offset,
                    limit: a.limit,
                })
            }
            "view_image" => {
                let a: ReadArgs = serde_json::from_value(v)?;
                Ok(Action::ViewImage { path: a.path })
            }
            "write_file" => {
                let a: WriteArgs = serde_json::from_value(v)?;
                Ok(Action::WriteFile {
                    path: a.path,
                    content: a.content,
                })
            }
            "edit_file" => {
                let a: EditArgs = serde_json::from_value(v)?;
                Ok(Action::EditFile {
                    path: a.path,
                    old: a.old_string,
                    new: a.new_string,
                })
            }
            "apply_patch" => {
                let a: PatchArgs = serde_json::from_value(v)?;
                Ok(Action::ApplyPatch { patch: a.patch })
            }
            "run_command" => {
                let a: CmdArgs = serde_json::from_value(v)?;
                Ok(Action::RunCommand { command: a.command })
            }
            "start_process" => {
                let a: StartProcessArgs = serde_json::from_value(v)?;
                Ok(Action::StartProcess { command: a.command })
            }
            "poll_process" => {
                let a: ProcessIdArgs = serde_json::from_value(v)?;
                Ok(Action::PollProcess { id: a.id })
            }
            "write_process" => {
                let a: WriteProcessArgs = serde_json::from_value(v)?;
                anyhow::ensure!(
                    a.eof || a.input.is_some(),
                    "input is required unless eof=true"
                );
                anyhow::ensure!(
                    !a.eof || a.input.as_deref().unwrap_or_default().is_empty(),
                    "EOF requires empty input"
                );
                Ok(Action::WriteProcess {
                    id: a.id,
                    input: a.input.unwrap_or_default(),
                    eof: a.eof,
                })
            }
            "stop_process" => {
                let a: ProcessIdArgs = serde_json::from_value(v)?;
                Ok(Action::StopProcess { id: a.id })
            }
            "fetch_url" => {
                let a: FetchArgs = serde_json::from_value(v)?;
                Ok(Action::FetchUrl { url: a.url })
            }
            "web_search" => {
                let a: WebSearchArgs = serde_json::from_value(v)?;
                Ok(Action::WebSearch {
                    query: a.query,
                    max_results: a.max_results.unwrap_or(5),
                })
            }
            "grep" => {
                let a: GrepArgs = serde_json::from_value(v)?;
                Ok(Action::Grep {
                    pattern: a.pattern,
                    path: a.path,
                    ignore_case: a.ignore_case,
                    context: a.context,
                    regex: a.regex,
                })
            }
            "lsp" => {
                let a: LspArgs = serde_json::from_value(v)?;
                let op = a.op.trim().to_lowercase();
                if !LSP_OPS.contains(&op.as_str()) {
                    bail!(
                        "unsupported lsp op '{}' — use one of: {}",
                        a.op,
                        LSP_OPS.join(", ")
                    );
                }
                // A position-taking op without a position would otherwise
                // quietly fall back to line 1, producing a confident answer
                // about the wrong line.
                let needs_pos = matches!(op.as_str(), "definition" | "references" | "hover");
                if needs_pos && a.file.is_none() {
                    bail!("lsp {op} needs a `file`");
                }
                if needs_pos && (a.line.is_none() || a.column.is_none()) {
                    bail!("lsp {op} needs `line` and `column` (both 1-based)");
                }
                Ok(Action::Lsp {
                    op,
                    file: a.file,
                    line: a.line,
                    column: a.column,
                    query: a.query,
                })
            }
            "git" => {
                let a: GitArgs = serde_json::from_value(v)?;
                let sub = a.subcommand.trim().to_lowercase();
                if !GIT_READ.contains(&sub.as_str()) && !GIT_WRITE.contains(&sub.as_str()) {
                    bail!(
                        "unsupported git subcommand '{}' — use one of: {}",
                        a.subcommand,
                        git_subcommands().join(", ")
                    );
                }
                // `commit` with no message would spawn an editor against a null
                // stdin: it hangs until the 60s timeout and then reports a
                // confusing failure. Fail here, where the model can fix it.
                if sub == "commit" && a.message.as_deref().map(str::trim).unwrap_or("").is_empty() {
                    bail!("git commit needs a `message`");
                }
                let message = a
                    .message
                    .map(|m| m.trim().to_string())
                    .filter(|m| !m.is_empty());
                Ok(Action::Git {
                    subcommand: sub,
                    path: a.path,
                    rev: a.rev,
                    limit: a.limit,
                    message,
                })
            }
            "glob" => {
                let a: GlobArgs = serde_json::from_value(v)?;
                Ok(Action::Glob {
                    pattern: a.pattern,
                    path: a.path,
                })
            }
            // Canonical name first; legacy alias kept for old sessions.
            "update_plan" | "todo_write" => {
                if let Ok(a) = serde_json::from_value::<UpdatePlanArgs>(v.clone()) {
                    Ok(Action::UpdatePlan {
                        todos: a
                            .plan
                            .into_iter()
                            .map(|t| TodoItem {
                                content: t.step,
                                status: t.status,
                            })
                            .collect(),
                    })
                } else {
                    let a: TodoWriteArgs = serde_json::from_value(v)?;
                    Ok(Action::UpdatePlan {
                        todos: a
                            .todos
                            .into_iter()
                            .map(|t| TodoItem {
                                content: t.content,
                                status: t.status,
                            })
                            .collect(),
                    })
                }
            }
            other => bail!("unknown tool '{other}'"),
        }
    }
    let v: serde_json::Value = if arguments.trim().is_empty() {
        serde_json::Value::Object(Default::default())
    } else {
        match serde_json::from_str(arguments) {
            Ok(v) => v,
            Err(e) => bail!("invalid tool arguments JSON: {e}"),
        }
    };
    inner(name, v)
}

impl Action {
    /// Short human-readable description for confirmation prompts.
    pub fn describe(&self) -> String {
        match self {
            Action::ListDir { path } => format!("list {path}"),
            Action::ReadFile { path, .. } => format!("read {path}"),
            Action::ViewImage { path } => format!("view image {path}"),
            Action::WriteFile { path, content } => {
                format!("write {} ({} bytes)", path, content.len())
            }
            Action::EditFile { path, old, new } => {
                format!(
                    "edit {} (-{} +{} lines)",
                    path,
                    old.lines().count(),
                    new.lines().count()
                )
            }
            Action::ApplyPatch { patch } => match crate::patch::parse_patch(patch) {
                Ok(hunks) => format!(
                    "apply_patch: {}",
                    hunks
                        .iter()
                        .map(|h| h.describe())
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
                Err(_) => "apply_patch (unparseable patch)".to_string(),
            },
            Action::RunCommand { command } => format!("$ {command}"),
            Action::StartProcess { command } => format!("start process: $ {command}"),
            Action::PollProcess { id } => format!("poll process #{id}"),
            Action::WriteProcess { id, input, eof } => {
                if *eof && input.is_empty() {
                    format!("close stdin of process #{id}")
                } else {
                    format!(
                        "write to process #{id}: {}",
                        input.lines().next().unwrap_or("")
                    )
                }
            }
            Action::StopProcess { id } => format!("stop process #{id}"),
            Action::FetchUrl { url } => format!("fetch {url}"),
            Action::WebSearch { query, .. } => format!("web search: {query}"),
            Action::Grep {
                pattern,
                path,
                ignore_case,
                context,
                regex,
            } => {
                let where_ = path.as_deref().unwrap_or(".");
                let mut flags = String::new();
                if *regex {
                    flags.push_str(" (regex)");
                }
                if *ignore_case {
                    flags.push_str(" (i)");
                }
                if let Some(c) = context {
                    flags.push_str(&format!(" ±{c}"));
                }
                format!("grep '{pattern}'{flags} in {where_}")
            }
            Action::Glob { pattern, path } => {
                let where_ = path.as_deref().unwrap_or(".");
                format!("glob '{pattern}' in {where_}")
            }
            Action::Lsp {
                op,
                file,
                line,
                column,
                ..
            } => {
                let at = match (file.as_deref(), *line, *column) {
                    (Some(f), Some(l), Some(c)) => format!(" {f}:{l}:{c}"),
                    (Some(f), _, _) => format!(" {f}"),
                    _ => String::new(),
                };
                format!("lsp {op}{at}")
            }
            Action::Git {
                subcommand,
                path,
                rev,
                limit,
                message,
            } => {
                // Same renderer the permission key and danger detector use, so
                // the prompt cannot under-report what actually runs.
                let mut s = git_command_line(
                    subcommand,
                    rev.as_deref(),
                    path.as_deref(),
                    message.as_deref(),
                );
                if let Some(n) = limit {
                    s.push_str(&format!(" (max {n})"));
                }
                s
            }
            Action::UpdatePlan { todos } => {
                let done = todos.iter().filter(|t| t.status == "completed").count();
                format!("plan update: {done}/{} done", todos.len())
            }
        }
    }

    /// Tools that only read state — always allowed, even in Plan mode.
    pub fn is_read_only(&self) -> bool {
        match self {
            Action::ListDir { .. }
            | Action::ReadFile { .. }
            | Action::ViewImage { .. }
            | Action::Grep { .. }
            | Action::Glob { .. }
            | Action::UpdatePlan { .. }
            | Action::FetchUrl { .. }
            | Action::PollProcess { .. }
            | Action::WebSearch { .. } => true,
            // `git` is read-only only for its inspection subcommands; `add`,
            // `commit`, `checkout` and friends all write.
            Action::Git { subcommand, .. } => GIT_READ.contains(&subcommand.as_str()),
            _ => false,
        }
    }

    /// True when this action is a pure read that can be awaited concurrently
    /// with the rest of its batch.
    ///
    /// Narrower than `is_read_only` on purpose. The read-only set also contains
    /// `view_image`, `update_plan` and `poll_process`, which are read-only but
    /// not order-independent: they append to the shared image list, update the
    /// on-screen plan, and advance process output respectively. Deferring those
    /// would reorder user-visible state.
    pub fn is_parallel_safe(&self) -> bool {
        match self {
            Action::ListDir { .. }
            | Action::ReadFile { .. }
            | Action::Grep { .. }
            | Action::Glob { .. }
            | Action::FetchUrl { .. }
            | Action::WebSearch { .. } => true,
            Action::Git { subcommand, .. } => GIT_READ.contains(&subcommand.as_str()),
            _ => false,
        }
    }

    /// True when this action can change files on disk (drives post-edit
    /// hooks and the undo snapshots).
    ///
    /// `git` is deliberately excluded even for its write subcommands: `checkout`
    /// and `stash` do rewrite the working tree, but which files they touched is
    /// only knowable by diffing against HEAD afterwards, and guessing would fire
    /// hooks on paths the user never edited. Git writes are also excluded from
    /// `/undo` — see `Agent::snapshot_for_undo`.
    pub fn mutates_files(&self) -> bool {
        matches!(
            self,
            Action::WriteFile { .. } | Action::EditFile { .. } | Action::ApplyPatch { .. }
        )
    }

    pub fn danger(&self, cwd: &Path) -> Danger {
        match self {
            Action::ListDir { .. }
            | Action::ReadFile { .. }
            | Action::ViewImage { .. }
            | Action::FetchUrl { .. }
            | Action::WebSearch { .. }
            | Action::Grep { .. }
            | Action::Glob { .. }
            | Action::PollProcess { .. }
            | Action::UpdatePlan { .. }
            | Action::Lsp { .. } => Danger::Safe,
            Action::Git {
                subcommand,
                rev,
                path,
                message,
                ..
            } => {
                if GIT_READ.contains(&subcommand.as_str()) {
                    return Danger::Safe;
                }
                // `checkout` and `stash` can discard or relocate uncommitted
                // work in ways the prompt cannot show, so they always ask.
                if GIT_ALWAYS_ASK.contains(&subcommand.as_str()) {
                    return Danger::High;
                }
                // Everything else runs through the same detector the shell
                // uses, so a pattern like `git commit` carrying a `rm -rf`
                // message is still classified High instead of slipping past
                // the shell-only path.
                let cmd = git_command_line(
                    subcommand,
                    rev.as_deref(),
                    path.as_deref(),
                    message.as_deref(),
                );
                if is_dangerous_command(&cmd) {
                    Danger::High
                } else {
                    Danger::Moderate
                }
            }
            Action::WriteFile { path, .. } | Action::EditFile { path, .. } => {
                // Writes outside the workspace always require explicit
                // approval, even in auto-edit / full-auto modes. Symlink-aware:
                // a link inside the workspace pointing out counts as outside.
                if contained_in_workspace(cwd, path) {
                    Danger::Moderate
                } else {
                    Danger::High
                }
            }
            Action::ApplyPatch { patch } => {
                match crate::patch::parse_patch(patch) {
                    Ok(hunks) => {
                        // Any hunk touching an outside/symlinked path → High;
                        // otherwise Moderate like ordinary edits.
                        let inside = |p: &Path| {
                            resolve_in(cwd, p.to_string_lossy().as_ref())
                                .map(|t| contained_in_workspace_path(cwd, &t))
                                .unwrap_or(false)
                        };
                        let all_inside = hunks.iter().all(|h| inside(h.classify_path()));
                        if all_inside {
                            Danger::Moderate
                        } else {
                            Danger::High
                        }
                    }
                    Err(_) => Danger::High,
                }
            }
            Action::RunCommand { command } => {
                if is_dangerous_command(command) {
                    Danger::High
                } else {
                    Danger::Moderate
                }
            }
            Action::StartProcess { command } => {
                if is_dangerous_command(command) {
                    Danger::High
                } else {
                    Danger::Moderate
                }
            }
            // Stdin may be code to an interpreter. Require explicit approval.
            Action::WriteProcess { .. } => Danger::High,
            Action::StopProcess { .. } => Danger::Moderate,
        }
    }

    /// Execute the action inside `cwd`, returning output plus unified diffs
    /// of any files this action mutated (empty for read-only actions).
    pub async fn perform_with_diff(
        &self,
        cwd: &Path,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<(String, Vec<crate::diff::FileDiff>)> {
        match self {
            // Mutating: capture pre-image → mutate → diff.
            Action::WriteFile { path, content } => {
                let p = resolve_in(cwd, path)?;
                let old = std::fs::read_to_string(&p).unwrap_or_default();
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                std::fs::write(&p, content.as_bytes())
                    .with_context(|| format!("writing {}", p.display()))?;
                let diff = crate::diff::unified_diff(path, &old, content, 3);
                Ok((
                    format!("wrote {} ({} bytes)", p.display(), content.len()),
                    if diff.is_empty() { vec![] } else { vec![diff] },
                ))
            }
            Action::EditFile { path, old, new } => {
                let p = resolve_in(cwd, path)?;
                let raw = std::fs::read_to_string(&p)
                    .with_context(|| format!("reading {}", p.display()))?;
                let count = raw.matches(old.as_str()).count();
                match count {
                    0 => bail!("pattern not found in {}. Nothing changed.", p.display()),
                    1 => {}
                    n => bail!(
                        "pattern appears {n} times in {}; refusing ambiguous edit. \
                         Provide more surrounding context.",
                        p.display()
                    ),
                }
                let updated = raw.replacen(old.as_str(), new.as_str(), 1);
                std::fs::write(&p, updated.as_bytes())
                    .with_context(|| format!("writing {}", p.display()))?;
                let diff = crate::diff::unified_diff(path, &raw, &updated, 3);
                Ok((
                    format!(
                        "edited {} (-{} +{} lines)",
                        p.display(),
                        old.lines().count(),
                        new.lines().count()
                    ),
                    if diff.is_empty() { vec![] } else { vec![diff] },
                ))
            }
            Action::ApplyPatch { patch } => {
                let hunks = crate::patch::parse_patch(patch)?;
                // Plan mode already blocks non-read-only actions; this extra
                // guard keeps outside-workspace hunks from sneaking through
                // when the caller skipped danger classification.
                for h in &hunks {
                    let target = resolve_in(cwd, h.classify_path().to_string_lossy().as_ref())?;
                    if !contained_in_workspace_path(cwd, &target) {
                        bail!(
                            "patch touches '{}' which is outside the workspace — refused",
                            h.classify_path().display()
                        );
                    }
                }
                let applied = crate::patch::apply_hunks(cwd, &hunks)?;
                Ok((applied.summary, applied.files))
            }
            // Everything else has no file mutation — reuse plain perform().
            other => {
                let out = Self::perform_plain(other, cwd, cancel).await?;
                Ok((out, Vec::new()))
            }
        }
    }

    /// The original read-only/command paths (no diff capture).
    async fn perform_plain(
        &self,
        cwd: &Path,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<String> {
        match self {
            Action::ViewImage { .. } => {
                bail!("view_image requires the agent's vision-message handler")
            }
            Action::ListDir { path } => {
                let root = resolve_in(cwd, path)?;
                let tree = build_tree(&root, 0, 2);
                Ok(tree)
            }
            Action::ReadFile {
                path,
                offset,
                limit,
            } => {
                let p = resolve_in(cwd, path)?;
                let meta =
                    std::fs::metadata(&p).with_context(|| format!("stat {}", p.display()))?;
                if meta.is_dir() {
                    return Ok(format!("[directory] {}", build_tree(&p, 0, 1)));
                }
                if meta.len() > MAX_READ_BYTES {
                    bail!(
                        "file too large ({}, limit {}). Read specific parts with run_command.",
                        meta.len(),
                        MAX_READ_BYTES
                    );
                }
                let raw = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
                let text = String::from_utf8_lossy(&raw);
                // cat -n style numbering helps the model anchor edit_file /
                // apply_patch hunks precisely.
                let all: Vec<&str> = text.lines().collect();
                let start = offset.unwrap_or(1).saturating_sub(1) as usize;
                if start >= all.len() && !all.is_empty() {
                    bail!(
                        "offset {} is past end of file ({} lines)",
                        offset.unwrap_or(1),
                        all.len()
                    );
                }
                let slice: Vec<(usize, &str)> = all
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(limit.map(|l| l as usize).unwrap_or(usize::MAX))
                    .map(|(i, l)| (i + 1, *l))
                    .collect();
                if slice.is_empty() && !all.is_empty() {
                    return Ok("(empty selection — file has no lines in range)".to_string());
                }
                let mut out = String::new();
                for (n, line) in &slice {
                    out.push_str(&format!("{n:>5}| {line}\n"));
                    if out.len() > MAX_TOOL_OUTPUT {
                        break;
                    }
                }
                if out.chars().count() > MAX_TOOL_OUTPUT {
                    let cut: String = out.chars().take(MAX_TOOL_OUTPUT).collect();
                    return Ok(format!(
                        "{cut}\n[truncated — {} lines shown of {}; continue with offset/limit]",
                        slice.len(),
                        all.len()
                    ));
                }
                let total = all.len();
                let last_shown = slice.last().map(|(n, _)| *n).unwrap_or(total);
                if limit.is_some() && last_shown < total {
                    out.push_str(&format!(
                        "[showing lines {}-{} of {} — pass offset={} for the next page]\n",
                        slice.first().map(|(n, _)| *n).unwrap_or(1),
                        last_shown,
                        total,
                        last_shown + 1
                    ));
                }
                Ok(out)
            }
            // Mutating actions never reach perform_plain — perform_with_diff
            // handles them (with diff capture) and routes only the rest here.
            Action::WriteFile { .. } | Action::EditFile { .. } | Action::ApplyPatch { .. } => {
                unreachable!("mutating actions are intercepted by perform_with_diff")
            }
            Action::RunCommand { command } => run_shell_cancellable(command, cwd, cancel).await,
            Action::StartProcess { .. }
            | Action::PollProcess { .. }
            | Action::WriteProcess { .. }
            | Action::StopProcess { .. }
            | Action::Lsp { .. } => {
                bail!("this tool requires an owning agent")
            }
            Action::FetchUrl { url } => fetch_url(url).await,
            Action::WebSearch { query, max_results } => web_search(query, *max_results).await,
            Action::Grep {
                pattern,
                path,
                ignore_case,
                context,
                regex,
            } => {
                let root = resolve_in(cwd, path.as_deref().unwrap_or("."))?;
                run_grep(&root, pattern, *ignore_case, context.unwrap_or(0), *regex)
            }
            Action::Glob { pattern, path } => {
                let root = resolve_in(cwd, path.as_deref().unwrap_or("."))?;
                run_glob(&root, pattern)
            }
            Action::Git {
                subcommand,
                path,
                rev,
                limit,
                message,
            } => {
                run_git(
                    cwd,
                    subcommand,
                    path.as_deref(),
                    rev.as_deref(),
                    *limit,
                    message.as_deref(),
                )
                .await
            }
            // The host (REPL) keeps the authoritative plan state; the model
            // only needs confirmation that the list was recorded.
            Action::UpdatePlan { todos } => {
                let mut out = String::from("Todo list updated:\n");
                for t in todos {
                    let mark = match t.status.as_str() {
                        "completed" => "[x]",
                        "in_progress" => "[~]",
                        _ => "[ ]",
                    };
                    out.push_str(&format!("  {mark} {}\n", t.content));
                }
                Ok(out)
            }
        }
    }
}

/// Run one `[hooks] post_edit` command. The touched paths are exported as
/// `LAUDACODE_CHANGED_FILES` (newline separated) so hooks can be generic
/// (`cargo fmt`, `cargo test`) without the agent re-running them. Output uses
/// the same `[exit: N]` report shape as `run_shell`.
pub async fn run_hook(
    command: &str,
    cwd: &Path,
    touched: &[String],
    timeout: std::time::Duration,
) -> Result<String> {
    let files = touched.join("\n");
    let child = spawn_shell(cwd, command, &[("LAUDACODE_CHANGED_FILES", &files)])?;
    capture_child(
        child,
        None,
        timeout,
        "interrupted while running hook",
        "hook",
    )
    .await
}

/// Recursive grep over text files, skipping binaries and junk dirs.
/// `context` > 0 includes that many surrounding lines per hit, groups
/// separated by `--` (grep -C style). With `regex` the pattern is compiled as
/// a regular expression; otherwise it is matched as literal text (the default,
/// so patterns like `foo(bar)` keep working verbatim).
fn run_grep(
    root: &Path,
    pattern: &str,
    ignore_case: bool,
    context: u32,
    regex: bool,
) -> Result<String> {
    const MAX_HITS: usize = 200;
    const MAX_OUTPUT_LINES: usize = 400;
    // Compiled matcher, or `None` for the literal fast path.
    let compiled = if regex {
        Some(
            regex::RegexBuilder::new(pattern)
                .case_insensitive(ignore_case)
                .size_limit(1 << 20)
                .build()
                .with_context(|| format!("invalid regex pattern: {pattern}"))?,
        )
    } else {
        None
    };
    let needle = if compiled.is_none() && ignore_case {
        pattern.to_lowercase()
    } else {
        pattern.to_string()
    };
    // (relative path, 1-based line no, line text) for each hit.
    let mut hits: Vec<(String, usize, String)> = Vec::new();
    let mut files_searched = 0usize;
    let mut stop = false;
    walk_files(root, &mut |p| {
        if stop || hits.len() >= MAX_HITS {
            stop = true;
            return false; // halt the whole walk
        }
        let Ok(raw) = std::fs::read(p) else {
            return true;
        };
        if raw.len() as u64 > MAX_READ_BYTES || raw.contains(&0u8) {
            return true; // skip huge or binary files
        }
        let text = String::from_utf8_lossy(&raw);
        files_searched += 1;
        let rel = p.strip_prefix(root).unwrap_or(p).display().to_string();
        for (i, line) in text.lines().enumerate() {
            let hit = match &compiled {
                Some(re) => re.is_match(line),
                None if ignore_case => line.to_lowercase().contains(&needle),
                None => line.contains(&needle),
            };
            if hit {
                hits.push((rel.clone(), i + 1, line.trim_end().to_string()));
                if hits.len() >= MAX_HITS {
                    stop = true;
                    break;
                }
            }
        }
        true
    });

    if hits.is_empty() {
        let hint =
            if !regex && pattern.contains(['.', '*', '+', '?', '(', '[', '|', '^', '$', '\\']) {
                "\nnote: patterns are matched literally — pass regex=true to use regex syntax"
            } else {
                ""
            };
        return Ok(format!(
            "no matches for '{pattern}' ({files_searched} files searched){hint}"
        ));
    }

    let truncated_hits = hits.len() >= MAX_HITS;
    let mut out = String::new();
    let mut out_lines = 0usize;
    if context == 0 {
        for (rel, n, line) in &hits {
            if out_lines >= MAX_OUTPUT_LINES {
                out.push_str("[output truncated]\n");
                break;
            }
            out.push_str(&format!("{rel}:{n}: {line}\n"));
            out_lines += 1;
        }
    } else {
        // Group hits per file, merging windows that overlap.
        let ctx = context as usize;
        let mut by_file: std::collections::BTreeMap<String, Vec<(usize, String)>> =
            std::collections::BTreeMap::new();
        for (rel, n, line) in &hits {
            by_file
                .entry(rel.clone())
                .or_default()
                .push((*n, line.clone()));
        }
        'files: for (rel, mut lines) in by_file {
            lines.sort_by_key(|(n, _)| *n);
            lines.dedup_by_key(|(n, _)| *n);
            let total = std::fs::read(root.join(&rel))
                .map(|b| String::from_utf8_lossy(&b).lines().count())
                .unwrap_or(0);
            let mut prev_end = 0usize; // last emitted line of previous window
            for &(hit_line, _) in &lines {
                let start = hit_line.saturating_sub(ctx).max(1);
                let end = (hit_line + ctx).min(total.max(hit_line));
                if out_lines >= MAX_OUTPUT_LINES {
                    out.push_str("[output truncated]\n");
                    break 'files;
                }
                if prev_end != 0 && start > prev_end + 1 {
                    out.push_str("--\n");
                    out_lines += 1;
                } else if prev_end != 0 {
                    // contiguous — no separator
                }
                let src = root.join(&rel);
                let Ok(content) = std::fs::read_to_string(&src) else {
                    continue;
                };
                for ln in start..=end {
                    if out_lines >= MAX_OUTPUT_LINES {
                        break 'files;
                    }
                    let text = content.lines().nth(ln - 1).unwrap_or("").trim_end();
                    let is_hit = lines.iter().any(|(n, _)| *n == ln);
                    let prefix = if is_hit { ":" } else { "-" };
                    out.push_str(&format!("{rel}{prefix}{ln}{prefix}{text}\n"));
                    out_lines += 1;
                    prev_end = ln;
                }
            }
        }
    }

    let mut footer = format!("\n{} matches", hits.len());
    if truncated_hits {
        footer.push_str(&format!(" [results truncated at {MAX_HITS}]"));
    }
    out.push_str(&footer);
    Ok(out)
}

/// Glob-style file search supporting `*`, `**`, and `?` via simple matching.
fn run_glob(root: &Path, pattern: &str) -> Result<String> {
    let mut hits: Vec<String> = Vec::new();
    let mut stop = false;
    walk_files(root, &mut |p| {
        if stop || hits.len() >= 300 {
            stop = true;
            return false;
        }
        let rel = p.strip_prefix(root).unwrap_or(p);
        if glob_match(pattern, &rel.display().to_string()) {
            hits.push(rel.display().to_string());
        }
        true
    });
    if hits.is_empty() {
        return Ok(format!("no files match '{pattern}'"));
    }
    Ok(format!("{}\n({} files)", hits.join("\n"), hits.len()))
}

/// Minimal glob: `**` crosses directories, `*` within one segment, `?` one char.
fn glob_match(pattern: &str, path: &str) -> bool {
    fn seg_match(pat: &[char], s: &[char]) -> bool {
        // Classic wildcard matcher with '*' support inside a segment.
        let (mut pi, mut si) = (0usize, 0usize);
        let (mut star_p, mut star_s) = (usize::MAX, 0usize);
        while si < s.len() {
            if pi < pat.len() && (pat[pi] == '?' || pat[pi] == s[si]) {
                pi += 1;
                si += 1;
            } else if pi < pat.len() && pat[pi] == '*' {
                star_p = pi;
                star_s = si;
                pi += 1;
            } else if star_p != usize::MAX {
                pi = star_p + 1;
                star_s += 1;
                si = star_s;
            } else {
                return false;
            }
        }
        while pi < pat.len() && pat[pi] == '*' {
            pi += 1;
        }
        pi == pat.len()
    }

    let pat_segs: Vec<&str> = pattern.split('/').collect();
    let path_segs: Vec<&str> = path.split('/').collect();

    // Handle `**` which can swallow zero or more segments.
    fn match_segs(pat: &[&str], path: &[&str]) -> bool {
        if pat.is_empty() {
            return path.is_empty();
        }
        if pat[0] == "**" {
            for skip in 0..=path.len() {
                if match_segs(&pat[1..], &path[skip..]) {
                    return true;
                }
            }
            false
        } else {
            !path.is_empty()
                && seg_match(
                    &pat[0].chars().collect::<Vec<_>>(),
                    &path[0].chars().collect::<Vec<_>>(),
                )
                && match_segs(&pat[1..], &path[1..])
        }
    }

    match_segs(&pat_segs, &path_segs)
}

/// Public wrapper so other modules (undo snapshots) can resolve paths the
/// same way tool execution does.
pub fn resolve_path_in(cwd: &Path, path: &str) -> Result<PathBuf> {
    resolve_in(cwd, path)
}

fn resolve_in(cwd: &Path, path: &str) -> Result<PathBuf> {
    let p = Path::new(path);
    let resolved = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    // Normalize "..".
    let mut out = PathBuf::new();
    for comp in resolved.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    Ok(out)
}

/// True when `path` (workspace-relative or absolute) resolves *inside* `cwd`
/// after following symlinks. Walks up to the nearest existing ancestor so
/// not-yet-created files are checked against their real parent too — a plain
/// lexical check would let a workspace symlink escape the sandbox.
pub fn contained_in_workspace(cwd: &Path, path: &str) -> bool {
    match resolve_in(cwd, path) {
        Ok(target) => contained_in_workspace_path(cwd, &target),
        Err(_) => false,
    }
}

/// `Path`-typed variant of `contained_in_workspace`.
pub fn contained_in_workspace_path(cwd: &Path, target: &Path) -> bool {
    let cwd_real = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut probe = target.to_path_buf();
    loop {
        match std::fs::canonicalize(&probe) {
            Ok(real) => return real.starts_with(&cwd_real),
            Err(_) => {
                // Component doesn't exist yet (new file/dir): test its parent.
                if !probe.pop() {
                    return false;
                }
            }
        }
    }
}

pub async fn run_shell(command: &str, cwd: &Path) -> Result<String> {
    run_shell_cancellable(command, cwd, None).await
}

/// `run_shell` with cooperative cancellation: the `cancel` flag is polled
/// every 200 ms while the child runs; a set flag kills the process group
/// (sh + children) and returns an interrupted marker so the agent loop can
/// unwind without waiting out the rest of the 180 s timeout.
pub async fn run_shell_cancellable(
    command: &str,
    cwd: &Path,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<String> {
    let child = spawn_shell(cwd, command, &[])?;
    capture_child(
        child,
        cancel,
        std::time::Duration::from_secs(180),
        "interrupted while running command",
        "command",
    )
    .await
}

/// `sh -c <command>` in `cwd`, stdin closed and both streams piped, with the
/// child in its own process group so a kill takes the whole tree.
fn spawn_shell(cwd: &Path, command: &str, envs: &[(&str, &str)]) -> Result<tokio::process::Child> {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.spawn().context("spawning shell (sh)")
}

/// Drain a spawned child's stdout/stderr, honouring a timeout and a
/// cooperative cancel flag. Shared by `sh -c` and the direct-exec tools
/// (`git`) so both report output in the same `[exit: N]` format.
async fn capture_child(
    child: tokio::process::Child,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    timeout: std::time::Duration,
    cancel_msg: &str,
    what: &str,
) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let mut child = child;
    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();
    // Drain both pipes concurrently: reading them sequentially deadlocks
    // once a child fills one pipe buffer while we block on the other.
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    // Cancellation poller: races the normal completion path; on cancel the
    // whole group dies (process_group above) instead of orphaning children.
    let check = async {
        if let Some(flag) = cancel {
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        } else {
            std::future::pending::<()>().await;
        }
    };
    let res = tokio::select! {
        r = tokio::time::timeout(timeout, async {
    let o = async {
        if let Some(o) = stdout_pipe.as_mut() {
            let _ = o.read_to_end(&mut out_buf).await;
        }
    };
    let e = async {
        if let Some(p) = stderr_pipe.as_mut() {
            let _ = p.read_to_end(&mut err_buf).await;
        }
    };
        let ((), (), status) = tokio::join!(o, e, child.wait());
        status
        }) => r,
        _ = check => {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL); }
            }
            let _ = child.kill().await;
            bail!("{cancel_msg}");
        }
    };

    let status = match res {
        Ok(s) => s.context("waiting for command")?,
        Err(_) => {
            let _ = child.kill().await;
            bail!("{what} timed out after {}s", timeout.as_secs())
        }
    };

    let mut report = String::new();
    let exit = status.code().unwrap_or(-1);
    report.push_str(&format!("[exit: {exit}]\n"));
    let so = String::from_utf8_lossy(&out_buf);
    let se = String::from_utf8_lossy(&err_buf);
    if !so.trim().is_empty() {
        report.push_str("--- stdout ---\n");
        report.push_str(&truncate(&so));
        report.push('\n');
    }
    if !se.trim().is_empty() {
        report.push_str("--- stderr ---\n");
        report.push_str(&truncate(&se));
        report.push('\n');
    }
    if so.trim().is_empty() && se.trim().is_empty() {
        report.push_str("(no output)\n");
    }
    Ok(report)
}

/// Run a git subcommand against `cwd`. Arguments are passed as a real argv
/// (never a shell string), so model-supplied paths/revs cannot inject extra
/// commands. `path` is a repo-relative pathspec, `rev` a revision or range,
/// and `limit` caps log output.
///
/// Local only: the subcommand allowlist (`GIT_READ` + `GIT_WRITE`) has no
/// remote operation, so nothing here can reach a network remote.
async fn run_git(
    cwd: &Path,
    subcommand: &str,
    path: Option<&str>,
    rev: Option<&str>,
    limit: Option<u32>,
    message: Option<&str>,
) -> Result<String> {
    anyhow::ensure!(
        GIT_READ.contains(&subcommand) || GIT_WRITE.contains(&subcommand),
        "unsupported git subcommand '{subcommand}'"
    );
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg(subcommand).current_dir(cwd);
    match subcommand {
        // Porcelain + branch header: stable, cheap, ideal for the model.
        "status" => {
            cmd.args(["--short", "--branch"]);
            if let Some(p) = path {
                cmd.arg("--").arg(p);
            }
        }
        "log" => {
            let n = limit.unwrap_or(20).clamp(1, 500);
            cmd.arg(format!("--max-count={n}"))
                .arg("--date=short")
                .arg("--pretty=format:%h %ad %an %s");
            if let Some(r) = rev {
                cmd.arg(r);
            }
            if let Some(p) = path {
                cmd.arg("--").arg(p);
            }
        }
        "diff" => {
            if let Some(r) = rev {
                cmd.arg(r);
            }
            if let Some(p) = path {
                cmd.arg("--").arg(p);
            }
        }
        "show" => {
            cmd.arg(rev.unwrap_or("HEAD"));
            if let Some(p) = path {
                cmd.arg("--").arg(p);
            }
        }
        "blame" => {
            let p = path.context("git blame needs a `path` to annotate")?;
            cmd.args(["--date=short", "-w"]);
            if let Some(r) = rev {
                cmd.arg(r);
            }
            cmd.arg("--").arg(p);
        }
        // Read-only list; extra operands are ignored so add/remove cannot
        // slip through the git tool.
        "worktree" => {
            cmd.args(["list", "--porcelain"]);
        }
        // ---- writes ----
        // No pathspec stages the whole tree. `-A` so deletions are staged too:
        // a bare `git add` silently skips them and the model reports a commit
        // that is missing files it thought it added.
        "add" => match path {
            Some(p) => {
                cmd.arg("--").arg(p);
            }
            None => {
                cmd.arg("-A");
            }
        },
        "commit" => {
            // Null stdin below means no editor; without -m git would sit there
            // until the timeout. parse_tool_action already rejects this.
            let m = message.context("git commit needs a `message`")?;
            cmd.arg("-m").arg(m);
        }
        // `rev` is the branch name; with none, this lists branches.
        "branch" => {
            if let Some(r) = rev {
                cmd.arg(r);
            }
        }
        // `rev` switches branches; with a `path` and no `rev` this restores
        // that one file from HEAD, discarding its local edits.
        "checkout" => match (rev, path) {
            (Some(r), Some(p)) => {
                cmd.arg(r).arg("--").arg(p);
            }
            (Some(r), None) => {
                cmd.arg(r);
            }
            (None, Some(p)) => {
                cmd.arg("--").arg(p);
            }
            (None, None) => {
                cmd.arg("--").arg(".");
            }
        },
        // `rev` is the stash verb: push (default), pop, list, drop, show.
        "stash" => {
            if let Some(r) = rev {
                cmd.arg(r);
            }
        }
        _ => unreachable!("subcommand validated above"),
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    cmd.process_group(0);
    let child = cmd
        .spawn()
        .context("spawning git — is it installed and on PATH?")?;
    let report = capture_child(
        child,
        None,
        std::time::Duration::from_secs(60),
        "interrupted while running git",
        "git",
    )
    .await?;
    // A non-zero exit is informative (not a crash): keep it, prefixed so the
    // model can tell "no commits yet" from a real failure.
    Ok(report)
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_TOOL_OUTPUT {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(MAX_TOOL_OUTPUT).collect();
        t.push_str("\n[output truncated]");
        t
    }
}

const FETCH_LIMIT: usize = 16 * 1024;
/// Hard cap on downloaded bytes — phones (Termux) have tight RAM budgets.
const FETCH_MAX_BYTES: u64 = 2 * 1024 * 1024;

async fn fetch_url(url: &str) -> Result<String> {
    use futures_util::StreamExt;
    if !url.starts_with("http://") && !url.starts_with("https://") {
        bail!("only http(s) URLs are supported");
    }
    let client = crate::api::build_web_client(
        std::time::Duration::from_secs(20),
        std::time::Duration::from_secs(45),
    )?;
    let resp = client.get(url).send().await.context("request failed")?;
    let status = resp.status();
    if let Some(len) = resp.content_length() {
        if len > FETCH_MAX_BYTES {
            bail!("response too large ({len} bytes, limit {FETCH_MAX_BYTES})");
        }
    }
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    // Read incrementally so oversized bodies can't balloon memory.
    let mut stream = resp.bytes_stream();
    let mut body: Vec<u8> = Vec::with_capacity(64 * 1024);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading response body")?;
        if body.len() as u64 + chunk.len() as u64 > FETCH_MAX_BYTES {
            bail!("response exceeded {FETCH_MAX_BYTES} bytes — aborted mid-download");
        }
        body.extend_from_slice(&chunk);
    }
    let body = String::from_utf8_lossy(&body).into_owned();

    let header = format!("[{url}] [HTTP {status}]\n");
    let text = if ctype.contains("text/html") || ctype.contains("application/xhtml") {
        html_to_text(&body)
    } else {
        body
    };
    let mut out: String = text.chars().take(FETCH_LIMIT).collect();
    if text.chars().count() > FETCH_LIMIT {
        out.push_str("\n[content truncated]");
    }
    Ok(header + out.trim())
}

/// Web search via DuckDuckGo's HTML endpoint (no API key, no new dependencies).
/// Returns a list of result titles + URLs so the agent can follow up with
/// `fetch_url` for the pages that matter.
async fn web_search(query: &str, max_results: usize) -> Result<String> {
    use futures_util::StreamExt;
    let url = "https://html.duckduckgo.com/html/?q=".to_string() + &urlencoding(query);
    let client = crate::api::build_web_client(
        std::time::Duration::from_secs(20),
        std::time::Duration::from_secs(45),
    )?;
    let resp = client
        .get(&url)
        .send()
        .await
        .context("search request failed")?;
    let status = resp.status();
    let mut body: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading search response")?;
        if body.len() as u64 + chunk.len() as u64 > FETCH_MAX_BYTES {
            bail!("search response exceeded {FETCH_MAX_BYTES} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    let html = String::from_utf8_lossy(&body).into_owned();
    let results = parse_ddg_results(&html, max_results);
    if results.is_empty() {
        return Ok(format!(
            "[web_search] no results for \"{query}\" [HTTP {status}]"
        ));
    }
    let mut out = String::new();
    for (i, (title, link)) in results.iter().enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, title, link));
    }
    Ok(format!("[web_search: {query}] [HTTP {status}]\n{out}")
        .trim()
        .to_string())
}

/// Naive DuckDuckGo HTML result parser: extracts `result__a` anchors.
fn parse_ddg_results(html: &str, max_results: usize) -> Vec<(String, String)> {
    let mut results = Vec::new();
    let bytes: Vec<char> = html.chars().collect();
    let mut i = 0usize;
    let mut count = 0usize;
    while i < bytes.len() {
        // Scan for `<a ... class="result__a"` anchors.
        if bytes.get(i).copied() == Some('<') {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != '>' {
                j += 1;
            }
            let tag: String = bytes[i + 1..j.min(bytes.len())].iter().collect();
            if tag.to_lowercase().contains("result__a") {
                // Extract href.
                let href = extract_attr(&tag, "href").unwrap_or_default();
                // Find the closing </a> and take the inner text.
                let mut k = j + 1;
                while k + 3 < bytes.len()
                    && !(bytes[k] == '<'
                        && bytes.get(k + 1).copied() == Some('/')
                        && bytes.get(k + 2).map(|c| c.to_ascii_lowercase()) == Some('a')
                        && bytes.get(k + 3).copied() == Some('>'))
                {
                    k += 1;
                }
                let inner: String = bytes[j + 1..k.min(bytes.len())].iter().collect();
                let title = strip_html(&inner);
                if !title.trim().is_empty() && !href.is_empty() {
                    results.push((title.trim().to_string(), href));
                    count += 1;
                    if count >= max_results {
                        break;
                    }
                }
                i = k;
                continue;
            }
            i = j.saturating_add(1);
            continue;
        }
        i += 1;
    }
    results
}

fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_lowercase();
    let name = name.to_lowercase();
    let pos = lower.find(&name)?;
    let after = lower[pos + name.len()..].trim_start();
    let rest2 = after.strip_prefix('=')?.trim_start();
    let val = if let Some(r) = rest2.strip_prefix('"') {
        let end = r.find('"')?;
        r[..end].to_string()
    } else {
        let end = rest2
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(rest2.len());
        rest2[..end].trim().to_string()
    };
    // DuckDuckGo returns redirect URLs wrapped in //duckduckgo.com/l/?uddg=...
    let full = if val.starts_with("//") {
        format!("https:{val}")
    } else {
        val
    };
    Some(full_redirect(&full))
}

fn full_redirect(url: &str) -> String {
    if let Some(idx) = url.find("uddg=") {
        let enc = &url[idx + 5..];
        let bytes = enc.split('&').next().unwrap_or(enc);
        if let Some(dec) = urlencoding_decode(bytes) {
            return dec;
        }
    }
    url.to_string()
}

fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urlencoding_decode(s: &str) -> Option<String> {
    let bytes: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c == '%' {
            let hi = bytes.get(i + 1)?.to_digit(16)? as u8;
            let lo = bytes.get(i + 2)?.to_digit(16)? as u8;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(c as u8);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&#x27;", "'")
        .replace("&quot;", "\"")
}

/// Minimal HTML → text conversion (no regex dependency).
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let bytes: Vec<char> = html.chars().collect();
    let mut i = 0usize;
    let mut skip_tag = 0usize; // 0 = normal, else nesting depth of skipped element
    while i < bytes.len() {
        let c = bytes[i];
        if c == '<' {
            // Find end of tag.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != '>' {
                j += 1;
            }
            let tag: String = bytes[i + 1..j.min(bytes.len())].iter().collect();
            let lower = tag.to_lowercase();
            let name: String = lower
                .trim_start_matches('/')
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric())
                .collect();
            let closing = lower.starts_with('/');
            let self_closing = tag.ends_with('/');

            match name.as_str() {
                "script" | "style" | "noscript" | "head" | "svg" | "iframe" => {
                    if !closing && !self_closing {
                        skip_tag += 1;
                    } else if closing {
                        skip_tag = skip_tag.saturating_sub(1);
                    }
                }
                "br" | "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
                | "section" | "article" | "pre"
                    if skip_tag == 0 =>
                {
                    out.push('\n');
                }
                _ => {}
            }
            i = j + 1;
            continue;
        }
        if skip_tag == 0 {
            if c == '&' {
                // Decode common entities.
                let rest: String = bytes[i..(i + 10).min(bytes.len())].iter().collect();
                let decoded = if let Some(semi) = rest.find(';') {
                    let ent = &rest[1..semi];
                    match ent {
                        "amp" => Some('&'),
                        "lt" => Some('<'),
                        "gt" => Some('>'),
                        "quot" => Some('"'),
                        "apos" => Some('\''),
                        "nbsp" => Some(' '),
                        e if e.starts_with("#x") || e.starts_with("#X") => {
                            u32::from_str_radix(&e[2..], 16)
                                .ok()
                                .and_then(char::from_u32)
                        }
                        e if e.starts_with('#') => {
                            e[1..].parse::<u32>().ok().and_then(char::from_u32)
                        }
                        _ => None,
                    }
                    .map(|ch| {
                        i += semi; // consume entity
                        ch
                    })
                } else {
                    None
                };
                if let Some(ch) = decoded {
                    out.push(ch);
                    // `i` currently sits on the entity's final char before
                    // ';' (advanced by `semi`); consume the ';' itself.
                    i += 1;
                    continue;
                }
                out.push('&');
            } else {
                out.push(c);
            }
        }
        i += 1;
    }
    // Collapse excessive blank lines.
    let mut collapsed = String::with_capacity(out.len());
    let mut blanks = 0;
    for line in out.lines() {
        let l = line.trim_end();
        if l.is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        collapsed.push_str(l);
        collapsed.push('\n');
    }
    collapsed
}

const DANGEROUS_PATTERNS: &[&str] = &[
    "sudo ",
    "sudo\t",
    "mkfs",
    "dd if=",
    ":(){",
    "fork()",
    "shutdown",
    "reboot",
    "halt",
    "init 0",
    "chmod -r 777 /",
    "chown -r",
    "> /dev/sd",
    "/dev/mem",
    "| sh",
    "| bash",
    "|zsh",
    "| zsh",
    "|sh",
    "| bash -",
    "curl |",
    "wget |",
    "git push --force",
    "git push -f",
    "history -c",
    "kill -9 1",
    "killall -9",
    " -delete",
    "git reset --hard",
    "git clean -fd",
    "drop table",
    "truncate -s 0 /",
    "mkswap",
    "base64 -d |",
];

pub fn is_dangerous_command(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    for pat in DANGEROUS_PATTERNS {
        if lower.contains(pat) {
            return true;
        }
    }
    // rm -rf targeting root/home/system-ish paths.
    if lower.contains("rm ")
        && (lower.contains("-rf") || lower.contains("-fr") || lower.contains("-r -f"))
    {
        let targets_root = [
            " /", "~/", "$home", "/*", "/etc", "/usr", "/var", "/system", "/data",
        ];
        if targets_root.iter().any(|t| {
            lower.contains(&format!("rm{t}"))
                || lower.contains(&format!("rm -rf{t}"))
                || lower.contains(&format!("rm -fr{t}"))
        }) {
            return true;
        }
        return true; // any recursive force delete deserves a second look
    }
    false
}

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    "__pycache__",
    ".venv",
    "venv",
    "vendor",
    ".cache",
    ".gradle",
    ".idea",
    ".next",
];

/// Recursive directory listing used by both list_dir and the initial context.
pub fn build_tree(root: &Path, depth: usize, max_depth: usize) -> String {
    if depth > max_depth {
        return String::new();
    }
    let mut lines = Vec::new();
    let mut entries: Vec<_> = match std::fs::read_dir(root) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return String::new(),
    };
    entries.sort_by_key(|e| e.file_name());
    let mut shown = 0usize;
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if SKIP_DIRS.contains(&name.as_str()) || name.starts_with('.') && name != ".env.example" {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let indent = "  ".repeat(depth);
        if is_dir {
            lines.push(format!("{indent}{name}/"));
            let sub = build_tree(&entry.path(), depth + 1, max_depth);
            if !sub.is_empty() {
                lines.push(sub);
            }
        } else {
            lines.push(format!("{indent}{name}"));
        }
        shown += 1;
        if shown >= 300 {
            lines.push(format!("{indent}..."));
            break;
        }
    }
    lines.join("\n")
}

pub fn project_overview(cwd: &Path) -> String {
    let mut s = String::from("Project tree (depth 2):\n");
    let tree = build_tree(cwd, 0, 2);
    if tree.trim().is_empty() {
        s.push_str("(empty directory)\n");
    } else {
        s.push_str(&tree);
        s.push('\n');
    }
    if let Ok(gi) = std::fs::read_to_string(cwd.join(".gitignore")) {
        let interesting: Vec<&str> = gi
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .take(20)
            .collect();
        if !interesting.is_empty() {
            s.push_str(&format!(".gitignore: {}\n", interesting.join(", ")));
        }
    }
    s
}

pub fn tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            r#type: "function",
            function: FunctionDef {
                name: Cow::Borrowed("view_image"),
                description: Cow::Borrowed("View a local PNG, JPEG, GIF, or WebP using model vision (requires a vision-capable model). Maximum 8 MiB. Image content is attached after this tool batch, not returned as text."),
                parameters: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string", "description": "Local image path, relative to the workspace or absolute"}},
                    "required": ["path"]
                }),
            },
        },
        ToolDef {
            r#type: "function",
            function: FunctionDef {
                name: Cow::Borrowed("list_dir"),
                description: Cow::Borrowed("List files and folders under a directory (recursive, depth-limited)."),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Directory path, relative to cwd. Optional, defaults to '.'"}
                    }
                }),
            },
        },
        ToolDef {
            r#type: "function",
            function: FunctionDef {
                name: Cow::Borrowed("read_file"),
                description: Cow::Borrowed("Read a file with line numbers (cat -n style). Large files are paginated — pass offset/limit to continue."),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path, relative to cwd"},
                        "offset": {"type": "integer", "description": "1-based line number to start from (default 1)"},
                        "limit": {"type": "integer", "description": "Max lines to return"}
                    },
                    "required": ["path"]
                }),
            },
        },
        ToolDef {
            r#type: "function",
            function: FunctionDef {
                name: Cow::Borrowed("write_file"),
                description: Cow::Borrowed("Create a file or overwrite it completely with new content."),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }),
            },
        },
        ToolDef {
            r#type: "function",
            function: FunctionDef {
                name: Cow::Borrowed("edit_file"),
                description: Cow::Borrowed("Replace an exact unique substring in a file. old_string must match exactly once. For multi-file or multi-hunk changes prefer apply_patch."),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "old_string": {"type": "string", "description": "Exact text to find (must be unique)"},
                        "new_string": {"type": "string", "description": "Replacement text"}
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
            },
        },
        ToolDef {
            r#type: "function",
            function: FunctionDef {
                name: Cow::Borrowed("apply_patch"),
                description: Cow::Borrowed("Apply a patch that can add, update (with move/rename), and delete multiple files in one call. Format:\n*** Begin Patch\n*** Add File: path\n+lines\n*** Update File: path\n@@ optional context anchor\n-old\n+new\n*** Delete File: path\n*** End Patch\nContext lines start with a space; '@@ <line>' anchors the next chunk; '*** End of File' appends at EOF."),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "patch": {"type": "string", "description": "The full patch text including *** Begin Patch / *** End Patch markers"}
                    },
                    "required": ["patch"]
                }),
            },
        },
        ToolDef {
            r#type: "function",
            function: FunctionDef {
                name: Cow::Borrowed("run_command"),
                description: Cow::Borrowed("Execute a shell command (sh -c) in the working directory and return its output."),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"}
                    },
                    "required": ["command"]
                }),
            },
        },
        ToolDef {
            r#type: "function",
            function: FunctionDef {
                name: Cow::Borrowed("fetch_url"),
                description: Cow::Borrowed("Fetch content from an http(s) URL. HTML pages are converted to plain text. Use for docs, API references, error lookups."),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "Absolute URL starting with http:// or https://"}
                    },
                    "required": ["url"]
                }),
            },
        },
        ToolDef {
            r#type: "function",
            function: FunctionDef {
                name: Cow::Borrowed("web_search"),
                description: Cow::Borrowed("Search the web and get a list of result titles + URLs for a query. Returns 1-10 top results (no API key needed). Use fetch_url on a result URL to read the full page."),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search terms"},
                        "max_results": {"type": "integer", "description": "Max results to return (1-10, default 5)"}
                    },
                    "required": ["query"]
                }),
            },
        },
        ToolDef {
            r#type: "function",
            function: FunctionDef {
                name: Cow::Borrowed("grep"),
                description: Cow::Borrowed("Search file contents across the project and return file:line: match. By default the pattern is matched as literal text; set regex=true for regular expressions (Rust/PCRE-ish syntax: \\d \\w \\s, alternation, anchors). Skips binaries, .git, target/, node_modules/. Combine with context=N for surrounding lines."),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Text to search for (literal, or a regex when regex=true)"},
                        "path": {"type": "string", "description": "Directory or file to search, relative to cwd. Optional."},
                        "ignore_case": {"type": "boolean", "description": "Case-insensitive search"},
                        "context": {"type": "integer", "description": "Lines of context around each match (grep -C)"},
                        "regex": {"type": "boolean", "description": "Treat pattern as a regular expression instead of literal text (default false)"}
                    },
                    "required": ["pattern"]
                }),
            },
        },
        ToolDef {
            r#type: "function",
            function: FunctionDef {
                name: Cow::Borrowed("git"),
                description: Cow::Borrowed("Inspect and commit locally. READ: status (short porcelain + branch), log (oneline history, newest first), diff (working-tree or vs a rev), show (one commit/its diff), blame (line authors for a file). WRITE: add (stage a pathspec, or everything with no path), commit (needs a message), branch (create, or list with no rev), checkout (switch rev, or restore a path), stash (push/pop/list/drop via rev). Optionally restrict with a pathspec `path` and a revision/range `rev`; cap log lines with `limit` (default 20). There is no push, fetch or reset here — use run_command for those. Not available in Plan mode for the write subcommands."),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "subcommand": {"type": "string", "enum": git_subcommands(), "description": "Git subcommand. Local only — no push/fetch/reset."},
                        "path": {"type": "string", "description": "Optional repo-relative pathspec. For `add` this is what gets staged (omit to stage everything); for `checkout` it restores that path from HEAD."},
                        "rev": {"type": "string", "description": "Revision, range or name, depending on subcommand: log/diff/show/blame take a rev or range (e.g. HEAD, HEAD~3, main, HEAD~5..HEAD); `branch` takes the new branch name; `checkout` takes the branch to switch to; `stash` takes push|pop|list|drop|show"},
                        "limit": {"type": "integer", "description": "Max entries for log (default 20, max 500)"},
                        "message": {"type": "string", "description": "Commit message. Required for `commit`."}
                    },
                    "required": ["subcommand"]
                }),
            },
        },
        ToolDef {
            r#type: "function",
            function: FunctionDef {
                name: Cow::Borrowed("lsp"),
                description: Cow::Borrowed("Query a language server for real compiler diagnostics and code navigation. OPS: diagnostics (errors/warnings for a file, or for every file the server has published about when `file` is omitted), definition (where a symbol is declared), references (every use of a symbol), hover (type signature and docs at a position), symbols (outline of a file, or a workspace-wide search when given `query` instead of `file`). line/column are 1-BASED, matching the line numbers read_file prints. `diagnostics` on a large or cold project may report that analysis is still running rather than a clean result — that is not the same as 'no errors', so re-check before concluding the file is fine."),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "op": {"type": "string", "enum": LSP_OPS, "description": "What to ask the language server."},
                        "file": {"type": "string", "description": "Repo-relative path. Required for definition/references/hover, optional for diagnostics (omit to get every file) and symbols."},
                        "line": {"type": "integer", "description": "1-based line number, as printed by read_file. Required for definition/references/hover."},
                        "column": {"type": "integer", "description": "1-based column. Required for definition/references/hover."},
                        "query": {"type": "string", "description": "For `symbols` with no `file`: a workspace-wide symbol search string."}
                    },
                    "required": ["op"]
                }),
            },
        },
        ToolDef {
            r#type: "function",
            function: FunctionDef {
                name: Cow::Borrowed("glob"),
                description: Cow::Borrowed("Find files by pattern. Supports *, ** (any depth), ? (single char). Example: src/**/*.rs finds all Rust files under src/."),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Glob pattern like 'src/**/*.rs'"},
                        "path": {"type": "string", "description": "Root directory for the search. Optional."}
                    },
                    "required": ["pattern"]
                }),
            },
        },
        ToolDef {
            r#type: "function",
            function: FunctionDef {
                name: Cow::Borrowed("start_process"),
                description: Cow::Borrowed("Start a long-lived process that keeps running across tool calls (dev server, watcher, REPL). Returns its id. Poll output with poll_process. Plain pipes, not a PTY."),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "Shell command to run (via sh -c)"}
                    },
                    "required": ["command"]
                }),
            },
        },
        ToolDef {
            r#type: "function",
            function: FunctionDef {
                name: Cow::Borrowed("poll_process"),
                description: Cow::Borrowed("Get a managed process's status and latest output tails (8 KiB per stream, older output discarded). Read-only; safe to call repeatedly."),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "integer", "description": "Process id from start_process"}
                    },
                    "required": ["id"]
                }),
            },
        },
        ToolDef {
            r#type: "function",
            function: FunctionDef {
                name: Cow::Borrowed("write_process"),
                description: Cow::Borrowed("Send input to a managed process's stdin (newline appended). eof=true instead closes stdin — the 'done writing' signal for REPLs. Returns the process's reaction."),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "integer", "description": "Process id from start_process"},
                        "input": {"type": "string", "description": "Text to send (omit when eof=true)"},
                        "eof": {"type": "boolean", "description": "true closes stdin instead of writing"}
                    },
                    "required": ["id"]
                }),
            },
        },
        ToolDef {
            r#type: "function",
            function: FunctionDef {
                name: Cow::Borrowed("stop_process"),
                description: Cow::Borrowed("Kill a managed process (and its children) and free its id. Returns its last output."),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "integer", "description": "Process id from start_process"}
                    },
                    "required": ["id"]
                }),
            },
        },
        ToolDef {
            r#type: "function",
            function: FunctionDef {
                name: Cow::Borrowed("update_plan"),
                description: Cow::Borrowed("Write your task plan. Use for any multi-step work: create steps at the start, mark exactly one step in_progress while working on it, mark completed as you finish each. Replace the whole list every call."),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "plan": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "step": {"type": "string", "description": "Short imperative task title"},
                                    "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                                },
                                "required": ["step", "status"]
                            }
                        }
                    },
                    "required": ["plan"]
                }),
            },
        },
    ]
}

/// Read-only toolset used in Plan mode — exploration without mutation.
pub fn plan_tool_defs() -> Vec<ToolDef> {
    tool_defs()
        .into_iter()
        .filter(|t| {
            matches!(
                t.function.name.as_ref(),
                "list_dir"
                    | "read_file"
                    | "view_image"
                    | "grep"
                    | "glob"
                    | "fetch_url"
                    | "git"
                    | "lsp"
            )
        })
        // `git` stays in the Plan toolset for its reads, so the subcommand enum
        // is narrowed to the read half. Without this the model is shown
        // `commit`/`checkout`, calls one, and gets the generic "you are in
        // PLAN mode" rejection instead of a capability it actually has.
        .map(|mut t| {
            if t.function.name == "git" {
                t.function.parameters["properties"]["subcommand"]["enum"] = json!(GIT_READ);
                t.function.parameters["properties"]["subcommand"]["description"] = json!(
                    "Read-only git subcommand (Plan mode hides add/commit/branch/checkout/stash)"
                );
            }
            t
        })
        .collect()
}

/// Recursively visit files under `root`, skipping junk dirs and dotfiles.
/// The visitor returns `false` to abort the entire walk (callers use this to
/// stop early once result caps are reached instead of grinding through huge
/// trees like node_modules).
pub fn walk_files(root: &Path, f: &mut dyn FnMut(&Path) -> bool) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        if name.starts_with('.') && name != ".env.example" {
            continue;
        }
        let path = entry.path();
        let is_dir = matches!(entry.file_type(), Ok(ref t) if t.is_dir());
        let is_file = matches!(entry.file_type(), Ok(ref t) if t.is_file());
        if is_dir {
            walk_files(&path, f);
        } else if is_file && !f(&path) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_star_within_segment() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "src/main.rs"));
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "src/deep/main.rs"));
    }

    #[test]
    fn glob_double_star_crosses_dirs() {
        assert!(glob_match("src/**/*.rs", "src/a.rs"));
        assert!(glob_match("src/**/*.rs", "src/deep/nested/a.rs"));
        assert!(!glob_match("src/**/*.rs", "other/a.rs"));
        assert!(glob_match("**/*.md", "docs/guide/intro.md"));
    }

    #[test]
    fn glob_question_mark() {
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "abbc"));
    }

    #[test]
    fn grep_literal_by_default_and_regex_on_request() {
        let dir = std::env::temp_dir().join(format!("lc-grep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "let total = 41;\nlet x = 7;\n").unwrap();
        // Literal (default): the metacharacters are matched as-is.
        let literal = run_grep(&dir, r"\d+", false, 0, false).unwrap();
        assert!(
            literal.contains("no matches"),
            "literal mode must not interpret regex: {literal}"
        );
        // Regex: \d+ matches the numbers.
        let re = run_grep(&dir, r"\d+", false, 0, true).unwrap();
        assert!(re.contains("a.rs:1: let total = 41;"), "{re}");
        assert!(re.contains("a.rs:2: let x = 7;"), "{re}");
        // Regex ignore_case.
        let ic = run_grep(&dir, "LET", true, 0, true).unwrap();
        assert!(ic.contains("2 matches"), "{ic}");
        // Context window rendering still works in regex mode.
        let ctx = run_grep(&dir, r"let x", false, 1, true).unwrap();
        assert!(
            ctx.contains("a.rs-1-let total = 41;"),
            "context line: {ctx}"
        );
        assert!(ctx.contains("a.rs:2:let x = 7;"), "hit line: {ctx}");
        // An invalid regex is a clean error, not a panic.
        assert!(run_grep(&dir, "(unclosed", false, 0, true).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn git_tool_parses_reads_and_classifies_them_safe() {
        let cwd = std::env::current_dir().unwrap();
        let args = r#"{"subcommand":"status"}"#;
        match parse_tool_action("git", args).unwrap() {
            Action::Git {
                subcommand,
                path,
                rev,
                limit,
                message,
            } => {
                assert_eq!(subcommand, "status");
                assert!(path.is_none() && rev.is_none() && limit.is_none());
                assert!(message.is_none());
            }
            other => panic!("wrong action: {other:?}"),
        }
        // Lowercasing + limit passthrough.
        match parse_tool_action(
            "git",
            r#"{"subcommand":"LOG","limit":5,"rev":"HEAD~2..HEAD"}"#,
        )
        .unwrap()
        {
            Action::Git {
                subcommand,
                rev,
                limit,
                ..
            } => {
                assert_eq!(subcommand, "log");
                assert_eq!(rev.as_deref(), Some("HEAD~2..HEAD"));
                assert_eq!(limit, Some(5));
            }
            other => panic!("wrong action: {other:?}"),
        }
        let a = parse_tool_action("git", args).unwrap();
        assert!(a.is_read_only(), "git reads are read-only");
        assert_eq!(a.danger(&cwd), Danger::Safe);
        assert!(parse_tool_action("git", "{}").is_err());
    }

    #[test]
    fn git_allowlist_is_local_only() {
        // Remote and history-rewriting operations stay behind run_command,
        // which runs the dangerous-command detector on a real command line.
        for sub in [
            "push", "fetch", "pull", "reset", "rebase", "remote", "clean",
        ] {
            assert!(
                parse_tool_action("git", &format!(r#"{{"subcommand":"{sub}"}}"#)).is_err(),
                "`git {sub}` must not be reachable through the git tool"
            );
        }
        // And no write subcommand is a read.
        let cwd = Path::new(".");
        for sub in GIT_WRITE {
            let json = if *sub == "commit" {
                r#"{"subcommand":"commit","message":"m"}"#.to_string()
            } else {
                format!(r#"{{"subcommand":"{sub}"}}"#)
            };
            let a = parse_tool_action("git", &json).unwrap();
            assert!(!a.is_read_only(), "git {sub} must not be read-only");
            assert_ne!(a.danger(cwd), Danger::Safe, "git {sub}");
        }
        for sub in GIT_READ {
            let a = parse_tool_action("git", &format!(r#"{{"subcommand":"{sub}"}}"#)).unwrap();
            assert!(a.is_read_only(), "git {sub} must stay read-only");
        }
    }

    #[test]
    fn git_danger_escalates_for_destructive_writes() {
        let cwd = Path::new(".");
        // Plain writes are Moderate, so BUILD auto-approves them.
        for args in [
            r#"{"subcommand":"add","path":"src/main.rs"}"#,
            r#"{"subcommand":"commit","message":"fix: retry loop"}"#,
            r#"{"subcommand":"branch","rev":"feature"}"#,
        ] {
            assert_eq!(
                parse_tool_action("git", args).unwrap().danger(cwd),
                Danger::Moderate,
                "{args}"
            );
        }
        // checkout/stash can drop uncommitted work the prompt cannot show.
        for args in [
            r#"{"subcommand":"checkout","rev":"main"}"#,
            r#"{"subcommand":"stash","rev":"drop"}"#,
        ] {
            assert_eq!(
                parse_tool_action("git", args).unwrap().danger(cwd),
                Danger::High,
                "{args}"
            );
        }
        // A dangerous payload smuggled through an otherwise-Moderate write is
        // still High, because danger() runs the same detector the shell uses.
        let sneaky = parse_tool_action(
            "git",
            r#"{"subcommand":"commit","message":"rm -rf ~/important"}"#,
        )
        .unwrap();
        assert_eq!(sneaky.danger(cwd), Danger::High);
    }

    #[test]
    fn git_commit_requires_a_message() {
        // stdin is null when git runs, so a message-less commit would hang
        // until the timeout instead of failing usefully.
        assert!(parse_tool_action("git", r#"{"subcommand":"commit"}"#).is_err());
        assert!(parse_tool_action("git", r#"{"subcommand":"commit","message":"  "}"#).is_err());
        // Whitespace is trimmed rather than passed through as a blank message.
        match parse_tool_action("git", r#"{"subcommand":"commit","message":"  real  "}"#).unwrap() {
            Action::Git { message, .. } => assert_eq!(message.as_deref(), Some("real")),
            other => panic!("wrong action: {other:?}"),
        }
    }

    #[test]
    fn git_command_line_agrees_across_prompt_permissions_and_danger() {
        // One renderer, so a rule written for the shell still matches the tool.
        // Defaults are shown, not elided: the prompt must not under-report
        // what `run_git` actually spawns.
        for (args, expected) in [
            (
                r#"{"subcommand":"commit","message":"fix: retry"}"#,
                "git commit -m fix: retry",
            ),
            (r#"{"subcommand":"add"}"#, "git add -A"),
            (r#"{"subcommand":"add","path":"a.rs"}"#, "git add -- a.rs"),
            (r#"{"subcommand":"branch"}"#, "git branch"),
            (
                r#"{"subcommand":"branch","rev":"feature"}"#,
                "git branch feature",
            ),
            (r#"{"subcommand":"checkout"}"#, "git checkout -- ."),
            (
                r#"{"subcommand":"checkout","rev":"main"}"#,
                "git checkout main",
            ),
            (
                r#"{"subcommand":"checkout","rev":"main","path":"a.rs"}"#,
                "git checkout main -- a.rs",
            ),
            (
                r#"{"subcommand":"checkout","path":"a.rs"}"#,
                "git checkout -- a.rs",
            ),
            (r#"{"subcommand":"stash"}"#, "git stash"),
            (r#"{"subcommand":"stash","rev":"pop"}"#, "git stash pop"),
            (r#"{"subcommand":"log","rev":"HEAD~2"}"#, "git log HEAD~2"),
            (r#"{"subcommand":"worktree"}"#, "git worktree list"),
        ] {
            let a = parse_tool_action("git", args).unwrap();
            let Action::Git {
                subcommand,
                path,
                rev,
                message,
                ..
            } = &a
            else {
                panic!("wrong action for {args}")
            };
            assert_eq!(
                git_command_line(
                    subcommand,
                    rev.as_deref(),
                    path.as_deref(),
                    message.as_deref(),
                ),
                expected,
                "{args}"
            );
            // The approval prompt shows the same thing.
            assert_eq!(a.describe(), expected, "{args}");
        }
        // `limit` is a display-only suffix; it is not a permission operand, and
        // run_git turns it into --max-count itself.
        let l = parse_tool_action("git", r#"{"subcommand":"log","limit":5}"#).unwrap();
        assert_eq!(git_command_line("log", None, None, None), "git log");
        assert_eq!(l.describe(), "git log (max 5)");
    }

    #[test]
    fn plan_mode_hides_git_writes_from_the_schema() {
        // The model must not be offered `commit` in Plan mode: it would call
        // it and get a generic "you are in PLAN mode" rejection.
        let plan_git = plan_tool_defs()
            .into_iter()
            .find(|t| t.function.name == "git")
            .expect("git stays in the plan toolset for its reads");
        let enum_vals = plan_git.function.parameters["properties"]["subcommand"]["enum"]
            .as_array()
            .expect("enum is an array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>();
        for sub in GIT_WRITE {
            assert!(
                !enum_vals.contains(sub),
                "plan mode must not advertise git {sub}"
            );
        }
        for sub in GIT_READ {
            assert!(enum_vals.contains(sub), "plan mode lost git {sub}");
        }
        // The full toolset still has them.
        let full_git = tool_defs()
            .into_iter()
            .find(|t| t.function.name == "git")
            .unwrap();
        let full = full_git.function.parameters["properties"]["subcommand"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>();
        for sub in GIT_WRITE {
            assert!(full.contains(sub), "build mode lost git {sub}");
        }
    }

    #[test]
    fn git_writes_are_not_undoable_and_do_not_fire_hooks() {
        // Snapshotting a pre-image here would blind-write stale content over a
        // later checkout. Matches the existing RunCommand rule.
        let a = parse_tool_action("git", r#"{"subcommand":"commit","message":"m"}"#).unwrap();
        assert!(!a.mutates_files());
        let b = parse_tool_action("git", r#"{"subcommand":"checkout","rev":"main"}"#).unwrap();
        assert!(!b.mutates_files());
    }

    #[test]
    fn grep_danger_detection() {
        assert!(is_dangerous_command("sudo rm -rf /"));
        assert!(is_dangerous_command("rm -rf ~"));
        assert!(is_dangerous_command("find . -name '*.rs' -delete"));
        assert!(is_dangerous_command("git reset --hard HEAD~3"));
        assert!(is_dangerous_command("git push --force origin main"));
        // Benign commands must not be flagged.
        assert!(!is_dangerous_command("cargo build --release"));
        assert!(!is_dangerous_command("echo $(date)")); // subshells are common and benign
        assert!(!is_dangerous_command("ls -la src/"));
    }

    #[test]
    fn process_tools_parse_and_classify_risk() {
        let cwd = std::env::current_dir().unwrap();
        for command in ["cargo build", "rm -rf ~"] {
            let args = serde_json::json!({"command": command}).to_string();
            let start = parse_tool_action("start_process", &args).unwrap();
            let run = parse_tool_action("run_command", &args).unwrap();
            assert_eq!(start.danger(&cwd), run.danger(&cwd));
            assert!(!start.is_read_only());
        }
        let poll = parse_tool_action("poll_process", r#"{"id":1}"#).unwrap();
        assert_eq!(poll.danger(&cwd), Danger::Safe);
        assert!(poll.is_read_only());
        let write = parse_tool_action("write_process", r#"{"id":1,"input":"hello"}"#).unwrap();
        assert_eq!(write.danger(&cwd), Danger::High);
        assert!(matches!(
            parse_tool_action("write_process", r#"{"id":1,"eof":true}"#).unwrap(),
            Action::WriteProcess { eof: true, .. }
        ));
        assert!(parse_tool_action("write_process", r#"{"id":1}"#).is_err());
        assert!(
            parse_tool_action("write_process", r#"{"id":1,"input":"lost","eof":true}"#).is_err()
        );
        assert!(parse_tool_action("poll_process", r#"{"id":-1}"#).is_err());
        assert!(matches!(
            parse_tool_action("stop_process", r#"{"id":1}"#).unwrap(),
            Action::StopProcess { id: 1 }
        ));
    }

    #[test]
    fn resolve_normalizes_parent_dirs() {
        let cwd = Path::new("/tmp/proj");
        assert_eq!(
            resolve_in(cwd, "src/../lib.rs").unwrap(),
            PathBuf::from("/tmp/proj/lib.rs")
        );
        assert_eq!(
            resolve_in(cwd, "../outside.txt").unwrap(),
            PathBuf::from("/tmp/outside.txt")
        );
        assert_eq!(
            resolve_in(cwd, "/abs/path.txt").unwrap(),
            PathBuf::from("/abs/path.txt")
        );
    }

    #[test]
    fn danger_uses_cwd_for_writes() {
        let base = std::env::temp_dir().join(format!("lc-danger-{}", std::process::id()));
        let cwd = base.join("proj");
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        let inside = Action::WriteFile {
            path: "src/x.rs".into(),
            content: "".into(),
        };
        let outside = Action::WriteFile {
            path: "/etc/hosts".into(),
            content: "".into(),
        };
        assert_eq!(inside.danger(&cwd), Danger::Moderate);
        assert_eq!(outside.danger(&cwd), Danger::High);
        let edit_escape = Action::EditFile {
            path: "../../escape".into(),
            old: "a".into(),
            new: "b".into(),
        };
        assert_eq!(edit_escape.danger(&cwd), Danger::High);
        // Symlink inside the workspace pointing OUT must classify as High.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", cwd.join("escape-link")).unwrap();
            let via_link = Action::WriteFile {
                path: "escape-link/passwd".into(),
                content: "".into(),
            };
            assert_eq!(
                via_link.danger(&cwd),
                Danger::High,
                "symlink escape must be caught"
            );
        }
        // apply_patch touching an outside path is High too.
        let outside_patch = Action::ApplyPatch {
            patch: "*** Begin Patch\n*** Add File: /etc/lc-pwned\n+x\n*** End Patch".into(),
        };
        assert_eq!(outside_patch.danger(&cwd), Danger::High);
        let inside_patch = Action::ApplyPatch {
            patch: "*** Begin Patch\n*** Update File: src/x.rs\n@@\n-a\n+b\n*** End Patch".into(),
        };
        assert_eq!(inside_patch.danger(&cwd), Danger::Moderate);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn read_file_pages_with_line_numbers() {
        let dir = std::env::temp_dir().join(format!("lc-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let body = (1..=30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join("f.txt"), &body).unwrap();
        let full = perform_sync(
            Action::ReadFile {
                path: "f.txt".into(),
                offset: None,
                limit: None,
            },
            &dir,
        );
        assert!(full.contains("    1| line1"));
        assert!(full.contains("   30| line30"));

        let page = perform_sync(
            Action::ReadFile {
                path: "f.txt".into(),
                offset: Some(25),
                limit: Some(3),
            },
            &dir,
        );
        assert!(page.contains("   25| line25"), "{page}");
        assert!(page.contains("   27| line27"));
        assert!(!page.contains("line28"));
        assert!(page.contains("offset=28"), "should hint next page: {page}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_update_plan_and_legacy_alias() {
        match parse_tool_action(
            "update_plan",
            r#"{"plan":[{"step":"explore","status":"completed"},{"step":"edit","status":"in_progress"}]}"#,
        )
        .unwrap()
        {
            Action::UpdatePlan { todos } => {
                assert_eq!(todos.len(), 2);
                assert_eq!(todos[0].content, "explore");
                assert_eq!(todos[1].status, "in_progress");
            }
            other => panic!("wrong action: {other:?}"),
        }
        // Legacy todo_write shape from old sessions still parses.
        match parse_tool_action(
            "todo_write",
            r#"{"todos":[{"content":"x","status":"pending"}]}"#,
        )
        .unwrap()
        {
            Action::UpdatePlan { todos } => assert_eq!(todos[0].content, "x"),
            other => panic!("wrong action: {other:?}"),
        }
        // apply_patch parses through the same entry point.
        match parse_tool_action(
            "apply_patch",
            r#"{"patch":"*** Begin Patch\n*** Delete File: z\n*** End Patch"}"#,
        )
        .unwrap()
        {
            Action::ApplyPatch { .. } => {}
            other => panic!("wrong action: {other:?}"),
        }
    }

    /// Drive the async perform_with_diff() from a sync test (pure fs work, no
    /// awaits actually suspend).
    fn perform_sync(action: Action, cwd: &Path) -> String {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        rt.block_on(action.perform_with_diff(cwd, None))
            .expect("perform should succeed")
            .0
    }

    /// Drive an async tools fn from a sync test. IO and time are both required
    /// because these spawn processes under a timeout, unlike `perform_sync`.
    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("test runtime")
            .block_on(f)
    }

    /// A throwaway repo with one commit on `main`.
    fn git_fixture(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lc-git-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sh = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .expect("git must be installed for this test");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        sh(&["init", "-q", "-b", "main"]);
        sh(&["config", "user.email", "t@example.com"]);
        sh(&["config", "user.name", "T"]);
        sh(&["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        sh(&["add", "a.txt"]);
        sh(&["commit", "-q", "-m", "initial"]);
        dir
    }

    fn git_out(
        dir: &Path,
        sub: &str,
        path: Option<&str>,
        rev: Option<&str>,
        msg: Option<&str>,
    ) -> String {
        block_on(run_git(dir, sub, path, rev, None, msg)).expect("run_git")
    }

    #[test]
    fn git_writes_commit_branch_and_checkout_for_real() {
        let dir = git_fixture("writes");
        // `add` with no path stages the whole tree, deletions included.
        std::fs::write(dir.join("b.txt"), "two\n").unwrap();
        std::fs::remove_file(dir.join("a.txt")).unwrap();
        git_out(&dir, "add", None, None, None);
        let st = git_out(&dir, "status", None, None, None);
        assert!(st.contains("b.txt"), "b.txt not staged:\n{st}");

        // `commit` takes the message as an operand, not on stdin.
        let out = git_out(&dir, "commit", None, None, Some("add b, drop a"));
        assert!(!out.contains("nothing to commit"), "{out}");

        // `branch` creates; with no rev it lists.
        git_out(&dir, "branch", None, Some("feature"), None);
        let list = git_out(&dir, "branch", None, None, None);
        assert!(list.contains("feature"), "branch not created:\n{list}");

        // `checkout` switches.
        git_out(&dir, "checkout", None, Some("feature"), None);
        let br = git_out(&dir, "status", None, None, None);
        assert!(br.contains("feature"), "did not switch:\n{br}");

        // `checkout` with a path restores just that file from HEAD.
        std::fs::write(dir.join("b.txt"), "clobbered\n").unwrap();
        git_out(&dir, "checkout", Some("b.txt"), None, None);
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "two\n");

        // `stash push` then `stash pop` round-trips an edit.
        std::fs::write(dir.join("b.txt"), "wip\n").unwrap();
        git_out(&dir, "stash", None, Some("push"), None);
        assert_ne!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "wip\n");
        git_out(&dir, "stash", None, Some("pop"), None);
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "wip\n");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The words `git_command_line` promises must actually reach git.
    ///
    /// The two renderers are written independently on purpose (one is a human
    /// prompt, one is a pattern key), so a test that only checked each against
    /// a hand-written string would pass while they disagreed with each other.
    /// This runs the real binary and reads back what it acted on.
    #[test]
    fn git_command_line_promises_match_what_git_actually_receives() {
        let dir = git_fixture("promise");
        let st = std::process::Command::new("git")
            .args(["status", "--porcelain", "--", "a.txt"])
            .current_dir(&dir)
            .output()
            .unwrap();
        // `git add -A` with no pathspec: the whole tree, deletions included.
        std::fs::write(dir.join("c.txt"), "three\n").unwrap();
        git_out(&dir, "add", None, None, None);
        let staged = String::from_utf8_lossy(&st.stdout).to_string();
        assert!(staged.is_empty(), "fixture should start clean: {staged:?}");
        let after = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["diff", "--cached", "--name-only"])
                .current_dir(&dir)
                .output()
                .unwrap()
                .stdout,
        )
        .to_string();
        assert!(after.contains("c.txt"), "add -A staged nothing: {after:?}");
        assert_eq!(git_command_line("add", None, None, None), "git add -A");

        // `git branch` with no rev lists rather than creating something.
        let listed = git_out(&dir, "branch", None, None, None);
        assert!(!listed.trim().is_empty());
        assert_eq!(git_command_line("branch", None, None, None), "git branch");

        // `git checkout` with no rev and no path restores the whole tree.
        std::fs::write(dir.join("a.txt"), "clobbered\n").unwrap();
        git_out(&dir, "checkout", None, None, None);
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "one\n",
            "bare `git checkout` must restore the tree, not do nothing"
        );
        assert_eq!(
            git_command_line("checkout", None, None, None),
            "git checkout -- ."
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn git_commit_without_a_message_fails_fast_instead_of_hanging() {
        // run_git is spawned with a null stdin, so letting git reach for an
        // editor would burn the whole 60s timeout.
        let dir = git_fixture("nomsg");
        let err = block_on(run_git(&dir, "commit", None, None, None, None)).unwrap_err();
        assert!(format!("{err:#}").contains("message"), "got: {err:#}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn git_worktree_list_is_read_only_and_runs() {
        let dir = git_fixture("worktree");
        let a = parse_tool_action("git", r#"{"subcommand":"worktree"}"#).unwrap();
        assert!(a.is_read_only());
        assert_eq!(a.danger(Path::new(".")), Danger::Safe);
        assert_eq!(a.describe(), "git worktree list");
        let out = git_out(&dir, "worktree", None, None, None);
        assert!(out.contains("worktree"), "{out}");
        assert!(out.contains(dir.to_string_lossy().as_ref()), "{out}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn git_rejects_remote_subcommands_at_both_layers() {
        // The parse layer (model-facing) and run_git (defence in depth).
        let dir = git_fixture("remote");
        for sub in ["push", "fetch", "reset"] {
            assert!(parse_tool_action("git", &format!(r#"{{"subcommand":"{sub}"}}"#)).is_err());
            assert!(
                block_on(run_git(&dir, sub, None, None, None, None)).is_err(),
                "{sub}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn html_to_text_strips_tags_and_decodes_entities() {
        let out = html_to_text("<p>Hello &amp; welcome</p><script>evil()</script>");
        assert!(out.contains("Hello & welcome"), "got: {out}");
        assert!(!out.contains("evil"));
        let out = html_to_text("a &lt;b&gt; &#65;");
        assert!(out.contains("a <b> A"), "got: {out}");
    }

    #[test]
    fn parse_tool_actions_roundtrip() {
        match parse_tool_action(
            "edit_file",
            r#"{"path":"a.rs","old_string":"x","new_string":"y"}"#,
        )
        .unwrap()
        {
            Action::EditFile { path, old, new } => {
                assert_eq!(
                    (path.as_str(), old.as_str(), new.as_str()),
                    ("a.rs", "x", "y")
                );
            }
            other => panic!("wrong action: {other:?}"),
        }
        assert!(parse_tool_action("nope", "{}").is_err());
        assert!(parse_tool_action("read_file", "not json").is_err());
        // Empty arguments are valid for optional-arg tools.
        match parse_tool_action("list_dir", "").unwrap() {
            Action::ListDir { path } => assert_eq!(path, "."),
            other => panic!("wrong action: {other:?}"),
        }
    }

    #[test]
    fn truncation_marks_output() {
        let short = truncate("hello");
        assert_eq!(short, "hello");
        let long = truncate(&"x".repeat(MAX_TOOL_OUTPUT + 10));
        assert!(long.ends_with("[output truncated]"));
        assert!(long.chars().count() <= MAX_TOOL_OUTPUT + 20);
    }

    #[test]
    fn web_search_parses_action_with_default_and_explicit_max() {
        match parse_tool_action("web_search", r#"{"query":"rust async"}"#).unwrap() {
            Action::WebSearch { query, max_results } => {
                assert_eq!(query, "rust async");
                assert_eq!(max_results, 5);
            }
            other => panic!("wrong action: {other:?}"),
        }
        match parse_tool_action("web_search", r#"{"query":"q","max_results":3}"#).unwrap() {
            Action::WebSearch { query, max_results } => {
                assert_eq!(query, "q");
                assert_eq!(max_results, 3);
            }
            other => panic!("wrong action: {other:?}"),
        }
    }

    #[test]
    fn web_search_url_helpers_roundtrip() {
        assert_eq!(urlencoding("a b&c"), "a%20b%26c");
        assert_eq!(urlencoding_decode("a%20b%26c").unwrap(), "a b&c");
        // DDG redirect unwrapping.
        assert_eq!(
            full_redirect("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fdoc&rut=xyz"),
            "https://example.com/doc"
        );
        assert_eq!(
            strip_html("<a class=\"result__a\">Title &amp; More</a>"),
            "Title & More"
        );
    }

    #[test]
    fn web_search_parses_ddg_html_results() {
        let html = r#"<div class="result">
          <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F1&rut=a">First Result</a>
          <a class="result__a" href="https://example.org/2">Second Result</a>
          <a class="result__a" href="https://example.net/3">Third Result</a>
        </div>"#;
        let results = parse_ddg_results(html, 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "First Result");
        assert_eq!(results[0].1, "https://example.com/1");
        assert_eq!(results[1].0, "Second Result");
        let empty = parse_ddg_results("<html><body>no anchors</body></html>", 5);
        assert!(empty.is_empty());
    }
}
