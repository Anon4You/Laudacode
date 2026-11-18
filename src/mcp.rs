//! Minimal MCP (Model Context Protocol) client — stdio transport only.
//!
//! An MCP server is a child process that speaks JSON-RPC 2.0 over its stdin and
//! stdout, one JSON object per line (NOT the Content-Length framing LSP uses —
//! the two are easy to confuse and the difference is silent: the server simply
//! never answers).
//!
//! Scope, deliberately small:
//!   * `initialize` / `notifications/initialized` handshake
//!   * `tools/list` (following `nextCursor`) and `tools/call`
//!   * server-initiated requests are answered `-32601` so a server that wants
//!     sampling or roots does not block forever waiting on us
//!
//! No HTTP/SSE transport, no resources, no prompts, no elicitation. Servers are
//! configured in `[[mcp_servers]]`, connected once at startup, and reconnected
//! lazily if they die.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Default per-request budget. Generous because some servers shell out on the
/// first `tools/call` (installing a package, warming an index), but bounded so
/// a wedged server cannot hang the agent forever.
const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Handshake budget. Kept short: the binary is already running, so a slow
/// `initialize` means a broken server.
const INIT_TIMEOUT_SECS: u64 = 20;
/// Bound on retained stderr/notifications. A chatty server must not grow these
/// without limit over a long session.
const LOG_CAP: usize = 50;
/// One JSON-RPC reply may not exceed this. A malformed or hostile server must
/// not be able to exhaust memory with a single newline-free line.
const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
/// A server that stops draining stdin must not wedge the agent forever.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ServerSpec {
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// `false` skips the server without deleting its config.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Working directory for the child. Relative paths resolve against the
    /// agent's cwd.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Whether these tools may be used in PLAN (read-only) mode. Defaults to
    /// `false`: we cannot know what a third-party tool does, and Plan mode
    /// promises the user nothing mutates.
    #[serde(default)]
    pub plan: bool,
    /// Per-request budget in seconds. Real servers vary wildly — some shell
    /// out on first call and install a package — so this is a knob, not a
    /// constant. 0 means the default.
    #[serde(default)]
    pub timeout_secs: u64,
}

/// Hand-written so `enabled` matches the serde default. A derived `Default`
/// would leave it `false`, and every `ServerSpec::default()` — including a
/// config with the key simply omitted — would silently drop the server.
impl Default for ServerSpec {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            enabled: true,
            cwd: None,
            plan: false,
            timeout_secs: 0,
        }
    }
}

fn default_true() -> bool {
    true
}

impl ServerSpec {
    /// A server with no command is a config typo, not a disabled server —
    /// catching it here beats an opaque spawn failure later.
    pub fn validate(&self, name: &str) -> Result<()> {
        if self.command.trim().is_empty() {
            bail!("[mcp_servers.{name}] needs a `command`");
        }
        Ok(())
    }
}

/// One tool advertised by a server.
#[derive(Debug, Clone)]
pub struct McpTool {
    /// Name as the server knows it, used in the `tools/call` params.
    pub tool: String,
    /// Name the model sees: `mcp__<server>__<tool>`.
    pub qualified: String,
    pub description: String,
    pub schema: serde_json::Value,
}

/// Marks a tool call as coming from an MCP server.
pub const PREFIX: &str = "mcp__";

