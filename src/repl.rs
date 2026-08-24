use anyhow::{bail, Context, Result};
use crossterm::style::Stylize;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use crate::agent::{Agent, AgentEvent, ApprovalMode, UiSink};
use crate::api::{ChatClient, Message};
use crate::config::{sanitize_name, ActiveProvider, Config, Provider};
use crate::session::Session;
use crate::tui::{self as tuiapp, Action as KeyAction, Entry, Tui};
use crate::tools::{self, Action};

// ---------------------------------------------------------------------------
// Plain terminal UI sink (used by `exec` mode)
// ---------------------------------------------------------------------------

pub struct TermUi {
    pub rl: DefaultEditor,
    approve_all: bool,
    in_reasoning: bool,
    reasoning_bytes: usize,
    /// `--json`: emit machine-readable event lines instead of prose.
    pub json_out: bool,
}

impl TermUi {
    pub fn new() -> Result<Self> {
        let rl = DefaultEditor::new()?;
        Ok(Self { rl, approve_all: false, in_reasoning: false, reasoning_bytes: 0, json_out: false })
    }

    fn emit_json(kind: &str, data: &str) {
        let obj = serde_json::json!({ "type": kind, "data": data });
        println!("{obj}");
    }

    pub fn begin_turn(&mut self) {
        self.in_reasoning = false;
        self.reasoning_bytes = 0;
    }

    pub fn end_turn(&mut self) {
        println!();
    }
}

impl UiSink for TermUi {
    fn on_event(&mut self, ev: AgentEvent) {
        if self.json_out {
            match ev {
                AgentEvent::Content(d) => Self::emit_json("content", &d),
                AgentEvent::Reasoning(d) => Self::emit_json("reasoning", &d),
                AgentEvent::ToolStart { name, summary } => {
                    let obj = serde_json::json!({ "type": "tool_start", "tool": name, "detail": summary });
                    println!("{obj}");
                }
                AgentEvent::ToolDone { name, ok, preview } => {
                    let obj = serde_json::json!({ "type": "tool_done", "tool": name, "ok": ok, "preview": preview });
                    println!("{obj}");
                }
                AgentEvent::ToolEdit { name, files } => {
                    let obj = serde_json::json!({ "type": "tool_edit", "tool": name, "files": files });
                    println!("{obj}");
                }
                AgentEvent::Usage(u) => {
                    let obj = serde_json::json!({
                        "type": "usage",
                        "prompt_tokens": u.prompt_tokens,
                        "completion_tokens": u.completion_tokens,
                        "total_tokens": u.total_tokens,
                    });
                    println!("{obj}");
                }
                AgentEvent::Todo(todos) => {
                    let obj = serde_json::json!({ "type": "plan", "steps": todos });
                    println!("{obj}");
                }
            }
            return;
        }
        match ev {
            AgentEvent::Content(delta) => {
                let mut sep = String::new();
                if self.in_reasoning {
                    sep.push('\n');
                    self.in_reasoning = false;
                }
                print!("{sep}{delta}");
                let _ = std::io::stdout().flush();
            }
            AgentEvent::Reasoning(delta) => {
                if !self.in_reasoning {
                    print!("{}", "· thinking".dark_grey());
                    self.in_reasoning = true;
                    self.reasoning_bytes = 0;
                }
                self.reasoning_bytes += delta.len();
                if self.reasoning_bytes / 600 > (self.reasoning_bytes - delta.len()) / 600 {
                    print!("{}", ".".dark_grey());
                    let _ = std::io::stdout().flush();
                }
            }
            AgentEvent::ToolStart { name, summary } => {
                println!("{}", format!("· {name}: {summary}").dark_grey());
                let _ = std::io::stdout().flush();
            }
            AgentEvent::ToolEdit { name, files } => {
                use crossterm::style::{Color as CT, Stylize as _};
                for f in &files {
                    println!(
                        "{}",
                        format!("┌─ {} (+{} −{})", f.path, f.added, f.removed)
                            .with(CT::Cyan)
                            .bold()
                    );
                    for l in &f.lines {
                        let line = match l.kind {
                            crate::diff::LineKind::Add => l.text.clone().with(CT::Green),
                            crate::diff::LineKind::Del => l.text.clone().with(CT::Red),
                            crate::diff::LineKind::Meta => format!("  {text}", text = l.text).with(CT::Blue).italic(),
                            crate::diff::LineKind::Ctx => l.text.clone().dark_grey().to_string().dark_grey(),
                        };
                        println!("│{line}");
                    }
                    println!("{}", "└─".with(CT::Cyan));
                }
                let _ = name; // header already shows the tool via ToolStart
                let _ = std::io::stdout().flush();
            }
            AgentEvent::ToolDone { ok: false, .. } => {
                println!("{}", "✗ failed".red().bold());
                let _ = std::io::stdout().flush();
            }
            AgentEvent::ToolDone { ok: true, .. } => {}
            _ => {}
        }
    }

    fn fork(&mut self, prefix: &str) -> Box<dyn UiSink> {
        Box::new(TermSubUi { prefix: prefix.to_string() })
    }

    fn approve(&mut self, action: &Action, danger: tools::Danger) -> bool {
        if self.approve_all && danger != tools::Danger::High {
            return true;
        }
        let desc = if danger == tools::Danger::High {
            format!("{}{}", action.describe(), " [DANGEROUS]".red().bold())
        } else {
            action.describe()
        };
        println!(
            "{}{}{}",
            "? ".yellow().bold(),
            "Laudacode wants to: ".bold(),
            desc
        );
        loop {
            match self.rl.readline("[y]es  [n]o  [a]lways: ") {
                Ok(line) => match line.trim().to_lowercase().as_str() {
                    "y" | "yes" => return true,
                    "" | "n" | "no" => return false,
                    "a" | "always" => {
                        self.approve_all = true;
                        println!(
                            "{}",
                            "  auto-approving for the rest of this session".dark_grey()
                        );
                        return true;
                    }
                    _ => continue,
                },
                Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => return false,
                Err(_) => return false,
            }
        }
    }
}

/// Plain-text sink for one sub-agent in exec mode — prefixes every event so
/// concurrent specialists stay attributable on a dumb terminal.
struct TermSubUi {
    prefix: String,
}

impl UiSink for TermSubUi {
    fn on_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::Content(d) => print!("{}{d}", format!("{} ", self.prefix).dark_grey()),
            AgentEvent::ToolStart { name, summary } => {
                println!("{}", format!("· {prefix}{name}: {summary}", prefix = self.prefix).dark_grey());
            }
            AgentEvent::ToolEdit { files, .. } => {
                use crossterm::style::{Color as CT, Stylize as _};
                for f in &files {
                    println!(
                        "{}",
                        format!("┌─ [{p}] {path} (+{a} −{r})", p = self.prefix, path = f.path, a = f.added, r = f.removed)
                            .with(CT::Cyan)
                    );
                }
            }
            AgentEvent::ToolDone { ok: false, .. } => println!("{}", "✗ failed".red().bold()),
            _ => {}
        }
        let _ = std::io::stdout().flush();
    }

    fn approve(&mut self, action: &Action, danger: tools::Danger) -> bool {
        // Sub-agent approvals in exec mode: plain stdin prompt.
        println!(
            "{}{}{}",
            "? ".yellow().bold(),
            format!("[{}] wants to: ", self.prefix).bold(),
            action.describe()
        );
        if danger == tools::Danger::High {
            println!("{}", "  [DANGEROUS]".red().bold());
        }
        let mut line = String::new();
        print!("[y]es / [n]o: ");
        let _ = std::io::stdout().flush();
        std::io::stdin().read_line(&mut line).is_ok()
            && matches!(line.trim().to_lowercase().as_str(), "y" | "yes" | "")
    }

    fn fork(&mut self, prefix: &str) -> Box<dyn UiSink> {
        Box::new(TermSubUi { prefix: format!("{}>{prefix}", self.prefix) })
    }
}

// ---------------------------------------------------------------------------
// Agent worker thread (the UI never blocks on the agent)
// ---------------------------------------------------------------------------

/// Events flowing from the worker thread to the TUI.
pub enum WorkerEvent {
    Ev(AgentEvent),
    /// Modal approval request; worker blocks until an answer arrives.
    NeedApproval(String),
    /// Worker started/stopped processing a command.
    Busy(bool),
    Info(String),
    Error(String),
    /// Conversation was replaced (resume) — TUI must clear its transcript
    /// and replay the restored messages under the new session identity.
    Reload { text: String, entries: Vec<Entry>, session_id: String },
    /// Final state sent right before the worker exits, so the shell
    /// goodbye can offer an exact `--resume <id>` command.
    SessionSummary { id: String, messages: usize },
    /// Generic picker: model lists, resume lists, approval modes…
    Pick { title: String, items: Vec<String> },
    /// The active endpoint changed (model switch, provider switch or a fresh
    /// `/provider add`) — the dashboard must re-render model/provider.
    ProviderSwitched { provider: String, model: String },
    /// Authenticated catalog fetch failed during `/provider add` — the UI
    /// switches to manual model-name capture instead of a picker.
    SetupModelsFailed,
}

/// Commands flowing from the TUI to the worker thread.
pub enum WorkerCmd {
    Submit(String),
    Retry,
    Compact,
    Clear,
    ListModels,
    SetModel(String),
    UseProvider(String),
    SetApprovalMode(ApprovalMode),
    Export,
    InitAgentsMd,
    ListProviders,
    ShowProvider,
    Status,
    Diff,
    /// Open the /resume picker with recent sessions.
    ListSessions,
    /// Revert file changes made during the most recent agent turn.
    Undo,
    /// Replace the live conversation with a stored session id.
    ResumeSession(String),
    /// Attach a local image to the next submitted prompt.
    QueueImage(String),
    /// Authenticated model-catalog fetch for `/provider add` (key already
    /// captured). Replies with a models picker or SetupModelsFailed.
    SetupListModels { base_url: String, api_key: String },
    /// Persist + activate a provider created by the in-TUI `/provider add`.
    FinishProviderSetup {
        name: String,
        base_url: String,
        model: String,
        api_key: String,
    },
    /// Open a picker of configured providers (`use` or `edit` intent).
    PickProvider(ProviderMenu),
    /// Fetch the catalog for a configured provider so the user can re-pick
    /// its model (`/provider edit`).
    EditProviderPickModel(String),
    /// Persist a new default model for a configured provider.
    EditProviderSetModel { provider: String, model: String },
    /// Replace the stored API key of a configured provider.
    FinishEditApiKey { provider: String, api_key: String },
    /// Persist the chosen color theme.
    SetTheme(String),
    /// Persist the chosen ambient effect.
    SetEffect(String),
    Quit,
}

/// Intent behind the configured-provider picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderMenu {
    Use,
    Edit,
}

/// Extra entry shown at the top of every model picker — catalogs lag behind
/// reality (hidden/experimental models like `stealth/ox-alpha` work by id
/// long before they appear in /models), so typing an id must always win.
pub const MANUAL_MODEL_ITEM: &str = "(+ type a model name instead)";

#[derive(Clone)]
struct WorkerBridge {
    tx: Sender<WorkerEvent>,
    approve_rx: Arc<std::sync::Mutex<Receiver<bool>>>,
    #[allow(dead_code)]
    cancel: Arc<AtomicBool>,
}

impl UiSink for WorkerBridge {
    fn on_event(&mut self, ev: AgentEvent) {
        let _ = self.tx.send(WorkerEvent::Ev(ev));
    }

    fn approve(&mut self, action: &Action, danger: tools::Danger) -> bool {
        let mut desc = action.describe();
        if danger == tools::Danger::High {
            desc.push_str("  [DANGEROUS]");
        }
        let _ = self.tx.send(WorkerEvent::NeedApproval(desc));
        matches!(self.approve_rx.lock().unwrap().recv(), Ok(true))
    }

    /// Each concurrent sub-agent gets its own bridge sharing the same
    /// channels; approval requests queue up in the single TUI modal.
    fn fork(&mut self, prefix: &str) -> Box<dyn UiSink> {
        Box::new(SubBridge { inner: WorkerBridge {
            tx: self.tx.clone(),
            approve_rx: self.approve_rx.clone(),
            cancel: self.cancel.clone(),
        }, prefix: format!("[{prefix}] ") })
    }
}

/// A forked [`WorkerBridge`] tagging events with the sub-agent's name.
struct SubBridge {
    inner: WorkerBridge,
    prefix: String,
}

impl UiSink for SubBridge {
    fn on_event(&mut self, ev: AgentEvent) {
        let tagged = match ev {
            AgentEvent::ToolStart { name, summary } => AgentEvent::ToolStart {
                name: format!("{}{}", self.prefix, name),
                summary,
            },
            AgentEvent::ToolDone { name, ok, preview } => AgentEvent::ToolDone {
                name: format!("{}{}", self.prefix, name),
                ok,
                preview,
            },
            other => other,
        };
        let _ = self.inner.tx.send(WorkerEvent::Ev(tagged));
    }

