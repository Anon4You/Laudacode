use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};

use crate::api::{FunctionDef, ToolDef};

const MAX_TOOL_OUTPUT: usize = 8 * 1024;
const MAX_READ_BYTES: u64 = 200 * 1024;

/// Actions the model can request. Parsed from tool-call arguments.
#[derive(Debug, Clone)]
pub enum Action {
    ListDir { path: String },
    ReadFile { path: String },
    WriteFile { path: String, content: String },
    EditFile { path: String, old: String, new: String },
    RunCommand { command: String },
    FetchUrl { url: String },
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
struct CmdArgs {
    command: String,
}

#[derive(Deserialize)]
struct FetchArgs {
    url: String,
}

pub fn parse_tool_action(name: &str, arguments: &str) -> Result<Action> {
    fn inner(name: &str, v: serde_json::Value) -> Result<Action> {
        match name {
            "list_dir" => {
                let a: ListDirArgs = serde_json::from_value(v)?;
                Ok(Action::ListDir { path: a.path.unwrap_or_else(|| ".".into()) })
            }
            "read_file" => {
                let a: ReadArgs = serde_json::from_value(v)?;
                Ok(Action::ReadFile { path: a.path })
            }
            "write_file" => {
                let a: WriteArgs = serde_json::from_value(v)?;
                Ok(Action::WriteFile { path: a.path, content: a.content })
            }
            "edit_file" => {
                let a: EditArgs = serde_json::from_value(v)?;
                Ok(Action::EditFile { path: a.path, old: a.old_string, new: a.new_string })
            }
            "run_command" => {
                let a: CmdArgs = serde_json::from_value(v)?;
                Ok(Action::RunCommand { command: a.command })
            }
            "fetch_url" => {
                let a: FetchArgs = serde_json::from_value(v)?;
                Ok(Action::FetchUrl { url: a.url })
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
            Action::ReadFile { path } => format!("read {path}"),
            Action::WriteFile { path, content } => {
                format!("write {} ({} bytes)", path, content.len())
            }
            Action::EditFile { path, old, new } => {
                format!("edit {} (-{} +{} lines)", path, old.lines().count(), new.lines().count())
            }
            Action::RunCommand { command } => format!("$ {command}"),
            Action::FetchUrl { url } => format!("fetch {url}"),
        }
    }

    pub fn danger(&self) -> Danger {
        match self {
            Action::ListDir { .. } | Action::ReadFile { .. } | Action::FetchUrl { .. } => Danger::Safe,
            Action::WriteFile { .. } | Action::EditFile { .. } => Danger::Moderate,
            Action::RunCommand { command } => {
                if is_dangerous_command(command) {
                    Danger::High
                } else {
                    Danger::Moderate
                }
            }
        }
    }

    /// Execute the action inside `cwd`, returning output for the model.
    pub async fn perform(&self, cwd: &Path) -> Result<String> {
        match self {
            Action::ListDir { path } => {
                let root = resolve_in(cwd, path)?;
                let tree = build_tree(&root, 0, 2);
                Ok(tree)
            }
            Action::ReadFile { path } => {
                let p = resolve_in(cwd, path)?;
                let meta = std::fs::metadata(&p)
                    .with_context(|| format!("stat {}", p.display()))?;
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
                let mut text = String::from_utf8_lossy(&raw).to_string();
                if text.chars().count() > MAX_TOOL_OUTPUT {
                    text = text.chars().take(MAX_TOOL_OUTPUT).collect();
                    text.push_str("\n[truncated]");
                }
                Ok(text)
            }
            Action::WriteFile { path, content } => {
                let p = resolve_in(cwd, path)?;
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                std::fs::write(&p, content.as_bytes())
                    .with_context(|| format!("writing {}", p.display()))?;
                Ok(format!("wrote {} ({} bytes)", p.display(), content.len()))
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
                Ok(format!("edited {} (-{} +{} lines)",
                    p.display(), old.lines().count(), new.lines().count()))
            }
            Action::RunCommand { command } => run_shell(command, cwd).await,
            Action::FetchUrl { url } => fetch_url(url).await,
        }
    }
}

fn resolve_in(cwd: &Path, path: &str) -> Result<PathBuf> {
    let p = Path::new(path);
    let resolved = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
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

pub async fn run_shell(command: &str, cwd: &Path) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawning shell (sh)")?;

    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();
    let timeout = tokio::time::Duration::from_secs(180);
    let res = tokio::time::timeout(timeout, async {
        if let Some(mut o) = child.stdout.take() {
            let _ = o.read_to_end(&mut out_buf).await;
        }
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_end(&mut err_buf).await;
        }
        child.wait().await
    })
    .await;

    let status = match res {
        Ok(s) => s.context("waiting for command")?,
        Err(_) => {
            let _ = child.kill().await;
            bail!("command timed out after {}s", timeout.as_secs())
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

async fn fetch_url(url: &str) -> Result<String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        bail!("only http(s) URLs are supported");
    }
    let client = reqwest::Client::builder()
        .user_agent(concat!("laudacode/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(20))
        .timeout(std::time::Duration::from_secs(45))
        .build()?;
    let resp = client.get(url).send().await.context("request failed")?;
    let status = resp.status();
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    let body = resp.text().await.context("reading response body")?;

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
                | "section" | "article" | "pre" => {
                    if skip_tag == 0 {
                        out.push('\n');
                    }
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
                            u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32)
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
                    i += 2;
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
    "sudo ", "sudo\t", "mkfs", "dd if=", ":(){", "fork()", "shutdown", "reboot",
    "halt", "init 0", "chmod -R 777 /", "chown -R", "> /dev/sd", "/dev/mem",
    "| sh", "| bash", "|zsh", "| zsh", "|sh", "| bash -", "curl |", "wget |",
    "git push --force", "git push -f", "history -c", "kill -9 1", "killall -9",
];

pub fn is_dangerous_command(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    for pat in DANGEROUS_PATTERNS {
        if lower.contains(pat) {
            return true;
        }
    }
    // rm -rf targeting root/home/system-ish paths.
    if lower.contains("rm ") && (lower.contains("-rf") || lower.contains("-fr") || lower.contains("-r -f")) {
        let targets_root = [" /", "~/", "$home", "/*", "/etc", "/usr", "/var", "/system", "/data"];
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
    ".git", "node_modules", "target", "dist", "build", "__pycache__",
    ".venv", "venv", "vendor", ".cache", ".gradle", ".idea", ".next",
    "Cargo.lock", "package-lock.json", "yarn.lock",
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
                name: "list_dir",
                description: "List files and folders under a directory (recursive, depth-limited).",
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
                name: "read_file",
                description: "Read the contents of a file.",
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path, relative to cwd"}
                    },
                    "required": ["path"]
                }),
            },
        },
        ToolDef {
            r#type: "function",
            function: FunctionDef {
                name: "write_file",
                description: "Create a file or overwrite it completely with new content.",
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
                name: "edit_file",
                description: "Replace an exact unique substring in a file. old_string must match exactly once.",
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
                name: "run_command",
                description: "Execute a shell command (sh -c) in the working directory and return its output.",
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
                name: "fetch_url",
                description: "Fetch content from an http(s) URL. HTML pages are converted to plain text. Use for docs, API references, error lookups.",
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "Absolute URL starting with http:// or https://"}
                    },
                    "required": ["url"]
                }),
            },
        },
    ]
}