/// OpenAI function names accept `[A-Za-z0-9_-]` only, and MCP tool names are
/// free-form (`fs.read`, `github/create_issue`, `a b`). Fold anything else to
/// `_` so an exotic server name cannot produce a schema the provider rejects.
fn sanitize(part: &str) -> String {
    part.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Build the model-facing name for a server tool.
pub fn qualified_name(server: &str, tool: &str) -> String {
    format!("{PREFIX}{}__{}", sanitize(server), sanitize(tool))
}

/// Split a qualified name without consulting any server. Lossy in the same way
/// [`sanitize`] is, so real dispatch goes through [`Mcp::resolve`].
pub fn split_qualified(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix(PREFIX)?;
    let (server, tool) = rest.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then_some((server, tool))
}

/// A tool schema must be a JSON Schema object or the provider rejects the
/// whole request. Servers occasionally omit `inputSchema` or send a non-object.
fn normalize_schema(v: serde_json::Value) -> serde_json::Value {
    if v.is_object() {
        v
    } else {
        serde_json::json!({ "type": "object", "properties": {} })
    }
}

#[derive(Debug, Deserialize)]
struct ToolDefWire {
    name: String,
    #[serde(default)]
    description: String,
    /// Servers send `inputSchema`; a few send `input_schema` (Python SDKs).
    #[serde(default, rename = "inputSchema")]
    input_schema: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct ToolsListResult {
    #[serde(default)]
    tools: Vec<ToolDefWire>,
    #[serde(default, rename = "nextCursor")]
    next_cursor: Option<String>,
}

/// Live child process plus the pipes needed to talk to it.
struct Child {
    proc: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    /// `None` only while a request has it checked out, so exactly one reader
    /// ever owns the stdout pipe.
    reader: Option<BufReader<tokio::process::ChildStdout>>,
    /// Bytes already read but not yet terminated by a newline. Kept across
    /// request timeouts so a cancelled read cannot swallow the next reply.
    line_buf: Vec<u8>,
    /// Server→client notifications seen while waiting for a response.
    notifications: Arc<Mutex<Vec<serde_json::Value>>>,
    /// First stderr lines, for diagnosing a server that dies on startup.
    stderr: Arc<Mutex<Vec<String>>>,
    next_id: u64,
}

pub struct Server {
    pub name: String,
    pub spec: ServerSpec,
    pub tools: Vec<McpTool>,
    /// Why the last connect attempt failed, if it did.
    pub last_error: Option<String>,
    child: Option<Child>,
}

impl Server {
    /// A server that is configured but not yet spawned.
    #[cfg(test)]
    pub fn new(name: String, spec: ServerSpec) -> Self {
        Self {
            name,
            spec,
            tools: Vec::new(),
            last_error: None,
            child: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.child.is_some()
    }

    /// Stop the server politely, then not so politely.
    pub async fn shutdown(&mut self) {
        let Some(c) = self.child.take() else {
            return;
        };
        let Child {
            mut proc, stdin, ..
        } = c;
        // Closing stdin is the documented shutdown signal for stdio servers.
        drop(stdin);
        let _ = tokio::time::timeout(Duration::from_secs(3), proc.wait()).await;
        if proc.try_wait().ok().flatten().is_none() {
            let _ = proc.start_kill();
        }
        self.tools.clear();
    }

    #[cfg(test)]
    pub fn notifications(&self) -> Vec<String> {
        self.child
            .as_ref()
            .map(|c| {
                c.notifications
                    .lock()
                    .map(|n| {
                        n.iter()
                            .map(|v| {
                                v.get("method")
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("?")
                                    .to_string()
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }
}

pub struct Mcp {
    pub servers: Vec<Server>,
    pub cwd: std::path::PathBuf,
}

impl Mcp {
    pub fn from_specs(cwd: &Path, specs: BTreeMap<String, ServerSpec>) -> Self {
        let servers = specs
            .into_iter()
            .filter(|(_, s)| s.enabled)
            .map(|(name, spec)| Server {
                name,
                spec,
                tools: Vec::new(),
                last_error: None,
                child: None,
            })
            .collect();
        Self {
            servers,
            cwd: cwd.to_path_buf(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// Resolve a model-facing tool name to `(server index, real tool name)`.
    ///
    /// Matches against connected servers rather than re-splitting the string,
    /// because [`sanitize`] is lossy: a server named `a.b` and one named `a_b`
    /// produce the same key, and only the live registry can tell them apart.
    pub fn resolve(&self, name: &str) -> Option<(usize, String)> {
        let (server, tool) = split_qualified(name)?;
        let si = self
            .servers
            .iter()
            .position(|s| sanitize(&s.name) == server)?;
        let real = self.servers[si]
            .tools
            .iter()
            .find(|t| sanitize(&t.tool) == tool)
            .map(|t| t.tool.clone())?;
        Some((si, real))
    }

    /// Install an already-connected server's tools. Test-only: `connect`
    /// builds these itself.
    #[cfg(test)]
    pub fn register(&mut self, name: String, spec: ServerSpec, tools: Vec<McpTool>) {
        self.servers.push(Server::new(name, spec));
        self.servers.last_mut().expect("just pushed").tools = tools;
    }

    /// Whether this tool may be used in PLAN mode, i.e. whether its server
    /// opted in with `plan = true`.
    pub fn server_allows_plan(&self, qualified: &str) -> bool {
        self.resolve(qualified)
            .map(|(i, _)| self.servers[i].spec.plan)
            .unwrap_or(false)
    }

    /// Tool schemas for the registry. Empty until a server connects, which is
    /// deliberate: one broken server must not take the whole toolset down.
    pub fn tool_defs(&self) -> Vec<crate::api::ToolDef> {
        let mut out = Vec::new();
        for s in &self.servers {
            for t in &s.tools {
                let mut desc = t.description.trim().to_string();
                if desc.is_empty() {
                    desc = format!("Tool `{}` from MCP server `{}`.", t.tool, s.name);
                }
                // Provenance matters: the model should know this is
                // third-party, not a built-in with the same authority.
                desc.push_str(&format!(" (MCP server `{}`)", s.name));
                out.push(crate::api::ToolDef {
                    r#type: "function",
                    function: crate::api::FunctionDef {
                        name: t.qualified.clone().into(),
                        description: desc.into(),
                        parameters: t.schema.clone(),
                    },
                });
            }
        }
        out
    }

    /// Connect every enabled server, recording failures instead of aborting.
    /// Returns the names that failed.
    pub async fn connect_all(&mut self) -> Vec<String> {
        let mut failed = Vec::new();
        for i in 0..self.servers.len() {
            if let Err(e) = self.connect(i).await {
                failed.push(format!("{}: {e:#}", self.servers[i].name));
            }
        }
        failed
    }

    /// Spawn a server and complete the handshake. Idempotent.
    ///
    /// Any failure is recorded on the server so `/mcp` can explain itself; a
    /// dead server must not look like a server with no tools.
    pub async fn connect(&mut self, idx: usize) -> Result<()> {
        self.connect_with_cancel(idx, None).await
    }

    async fn connect_with_cancel(
        &mut self,
        idx: usize,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        match self.connect_inner(idx, cancel).await {
            Ok(()) => {
                self.servers[idx].last_error = None;
                Ok(())
            }
            Err(e) => {
                let mut msg = format!("{e:#}");
                if let Some(tail) = self.stderr_tail(idx).first() {
                    msg.push_str(&format!(" (stderr: {tail})"));
                }
                self.servers[idx].last_error = Some(msg.clone());
                // Reap the half-started process rather than leaking it.
                self.servers[idx].shutdown().await;
                Err(e)
            }
        }
    }

    async fn connect_inner(
        &mut self,
        idx: usize,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        if self.servers[idx].is_running() {
            return Ok(());
        }
        let name = self.servers[idx].name.clone();
        let spec = self.servers[idx].spec.clone();
        spec.validate(&name)?;
        let budget = spec.timeout_secs;

        let mut cmd = tokio::process::Command::new(&spec.command);
        cmd.args(&spec.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // Piped and drained on a task, never inherited: an inherited stderr
            // would scribble over the TUI.
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let dir = spec
            .cwd
            .as_ref()
            .map_or_else(|| self.cwd.clone(), |c| self.cwd.join(c));
        cmd.current_dir(&dir);
        crate::child_env::scrub(&mut cmd);
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        let mut proc = cmd
            .spawn()
            .with_context(|| format!("spawning MCP server '{name}' ({})", spec.command))?;
        let stdin = proc.stdin.take().context("server stdin unavailable")?;
        let stdout = proc.stdout.take().context("server stdout unavailable")?;
        let stderr = proc.stderr.take();

        // Drain stderr continuously. If nothing reads that pipe it fills, the
        // server blocks on write, and every later request times out — the
        // classic "it worked until the second call" MCP bug.
        let stderr_buf: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        if let Some(pipe) = stderr {
            let sink = Arc::clone(&stderr_buf);
            tokio::spawn(async move {
                let mut lines = BufReader::new(pipe).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(mut v) = sink.lock() {
                        v.push(line);
                        if v.len() > LOG_CAP {
                            let drop_n = v.len() - LOG_CAP;
                            v.drain(..drop_n);
                        }
                    }
                }
            });
        }

        self.servers[idx].child = Some(Child {
            proc,
            stdin,
            reader: Some(BufReader::new(stdout)),
            line_buf: Vec::new(),
            notifications: Arc::new(Mutex::new(Vec::new())),
            stderr: stderr_buf,
            next_id: 0,
        });
        let init = serde_json::json!({
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "clientInfo": { "name": "laudacode", "version": env!("CARGO_PKG_VERSION") }
            }
        });
        self.request(
            idx,
            "initialize",
            init,
            Duration::from_secs(if budget == 0 {
                INIT_TIMEOUT_SECS
            } else {
                budget
            }),
            cancel,
        )
        .await
        .with_context(|| format!("initializing MCP server '{name}'"))?;

        // A notification: no id, no reply expected.
        self.notify(
            idx,
            &serde_json::json!({ "method": "notifications/initialized" }),
        )
        .await?;

        self.list_tools(idx, cancel).await?;
        Ok(())
    }

    /// `tools/list`, following `nextCursor` until the server stops paging.
    async fn list_tools(
        &mut self,
        idx: usize,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        let name = self.servers[idx].name.clone();
        let mut tools: Vec<McpTool> = Vec::new();
        let mut cursor: Option<String> = None;
        // Bounded so a server that always hands back a cursor cannot spin us.
        for _ in 0..64 {
            let params = match &cursor {
                Some(c) => serde_json::json!({ "cursor": c }),
                None => serde_json::json!({}),
            };
            let reply = self
                .request(idx, "tools/list", params, self.timeout(idx), cancel)
                .await
                .with_context(|| format!("tools/list on MCP server '{name}'"))?;
            let parsed: ToolsListResult = serde_json::from_value(reply)
                .with_context(|| format!("malformed tools/list from '{name}'"))?;
            for t in parsed.tools {
                tools.push(McpTool {
                    qualified: qualified_name(&name, &t.name),
                    tool: t.name,
                    description: t.description,
                    schema: normalize_schema(t.input_schema),
                });
            }
            match parsed.next_cursor {
                Some(c) if !c.is_empty() => cursor = Some(c),
                _ => break,
            }
        }
        // `sanitize` is lossy, so `a.b` and `a_b` collide. Fail loudly at
        // connect time: `resolve` can only return the first match, so letting
        // the second server through would route calls to the wrong process.
        for t in &tools {
            if self
                .servers
                .iter()
                .enumerate()
                .any(|(j, s)| j != idx && s.tools.iter().any(|e| e.qualified == t.qualified))
            {
                anyhow::bail!(
                    "tool name collision: '{}' is already provided by another \
                     MCP server. Rename the server or the tool.",
                    t.qualified
                );
            }
        }
        self.servers[idx].tools = tools;
        Ok(())
    }

    /// Invoke a tool and flatten the MCP content blocks into text.
    pub async fn call_tool(
        &mut self,
        idx: usize,
        tool: &str,
        arguments: &str,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<String> {
        let name = self.servers[idx].name.clone();
        if !self.servers[idx].is_running() {
            self.connect_with_cancel(idx, cancel).await.ok();
        }
        if crate::agent::is_cancelled(cancel) {
            bail!("cancelled");
        }
        if !self.servers[idx].is_running() {
            bail!("MCP server '{name}' is not running");
        }
        // Models occasionally pass a bare string or array; the wire format
        // wants an object, so wrap rather than reject.
        let args: serde_json::Value = match serde_json::from_str::<serde_json::Value>(arguments) {
            Ok(v) if v.is_object() => v,
            Ok(other) => serde_json::json!({ "input": other }),
            Err(_) => serde_json::json!({}),
        };
        let params = serde_json::json!({ "name": tool, "arguments": args });
        let reply = self
            .request(idx, "tools/call", params, self.timeout(idx), cancel)
            .await
            .with_context(|| format!("tools/call '{tool}' on MCP server '{name}'"))?;
        Ok(flatten_content(&reply))
    }

    async fn notify(&mut self, idx: usize, msg: &serde_json::Value) -> Result<()> {
        let mut line = serde_json::to_vec(msg)?;
        line.push(b'\n');
        let c = self.servers[idx]
            .child
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("MCP server is not running"))?;
        let write = async {
            c.stdin
                .write_all(&line)
                .await
                .context("writing to MCP server")?;
            c.stdin.flush().await.context("flushing MCP server stdin")?;
            Ok::<(), anyhow::Error>(())
        };
        match tokio::time::timeout(WRITE_TIMEOUT, write).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                self.servers[idx].shutdown().await;
                bail!("timed out writing to MCP server")
            }
        }
    }

    /// Send a request and read until its response arrives, answering any
    /// server→client request so the server is never left waiting on us.
    async fn request(
        &mut self,
        idx: usize,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<serde_json::Value> {
        let id = {
            let c = self.servers[idx]
                .child
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("MCP server is not running"))?;
            c.next_id += 1;
            c.next_id
        };
        let msg = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        });
        self.notify(idx, &msg).await?;

        // Check the reader out so exactly one owner touches the stdout pipe.
        // The pending buffer comes too: a timeout may cancel `fill_buf` mid-line.
        let (mut reader, mut line_buf) = {
            let c = self.servers[idx]
                .child
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("MCP server stdout is unavailable"))?;
            let reader = c
                .reader
                .take()
                .ok_or_else(|| anyhow::anyhow!("MCP server stdout is unavailable"))?;
            (reader, std::mem::take(&mut c.line_buf))
        };

        let deadline = tokio::time::Instant::now() + timeout;
        let mut line = String::new();
        let outcome: Result<serde_json::Value> = loop {
            if crate::agent::is_cancelled(cancel) {
                break Err(anyhow::anyhow!("cancelled"));
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break Err(self
                    .timeout_error(idx, timeout)
                    .context("MCP request timed out"));
            }
            match tokio::time::timeout(
                remaining.min(Duration::from_millis(200)),
                read_line_capped(&mut reader, &mut line_buf, &mut line),
            )
            .await
            {
                Err(_) => {
                    if crate::agent::is_cancelled(cancel) {
                        break Err(anyhow::anyhow!("cancelled"));
                    }
                    continue; // deadline and cancel re-checked at the top
                }
                Ok(Err(e)) => break Err(e).context("reading from MCP server"),
                Ok(Ok(None)) => break Err(self.closed_error(idx)),
                Ok(Ok(Some(_))) => {}
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let v: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                // A server that prints a banner to stdout breaks strict
                // JSON-RPC. Skipping the line beats aborting the session.
                Err(_) => continue,
            };
            if v.get("method").is_some() {
                self.handle_server_message(idx, &v).await;
                continue;
            }
            // Match the id. A late reply to a request we already timed out on
            // must not be mistaken for this one's answer.
            match v.get("id").and_then(|i| i.as_u64()) {
                Some(got) if got != id => continue,
                None => continue,
                _ => {}
            }
            if let Some(err) = v.get("error") {
                let msg = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error");
                break Err(anyhow::anyhow!("{msg}"));
            }
            break Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null));
        };

        // Hand the reader back even on failure, or the server is unusable.
        if let Some(c) = self.servers[idx].child.as_mut() {
            c.reader = Some(reader);
            c.line_buf = line_buf;
        }
        outcome
    }

    /// A server→client message: answer requests, stash notifications.
    async fn handle_server_message(&mut self, idx: usize, v: &serde_json::Value) {
        let has_id = v.get("id").is_some();
        if has_id {
            // We implement none of sampling/roots/elicitation, but we must
            // answer or the server waits forever.
            let reply = serde_json::json!({
                "jsonrpc": "2.0", "id": v.get("id").cloned().unwrap_or(serde_json::Value::Null),
                "error": { "code": -32601, "message": "laudacode implements no server-initiated requests" }
            });
            let _ = self.notify(idx, &reply).await;
            return;
        }
        if let Some(c) = self.servers[idx].child.as_mut() {
            if let Ok(mut n) = c.notifications.lock() {
                n.push(v.clone());
                if n.len() > LOG_CAP {
                    n.remove(0);
                }
            }
        }
    }

    fn timeout_error(&self, idx: usize, timeout: Duration) -> anyhow::Error {
        let errs = self.stderr_tail(idx).join("; ");
        anyhow::anyhow!(
            "MCP server '{}' did not answer within {}s{}",
            self.servers[idx].name,
            timeout.as_secs(),
            if errs.is_empty() {
                String::new()
            } else {
                format!(" (stderr: {errs})")
            }
        )
    }

    fn closed_error(&self, idx: usize) -> anyhow::Error {
        let errs = self.stderr_tail(idx).join("; ");
        anyhow::anyhow!(
            "MCP server '{}' closed its stdout{}",
            self.servers[idx].name,
            if errs.is_empty() {
                String::new()
            } else {
                format!(" (stderr: {errs})")
            }
        )
    }

    /// Per-server request budget, falling back to the default.
    fn timeout(&self, idx: usize) -> Duration {
        Duration::from_secs(match self.servers[idx].spec.timeout_secs {
            0 => DEFAULT_TIMEOUT_SECS,
            n => n,
        })
    }

    fn stderr_tail(&self, idx: usize) -> Vec<String> {
        self.servers[idx]
            .child
            .as_ref()
            .and_then(|c| c.stderr.lock().ok().map(|s| s.clone()))
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub async fn shutdown_all(&mut self) {
        for s in &mut self.servers {
            s.shutdown().await;
        }
    }
}

/// Read one newline-terminated message without `read_line`'s unbounded
/// allocation. Returns `None` at EOF.
async fn read_line_capped(
    reader: &mut BufReader<tokio::process::ChildStdout>,
    pending: &mut Vec<u8>,
    out: &mut String,
) -> std::io::Result<Option<usize>> {
    loop {
        if let Some(n) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=n).collect();
            *out = String::from_utf8(line).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "MCP message is not UTF-8",
                )
            })?;
            return Ok(Some(out.len()));
        }
        if pending.len() > MAX_MESSAGE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("MCP message exceeds {} bytes", MAX_MESSAGE_BYTES),
            ));
        }
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if pending.is_empty() {
                return Ok(None);
            }
            let line = std::mem::take(pending);
            *out = String::from_utf8(line).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "MCP message is not UTF-8",
                )
            })?;
            return Ok(Some(out.len()));
        }
        let take = available
            .iter()
            .position(|&b| b == b'\n')
            .map(|n| n + 1)
            .unwrap_or(available.len());
        if pending.len() + take > MAX_MESSAGE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("MCP message exceeds {} bytes", MAX_MESSAGE_BYTES),
            ));
        }
        pending.extend_from_slice(&available[..take]);
        reader.consume(take);
    }
}