    fn approve(&mut self, action: &Action, danger: tools::Danger) -> bool {
        self.inner.approve(action, danger)
    }

    fn fork(&mut self, prefix: &str) -> Box<dyn UiSink> {
        self.inner.fork(prefix)
    }
}

pub struct WorkerHandle {
    pub cmd: Sender<WorkerCmd>,
    pub approve: Sender<bool>,
    pub events: Receiver<WorkerEvent>,
    pub cancel: Arc<AtomicBool>,
}

/// Move `App` onto a dedicated thread that owns the agent and answers
/// commands from the TUI. Keeps the UI responsive during streaming and lets
/// Esc interrupt a running turn.
pub fn spawn_worker(app: App) -> WorkerHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
    let (ev_tx, ev_rx) = mpsc::channel::<WorkerEvent>();
    let (approve_tx, approve_rx) = mpsc::channel::<bool>();
    let cancel = Arc::new(AtomicBool::new(false));

    std::thread::Builder::new()
        .name("laudacode-agent".into())
        .spawn(move || worker_main(app, ev_tx, cmd_rx, approve_rx))
        .expect("spawning agent worker thread");

    WorkerHandle { cmd: cmd_tx, approve: approve_tx, events: ev_rx, cancel }
}

fn worker_main(
    mut app: App,
    ev_tx: Sender<WorkerEvent>,
    cmd_rx: Receiver<WorkerCmd>,
    approve_rx: Receiver<bool>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ev_tx.send(WorkerEvent::Error(format!("building runtime: {e}")));
            return;
        }
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let approve_rx = Arc::new(std::sync::Mutex::new(approve_rx));
    let mut last_task: Option<String> = None;
    // Images queued via /image or the -i flag — consumed by the next Submit.
    let mut pending_images: Vec<String> = std::mem::take(&mut app.pending_images);

    for cmd in cmd_rx {
        match cmd {
            WorkerCmd::Quit => {
                // Flush state and tell the host how to resume this session.
                app.persist();
                let _ = ev_tx.send(WorkerEvent::SessionSummary {
                    id: app.session.id.clone(),
                    messages: app.agent.messages.iter().filter(|m| m.role != "system").count(),
                });
                break;
            }
            WorkerCmd::Submit(text) => {
                if let Some(memory) = text.strip_prefix('#') {
                    match add_memory(&app.cwd, memory) {
                        Ok(msg) => { let _ = ev_tx.send(WorkerEvent::Info(msg)); }
                        Err(e) => { let _ = ev_tx.send(WorkerEvent::Error(format!("{e:#}"))); }
                    }
                    continue;
                }
                if let Some(shell_cmd) = text.strip_prefix('!') {
                    run_passthrough_shell(&app, &rt, &ev_tx, shell_cmd);
                    continue;
                }
                // Persist for ↑/↓ recall in future sessions.
                append_prompt_history(&text);
                last_task = Some(text.clone());
                let images = std::mem::take(&mut pending_images);
                run_agent_turn(&mut app, &rt, &ev_tx, &approve_rx, &cancel, &text, &images);
            }
            WorkerCmd::Retry => match last_task.clone() {
                Some(task) => {
                    run_agent_turn(&mut app, &rt, &ev_tx, &approve_rx, &cancel, &task, &[]);
                }
                None => {
                    let _ = ev_tx.send(WorkerEvent::Info("nothing to retry yet".into()));
                }
            },
            WorkerCmd::Compact => {
                let _ = ev_tx.send(WorkerEvent::Busy(true));
                match rt.block_on(app.agent.compact()) {
                    Ok(s) => {
                        app.persist();
                        let preview: String = s.chars().take(400).collect();
                        let _ = ev_tx.send(WorkerEvent::Info(format!(
                            "context compacted:\n{preview}"
                        )));
                    }
                    Err(e) => {
                        let _ = ev_tx.send(WorkerEvent::Error(e.to_string()));
                    }
                }
                let _ = ev_tx.send(WorkerEvent::Busy(false));
            }
            WorkerCmd::Clear => {
                let system = app.agent.messages.first().and_then(|m| m.content.clone());
                app.agent.messages.clear();
                if let Some(sys) = system {
                    app.agent.messages.push(Message::system(sys));
                }
                app.agent.todos.clear();
                app.session.messages.clear();
                let _ = ev_tx.send(WorkerEvent::Info("conversation cleared".into()));
            }
            WorkerCmd::ListModels => {
                let _ = ev_tx.send(WorkerEvent::Busy(true));
                match rt.block_on(app.agent.client.list_models()) {
                    Ok(models) => {
                        let _ = ev_tx.send(WorkerEvent::Pick {
                            title: "model".into(),
                            items: with_manual_model_entry(models),
                        });
                    }
                    Err(e) => {
                        let _ =
                            ev_tx.send(WorkerEvent::Error(format!("listing models: {e:#}")));
                    }
                }
                let _ = ev_tx.send(WorkerEvent::Busy(false));
            }
            WorkerCmd::SetModel(model) => {
                app.agent.model = model.clone();
                let _ = ev_tx.send(WorkerEvent::ProviderSwitched {
                    provider: app.active.name.clone(),
                    model,
                });
            }
            WorkerCmd::SetApprovalMode(mode) => {
                app.agent.mode = mode;
            }
            WorkerCmd::UseProvider(name) => match switch_to(
                &mut app.config,
                &mut app.agent,
                &name,
                &app.cwd,
            ) {
                Ok(active) => {
                    // Keep App state in sync so /status and the dashboard
                    // reflect the switch immediately.
                    app.active = active;
                    let _ = ev_tx.send(WorkerEvent::ProviderSwitched {
                        provider: name.clone(),
                        model: app.agent.model.clone(),
                    });
                    let _ = ev_tx.send(WorkerEvent::Info(format!(
                        "switched to '{name}' — model {}",
                        app.agent.model
                    )));
                }
                Err(e) => {
                    let _ = ev_tx.send(WorkerEvent::Error(format!("{e:#}")));
                }
            },
            WorkerCmd::ListProviders => {
                let mut lines = String::new();
                for (n, p) in &app.config.providers {
                    let star = if Some(n.as_str()) == app.config.active_provider.as_deref() {
                        "*"
                    } else {
                        " "
                    };
                    lines.push_str(&format!("{star} {n} — {} ({})\n", p.base_url, p.model));
                }
                if lines.is_empty() {
                    lines = "no providers configured yet".into();
                }
                let _ = ev_tx.send(WorkerEvent::Info(lines));
            }
            WorkerCmd::ShowProvider => {
                let a = &app.active;
                let src = |f: &str| {
                    a.sources.get(f).map(String::as_str).unwrap_or("none")
                };
                let mut lines = format!(
                    "provider : {}\nbase_url : {} (from: {})\nmodel    : {} (from: {})\napi_key  : set (from: {})\nconfig   : {}\n",
                    a.name,
                    a.base_url,
                    src("base_url"),
                    a.model,
                    src("model"),
                    src("api_key"),
                    Config::toml_path().display(),
                );
                if !a.api_key.is_empty() && placeholder_key(&a.api_key) {
                    lines.push_str("warning  : key looks like a placeholder — fix the config file or unset OPENAI_API_KEY\n");
                }
                if !a.headers.is_empty() {
                    let names: Vec<&str> = a.headers.keys().map(|k| k.as_str()).collect();
                    lines.push_str(&format!("headers  : {}\n", names.join(", ")));
                }
                lines.push_str("precedence: command line > environment > config file");
                let _ = ev_tx.send(WorkerEvent::Info(lines));
            }
            WorkerCmd::Export => match export_transcript(&app) {
                Ok(path) => {
                    let _ = ev_tx.send(WorkerEvent::Info(format!("transcript saved to {}", path.display())));
                }
                Err(e) => {
                    let _ = ev_tx.send(WorkerEvent::Error(format!("export failed: {e:#}")));
                }
            },
            WorkerCmd::InitAgentsMd => {
                if app.cwd.join("AGENTS.md").exists() {
                    let _ = ev_tx.send(WorkerEvent::Info(
                        "AGENTS.md already exists — edit it directly or ask the agent to update it"
                            .into(),
                    ));
                } else {
                    // Smart /init: have the agent analyze the project and
                    // write a real brief instead of dumping a stub.
                    let _ = ev_tx.send(WorkerEvent::Busy(true));
                    const INIT_PROMPT: &str = "\
Create an AGENTS.md file for THIS project in this directory. First explore: read Cargo.toml/package.json/Makefile/etc., list directories, skim key sources. Then write AGENTS.md containing: project overview (2-3 lines), build & test commands, code layout, and conventions you can infer. Keep it under 40 lines.";
                    cancel.store(false, Ordering::Relaxed);
                    let mut bridge = WorkerBridge {
                        tx: ev_tx.clone(),
                        approve_rx: approve_rx.clone(),
                        cancel: cancel.clone(),
                    };
                    match rt.block_on(app.agent.run_turn(INIT_PROMPT, &[], &mut bridge, Some(&cancel))) {
                        Ok(()) => {
                            app.persist();
                            let msg = if app.cwd.join("AGENTS.md").exists() {
                                "created AGENTS.md — review it and adjust to taste".to_string()
                            } else {
                                "agent finished but did not write AGENTS.md — try again".to_string()
                            };
                            let _ = ev_tx.send(WorkerEvent::Info(msg));
                        }
                        Err(e) => {
                            // Offline / broken provider → fall back to stub.
                            let fallback = init_agents_md_stub(&app.cwd)
                                .unwrap_or_else(|fe| format!("{e:#}; stub also failed: {fe:#}"));
                            let _ = ev_tx.send(WorkerEvent::Info(fallback));
                        }
                    }
                    let _ = ev_tx.send(WorkerEvent::Busy(false));
                }
            }
            WorkerCmd::Status => {
                let a = &app.agent;
                let mode = match a.mode {
                    ApprovalMode::Suggest => "PLAN (read-only)",
                    ApprovalMode::AutoEdit => "BUILD (edits auto-approved)",
                    ApprovalMode::FullAuto => "FULL AUTO",
                };
                let src = |f: &str| {
                    app.active.sources.get(f).map(String::as_str).unwrap_or("none")
                };
                let usage = match a.last_usage {
                    Some(u) => format!("ctx {} tok · out {} tok", u.prompt_tokens, u.completion_tokens),
                    None => "no requests yet".to_string(),
                };
                let lines = format!(
                    "provider : {} ({})\nmodel    : {} [key from: {}]\nmode     : {}\nsession  : {} · {} messages\nusage    : {}\ncwd      : {}\nconfig   : {}",
                    app.active.name,
                    app.active.base_url,
                    a.model,
                    src("api_key"),
                    mode,
                    app.session.id,
                    a.messages.len(),
                    usage,
                    app.cwd.display(),
                    Config::toml_path().display(),
                );
                let _ = ev_tx.send(WorkerEvent::Info(lines));
            }
            WorkerCmd::Diff => {
                let _ = ev_tx.send(WorkerEvent::Busy(true));
                let out = rt.block_on(tools::run_shell(
                    "git --no-pager diff --stat HEAD 2>&1 | tail -15; \
                     echo; git --no-pager diff -U1 HEAD 2>&1 | head -c 2500",
                    &app.cwd,
                ));
                match out {
                    Ok(o) if o.contains("[exit: 0]") && o.lines().count() > 1 => {
                        let body = o.split_once('\n').map(|(_, r)| r).unwrap_or(&o);
                        let _ = ev_tx.send(WorkerEvent::Info(format!("git diff:\n{}", body.trim_end())));
                    }
                    Ok(_) => {
                        let _ = ev_tx.send(WorkerEvent::Info("no uncommitted changes".into()));
                    }
                    Err(e) => {
                        let _ = ev_tx.send(WorkerEvent::Error(format!("{e:#}")));
                    }
                }
                let _ = ev_tx.send(WorkerEvent::Busy(false));
            }
            WorkerCmd::Undo => match app.agent.undo_last_turn() {
                Ok(msg) => { let _ = ev_tx.send(WorkerEvent::Info(msg)); }
                Err(e) => { let _ = ev_tx.send(WorkerEvent::Error(format!("{e:#}"))); }
            },
            WorkerCmd::ListSessions => {
                let recent = Session::list_recent(12);
                if recent.is_empty() {
                    let _ = ev_tx.send(WorkerEvent::Info("no saved sessions yet".into()));
                } else {
                    let items = recent
                        .into_iter()
                        .map(|(id, created, preview)| {
                            format!("{id} · {} · {preview}", fmt_unix_date(created))
                        })
                        .collect();
                    let _ = ev_tx.send(WorkerEvent::Pick {
                        title: "resume".into(),
                        items,
                    });
                }
            }
            WorkerCmd::ResumeSession(id) => {
                // The picker sends "id · date · preview" — take the id part.
                let real_id = id.split(" · ").next().unwrap_or(&id).to_string();
                match resume_session(&mut app, &real_id) {
                    Ok((msg, entries)) => {
                        let _ = ev_tx.send(WorkerEvent::Reload {
                            text: msg,
                            entries,
                            session_id: app.session.id.clone(),
                        });
                    }
                    Err(e) => { let _ = ev_tx.send(WorkerEvent::Error(format!("{e:#}"))); }
                }
            }
            WorkerCmd::QueueImage(path) => match load_image_data_uri(&app.cwd, &path) {
                Ok(uri) => {
                    pending_images.push(uri);
                    let _ = ev_tx.send(WorkerEvent::Info(format!(
                        "image attached ({} queued) — it will ride along with your next message",
                        pending_images.len()
                    )));
                }
                Err(e) => {
                    let _ = ev_tx.send(WorkerEvent::Error(format!("{e:#}")));
                }
            },
            WorkerCmd::SetupListModels { base_url, api_key } => {
                // Authenticated catalog peek — this doubles as proof that
                // the freshly typed key actually works before we save it.
                let probe = ChatClient::new(&base_url, &api_key, &Default::default(), false, None);
                match probe.and_then(|c| rt.block_on(c.list_models())) {
                    Ok(models) => {
                        let _ = ev_tx.send(WorkerEvent::Pick {
                            title: "models".into(),
                            items: with_manual_model_entry(models),
                        });
                    }
                    Err(_) => {
                        let _ = ev_tx.send(WorkerEvent::SetupModelsFailed);
                    }
                }
            }
            WorkerCmd::FinishProviderSetup { name, base_url, model, api_key } => {
                match finish_provider_setup(
                    &mut app, &rt, &name, &base_url, &model, &api_key,
                ) {
                    Ok(note) => {
                        let _ = ev_tx.send(WorkerEvent::ProviderSwitched {
                            provider: app.active.name.clone(),
                            model: app.agent.model.clone(),
                        });
                        let _ = ev_tx.send(WorkerEvent::Info(note));
                    }
                    Err(e) => {
                        let _ = ev_tx.send(WorkerEvent::Error(format!("provider setup failed: {e:#}")));
                    }
                }
            }
            WorkerCmd::SetTheme(name) => {
                app.config.theme = Some(name.clone());
                let res = app.config.save();
                let msg = match res {
                    Ok(()) => format!("theme saved: {name}"),
                    Err(e) => format!("{e:#}"),
                };
                let _ = ev_tx.send(WorkerEvent::Info(msg));
            }
            WorkerCmd::SetEffect(name) => {
                app.config.effect = Some(name.clone());
                let res = app.config.save();
                let msg = match res {
                    Ok(()) => format!("effect saved: {name}"),
                    Err(e) => format!("{e:#}"),
                };
                let _ = ev_tx.send(WorkerEvent::Info(msg));
            }
            WorkerCmd::PickProvider(purpose) => {
                if app.config.providers.is_empty() {
                    let _ = ev_tx.send(WorkerEvent::Info(
                        "no providers configured yet — run /provider → add first".into(),
                    ));
                } else {
                    let title = match purpose {
                        ProviderMenu::Use => "provider_use",
                        ProviderMenu::Edit => "provider_edit",
                    };
                    let items = app
                        .config
                        .providers
                        .iter()
                        .map(|(n, p)| format!("{n} · {}", p.model))
                        .collect();
                    let _ = ev_tx.send(WorkerEvent::Pick { title: title.into(), items });
                }
            }
            WorkerCmd::EditProviderPickModel(name) => {
                let Some(p) = app.config.providers.get(&name).cloned() else {
                    let _ = ev_tx.send(WorkerEvent::Error(format!("provider '{name}' not found")));
                    continue;
                };
                let client = ChatClient::new(&p.base_url, &p.api_key, &p.headers, false, None);
                match client.and_then(|c| rt.block_on(c.list_models())) {
                    Ok(models) => {
                        let mut items = with_manual_model_entry(models);
                        items.insert(0, "(keep current model)".into());
                        let _ = ev_tx.send(WorkerEvent::Pick {
                            title: "edit model".into(),
                            items,
                        });
                    }
                    Err(e) => {
                        let _ = ev_tx.send(WorkerEvent::Error(format!(
                            "couldn't list models for '{name}': {e:#} — is the stored key valid?"
                        )));
                    }
                }
            }
            WorkerCmd::EditProviderSetModel { provider, model } => {
                match edit_provider_model(&mut app, &provider, &model) {
                    Ok(msg) => {
                        let _ = ev_tx.send(WorkerEvent::ProviderSwitched {
                            provider: app.active.name.clone(),
                            model: app.agent.model.clone(),
                        });
                        let _ = ev_tx.send(WorkerEvent::Info(msg));
                    }
                    Err(e) => {
                        let _ = ev_tx.send(WorkerEvent::Error(format!("{e:#}")));
                    }
                }
            }
            WorkerCmd::FinishEditApiKey { provider, api_key } => {
                match finish_edit_api_key(&mut app, &rt, &provider, &api_key) {
                    Ok(note) => {
                        let _ = ev_tx.send(WorkerEvent::ProviderSwitched {
                            provider: app.active.name.clone(),
                            model: app.agent.model.clone(),
                        });
                        let _ = ev_tx.send(WorkerEvent::Info(note));
                    }
                    Err(e) => {
                        let _ = ev_tx.send(WorkerEvent::Error(format!("{e:#}")));
                    }
                }
            }
        }
    }
    app.persist();
}

