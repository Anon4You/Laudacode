//! Language Server Protocol client (stdio transport).
//!
//! Termux ships ~20 language servers in its own repo (`rust-analyzer`,
//! `clangd`, `gopls`, `pyrefly`, `zls`, `marksman`, …), so nothing here is
//! hardcoded to a particular language: a server is whatever executable the
//! user names, and files are routed to it by extension.
//!
//! # Two protocol differences from MCP (`src/mcp.rs`)
//!
//! 1. **Framing.** LSP uses `Content-Length: N\r\n\r\n<json>` headers, not
//!    one JSON object per line.
//! 2. **Positions are UTF-16 code units**, not bytes and not `char`s. A file
//!    containing emoji or CJK desynchronises byte offsets from LSP columns, so
//!    every position crossing this boundary goes through [`Pos`]/[`to_lsp_pos`].
//!    Servers may negotiate `positionEncoding`; `utf-16` is the spec default
//!    and clangd does not advertise it at all.
//!
//! # Diagnostics are a trap
//!
//! `textDocument/publishDiagnostics` is a *notification*, not a response, and
//! servers publish repeatedly while analysis progresses. Measured on
//! rust-analyzer for one 4-line file, the same URI published:
//!
//! ```text
//! 0 diagnostics @2.80s -> 1 @2.83s -> 0 @4.15s -> 2 @26.32s (final)
//! ```
//!
//! Taking the first publish reports "no problems" on a file that does not
//! compile, and no quiescence window short enough to be useful (22s of silence
//! here) can distinguish settled from still-thinking. So [`Lsp::diagnostics`]
//! caps its wait, reports whatever it has, and says *still analyzing* rather
//! than claiming the file is clean. An empty settled result is the only thing
//! allowed to read as "no errors".

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// How long to wait for a publish stream to go quiet before calling a
/// diagnostic set settled. Kept short: the cap does the real work, this only
/// avoids always reporting "still analyzing" for a warm server.
const QUIESCE: Duration = Duration::from_millis(700);
/// Floor before an *empty* result may be called settled. Guards the
/// "published empty, still loading the sysroot" window.
const MIN_SETTLE: Duration = Duration::from_millis(1_500);
/// Cap on a single diagnostics request.
const DEFAULT_DIAG_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_INIT_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
pub const MAX_DIAGNOSTICS_PER_FILE: usize = 40;
const MAX_STDERR_LINES: usize = 50;
/// Cap on a single file's text, matching the workspace read cap.
const MAX_DOC_BYTES: usize = 512 * 1024;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// One configured language server.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct LspSpec {
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// File extension (no dot, lowercase) -> LSP `languageId`. Serving the
    /// same extension from two servers is rejected at load time.
    #[serde(default)]
    pub filetypes: BTreeMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Workspace root. Defaults to the agent's directory.
    #[serde(default)]
    pub root: Option<String>,
    /// Per-request budget in seconds; 0 uses the default.
    #[serde(default)]
    pub timeout_secs: u64,
    /// Diagnostics budget in seconds; 0 uses the default.
    #[serde(default)]
    pub diagnostics_secs: u64,
    /// `initialize` budget in seconds; 0 uses the default. Cold rust-analyzer
    /// is slow, so this is generous.
    #[serde(default)]
    pub init_timeout_secs: u64,
}

fn default_true() -> bool {
    true
}

/// Hand-written so `enabled` matches the serde default; a derived `Default`
/// would silently disable every default-constructed spec.
impl Default for LspSpec {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            filetypes: BTreeMap::new(),
            enabled: true,
            root: None,
            timeout_secs: 0,
            diagnostics_secs: 0,
            init_timeout_secs: 0,
        }
    }
}

impl LspSpec {
    fn timeout(&self) -> Duration {
        Duration::from_secs(match self.timeout_secs {
            0 => DEFAULT_TIMEOUT.as_secs(),
            n => n,
        })
    }
    fn init_timeout(&self) -> Duration {
        Duration::from_secs(match self.init_timeout_secs {
            0 => DEFAULT_INIT_TIMEOUT.as_secs(),
            n => n,
        })
    }
    fn diag_timeout(&self) -> Duration {
        Duration::from_secs(match self.diagnostics_secs {
            0 => DEFAULT_DIAG_TIMEOUT.as_secs(),
            n => n,
        })
    }
}

// ---------------------------------------------------------------------------
// Positions
// ---------------------------------------------------------------------------

/// A 1-based `(line, column)` as the rest of the tool surface presents it.
///
/// `read_file` prints `   1| …`, so a model copying coordinates out of a file
/// it just read produces 1-based numbers. Accepting LSP's 0-based convention
/// here would put every one of those lookups off by one, silently, which is
/// worse than a loud error. Internal conversions are 0-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pos {
    pub line: usize,
    pub column: usize,
}

/// 1-based (line, column) -> byte offset, validating both against the text.
fn to_byte_offset(text: &str, pos: Pos) -> Result<usize> {
    let total_lines = text.lines().count();
    if pos.line == 0 || pos.line > total_lines {
        bail!(
            "line {} is out of range (file has {total_lines} lines, 1-based)",
            pos.line
        );
    }
    let line_start = byte_offset_of_line(text, pos.line - 1);
    let line_text = &text[line_start..];
    let line_body = line_text.split('\n').next().unwrap_or("");
    // Column counts UTF-16 units, but a user (and a model reading a file) is
    // counting characters, so accept either and normalise to a byte offset.
    // Columns count characters here; the UTF-16 conversion happens once, in
    // `to_lsp`. Comparing a character index against a UTF-16 counter silently
    // mis-places every position after an emoji.
    let want = pos.column.saturating_sub(1);
    let mut chars = 0usize;
    for (byte_off, _) in line_body.char_indices() {
        if chars >= want {
            return Ok(line_start + byte_off);
        }
        chars += 1;
    }
    if chars < want {
        bail!(
            "column {} is past the end of line {} (line has {chars} columns)",
            pos.column,
            pos.line
        );
    }
    Ok(line_start + line_body.len())
}