/// MCP returns content blocks; the model only wants text.
fn flatten_content(result: &serde_json::Value) -> String {
    let is_error = result
        .get("isError")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let mut out: Vec<String> = Vec::new();
    if let Some(items) = result.get("content").and_then(|c| c.as_array()) {
        for item in items {
            match item.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                        out.push(t.to_string());
                    }
                }
                Some("image") => out.push("[image content omitted]".into()),
                Some("resource") => out.push(
                    item.get("resource")
                        .and_then(|r| r.get("text"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("[resource omitted]")
                        .to_string(),
                ),
                _ => out.push(item.to_string()),
            }
        }
    }
    if out.is_empty() {
        // A server that answers with an unexpected shape should still be
        // legible to the model rather than silently empty.
        out.push(result.to_string());
    }
    let body = out.join("\n");
    let text = if is_error {
        format!("[mcp tool reported an error] {body}")
    } else {
        body
    };
    crate::tools::truncate(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Production uses a current-thread runtime too, so this is the same
    /// interleaving the real thing gets.
    fn rt<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn spec(cmd: &str) -> ServerSpec {
        ServerSpec {
            command: cmd.into(),
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn qualified_names_survive_exotic_server_and_tool_names() {
        assert_eq!(qualified_name("files", "read"), "mcp__files__read");
        // Dots, slashes and spaces are illegal in an OpenAI function name.
        assert_eq!(qualified_name("a.b", "x/y z"), "mcp__a_b__x_y_z");
        assert_eq!(split_qualified("mcp__files__read"), Some(("files", "read")));
        assert_eq!(split_qualified("files__read"), None);
        assert_eq!(split_qualified("mcp__nope"), None);
        assert_eq!(split_qualified("mcp____read"), None);
    }

    #[test]
    fn flatten_caps_hostile_output() {
        let big = "x".repeat(100_000);
        let out = flatten_content(&serde_json::json!({
            "content": [{"type": "text", "text": big}]
        }));
        assert!(out.chars().count() <= 8 * 1024 + 32, "{}", out.len());
        assert!(out.contains("[output truncated]"));
    }

    #[test]
    fn flatten_joins_text_blocks_and_flags_errors() {
        let ok = serde_json::json!({"content": [
            {"type": "text", "text": "one"},
            {"type": "text", "text": "two"}
        ]});
        assert_eq!(flatten_content(&ok), "one\ntwo");
        let err = serde_json::json!({"isError": true, "content": [
            {"type": "text", "text": "nope"}
        ]});
        assert!(flatten_content(&err).contains("[mcp tool reported an error]"));
        // An empty result still yields something the model can read.
        assert!(!flatten_content(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn disabled_servers_are_dropped_at_construction() {
        let mut specs = BTreeMap::new();
        specs.insert("on".to_string(), spec("true"));
        specs.insert(
            "off".to_string(),
            ServerSpec {
                enabled: false,
                ..spec("true")
            },
        );
        let m = Mcp::from_specs(Path::new("."), specs);
        assert_eq!(m.servers.len(), 1);
        assert_eq!(m.servers[0].name, "on");
        assert!(!m.is_empty());
    }

    #[test]
    fn a_server_without_a_command_is_rejected_with_a_useful_message() {
        let mut specs = BTreeMap::new();
        specs.insert("broken".to_string(), ServerSpec::default());
        let m = Mcp::from_specs(Path::new("."), specs);
        let e = m.servers[0].spec.validate("broken").unwrap_err();
        assert!(format!("{e}").contains("broken"), "{e}");
    }

    #[test]
    fn tool_defs_carry_provenance_and_a_usable_schema() {
        let mut specs = BTreeMap::new();
        specs.insert("files".to_string(), spec("true"));
        let mut m = Mcp::from_specs(Path::new("."), specs);
        m.servers[0].tools = vec![McpTool {
            tool: "read".into(),
            qualified: "mcp__files__read".into(),
            description: String::new(),
            schema: normalize_schema(serde_json::Value::Null),
        }];
        let defs = m.tool_defs();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].function.name, "mcp__files__read");
        let d = defs[0].function.description.to_string();
        assert!(d.contains("MCP server `files`"), "{d}");
        // A null schema still has to be a valid object schema.
        assert_eq!(defs[0].function.parameters["type"], "object");
        // And it resolves back to the real, unsanitized tool name.
        assert_eq!(m.resolve("mcp__files__read"), Some((0, "read".to_string())));
        assert_eq!(m.resolve("mcp__files__nope"), None);
        assert_eq!(m.resolve("read_file"), None);
    }

    #[test]
    fn resolve_finds_tools_whose_names_needed_sanitizing() {
        let mut specs = BTreeMap::new();
        specs.insert("a.b".to_string(), spec("true"));
        let mut m = Mcp::from_specs(Path::new("."), specs);
        m.servers[0].tools = vec![McpTool {
            tool: "read/file".into(),
            qualified: qualified_name("a.b", "read/file"),
            description: "d".into(),
            schema: serde_json::json!({}),
        }];
        // The model saw `mcp__a_b__read_file`; the server wants `read/file`.
        assert_eq!(
            m.resolve("mcp__a_b__read_file"),
            Some((0, "read/file".into()))
        );
    }

    /// Drive the client against a real stdio MCP server.
    ///
    /// The script speaks the protocol by hand: it emits a banner, a
    /// notification and a server→client `sampling/createMessage` before
    /// answering anything, pages `tools/list` across two pages, exposes a tool
    /// name (`echo/two`) that is illegal in an OpenAI function name, and
    /// returns a JSON-RPC error for one tool. Asserting against a real process
    /// is the only way to catch framing mistakes, which otherwise show up as
    /// a server that silently never answers.
    /// The fixture lives in the repo so a fresh clone still exercises the real
    /// protocol. `python3` is the only external requirement; without it there
    /// is nothing to test, so skip loudly rather than pass silently.
    fn tiny_server() -> Option<String> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tiny_mcp_server.py");
        if !p.exists() {
            return None;
        }
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: python3 not available for the MCP fixture");
            return None;
        }
        Some(p.display().to_string())
    }

    fn mcp_with(script: &str) -> Mcp {
        let mut specs = BTreeMap::new();
        specs.insert("tiny".to_string(), spec("python3"));
        specs.get_mut("tiny").unwrap().args = vec![script.to_string()];
        Mcp::from_specs(Path::new("."), specs)
    }

    #[test]
    fn end_to_end_handshake_listing_and_calling() {
        let Some(script) = tiny_server() else {
            eprintln!("skipping: tiny MCP server script not present");
            return;
        };
        rt(async {
            let mut m = mcp_with(&script);
            m.connect(0).await.expect("handshake");
            assert!(m.servers[0].is_running());
            assert!(m.servers[0].last_error.is_none());

            // Both pages arrived, and the illegal name was sanitized for the
            // model while the real name is retained for the call.
            let names: Vec<&str> = m.servers[0]
                .tools
                .iter()
                .map(|t| t.qualified.as_str())
                .collect();
            assert!(names.contains(&"mcp__tiny__echo"), "{names:?}");
            assert!(names.contains(&"mcp__tiny__boom"), "{names:?}");
            assert!(names.contains(&"mcp__tiny__echo_two"), "{names:?}");
            assert_eq!(m.servers[0].tools.len(), 3, "{names:?}");
            let real = m.resolve("mcp__tiny__echo_two").unwrap();
            assert_eq!(real.1, "echo/two");

            // The notification was stashed, and the sampling request answered.
            assert!(!m.servers[0].notifications().is_empty());

            // Content blocks flatten to text.
            let out = m
                .call_tool(0, "echo", r#"{"text":"hi"}"#, None)
                .await
                .expect("tools/call");
            assert!(out.contains("ECHO:hi"), "{out}");
            assert!(out.contains("second block"), "{out}");

            // A JSON-RPC error surfaces as an error, not as a silent empty.
            let err = m.call_tool(0, "boom", "{}", None).await.unwrap_err();
            assert!(format!("{err:#}").contains("tool exploded"), "{err:#}");

            m.shutdown_all().await;
            assert!(!m.servers[0].is_running());
        });
    }

    #[test]
    fn two_servers_claiming_the_same_tool_name_are_rejected() {
        let Some(script) = tiny_server() else {
            eprintln!("skipping: tiny MCP server script not present");
            return;
        };
        // Both sanitize to `a_b`, so both advertise `mcp__a_b__echo`.
        let mut specs = BTreeMap::new();
        for name in ["a.b", "a_b"] {
            specs.insert(
                name.to_string(),
                ServerSpec {
                    command: "python3".into(),
                    args: vec![script.clone()],
                    ..Default::default()
                },
            );
        }
        let mut m = Mcp::from_specs(Path::new("."), specs);
        rt(async {
            m.connect(0).await.expect("first handshake");
            let e = m.connect(1).await.unwrap_err().to_string();
            assert!(e.contains("collision"), "got: {e}");
            assert!(m.servers[1].last_error.is_some());
            // The surviving server still owns the name.
            assert_eq!(m.resolve("mcp__a_b__echo").unwrap().0, 0);
        });
    }

    #[test]
    fn plan_opt_in_is_required_before_a_tool_shows_up_in_plan_mode() {
        let specs = BTreeMap::from([
            (
                "reader".to_string(),
                ServerSpec {
                    command: "true".into(),
                    ..Default::default()
                },
            ),
            (
                "writer".to_string(),
                ServerSpec {
                    command: "true".into(),
                    plan: true,
                    ..Default::default()
                },
            ),
        ]);
        let mut m = Mcp::from_specs(&std::env::temp_dir(), specs);
        m.servers[0].tools = vec![McpTool {
            tool: "read".into(),
            qualified: "mcp__reader__read".into(),
            description: String::new(),
            schema: normalize_schema(serde_json::Value::Null),
        }];
        m.servers[1].tools = vec![McpTool {
            tool: "read".into(),
            qualified: "mcp__writer__read".into(),
            description: String::new(),
            schema: normalize_schema(serde_json::Value::Null),
        }];
        assert!(!m.server_allows_plan("mcp__reader__read"));
        assert!(m.server_allows_plan("mcp__writer__read"));
        // Unknown names must not be treated as allowed, or Plan mode would
        // hand a model a tool we cannot attribute to any server.
        assert!(!m.server_allows_plan("mcp__ghost__read"));
    }

    #[test]
    fn cancellation_stops_waiting_for_a_slow_tool() {
        let Some(script) = tiny_server() else {
            eprintln!("skipping: tiny MCP server script not present");
            return;
        };
        let mut specs = BTreeMap::new();
        specs.insert("tiny".to_string(), spec("python3"));
        specs.get_mut("tiny").unwrap().args = vec![script, "--slow-call".into()];
        let mut m = Mcp::from_specs(Path::new("."), specs);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        rt(async {
            m.connect(0).await.expect("handshake");
            let e = m
                .call_tool(0, "echo", r#"{"text":"hi"}"#, Some(&cancel))
                .await
                .unwrap_err();
            assert!(format!("{e:#}").contains("cancelled"), "{e:#}");
        });
    }

    #[test]
    fn a_server_that_never_answers_times_out_instead_of_hanging() {
        // The real failure mode of a bad stdio server is silence.
        let dir = std::env::temp_dir().join(format!("lc-mcp-silent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("silent.py");
        std::fs::write(&script, "import sys\nfor line in sys.stdin:\n    pass\n").unwrap();
        let mut specs = BTreeMap::new();
        specs.insert("silent".to_string(), spec("python3"));
        specs.get_mut("silent").unwrap().args = vec![script.display().to_string()];
        specs.get_mut("silent").unwrap().timeout_secs = 2;
        let mut m = Mcp::from_specs(&dir, specs);
        rt(async {
            let e = m.connect(0).await.unwrap_err();
            let msg = format!("{e:#}");
            assert!(
                msg.contains("silent") || msg.contains("did not answer"),
                "{msg}"
            );
            // A failed server is recorded, not silently dropped.
            assert!(m.servers[0].last_error.is_some());
            assert!(m.tool_defs().is_empty());
        });
        std::fs::remove_dir_all(&dir).ok();
    }
}