fn run_agent_turn(
    app: &mut App,
    rt: &tokio::runtime::Runtime,
    ev_tx: &Sender<WorkerEvent>,
    approve_rx: &Arc<std::sync::Mutex<Receiver<bool>>>,
    cancel: &Arc<AtomicBool>,
    text: &str,
    images: &[String],
) {
    cancel.store(false, Ordering::Relaxed);
    let _ = ev_tx.send(WorkerEvent::Busy(true));
    let mut bridge = WorkerBridge {
        tx: ev_tx.clone(),
        approve_rx: approve_rx.clone(),
        cancel: cancel.clone(),
    };
    let res = rt.block_on(app.agent.run_turn(text, images, &mut bridge, Some(cancel)));
    if let Err(e) = res {
        let msg = format!("{e:#}");
        let _ = ev_tx.send(if msg.contains("interrupted") {
            WorkerEvent::Info("interrupted".into())
        } else {
            WorkerEvent::Error(msg)
        });
    }
    app.persist();
    let _ = ev_tx.send(WorkerEvent::Busy(false));
}

/// Append a `# memory` bullet to AGENTS.md in the project root.
fn add_memory(cwd: &std::path::Path, note: &str) -> Result<String> {
    let note = note.trim();
    if note.is_empty() {
        bail!("empty memory — usage: #<fact to remember about this project>");
    }
    let path = cwd.join("AGENTS.md");
    let mut body = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => "# AGENTS.md — instructions for AI coding agents\n".to_string(),
    };
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&format!("- {note}\n"));
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(format!("remembered: {note}"))
}

/// Load + base64-encode an image file into a data URI (vision input).
pub fn load_image_data_uri(_cwd: &PathBuf, path: &str) -> Result<String> {
    const ALLOWED: &[(&str, &str)] = &[
        ("png", "image/png"),
        ("jpg", "image/jpeg"),
        ("jpeg", "image/jpeg"),
        ("webp", "image/webp"),
        ("gif", "image/gif"),
    ];
    let p = PathBuf::from(path);
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    let mime = ALLOWED
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, m)| *m)
        .with_context(|| format!(
            "unsupported image type '.{ext}' — use png/jpg/jpeg/webp/gif"
        ))?;
    let bytes = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
    if bytes.len() > 8 * 1024 * 1024 {
        bail!("image too large ({} bytes, limit 8 MiB)", bytes.len());
    }
    Ok(format!("data:{mime};base64,{}", crate::api::base64_encode(&bytes)))
}

/// Replace the live conversation with a stored session. Returns a status
/// line plus the replayed transcript for the TUI.
fn resume_session(app: &mut App, id: &str) -> Result<(String, Vec<Entry>)> {
    let sess = Session::load(id)?;
    let kept: Vec<Message> = sess
        .restore()
        .into_iter()
        .filter(|m| m.role != "system")
        .collect();
    anyhow::ensure!(
        !kept.is_empty(),
        "session '{id}' has no messages to restore"
    );
    app.agent.messages.truncate(1); // keep the fresh system prompt
    app.agent.messages.extend(kept);
    app.agent.messages.push(Message::user(
        "[Resuming previous session above. Continue where it left off.]".to_string(),
    ));
    app.agent.todos.clear();
    app.session = sess;
    app.persist();
    // Replay what happened so the user sees their earlier work on screen.
    let entries = transcript_entries(&app.agent.messages);
    Ok((format!("resumed session {id} — replayed {} items above", entries.len()), entries))
}

/// `1760000000` → `2025-10-09` without pulling chrono.
fn fmt_unix_date(secs: u64) -> String {
    let days = secs / 86_400;
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

fn run_passthrough_shell(
    app: &App,
    rt: &tokio::runtime::Runtime,
    ev_tx: &Sender<WorkerEvent>,
    shell_cmd: &str,
) {
    let trimmed = shell_cmd.trim();
    if trimmed.is_empty() {
        let _ = ev_tx.send(WorkerEvent::Error("usage: !<command>".into()));
        return;
    }
    let _ = ev_tx.send(WorkerEvent::Busy(true));
    match rt.block_on(tools::run_shell(trimmed, &app.cwd)) {
        Ok(out) => {
            let _ = ev_tx.send(WorkerEvent::Info(out.trim_end().to_string()));
        }
        Err(e) => {
            let _ = ev_tx.send(WorkerEvent::Error(format!("{e:#}")));
        }
    }
    let _ = ev_tx.send(WorkerEvent::Busy(false));
}

fn export_transcript(app: &App) -> Result<PathBuf> {
    let mut out = String::from("# Laudacode session\n\n");
    for msg in &app.agent.messages {
        let role = match msg.role.as_str() {
            "system" => continue,
            r => r,
        };
        let body = msg.content.clone().unwrap_or_default();
        if body.is_empty() && !msg.tool_calls.is_empty() {
            out.push_str("## assistant (tool calls)\n");
            for tc in &msg.tool_calls {
                out.push_str(&format!("- `{}` {}\n", tc.function.name, tc.function.arguments));
            }
            out.push('\n');
        } else {
            out.push_str(&format!("## {role}\n\n{body}\n\n"));
        }
    }
    let dir = app.cwd.join(".laudacode");
    std::fs::create_dir_all(&dir).context("creating .laudacode export dir")?;
    let path = dir.join(format!("session-{}.md", app.session.id));
    std::fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn init_agents_md_stub(cwd: &std::path::Path) -> Result<String> {
    let path = cwd.join("AGENTS.md");
    if path.exists() {
        return Ok("AGENTS.md already exists here".into());
    }
    let stub = "# AGENTS.md — instructions for AI coding agents\n\n\
        - Describe build/test commands here.\n\
        - Describe code conventions the agent must follow.\n\
        - Keep it short; it is loaded into the system prompt.\n";
    std::fs::write(&path, stub).with_context(|| format!("writing {}", path.display()))?;
    Ok("created a stub AGENTS.md (offline fallback) — edit it with project instructions".into())
}

// ---------------------------------------------------------------------------
// Custom commands (.laudacode/commands/*.md + global dir)
// ---------------------------------------------------------------------------

/// A user-defined slash command (parity with modern agent CLIs).
#[derive(Debug, Clone)]
pub struct CustomCmd {
    pub name: String,
    pub description: String,
    pub template: String,
}

/// Scan the project `.laudacode/commands/` and the global
/// `config_dir/laudacode/commands/`. Project entries override global ones.
pub fn load_custom_commands(cwd: &std::path::Path) -> Vec<CustomCmd> {
    let mut out: Vec<CustomCmd> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(g) = dirs::config_dir() {
        dirs.push(g.join("laudacode").join("commands"));
    }
    dirs.push(cwd.join(".laudacode").join("commands"));
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut files: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        files.sort_by_key(|e| e.file_name());
        for f in files {
            let name = f.file_name().to_string_lossy().to_string();
            if !name.ends_with(".md") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(f.path()) else { continue };
            let (description, template) = split_frontmatter(&raw);
            let cmd_name = name.trim_end_matches(".md").to_string();
            out.retain(|c: &CustomCmd| c.name != cmd_name); // later dir wins
            out.push(CustomCmd { name: cmd_name, description, template });
        }
    }
    out
}

/// Split optional `--- description: … ---` frontmatter from the body.
fn split_frontmatter(raw: &str) -> (String, String) {
    let trimmed = raw.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let head = &rest[..end];
            let body = rest[end + 4..].trim_start_matches('\n').to_string();
            let mut description = String::new();
            for line in head.lines() {
                if let Some(v) = line.trim().strip_prefix("description:") {
                    description = v.trim().to_string();
                }
            }
            return (description, body);
        }
    }
    (String::new(), trimmed.to_string())
}