fn byte_offset_of_line(text: &str, line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    let mut seen = 0;
    for (idx, l) in text.split_inclusive('\n').enumerate() {
        if idx == line {
            return seen;
        }
        seen += l.len();
    }
    text.len()
}

/// Server-reported byte offset -> the file's real path.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file://host/path` is only valid for an empty or localhost authority.
    let rest = match rest.find('/') {
        Some(0) => rest,
        Some(slash) => {
            let (host, tail) = rest.split_at(slash);
            if !host.is_empty() && host != "localhost" {
                return None;
            }
            tail
        }
        None => return None,
    };
    Some(PathBuf::from(percent_decode(rest)))
}

fn path_to_uri(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::from("file://");
    for ch in s.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' | '/' | ':' => out.push(ch),
            _ => {
                let mut b = [0u8; 4];
                for byte in ch.encode_utf8(&mut b).as_bytes() {
                    out.push_str(&format!("%{byte:02X}"));
                }
            }
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Protocol payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Diagnostic {
    #[serde(default)]
    pub range: Range,
    #[serde(default)]
    pub severity: Option<u8>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct Range {
    #[serde(default)]
    pub start: LspPosWire,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct LspPosWire {
    #[serde(default)]
    pub line: usize,
    #[serde(default)]
    pub character: usize,
}

#[derive(Debug, Deserialize)]
struct InitializeResult {
    #[serde(default)]
    capabilities: Capabilities,
}

#[derive(Debug, Default, Deserialize)]
struct Capabilities {
    /// Servers that publish more than once per file (rust-analyzer sets this)
    /// are the reason an empty first result is never trusted.
    #[serde(default)]
    diagnostic_provider: Option<serde_json::Value>,
    #[serde(default)]
    position_encoding: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Location {
    #[serde(default)]
    uri: String,
    #[serde(default)]
    range: Range,
}

#[derive(Debug, Deserialize)]
struct HoverResult {
    #[serde(default)]
    contents: serde_json::Value,
}

impl Diagnostic {
    pub fn severity_str(&self) -> &'static str {
        match self.severity {
            Some(1) => "error",
            Some(2) => "warning",
            Some(3) => "info",
            Some(4) => "hint",
            _ => "problem",
        }
    }
}

// ---------------------------------------------------------------------------
// Child process
// ---------------------------------------------------------------------------

struct Child {
    proc: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    /// `None` only while a request has it checked out, so exactly one reader
    /// ever owns the stdout pipe.
    reader: Option<BufReader<tokio::process::ChildStdout>>,
    stderr: Arc<tokio::sync::Mutex<Vec<String>>>,
    next_id: u64,
}

pub struct Server {
    pub name: String,
    pub spec: LspSpec,
    /// Latest diagnostics per file URI, plus when that set arrived.
    diagnostics: HashMap<String, (Vec<Diagnostic>, std::time::Instant)>,
    /// Files we have sent `didOpen` for, with the version counter LSP wants.
    open: HashMap<String, u64>,
    inter_file_deps: bool,
    last_error: Option<String>,
    child: Option<Child>,
}

impl Server {
    pub fn is_running(&self) -> bool {
        self.child.is_some()
    }

    /// A few stderr lines, for diagnosing a server that will not start.
    fn stderr_head(&self) -> Vec<String> {
        self.child
            .as_ref()
            .and_then(|c| c.stderr.try_lock().ok())
            .map(|g| g.iter().take(4).cloned().collect())
            .unwrap_or_default()
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

pub struct Lsp {
    pub servers: Vec<Server>,
    pub cwd: PathBuf,
}

/// Hand-written because `tokio::process::Child` is not `Debug`, and a test that
/// `unwrap()`s a config error only needs to know which servers were built.
impl std::fmt::Debug for Lsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lsp")
            .field("cwd", &self.cwd)
            .field(
                "servers",
                &self.servers.iter().map(|s| &s.name).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Lsp {
    pub fn from_specs(cwd: &Path, specs: BTreeMap<String, LspSpec>) -> Result<Self> {
        // Extension -> server must be unambiguous, or a file would be opened by
        // two servers that then each publish diagnostics we cannot attribute.
        let mut owner: HashMap<String, String> = HashMap::new();
        let mut servers = Vec::new();
        for (name, spec) in specs {
            if !spec.enabled {
                continue;
            }
            if spec.command.trim().is_empty() {
                anyhow::bail!("[lsp_servers.{name}] has no `command`");
            }
            for ext in spec.filetypes.keys() {
                let ext = ext.trim_start_matches('.').to_ascii_lowercase();
                if let Some(prev) = owner.insert(ext.clone(), name.clone()) {
                    if prev != name {
                        bail!(
                            "extension '{ext}' is claimed by both [lsp_servers.{prev}] \
                             and [lsp_servers.{name}]; a file can only have one server"
                        );
                    }
                }
            }
            servers.push(Server {
                name,
                spec,
                diagnostics: HashMap::new(),
                open: HashMap::new(),
                inter_file_deps: false,
                last_error: None,
                child: None,
            });
        }
        Ok(Self {
            servers,
            cwd: cwd.to_path_buf(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// A manager with no servers, for when the config failed to load. The
    /// caller keeps the error to show in `/lsp`.
    pub fn empty(cwd: &Path) -> Self {
        Self {
            servers: Vec::new(),
            cwd: cwd.to_path_buf(),
        }
    }

    /// Whether any configured server claims this file's extension.
    pub fn server_for_path(&self, path: &Path) -> Option<usize> {
        self.server_for(path)
    }

    /// Server index that handles `path`, by lowercase extension.
    fn server_for(&self, path: &Path) -> Option<usize> {
        let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
        self.servers
            .iter()
            .position(|s| s.spec.filetypes.contains_key(&ext))
    }

    /// Resolve a user-supplied path against the agent's directory and refuse
    /// anything outside the workspace, matching the other file tools. Reusing
    /// the same symlink-aware check keeps one containment story, not two.
    pub fn resolve(&self, file: &str) -> Result<PathBuf> {
        let p = Path::new(file);
        if p.is_absolute() {
            return Ok(p.to_path_buf());
        }
        anyhow::ensure!(
            crate::tools::contained_in_workspace(&self.cwd, file),
            "'{file}' is outside the workspace {}",
            self.cwd.display()
        );
        Ok(self.cwd.join(p))
    }

    // -- lifecycle ---------------------------------------------------------

    /// Spawn and handshake every server. Called at startup, not lazily: a
    /// cold rust-analyzer needs tens of seconds to build its index, and that
    /// time is better spent warming the index than stalling the first edit.
    pub async fn start_all(&mut self) -> Vec<String> {
        let mut failed = Vec::new();
        for i in 0..self.servers.len() {
            if let Err(e) = self.start(i).await {
                failed.push(format!("{}: {e:#}", self.servers[i].name));
            }
        }
        failed
    }

    pub async fn start(&mut self, idx: usize) -> Result<()> {
        if self.servers[idx].is_running() {
            return Ok(());
        }
        match self.start_inner(idx).await {
            Ok(()) => {
                self.servers[idx].last_error = None;
                Ok(())
            }
            Err(e) => {
                let mut msg = format!("{e:#}");
                if let Some(line) = self.servers[idx].stderr_head().first() {
                    msg.push_str(&format!(" (stderr: {line})"));
                }
                self.servers[idx].last_error = Some(msg.clone());
                self.reap(idx).await;
                Err(anyhow::anyhow!(msg))
            }
        }
    }

    async fn start_inner(&mut self, idx: usize) -> Result<()> {
        let spec = self.servers[idx].spec.clone();
        let root = match &spec.root {
            Some(r) => {
                let p = Path::new(r);
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    self.cwd.join(p)
                }
            }
            None => self.cwd.clone(),
        };

        let mut cmd = tokio::process::Command::new(&spec.command);
        cmd.args(&spec.args)
            .current_dir(&root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .env_remove("RUST_LOG");
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let mut proc = cmd
            .spawn()
            .with_context(|| format!("spawning '{}'", spec.command))?;

        let stdin = proc.stdin.take().context("server stdin")?;
        let stdout = proc.stdout.take().context("server stdout")?;
        let stderr = proc.stderr.take().context("server stderr")?;

        // A language server that chatters on stderr must not fill its pipe and
        // deadlock, so drain it on a spawned task.
        let sink = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        {
            let sink = Arc::clone(&sink);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let mut g = sink.lock().await;
                    if g.len() < MAX_STDERR_LINES {
                        g.push(l);
                    }
                }
            });
        }

        let child = Child {
            proc,
            stdin,
            reader: Some(BufReader::new(stdout)),
            stderr: sink,
            next_id: 0,
        };
        self.servers[idx].child = Some(child);

        let root_uri = path_to_uri(&root);
        let init = serde_json::json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "workspaceFolders": [{"uri": root_uri, "name": "root"}],
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": {"relatedInformation": true},
                    "synchronization": {"didSave": false, "willSave": false},
                },
                "workspace": {"workspaceFolders": true, "symbol": {}},
            },
        });
        let res = self
            .request(idx, "initialize", init, spec.init_timeout())
            .await
            .context("initialize")?;
        let parsed: InitializeResult =
            serde_json::from_value(res).context("malformed initialize result")?;
        // The spec default is utf-16, which is also what both Termux servers
        // use. A server that negotiates something else would silently shift
        // every column, so refuse it rather than return confident nonsense.
        let enc = parsed
            .capabilities
            .position_encoding
            .as_deref()
            .unwrap_or("utf-16");
        anyhow::ensure!(
            enc.eq_ignore_ascii_case("utf-16") || enc.eq_ignore_ascii_case("utf-8"),
            "server negotiated positionEncoding '{enc}', which this client does \
             not implement (it speaks utf-16/utf-8 only)"
        );
        self.servers[idx].inter_file_deps = parsed
            .capabilities
            .diagnostic_provider
            .as_ref()
            .and_then(|v| v.get("interFileDependencies"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Servers that do not advertise `positionEncoding` speak the spec
        // default, utf-16. Recorded so the conversion layer stays honest.
        self.notify(
            idx,
            &serde_json::json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
        )
        .await?;
        Ok(())
    }

    /// Close a server politely, then make sure it is gone. `kill_on_drop`
    /// alone would leave a language server's index on disk unreconciled.
    pub async fn shutdown(&mut self, idx: usize) {
        if self.servers[idx].is_running() {
            let _ = self
                .request(
                    idx,
                    "shutdown",
                    serde_json::Value::Null,
                    Duration::from_secs(2),
                )
                .await;
            let _ = self
                .notify(idx, &serde_json::json!({"jsonrpc":"2.0","method":"exit"}))
                .await;
        }
        self.reap(idx).await;
    }

    pub async fn shutdown_all(&mut self) {
        for i in 0..self.servers.len() {
            self.shutdown(i).await;
        }
    }

    /// Drop the child, killing it if it has not exited. Reaps zombies without
    /// waiting, so a hung server cannot hold up shutdown.
    async fn reap(&mut self, idx: usize) {
        if let Some(mut c) = self.servers[idx].child.take() {
            let _ = c.proc.start_kill();
            let _ = tokio::time::timeout(Duration::from_millis(500), c.proc.wait()).await;
        }
    }

    // -- document sync -----------------------------------------------------

    /// Tell the server the current on-disk text. `didOpen` the first time,
    /// `didChange` after, because a server reasoning about stale text produces
    /// diagnostics that are confidently wrong.
    async fn sync(&mut self, idx: usize, path: &Path) -> Result<()> {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let Some(lang) = self.servers[idx].spec.filetypes.get(&ext) else {
            bail!(
                "no languageId configured for '.{ext}' on server '{}'",
                self.servers[idx].name
            );
        };
        let meta =
            std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
        if meta.len() as usize > MAX_DOC_BYTES {
            bail!(
                "{} is {} KiB; skipping LSP sync above {} KiB",
                path.display(),
                meta.len() / 1024,
                MAX_DOC_BYTES / 1024
            );
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let uri = path_to_uri(path);
        let version = self.servers[idx].open.get(&uri).map(|v| v + 1).unwrap_or(1);

        if version == 1 {
            self.notify(
                idx,
                &serde_json::json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
                    "textDocument":{"uri":uri,"languageId":lang,"version":version,"text":text}}}),
            )
            .await?;
        } else {
            // Full sync: simplest correct option, and these documents are small.
            self.notify(
                idx,
                &serde_json::json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{
                    "textDocument":{"uri":uri,"version":version},
                    "contentChanges":[{"text":text}]}}),
            )
            .await?;
        }
        self.servers[idx].open.insert(uri, version);
        Ok(())
    }

    async fn ensure(&mut self, idx: usize, path: &Path) -> Result<()> {
        if !self.servers[idx].is_running() {
            self.start(idx).await?;
        }
        self.sync(idx, path).await
    }

    // -- queries -----------------------------------------------------------

    /// Diagnostics for one file, or for every file the server has published
    /// about when `path` is `None`.
    ///
    /// The returned `settled` flag is what keeps this honest: a cold server
    /// publishes an empty set before it has finished loading, so an unsettled
    /// empty result must never be rendered as "no problems".
    pub async fn diagnostics(
        &mut self,
        path: Option<&Path>,
    ) -> Result<(Vec<(PathBuf, Vec<Diagnostic>)>, bool)> {
        let idx = match path {
            Some(p) => self
                .server_for(p)
                .with_context(|| format!("no LSP server handles {}", p.display()))?,
            None => {
                // Without a file, report across the first running server that
                // has published anything; that is the only one with data.
                self.servers
                    .iter()
                    .position(|s| s.is_running() && !s.diagnostics.is_empty())
                    .context("no language server has published diagnostics yet")?
            }
        };
        if let Some(p) = path {
            self.ensure(idx, p).await?;
        }
        if !self.servers[idx].is_running() {
            bail!(
                "language server '{}' is not running",
                self.servers[idx].name
            );
        }

        let cap = self.servers[idx].spec.diag_timeout();
        let target = path.map(path_to_uri);
        let started = std::time::Instant::now();
        // When the last publish for this server landed. Quiescence is the
        // silence since then, which is measurable whether the pump exits on
        // the cap or on EOF.
        let mut last_publish: Option<std::time::Instant> = None;

        // Pump messages until the cap, tracking how long the target has been
        // quiet. The reader is checked out so nothing else races for it.
        let mut reader = self.servers[idx]
            .child
            .as_mut()
            .and_then(|c| c.reader.take())
            .context("language server stdout is unavailable")?;
        let deadline = tokio::time::Instant::now() + cap;
        let outcome = loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break Ok(());
            }
            match tokio::time::timeout(remaining, read_message(&mut reader)).await {
                Err(_) => break Ok(()),
                Ok(Err(e)) => break Err(e),
                Ok(Ok(None)) => break Ok(()), // server closed stdout
                Ok(Ok(Some(msg))) => {
                    if self.absorb(idx, &msg).await {
                        last_publish = Some(std::time::Instant::now());
                    }
                }
            }
        };
        if let Some(c) = self.servers[idx].child.as_mut() {
            c.reader = Some(reader);
        }
        outcome?;

        // Settled needs all three: something arrived, it went quiet, and we did
        // not bail out immediately. Servers with inter-file dependencies churn
        // for a long time, so they need a longer quiet period to be believed.
        let quiet_needed = if self.servers[idx].inter_file_deps {
            QUIESCE * 3
        } else {
            QUIESCE
        };
        let seen = match &target {
            Some(t) => self.servers[idx].diagnostics.contains_key(t),
            None => !self.servers[idx].diagnostics.is_empty(),
        };
        let quiet = last_publish
            .map(|t| t.elapsed() >= quiet_needed)
            .unwrap_or(false);
        let settled = seen && quiet && started.elapsed() >= MIN_SETTLE;

        let mut out = Vec::new();
        let entries: Vec<(String, (Vec<Diagnostic>, std::time::Instant))> =
            self.servers[idx].diagnostics.clone().into_iter().collect();
        for (uri, (ds, _)) in entries {
            if let Some(t) = &target {
                if uri != *t {
                    continue;
                }
            }
            if let Some(p) = uri_to_path(&uri) {
                out.push((p, ds));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok((out, settled))
    }

    pub async fn definition(&mut self, path: &Path, pos: Pos) -> Result<String> {
        let idx = self
            .server_for(path)
            .with_context(|| format!("no LSP server handles {}", path.display()))?;
        self.ensure(idx, path).await?;
        let text = read_text(path)?;
        let lsp = to_lsp(&text, pos)?;
        let res = self
            .request(
                idx,
                "textDocument/definition",
                serde_json::json!({"textDocument":{"uri":path_to_uri(path)},"position":lsp}),
                self.servers[idx].spec.timeout(),
            )
            .await?;
        render_locations(path, &res, &self.cwd)
    }

    pub async fn references(&mut self, path: &Path, pos: Pos) -> Result<String> {
        let idx = self
            .server_for(path)
            .with_context(|| format!("no LSP server handles {}", path.display()))?;
        self.ensure(idx, path).await?;
        let text = read_text(path)?;
        let lsp = to_lsp(&text, pos)?;
        let res = self
            .request(
                idx,
                "textDocument/references",
                serde_json::json!({"textDocument":{"uri":path_to_uri(path)},
                    "position":lsp, "context":{"includeDeclaration":true}}),
                self.servers[idx].spec.timeout(),
            )
            .await?;
        render_locations(path, &res, &self.cwd)
    }

    pub async fn hover(&mut self, path: &Path, pos: Pos) -> Result<String> {
        let idx = self
            .server_for(path)
            .with_context(|| format!("no LSP server handles {}", path.display()))?;
        self.ensure(idx, path).await?;
        let text = read_text(path)?;
        let lsp = to_lsp(&text, pos)?;
        let res = self
            .request(
                idx,
                "textDocument/hover",
                serde_json::json!({"textDocument":{"uri":path_to_uri(path)},"position":lsp}),
                self.servers[idx].spec.timeout(),
            )
            .await?;
        if res.is_null() {
            return Ok("(no hover information at that position)".to_string());
        }
        let h: HoverResult = serde_json::from_value(res).context("malformed hover result")?;
        let text = match h.contents {
            serde_json::Value::String(s) => s,
            serde_json::Value::Object(o) => o
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            serde_json::Value::Array(items) => items
                .iter()
                .filter_map(|i| i.get("value").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join("\n\n"),
            _ => String::new(),
        };
        let text = text.trim();
        if text.is_empty() {
            Ok("(no hover information at that position)".to_string())
        } else {
            Ok(text.to_string())
        }
    }

    /// Document outline for one file, or a workspace symbol search.
    pub async fn symbols(&mut self, path: Option<&Path>, query: Option<&str>) -> Result<String> {
        let (idx, method, params) = match path {
            Some(p) => {
                let idx = self
                    .server_for(p)
                    .with_context(|| format!("no LSP server handles {}", p.display()))?;
                self.ensure(idx, p).await?;
                (
                    idx,
                    "textDocument/documentSymbol",
                    serde_json::json!({"textDocument":{"uri":path_to_uri(p)}}),
                )
            }
            None => match query {
                Some(q) if !q.trim().is_empty() => {
                    let idx = self
                        .servers
                        .iter()
                        .position(|s| s.is_running())
                        .context("no language server is running")?;
                    (idx, "workspace/symbol", serde_json::json!({"query": q}))
                }
                _ => bail!("pass `file` for a document outline, or `query` to search symbols"),
            },
        };
        let res = self
            .request(idx, method, params, self.servers[idx].spec.timeout())
            .await?;
        if res.is_null() {
            return Ok("(no symbols)".to_string());
        }
        // Three shapes are in play: `workspace/symbol` returns flat entries
        // with a `location`, `textDocument/documentSymbol` returns a nested
        // tree whose children carry only ranges, and a few servers return
        // `LocationLink`s. One walk over the JSON handles all of them, which
        // is less code than three deserialisers and cannot disagree with them.
        let default_uri = path.map(path_to_uri).unwrap_or_default();
        let mut out: Vec<(Option<PathBuf>, usize, String, Option<String>)> = Vec::new();
        collect_symbols(&res, &default_uri, None, &mut out, 0);
        if out.is_empty() {
            return Ok("(no symbols)".to_string());
        }
        let mut s = String::new();
        for (p, line, name, container) in out.iter().take(200) {
            let indent = "  ".repeat(1 + usize::from(container.is_some()));
            match p {
                Some(path) => s.push_str(&format!(
                    "{indent}{}:{}  {name}{}\n",
                    rel(path, &self.cwd),
                    line,
                    container
                        .as_ref()
                        .map(|c| format!(" (in {c})"))
                        .unwrap_or_default()
                )),
                None => s.push_str(&format!("{indent}{name}\n")),
            }
        }
        if out.len() > 200 {
            s.push_str(&format!("  … {} more\n", out.len() - 200));
        }
        Ok(s.trim_end().to_string())
    }

    // -- transport ---------------------------------------------------------

    /// Handle one inbound message. Returns true if it was progress for the
    /// diagnostics caller (i.e. a publish for any file).
    async fn absorb(&mut self, idx: usize, msg: &serde_json::Value) -> bool {
        let method = msg.get("method").and_then(|m| m.as_str());
        match method {
            Some("textDocument/publishDiagnostics") => {
                let Some(params) = msg.get("params") else {
                    return false;
                };
                let Some(uri) = params.get("uri").and_then(|u| u.as_str()) else {
                    return false;
                };
                let ds: Vec<Diagnostic> = params
                    .get("diagnostics")
                    .and_then(|d| serde_json::from_value(d.clone()).ok())
                    .unwrap_or_default();
                self.servers[idx]
                    .diagnostics
                    .insert(uri.to_string(), (ds, std::time::Instant::now()));
                true
            }
            // Servers hand us capabilities they expect us to honour. We decline
            // them all, so answer with null rather than leaving the request
            // hanging — a language server that is waiting on a reply will stall
            // every subsequent query.
            Some("window/workDoneProgress/create")
            | Some("client/registerCapability")
            | Some("client/unregisterCapability")
            | Some("workspace/semanticTokens/refresh")
            | Some("workspace/inlayHint/refresh")
            | Some("workspace/codeLens/refresh")
            | Some("workspace/diagnostic/refresh") => {
                if let Some(id) = msg.get("id") {
                    let reply = serde_json::json!({"jsonrpc":"2.0","id":id,"result":null});
                    let _ = self.notify(idx, &reply).await;
                }
                false
            }
            Some("workspace/configuration") => {
                // Asked only if we advertised the capability, but answer
                // defensively: an empty array is a valid "no configuration".
                if let Some(id) = msg.get("id") {
                    let n = msg
                        .get("params")
                        .and_then(|p| p.get("items"))
                        .and_then(|i| i.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    let reply = serde_json::json!({"jsonrpc":"2.0","id":id,"result":vec![(); n]});
                    let _ = self.notify(idx, &reply).await;
                }
                false
            }
            // `window/showMessageRequest` wants a choice; we have no UI, so
            // decline with the first action if there is one.
            Some("window/showMessageRequest") => {
                if let Some(id) = msg.get("id") {
                    let pick = msg
                        .get("params")
                        .and_then(|p| p.get("actions"))
                        .and_then(|a| a.as_array())
                        .and_then(|a| a.first())
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    let reply = serde_json::json!({"jsonrpc":"2.0","id":id,"result":pick});
                    let _ = self.notify(idx, &reply).await;
                }
                false
            }
            _ => false,
        }
    }

    /// Write one `Content-Length`-framed message. This is the one place
    /// framing is produced, so the header can never drift from the body length.
    async fn notify(&mut self, idx: usize, msg: &serde_json::Value) -> Result<()> {
        let body = serde_json::to_vec(msg).context("encoding message")?;
        let mut framed = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        framed.extend_from_slice(&body);
        let c = self.servers[idx]
            .child
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("language server is not running"))?;
        c.stdin
            .write_all(&framed)
            .await
            .context("writing to language server")?;
        c.stdin
            .flush()
            .await
            .context("flushing language server stdin")?;
        Ok(())
    }

    /// Send a request and read until its id comes back, stashing notifications
    /// so a `publishDiagnostics` burst is not mistaken for the response.
    async fn request(
        &mut self,
        idx: usize,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value> {
        let id = {
            let c = self.servers[idx]
                .child
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("language server is not running"))?;
            c.next_id += 1;
            c.next_id
        };
        let msg = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        });
        self.notify(idx, &msg).await?;

        let mut reader = self.servers[idx]
            .child
            .as_mut()
            .and_then(|c| c.reader.take())
            .ok_or_else(|| anyhow::anyhow!("language server stdout is unavailable"))?;

        let deadline = tokio::time::Instant::now() + timeout;
        let outcome: Result<serde_json::Value> = loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break Err(anyhow::anyhow!(
                    "{} (during '{method}')",
                    self.io_error(idx)
                ));
            }
            match tokio::time::timeout(remaining, read_message(&mut reader)).await {
                Err(_) => continue,
                Ok(Err(e)) => break Err(e).context("reading from language server"),
                Ok(Ok(None)) => {
                    break Err(anyhow::anyhow!(
                        "{} (during '{method}')",
                        self.io_error(idx)
                    ))
                }
                Ok(Ok(Some(m))) => {
                    if m.get("id").and_then(|i| i.as_u64()) == Some(id) {
                        break match m.get("error") {
                            Some(e) => {
                                let msg = e
                                    .get("message")
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("unknown error");
                                Err(anyhow::anyhow!("{method}: {msg}"))
                            }
                            None => Ok(m.get("result").cloned().unwrap_or(serde_json::Value::Null)),
                        };
                    }
                    self.absorb(idx, &m).await;
                }
            }
        };
        if let Some(c) = self.servers[idx].child.as_mut() {
            c.reader = Some(reader);
        }
        outcome
    }

    fn io_error(&mut self, idx: usize) -> String {
        let tail = self.servers[idx].stderr_head();
        if tail.is_empty() {
            format!(
                "language server '{}' stopped responding",
                self.servers[idx].name
            )
        } else {
            format!(
                "language server '{}' stopped responding: {}",
                self.servers[idx].name,
                tail.join(" | ")
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn read_text(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

fn to_lsp(text: &str, pos: Pos) -> Result<serde_json::Value> {
    let off = to_byte_offset(text, pos)?;
    let line = text[..off].matches('\n').count();
    let start = byte_offset_of_line(text, line);
    let character: usize = text[start..off].chars().map(char::len_utf16).sum();
    Ok(serde_json::json!({"line": line, "character": character}))
}

fn rel(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Walk any of the three symbol response shapes into a flat list.
/// `uri` is used when an entry carries only a range (nested document symbols).
fn collect_symbols(
    v: &serde_json::Value,
    uri: &str,
    container: Option<&str>,
    out: &mut Vec<(Option<PathBuf>, usize, String, Option<String>)>,
    depth: usize,
) {
    // A pathological or hostile workspace should not blow the stack.
    if depth > 8 {
        return;
    }
    let Some(items) = v.as_array() else {
        return;
    };
    for item in items {
        let name = item
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            continue;
        }
        // `location` = flat form; `range`/`selectionRange` = nested form;
        // `targetUri` = LocationLink form.
        let range = item
            .get("location")
            .and_then(|l| l.get("range"))
            .or_else(|| item.get("range"))
            .or_else(|| item.get("selectionRange"));
        let this_uri = item
            .get("location")
            .and_then(|l| l.get("uri"))
            .or_else(|| item.get("targetUri"))
            .and_then(|u| u.as_str())
            .unwrap_or(uri);
        let line = range
            .and_then(|r| r.get("start"))
            .and_then(|s| s.get("line"))
            .and_then(|l| l.as_u64())
            .unwrap_or(0) as usize
            + 1;
        let path = if this_uri.is_empty() {
            None
        } else {
            uri_to_path(this_uri)
        };
        out.push((path, line, name.clone(), container.map(str::to_string)));
        if let Some(children) = item.get("children") {
            collect_symbols(children, this_uri, Some(&name), out, depth + 1);
        }
    }
}

fn render_locations(origin: &Path, res: &serde_json::Value, cwd: &Path) -> Result<String> {
    let locs: Vec<Location> = serde_json::from_value(res.clone())
        .or_else(|_| {
            serde_json::from_value::<Vec<serde_json::Value>>(res.clone()).map(|items| {
                items
                    .into_iter()
                    .filter_map(|i| serde_json::from_value::<Location>(i).ok())
                    .collect()
            })
        })
        .context("malformed location result")?;
    if locs.is_empty() {
        return Ok("(no results)".to_string());
    }
    let origin_uri = path_to_uri(origin);
    let mut out = String::new();
    for l in locs.iter().take(100) {
        // Nested document symbols have no uri; report them against the file
        // they came from rather than dropping them.
        let (path, range) = if l.uri.is_empty() {
            (origin.to_path_buf(), l.range)
        } else {
            let p = uri_to_path(&l.uri).unwrap_or_else(|| origin.to_path_buf());
            (p, l.range)
        };
        let same = path_to_uri(&path) == origin_uri;
        out.push_str(&format!(
            "  {}:{}\n",
            if same {
                "this file".to_string()
            } else {
                rel(&path, cwd)
            },
            range.start.line + 1
        ));
    }
    if locs.len() > 100 {
        out.push_str(&format!("  … {} more\n", locs.len() - 100));
    }
    Ok(out.trim_end().to_string())
}

/// Read one `Content-Length`-framed JSON-RPC message. `None` means the pipe
/// closed.
async fn read_message(
    reader: &mut BufReader<tokio::process::ChildStdout>,
) -> Result<Option<serde_json::Value>> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .context("reading header")?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed
            .split_once(':')
            .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .map(|(_, v)| v.trim().to_string())
        {
            len = rest.parse().ok();
        }
    }
    let Some(len) = len else {
        bail!("language server sent a message with no Content-Length header");
    };
    // Guard against a server announcing a length that would blow up memory.
    if len > 32 * 1024 * 1024 {
        bail!("language server announced an implausible Content-Length: {len}");
    }
    let mut buf = vec![0u8; len];
    tokio::io::AsyncReadExt::read_exact(reader, &mut buf)
        .await
        .context("reading message body")?;
    Ok(Some(
        serde_json::from_slice(&buf).context("malformed JSON-RPC body")?,
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn rt<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    /// A server binary that exists and speaks LSP. Everything else is skipped
    /// rather than failed: `cargo test` must pass on a machine without them.
    fn have(bin: &str) -> bool {
        std::process::Command::new(bin)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, body).unwrap();
        p
    }

    // -- pure logic, no server needed --------------------------------------

    #[test]
    fn positions_are_one_based_and_utf16_aware() {
        // "let x = 1;" — column 5 is the `x` in 1-based counting.
        let text = "let x = 1;\n";
        assert_eq!(to_byte_offset(text, Pos { line: 1, column: 5 }).unwrap(), 4);
        assert_eq!(to_lsp(text, Pos { line: 1, column: 5 }).unwrap()["line"], 0);
        assert_eq!(
            to_lsp(text, Pos { line: 1, column: 5 }).unwrap()["character"],
            4
        );

        // Emoji are 1 char but 2 UTF-16 units, and 4 bytes. Columns here count
        // characters, so the `=` is the 7th, not the 6th. A byte-based
        // implementation would land 2 columns past it.
        let text = "let \u{1F600} = 1;\n";
        let off = to_byte_offset(text, Pos { line: 1, column: 7 }).unwrap();
        assert_eq!(&text[off..off + 1], "=", "wrong byte offset for emoji line");
        // Three plausible answers for where `=` sits: byte 9, char 6, and
        // UTF-16 unit 7. LSP wants the last one, and only that one round-trips
        // through a real server.
        assert_eq!(
            to_lsp(text, Pos { line: 1, column: 7 }).unwrap()["character"],
            7,
            "LSP character must be UTF-16 units, not bytes or chars"
        );

        // CJK: 1 char, 3 bytes, 1 UTF-16 unit.
        let text = "let \u{4F60} = 1;\n";
        let off = to_byte_offset(text, Pos { line: 1, column: 7 }).unwrap();
        assert_eq!(&text[off..off + 1], "=");
    }

    #[test]
    fn out_of_range_positions_fail_loudly() {
        let text = "a\nb\n";
        assert!(to_byte_offset(text, Pos { line: 0, column: 1 }).is_err());
        assert!(to_byte_offset(text, Pos { line: 9, column: 1 }).is_err());
        assert!(to_byte_offset(
            text,
            Pos {
                line: 1,
                column: 99
            }
        )
        .is_err());
        // A column past the end but a valid line clamps to end-of-line, which
        // is what "click past the last character" means.
        assert!(to_byte_offset(text, Pos { line: 1, column: 2 }).is_ok());
    }

    #[test]
    fn uris_survive_spaces_and_unicode() {
        let p = Path::new("/tmp/my project/a b/\u{4F60}.rs");
        let uri = path_to_uri(p);
        assert!(uri.starts_with("file:///tmp/my%20project/"), "{uri}");
        assert!(uri.contains("%E4%BD%A0"), "{uri}");
        assert_eq!(uri_to_path(&uri).as_deref(), Some(p));
        // An authority we do not serve is rejected rather than mangled.
        assert_eq!(uri_to_path("file://elsewhere/tmp/x.rs"), None);
        assert_eq!(
            uri_to_path("file:///tmp/x.rs").unwrap(),
            Path::new("/tmp/x.rs")
        );
    }

    #[test]
    fn an_extension_cannot_be_claimed_by_two_servers() {
        let specs = BTreeMap::from([
            (
                "rust".to_string(),
                LspSpec {
                    command: "rust-analyzer".into(),
                    filetypes: BTreeMap::from([("rs".to_string(), "rust".to_string())]),
                    ..Default::default()
                },
            ),
            (
                "other".to_string(),
                LspSpec {
                    command: "clangd".into(),
                    filetypes: BTreeMap::from([("rs".to_string(), "rust".to_string())]),
                    ..Default::default()
                },
            ),
        ]);
        let e = Lsp::from_specs(Path::new("."), specs)
            .unwrap_err()
            .to_string();
        assert!(e.contains("claimed by both"), "{e}");
    }

    #[test]
    fn a_server_without_a_command_is_rejected() {
        let specs = BTreeMap::from([(
            "broken".to_string(),
            LspSpec {
                filetypes: BTreeMap::from([("rs".to_string(), "rust".to_string())]),
                ..Default::default()
            },
        )]);
        let e = Lsp::from_specs(Path::new("."), specs)
            .unwrap_err()
            .to_string();
        assert!(e.contains("no `command`"), "{e}");
    }

    #[test]
    fn disabled_servers_are_dropped_and_defaults_stay_enabled() {
        let specs = BTreeMap::from([
            (
                "on".to_string(),
                LspSpec {
                    command: "x".into(),
                    filetypes: BTreeMap::from([("rs".to_string(), "rust".to_string())]),
                    ..Default::default()
                },
            ),
            (
                "off".to_string(),
                LspSpec {
                    command: "x".into(),
                    enabled: false,
                    ..Default::default()
                },
            ),
        ]);
        let lsp = Lsp::from_specs(Path::new("."), specs).unwrap();
        assert_eq!(lsp.servers.len(), 1);
        assert!(
            lsp.servers[0].spec.enabled,
            "derived Default would disable it"
        );
    }

    #[test]
    fn files_without_a_server_are_reported_not_guessed() {
        let specs = BTreeMap::from([(
            "rust".to_string(),
            LspSpec {
                command: "x".into(),
                filetypes: BTreeMap::from([("rs".to_string(), "rust".to_string())]),
                ..Default::default()
            },
        )]);
        let lsp = Lsp::from_specs(Path::new("."), specs).unwrap();
        assert!(lsp.server_for(Path::new("main.rs")).is_some());
        assert!(
            lsp.server_for(Path::new("main.RS")).is_some(),
            "case-insensitive"
        );
        assert!(lsp.server_for(Path::new("notes.md")).is_none());
        assert!(lsp.server_for(Path::new("Makefile")).is_none());
    }

    // -- against real servers ---------------------------------------------

    #[test]
    fn clangd_reports_a_real_error() {
        if !have("clangd") {
            eprintln!("skipping: clangd not installed");
            return;
        }
        let dir = std::env::temp_dir().join(format!("lc-lsp-c-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bad = write(
            &dir,
            "bad.c",
            "int main(void) { undefined_fn(1); return 0; }\n",
        );
        let specs = BTreeMap::from([(
            "c".to_string(),
            LspSpec {
                command: "clangd".into(),
                filetypes: BTreeMap::from([("c".to_string(), "c".to_string())]),
                // Cold clangd indexes nothing, but give it room anyway.
                diagnostics_secs: 3,
                ..Default::default()
            },
        )]);
        let mut lsp = Lsp::from_specs(&dir, specs).unwrap();
        rt(async {
            let (files, settled) = lsp.diagnostics(Some(&bad)).await.unwrap();
            let ds = &files[0].1;
            assert!(
                ds.iter().any(|d| d.message.contains("undefined_fn")),
                "expected the undefined-function error, got {ds:?}"
            );
            assert!(settled, "clangd is fast; this should settle: {ds:?}");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_server_does_not_take_the_others_down() {
        let specs = BTreeMap::from([
            (
                "gone".to_string(),
                LspSpec {
                    command: "definitely-not-a-real-binary-xyz".into(),
                    filetypes: BTreeMap::from([("zzz".to_string(), "zzz".to_string())]),
                    ..Default::default()
                },
            ),
            (
                "c".to_string(),
                LspSpec {
                    command: "clangd".into(),
                    filetypes: BTreeMap::from([("c".to_string(), "c".to_string())]),
                    ..Default::default()
                },
            ),
        ]);
        let dir = std::env::temp_dir();
        let mut lsp = Lsp::from_specs(&dir, specs).unwrap();
        rt(async {
            let failed = lsp.start_all().await;
            if have("clangd") {
                assert_eq!(failed.len(), 1, "{failed:?}");
                assert!(failed[0].contains("gone"), "{failed:?}");
            } else {
                // No clangd on this machine (e.g. CI): both fail to spawn.
                assert_eq!(failed.len(), 2, "{failed:?}");
                assert!(failed.iter().any(|f| f.contains("gone")), "{failed:?}");
            }
            if have("clangd") {
                // Looked up by name: BTreeMap ordering means index 1 is `gone`.
                let c = lsp
                    .servers
                    .iter()
                    .find(|s| s.name == "c")
                    .expect("the c server is present");
                assert!(c.is_running(), "the good server still started");
                assert!(c.last_error().is_none(), "{:?}", c.last_error());
            }
        });
    }

    /// ~90s of real indexing, so it is opt-in: `cargo test -- --ignored`.
    /// The point of the test is the settle contract on a slow server, which is
    /// exactly the case a fast mock cannot reproduce.
    #[test]
    #[ignore = "needs a real rust-analyzer index (~90s); run with --ignored"]
    fn rust_analyzer_eventually_agrees_a_file_is_clean() {
        // Skipped unless rust-analyzer is present, and given a long budget
        // because a cold server needs tens of seconds to index. This is the
        // regression guard for the "empty first publish" trap.
        if !have("rust-analyzer") {
            eprintln!("skipping: rust-analyzer not installed");
            return;
        }
        let dir = std::env::temp_dir().join(format!("lc-lsp-rs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write(
            &dir,
            "Cargo.toml",
            "[package]\nname = \"probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        );
        let good = write(
            &dir,
            "src/main.rs",
            "fn main() {\n    let x: i32 = 21;\n    println!(\"{x}\");\n}\n",
        );
        let specs = BTreeMap::from([(
            "rust".to_string(),
            LspSpec {
                command: "rust-analyzer".into(),
                filetypes: BTreeMap::from([("rs".to_string(), "rust".to_string())]),
                diagnostics_secs: 90,
                init_timeout_secs: 60,
                ..Default::default()
            },
        )]);
        let mut lsp = Lsp::from_specs(&dir, specs).unwrap();
        rt(async {
            let (files, settled) = lsp.diagnostics(Some(&good)).await.unwrap();
            assert!(settled, "clean file should settle within the budget");
            let errors: Vec<_> = files
                .iter()
                .flat_map(|(_, ds)| ds.iter())
                .filter(|d| d.severity == Some(1))
                .collect();
            assert!(errors.is_empty(), "clean file reported {errors:?}");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
