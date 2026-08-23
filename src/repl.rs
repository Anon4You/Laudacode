use anyhow::{bail, Context, Result};
use crossterm::style::Stylize;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::io::Write;
use std::path::PathBuf;

use crate::agent::{Agent, ApprovalMode, UiSink};
use crate::api::{ChatClient, Message};
use crate::config::{sanitize_name, ActiveProvider, Config, Provider};
use crate::session::Session;
use crate::tools::{self, Action};

// ---------------------------------------------------------------------------
// Terminal UI sink
// ---------------------------------------------------------------------------

pub struct TermUi {
    pub rl: DefaultEditor,
    approve_all: bool,
    in_reasoning: bool,
    reasoning_bytes: usize,
}

impl TermUi {
    pub fn new() -> Result<Self> {
        let rl = DefaultEditor::new()?;
        Ok(Self { rl, approve_all: false, in_reasoning: false, reasoning_bytes: 0 })
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
    fn on_content(&mut self, delta: &str) {
        let mut sep = String::new();
        if self.in_reasoning {
            sep.push_str("\n");
            self.in_reasoning = false;
        }
        print!("{sep}{delta}");
        let _ = std::io::stdout().flush();
    }

    fn on_reasoning(&mut self, delta: &str) {
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

    fn approve(&mut self, action: &Action) -> bool {
        if self.approve_all {
            return true;
        }
        let desc = if action.danger() == tools::Danger::High {
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

    fn on_tool_done(&mut self, action: &Action, ok: bool) {
        let mark = if ok { "✓".green().bold() } else { "✗".red().bold() };
        let desc: String = action.describe().chars().take(80).collect();
        println!("{mark} {desc}");
    }
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
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        cwd: PathBuf,
        cli_provider: Option<&str>,
        cli_base_url: Option<&str>,
        cli_api_key: Option<&str>,
        cli_model: Option<&str>,
        mode_override: Option<ApprovalMode>,
    ) -> Result<Self> {
        let config = Config::load()?;
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
        )?;
        let agent = Agent::new(client, active.model.clone(), cwd.clone(), mode);

        // Resume latest session if requested by caller via Config trick — handled outside.
        let session = Session::new();
        let ui = TermUi::new()?;
        Ok(Self { config, active, agent, session, ui, cwd })
    }

    pub fn restore_session(&mut self, sess: Session) {
        let restored = sess.restore();
        let kept: Vec<Message> = restored
            .into_iter()
            .filter(|m| m.role != "system")
            .collect();
        if !kept.is_empty() {
            self.agent.messages.push(Message::user(
                "[Resuming previous session. Context below.]".to_string(),
            ));
            self.agent.messages.extend(kept);
        }
        self.session = sess;
        println!("{}", "· resumed previous session".dark_grey());
    }

    /// Run one user line through the agent (streams to terminal).
    pub async fn chat_turn(&mut self, input: &str) -> Result<()> {
        self.ui.begin_turn();
        let res = self.agent.run_turn(input, &mut self.ui).await;
        self.ui.end_turn();
        res?;
        self.persist();
        Ok(())
    }

    fn persist(&mut self) {
        self.session.messages = self.agent.messages.clone();
        if let Err(e) = self.session.save() {
            eprintln!("{}", format!("warn: could not save session: {e}").dark_grey());
        }
    }

    pub fn save_history(&mut self) {
        if let Some(dir) = dirs::data_dir() {
            let p = dir.join("laudacode").join("history");
            let _ = self.ui.rl.save_history(&p);
        }
    }

    pub fn load_history(&mut self) {
        if let Some(dir) = dirs::data_dir() {
            let p = dir.join("laudacode").join("history");
            let _ = self.ui.rl.load_history(&p);
        }
    }

    // -----------------------------------------------------------------------
    // REPL
    // -----------------------------------------------------------------------

    pub async fn run_repl(mut self) -> Result<()> {
        self.load_history();
        banner(&self.active, self.agent.mode);

        loop {
            let short: String = self.cwd.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "~".into());
            let prompt = format!("{short} » ");
            match self.ui.rl.readline(&prompt) {
                Ok(raw) => {
                    let line = raw.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let _ = self.ui.rl.add_history_entry(line);
                    if line.starts_with('/') {
                        match self.slash(line).await {
                            Ok(SlashResult::Continue) => {}
                            Ok(SlashResult::Exit) => break,
                            Err(e) => err_line(&e),
                        }
                    } else if let Err(e) = self.chat_turn(line).await {
                        err_line(&e);
                    }
                }
                Err(ReadlineError::Interrupted) => {
                    println!("{}", "(^C again or /exit to quit)".dark_grey());
                }
                Err(ReadlineError::Eof) => break,
                Err(e) => return Err(e.into()),
            }
        }
        self.save_history();
        println!("{}", "bye 👋".dark_grey());
        Ok(())
    }

    /// One-shot non-interactive task (exec mode).
    pub async fn run_once(mut self, task: &str) -> Result<()> {
        self.chat_turn(task).await?;
        self.save_history();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Slash commands
    // -----------------------------------------------------------------------

    async fn slash(&mut self, line: &str) -> Result<SlashResult> {
        let mut parts = line[1..].split_whitespace();
        let cmd = parts.next().unwrap_or("");
        let rest: Vec<&str> = parts.collect();

        match cmd {
            "help" => {
                help();
            }
            "exit" | "quit" | "q" => return Ok(SlashResult::Exit),
            "clear" => {
                let agent = Agent::new(
                    rebuild_client(&self.active)?,
                    self.active.model.clone(),
                    self.cwd.clone(),
                    self.agent.mode,
                );
                self.agent = agent;
                self.session = Session::new();
                clear_screen();
                println!("{}", "· conversation cleared".dark_grey());
            }
            "status" => status(self),
            "model" => {
                if rest.is_empty() {
                    println!("current model: {}", self.agent.model.clone().cyan());
                    println!("usage: /model <model-name>");
                } else {
                    let m = rest.join(" ");
                    self.agent.model = m.clone();
                    println!("· model set to {} (session only)", m.cyan());
                }
            }
            "mode" => {
                if rest.is_empty() {
                    println!("approval mode: {}", self.agent.mode.as_str().cyan());
                    println!("usage: /mode <suggest|auto-edit|full-auto>");
                } else {
                    match ApprovalMode::parse(rest[0]) {
                        Some(m) => {
                            self.agent.mode = m;
                            self.config.approval_mode = Some(m.as_str().to_string());
                            let _ = self.config.save();
                            println!("· approval mode: {}", m.as_str().cyan());
                        }
                        None => bail!("unknown mode '{}'. Options: suggest, auto-edit, full-auto", rest[0]),
                    }
                }
            }
            "diff" => {
                let out = tools::run_shell("git --no-pager diff --stat && git --no-pager diff", &self.cwd).await?;
                println!("{}", out.trim_end());
            }
            "context" => {
                if rest.is_empty() {
                    println!("{}", tools::project_overview(&self.cwd));
                } else {
                    for p in rest {
                        match std::fs::read_to_string(p) {
                            Ok(content) => {
                                self.agent.messages.push(Message::user(format!(
                                    "[User attached file: {p}]\n```\n{content}\n```"
                                )));
                                println!("· attached {p}");
                            }
                            Err(e) => bail!("{p}: {e}"),
                        }
                    }
                }
            }
            "compact" => {
                print!("{}", "compacting… ".dark_grey());
                let _ = std::io::stdout().flush();
                match self.agent.compact().await {
                    Ok(summary) => {
                        println!("{}", "done".dark_grey());
                        println!("{}", summary.dark_grey());
                        self.persist();
                    }
                    Err(e) => println!("\n{}", format!("✗ {e}").red()),
                }
            }
            "save" => {
                self.persist();
                println!("· saved session {}", self.session.id.clone().cyan());
            }
            "init" => {
                self.chat_turn(
                    "Create an AGENTS.md file for this project. Inspect the repo first \
                     (list_dir/read_file), then write a concise AGENTS.md covering: project \
                     overview, build/test commands, code style conventions, and any gotchas. \
                     If AGENTS.md already exists, improve it.",
                )
                .await?;
            }
            "provider" => {
                self.provider_cmd(&rest)?;
            }
            other => bail!(
                "unknown command '/{other}' — try /help"
            ),
        }
        Ok(SlashResult::Continue)
    }

    fn provider_cmd(&mut self, rest: &[&str]) -> Result<()> {
        match rest.first().copied().unwrap_or("list") {
            "add" => {
                let name = add_provider_flow(&mut self.config, rest.get(1).copied())?;
                switch_to(&mut self.config, &mut self.agent, &name, &self.cwd)?;
                println!("· switched to '{name}'");
            }
            "list" => list_providers(&self.config),
            "ls" => list_providers(&self.config),
            "use" => {
                let name = rest.get(1).copied().context("usage: /provider use <name>")?;
                switch_to(&mut self.config, &mut self.agent, name, &self.cwd)?;
                println!("· switched to '{name}'");
            }
            "edit" => {
                let name = rest.get(1).copied().context("usage: /provider edit <name>")?;
                edit_provider_flow(&mut self.config, name)?;
                if self.active.name == name {
                    switch_to(&mut self.config, &mut self.agent, name, &self.cwd)?;
                    println!("· reloaded active provider '{name}'");
                }
            }
            "rm" | "remove" | "delete" => {
                let name = rest.get(1).copied().context("usage: /provider rm <name>")?;
                if self.config.providers.remove(name).is_none() {
                    bail!("provider '{name}' not found");
                }
                self.config.save()?;
                println!("· removed '{name}'");
            }
            "show" => {
                let fallback = self.active.name.clone();
                let name = rest.get(1).copied().unwrap_or(fallback.as_str());
                match self.config.providers.get(name) {
                    Some(p) => {
                        println!("name:     {}", name.cyan());
                        println!("base_url: {}", p.base_url);
                        println!("model:    {}", p.model);
                        println!("api_key:  {}", mask_key(&p.api_key));
                        if !p.headers.is_empty() {
                            println!("headers:  {:?}", p.headers.keys());
                        }
                    }
                    None => bail!("provider '{name}' not found"),
                }
            }
            other => bail!("unknown provider command '{other}' — try add|list|use|edit|rm|show"),
        }
        Ok(())
    }
}

enum SlashResult {
    Continue,
    Exit,
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

pub fn mask_key(key: &str) -> String {
    if key.len() <= 8 {
        "****".into()
    } else {
        format!("{}…{}", &key[..4], &key[key.len() - 4..])
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

    let p = Provider { base_url, api_key, model, headers: header_map };
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
    let key_shown = mask_key(&p.api_key);
    let api_key_in = prompt_line(&format!("api_key [{key_shown}]"), &p.api_key)?;
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
    };
    cfg.providers.insert(name.to_string(), updated);
    cfg.save()?;
    println!("· updated '{name}'");
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
    loop {
        match DefaultEditor::new()?.readline(&shown) {
            Ok(l) => {
                let t = l.trim().to_string();
                return Ok(if t.is_empty() { default.to_string() } else { t });
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
                bail!("cancelled")
            }
            Err(e) => return Err(e.into()),
        }
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

// ---------------------------------------------------------------------------
// Misc UI
// ---------------------------------------------------------------------------

fn status(app: &App) {
    println!(
        "provider: {}   model: {}   mode: {}",
        app.active.name.clone().cyan(),
        app.agent.model.clone().green(),
        app.agent.mode.as_str().yellow()
    );
    println!("base_url: {}", app.active.base_url.clone().dark_grey());
    println!(
        "session:  {}   messages: {}   dir: {}",
        app.session.id.clone().dark_grey(),
        app.agent.messages.len(),
        app.cwd.display()
    );
}

fn help() {
    println!("{}", "commands".bold());
    println!("  /help                 show this help");
    println!("  /provider add|list|use|edit|rm|show [name]");
    println!("                        manage API providers (multi-provider support)");
    println!("  /model <name>         switch model (session)");
    println!("  /mode <suggest|auto-edit|full-auto>");
    println!("                        approval policy for edits & commands");
    println!("  /status               current configuration overview");
    println!("  /context [path…]      attach file(s) to the conversation");
    println!("  /diff                 show git diff of the working tree");
    println!("  /init                 generate AGENTS.md for this project");
    println!("  /compact              summarize history to free context space");
    println!("  /clear                start a fresh conversation");
    println!("  /save                 persist session");
    println!("  /exit                 quit");
    println!();
    println!("{}", "tools the agent can use".bold());
    println!("  list_dir · read_file · write_file · edit_file · run_command · fetch_url");
}

pub fn banner(active: &ActiveProvider, mode: ApprovalMode) {
    let logo = r#"
    _             _            _
   | |    __ _ __| | ____ _ __| | ___
   | |   / _` / _| |/ / _` / _` |/ _ \
   | |__| (_| \__ \   (_| (_| |  __/
   |_____\__,_|___/_|\_\__,_|\__,_|\___|
"#;
    println!("{}", logo.cyan());
    println!(
        "  v{} · {} · {} · mode={}   (type /help)",
        env!("CARGO_PKG_VERSION"),
        active.name.clone().cyan(),
        active.model.clone().green(),
        mode.as_str().yellow()
    );
    println!();
}

pub fn clear_screen() {
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0)
    );
}

pub fn err_line(e: &anyhow::Error) {
    println!("{} {e:#}", "✗".red().bold());
}