/// Render a command template:
/// - `$ARGUMENTS` → all args; `$1..$9` → positional args
/// - `` !`cmd` `` → shell output (runs in cwd)
/// - `@path` → file contents inline
pub fn render_command_template(tpl: &str, args: &str, cwd: &std::path::Path) -> String {
    let argv: Vec<&str> = args.split_whitespace().collect();
    let mut out = tpl.replace("$ARGUMENTS", args.trim());
    for (i, v) in argv.iter().enumerate().take(9) {
        out = out.replace(&format!("${}", i + 1), v);
    }

    // Shell injection: !`cmd`
    while let Some(start) = out.find("!`") {
        let Some(rel_end) = out[start + 2..].find('`') else { break };
        let cmd = &out[start + 2..start + 2 + rel_end];
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .output()
            .ok()
            .map(|o| {
                let mut s =
                    String::from_utf8_lossy(&o.stdout).into_owned();
                s.push_str(&String::from_utf8_lossy(&o.stderr));
                s.trim_end().to_string()
            })
            .unwrap_or_else(|| "(command failed)".into());
        let capped: String = output.chars().take(4000).collect();
        out.replace_range(start..start + 2 + rel_end + 1, &capped);
    }

    // File references: @relative/path (until whitespace)
    let mut rendered = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(at) = rest.find('@') {
        let boundary = at == 0 || !{
            let prev = rest[..at].chars().last().unwrap_or(' ');
            prev.is_alphanumeric()
        };
        let token: String = rest[at + 1..]
            .chars()
            .take_while(|c| !c.is_whitespace())
            .collect();
        if boundary && !token.is_empty() && token != "ARGUMENTS" {
            if let Ok(content) = std::fs::read_to_string(cwd.join(&token)) {
                let capped: String = content.chars().take(8 * 1024).collect();
                rendered.push_str(&rest[..at]);
                rendered.push_str(&format!(
                    "\n--- {token} ---\n{capped}\n--- end {token} ---\n"
                ));
                rest = &rest[at + 1 + token.len()..];
                continue;
            }
        }
        rendered.push_str(&rest[..at + 1]);
        rest = &rest[at + 1..];
    }
    rendered.push_str(rest);
    rendered
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

pub struct App {
    pub config: Config,
    pub active: ActiveProvider,
    pub agent: Agent,
    pub session: Session,
    pub ui: TermUi,
    pub cwd: PathBuf,
    /// Assumed context-window size (tokens) for the TUI meter.
    pub ctx_window: u64,
    /// Data-URI images queued for the first prompt (`-i` flag).
    pub pending_images: Vec<String>,
    /// Entries to replay into the TUI on start (set by session restores).
    pub pending_transcript: Vec<Entry>,
    /// Custom commands loaded at startup (kept for submission routing).
    pending_custom_cmds: Vec<CustomCmd>,
}

/// Summary of a finished TUI session, used for the exit resume hint.
pub struct SessionExit {
    pub id: String,
    pub messages: usize,
}

impl App {
    // -----------------------------------------------------------------------
    // Full-screen TUI mode
    // -----------------------------------------------------------------------

    /// Run the interactive full-screen TUI until the user quits.
    ///
    /// Sync on purpose: the TUI event loop owns the terminal, and all agent
    /// work happens on the worker thread (`spawn_worker`). Returns the final
    /// session identity so the shell can print an exact `--resume` hint.
    pub fn run_tui(self) -> Result<SessionExit> {
        tuiapp::enter_tui()?;
        let res = self.tui_main();
        tuiapp::leave_tui();
        res
    }

    fn tui_main(mut self) -> Result<SessionExit> {
        let initial_id = self.session.id.clone();
        let project_cwd = self.cwd.clone();
        let mut tui = Tui::new();
        // Keep UI and agent in lock-step from frame one: without this the
        // composer claimed BUILD while the agent still enforced read-only
        // PLAN rules (the agent's default is Suggest, the widget's was Build).
        tui.mode = tui_mode_of(self.agent.mode);
        tui.set_usage(0, self.ctx_window);
        // Replay any restored conversation (from --resume / --continue-last)
        // so the transcript shows earlier work from the first frame.
        for e in std::mem::take(&mut self.pending_transcript) {
            tui.push(e);
        }
        tui.dash.set_session(
            &self.session.id,
            &self.active.model,
            &self.active.name,
            &home_shortened(&self.cwd),
            self.agent.messages.iter().filter(|m| m.role != "system").count(),
        );
        // Seed the @-mention list from the project tree.
        {
            let mut files = Vec::new();
            tools::walk_files(&self.cwd, &mut |p| {
                if files.len() < 2000 {
                    if let Ok(rel) = p.strip_prefix(&self.cwd) {
                        files.push(rel.display().to_string());
                    }
                    return true;
                }
                false
            });
            tui.set_files(files);
        }
        // User-defined /commands from .laudacode/commands + global dir.
        let custom_cmds = load_custom_commands(&self.cwd);
        if !custom_cmds.is_empty() {
            let listing = custom_cmds
                .iter()
                .map(|c| format!("  /{:<14} {}", c.name, c.description))
                .collect::<Vec<_>>()
                .join("\n");
            tui.push(Entry::Info(format!(
                "custom commands loaded:\n{listing}\n\nargs: $ARGUMENTS/$1 · @file inlines a file · !`cmd` injects output"
            )));
        }
        tui.custom_templates = custom_cmds
            .iter()
            .map(|c| (c.name.clone(), c.template.clone()))
            .collect();
        tui.set_custom_cmds(
            custom_cmds
                .iter()
                .map(|c| (c.name.clone(), c.description.clone()))
                .collect(),
        );
        self.pending_custom_cmds = custom_cmds;
        let config_note = if placeholder_key(&self.active.api_key) {
            format!(
                "\n⚠ API key looks like a placeholder — fix {} or unset OPENAI_API_KEY",
                Config::toml_path().display()
            )
        } else if self.active.sources.get("api_key").map(String::as_str) == Some("environment") {
            format!(
                "\n· API key is coming from $OPENAI_API_KEY (overrides {})",
                Config::toml_path().display()
            )
        } else {
            String::new()
        };
        // No wizard on first run — the user connects from inside the TUI.
        let needs_setup = self.active.api_key.is_empty() || self.active.model.is_empty();
        tui.needs_setup = needs_setup;
        // ↑/↓ recall of prompts from previous sessions too.
        tui.seed_history(load_prompt_history());
        // Apply persisted look & feel before the first frame renders.
        if let Some(t) = &self.config.theme {
            crate::theme::set(t);
        }
        tui.fx = crate::effects::Engine::new(crate::effects::EffectKind::parse(
            self.config.effect.as_deref(),
        ));
        let shown_model = if self.active.model.is_empty() {
            "(not configured)".to_string()
        } else {
            self.active.model.clone()
        };
        tui.push(Entry::Info(format!(
            "LaudaCode ready — model {shown_model} · mode {}\nTab cycles PLAN → BUILD → FULL AUTO · type / for commands (Tab completes) · Esc interrupts{}",
            tui.mode.label(),
            config_note
        )));
        if needs_setup {
            let presets = PROVIDER_PRESETS
                .iter()
                .map(|(k, _)| *k)
                .collect::<Vec<_>>()
                .join(", ");
            tui.push(Entry::Info(format!(
                "no provider configured yet — run /provider add to connect\npresets: {presets}"
            )));
        }
        let subtitle = format!("· {}", self.active.name);

        let worker = spawn_worker(self);
        // The UI closure takes ownership of the command/approve handles; the
        // event stream is shared (Arc) so we can also catch the worker's
        // final summary after the loop ends.
        let events = Arc::new(std::sync::Mutex::new(worker.events));
        let wait_events = Arc::clone(&events);
        // Clone the channel handles for the closure — `worker` itself keeps
        // nothing else we need here.
        let ui_cmd = worker.cmd.clone();
        let ui_approve = worker.approve.clone();
        let ui_cancel = Arc::clone(&worker.cancel);

        let keep_going = tuiapp::run_tui(&mut tui, subtitle, move |tui, action| {
            // Drain everything the worker produced since the last tick.
            while let Ok(ev) = events.lock().unwrap().try_recv() {
                apply_worker_event(tui, ev);
            }

            match action {
                KeyAction::Quit => {
                    let _ = ui_approve.send(false); // unblock a pending approval
                    let _ = ui_cmd.send(WorkerCmd::Quit);
                    return false;
                }
                KeyAction::CycleMode => cycle_mode(tui, &ui_cmd),
                KeyAction::ToggleBanner => {
                    tui.toggle_banner();
                    let state = if tui.banner_visible() { "shown" } else { "hidden" };
                    tui.set_status(format!("banner {state} — Ctrl+B to toggle"));
                }
                KeyAction::Approve(answer) => {
                    let _ = ui_approve.send(answer);
                }
                KeyAction::ApproveAlways => {
                    // "Always allow": approve now + flip to FULL AUTO.
                    tui.mode = tuiapp::Mode::FullAuto;
                    let _ = ui_cmd.send(WorkerCmd::SetApprovalMode(ApprovalMode::FullAuto));
                    tui.push(Entry::Info(
                        "approved — approval mode set to FULL AUTO for this session".into(),
                    ));
                    let _ = ui_approve.send(true);
                }
                KeyAction::Interrupt => {
                    if tui.is_busy() {
                        ui_cancel.store(true, Ordering::Relaxed);
                        tui.set_status("interrupting…");
                    }
                }
                KeyAction::OpenSlash(sel) => {
                    // Picker selections arrive as "title:value".
                    if let Some(model) = sel.strip_prefix("model:") {
                        if model == MANUAL_MODEL_ITEM {
                            // Hidden/new model not in the catalog — type it.
                            tui.open_input_modal(tuiapp::InputModal::new(
                                "Model id",
                                "Type the exact model id (e.g. stealth/ox-alpha) and press Enter.",
                                false,
                            ));
                            tui.set_status("enter model id");
                        } else {
                            tui.set_status("switching model");
                            let _ = ui_cmd.send(WorkerCmd::SetModel(model.to_string()));
                        }
                    } else if let Some(resume_id) = sel.strip_prefix("resume:") {
                        let real = resume_id.split(" · ").next().unwrap_or(resume_id).to_string();
                        tui.set_status("restoring session");
                        let _ = ui_cmd.send(WorkerCmd::ResumeSession(real));
                    } else if let Some(image) = sel.strip_prefix("image:") {
                        let _ = ui_cmd.send(WorkerCmd::QueueImage(image.to_string()));
                    } else if let Some(mode_label) = sel.strip_prefix("approvals:") {
                        apply_mode_by_label(tui, &ui_cmd, mode_label);
                    } else if let Some(name) = sel.strip_prefix("theme:") {
                        if crate::theme::set(name) {
                            tui.set_status(format!("theme: {name}"));
                            let _ = ui_cmd.send(WorkerCmd::SetTheme(name.to_string()));
                        } else {
                            tui.push(Entry::Error(format!("unknown theme '{name}'")));
                        }
                    } else if let Some(kind) = sel.strip_prefix("effect:") {
                        let k = crate::effects::EffectKind::parse(Some(kind));
                        tui.fx.set(k);
                        tui.set_status(format!("effect: {}", k.as_str()));
                        let _ = ui_cmd.send(WorkerCmd::SetEffect(k.as_str().to_string()));
                    } else if let Some(what) = sel.strip_prefix("provider_menu:") {
                        // `/provider` root menu: add · use · edit.
                        match what.split(" · ").next().unwrap_or("") {
                            "add" => {
                                let items = PROVIDER_PRESETS
                                    .iter()
                                    .map(|(k, u)| format!("{k} · {u}"))
                                    .collect();
                                tui.open_picker("provider_add", items);
                            }
                            "use" => {
                                tui.set_status("loading providers");
                                let _ = ui_cmd.send(WorkerCmd::PickProvider(ProviderMenu::Use));
                            }
                            "edit" => {
                                tui.set_status("loading providers");
                                let _ = ui_cmd.send(WorkerCmd::PickProvider(ProviderMenu::Edit));
                            }
                            _ => {}
                        }
                    } else if let Some(label) = sel.strip_prefix("provider_add:") {
                        // Add step 1: preset picked → API-key dialog opens.
                        match parse_preset_label(label) {
                            Some((key, base_url)) => {
                                tui.pending_setup =
                                    Some(tuiapp::ProviderSetup::add(&key, &base_url));
                                tui.open_input_modal(tuiapp::InputModal::new(
                                    format!("API key — {key}"),
                                    format!("Paste your {key} API key and press Enter.\nIt is masked, stored only in this machine's config, and verified with a live test request before anything is saved."),
                                    true,
                                ));
                                tui.set_status(format!("{key}: enter API key"));
                            }
                            None => tui.push(Entry::Error("bad provider preset".into())),
                        }
                    } else if let Some(models) = sel.strip_prefix("models:") {
                        // Add final step: model chosen from the authenticated
                        // catalog (the key was already proven to get here).
                        let model_choice = tui
                            .pending_setup
                            .as_mut()
                            .filter(|ps| ps.kind == tuiapp::SetupKind::Add)
                            .map(|ps| (ps.name.clone(), ps.base_url.clone(), ps.api_key.clone()));
                        if let Some((name, base_url, api_key)) = model_choice {
                            if models == MANUAL_MODEL_ITEM {
                                // Hidden/new model not in the catalog — type it.
                                tui.open_input_modal(tuiapp::InputModal::new(
                                    format!("Model id — {name}"),
                                    "Catalogs lag behind: type the exact model id (e.g. stealth/ox-alpha) and press Enter.",
                                    false,
                                ));
                                tui.set_status("enter model id");
                            } else if models.starts_with('(') {
                                // Defensive: any other special entry reopens the dialog.
                                tui.open_input_modal(tuiapp::InputModal::new(
                                    format!("Model id — {name}"),
                                    "Type the exact model id and press Enter.",
                                    false,
                                ));
                            } else {
                                tui.push(Entry::User(format!("model: {models}")));
                                tui.pending_setup = None;
                                tui.set_status("saving provider…");
                                let _ = ui_cmd.send(WorkerCmd::FinishProviderSetup {
                                    name,
                                    base_url,
                                    model: models.to_string(),
                                    api_key: api_key.unwrap_or_default(),
                                });
                            }
                        }
                    } else if let Some(label) = sel.strip_prefix("provider_use:") {
                        // Menu → use: label is "{name} · {model}".
                        if let Some((name, _)) = parse_preset_label(label) {
                            tui.set_status(format!("switching to {name}"));
                            let _ = ui_cmd.send(WorkerCmd::UseProvider(name));
                        }
                    } else if let Some(label) = sel.strip_prefix("provider_edit:") {
                        // Menu → edit: pick the provider, then choose a field.
                        if let Some((name, _)) = parse_preset_label(label) {
                            tui.edit_target = Some(name.clone());
                            tui.open_picker(
                                "provider_edit_field",
                                vec![
                                    format!("replace api key · {name}"),
                                    format!("change model · {name}"),
                                ],
                            );
                        }
                    } else if let Some(label) = sel.strip_prefix("provider_edit_field:") {
                        if let Some((field, name)) = parse_preset_label(label) {
                            if field == "replace api key" {
                                tui.pending_setup = Some(tuiapp::ProviderSetup::edit_key(&name));
                                tui.open_input_modal(tuiapp::InputModal::new(
                                    format!("New API key — {name}"),
                                    "Paste the replacement key and press Enter.\nIt is verified live; on failure the old key stays.",
                                    true,
                                ));
                                tui.set_status(format!("{name}: enter new API key"));
                            } else if field == "change model" {
                                tui.set_status(format!("fetching models for {name}…"));
                                let _ = ui_cmd.send(WorkerCmd::EditProviderPickModel(name));
                            }
                        }
                    } else if let Some(model) = sel.strip_prefix("edit model:") {
                        let provider = tui.edit_target.clone();
                        if let Some(provider) = provider {
                            if model == MANUAL_MODEL_ITEM {
                                // Hidden/new model — type the exact id.
                                tui.open_input_modal(tuiapp::InputModal::new(
                                    format!("Model id — {provider}"),
                                    "Type the exact model id (e.g. stealth/ox-alpha) and press Enter.",
                                    false,
                                ));
                                tui.set_status("enter model id");
                            } else if model.starts_with('(') {
                                tui.set_status("model kept");
                                tui.edit_target = None;
                            } else {
                                tui.edit_target = None;
                                tui.set_status("saving model…");
                                let _ = ui_cmd.send(WorkerCmd::EditProviderSetModel {
                                    provider,
                                    model: model.to_string(),
                                });
                            }
                        }
                    }
                }
                KeyAction::Submit(text) => {
                    if text.starts_with('/') {
                        if !handle_slash(tui, &ui_cmd, &text) {
                            let _ = ui_approve.send(false); // unblock pending approval
                            let _ = ui_cmd.send(WorkerCmd::Quit);
                            return false;
                        }
                    } else if tui.needs_setup && !text.starts_with('#') && !text.starts_with('!') {
                        // Nothing configured yet — steer to /provider add
                        // instead of failing on the first API call.
                        tui.push(Entry::Info(
                            "no provider configured — run /provider and choose add\n(pick tokenrouter/openai/openrouter/… → API key → model)".into(),
                        ));
                    } else if tui.is_busy() && !text.starts_with('#') && !text.starts_with('!') {
                        // Queue the next prompt while the agent works — it runs
                        // automatically when the current turn finishes.
                        tui.push(Entry::User(format!("⏳ {text}")));
                        tui.dash.messages += 1;
                        tui.set_status("queued");
                        let _ = ui_cmd.send(WorkerCmd::Submit(text));
                    } else {
                        let mut prompt = text.clone();
                        // Custom commands take priority over built-ins.
                        if prompt.starts_with('/') {
                            let (head, rest) = match prompt.split_once(' ') {
                                Some((h, r)) => (h.to_string(), r.to_string()),
                                None => (prompt.clone(), String::new()),
                            };
                            let bare = head.trim_start_matches('/');
                            if let Some(cc) = custom_lookup(tui, bare) {
                                tui.push(Entry::User(text.clone()));
                                prompt = render_command_template(&cc.template, &rest, &project_cwd);
                                let _ = ui_cmd.send(WorkerCmd::Submit(prompt));
                                return true;
                            }
                        }
                        if text.starts_with('#') || text.starts_with('!') {
                            // Memory notes / shell passthrough run even while busy.
                        } else {
                            tui.push(Entry::User(text.clone()));
                            tui.dash.messages += 1;
                        }
                        tui.set_status("thinking");
                        let _ = ui_cmd.send(WorkerCmd::Submit(prompt));
                    }
                }
                KeyAction::InputSubmit(value) => {
                    // Answer from the centered input dialog (API key / model).
                    if value.trim().is_empty() {
                        tui.set_status("empty input — cancelled");
                        tui.pending_setup = None;
                    } else if let Some(ps) = tui.pending_setup.as_mut() {
                        match (&ps.kind, &ps.api_key) {
                            (tuiapp::SetupKind::Add, None) => {
                                // Key answered → prove it via authenticated catalog.
                                ps.api_key = Some(value.clone());
                                let base_url = ps.base_url.clone();
                                tui.push(Entry::User(format!("api key: {}", mask_key(&value))));
                                tui.set_status("fetching models…");
                                let _ = ui_cmd.send(WorkerCmd::SetupListModels {
                                    base_url,
                                    api_key: value,
                                });
                            }
                            (tuiapp::SetupKind::Add, Some(_)) => {
                                // Manual model id (catalog unavailable or hidden model).
                                let (name, base_url) = (ps.name.clone(), ps.base_url.clone());
                                let api_key = ps.api_key.clone().unwrap_or_default();
                                tui.push(Entry::User(format!("model: {value}")));
                                tui.pending_setup = None;
                                tui.set_status("saving provider…");
                                let _ = ui_cmd.send(WorkerCmd::FinishProviderSetup {
                                    name,
                                    base_url,
                                    model: value,
                                    api_key,
                                });
                            }
                            (tuiapp::SetupKind::EditKey, _) => {
                                let provider = ps.name.clone();
                                tui.push(Entry::User(format!("api key: {}", mask_key(&value))));
                                tui.pending_setup = None;
                                tui.set_status("verifying new key…");
                                let _ = ui_cmd.send(WorkerCmd::FinishEditApiKey {
                                    provider,
                                    api_key: value,
                                });
                            }
                        }
                    } else if let Some(provider) = tui.edit_target.take() {
                        // Typed model id from /provider edit → change model.
                        tui.push(Entry::User(format!("model: {value}")));
                        tui.set_status("saving model…");
                        let _ = ui_cmd.send(WorkerCmd::EditProviderSetModel {
                            provider,
                            model: value,
                        });
                    } else {
                        // Typed model id from /model → live session switch.
                        tui.push(Entry::User(format!("model: {value}")));
                        tui.set_status("switching model");
                        let _ = ui_cmd.send(WorkerCmd::SetModel(value));
                    }
                }
                KeyAction::None => {}
            }
            true
        });

        if keep_going.is_err() {
            return Err(anyhow::anyhow!("tui loop failed"));
        }

        // The worker sends its final state right before exiting — give it a
        // moment so the shell goodbye can offer an exact resume command.
        use std::sync::mpsc::RecvTimeoutError;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut exit_id = initial_id;
        let mut exit_messages = 0usize;
        while std::time::Instant::now() < deadline {
            match wait_events.lock().unwrap().recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(WorkerEvent::SessionSummary { id, messages }) => {
                    exit_id = id;
                    exit_messages = messages;
                    break;
                }
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(_) => break, // worker gone without a summary
            }
        }
        Ok(SessionExit { id: exit_id, messages: exit_messages })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn build_with_config(
        cwd: PathBuf,
        config: Config,
        cli_provider: Option<&str>,
        cli_base_url: Option<&str>,
        cli_api_key: Option<&str>,
        cli_model: Option<&str>,
        mode_override: Option<ApprovalMode>,
    ) -> Result<Self> {
        let active =
            config.resolve_active(cli_provider, cli_base_url, cli_api_key, cli_model)?;
        if let Some(t) = &config.theme {
            crate::theme::set(t);
        }
        // BUILD is the default collaboration mode (auto-approve edits,
        // ask for commands) unless overridden by flag/config.
        let mode = mode_override
            .or_else(|| {
                config
                    .approval_mode
                    .as_deref()
                    .and_then(ApprovalMode::parse)
            })
            .unwrap_or(ApprovalMode::AutoEdit);
        let client = ChatClient::new(
            &active.base_url,
            &active.api_key,
            &active.headers,
            active.name == "openrouter" || active.base_url.contains("openrouter"),
            active.reasoning_effort.clone(),
        )?;
        let permissions = config.permission.clone();
        // Install user-defined specialists from [agents.*] before any
        // delegate schema is built.
        crate::agents::install_custom(&config.agents);
        let agent = Agent::new(client, active.model.clone(), cwd.clone(), mode, permissions);

        let session = Session::new();
        let ui = TermUi::new()?;
        let ctx_window = config.context_window.unwrap_or(128_000);
        Ok(Self { config, active, agent, session, ui, cwd, ctx_window, pending_images: vec![], pending_transcript: vec![], pending_custom_cmds: vec![] })
    }

    /// Bare App with a placeholder provider — used when config resolution
    /// fails on a fresh install so the interactive wizard can still run.
    pub fn build_unconfigured(cwd: PathBuf) -> Result<Self> {
        let config = Config::load().unwrap_or_default();
        let active = ActiveProvider {
            name: "unconfigured".into(),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            headers: Default::default(),
            sources: Default::default(),
            reasoning_effort: None,
        };
        let client = ChatClient::new("http://localhost:0/v1", "", &active.headers, false, None)?;
        let agent = Agent::new(
            client,
            String::new(),
            cwd.clone(),
            ApprovalMode::AutoEdit,
            crate::permissions::Permissions::default(),
        );
        let session = Session::new();
        let ui = TermUi::new()?;
        let ctx_window = 128_000;
        Ok(Self { config, active, agent, session, ui, cwd, ctx_window, pending_images: vec![], pending_transcript: vec![], pending_custom_cmds: vec![] })
    }

    pub fn restore_session(&mut self, sess: Session) {
        let restored = sess.restore();
        let kept: Vec<Message> = restored
            .into_iter()
            .filter(|m| m.role != "system")
            .collect();
        if !kept.is_empty() {
            self.agent.messages.extend(kept);
            self.agent.messages.push(Message::user(
                "[Resuming previous session above. Continue where it left off.]".to_string(),
            ));
        }
        self.session = sess;
        // Replay the restored conversation into the TUI so the user actually
        // SEES their earlier work instead of a blank transcript.
        self.pending_transcript = transcript_entries(&self.agent.messages);
        println!("{}", "· resumed previous session".dark_grey());
    }

    fn persist(&mut self) {
        self.session.messages = self.agent.messages.clone();
        if let Err(e) = self.session.save() {
            eprintln!("{}", format!("warn: could not save session: {e}").dark_grey());
        }
    }

    /// One-shot non-interactive task (exec mode). `images` are data URIs.
    pub async fn run_once(mut self, task: &str, images: &[String]) -> Result<()> {
        print_banner();
        self.ui.begin_turn();
        let res = self.agent.run_turn(task, images, &mut self.ui, None).await;
        self.ui.end_turn();
        res?;
        self.persist();
        Ok(())
    }
}

/// Apply one worker event to the transcript.
fn apply_worker_event(tui: &mut Tui, ev: WorkerEvent) {
    match ev {
        WorkerEvent::Ev(AgentEvent::Content(d)) => tui.push_stream_text(&d),
        WorkerEvent::Ev(AgentEvent::Reasoning(d)) => tui.push_reasoning_text(&d),
        WorkerEvent::Ev(AgentEvent::ToolStart { name, summary }) => {
            tui.clear_status();
            tui.push(Entry::ToolCall { name, summary });
        }
        WorkerEvent::Ev(AgentEvent::ToolDone { name, ok, preview }) => {
            tui.push(Entry::ToolResult { name, ok, preview });
        }
        WorkerEvent::Ev(AgentEvent::ToolEdit { name, files }) => {
            tui.push(Entry::ToolDiff { name, files });
        }
        WorkerEvent::Ev(AgentEvent::Usage(u)) => {
            let total = tui.ctx_total;
            tui.set_usage(u.prompt_tokens, total);
            tui.dash.record_usage(u.prompt_tokens, u.completion_tokens);
            tui.set_status(format!("ctx {} tok", u.prompt_tokens));
        }
        WorkerEvent::Ev(AgentEvent::Todo(todos)) => {
            tui.dash.set_plan(&todos);
            let done = todos.iter().filter(|t| t.status == "completed").count();
            tui.set_status(format!("plan {}/{}", done, todos.len()));
        }
        WorkerEvent::NeedApproval(desc) => {
            tui.open_approval(desc);
        }
        WorkerEvent::Busy(b) => {
            if b {
                tui.set_busy(true, "working");
            } else {
                tui.set_busy(false, "working");
                tui.clear_status();
            }
        }
        WorkerEvent::Info(s) => tui.push(Entry::Info(s)),
        WorkerEvent::Error(s) => tui.push(Entry::Error(s)),
        WorkerEvent::Reload { text, entries, session_id } => {
            let count = entries.len();
            tui.entries.clear();
            for e in entries {
                tui.push(e);
            }
            tui.push(Entry::Info(text));
            tui.dash.session_id = crate::tui::shorten_session_id(&session_id);
            tui.dash.messages = count;
        }
        // Consumed by tui_main after the loop ends; never reaches the UI.
        WorkerEvent::SessionSummary { .. } => {}
        WorkerEvent::Pick { title, items } => tui.open_picker(title, items),
        WorkerEvent::ProviderSwitched { provider, model } => {
            tui.dash.set_endpoint(&provider, &model);
            tui.needs_setup = false;
            tui.subtitle = format!("· {provider}");
            tui.set_status(format!("{provider} · {model}"));
        }
        WorkerEvent::SetupModelsFailed => {
            // The key didn't survive an authenticated catalog fetch — offer
            // the manual-model dialog, but don't pretend it's verified.
            if let Some(ps) = tui.pending_setup.as_ref() {
                if ps.kind == tuiapp::SetupKind::Add {
                    let name = ps.name.clone();
                    tui.open_input_modal(tuiapp::InputModal::new(
                        format!("Model id — {name}"),
                        "Couldn't list models with that key.\nType the exact model id (e.g. gpt-4o-mini) and press Enter to save anyway.",
                        false,
                    ));
                    tui.set_status("enter model id");
                }
            }
        }
    }
}

/// Rebuild visible transcript entries from stored conversation messages so
/// a resumed session shows everything that happened earlier — not just feed
/// it into the model's context silently.
pub fn transcript_entries(messages: &[Message]) -> Vec<Entry> {
    use std::collections::HashMap;
    let mut out: Vec<Entry> = Vec::new();
    // tool_call id -> tool name, so results can be paired with their calls.
    let mut open_calls: HashMap<String, String> = HashMap::new();
    const MARKERS: &[&str] = &[
        "[Resuming previous session",
        "[Conversation was compacted",
    ];
    for m in messages {
        match m.role.as_str() {
            "user" => {
                let Some(text) = &m.content else { continue };
                if MARKERS.iter().any(|k| text.starts_with(k)) {
                    continue; // housekeeping markers are noise on screen
                }
                out.push(Entry::User(text.clone()));
            }
            "assistant" => {
                for tc in &m.tool_calls {
                    open_calls.insert(tc.id.clone(), tc.function.name.clone());
                    let summary = tools::parse_tool_action(
                        &tc.function.name,
                        &tc.function.arguments,
                    )
                    .map(|a| a.describe())
                    .unwrap_or_else(|_| {
                        tc.function.arguments.chars().take(80).collect()
                    });
                    out.push(Entry::ToolCall {
                        name: tc.function.name.clone(),
                        summary,
                    });
                }
                if let Some(text) = &m.content {
                    if !text.trim().is_empty() {
                        out.push(Entry::Assistant(text.clone()));
                    }
                }
            }
            "tool" => {
                let name = m
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| open_calls.get(id).cloned())
                    .unwrap_or_else(|| "tool".into());
                let body = m.content.clone().unwrap_or_default();
                let failed =
                    body.starts_with("Error") || body.starts_with("Command failed");
                out.push(Entry::ToolResult {
                    name,
                    ok: !failed,
                    preview: body.lines().take(4).collect::<Vec<_>>().join("\n"),
                });
            }
            _ => {} // system prompts stay invisible
        }
    }
    // Keep memory bounded on marathon sessions — show the most recent tail.
    if out.len() > 400 {
        out.drain(..out.len() - 400);
    }
    out
}


