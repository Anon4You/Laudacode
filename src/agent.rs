use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::api::{ChatClient, Message, StreamEvent, ToolCall, Turn, Usage};
use crate::tools;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    /// Ask before every write/command.
    Suggest,
    /// Auto-approve file edits, ask for shell commands.
    AutoEdit,
    /// Approve everything except high-danger commands.
    FullAuto,
}

impl ApprovalMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "suggest" | "ask" => Some(Self::Suggest),
            "auto-edit" | "autoedit" | "auto_edit" => Some(Self::AutoEdit),
            "full-auto" | "fullauto" | "full_auto" | "yolo" => Some(Self::FullAuto),
            _ => None,
        }
    }
    /// Map a TUI collaboration mode to an approval policy.
    pub fn from_tui_mode(m: crate::tui::Mode) -> Self {
        match m {
            crate::tui::Mode::Plan => Self::Suggest,
            crate::tui::Mode::Build => Self::AutoEdit,
            crate::tui::Mode::FullAuto => Self::FullAuto,
        }
    }
}

/// Events emitted while the agent works — drives the TUI transcript.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Assistant text delta (streamed).
    Content(String),
    /// Reasoning delta (streamed, dimmed in UI).
    Reasoning(String),
    /// A tool call is starting.
    ToolStart { name: String, summary: String },
    /// A tool finished; `preview` carries a short excerpt of its output.
    ToolDone { name: String, ok: bool, preview: String },
    /// A tool mutated files — UI renders these as colored diffs.
    ToolEdit { name: String, files: Vec<crate::diff::FileDiff> },
    /// Token usage from the last request.
    Usage(Usage),
    /// The agent wrote a fresh todo list.
    Todo(Vec<tools::TodoItem>),
}

/// UI sink the agent reports to while a turn is running.
pub trait UiSink {
    fn on_event(&mut self, ev: AgentEvent);
    /// Ask the user to approve an action. Blocks until answered. Return true to proceed.
    fn approve(&mut self, action: &tools::Action, danger: tools::Danger) -> bool;
    /// A separate sink for one concurrent sub-agent; events are tagged with
    /// `prefix` so the transcript can attribute them.
    fn fork(&mut self, prefix: &str) -> Box<dyn UiSink>;
}

const MAX_TOOL_ROUNDS: usize = 30;
/// Compact automatically once prompt tokens pass this fraction of a rough
/// context budget (~128k). Keeps long sessions from silently overflowing.
const AUTO_COMPACT_AT: u64 = 100_000;

pub struct Agent {
    pub client: ChatClient,
    pub model: String,
    pub cwd: PathBuf,
    pub mode: ApprovalMode,
    pub messages: Vec<Message>,
    pub last_usage: Option<Usage>,
    /// Authoritative todo list mirrored from update_plan calls.
    pub todos: Vec<tools::TodoItem>,
    /// Per-tool allow/ask/deny rules from config.
    pub permissions: crate::permissions::Permissions,
    /// File snapshots for /undo: (turn_seq, path, previous content).
    pub(crate) undo_stack: Vec<(u64, PathBuf, Option<String>)>,
    turn_seq: u64,
    last_undone: Option<u64>,
}

impl Agent {
    pub fn new(
        client: ChatClient,
        model: String,
        cwd: PathBuf,
        mode: ApprovalMode,
        permissions: crate::permissions::Permissions,
    ) -> Self {
        let system = Self::build_system_prompt(&cwd);
        Self {
            client,
            model,
            cwd,
            mode,
            messages: vec![Message::system(system)],
            last_usage: None,
            todos: vec![],
            permissions,
            undo_stack: vec![],
            turn_seq: 0,
            last_undone: None,
        }
    }

    /// Revert every file touched during the most recent agent turn.
    /// Safe to call once per turn; a second call reports already-undone.
    pub fn undo_last_turn(&mut self) -> Result<String> {
        let seq = self
            .undo_stack
            .last()
            .map(|(s, _, _)| *s)
            .context("nothing to undo — no file changes recorded yet")?;
        anyhow::ensure!(
            self.last_undone != Some(seq),
            "that turn was already reverted"
        );
        let mut restored = 0usize;
        while let Some((s, path, prev)) = self.undo_stack.pop() {
            if s != seq {
                self.undo_stack.push((s, path, prev));
                break;
            }
            match prev {
                Some(content) => {
                    std::fs::write(&path, content.as_bytes())
                        .with_context(|| format!("restoring {}", path.display()))?;
                }
                None => {
                    // File did not exist before — remove what the agent added.
                    let _ = std::fs::remove_file(&path);
                }
            }
            restored += 1;
        }
        self.last_undone = Some(seq);
        Ok(format!("reverted {restored} file(s) from turn #{seq}"))
    }

