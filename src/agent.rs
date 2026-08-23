use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::api::{ChatClient, Message, StreamEvent, Turn};
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
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Suggest => "suggest",
            Self::AutoEdit => "auto-edit",
            Self::FullAuto => "full-auto",
        }
    }
}

/// UI sink the agent reports to while a turn is running.
pub trait UiSink {
    fn on_content(&mut self, delta: &str);
    fn on_reasoning(&mut self, delta: &str);
    /// Ask the user to approve an action. Return true to proceed.
    fn approve(&mut self, action: &tools::Action) -> bool;
    /// Report a finished tool execution (for live display).
    fn on_tool_done(&mut self, action: &tools::Action, ok: bool);
}

const MAX_TOOL_ROUNDS: usize = 30;

pub struct Agent {
    pub client: ChatClient,
    pub model: String,
    pub cwd: PathBuf,
    pub mode: ApprovalMode,
    pub messages: Vec<Message>,
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
        }
    }

    fn build_system_prompt(cwd: &PathBuf) -> String {
        let overview = tools::project_overview(cwd);
        let agents_md = load_agents_md(cwd);
        format!(
            r#"You are Laudacode, an expert AI coding agent running in the user's terminal.
You help with software engineering: writing code, explaining, debugging, refactoring, fetching docs from the web, and running commands.

Environment:
- Working directory (your sandbox root): {cwd}
- OS: {os} (may be Android/Termux — prefer portable, POSIX-friendly commands; `sh` is available)
- Today: {date}

{overview}
{agents_md}
Rules:
1. Use the provided tools to inspect files BEFORE editing them. Never guess file contents.
2. Prefer edit_file with exact unique snippets over rewriting whole files.
3. Use fetch_url when you need external docs or API references instead of guessing.
4. Keep responses concise. Use short markdown. Code blocks must specify the language.
5. When you finish a task, summarize what changed in 1-3 bullet points.
6. If something fails, read the error output and iterate until fixed.
7. Do not invent APIs or libraries that are not already used by the project."#,
            cwd = cwd.display(),
            os = std::env::consts::OS,
            date = chrono_today(),
            overview = overview,
            agents_md = agents_md,
        )
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
                    "tool" => format!("[tool result] {}", &body[..body.len().min(400)]),
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
            .stream_chat(&self.model, &summary_msgs, &[], |_| {})
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

    /// Run one full agent turn: user input -> (tool calls)* -> final answer.
    pub async fn run_turn(&mut self, input: &str, ui: &mut dyn UiSink) -> Result<()> {
        self.messages.push(Message::user(input));
        let tool_defs = tools::tool_defs();

        for _round in 0..MAX_TOOL_ROUNDS {
            let mut saw_output = false;
            let turn: Turn = self
                .client
                .stream_chat(&self.model, &self.messages, &tool_defs, |ev| match ev {
                    StreamEvent::Content(s) => {
                        saw_output = true;
                        ui.on_content(&s);
                    }
                    StreamEvent::Reasoning(s) => ui.on_reasoning(&s),
                })
                .await
                .context("chat completion failed")?;
            let _ = saw_output;

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
                let result = self.execute_call(&tc, ui).await;
                self.messages.push(Message::tool_result(&tc.id, result));
            }
        }
        anyhow::bail!("agent stopped after {MAX_TOOL_ROUNDS} tool rounds (possible loop)");
    }

    async fn execute_call(&mut self, tc: &crate::api::ToolCall, ui: &mut dyn UiSink) -> String {
        let action = match tools::parse_tool_action(&tc.function.name, &tc.function.arguments) {
            Ok(a) => a,
            Err(e) => return format!("Error parsing tool call: {e}"),
        };

        // Approval gate.
        let approved = match action.danger() {
            tools::Danger::Safe => true,
            tools::Danger::High => ui.approve(&action),
            tools::Danger::Moderate => {
                let is_edit =
                    matches!(action, tools::Action::WriteFile { .. } | tools::Action::EditFile { .. });
                match self.mode {
                    ApprovalMode::FullAuto => true,
                    ApprovalMode::AutoEdit if is_edit => true,
                    _ => ui.approve(&action),
                }
            }
        };
        if !approved {
            return "User DECLINED this action. Ask what to do differently or proceed another way."
                .to_string();
        }

        match action.perform(&self.cwd).await {
            Ok(out) => {
                ui.on_tool_done(&action, true);
                out
            }
            Err(e) => {
                ui.on_tool_done(&action, false);
                format!("Command failed: {e:#}")
            }
        }
    }
}

/// Collect AGENTS.md instructions: global config one, then nearest ancestors
/// up to the repo root (max 3 files, 8 KB each).
fn load_agents_md(cwd: &PathBuf) -> String {
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
    let mut cur = cwd.clone();
    loop {
        chain.push(cur.join("AGENTS.md"));
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
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