fn tui_mode_of(mode: ApprovalMode) -> tuiapp::Mode {
    match mode {
        ApprovalMode::Suggest => tuiapp::Mode::Plan,
        ApprovalMode::AutoEdit => tuiapp::Mode::Build,
        ApprovalMode::FullAuto => tuiapp::Mode::FullAuto,
    }
}

/// Display helper: collapse $HOME to ~ for dashboard cwd rows.
fn home_shortened(p: &std::path::Path) -> String {
    match dirs::home_dir() {
        Some(home) => p
            .strip_prefix(&home)
            .map(|rel| format!("~/{}", rel.display()))
            .unwrap_or_else(|_| p.display().to_string()),
        None => p.display().to_string(),
    }
}

/// Find a user-defined command by bare name in the TUI's loaded lists.
fn custom_lookup(tui: &Tui, name: &str) -> Option<CustomCmd> {
    let (_, description) = tui.custom_cmds.iter().find(|(n, _)| n == name)?;
    let template = tui.custom_templates.get(name)?.clone();
    Some(CustomCmd {
        name: name.to_string(),
        description: description.clone(),
        template,
    })
}

/// Cycle PLAN → BUILD → FULL AUTO and inform the worker.
fn cycle_mode(tui: &mut Tui, cmd: &Sender<WorkerCmd>) {
    tui.mode = tui.mode.next();
    let label = match tui.mode {
        tuiapp::Mode::Plan => "PLAN — read-only exploration, no edits",
        tuiapp::Mode::Build => "BUILD — edits auto-approved",
        tuiapp::Mode::FullAuto => "FULL AUTO — everything approved",
    };
    tui.set_status(label);
    tui.push(Entry::Info(format!("switched to {}", tui.mode.label())));
    let _ = cmd.send(WorkerCmd::SetApprovalMode(ApprovalMode::from_tui_mode(tui.mode)));
}