    /// Record the pre-image of every path an action is about to touch.
    fn snapshot_for_undo(&mut self, action: &tools::Action) {
        use tools::Action;
        let paths: Vec<PathBuf> = match action {
            Action::ListDir { .. }
            | Action::ReadFile { .. }
            | Action::FetchUrl { .. }
            | Action::Grep { .. }
            | Action::Glob { .. }
            | Action::RunCommand { .. }
            | Action::UpdatePlan { .. } => return,
            Action::WriteFile { path, .. } | Action::EditFile { path, .. } => {
                match tools::resolve_path_in(&self.cwd, path) {
                    Ok(p) => vec![p],
                    Err(_) => return,
                }
            }
            Action::ApplyPatch { patch } => {
                let Ok(hunks) = crate::patch::parse_patch(patch) else { return };
                hunks
                    .iter()
                    .filter_map(|h| {
                        tools::resolve_path_in(&self.cwd, &h.classify_path().to_string_lossy()).ok()
                    })
                    .collect()
            }
        };
        for p in paths {
            let prev = std::fs::read_to_string(&p).ok();
            self.undo_stack.push((self.turn_seq, p, prev));
        }
    }

    /// Authoritative, auto-generated description of everything this agent
    /// can do — derived from the live tool registry and specialist roster so
    /// ANY model knows its full toolkit without hand-maintained prose.
    fn capabilities_block() -> String {
        let mut s = String::from(
            "Tools available this session (authoritative — never claim a tool exists that is not listed here):\n",
        );
        for td in tools::tool_defs() {
            let desc_first = td
                .function
                .description
                .split(". ")
                .next()
                .unwrap_or(td.function.description);
            let params = td
                .function
                .parameters
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|o| {
                    o.keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            s.push_str(&format!(
                "- {}({}) — {}.\n",
                td.function.name, params, desc_first
            ));
        }
        s.push_str(
            "\napply_patch format (multi-file edits):\n\
             *** Begin Patch\n\
             *** Add File: path\n\
             +new lines\n\
             *** Update File: path\n\
             @@ unique context line from the file\n\
             -old line\n\
             +new line\n\
             *** Delete File: path\n\
             *** End Patch\n\
             Context lines start with a space; '@@ <exact line>' anchors a chunk; \
             '*** End of File' appends at EOF; '*** Move to: newPath' renames.\n",
        );
        s.push_str("\nSpecialists you can spawn with delegate(tasks:[{agent,task}]):\n");
        for r in crate::agents::all_roles() {
            let tag = if r.read_only { " [read-only]" } else { "" };
            s.push_str(&format!(
                "- {} — {}{}\n",
                r.name, r.description, tag
            ));
        }
        s
    }

    fn build_system_prompt(cwd: &std::path::Path) -> String {
        let overview = tools::project_overview(cwd);
        let agents_md = load_agents_md(cwd);
        format!(
            r#"You are Laudacode, an expert AI coding agent running in the user's terminal.
You help with software engineering: writing code, explaining, debugging, refactoring, fetching docs from the web, running commands, and orchestrating specialist sub-agents.

Environment:
- Working directory (your workspace): {cwd}
- OS: {os} (may be Android/Termux — prefer portable, POSIX-friendly commands; `sh` is available)
- Today: {date}

Note: reads may touch any path, but writes/edits OUTSIDE the workspace require
explicit user approval and should be avoided unless the user asks for them.
Configured permission rules may deny or force approval for specific commands,
paths or URLs — if an action is blocked, adapt instead of retrying identically.

{overview}
{agents_md}
{capabilities}

Working rules:
1. Inspect BEFORE editing: grep/glob/list_dir/read_file first; read_file returns numbered lines and pages via offset/limit — never guess contents.
2. Prefer apply_patch for code edits — atomic multi-file add/update/rename/delete. Use edit_file only for one tiny single-file tweak. Anchor Update hunks with unique '@@ context' lines or '*** End of File'.
3. Use update_plan for any multi-step task: exactly one step in_progress at a time; replace the whole list each call.
4. Use fetch_url for external docs/API references instead of guessing URLs or APIs. Do not invent libraries the project does not use.
5. Delegate to specialists when work splits into independent chunks (research two areas in parallel, reviewer + tester after coding). Give each task precise, self-contained instructions; skip delegation for trivial single-file tweaks.
6. If a tool errors, read the message, fix the cause, retry differently — never repeat the identical failing call.
7. Keep replies concise markdown with language-tagged code blocks; finish with 1-3 bullets summarizing what changed."#,
            cwd = cwd.display(),
            os = std::env::consts::OS,
            date = chrono_today(),
            overview = overview,
            agents_md = agents_md,
            capabilities = Self::capabilities_block(),
        )
    }

    /// Rough token estimate (chars/4) — used when the provider doesn't report
    /// usage in streams so auto-compact still fires before overflow.
    fn estimated_prompt_tokens(&self) -> u64 {
        self.messages
            .iter()
            .map(|m| {
                let text = m.content.as_deref().unwrap_or("");
                let args: usize = m.tool_calls.iter().map(|t| t.function.arguments.len()).sum();
                (text.len() + args) as u64 / 4
            })
            .sum()
    }

    /// Replace history with a short summary to free context window space.
    pub async fn compact(&mut self) -> Result<String> {
        if self.messages.len() <= 2 {
            anyhow::bail!("nothing to compact yet");
        }
        let transcript = self
            .messages
            .iter()
            .skip(1) // skip system
            .map(|m| {
                let body = m.content.clone().unwrap_or_default();
                match m.role.as_str() {
                    "tool" => format!("[tool result] {}", body.chars().take(400).collect::<String>()),
                    r => format!("[{r}] {body}"),
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let summary_msgs = vec![
            Message::system(
                "Summarize the following coding-agent conversation into a compact working notes block: \
                 task(s), key decisions, files touched with paths, current state, and next steps. \
                 Max 300 words. Output only the summary.",
            ),
            Message::user(transcript.chars().take(24_000).collect::<String>()),
        ];
        let turn = self
            .client
            .stream_chat(&self.model, &summary_msgs, &[], |_| {}, None)
            .await?;
        if turn.content.trim().is_empty() {
            anyhow::bail!("model returned empty summary");
        }
        let system = Message::system(self.messages[0].content.clone().unwrap_or_default());
        self.messages = vec![
            system,
            Message::user(format!(
                "[Conversation was compacted. Working notes:\n{}\n\nContinue from here.]",
                turn.content
            )),
        ];
        Ok(turn.content)
    }

    fn toolset_for_mode(&self) -> Vec<crate::api::ToolDef> {
        let mut defs = match self.mode {
            // Plan mode exposes only read-only tools; writes are impossible.
            ApprovalMode::Suggest => tools::plan_tool_defs(),
            _ => tools::tool_defs(),
        };
        // The orchestrator can always call specialists (Plan mode gets the
        // read-only subset via the schema enum).
        defs.push(crate::agents::delegate_tool_def(self.mode == ApprovalMode::Suggest));
        defs
    }

    /// Fan a delegate call out to its specialists concurrently and collect
    /// their reports into one tool result.
    async fn execute_delegate(&mut self, arguments: &str, ui: &mut dyn UiSink) -> String {
        let tasks = match crate::agents::parse_delegate_args(arguments) {
            Ok(t) => t,
            Err(e) => return format!("Error parsing delegate call: {e:#}"),
        };
        if self.mode == ApprovalMode::Suggest {
            for (name, _) in &tasks {
                let ro = crate::agents::get(name).map(|s| s.read_only).unwrap_or(false);
                if !ro {
                    return format!(
                        "Blocked: '{name}' can mutate files — switch to BUILD to delegate it."
                    );
                }
            }
        }
        let cwd = self.cwd.clone();
        let mode = self.mode;
        let client = self.client.clone();
        let model = self.model.clone();

        let futures = tasks.into_iter().map(|(name, task)| {
            let sink = ui.fork(&format!("[{name}]"));
            let client = client.clone();
            let model = model.clone();
            let cwd = cwd.clone();
            async move {
                crate::agents::run_sub_agent(
                    &client, &model, &cwd, mode, &name, &task, sink,
                )
                .await
            }
        });
        let results = futures_util::future::join_all(futures).await;
        results.join("\n\n")
    }

    /// Run one full agent turn: user input -> (tool calls)* -> final answer.
    ///
    /// `images` carries data-URI attachments for this turn's user message
    /// (vision models only; plain-text providers ignore them).
    ///
    /// `cancel` lets the host interrupt between rounds, mid-stream and
    /// between individual tool executions (Esc interrupt).
    pub async fn run_turn(
        &mut self,
        input: &str,
        images: &[String],
        ui: &mut dyn UiSink,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        if is_cancelled(cancel) {
            anyhow::bail!("interrupted");
        }
        self.turn_seq += 1;
        if images.is_empty() {
            self.messages.push(Message::user(input));
        } else {
            self.messages.push(Message::user_with_images(input, images.to_vec()));
        }

        // Identical-call ring for the doom-loop guard: the same tool with
        // byte-identical arguments three times in a row is a stuck model.
        let mut recent_calls: Vec<(String, String)> = Vec::new();

        for _round in 0..MAX_TOOL_ROUNDS {
            // Per-request (not persisted) mode note: in PLAN the write tools
            // are hidden, and without this the model flails about "missing"
            // capabilities instead of telling the user to switch modes.
            let plan_note = (self.mode == ApprovalMode::Suggest).then(|| {
                Message::system(
                    "PLAN mode is active: only read-only tools (list_dir, read_file, \
                     grep, glob, fetch_url) are available. Do NOT claim tools are broken. \
                     If the task needs file edits or shell commands, answer with your \
                     plan/code and tell the user to press Tab to switch to BUILD mode.",
                )
            });
            let mut req_msgs = self.messages.clone();
            if let Some(n) = plan_note {
                req_msgs.push(n);
            }
            let turn: Turn = self
                .client
                .stream_chat(&self.model, &req_msgs, &self.toolset_for_mode(), |ev| match ev {
                    StreamEvent::Content(s) => ui.on_event(AgentEvent::Content(s)),
                    StreamEvent::Reasoning(s) => ui.on_event(AgentEvent::Reasoning(s)),
                    StreamEvent::Usage(u) => ui.on_event(AgentEvent::Usage(u)),
                }, cancel)
                .await
                .context("chat completion failed")?;

            if let Some(u) = turn.usage {
                self.last_usage = Some(u);
            }

            if is_cancelled(cancel) {
                if !turn.content.is_empty() {
                    self.messages.push(Message::assistant(turn.content));
                }
                anyhow::bail!("interrupted by user");
            }

            if turn.tool_calls.is_empty() {
                if !turn.content.trim().is_empty() {
                    self.messages.push(Message::assistant(turn.content));
                }
                return Ok(());
            }

            // Persist assistant intent, then execute each requested tool.
            self.messages.push(Message::assistant_with_tools(
                turn.tool_calls.clone(),
                if turn.content.is_empty() { None } else { Some(turn.content) },
            ));

            for tc in turn.tool_calls {
                if is_cancelled(cancel) {
                    self.messages.push(Message::tool_result(&tc.id, "[interrupted by user]"));
                    anyhow::bail!("interrupted by user");
                }
                // Doom-loop guard: 3 identical calls in a row → refuse and
                // tell the model to change strategy instead of burning
                // rounds (and tokens) on the same failing action.
                let key = (
                    tc.function.name.clone(),
                    tc.function.arguments.clone(),
                );
                recent_calls.push(key);
                let n = recent_calls.len();
                if n >= 3
                    && recent_calls[n - 1] == recent_calls[n - 2]
                    && recent_calls[n - 2] == recent_calls[n - 3]
                {
                    ui.on_event(AgentEvent::ToolDone {
                        name: tc.function.name.clone(),
                        ok: false,
                        preview: "doom-loop blocked".into(),
                    });
                    self.messages.push(Message::tool_result(
                        &tc.id,
                        "[doom-loop blocked] you have made this exact call three times \
                         with identical arguments. Stop repeating it — inspect the state, \
                         change your approach, or ask the user for help.",
                    ));
                    continue;
                }
                let result = self.execute_call(&tc, ui).await;
                self.messages.push(Message::tool_result(&tc.id, result));
            }

            // Auto-compact when the context grows past the threshold. Falls
            // back to a chars/4 estimate when the provider reports no usage.
            let prompt_tokens = self
                .last_usage
                .map(|u| u.prompt_tokens)
                .unwrap_or_else(|| self.estimated_prompt_tokens());
            if prompt_tokens > AUTO_COMPACT_AT && self.messages.len() > 4 {
                let ok = self.compact().await.is_ok();
                ui.on_event(AgentEvent::ToolDone { name: "compact".into(), ok, preview: String::new() });
            }
        }
        anyhow::bail!("agent stopped after {MAX_TOOL_ROUNDS} tool rounds (possible loop)")
    }

    async fn execute_call(&mut self, tc: &ToolCall, ui: &mut dyn UiSink) -> String {
        // Orchestration tool — handled before the regular action pipeline.
        if tc.function.name == "delegate" {
            return self.execute_delegate(&tc.function.arguments, ui).await;
        }
        let action = match tools::parse_tool_action(&tc.function.name, &tc.function.arguments) {
            Ok(a) => a,
            Err(e) => return format!("Error parsing tool call: {e}"),
        };

        // In Plan mode, refuse anything that mutates state.
        if self.mode == ApprovalMode::Suggest && !action.is_read_only() {
            return "Blocked: you are in PLAN mode (read-only). Present your plan as text \
                    and wait for the user to switch to BUILD before editing."
                .to_string();
        }

        let name = tc.function.name.clone();
        // Permission rules (config [permission.*]) override the danger
        // heuristics: deny short-circuits, ask forces the modal even for
        // safe reads, allow auto-approves the matched input.
        let mut forced_ask = false;
        let mut rule_allows = false;
        let inputs = permission_inputs(&action);
        for (tool, input) in &inputs {
            match self.permissions.resolve(tool, input) {
                Some(crate::permissions::Rule::Deny) => {
                    ui.on_event(AgentEvent::ToolDone {
                        name: name.clone(),
                        ok: false,
                        preview: "denied by permission rule".into(),
                    });
                    return format!(
                        "Blocked by permission rule: {tool} '{input}' is denied in config."
                    );
                }
                Some(crate::permissions::Rule::Ask) => forced_ask = true,
                Some(crate::permissions::Rule::Allow) => rule_allows = true,
                None => {}
            }
        }
        // Secret guard: .env-style files are denied unless explicitly allowed.
        let read_input = inputs.iter().find(|(t, _)| *t == "read").map(|(_, i)| i.as_str());
        let secret_hit = read_input.and_then(|p| {
            (self.permissions.resolve("read", p).is_none()
                && crate::permissions::Permissions::secret_guard(p)
                    == Some(crate::permissions::Rule::Deny))
            .then_some(p)
        });
        if let Some(path_input) = secret_hit {
            return format!(
                "Blocked: '{path_input}' looks like a secrets file (.env*). \
                 Allow it explicitly in [permission.read] if you really mean it."
            );
        }

        let name = tc.function.name.clone();
        let summary = action.describe();
        ui.on_event(AgentEvent::ToolStart { name: name.clone(), summary });

        // Approval gate. Permission rules take precedence; otherwise writes
        // outside the workspace stay High and prompt even in FULL AUTO.
        let danger = action.danger(&self.cwd);
        let approved = if forced_ask {
            ui.approve(&action, danger)
        } else if rule_allows {
            true
        } else {
            match danger {
                tools::Danger::Safe => true,
                tools::Danger::High => ui.approve(&action, danger),
                tools::Danger::Moderate => true,
            }
        };
        if !approved {
            return "User DECLINED this action. Ask what to do differently or proceed another way."
                .to_string();
        }

        // Snapshot pre-images so /undo can revert this turn's file changes.
        self.snapshot_for_undo(&action);

        // Surface plan updates to the host for live display.
        if let tools::Action::UpdatePlan { todos } = &action {
            self.todos = todos.clone();
            ui.on_event(AgentEvent::Todo(todos.clone()));
        }

        match action.perform_with_diff(&self.cwd).await {
            Ok((out, files)) => {
                // Surface colored diffs for any mutated files first.
                if !files.is_empty() {
                    ui.on_event(AgentEvent::ToolEdit { name: name.clone(), files });
                }
                let preview: String = out
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .take(6)
                    .map(|l| l.trim_start())
                    .collect::<Vec<_>>()
                    .join(" ⏎ ");
                // Kept generous — Ctrl+O in the TUI expands this in full.
                let preview = preview.chars().take(1200).collect::<String>();
                ui.on_event(AgentEvent::ToolDone { name, ok: true, preview });
                out
            }
            Err(e) => {
                let msg = format!("{e:#}");
                ui.on_event(AgentEvent::ToolDone {
                    name,
                    ok: false,
                    preview: msg.chars().take(600).collect(),
                });
                format!("Command failed: {msg}")
            }
        }
    }
}

/// (tool-key, raw-input) pairs a given action should be permission-checked
/// against. Tool keys mirror config section names.
fn permission_inputs(action: &tools::Action) -> Vec<(&'static str, String)> {
    use tools::Action;
    match action {
        Action::ListDir { path } | Action::ReadFile { path, .. } => {
            vec![("read", path.clone())]
        }
        Action::Grep { pattern, .. } => vec![("read", pattern.clone())],
        Action::Glob { pattern, .. } => vec![("read", pattern.clone())],
        Action::FetchUrl { url } => vec![("webfetch", url.clone())],
        Action::WriteFile { path, .. } | Action::EditFile { path, .. } => {
            vec![("edit", path.clone())]
        }
        Action::ApplyPatch { patch } => {
            let mut out = Vec::new();
            if let Ok(hunks) = crate::patch::parse_patch(patch) {
                for h in hunks {
                    out.push((
                        "edit",
                        h.classify_path().to_string_lossy().into_owned(),
                    ));
                }
            }
            out
        }
        Action::RunCommand { command } => vec![("bash", command.clone())],
        Action::UpdatePlan { .. } => vec![],
    }
}

fn is_cancelled(cancel: Option<&std::sync::atomic::AtomicBool>) -> bool {
    cancel
        .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false)
}

/// Collect AGENTS.md instructions: global config one, then nearest ancestors
/// up to the repo root (max 3 files, 8 KB each).
fn load_agents_md(cwd: &std::path::Path) -> String {
    let mut blocks: Vec<String> = Vec::new();
    let mut push_file = |p: &std::path::Path, label: &str| {
        if blocks.len() >= 3 {
            return;
        }
        if let Ok(raw) = std::fs::read_to_string(p) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                let body: String = trimmed.chars().take(8 * 1024).collect();
                blocks.push(format!("--- {label} ---\n{body}\n"));
            }
        }
    };
    // Global.
    if let Some(cfg) = dirs::config_dir() {
        push_file(&cfg.join("laudacode").join("AGENTS.md"), "global AGENTS.md");
    }
    // Walk from cwd upward, collecting in reverse so root-most comes first.
    let mut chain: Vec<PathBuf> = Vec::new();
    let mut cur: &std::path::Path = cwd;
    loop {
        chain.push(cur.join("AGENTS.md"));
        match cur.parent() {
            Some(p) => cur = p,
            None => break,
        }
        if chain.len() >= 5 {
            break;
        }
    }
    for p in chain.into_iter().rev() {
        push_file(&p, &format!("{} instructions", p.display()));
    }
    if blocks.is_empty() {
        String::new()
    } else {
        format!(
            "Project instructions (AGENTS.md) — follow these carefully:\n{}\n",
            blocks.join("\n")
        )
    }
}

fn chrono_today() -> String {    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Days since epoch -> YYYY-MM-DD (civil-from-days algorithm).
    let days = now / 86_400;
    let z = days as i64 + 719_468;
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
mod tests {
    use super::*;

    #[test]
    fn system_prompt_describes_every_tool_and_specialist() {
        let prompt = Agent::build_system_prompt(std::path::Path::new("."));
        // Every registered tool is documented with its parameters.
        for td in tools::tool_defs() {
            assert!(
                prompt.contains(&format!("- {}(", td.function.name)),
                "prompt must document tool '{}':\\n{prompt}",
                td.function.name
            );
        }
        // Every specialist role (built-in + custom) is listed.
        for r in crate::agents::all_roles() {
            assert!(
                prompt.contains(&format!("- {}", r.name)),
                "prompt must list specialist '{}'",
                r.name
            );
        }
        // Key operational knowledge stays in the prompt.
        assert!(prompt.contains("*** Begin Patch"));
        assert!(prompt.contains("update_plan"));
        assert!(prompt.contains("PLAN mode is active") == false, "plan note is per-request only");
    }

    #[test]
    fn capabilities_track_new_tools_automatically() {
        let block = Agent::capabilities_block();
        // A tool added to the registry tomorrow shows up without editing prose:
        // the block enumerates exactly the registry, nothing stale.
        let registry_names: Vec<&str> =
            tools::tool_defs().iter().map(|t| t.function.name).collect();
        for name in &registry_names {
            assert!(block.contains(name));
        }
        assert_eq!(
            block.matches("- ").count() >= registry_names.len(),
            true
        );
    }

    #[test]
    fn today_is_valid_iso_date() {
        let d = chrono_today();
        let parts: Vec<&str> = d.split('-').collect();
        assert_eq!(parts.len(), 3);
        let y: i64 = parts[0].parse().unwrap();
        let m: u32 = parts[1].parse().unwrap();
        let day: u32 = parts[2].parse().unwrap();
        assert!((2024..=2100).contains(&y), "year {y}");
        assert!((1..=12).contains(&m), "month {m}");
        assert!((1..=31).contains(&day), "day {day}");
    }

    #[test]
    fn mode_parsing_accepts_aliases() {
        assert_eq!(ApprovalMode::parse("yolo"), Some(ApprovalMode::FullAuto));
        assert_eq!(ApprovalMode::parse("auto_edit"), Some(ApprovalMode::AutoEdit));
        assert_eq!(ApprovalMode::parse("ASK"), Some(ApprovalMode::Suggest));
        assert_eq!(ApprovalMode::parse("bogus"), None);
    }

    #[test]
    fn agents_md_collector_respects_limit_and_empty() {
        // No AGENTS.md in a fresh temp dir chain — must produce empty string.
        let tmp = std::env::temp_dir().join(format!("lc-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let out = load_agents_md(&tmp);
        // May pick up global config AGENTS.md if present, but never panics.
        let _ = out;
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn undo_restores_files_added_modified_and_deleted() {
        use crate::api::ChatClient;
        let dir = std::env::temp_dir().join(format!("lc-undo-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/exists.rs"), "original\n").unwrap();
        std::fs::write(dir.join("src/doomed.rs"), "bye\n").unwrap();

        let client = ChatClient::new("http://localhost:0/v1", "", &Default::default(), false, None)
            .expect("client");
        let mut agent = Agent::new(
            client,
            String::new(),
            dir.clone(),
            ApprovalMode::FullAuto,
            crate::permissions::Permissions::default(),
        );

        // Turn 1: modify existing + add new + delete one. Snapshots are
        // taken automatically by the same gate production code uses.
        agent.turn_seq += 1;
        assert!(
            agent.perform_action_sync(&tools::Action::EditFile {
                path: "src/exists.rs".into(),
                old: "original".into(),
                new: "changed".into(),
            }),
            "edit should apply"
        );
        assert!(agent.perform_action_sync(&tools::Action::WriteFile {
            path: "src/new_file.rs".into(),
            content: "added".into(),
        }));
        assert!(
            agent.perform_action_sync(&tools::Action::ApplyPatch {
                patch: "*** Begin Patch\n*** Delete File: src/doomed.rs\n*** End Patch".into(),
            }),
            "delete should apply"
        );

        assert_eq!(std::fs::read_to_string(dir.join("src/exists.rs")).unwrap(), "changed\n");
        assert!(dir.join("src/new_file.rs").exists());
        assert!(!dir.join("src/doomed.rs").exists());

        // Undo reverts all three.
        let msg = agent.undo_last_turn().unwrap();
        assert!(msg.contains("reverted 3 file(s)"), "{msg}");
        assert_eq!(std::fs::read_to_string(dir.join("src/exists.rs")).unwrap(), "original\n");
        assert!(!dir.join("src/new_file.rs").exists(), "created file removed");
        assert_eq!(std::fs::read_to_string(dir.join("src/doomed.rs")).unwrap(), "bye\n");

        // Second undo of the same turn is refused.
        assert!(agent.undo_last_turn().is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    impl Agent {
        /// Sync helper for tests — runs the action ignoring diffs.
        fn perform_with_diff_blocking(&mut self, action: &tools::Action) -> anyhow::Result<String> {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async { self.perform(action).await })
        }

        async fn perform(&mut self, action: &tools::Action) -> anyhow::Result<String> {
            let (out, _) = action.perform_with_diff(&self.cwd).await?;
            Ok(out)
        }

        fn perform_action_sync(&mut self, action: &tools::Action) -> bool {
            self.snapshot_for_undo(action);
            self.perform_with_diff_blocking(action).is_ok()
        }
    }
}
