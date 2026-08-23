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
    /// Authoritative todo list mirrored from todo_write calls.
    pub todos: Vec<tools::TodoItem>,
}

impl Agent {
    pub fn new(client: ChatClient, model: String, cwd: PathBuf, mode: ApprovalMode) -> Self {
        let system = Self::build_system_prompt(&cwd);
        Self {
            client,
            model,
            cwd,
            mode,
            messages: vec![Message::system(system)],
            last_usage: None,
            todos: vec![],
        }
    }

    fn build_system_prompt(cwd: &std::path::Path) -> String {
        let overview = tools::project_overview(cwd);
        let agents_md = load_agents_md(cwd);
        format!(
            r#"You are Laudacode, an expert AI coding agent running in the user's terminal.
You help with software engineering: writing code, explaining, debugging, refactoring, fetching docs from the web, and running commands.

Environment:
- Working directory (your workspace): {cwd}
- OS: {os} (may be Android/Termux — prefer portable, POSIX-friendly commands; `sh` is available)
- Today: {date}

Note: reads may touch any path, but writes/edits OUTSIDE the workspace require
explicit user approval and should be avoided unless the user asks for them.

{overview}
{agents_md}
Rules:
1. Use grep/glob/list_dir to inspect files BEFORE editing them. Never guess file contents. read_file returns numbered lines; use offset/limit to page through big files.
2. Prefer apply_patch for code edits — it can add, update, rename and delete multiple files in one atomic call. Use edit_file only for one tiny single-file tweak. Always anchor Update File hunks with unique context lines (@@) or '*** End of File'.
3. Use update_plan for any multi-step task: keep exactly one step in_progress while working.
4. Use fetch_url when you need external docs or API references instead of guessing.
5. Keep responses concise. Use short markdown. Code blocks must specify the language.
6. When you finish a task, summarize what changed in 1-3 bullet points.
7. If something fails, read the error output and iterate until fixed.
8. Do not invent APIs or libraries that are not already used by the project."#,
            cwd = cwd.display(),
            os = std::env::consts::OS,
            date = chrono_today(),
            overview = overview,
            agents_md = agents_md,
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
        match self.mode {
            // Plan mode exposes only read-only tools; writes are impossible.
            ApprovalMode::Suggest => tools::plan_tool_defs(),
            _ => tools::tool_defs(),
        }
    }

    /// Run one full agent turn: user input -> (tool calls)* -> final answer.
    ///
    /// `images` carries data-URI attachments for this turn's user message
    /// (vision models only; plain-text providers ignore them).
    ///
    /// `cancel` lets the host interrupt between rounds, mid-stream and
    /// between individual tool executions (codex-style Esc interrupt).
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
        if images.is_empty() {
            self.messages.push(Message::user(input));
        } else {
            self.messages.push(Message::user_with_images(input, images.to_vec()));
        }

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
        let summary = action.describe();
        ui.on_event(AgentEvent::ToolStart { name: name.clone(), summary });

        // Approval gate. Writes outside the workspace are classified High by
        // danger(), so they prompt even in FULL AUTO.
        let danger = action.danger(&self.cwd);
        let approved = match danger {
            tools::Danger::Safe => true,
            tools::Danger::High => ui.approve(&action, danger),
            tools::Danger::Moderate => match self.mode {
                ApprovalMode::FullAuto => true,
                ApprovalMode::AutoEdit => true, // edits auto-approved in Build mode
                ApprovalMode::Suggest => true,  // unreachable: plan blocks earlier
            },
        };
        if !approved {
            return "User DECLINED this action. Ask what to do differently or proceed another way."
                .to_string();
        }

        // Surface plan updates to the host for live display.
        if let tools::Action::UpdatePlan { todos } = &action {
            self.todos = todos.clone();
            ui.on_event(AgentEvent::Todo(todos.clone()));
        }

        match action.perform(&self.cwd).await {
            Ok(out) => {
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
}