/// Apply a mode chosen from the /approvals picker.
fn apply_mode_by_label(tui: &mut Tui, cmd: &Sender<WorkerCmd>, label: &str) {
    let mode = match label.to_lowercase().as_str() {
        "plan (read-only)" | "plan" => tuiapp::Mode::Plan,
        "build (auto-edit)" | "build" => tuiapp::Mode::Build,
        "full auto" | "full-auto" | "fullauto" => tuiapp::Mode::FullAuto,
        _ => return,
    };
    tui.mode = mode;
    let _ = cmd.send(WorkerCmd::SetApprovalMode(ApprovalMode::from_tui_mode(mode)));
    tui.push(Entry::Info(format!("approval mode set to {}", mode.label())));
}

/// Slash-command dispatch inside the TUI — forwards to the worker./// Returns false when the loop should quit.
fn handle_slash(tui: &mut Tui, cmd: &Sender<WorkerCmd>, line: &str) -> bool {
    let mut parts = line.split_whitespace();
    let name = parts.next().unwrap_or("").trim_start_matches('/').to_lowercase();
    let arg: Vec<&str> = parts.collect();
    match name.as_str() {
        "help" => {
            tui.push(Entry::Info(
                "Type / to open command autocomplete — filter by typing, ↑/↓ to move, Tab or Enter to complete.\n\n\
                 /model        pick a model from the provider's live list\n\
                 /approvals    switch approval mode via picker (or Tab)\n\
                 /agents       list the specialist sub-agent team\n\
                                   /provider     menu: add · use · edit · list\n\
                 /compact      summarize history to free context\n\
                 /clear        reset conversation\n\
                 /retry        re-run the previous task\n\
                 /resume       restore a previous session by id\n\
                 /image <path> attach an image to your next message\n\
                 /export       save the transcript as markdown\n\
                 /status       provider · model · session info\n\
                 /diff         show uncommitted git changes\n\
                 /undo         revert file changes from the last turn\n\
                 /init         analyze the project and write AGENTS.md\n\
                 /your-cmd     custom commands from .laudacode/commands/*.md\n\
                 @file         mention a file in your prompt\n\
                 #note         save a memory into AGENTS.md\n\
                 !<command>    run a shell command locally (no agent)\n\
                 ctrl+o        expand recent tool output\n\
                 /quit         exit Laudacode\n\
                 Esc           interrupt the agent / release scroll"
                    .into(),
            ));
        }
        "approvals" | "mode" => {
            // Static picker opened directly on the UI thread; selection
            // arrives back as OpenSlash("approvals:<label>").
            tui.open_picker(
                "approvals",
                vec![
                    "plan (read-only)".into(),
                    "build (auto-edit)".into(),
                    "full auto".into(),
                ],
            );
        }
        "agents" | "team" => {
            tui.push(Entry::Info(crate::agents::describe_team()));
        }
        "compact" => {
            tui.set_status("compacting");
            let _ = cmd.send(WorkerCmd::Compact);
        }
        "clear" | "new" => {
            let _ = cmd.send(WorkerCmd::Clear);
            tui.entries.clear();
            tui.push(Entry::Info("conversation cleared".into()));
        }
        "quit" | "exit" => {
            tui.push(Entry::Info("goodbye".into()));
            return false;
        }
        "provider" => match arg.first().copied() {
            // Bare `/provider` → fully interactive menu.
            None => {
                tui.open_picker(
                    "provider_menu",
                    vec![
                        "add · connect a new provider".to_string(),
                        "use · switch active provider".to_string(),
                        "edit · change a key or model".to_string(),
                        "list · show configured providers".to_string(),
                    ],
                );
            }
            Some("add" | "setup") => {
                let items = PROVIDER_PRESETS
                    .iter()
                    .map(|(k, u)| format!("{k} · {u}"))
                    .collect();
                tui.open_picker("provider_add", items);
            }
            Some("use") => {
                let _ = cmd.send(WorkerCmd::PickProvider(ProviderMenu::Use));
            }
            Some("edit") => {
                let _ = cmd.send(WorkerCmd::PickProvider(ProviderMenu::Edit));
            }
            Some("cancel" | "abort") => {
                tui.input_modal = None;
                if tui.pending_setup.take().is_some() || tui.edit_target.take().is_some() {
                    tui.set_status("provider setup cancelled");
                    tui.push(Entry::Info("provider setup cancelled".into()));
                } else {
                    tui.set_status("nothing to cancel");
                }
            }
            Some("list" | "ls") => {
                let _ = cmd.send(WorkerCmd::ListProviders);
            }
            Some("show" | "status") => {
                let _ = cmd.send(WorkerCmd::ShowProvider);
            }
            Some(other) => {
                tui.push(Entry::Error(format!(
                    "unknown /provider subcommand '{other}' — open the menu with plain /provider \
                     (add · use · edit · list)"
                )));
            }
        },
        "model" => {
            tui.set_status("fetching models");
            let _ = cmd.send(WorkerCmd::ListModels);
        }
        "retry" => {
            tui.set_status("retrying");
            let _ = cmd.send(WorkerCmd::Retry);
        }
        "resume" | "continue" => {
            tui.set_status("loading sessions");
            let _ = cmd.send(WorkerCmd::ListSessions);
        }
        "image" => match arg.first() {
            Some(path) => {
                let _ = cmd.send(WorkerCmd::QueueImage((*path).to_string()));
            }
            None => tui.push(Entry::Error("usage: /image <path/to/file.png>".into())),
        },
        "export" => {
            let _ = cmd.send(WorkerCmd::Export);
        }
        "init" => {
            let _ = cmd.send(WorkerCmd::InitAgentsMd);
        }
        "status" => {
            tui.set_status("gathering status");
            let _ = cmd.send(WorkerCmd::Status);
        }
        "diff" => {
            tui.set_status("computing diff");
            let _ = cmd.send(WorkerCmd::Diff);
        }
        "theme" => {
            let items = crate::theme::names().into_iter().map(String::from).collect();
            tui.open_picker("theme", items);
        }
        "effect" => {
            let items = crate::effects::EffectKind::all()
                .iter()
                .map(|k| k.as_str().to_string())
                .collect();
            tui.open_picker("effect", items);
        }
        "undo" => {
            tui.set_status("reverting last turn");
            let _ = cmd.send(WorkerCmd::Undo);
        }
        other => {
            tui.push(Entry::Error(format!("unknown command '/{other}' — try /help")));
        }
    }
    true
}

