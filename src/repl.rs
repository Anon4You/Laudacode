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
            AgentEvent::ToolDone { ok: false, .. } => {
                println!("{}", "✗ failed".red().bold());
                let _ = std::io::stdout().flush();
            }
            AgentEvent::ToolDone { ok: true, .. } => {}
            _ => {}
        }
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
    /// Conversation was replaced (resume) — TUI must clear its transcript.
    Reload(String),
    /// Final state sent right before the worker exits, so the shell
    /// goodbye can offer an exact `--resume <id>` command.
    SessionSummary { id: String, messages: usize },
    /// Generic picker: model lists, resume lists, approval modes…
    Pick { title: String, items: Vec<String> },
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
    /// Replace the live conversation with a stored session id.
    ResumeSession(String),
    /// Attach a local image to the next submitted prompt.
    QueueImage(String),
    Quit,
}

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
                            items: models,
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
                let _ = ev_tx.send(WorkerEvent::Info(format!("model set to {model}")));
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
                Ok(()) => {
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
            WorkerCmd::InitAgentsMd => match init_agents_md(&app.cwd) {
                Ok(msg) => {
                    let _ = ev_tx.send(WorkerEvent::Info(msg));
                }
                Err(e) => {
                    let _ = ev_tx.send(WorkerEvent::Error(format!("{e:#}")));
                }
            },
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
                    Ok(msg) => { let _ = ev_tx.send(WorkerEvent::Reload(msg)); }
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

/// Replace the live conversation with a stored session.
fn resume_session(app: &mut App, id: &str) -> Result<String> {
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
    Ok(format!("resumed session {id}"))
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

fn init_agents_md(cwd: &std::path::Path) -> Result<String> {
    let path = cwd.join("AGENTS.md");
    if path.exists() {
        return Ok("AGENTS.md already exists here".into());
    }
    let stub = "# AGENTS.md — instructions for AI coding agents\n\n\
        - Describe build/test commands here.\n\
        - Describe code conventions the agent must follow.\n\
        - Keep it short; it is loaded into the system prompt.\n";
    std::fs::write(&path, stub).with_context(|| format!("writing {}", path.display()))?;
    Ok("created AGENTS.md — edit it to give the agent project instructions".into())
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
}

/// Summary of a finished TUI session, used for the exit resume hint.
pub struct SessionExit {
    pub id: String,
    pub messages: usize,
}

impl App {
    /// True when no provider is usable yet (fresh install, no config).
    pub fn needs_onboarding(&self) -> bool {
        self.active.api_key.is_empty()
            || self.active.base_url.is_empty()
            || (self.config.providers.is_empty() && self.active.name == "default")
    }

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

    fn tui_main(self) -> Result<SessionExit> {
        let initial_id = self.session.id.clone();
        let mut tui = Tui::new();
        // Keep UI and agent in lock-step from frame one: without this the
        // composer claimed BUILD while the agent still enforced read-only
        // PLAN rules (the agent's default is Suggest, the widget's was Build).
        tui.mode = tui_mode_of(self.agent.mode);
        tui.set_usage(0, self.ctx_window);
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
        tui.push(Entry::Info(format!(
            "LaudaCode ready — model {} · mode {}\nTab cycles PLAN → BUILD → FULL AUTO · type / for commands (Tab completes) · Esc interrupts{}",
            self.active.model,
            tui.mode.label(),
            config_note
        )));
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
                        tui.set_status("switching model");
                        let _ = ui_cmd.send(WorkerCmd::SetModel(model.to_string()));
                    } else if let Some(resume_id) = sel.strip_prefix("resume:") {
                        let real = resume_id.split(" · ").next().unwrap_or(resume_id).to_string();
                        tui.set_status("restoring session");
                        let _ = ui_cmd.send(WorkerCmd::ResumeSession(real));
                    } else if let Some(image) = sel.strip_prefix("image:") {
                        let _ = ui_cmd.send(WorkerCmd::QueueImage(image.to_string()));
                    } else if let Some(mode_label) = sel.strip_prefix("approvals:") {
                        apply_mode_by_label(tui, &ui_cmd, mode_label);
                    }
                }
                KeyAction::Submit(text) => {
                    if text.starts_with('/') {
                        if !handle_slash(tui, &ui_cmd, &text) {
                            let _ = ui_approve.send(false); // unblock pending approval
                            let _ = ui_cmd.send(WorkerCmd::Quit);
                            return false;
                        }
                    } else if tui.is_busy() && !text.starts_with('#') && !text.starts_with('!') {
                        tui.push(Entry::Error(
                            "agent is busy — press Esc to interrupt first".into(),
                        ));
                    } else {
                        if text.starts_with('#') || text.starts_with('!') {
                            // Memory notes / shell passthrough run even while busy.
                        } else {
                            tui.push(Entry::User(text.clone()));
                        }
                        tui.set_status("thinking");
                        let _ = ui_cmd.send(WorkerCmd::Submit(text));
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
        let mode = mode_override
            .or_else(|| {
                config
                    .approval_mode
                    .as_deref()
                    .and_then(ApprovalMode::parse)
            })
            .unwrap_or(ApprovalMode::Suggest);
        let client = ChatClient::new(
            &active.base_url,
            &active.api_key,
            &active.headers,
            active.name == "openrouter" || active.base_url.contains("openrouter"),
            active.reasoning_effort.clone(),
        )?;
        let agent = Agent::new(client, active.model.clone(), cwd.clone(), mode);

        let session = Session::new();
        let ui = TermUi::new()?;
        let ctx_window = config.context_window.unwrap_or(128_000);
        Ok(Self { config, active, agent, session, ui, cwd, ctx_window, pending_images: vec![] })
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
        let agent = Agent::new(client, String::new(), cwd.clone(), ApprovalMode::Suggest);
        let session = Session::new();
        let ui = TermUi::new()?;
        let ctx_window = 128_000;
        Ok(Self { config, active, agent, session, ui, cwd, ctx_window, pending_images: vec![] })
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
        WorkerEvent::Ev(AgentEvent::Usage(u)) => {
            let total = tui.ctx_total;
            tui.set_usage(u.prompt_tokens, total);
            tui.set_status(format!("ctx {} tok", u.prompt_tokens));
        }
        WorkerEvent::Ev(AgentEvent::Todo(todos)) => {
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
        WorkerEvent::Reload(s) => {
            tui.entries.clear();
            tui.push(Entry::Info(s));
        }
        // Consumed by tui_main after the loop ends; never reaches the UI.
        WorkerEvent::SessionSummary { .. } => {}
        WorkerEvent::Pick { title, items } => tui.open_picker(title, items),
    }
}

/// Map the agent's approval policy to the TUI mode chip (inverse of
/// `ApprovalMode::from_tui_mode`).
fn tui_mode_of(mode: ApprovalMode) -> tuiapp::Mode {
    match mode {
        ApprovalMode::Suggest => tuiapp::Mode::Plan,
        ApprovalMode::AutoEdit => tuiapp::Mode::Build,
        ApprovalMode::FullAuto => tuiapp::Mode::FullAuto,
    }
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
                 /provider     manage providers (list|show|use <name>)\n\
                 /compact      summarize history to free context\n\
                 /clear        reset conversation\n\
                 /retry        re-run the previous task\n\
                 /resume       restore a previous session by id\n\
                 /image <path> attach an image to your next message\n\
                 /export       save the transcript as markdown\n\
                 /status       provider · model · session info\n\
                 /diff         show uncommitted git changes\n\
                 /init         create an AGENTS.md project brief\n\
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
        "provider" => match arg.first().copied().unwrap_or("list") {
            "list" | "ls" => {
                let _ = cmd.send(WorkerCmd::ListProviders);
            }
            "show" | "status" => {
                let _ = cmd.send(WorkerCmd::ShowProvider);
            }
            "use" => match arg.get(1) {
                Some(name) => {
                    let _ = cmd.send(WorkerCmd::UseProvider((*name).to_string()));
                }
                None => tui.push(Entry::Error("usage: /provider use <name>".into())),
            },
            other => {
                tui.push(Entry::Error(format!(
                    "provider '{other}' not supported in the TUI — use list|use, \
                     or run `laudacode provider add` outside the TUI"
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
    // Same green→cyan→blue gradient as the TUI banner.
    const GRADIENT: &[CT] = &[
        CT::Green,
        CT::DarkGreen,
        CT::Cyan,
        CT::DarkCyan,
        CT::Blue,
        CT::DarkBlue,
    ];
    for (i, line) in crate::tui::BANNER.lines().enumerate() {
        let color = GRADIENT.get(i).copied().unwrap_or(CT::Green);
        println!("{}", line.with(color).bold());
    }
    println!(
        "{}",
        format!(
            " LaudaCode v{} — AI coding agent for your terminal",
            env!("CARGO_PKG_VERSION")
        )
        .dark_grey()
    );
    println!();
}

// ---------------------------------------------------------------------------
// Onboarding wizard (runs before the TUI when no provider is configured)
// ---------------------------------------------------------------------------

/// First-run wizard: never exits unconfigured. Builds a provider, verifies the
/// key with a models fetch when possible, then returns the rebuilt App state
/// via `switch_to`.
pub fn onboarding_wizard(cfg: &mut Config) -> Result<String> {
    print_banner();
    println!("{}", "Welcome to Laudacode — let's connect you to a model provider.".bold());
    println!("{}", "You can re-run this anytime with /provider add.".dark_grey());

    loop {
        let name = add_provider_flow(cfg, None)?;
        let p = cfg.providers.get(&name).context("provider vanished")?;
        // Verify connectivity unless it's a local server.
        if !p.base_url.contains("localhost") && !p.base_url.contains("127.0.0.1") {
            print!("· checking connection… ");
            std::io::stdout().flush().ok();
            match verify_provider(p) {
                Ok(n) => println!("{}", format!("ok ({n} models)").green()),
                Err(e) => {
                    println!("{}", "failed".red().bold());
                    println!("{}", format!("  {e:#}").dark_grey());
                    println!("{}", "Check the key/URL, or press Enter to retry, 's' to save anyway:".dark_grey());
                    let ans = prompt_line("", "")?;
                    if !ans.trim().eq_ignore_ascii_case("s") {
                        cfg.providers.remove(&name);
                        continue;
                    }
                }
            }
        }
        cfg.active_provider = Some(name.clone());
        cfg.save()?;
        return Ok(name);
    }
}

/// Run a future on a throwaway current-thread runtime.
pub fn block_current<F: std::future::Future>(fut: F) -> Result<F::Output> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    Ok(rt.block_on(fut))
}

/// Cheap liveness check: GET /models and count entries.
fn verify_provider(p: &Provider) -> Result<usize> {
    let client = rebuild_client(&ActiveProvider {
        name: "verify".into(),
        base_url: p.base_url.clone(),
        api_key: p.api_key.clone(),
        model: p.model.clone(),
        headers: p.headers.clone(),
        sources: Default::default(),
        reasoning_effort: p.reasoning_effort.clone(),
    })?;
    let models = block_current(client.list_models())??;
    Ok(models.len())
}

// ---------------------------------------------------------------------------
// Provider management helpers (shared with CLI)
// ---------------------------------------------------------------------------

pub fn rebuild_client(active: &ActiveProvider) -> Result<ChatClient> {
    ChatClient::new(
        &active.base_url,
        &active.api_key,
        &active.headers,
        active.base_url.contains("openrouter"),
        active.reasoning_effort.clone(),
    )
}

fn switch_to(cfg: &mut Config, agent: &mut Agent, name: &str, _cwd: &PathBuf) -> Result<()> {
    if !cfg.providers.contains_key(name) {
        bail!("provider '{name}' not found");
    }
    let active = cfg.resolve_active(Some(name), None, None, None)?;
    agent.client = rebuild_client(&active)?;
    agent.model = active.model.clone();
    cfg.active_provider = Some(name.to_string());
    cfg.save()?;
    Ok(())
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
        ("openai", "https://api.openai.com/v1"),
        ("openrouter", "https://openrouter.ai/api/v1"),
        ("groq", "https://api.groq.com/openai/v1"),
        ("deepseek", "https://api.deepseek.com/v1"),
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
    fn header_parsing() {
        let m = parse_headers("X-A: 1 , , X-B: two words ");
        assert_eq!(m.get("X-A").map(String::as_str), Some("1"));
        assert_eq!(m.get("X-B").map(String::as_str), Some("two words"));
        assert_eq!(m.len(), 2);
        assert!(parse_headers("").is_empty());
        assert!(parse_headers("no-colon-here").is_empty());
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