/// Keys that are obviously placeholders — warn instead of failing opaquely.
fn placeholder_key(key: &str) -> bool {
    let k = key.trim().to_lowercase();
    if k.is_empty() {
        return true;
    }
    const PLACEHOLDERS: &[&str] = &[
        "<key>", "your-key", "your_key", "yourkey", "changeme", "change-me",
        "xxx", "placeholder", "sk-test", "sk-xxx", "sk-...", "none", "null",
    ];
    PLACEHOLDERS.iter().any(|p| k == *p || (k.starts_with("sk-") && p.starts_with("sk-") && k.contains(&p[3..])))
}

/// Print the brand banner to a plain terminal (exec mode, wizard).
pub fn print_banner() {
    use crossterm::style::Color as CT;
    // Theme-driven gradient; branding text is embedded in the art itself.
    fn to_ct(c: ratatui::style::Color) -> CT {
        use ratatui::style::Color as RC;
        match c {
            RC::Rgb(r, g, b) => CT::Rgb { r, g, b },
            RC::Black => CT::Black,
            RC::White => CT::White,
            RC::Gray => CT::Grey,
            RC::DarkGray => CT::DarkGrey,
            RC::Red => CT::Red,
            RC::LightRed => CT::DarkRed,
            RC::Green => CT::Green,
            RC::LightGreen => CT::DarkGreen,
            RC::Yellow => CT::Yellow,
            RC::LightYellow => CT::DarkYellow,
            RC::Blue => CT::Blue,
            RC::LightBlue => CT::DarkBlue,
            RC::Cyan => CT::Cyan,
            RC::LightCyan => CT::DarkCyan,
            _ => CT::Green,
        }
    }
    let grad = crate::theme::banner_gradient(crate::tui::BANNER.lines().count());
    for (i, line) in crate::tui::BANNER.lines().enumerate() {
        let color = grad.get(i).copied().map(to_ct).unwrap_or(CT::Green);
        println!("{}", line.with(color).bold());
    }
    println!();
}

// ---------------------------------------------------------------------------
// Provider management helpers (shared with CLI)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Provider management helpers (shared with CLI)
// ---------------------------------------------------------------------------

/// Run a future on a throwaway current-thread runtime.
pub fn block_current<F: std::future::Future>(fut: F) -> Result<F::Output> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    Ok(rt.block_on(fut))
}

// ---------------------------------------------------------------------------
// Prompt history (~/.local/share/laudacode/sessions/history.txt)
// ---------------------------------------------------------------------------

fn history_path() -> PathBuf {
    Session::dir().join("history.txt")
}

/// Load prompts saved by previous sessions for ↑/↓ recall.
pub fn load_prompt_history() -> Vec<String> {
    std::fs::read_to_string(history_path())
        .map(|raw| {
            raw.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Persist one submitted prompt (best-effort; history is a convenience).
fn append_prompt_history(line: &str) {
    if line.trim().is_empty() {
        return;
    }
    let path = history_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    use std::fs::OpenOptions;
    use std::io::Write;
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
    // The file holds everything the user typed — owner-only on unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    trim_history_file(&path);
}

/// Keep the on-disk history within [`HISTORY_MAX`] lines.
fn trim_history_file(path: &PathBuf) {
    const MAX_LINES: usize = 500;
    let Ok(raw) = std::fs::read_to_string(path) else { return };
    let lines: Vec<&str> = raw.lines().collect();
    if lines.len() <= MAX_LINES {
        return;
    }
    let keep = lines[lines.len() - MAX_LINES..].join("\n");
    let _ = std::fs::write(path, keep + "\n");
}

/// Known OpenAI-compatible endpoints surfaced by `/provider add` (TUI) and
/// the interactive `laudacode provider add` flow. OpenRouter first — it is
/// the default recommendation (widest model catalog).
pub const PROVIDER_PRESETS: &[(&str, &str)] = &[
    ("openrouter", "https://openrouter.ai/api/v1"),
    ("tokenrouter", "https://api.tokenrouter.com/v1"),
    ("openai", "https://api.openai.com/v1"),
    ("groq", "https://api.groq.com/openai/v1"),
    ("deepseek", "https://api.deepseek.com/v1"),
    ("together", "https://api.together.xyz/v1"),
    ("ollama", "http://localhost:11434/v1"),
    ("lmstudio", "http://localhost:1234/v1"),
];

/// Split a `"{key} · {base_url}"` picker label back into its parts.
fn parse_preset_label(label: &str) -> Option<(String, String)> {
    let (k, u) = label.split_once(" · ")?;
    Some((k.trim().to_string(), u.trim().to_string()))
}

/// Mask an API key for transcript display: keep only a short tail.
fn mask_key(key: &str) -> String {
    let n = key.chars().count();
    if n <= 8 {
        "••••".into()
    } else {
        let tail: String = key.chars().skip(n - 4).collect();
        format!("••••••••{tail}")
    }
}

/// Prepend the manual-entry option to any model list so hidden/new models
/// that are absent from the catalog can still be typed by id.
fn with_manual_model_entry(mut models: Vec<String>) -> Vec<String> {
    models.truncate(200);
    models.insert(0, MANUAL_MODEL_ITEM.into());
    models
}

pub fn rebuild_client(active: &ActiveProvider) -> Result<ChatClient> {
    ChatClient::new(
        &active.base_url,
        &active.api_key,
        &active.headers,
        active.base_url.contains("openrouter"),
        active.reasoning_effort.clone(),
    )
}

/// Prove a candidate provider's key + model with a real 1-token completion
/// BEFORE it is persisted (shared by TUI setup and CLI `provider add|edit`).
/// Local servers are skipped. Nothing is written by this function.
pub fn verify_provider_creds(p: &Provider) -> Result<()> {
    let local = p.base_url.contains("localhost") || p.base_url.contains("127.0.0.1");
    if local {
        return Ok(());
    }
    println!("· verifying key and model with a live test request…");
    let client = rebuild_client(&ActiveProvider {
        name: "verify".into(),
        base_url: p.base_url.clone(),
        api_key: p.api_key.clone(),
        model: p.model.clone(),
        headers: p.headers.clone(),
        sources: Default::default(),
        reasoning_effort: None,
    })?;
    let res = block_current(client.probe_chat(&p.model))?;
    res.with_context(|| {
        format!(
            "NOT saved — nothing changed. Check the key/model for {} and retry",
            p.base_url
        )
    })?;
    Ok(())
}

fn switch_to(
    cfg: &mut Config,
    agent: &mut Agent,
    name: &str,
    _cwd: &PathBuf,
) -> Result<ActiveProvider> {
    if !cfg.providers.contains_key(name) {
        bail!("provider '{name}' not found");
    }
    let active = cfg.resolve_active(Some(name), None, None, None)?;
    agent.client = rebuild_client(&active)?;
    agent.model = active.model.clone();
    cfg.active_provider = Some(name.to_string());
    cfg.save()?;
    Ok(active)
}

/// Complete an in-TUI `/provider add`: persist the provider, activate it and
/// hot-swap the live client. Returns a human-readable summary (including a
/// soft connectivity check that never fails the setup).
fn finish_provider_setup(
    app: &mut App,
    rt: &tokio::runtime::Runtime,
    name: &str,
    base_url: &str,
    model: &str,
    api_key: &str,
) -> Result<String> {
    let sanitized = sanitize_name(name)?;
    let is_local = base_url.contains("localhost") || base_url.contains("127.0.0.1");
    anyhow::ensure!(
        !api_key.trim().is_empty() || is_local,
        "API key required for {base_url} (local servers may leave it blank)"
    );
    anyhow::ensure!(!model.trim().is_empty(), "model name required");

    let p = Provider {
        base_url: base_url.to_string(),
        api_key: api_key.trim().to_string(),
        model: model.trim().to_string(),
        headers: if base_url.contains("openrouter") {
            parse_headers("HTTP-Referer: https://github.com/Anon4You/Laudacode, X-Title: Laudacode")
        } else {
            Default::default()
        },
        reasoning_effort: None,
    };

    // Prove the key AND the chosen model with a real 1-token completion
    // BEFORE saving anything — a public /models endpoint can't tell a good
    // key from a bad one, so this is the check that prevents broken setups.
    if !is_local {
        let probe = ChatClient::new(base_url, &p.api_key, &p.headers, false, None)?;
        rt.block_on(probe.probe_chat(&p.model)).with_context(|| {
            format!("'{sanitized}' was NOT saved — nothing changed. Fix the key/model and retry /provider add")
        })?;
    }

    // Re-running /provider add for the same preset overwrites cleanly.
    app.config.providers.insert(sanitized.clone(), p);
    let active = switch_to(&mut app.config, &mut app.agent, &sanitized, &app.cwd)?;
    app.active = active;

    Ok(format!(
        "saved and activated '{sanitized}' · {} · {}\n· verified working with a live test request",
        app.active.base_url, app.agent.model
    ))
}

/// `/provider edit` → change model: persist it on the stored provider and
/// hot-swap the live agent when that provider is currently active.
fn edit_provider_model(app: &mut App, provider: &str, model: &str) -> Result<String> {
    let model = model.trim();
    anyhow::ensure!(!model.is_empty(), "model name required");
    {
        let p = app
            .config
            .providers
            .get_mut(provider)
            .ok_or_else(|| anyhow::anyhow!("provider '{provider}' not found"))?;
        p.model = model.to_string();
    }
    let is_active = app.config.active_provider.as_deref() == Some(provider);
    if is_active {
        // Re-resolve + rebuild so the running session uses it immediately.
        let active = switch_to(&mut app.config, &mut app.agent, provider, &app.cwd)?;
        app.active = active;
    } else {
        app.config.save()?;
    }
    Ok(format!("model for '{provider}' set to {model}{}", if is_active { " (live)" } else { "" }))
}

/// `/provider edit` → replace the stored API key of a configured provider.
/// When the provider is active, the live client is rebuilt and verified;
/// when verification fails the old key is restored instead of saving a
/// broken one.
fn finish_edit_api_key(
    app: &mut App,
    rt: &tokio::runtime::Runtime,
    provider: &str,
    api_key: &str,
) -> Result<String> {
    anyhow::ensure!(
        !api_key.trim().is_empty(),
        "API key cannot be empty — setup cancelled, old key kept"
    );
    let is_local = app
        .config
        .providers
        .get(provider)
        .map(|p| p.base_url.contains("localhost") || p.base_url.contains("127.0.0.1"))
        .unwrap_or(false);
    let old_key = {
        let p = app
            .config
            .providers
            .get_mut(provider)
            .ok_or_else(|| anyhow::anyhow!("provider '{provider}' not found"))?;
        std::mem::replace(&mut p.api_key, api_key.trim().to_string())
    };
    let is_active = app.config.active_provider.as_deref() == Some(provider);
    if is_active {
        let active = switch_to(&mut app.config, &mut app.agent, provider, &app.cwd)?;
        app.active = active;
        if !is_local {
            // Prove the new key with a real completion before keeping it;
            // roll back to the old key on failure.
            let model = app.agent.model.clone();
            if let Err(e) = rt.block_on(app.agent.client.probe_chat(&model)) {
                if let Some(p) = app.config.providers.get_mut(provider) {
                    p.api_key = old_key;
                }
                let active = switch_to(&mut app.config, &mut app.agent, provider, &app.cwd)?;
                app.active = active;
                bail!("key rejected ({e:#}) — old key restored");
            }
        }
        Ok(format!("API key updated for '{provider}' — verified with a live test request"))
    } else {
        // Inactive provider: verify before saving so a typo can't poison
        // the config for later.
        let (base_url, model, headers) = {
            let p = app.config.providers.get(provider).unwrap();
            (p.base_url.clone(), p.model.clone(), p.headers.clone())
        };
        if !is_local {
            let probe = ChatClient::new(&base_url, api_key.trim(), &headers, false, None)?;
            rt.block_on(probe.probe_chat(&model)).with_context(|| {
                "key rejected — old key kept unchanged"
            })?;
        }
        app.config.save()?;
        Ok(format!("API key updated for '{provider}' (not active — /provider use to switch)"))
    }
}

pub fn list_providers(cfg: &Config) {
    let active_name = cfg.active_provider.clone().unwrap_or_default();
    if cfg.providers.is_empty() {
        println!("{}", "no providers configured yet".dark_grey());
        println!("run: laudacode provider add");
        return;
    }
    for (name, p) in &cfg.providers {
        let star = if *name == active_name { format!("{}", "*".cyan().bold()) } else { " ".to_string() };
        println!(
            "{star} {:<14} {} ({})",
            name.clone().bold(),
            p.base_url.clone().dark_grey(),
            p.model.clone().green()
        );
    }
}

/// Interactive (or flag-driven) provider creation. Returns the provider name.
pub fn add_provider_flow(
    cfg: &mut Config,
    name_arg: Option<&str>,
) -> Result<String> {
    println!("{}", "── add provider ──".bold());

    let name = match name_arg {
        Some(n) => sanitize_name(n)?,
        None => sanitize_name(&prompt_line("name", "")?)?,
    };
    if cfg.providers.contains_key(&name) {
        bail!("provider '{name}' already exists (use /provider edit {name})");
    }

    let presets: &[(&str, &str)] = &[
        ("openrouter", "https://openrouter.ai/api/v1"),
        ("tokenrouter", "https://api.tokenrouter.com/v1"),
        ("openai", "https://api.openai.com/v1"),
        ("groq", "https://api.groq.com/openai/v1"),
        ("deepseek", "https://api.deepseek.com/v1"),
        ("together", "https://api.together.xyz/v1"),
        ("ollama", "http://localhost:11434/v1"),
        ("lmstudio", "http://localhost:1234/v1"),
        ("custom", ""),
    ];
    println!("{}", "pick a preset:".dark_grey());
    for (i, (pname, url)) in presets.iter().enumerate() {
        println!("  {}) {:<11} {}", i + 1, pname, url.dark_grey());
    }
    let choice = prompt_line("preset number", "1")?;
    let idx: usize = choice.trim().parse().unwrap_or(1);
    let (base_default, is_local) = match presets.get(idx.saturating_sub(1)) {
        Some((_, u)) => (*u, u.contains("localhost")),
        None => ("", false),
    };

    let base_url = prompt_line("base_url", base_default)?;
    let model = prompt_line("model", "")?;
    let api_key = if is_local {
        prompt_line("api_key (blank for none)", "")?
    } else {
        prompt_hidden("api_key")?
    };

    let headers = prompt_line("extra headers (Key: Value, …)", "")?;
    let header_map = parse_headers(&headers);

    let p = Provider { base_url, api_key, model, headers: header_map, reasoning_effort: None };
    // Prove the key/model before touching the config file.
    verify_provider_creds(&p)?;
    cfg.providers.insert(name.clone(), p);
    cfg.save()?;
    println!("· saved provider '{}'", name.clone().green());
    Ok(name)
}

pub fn edit_provider_flow(cfg: &mut Config, name: &str) -> Result<()> {
    let p = cfg
        .providers
        .get(name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("provider '{name}' not found"))?;
    println!("{}", format!("── editing '{name}' (enter = keep current) ──").bold());
    let base_url = prompt_line("base_url", &p.base_url)?;
    let model = prompt_line("model", &p.model)?;
    let api_key_in = prompt_line("api_key (enter = keep stored key)", "")?;
    let headers = prompt_line(
        "extra headers",
        &p.headers
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join(", "),
    )?;
    let updated = Provider {
        base_url,
        model,
        api_key: if api_key_in.is_empty() { p.api_key.clone() } else { api_key_in },
        headers: parse_headers(&headers),
        reasoning_effort: p.reasoning_effort.clone(),
    };
    // Prove the (possibly unchanged) credentials before saving.
    verify_provider_creds(&updated)?;
    cfg.providers.insert(name.to_string(), updated);
    cfg.save()?;
    println!("· updated '{name}' (key stays in {})", Config::toml_path().display());
    Ok(())
}

pub fn parse_headers(s: &str) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for pair in s.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        if let Some((k, v)) = pair.split_once(':') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

fn prompt_line(label: &str, default: &str) -> Result<String> {
    let shown = if default.is_empty() {
        format!("{label}: ")
    } else {
        format!("{label} [{}]: ", default)
    };
    match DefaultEditor::new()?.readline(&shown) {
        Ok(l) => {
            let t = l.trim().to_string();
            Ok(if t.is_empty() { default.to_string() } else { t })
        }
        Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
            bail!("cancelled")
        }
        Err(e) => Err(e.into()),
    }
}

fn prompt_hidden(label: &str) -> Result<String> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal;
    println!("{}", label.to_string() + ": ");
    let _ = std::io::stdout().flush();
    terminal::enable_raw_mode()?;
    let mut out = String::new();
    let res = (|| -> Result<String> {
        loop {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                match k.code {
                    KeyCode::Enter => break,
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        bail!("cancelled")
                    }
                    KeyCode::Backspace => {
                        if out.pop().is_some() {
                            print!("\x08 \x08");
                            let _ = std::io::stdout().flush();
                        }
                    }
                    KeyCode::Char(c) => {
                        out.push(c);
                        print!("*");
                        let _ = std::io::stdout().flush();
                    }
                    _ => {}
                }
            }
        }
        Ok(out)
    })();
    terminal::disable_raw_mode()?;
    println!();
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_command_pipeline_end_to_end() {
        let dir = std::env::temp_dir().join(format!("lc-cmds-{}", std::process::id()));
        let cmds = dir.join(".laudacode/commands");
        std::fs::create_dir_all(&cmds).unwrap();
        std::fs::write(
            cmds.join("greet.md"),
            "---\ndescription: say hi\n---\nHello $1 you said: $ARGUMENTS\n",
        )
        .unwrap();
        // Project overrides global on name clash.
        std::fs::write(cmds.join("review.md"), "Review @notes.md now\n").unwrap();
        std::fs::write(dir.join("notes.md"), "IMPORTANT NOTE CONTENT").unwrap();

        let loaded = load_custom_commands(&dir);
        assert_eq!(loaded.len(), 2);
        let greet = loaded.iter().find(|c| c.name == "greet").unwrap();
        assert_eq!(greet.description, "say hi");

        let rendered = render_command_template(&greet.template, "Bob extra", &dir);
        assert!(rendered.contains("Hello Bob"), "{rendered}");
        assert!(rendered.contains("you said: Bob extra"), "{rendered}");

        let review = loaded.iter().find(|c| c.name == "review").unwrap();
        let rendered = render_command_template(&review.template, "", &dir);
        assert!(rendered.contains("IMPORTANT NOTE CONTENT"), "{rendered}");
        assert!(!rendered.contains("@notes.md") || rendered.contains("--- notes.md ---"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn header_parsing() {
        let m = parse_headers("X-A: 1 , , X-B: two words ");
        assert_eq!(m.get("X-A").map(String::as_str), Some("1"));
        assert_eq!(m.get("X-B").map(String::as_str), Some("two words"));
        assert_eq!(m.len(), 2);
        assert!(parse_headers("").is_empty());
        assert!(parse_headers("no-colon-here").is_empty());
    }

    #[test]
    fn openrouter_preset_is_first_and_labels_roundtrip() {
        let (name, url) = PROVIDER_PRESETS.first().expect("presets non-empty");
        assert_eq!(*name, "openrouter");
        assert_eq!(*url, "https://openrouter.ai/api/v1");
        assert!(
            PROVIDER_PRESETS.iter().any(|(k, _)| *k == "tokenrouter"),
            "tokenrouter stays available"
        );
        // Every preset label round-trips through the picker parser.
        for (k, u) in PROVIDER_PRESETS {
            let parsed = parse_preset_label(&format!("{k} · {u}")).unwrap();
            assert_eq!(parsed, (k.to_string(), u.to_string()));
        }
        assert!(parse_preset_label("no separator here").is_none());
    }

    #[test]
    fn key_masking_keeps_only_tail() {
        assert_eq!(mask_key(""), "••••");
        assert_eq!(mask_key("short"), "••••");
        assert_eq!(mask_key("sk-1234567890abcd"), "••••••••abcd");
    }

    #[test]
    fn cli_verification_skips_local_servers_without_network() {
        let p = Provider {
            base_url: "http://localhost:11434/v1".into(),
            api_key: String::new(),
            model: "qwen2.5-coder:7b".into(),
            headers: Default::default(),
            reasoning_effort: None,
        };
        assert!(verify_provider_creds(&p).is_ok(), "local URLs skip the probe");
    }

    #[test]
    fn default_mode_is_build() {
        let dir = std::env::temp_dir().join(format!("lc-defmode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Fully specified endpoint so resolution never touches env vars.
        let args = (
            Some("t"),
            Some("http://localhost:9/v1"),
            Some("k"),
            Some("m"),
        );
        let app = App::build_with_config(
            dir.clone(),
            Config::default(),
            args.0, args.1, args.2, args.3, None,
        )
        .expect("app builds");
        assert_eq!(app.agent.mode, ApprovalMode::AutoEdit, "BUILD is the default mode");
        // Explicit config still wins over the default.
        let cfg = Config {
            approval_mode: Some("suggest".into()),
            ..Default::default()
        };
        let app = App::build_with_config(dir, cfg, args.0, args.1, args.2, args.3, None).unwrap();
        assert_eq!(app.agent.mode, ApprovalMode::Suggest);
    }

    #[test]
    fn prompt_history_roundtrips_through_disk() {        // Same lock as session tests — both flip LAUDACODE_SESSIONS_DIR.
        let _g = crate::session::test_sync::env_lock();
        let dir = std::env::temp_dir().join(format!("lc-hist-{}-{}", std::process::id(), std::time::Instant::now().elapsed().as_nanos()));
        std::env::set_var("LAUDACODE_SESSIONS_DIR", &dir);
        // Isolated dir starts empty.
        assert!(load_prompt_history().is_empty());
        append_prompt_history("first task");
        append_prompt_history("second task");
        append_prompt_history("   ");
        assert_eq!(load_prompt_history(), vec!["first task", "second task"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restored_sessions_replay_into_visible_transcript() {
        // A realistic stored conversation: user ask → tool call → result → answer.
        let msgs = vec![
            Message::system("system prompt"),
            Message::user("fix the build"),
            Message::assistant_with_tools(
                vec![crate::api::ToolCall {
                    id: "call_0".into(),
                    kind: "function".into(),
                    function: crate::api::FunctionCall {
                        name: "run_command".into(),
                        arguments: r#"{"command":"cargo build"}"#.into(),
                    },
                }],
                None,
            ),
            Message::tool_result("call_0", "[exit: 0]\nFinished dev profile"),
            Message::assistant("Fixed — one missing import."),
        ];
        let entries = transcript_entries(&msgs);
        let kinds: Vec<String> = entries
            .iter()
            .map(|e| match e {
                Entry::User(_) => "user".into(),
                Entry::Assistant(t) => format!("assistant:{t}"),
                Entry::ToolCall { name, .. } => format!("call:{name}"),
                Entry::ToolResult { name, ok, .. } => format!("result:{name}:{ok}"),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "user",
                "call:run_command",
                "result:run_command:true",
                "assistant:Fixed — one missing import.",
            ],
            "{kinds:?}"
        );
        // System prompts and housekeeping markers never leak on screen.
        let with_markers = vec![
            Message::user("[Resuming previous session above. Continue where it left off.]".to_string()),
            Message::user("real question"),
        ];
        let replay = transcript_entries(&with_markers);
        assert_eq!(replay.len(), 1);
    }

    #[test]
    fn placeholder_keys_are_flagged() {
        for k in ["", "<key>", "sk-test-invalid", "changeme", "YOUR-KEY"] {
            assert!(placeholder_key(k), "should flag: {k}");
        }
        // A real-looking OpenRouter key must not be flagged.
        assert!(!placeholder_key("sk-or-v1-abc123def4567890"));
    }
}
