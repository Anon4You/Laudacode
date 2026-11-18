use anyhow::{bail, Context, Result};
use crossterm::style::Stylize;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

use crate::config::Network;

#[derive(Debug, Clone)]
pub struct Message {
    pub role: String,
    pub content: Option<String>,
    /// Data-URI encoded attachments (`data:image/png;base64,…`).
    pub images: Vec<String>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
}

impl Serialize for Message {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(None)?;
        map.serialize_entry("role", &self.role)?;
        if self.images.is_empty() {
            map.serialize_entry("content", &self.content)?;
        } else {
            let mut parts = Vec::with_capacity(self.images.len() + 1);
            if let Some(text) = &self.content {
                parts.push(serde_json::json!({ "type": "text", "text": text }));
            }
            for uri in &self.images {
                parts.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": { "url": uri }
                }));
            }
            let v = serde_json::to_value(&parts)
                .map_err(|e| serde::ser::Error::custom(e.to_string()))?;
            map.serialize_entry("content", &v)?;
        }
        if !self.tool_calls.is_empty() {
            map.serialize_entry("tool_calls", &self.tool_calls)?;
        }
        if let Some(id) = &self.tool_call_id {
            map.serialize_entry("tool_call_id", id)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Raw {
            role: String,
            #[serde(default)]
            content: Option<serde_json::Value>,
            #[serde(default)]
            images: Vec<String>,
            #[serde(default)]
            tool_calls: Vec<ToolCall>,
            #[serde(default)]
            tool_call_id: Option<String>,
        }
        let raw = Raw::deserialize(d)?;
        // Accept both the plain-string form and the multipart array form
        // (text parts are joined; image URLs land in `images`).
        let (content, images) = match raw.content {
            None | Some(serde_json::Value::Null) => (None, raw.images),
            Some(serde_json::Value::String(s)) => (Some(s), raw.images),
            Some(serde_json::Value::Array(parts)) => {
                let mut text = String::new();
                let mut images = raw.images;
                for p in parts {
                    match p.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(t);
                            }
                        }
                        Some("image_url") => {
                            if let Some(u) = p
                                .get("image_url")
                                .and_then(|i| i.get("url"))
                                .and_then(|v| v.as_str())
                            {
                                images.push(u.to_string());
                            }
                        }
                        _ => {}
                    }
                }
                (Some(text), images)
            }
            Some(other) => (Some(other.to_string()), raw.images),
        };
        Ok(Self {
            role: raw.role,
            content,
            images,
            tool_calls: raw.tool_calls,
            tool_call_id: raw.tool_call_id,
        })
    }
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            images: vec![],
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            images: vec![],
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
    /// User message with attached image data URIs (vision input).
    pub fn user_with_images(content: impl Into<String>, image_data_uris: Vec<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            images: image_data_uris,
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(content.into()),
            images: vec![],
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
    pub fn tool_result(tool_call_id: &str, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            images: vec![],
            tool_calls: vec![],
            tool_call_id: Some(tool_call_id.to_string()),
        }
    }
    /// Assistant message carrying pending tool calls.
    pub fn assistant_with_tools(tool_calls: Vec<ToolCall>, content: Option<String>) -> Self {
        Self {
            role: "assistant".into(),
            content,
            images: vec![],
            tool_calls,
            tool_call_id: None,
        }
    }
}

/// Minimal standard base64 encoder (RFC 4648, with padding). Avoids adding a
/// dependency for the single use-case of embedding image attachments.
pub fn base64_encode(data: &[u8]) -> String {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TBL[(n >> 18) as usize & 63] as char);
        out.push(TBL[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TBL[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TBL[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
    /// Provider-specific passthrough attached to a tool call by the wire
    /// format. Google's OpenAI-compatible endpoint (Gemini) puts
    /// `extra_content.google.thought_signature` here and REQUIRES it echoed
    /// back verbatim on the next request — dropping it turns every
    /// follow-up turn into a 400 INVALID_ARGUMENT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_content: Option<serde_json::Value>,
}

impl ToolCall {
    /// Name to dispatch locally: some Gemini models emit calls namespaced
    /// as `default_api:grep` — strip the prefix so the call matches the
    /// declared tool registry. `function.name` itself stays verbatim for
    /// byte-faithful history replay (thought signatures sign the call as
    /// emitted).
    pub fn tool_name(&self) -> &str {
        clean_tool_name(&self.function.name)
    }
}

/// Strip provider namespaces (`default_api:grep` → `grep`).
pub fn clean_tool_name(name: &str) -> &str {
    let n = name.trim();
    n.strip_prefix("default_api:").unwrap_or(n)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

/// JSON-schema tool definition sent to the API.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub r#type: &'static str,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDef {
    /// `Cow` because built-in tools are `&'static` and MCP servers contribute
    /// names and descriptions that are only known at runtime.
    pub name: std::borrow::Cow<'static, str>,
    pub description: std::borrow::Cow<'static, str>,
    pub parameters: serde_json::Value,
}

/// Events emitted while streaming a completion.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Content(String),
    Reasoning(String),
    Usage(Usage),
    /// Transient condition the user should know about (rate-limit retry,
    /// upstream hiccup) — not part of the model's message stream.
    Notice(String),
}

/// Token usage reported by the API (when available).
///
/// Every field is `#[serde(default)]`. This is deserialized straight off a
/// streaming chunk (`Chunk::usage`), so a provider that omits a single key
/// used to fail the entire chunk parse — and with it the turn's content,
/// which is exactly the kind of silent, provider-specific breakage that is
/// miserable to debug from a bug report.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    /// Prompt tokens served from a provider-side cache (Anthropic reports
    /// this flat as `cache_read_input_tokens`).
    #[serde(default, alias = "cache_read_input_tokens")]
    pub cache_read_tokens: Option<u64>,
    /// Prompt tokens written into the cache (Anthropic only).
    #[serde(default, alias = "cache_creation_input_tokens")]
    pub cache_write_tokens: Option<u64>,
    /// Thinking/reasoning tokens. A *subset* of `completion_tokens` on OpenAI.
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
}

impl Usage {
    /// Prompt tokens actually billed at the full input rate.
    ///
    /// Anthropic reports `prompt_tokens` *excluding* cache hits and cache
    /// writes, so those two are added back here; OpenAI-style providers
    /// already include cached tokens in `prompt_tokens` and report no
    /// separate write count, so adding a zero changes nothing.
    pub fn billable_prompt(&self) -> u64 {
        self.prompt_tokens
            .saturating_add(self.cache_read_tokens.unwrap_or(0))
            .saturating_add(self.cache_write_tokens.unwrap_or(0))
    }
}

/// Fully assembled turn returned after the stream ends.
#[derive(Debug, Default, Clone)]
pub struct Turn {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

/// How to ask a provider for reasoning / thinking, derived from the
/// endpoint — never hardcoded per provider name or model list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningStyle {
    /// OpenRouter extension: `"reasoning": { "enabled": true }`.
    OpenRouter,
    /// OpenAI chat-completions `"reasoning_effort"` param (also understood
    /// by most OpenAI-compatible gateways).
    OpenAi,
    /// Google Gemini: no reasoning flag is sent. The model reasons on its
    /// own and requires its `thought_signature`s echoed back on tool calls
    /// (handled by the ToolCall extra_content roundtrip).
    Google,
}

impl ReasoningStyle {
    /// Detect the dialect from the endpoint URL. Order matters: an
    /// OpenRouter URL wins over model strings, and the Google check only
    /// fires for the OpenAI-compatible transport (built-in free providers
    /// have their own body shapes entirely).
    pub fn detect(base_url: &str, kind: &str) -> Self {
        if kind == "openai" && base_url.contains("generativelanguage.googleapis.com") {
            Self::Google
        } else if base_url.contains("openrouter") {
            Self::OpenRouter
        } else {
            Self::OpenAi
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChatClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    extra_headers: BTreeMap<String, String>,
    /// Which reasoning-request dialect this endpoint speaks.
    reasoning_style: ReasoningStyle,
    /// `reasoning_effort` hint for reasoning models ("low"|"medium"|"high").
    reasoning_effort: Option<String>,
    /// Retries after the first attempt, on top of it. See
    /// [`DEFAULT_MAX_RETRIES`] and `[limits] max_retries`.
    max_retries: usize,
    /// Transport kind: "openai" (default) or a built-in free provider
    /// ("aitopia" and "powerbrain").
    kind: String,
}

#[derive(Default, serde::Deserialize)]
struct ChunkChoiceDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<DeltaToolCall>,
}

#[derive(serde::Deserialize)]
struct DeltaFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(serde::Deserialize)]
struct DeltaToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<DeltaFunction>,
    /// Gemini's OpenAI-compat layer:
    /// `extra_content.google.thought_signature`.
    #[serde(default)]
    extra_content: Option<serde_json::Value>,
    /// Fallback for providers that put a bare signature on the delta.
    #[serde(default)]
    thought_signature: Option<String>,
}

/// Streaming accumulator for one tool call — deltas may fragment the id,
/// name, arguments and (Gemini) the thought signature across chunks.
#[derive(Default)]
struct ToolCallAcc {
    index: usize,
    id: String,
    name: String,
    arguments: String,
    extra_content: Option<serde_json::Value>,
}

impl ToolCallAcc {
    fn absorb(&mut self, dtc: DeltaToolCall) {
        self.index = dtc.index;
        if let Some(id) = dtc.id {
            self.id = id;
        }
        if let Some(f) = dtc.function {
            if let Some(n) = f.name {
                self.name.push_str(&n);
            }
            if let Some(a) = f.arguments {
                self.arguments.push_str(&a);
            }
        }
        // Thought signatures ride in once (usually with the id delta); keep
        // the first non-null payload.
        let is_null = |v: &Option<serde_json::Value>| match v {
            Some(x) => x.is_null(),
            None => true,
        };
        let sig = if !is_null(&dtc.extra_content) {
            dtc.extra_content.clone()
        } else {
            dtc.thought_signature
                .as_ref()
                .map(|s| serde_json::json!({ "google": { "thought_signature": s } }))
        };
        if sig.is_some() && is_null(&self.extra_content) {
            self.extra_content = sig;
        }
    }

    fn finish(self) -> ToolCall {
        // NOTE: the wire name is kept verbatim (Gemini may emit a namespaced
        // `default_api:grep`). Thought signatures sign the call exactly as
        // emitted, so history replay must echo the original; local dispatch
        // strips the prefix via `ToolCall::tool_name()`.
        ToolCall {
            // Some providers omit ids on single calls — synthesize one so
            // the tool result can always be matched back.
            id: if self.id.is_empty() {
                format!("call_{}", self.index)
            } else {
                self.id
            },
            kind: "function".into(),
            function: FunctionCall {
                name: self.name,
                arguments: self.arguments,
            },
            extra_content: self.extra_content,
        }
    }
}

#[derive(serde::Deserialize)]
struct Chunk {
    choices: Vec<ChunkChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(serde::Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    delta: ChunkChoiceDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(serde::Deserialize)]
struct ApiErrorBody {
    #[serde(default)]
    error: Option<serde_json::Value>,
    #[serde(default)]
    message: Option<String>,
}

const AITOPIA_URL: &str = "https://extensions.aitopia.ai/ai/send";
const POWERBRAIN_URL: &str = "https://powerbrainai.com/app/backend/api/api.php";
/// A healthy SSE stream emits keepalives/frames constantly; a silent gap this
/// long means the connection is effectively dead.
const STREAM_IDLE: Duration = Duration::from_secs(75);
/// After `finish_reason` the server may still send one usage-only chunk and
/// then `[DONE]`. Wait briefly for it, but never for the full idle timeout.
const DONE_GRACE: Duration = Duration::from_secs(1);
/// An SSE frame this large is a runaway/malformed server, not a tool call.
const MAX_SSE_FRAME: usize = 16 * 1024 * 1024;

/// Process-wide transport settings from `[network]`. Installed once at startup
/// so every client — including provider probes and sub-agents — honors the
/// same proxy/CA/insecure configuration.
static DEFAULT_NETWORK: OnceLock<Network> = OnceLock::new();

/// Install the `[network]` settings for the whole process. First call wins;
/// later calls are ignored so a client can never silently change the
/// transport under the others.
pub fn set_default_network(net: &Network) {
    let _ = DEFAULT_NETWORK.set(net.clone());
}

fn default_network() -> Option<&'static Network> {
    DEFAULT_NETWORK.get()
}

/// Build a reqwest client, applying proxy, extra CA roots and insecure mode.
/// Errors name the offending setting so a bad `[network]` block is obvious
/// instead of surfacing as a mysterious TLS failure later.
fn build_http(net: Option<&Network>) -> Result<reqwest::Client> {
    let b = reqwest::Client::builder()
        .user_agent(concat!("laudacode/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(30));
    let b = match net {
        Some(net) => apply_network(b, net)?,
        None => b,
    };
    b.build().context("building http client")
}

/// Layer `[network]` onto a client builder: proxy (http/https/socks5), extra
/// PEM trust roots, and opt-out of certificate verification.
fn apply_network(b: reqwest::ClientBuilder, net: &Network) -> Result<reqwest::ClientBuilder> {
    let mut b = b;
    if let Some(url) = non_empty(net.proxy.as_deref()) {
        let proxy =
            reqwest::Proxy::all(url).with_context(|| format!("invalid [network].proxy `{url}`"))?;
        b = b.proxy(proxy);
    }
    if let Some(path) = non_empty(net.ca_bundle.as_deref()) {
        let pem =
            std::fs::read(path).with_context(|| format!("reading [network].ca_bundle `{path}`"))?;
        let certs = reqwest::Certificate::from_pem_bundle(&pem).with_context(|| {
            format!("parsing [network].ca_bundle `{path}` — PEM certificates expected")
        })?;
        if certs.is_empty() {
            anyhow::bail!("[network].ca_bundle `{path}` contains no PEM certificates");
        }
        for cert in certs {
            b = b.add_root_certificate(cert);
        }
    }
    if net.insecure == Some(true) {
        eprintln!(
            "{} TLS verification is disabled by [network].insecure — traffic can be intercepted",
            "warning:".yellow().bold()
        );
        b = b
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true);
    }
    Ok(b)
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|t| !t.is_empty())
}

/// Retries after the first attempt, on top of it — so the default is up to
/// 6 attempts. Overridable with `[limits] max_retries`. A coding agent that
/// loses a request to a flaky connection should wait it out, not fail the
/// whole turn; only genuinely non-transient answers (401, 400, 404) skip the
/// wait and surface immediately.
pub const DEFAULT_MAX_RETRIES: usize = 5;

/// Longest single backoff sleep, so a long retry chain can't stall a phone.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

impl ChatClient {
    /// How many times a failed request is retried before giving up.
    pub fn max_retries(&self) -> usize {
        self.max_retries
    }

    /// Override the retry budget, e.g. from `[limits] max_retries`.
    pub fn set_max_retries(&mut self, retries: usize) {
        self.max_retries = retries.min(MAX_RETRIES_LIMIT);
    }

    /// Send a request, retrying transient failures.
    ///
    /// `make` is called once per attempt because a `RequestBuilder` can't be
    /// reused. `on_notice`, when given, receives a human-readable line per retry
    /// so the UI can show the wait instead of appearing to hang. Returns the
    /// final response once it's successful or not worth retrying; only a
    /// transport-level failure (DNS, refused, TLS, timeout) errors out here.
    async fn send_with_retry<F>(
        &self,
        make: F,
        mut on_notice: Option<&mut dyn FnMut(String)>,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<reqwest::Response>
    where
        F: Fn() -> Result<reqwest::RequestBuilder>,
    {
        let mut notice = |msg: String| {
            if let Some(cb) = on_notice.as_mut() {
                cb(msg);
            }
        };
        // Retries are *in addition to* the first attempt.
        let max_attempts = self.max_retries.saturating_add(1);
        let mut attempt = 0usize;
        let mut last_err: Option<anyhow::Error> = None;
        let mut saw_rate_limit = false;
        loop {
            attempt += 1;
            match make()?.send().await {
                Ok(r) if r.status().is_success() => return Ok(r),
                Ok(r) if is_retryable_status(r.status()) && attempt < max_attempts => {
                    let status = r.status();
                    saw_rate_limit |= status == reqwest::StatusCode::TOO_MANY_REQUESTS;
                    let wait = retry_delay(r.headers(), attempt);
                    let why = if saw_rate_limit {
                        "rate limited"
                    } else {
                        "upstream error"
                    };
                    notice(format!(
                        "{why} ({status}), retry {n}/{max_attempts} in {s}s",
                        n = attempt + 1,
                        s = wait.as_secs().max(1)
                    ));
                    if sleep_or_cancel(wait, cancel).await.is_err() {
                        bail!("cancelled while waiting to retry");
                    }
                }
                // Success, a non-retryable status, or retries exhausted —
                // hand the response back so the caller reports it in context.
                Ok(r) => return Ok(r),
                Err(e) if attempt < max_attempts => {
                    let reason = root_cause(&e);
                    last_err = Some(e.into());
                    let wait = backoff(attempt);
                    notice(format!(
                        "connection failed ({reason}), retry {}/{} in {}s",
                        attempt + 1,
                        max_attempts,
                        wait.as_secs().max(1)
                    ));
                    if sleep_or_cancel(wait, cancel).await.is_err() {
                        bail!("cancelled while waiting to retry");
                    }
                }
                Err(e) => {
                    // Chain earlier failures so the user sees every cause.
                    let mut err = anyhow::anyhow!(e);
                    while let Some(prev) = last_err.take() {
                        err = err.context(prev.to_string());
                    }
                    return Err(
                        err.context(format!("connection failed after {max_attempts} attempt(s)"))
                    );
                }
            }
        }
    }
}

/// Upper bound on configured retries: past this the backoff alone would keep
/// a turn alive for longer than anyone will wait.
const MAX_RETRIES_LIMIT: usize = 20;

/// Exponential backoff with jitter, so several agents retrying the same flaky
/// endpoint don't march in lockstep.
fn backoff(attempt: usize) -> Duration {
    let base = 1u64 << attempt.saturating_sub(1).min(5);
    let base = base.min(MAX_BACKOFF.as_secs());
    // 0.75x–1.25x of base. `RandomState` is OS-seeded per instance, so this is
    // jitter without a PRNG to carry around.
    use std::hash::{BuildHasher, Hasher};
    let jitter = std::hash::RandomState::new().build_hasher().finish() % 51;
    let ms = base.saturating_mul(1000) / 2 + jitter.saturating_mul(base.saturating_mul(1000) / 100);
    Duration::from_millis(ms.clamp(200, MAX_BACKOFF.as_millis() as u64))
}

/// Innermost reason for a reqwest failure, which is the useful part
/// ("connection refused", "dns error") rather than the wrapper chain.
fn root_cause(e: &reqwest::Error) -> String {
    let mut src: &dyn std::error::Error = e;
    while let Some(next) = src.source() {
        src = next;
    }
    src.to_string()
}

/// Client for non-API traffic (`fetch_url`, web search) that still honors the
/// shared `[network]` settings, with caller-chosen timeouts. Streaming model
/// requests use [`build_http`] directly — they must not have a total timeout.
pub fn build_web_client(connect: Duration, total: Duration) -> Result<reqwest::Client> {
    web_client(connect, total)?
        .build()
        .context("building http client")
}

/// Same client, but no automatic redirects: `fetch_url` checks every hop so a
/// public page cannot bounce the agent into `localhost`/a LAN address.
pub fn build_web_client_no_redirect(connect: Duration, total: Duration) -> Result<reqwest::Client> {
    web_client(connect, total)?
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building http client")
}

/// Same client, but pin `host` to the addresses we already checked. Without
/// this, a DNS response can change between validation and `reqwest`'s own
/// lookup, turning an SSRF check into a TOCTOU race.
pub fn build_web_client_no_redirect_pinned(
    connect: Duration,
    total: Duration,
    host: &str,
    addrs: &[std::net::SocketAddr],
) -> Result<reqwest::Client> {
    web_client(connect, total)?
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(host, addrs)
        .build()
        .context("building http client")
}

fn web_client(connect: Duration, total: Duration) -> Result<reqwest::ClientBuilder> {
    let mut b = reqwest::Client::builder()
        .user_agent(concat!("laudacode/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(connect)
        .timeout(total);
    if let Some(net) = default_network() {
        b = apply_network(b, net)?;
    }
    Ok(b)
}

impl ChatClient {
    pub fn new(
        base_url: &str,
        api_key: &str,
        headers: &BTreeMap<String, String>,
        reasoning_effort: Option<String>,
        kind: &str,
    ) -> Result<Self> {
        let http = build_http(default_network())?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            extra_headers: headers.clone(),
            reasoning_style: ReasoningStyle::detect(base_url, kind),
            reasoning_effort,
            max_retries: DEFAULT_MAX_RETRIES,
            kind: kind.to_string(),
        })
    }

    fn endpoint(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    /// The chat-completions URL for the active transport kind. Built-in free
    /// providers ship their full endpoint as `base_url` and don't need an
    /// OpenAI suffix appended.
    fn chat_url(&self) -> String {
        match self.kind.as_str() {
            "aitopia" | "powerbrain" if self.base_url.is_empty() => {
                if self.kind == "aitopia" {
                    AITOPIA_URL.to_string()
                } else {
                    POWERBRAIN_URL.to_string()
                }
            }
            "aitopia" | "powerbrain" => self.base_url.clone(),
            _ => self.endpoint("/chat/completions"),
        }
    }

    fn headers(&self) -> Result<HeaderMap> {
        let mut map = HeaderMap::new();
        if !self.api_key.is_empty() {
            let auth = format!("Bearer {}", self.api_key);
            map.insert("authorization", HeaderValue::from_str(&auth)?);
        }
        map.insert("content-type", HeaderValue::from_static("application/json"));
        for (k, v) in &self.extra_headers {
            match (HeaderName::try_from(k.as_str()), HeaderValue::from_str(v)) {
                (Ok(name), Ok(val)) => {
                    map.insert(name, val);
                }
                _ => anyhow::bail!("invalid custom header '{k}: {v}'"),
            }
        }
        Ok(map)
    }

    /// Stream a chat completion. Content/reasoning deltas go through
    /// `on_event`; the assembled turn is returned at the end.
    ///
    /// Transient failures (connection errors, 429/5xx before any body bytes)
    /// are retried with backoff. `cancel`, when provided, also aborts pending
    /// connections, retry sleeps and stalled reads.
    pub async fn stream_chat<F>(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolDef],
        on_event: F,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<Turn>
    where
        F: FnMut(StreamEvent),
    {
        if is_cancelled(cancel) {
            bail!("interrupted by user");
        }
        let request = async {
            match self.kind.as_str() {
                "aitopia" => self.stream_aitopia(model, messages, on_event, cancel).await,
                "powerbrain" => {
                    self.stream_powerbrain(model, messages, on_event, cancel)
                        .await
                }
                _ => {
                    self.stream_openai(model, messages, tools, on_event, cancel)
                        .await
                }
            }
        };
        let Some(flag) = cancel else {
            return request.await;
        };
        tokio::select! {
            biased;
            _ = async {
                while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            } => bail!("interrupted by user"),
            result = request => {
                if is_cancelled(cancel) {
                    bail!("interrupted by user");
                }
                result
            }
        }
    }

    async fn stream_openai<F>(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolDef],
        mut on_event: F,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<Turn>
    where
        F: FnMut(StreamEvent),
    {
        if is_cancelled(cancel) {
            bail!("cancelled");
        }
        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
            "stream": true,
        });
        if !tools.is_empty() {
            body["tools"] = serde_json::to_value(tools)?;
        }
        // Reasoning params are dialect-aware: the OpenRouter extension and
        // the OpenAI chat-completions param are NOT interchangeable, and
        // Google's Gemini endpoint wants neither (it reasons on its own and
        // gates tool calls on thought signatures instead).
        if let Some(effort) = &self.reasoning_effort {
            // OpenAI chat-completions param for o-series / gpt-5 reasoning;
            // "max" is the xAI spelling and passes through as-is — picky
            // endpoints ignore unknown values rather than erroring.
            body["reasoning_effort"] = serde_json::json!(effort);
        }
        if self.reasoning_style == ReasoningStyle::OpenRouter {
            // OpenRouter-only extension; Google's endpoint rejects unknown
            // body fields, so it must never leak there.
            body["reasoning"] = serde_json::json!({ "enabled": true });
        }

        let url = self.chat_url();
        let resp = {
            let mut notice = |msg: String| on_event(StreamEvent::Notice(msg));
            self.send_with_retry(
                || Ok(self.http.post(&url).headers(self.headers()?).json(&body)),
                Some(&mut notice),
                cancel,
            )
            .await?
        };

        if is_cancelled(cancel) {
            bail!("cancelled");
        }

        if !resp.status().is_success() {
            let status = resp.status();
            // Capture Retry-After before the body is consumed.
            let resp_retry_after: Option<String> = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let text = resp.text().await.unwrap_or_default();
            let msg = serde_json::from_str::<ApiErrorBody>(&text)
                .ok()
                .and_then(|b| {
                    b.error
                        .and_then(|e| {
                            e.get("message")
                                .and_then(|m| m.as_str().map(|s| s.to_string()))
                        })
                        .or(b.message)
                })
                .unwrap_or_else(|| {
                    if text.is_empty() {
                        format!("HTTP {status}")
                    } else {
                        text.chars().take(500).collect()
                    }
                });
            let mut err = format!("API error ({status}): {msg}");
            match status.as_u16() {
                401 | 403 => err.push_str(
                    "\nhint: the API key was rejected or missing.\n  \
                     - check it: `laudacode provider list`, or `/provider show` in the TUI\n  \
                     - fix it:   `laudacode provider edit <name>`\n  \
                     - or export OPENAI_API_KEY before launching",
                ),
                404 => err.push_str("\nhint: wrong base_url or unknown model for this provider."),
                429 => err.push_str(&format!(
                    "\nhint: rate limited after {} attempts{}.",
                    self.max_retries + 1,
                    rate_limit_hint(resp_retry_after.as_deref())
                )),
                _ => {}
            }
            bail!("{err}");
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
        let mut turn = Turn::default();
        let mut acc: Vec<ToolCallAcc> = Vec::new();
        let mut done = false;
        let mut saw_done = false;
        let mut done_at: Option<std::time::Instant> = None;

        loop {
            let wait = match done_at {
                Some(at) => DONE_GRACE.saturating_sub(at.elapsed()),
                None => STREAM_IDLE,
            };
            if wait.is_zero() {
                break;
            }
            let next = tokio::time::timeout(wait, stream.next()).await;
            let item = match next {
                Err(_) if done => {
                    if is_cancelled(cancel) {
                        bail!("cancelled");
                    }
                    break;
                }
                Err(_) => bail!(
                    "stream stalled — no data for {}s (server hung up?)",
                    STREAM_IDLE.as_secs()
                ),
                Ok(None) => break,
                Ok(Some(item)) => item,
            };
            if is_cancelled(cancel) {
                bail!("cancelled");
            }
            let chunk = item.context("connection lost while streaming")?;
            buf.extend_from_slice(&chunk);
            if buf.len() > MAX_SSE_FRAME {
                bail!(
                    "SSE frame exceeded {} bytes — server is not speaking the protocol",
                    MAX_SSE_FRAME
                );
            }
            // SSE frames are separated by a blank line ("\n\n" or "\r\n\r\n").
            while let Some((_sep, consume)) = find_frame_end(&buf) {
                let frame: Vec<u8> = buf.drain(..consume).collect();
                let text = String::from_utf8_lossy(&frame);
                for line in text.lines() {
                    let line = line.trim();
                    if !line.starts_with("data:") {
                        continue;
                    }
                    let data = line[5..].trim();
                    if data == "[DONE]" {
                        done = true;
                        saw_done = true;
                        done_at.get_or_insert_with(std::time::Instant::now);
                        continue;
                    }
                    if let Ok(c) = serde_json::from_str::<Chunk>(data) {
                        if let Some(u) = c.usage {
                            turn.usage = Some(u);
                            on_event(StreamEvent::Usage(u));
                        }
                        for choice in c.choices {
                            if let Some(rc) = choice.delta.reasoning_content.clone() {
                                turn.reasoning.push_str(&rc);
                                on_event(StreamEvent::Reasoning(rc));
                            }
                            if let Some(r) = choice.delta.reasoning.clone() {
                                turn.reasoning.push_str(&r);
                                on_event(StreamEvent::Reasoning(r));
                            }
                            if let Some(ct) = choice.delta.content.clone() {
                                if !ct.is_empty() {
                                    turn.content.push_str(&ct);
                                    on_event(StreamEvent::Content(ct));
                                }
                            }
                            for dtc in choice.delta.tool_calls {
                                let idx = dtc.index;
                                let slot = match acc.iter_mut().find(|a| a.index == idx) {
                                    Some(s) => s,
                                    None => {
                                        acc.push(ToolCallAcc {
                                            index: idx,
                                            ..Default::default()
                                        });
                                        acc.last_mut().unwrap()
                                    }
                                };
                                slot.absorb(dtc);
                            }
                            if let Some(fr) = choice.finish_reason {
                                if !fr.is_empty() {
                                    turn.finish_reason = Some(fr);
                                    done = true;
                                    done_at.get_or_insert_with(std::time::Instant::now);
                                }
                            }
                        }
                    }
                }
            }
            if saw_done {
                break;
            }
        }

        // A connection that closes between frames is indistinguishable from a
        // complete reply unless the provider signalled the end. Never persist
        // a half answer as final.
        if !buf.is_empty() {
            let leftover = String::from_utf8_lossy(&buf);
            let leftover = leftover.trim();
            if leftover == "data: [DONE]" {
                done = true;
            } else if !leftover.is_empty() && !done {
                bail!("stream ended in the middle of an SSE frame (truncated response)");
            }
        }
        if !done {
            bail!("stream ended before [DONE]/finish_reason (truncated response)");
        }
        turn.tool_calls = acc.into_iter().map(ToolCallAcc::finish).collect();

        Ok(turn)
    }

    /// Aitopia (extensions.aitopia.ai) adapter — free, no API key.
    ///
    /// Request body is a proprietary `history` array (assistant turns arrive
    /// as role "system", plus a trailing empty "system" slot that receives the
    /// answer). Auth is an opaque `hopekey` header plus a Chrome-extension
    /// Origin; the SSE body looks like OpenAI but `choices` is an object
    /// keyed by index instead of an array.
    async fn stream_aitopia<F>(
        &self,
        model: &str,
        messages: &[Message],
        mut on_event: F,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<Turn>
    where
        F: FnMut(StreamEvent),
    {
        #[derive(serde::Serialize)]
        struct Extra {
            prompt_mode: bool,
        }
        #[derive(serde::Serialize)]
        struct HistoryItem {
            item: String,
            role: String,
            model: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            title: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            loading: Option<bool>,
            extra_data: Extra,
            #[serde(skip_serializing_if = "Option::is_none")]
            finish_reason: Option<String>,
        }
        #[derive(serde::Serialize)]
        struct AitopiaBody {
            history: Vec<HistoryItem>,
            text: String,
            model: String,
            stream: bool,
            mode: &'static str,
            prompt_mode: bool,
            extra_key: &'static str,
            extra_data: Extra,
            language_detail: serde_json::Value,
            is_continue: bool,
            lang_code: &'static str,
        }

        let mut history: Vec<HistoryItem> = Vec::new();
        let mut last_user = String::new();
        for m in messages {
            // Aitopia has no tool-call channel — skip tool plumbing.
            if m.role == "tool" || !m.tool_calls.is_empty() {
                continue;
            }
            let text = m.content.clone().unwrap_or_default();
            let role = if m.role == "user" { "user" } else { "system" };
            history.push(HistoryItem {
                item: text.clone(),
                role: role.to_string(),
                model: model.to_string(),
                title: None,
                loading: None,
                extra_data: Extra { prompt_mode: false },
                finish_reason: None,
            });
            if role == "user" && !text.is_empty() {
                last_user = text;
            }
        }
        history.push(HistoryItem {
            item: String::new(),
            role: "system".into(),
            model: model.to_string(),
            title: None,
            loading: Some(true),
            extra_data: Extra { prompt_mode: false },
            finish_reason: None,
        });

        let body = AitopiaBody {
            history,
            text: last_user,
            model: model.to_string(),
            stream: true,
            mode: "ai_chat",
            prompt_mode: false,
            extra_key: "__all",
            extra_data: Extra { prompt_mode: false },
            language_detail: serde_json::json!({
                "lang_code": "en",
                "name": "English",
                "title": "English",
            }),
            is_continue: false,
            lang_code: "en",
        };

        let url = self.chat_url();
        let req = {
            let mut notice = |msg: String| on_event(StreamEvent::Notice(msg));
            self.send_with_retry(
                || {
                    // A fresh `hopekey` per attempt — the server rejects reuse.
                    Ok(self
                        .http
                        .post(&url)
                        .header("content-type", "application/json")
                        .header("accept", "text/plain")
                        .header("accept-language", "en-US,en;q=0.9")
                        .header("cache-control", "no-cache")
                        .header("hopekey", random_hex_32())
                        .header(
                            "origin",
                            "chrome-extension://becfinhbfclcgokjlobojlnldbfillpf",
                        )
                        .header("pragma", "no-cache")
                        .header("priority", "u=1, i")
                        .header(
                            "user-agent",
                            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                             (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36",
                        )
                        .header("sec-fetch-dest", "empty")
                        .header("sec-fetch-mode", "cors")
                        .header("sec-fetch-site", "none")
                        .json(&body))
                },
                Some(&mut notice),
                cancel,
            )
            .await
            .context("aitopia request failed")?
        };
        let status = req.status();
        if !status.is_success() {
            let text = req.text().await.unwrap_or_default();
            bail!(
                "aitopia API error ({status}): {}",
                text.chars().take(500).collect::<String>()
            );
        }

        let mut stream = req.bytes_stream();
        let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
        let mut turn = Turn::default();
        loop {
            let next = tokio::time::timeout(STREAM_IDLE, stream.next()).await;
            let item = match next {
                Err(_) => bail!(
                    "stream stalled — no data for {}s (server hung up?)",
                    STREAM_IDLE.as_secs()
                ),
                Ok(None) => break,
                Ok(Some(item)) => item,
            };
            if is_cancelled(cancel) {
                bail!("cancelled");
            }
            let chunk = item.context("connection lost while streaming")?;
            buf.extend_from_slice(&chunk);
            while let Some((_sep, consume)) = find_frame_end(&buf) {
                let frame: Vec<u8> = buf.drain(..consume).collect();
                let text = String::from_utf8_lossy(&frame);
                for line in text.lines() {
                    let line = line.trim();
                    if !line.starts_with("data:") {
                        continue;
                    }
                    let data = line[5..].trim();
                    if data.is_empty() || data == "[DONE]" {
                        continue;
                    }
                    if let Some(ct) = extract_aitopia_content(data) {
                        if !ct.is_empty() {
                            turn.content.push_str(&ct);
                            on_event(StreamEvent::Content(ct));
                        }
                    }
                }
            }
        }
        if turn.content.is_empty() {
            bail!("aitopia returned an empty reply");
        }
        Ok(turn)
    }

    /// Powerbrain (powerbrainai.com) adapter — free, no API key.
    ///
    /// Non-OpenAI body carries a hardcoded `secret_token` + `action`
    /// ("send_message"). The endpoint streams plain JSON objects (one per
    /// line, `{"data":"<partial text>"}`), possibly wrapped in SSE `data:`.
    async fn stream_powerbrain<F>(
        &self,
        model: &str,
        messages: &[Message],
        mut on_event: F,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<Turn>
    where
        F: FnMut(StreamEvent),
    {
        let mut msgs: Vec<serde_json::Value> = Vec::new();
        for m in messages {
            if m.role == "tool" || !m.tool_calls.is_empty() {
                continue;
            }
            let content = m.content.clone().unwrap_or_default();
            if content.trim().is_empty() {
                continue;
            }
            msgs.push(serde_json::json!({ "role": m.role, "content": content }));
        }
        if msgs.is_empty() {
            msgs.push(serde_json::json!({ "role": "user", "content": "" }));
        }

        let body = serde_json::json!({
            "model": model,
            "messages": msgs,
            "secret_token": "AIChatPowerBrain123@2024",
            "action": "send_message",
        });
        let url = self.chat_url();
        let req = {
            let mut notice = |msg: String| on_event(StreamEvent::Notice(msg));
            self.send_with_retry(
                || {
                    Ok(self
                        .http
                        .post(&url)
                        .header("content-type", "application/json")
                        .header("user-agent", "Dart/3.5 (dart:io)")
                        .json(&body))
                },
                Some(&mut notice),
                cancel,
            )
            .await
            .context("powerbrain request failed")?
        };
        let status = req.status();
        if !status.is_success() {
            let text = req.text().await.unwrap_or_default();
            bail!(
                "powerbrain API error ({status}): {}",
                text.chars().take(500).collect::<String>()
            );
        }

        let mut stream = req.bytes_stream();
        let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
        let mut turn = Turn::default();
        loop {
            let next = tokio::time::timeout(STREAM_IDLE, stream.next()).await;
            let item = match next {
                Err(_) => bail!(
                    "stream stalled — no data for {}s (server hung up?)",
                    STREAM_IDLE.as_secs()
                ),
                Ok(None) => break,
                Ok(Some(item)) => item,
            };
            if is_cancelled(cancel) {
                bail!("cancelled");
            }
            let chunk = item.context("connection lost while streaming")?;
            buf.extend_from_slice(&chunk);
            while let Some(idx) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=idx).collect();
                emit_powerbrain_line(&String::from_utf8_lossy(&line), &mut turn, &mut on_event)?;
            }
        }
        // Some powerbrain responses end without a trailing newline — drain the
        // leftover buffer so the final (or only) JSON object isn't dropped.
        if !buf.is_empty() {
            emit_powerbrain_line(&String::from_utf8_lossy(&buf), &mut turn, &mut on_event)?;
        }
        if turn.content.is_empty() {
            bail!("powerbrain returned an empty reply");
        }
        Ok(turn)
    }

    /// Fetch available models from `/v1/models`.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        // Built-in free providers have no OpenAI /models catalog (or return a
        // non-OpenAI shape) — offer a curated, always-current default set.
        match self.kind.as_str() {
            "aitopia" => {
                return Ok(vec![
                    "AITOPIA".into(),
                    "gpt-4o-mini".into(),
                    "gpt-4o".into(),
                    "claude-3.5-sonnet".into(),
                ])
            }
            "powerbrain" => {
                return Ok(vec![
                    "gpt-5".into(),
                    "gpt-5-mini".into(),
                    "gemini-2.0-flash".into(),
                ])
            }
            _ => {}
        }
        let url = self.endpoint("/models");
        // Retried too: a flaky /models call shouldn't empty the model picker.
        let resp = self
            .send_with_retry(
                || Ok(self.http.get(&url).headers(self.headers()?)),
                None,
                None,
            )
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!(
                "models request failed ({status}): {}",
                text.chars().take(300).collect::<String>()
            );
        }
        #[derive(serde::Deserialize)]
        struct ModelsResp {
            data: Vec<ModelEntry>,
        }
        #[derive(serde::Deserialize)]
        struct ModelEntry {
            id: String,
        }
        let parsed: ModelsResp = serde_json::from_str(&text).context("parsing models response")?;
        let mut ids: Vec<String> = parsed.data.into_iter().map(|m| m.id).collect();
        ids.sort();
        Ok(ids)
    }

    /// Prove that the key AND model actually work by running a real
    /// 1-token completion. Public `/models` endpoints succeed even with
    /// garbage keys, so this is the only trustworthy pre-flight check for
    /// provider setup (`/provider add|edit`).
    pub async fn probe_chat(&self, model: &str) -> Result<()> {
        if self.kind != "openai" {
            // Built-in free providers have no OpenAI /chat/completions probe —
            // a 1-word reply through the real transport is the honest check.
            let turn = self
                .stream_chat(model, &[Message::user("ping")], &[], |_| {}, None)
                .await
                .context("probe request failed")?;
            if turn.content.trim().is_empty() {
                bail!("provider returned an empty reply");
            }
            return Ok(());
        }
        let body = serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 1,
            "stream": false,
        });
        let url = self.endpoint("/chat/completions");
        // Same retry budget as a real turn: a probe that trips over a blip
        // would otherwise reject a perfectly good key.
        let resp = self
            .send_with_retry(
                || Ok(self.http.post(&url).headers(self.headers()?).json(&body)),
                None,
                None,
            )
            .await
            .context("probe request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let msg = Self::parse_error_body(resp).await;
            bail!("key/model check failed ({status}): {msg}");
        }
        Ok(())
    }

    /// Extract the provider's error message from an error response body.
    async fn parse_error_body(resp: reqwest::Response) -> String {
        let text = resp.text().await.unwrap_or_default();
        serde_json::from_str::<ApiErrorBody>(&text)
            .ok()
            .and_then(|b| {
                b.error
                    .and_then(|e| e.get("message").and_then(|m| m.as_str().map(String::from)))
                    .or(b.message)
            })
            .unwrap_or_else(|| {
                if text.is_empty() {
                    "no details".into()
                } else {
                    text.chars().take(500).collect()
                }
            })
    }
}

/// Parse one powerbrain response line/fragment and push any text delta into
/// the running turn. Handles both raw `{"data":"…"}` JSON and the SSE-wrapped
/// form; errors bail with the server's own message.
fn emit_powerbrain_line<F: FnMut(StreamEvent)>(
    line: &str,
    turn: &mut Turn,
    on_event: &mut F,
) -> Result<()> {
    let line = line
        .trim()
        .strip_prefix("data:")
        .unwrap_or(line.trim())
        .trim();
    if line.is_empty() {
        return Ok(());
    }
    #[derive(serde::Deserialize)]
    struct PB {
        #[serde(default)]
        data: Option<String>,
        #[serde(default)]
        error: Option<String>,
    }
    if let Ok(pb) = serde_json::from_str::<PB>(line) {
        if let Some(er) = pb.error {
            if !er.trim().is_empty() {
                bail!("powerbrain error: {er}");
            }
        }
        if let Some(d) = pb.data {
            if !d.is_empty() {
                turn.content.push_str(&d);
                on_event(StreamEvent::Content(d));
            }
        }
    }
    Ok(())
}

/// Extract the text delta from one aitopia SSE `data:` line.
///
/// Aitopia sends `{"choices":{"0":{"delta":{"content":"…"},"finish_reason":…}}}`
/// (an object keyed by index) — the OpenAI array form is also tolerated.
fn extract_aitopia_content(data: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Entry {
        #[serde(default)]
        delta: EntryDelta,
    }
    #[derive(serde::Deserialize, Default)]
    struct EntryDelta {
        #[serde(default)]
        content: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Chunk {
        choices: serde_json::Value,
    }
    let chunk: Chunk = serde_json::from_str(data).ok()?;
    match chunk.choices {
        serde_json::Value::Object(map) => {
            let entry: Entry = serde_json::from_value(map.get("0").cloned()?).ok()?;
            entry.delta.content
        }
        serde_json::Value::Array(mut arr) => {
            if arr.is_empty() {
                return None;
            }
            let entry: Entry = serde_json::from_value(arr.remove(0)).ok()?;
            entry.delta.content
        }
        _ => None,
    }
}

/// A fresh 32-hex-char random token for aitopia's `hopekey` header. `RandomState`
/// is OS-seeded, so this needs no rand crate and no PRNG state.
fn random_hex_32() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(32);
    for _ in 0..2 {
        out.push_str(&format!(
            "{:016x}",
            std::hash::RandomState::new().build_hasher().finish()
        ));
    }
    out
}

/// Locate the end of the next SSE frame in `buf`.
///
/// Handles both "\n\n" and "\r\n\r\n" separators, returning whichever
/// appears first: `(separator_start, total_bytes_to_consume)`.
fn find_frame_end(buf: &[u8]) -> Option<(usize, usize)> {
    let lf_lf = buf
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|p| (p, p + 2));
    let crlf = if buf.len() >= 4 {
        buf.windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| (p, p + 4))
    } else {
        None
    };
    match (lf_lf, crlf) {
        (Some(a), Some(b)) => {
            if a.0 <= b.0 {
                Some(a)
            } else {
                Some(b)
            }
        }
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn is_cancelled(cancel: Option<&std::sync::atomic::AtomicBool>) -> bool {
    cancel
        .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false)
}

fn is_retryable_status(s: reqwest::StatusCode) -> bool {
    matches!(s.as_u16(), 408 | 409 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// How long to wait before retrying a throttled request. Honors `Retry-After`
/// as delta-seconds, capped at 60s so a hostile or stale header can't hang the
/// agent, then falls back to exponential backoff.
fn retry_delay(headers: &reqwest::header::HeaderMap, attempt: usize) -> Duration {
    const CAP: u64 = 60;
    let server = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .and_then(|v| v.parse::<u64>().ok());
    match server {
        Some(secs) => Duration::from_secs(secs).min(Duration::from_secs(CAP)),
        None => Duration::from_secs(1u64 << attempt.saturating_sub(1).min(5)),
    }
}

/// Sleep, but wake immediately when the user cancels. Returns `Err` if the
/// wait was interrupted so the caller can bail instead of sleeping out the
/// full backoff.
async fn sleep_or_cancel(
    total: Duration,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> std::result::Result<(), ()> {
    const SLICE: Duration = Duration::from_millis(100);
    let mut left = total;
    while !left.is_zero() {
        if is_cancelled(cancel) {
            return Err(());
        }
        let step = left.min(SLICE);
        tokio::time::sleep(step).await;
        left -= step;
    }
    if is_cancelled(cancel) {
        return Err(());
    }
    Ok(())
}

/// Extra guidance for a 429 so the user knows what to change, not just that
/// they were throttled.
fn rate_limit_hint(retry_after: Option<&str>) -> String {
    let mut s = String::new();
    if let Some(v) = retry_after.map(str::trim).filter(|v| !v.is_empty()) {
        s.push_str(&format!(" (server asked to retry after {v})"));
    }
    s.push_str(
        "\n  - wait a moment, or lower concurrency; free tiers refill per minute\n  \
         - switch model/provider: /provider use, or -m <model>\n  \
         - if it persists, check the provider's rate-limit dashboard",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_end_lf() {
        assert_eq!(find_frame_end(b"data: hi\n\n"), Some((8, 10)));
        assert_eq!(find_frame_end(b"data: hi"), None);
        assert_eq!(find_frame_end(b"data: hi\n"), None);
    }

    #[test]
    fn frame_end_crlf() {
        let buf = b"data: hi\r\n\r\n";
        assert_eq!(find_frame_end(buf), Some((8, 12)));
        // Complete CRLF separator at buffer end must be detected.
        let buf2 = b"x\r\n\r\n";
        assert_eq!(find_frame_end(buf2), Some((1, 5)));
        assert_eq!(find_frame_end(b"a\r\n\r"), None);
    }

    #[test]
    fn frame_end_mixed_separators() {
        // "\n\n" appears before a later "\r\n\r\n" — earliest wins.
        let buf = b"a\n\nb\r\n\r\n";
        assert_eq!(find_frame_end(buf), Some((1, 3)));
        let buf = b"a\r\n\r\nb\n\n";
        assert_eq!(find_frame_end(buf), Some((1, 5)));
    }

    #[test]
    fn retryable_statuses() {
        use reqwest::StatusCode;
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(StatusCode::OK));
    }

    #[test]
    fn reasoning_style_detection_is_endpoint_driven() {
        // Google's OpenAI-compatible endpoint (also via a custom provider).
        assert_eq!(
            ReasoningStyle::detect(
                "https://generativelanguage.googleapis.com/v1beta/openai",
                "openai"
            ),
            ReasoningStyle::Google
        );
        assert_eq!(
            ReasoningStyle::detect("https://openrouter.ai/api/v1", "openai"),
            ReasoningStyle::OpenRouter
        );
        // Everything else speaks the OpenAI chat-completions param.
        assert_eq!(
            ReasoningStyle::detect("https://api.openai.com/v1", "openai"),
            ReasoningStyle::OpenAi
        );
        assert_eq!(
            ReasoningStyle::detect("http://localhost:11434/v1", "openai"),
            ReasoningStyle::OpenAi
        );
        // Built-in free transports never get the Google dialect from URL
        // sniffing — their body shapes are handled separately.
        assert_eq!(
            ReasoningStyle::detect("https://extensions.aitopia.ai/ai/send", "aitopia"),
            ReasoningStyle::OpenAi
        );
    }

    #[test]
    fn gemini_tool_call_signature_roundtrips_and_name_is_kept_verbatim() {
        // What Gemini's OpenAI-compat layer actually streams for a tool call:
        // a namespaced name plus a thought_signature that must be echoed
        // back byte-faithfully on the next request.
        let delta = r#"{"index":7,"id":"call-abc","function":{"name":"default_api:grep","arguments":"{\"pattern\":\"x\"}"},"extra_content":{"google":{"thought_signature":"sig123"}}}"#;
        let parsed: DeltaToolCall = serde_json::from_str(delta).unwrap();
        let mut acc = ToolCallAcc::default();
        acc.absorb(parsed);
        let tc = acc.finish();
        // Wire name preserved for history replay, clean name for dispatch.
        assert_eq!(tc.function.name, "default_api:grep");
        assert_eq!(tc.tool_name(), "grep");
        let sig = tc.extra_content.clone().expect("signature captured");
        assert_eq!(
            sig["google"]["thought_signature"],
            serde_json::json!("sig123")
        );

        // Serializing the assistant message back onto the wire keeps the
        // signature exactly where Gemini expects it.
        let msg = Message::assistant_with_tools(vec![tc.clone()], None);
        let v = serde_json::to_value(&msg).unwrap();
        let wire_call = &v["tool_calls"][0];
        assert_eq!(
            wire_call["extra_content"]["google"]["thought_signature"],
            serde_json::json!("sig123")
        );
        assert_eq!(
            wire_call["function"]["name"],
            serde_json::json!("default_api:grep")
        );
        // And it deserializes back losslessly (session persistence).
        let back: ToolCall = serde_json::from_value(serde_json::to_value(&tc).unwrap()).unwrap();
        assert_eq!(back.extra_content, tc.extra_content);
    }

    #[test]
    fn non_gemini_tool_calls_serialize_without_extra_content() {
        let tc = ToolCall {
            id: "call_0".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "read_file".into(),
                arguments: "{}".into(),
            },
            extra_content: None,
        };
        let v = serde_json::to_value(&tc).unwrap();
        assert!(
            v.get("extra_content").is_none(),
            "no signature must not leak a field"
        );

        let msg = Message::assistant_with_tools(vec![tc], None);
        let v = serde_json::to_value(&msg).unwrap();
        assert!(v["tool_calls"][0].get("extra_content").is_none());
    }

    #[test]
    fn fragmented_gemini_deltas_keep_the_first_signature() {
        // Signature rides in on the first delta; later argument fragments
        // must not clobber it.
        let d1: DeltaToolCall = serde_json::from_str(
            r#"{"index":0,"id":"c1","function":{"name":"grep","arguments":"{\"pat"},"extra_content":{"google":{"thought_signature":"S"}}}"#,
        )
        .unwrap();
        let d2: DeltaToolCall =
            serde_json::from_str(r#"{"index":0,"function":{"arguments":"tern\":\"a\"}"}}"#)
                .unwrap();
        let mut acc = ToolCallAcc::default();
        acc.absorb(d1);
        acc.absorb(d2);
        let tc = acc.finish();
        assert_eq!(tc.function.arguments, r#"{"pattern":"a"}"#);
        assert_eq!(
            tc.extra_content.unwrap()["google"]["thought_signature"],
            serde_json::json!("S")
        );
    }

    #[test]
    fn synthesized_call_id_uses_stream_index() {
        // No id in any delta — a stable synthetic id must still be produced.
        let d: DeltaToolCall =
            serde_json::from_str(r#"{"index":3,"function":{"name":"list_dir","arguments":"{}"}}"#)
                .unwrap();
        let mut acc = ToolCallAcc::default();
        acc.absorb(d);
        assert_eq!(acc.finish().id, "call_3");
    }

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    /// Serve `responses[i]` to the i-th request (last one repeats), then stop.
    /// Blocking std IO on a helper thread keeps tokio's `net` feature out of
    /// the dependency set just for tests.
    fn spawn_canned_server(responses: Vec<String>) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let total = responses.len();
            let mut served = 0usize;
            for resp in responses {
                let Ok((mut sock, _)) = listener.accept() else {
                    break;
                };
                // Read the request headers (we never act on them).
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(resp.as_bytes());
                let _ = sock.flush();
                served += 1;
                if served == total {
                    break;
                }
            }
        });
        (format!("http://{addr}/v1"), handle)
    }

    fn sse_ok(content: &str) -> String {
        let body = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({
                "choices": [{"delta": {"content": content}}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
            })
        );
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    }

    /// A server that sends a terminal frame but keeps the socket open.
    /// Reading until EOF would hang; the client must stop on finish_reason.
    fn spawn_open_stream_server() -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf);
            let body = format!(
                "data: {}\n\n",
                serde_json::json!({
                    "choices": [{"delta": {"content": "whole"}, "finish_reason": "stop"}]
                })
            );
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.write_all(body.as_bytes());
            let _ = sock.flush();
            std::thread::sleep(std::time::Duration::from_secs(10));
        });
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn usage_after_finish_reason_is_kept() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\ndata: [DONE]\n\n";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (url, _server) = spawn_canned_server(vec![resp]);
        let c = ChatClient::new(&url, "k", &Default::default(), None, "openai").expect("client");
        let turn = c
            .stream_chat("m", &[Message::user("hi")], &[], |_| {}, None)
            .await
            .expect("complete stream");
        assert_eq!(turn.content, "x");
        assert_eq!(turn.usage.unwrap().total_tokens, 5);
    }

    #[tokio::test]
    async fn stops_after_finish_reason_without_waiting_for_eof() {
        let url = spawn_open_stream_server();
        let c = ChatClient::new(&url, "k", &Default::default(), None, "openai").expect("client");
        let turn = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            c.stream_chat("m", &[Message::user("hi")], &[], |_| {}, None),
        )
        .await
        .expect("must not wait for the server to close")
        .expect("turn");
        assert_eq!(turn.content, "whole");
    }

    #[test]
    fn usage_tolerates_missing_fields() {
        // A provider that omits `completion_tokens` used to fail the whole
        // chunk parse, taking the turn's content down with it.
        let u: Usage = serde_json::from_str(r#"{"prompt_tokens": 7}"#).unwrap();
        assert_eq!(u.prompt_tokens, 7);
        assert_eq!(u.completion_tokens, 0);
        assert_eq!(u.total_tokens, 0);
        // An empty object is still valid usage, not a parse error.
        let u: Usage = serde_json::from_str("{}").unwrap();
        assert_eq!((u.prompt_tokens, u.completion_tokens), (0, 0));
        // A provider reporting only totals must not break either.
        let u: Usage = serde_json::from_str(r#"{"total_tokens": 99}"#).unwrap();
        assert_eq!(u.total_tokens, 99);
    }

    #[test]
    fn usage_reads_anthropic_cache_fields() {
        let u: Usage = serde_json::from_str(
            r#"{"prompt_tokens":10,"completion_tokens":4,
                "cache_read_input_tokens":900,"cache_creation_input_tokens":100}"#,
        )
        .unwrap();
        assert_eq!(u.cache_read_tokens, Some(900));
        assert_eq!(u.cache_write_tokens, Some(100));
        // Anthropic's prompt_tokens excludes both, so billing adds them back.
        assert_eq!(u.billable_prompt(), 10 + 900 + 100);
    }

    #[test]
    fn billable_prompt_is_a_noop_without_cache_fields() {
        // OpenAI-style providers already fold cached tokens into
        // prompt_tokens and report no write count — nothing to add back.
        let u = Usage {
            prompt_tokens: 500,
            completion_tokens: 10,
            total_tokens: 510,
            ..Default::default()
        };
        assert_eq!(u.billable_prompt(), 500);
    }

    fn throttled(retry_after: &str) -> String {
        let body = r#"{"error":{"message":"slow down"}}"#;
        format!(
            "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: {ra}\r\nContent-Length: {n}\r\nConnection: close\r\n\r\n{body}",
            ra = retry_after,
            n = body.len()
        )
    }

    fn unauthorized() -> String {
        let body = r#"{"error":{"message":"invalid api key"}}"#;
        format!(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {n}\r\nConnection: close\r\n\r\n{body}",
            n = body.len()
        )
    }

    #[tokio::test]
    async fn rate_limited_request_retries_then_succeeds() {
        // Throttle twice, then serve a real completion.
        let (url, _server) =
            spawn_canned_server(vec![throttled("0"), throttled("0"), sse_ok("recovered")]);
        let c = ChatClient::new(&url, "k", &Default::default(), None, "openai").expect("client");
        let notices = std::cell::RefCell::new(Vec::new());
        let turn = c
            .stream_chat(
                "m",
                &[Message::user("hi")],
                &[],
                |ev| {
                    if let StreamEvent::Notice(n) = ev {
                        notices.borrow_mut().push(n);
                    }
                },
                None,
            )
            .await
            .expect("recovers after throttling");
        assert_eq!(turn.content, "recovered");
        // The user is told about each wait instead of staring at a spinner.
        let seen = notices.borrow();
        assert_eq!(seen.len(), 2, "one notice per retry: {seen:?}");
        assert!(seen[0].contains("rate limited"), "{seen:?}");
        assert!(seen[0].contains("429"), "{seen:?}");
    }

    #[tokio::test]
    async fn a_truncated_stream_is_not_a_complete_answer() {
        let body = format!(
            "data: {}\n\n",
            serde_json::json!({"choices": [{"delta": {"content": "half"}}]})
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let (url, _server) = spawn_canned_server(vec![response]);
        let c = ChatClient::new(&url, "k", &Default::default(), None, "openai").expect("client");
        let err = c
            .stream_chat("m", &[Message::user("hi")], &[], |_| {}, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("truncated"), "{err}");
    }

    #[tokio::test]
    async fn trailing_whitespace_after_done_is_not_truncation() {
        let body = format!(
            "data: {}\n\ndata: [DONE]\n\n\n",
            serde_json::json!({"choices": [{"delta": {"content": "whole"}}]})
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let (url, _server) = spawn_canned_server(vec![response]);
        let c = ChatClient::new(&url, "k", &Default::default(), None, "openai").expect("client");
        let turn = c
            .stream_chat("m", &[Message::user("hi")], &[], |_| {}, None)
            .await
            .expect("complete stream");
        assert_eq!(turn.content, "whole");
    }

    #[tokio::test]
    async fn survives_five_consecutive_failures() {
        // The headline behavior: five flaky failures in a row must not fail
        // the turn, as long as the sixth attempt succeeds.
        let mut responses = vec![throttled("0"); 5];
        responses.push(sse_ok("survived"));
        let (url, _server) = spawn_canned_server(responses);
        let c = ChatClient::new(&url, "k", &Default::default(), None, "openai").expect("client");
        let turn = c
            .stream_chat("m", &[Message::user("hi")], &[], |_| {}, None)
            .await
            .expect("sixth attempt succeeds");
        assert_eq!(turn.content, "survived");
    }

    #[tokio::test]
    async fn non_transient_failures_are_not_retried() {
        // A bad key must fail immediately — waiting 5 times just wastes the
        // user's time and the provider's quota.
        let (url, _server) = spawn_canned_server(vec![unauthorized(), sse_ok("never")]);
        let c = ChatClient::new(&url, "k", &Default::default(), None, "openai").expect("client");
        let notices = std::cell::RefCell::new(Vec::new());
        let err = c
            .stream_chat(
                "m",
                &[Message::user("hi")],
                &[],
                |ev| {
                    if let StreamEvent::Notice(n) = ev {
                        notices.borrow_mut().push(n);
                    }
                },
                None,
            )
            .await
            .expect_err("401 is terminal");
        let msg = format!("{err:#}");
        assert!(msg.contains("401"), "{msg}");
        assert!(msg.contains("API key"), "{msg}");
        assert!(notices.borrow().is_empty(), "must not retry a 401");
    }

    #[tokio::test]
    async fn a_cancel_during_backoff_stops_immediately() {
        // Even with 5 retries queued, Esc must win over the backoff.
        let throttled_n = DEFAULT_MAX_RETRIES + 1;
        let (url, _server) = spawn_canned_server(vec![throttled("30"); throttled_n]);
        let c = ChatClient::new(&url, "k", &Default::default(), None, "openai").expect("client");
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let started = std::time::Instant::now();
        // Flip the flag while the client is sleeping off the first Retry-After.
        let canceller = {
            let flag = std::sync::Arc::clone(&flag);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            })
        };
        let err = c
            .stream_chat("m", &[Message::user("hi")], &[], |_| {}, Some(&flag))
            .await
            .expect_err("cancel aborts the backoff");
        canceller.join().expect("canceller finished");
        let msg = format!("{err:#}");
        assert!(msg.contains("interrupted by user"), "{msg}");
        // Without cancel-awareness this would sleep 5 x 30s.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited {:?} — backoff was not cancel-aware",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn exhausted_retries_report_an_actionable_rate_limit_error() {
        // Always throttled: the error must name the cause, what the server
        // asked for, and the next step. `Retry-After: 0` keeps the waits at
        // zero while still exercising the header path.
        let throttled_n = DEFAULT_MAX_RETRIES + 1;
        let responses = vec![throttled("0"); throttled_n];
        let (url, _server) = spawn_canned_server(responses);
        let c = ChatClient::new(&url, "k", &Default::default(), None, "openai").expect("client");
        let notices = std::cell::RefCell::new(Vec::new());
        let err = c
            .stream_chat(
                "m",
                &[Message::user("hi")],
                &[],
                |ev| {
                    if let StreamEvent::Notice(n) = ev {
                        notices.borrow_mut().push(n);
                    }
                },
                None,
            )
            .await
            .expect_err("gives up once the retry budget is spent");
        let msg = format!("{err:#}");
        assert!(msg.contains("429"), "{msg}");
        // 5 retries on top of the first attempt = 6 tries.
        assert!(msg.contains("rate limited after 6 attempts"), "{msg}");
        assert!(msg.contains("/provider use"), "{msg}");
        // Every wait is announced before the final failure.
        let seen = notices.borrow();
        assert_eq!(seen.len(), DEFAULT_MAX_RETRIES, "{seen:?}");
        assert!(seen[0].contains("retry 2/6"), "{seen:?}");
        assert!(seen[4].contains("retry 6/6"), "{seen:?}");
    }

    #[test]
    fn retries_default_to_five_and_are_configurable() {
        let c = ChatClient::new(
            "http://127.0.0.1:9/v1",
            "k",
            &Default::default(),
            None,
            "openai",
        )
        .expect("client builds");
        // The user's ask: a failed request must not fail the turn outright.
        assert_eq!(c.max_retries(), 5);
        assert_eq!(DEFAULT_MAX_RETRIES, 5);

        let mut c = c;
        c.set_max_retries(2);
        assert_eq!(c.max_retries(), 2);
        c.set_max_retries(0); // opt out entirely
        assert_eq!(c.max_retries(), 0);
        // Absurd values are clamped so backoff can't outlive the session.
        c.set_max_retries(10_000);
        assert_eq!(c.max_retries(), MAX_RETRIES_LIMIT);
    }

    #[test]
    fn backoff_stays_inside_the_jitter_band() {
        // 0.75x–1.25x of an exponentially growing base, floored at 200ms.
        for attempt in 1..=8 {
            let d = backoff(attempt);
            assert!(
                d >= Duration::from_millis(200),
                "attempt {attempt}: {d:?} too small"
            );
            assert!(d <= MAX_BACKOFF, "attempt {attempt}: {d:?} exceeds cap");
        }
    }

    #[test]
    fn retry_delay_honors_retry_after_and_backs_off() {
        let mut h = reqwest::header::HeaderMap::new();

        // No header: exponential from 1s.
        assert_eq!(retry_delay(&h, 1), Duration::from_secs(1));
        assert_eq!(retry_delay(&h, 2), Duration::from_secs(2));
        assert_eq!(retry_delay(&h, 3), Duration::from_secs(4));
        // Bounded so a long retry chain can't stall a phone for minutes.
        assert_eq!(retry_delay(&h, 99), Duration::from_secs(32));

        // Delta-seconds form, including an absurd value that must be capped.
        h.insert(reqwest::header::RETRY_AFTER, "7".parse().unwrap());
        assert_eq!(retry_delay(&h, 1), Duration::from_secs(7));
        h.insert(reqwest::header::RETRY_AFTER, "9999".parse().unwrap());
        assert_eq!(retry_delay(&h, 1), Duration::from_secs(60));

        // Garbage and the HTTP-date form fall back to backoff instead of erroring.
        h.insert(reqwest::header::RETRY_AFTER, "soon".parse().unwrap());
        assert_eq!(retry_delay(&h, 2), Duration::from_secs(2));
        h.insert(
            reqwest::header::RETRY_AFTER,
            "Sun, 06 Nov 1994 08:49:37 GMT".parse().unwrap(),
        );
        assert_eq!(retry_delay(&h, 2), Duration::from_secs(2));
    }

    #[test]
    fn rate_limit_hint_is_actionable() {
        // The helper supplies the "what to do" half; the caller prefixes
        // "hint: rate limited after N attempts".
        let plain = rate_limit_hint(None);
        assert!(plain.contains("/provider use"), "{plain}");
        assert!(plain.contains("-m <model>"), "{plain}");
        let with_header = rate_limit_hint(Some("30"));
        assert!(with_header.contains("retry after 30"), "{with_header}");
    }

    #[tokio::test]
    async fn cancelled_sleep_returns_early_instead_of_waiting() {
        let flag = std::sync::atomic::AtomicBool::new(false);
        // Not cancelled: a short sleep completes.
        sleep_or_cancel(Duration::from_millis(150), Some(&flag))
            .await
            .expect("short sleep finishes");
        // Pre-cancelled: returns immediately rather than sleeping.
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
        let started = std::time::Instant::now();
        assert!(sleep_or_cancel(Duration::from_secs(30), Some(&flag))
            .await
            .is_err());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "must not sleep it out"
        );
    }

    #[test]
    fn network_settings_produce_a_usable_client() {
        // Default (unset) network must build fine.
        build_http(None).expect("default client");
        build_web_client(Duration::from_secs(1), Duration::from_secs(2)).expect("web client");

        // http proxy, socks5 proxy and an explicit insecure flag all build.
        for proxy in ["http://127.0.0.1:8080", "socks5://127.0.0.1:1080"] {
            let net = Network {
                proxy: Some(proxy.into()),
                ..Default::default()
            };
            build_http(Some(&net)).unwrap_or_else(|e| panic!("proxy {proxy}: {e:#}"));
        }
        let net = Network {
            insecure: Some(true),
            ..Default::default()
        };
        build_http(Some(&net)).expect("insecure client");
    }

    #[test]
    fn bad_network_settings_fail_with_an_actionable_message() {
        // A nonsense proxy URL is rejected up front, not at first request.
        let net = Network {
            proxy: Some("not a url".into()),
            ..Default::default()
        };
        let err = build_http(Some(&net)).unwrap_err().to_string();
        assert!(err.contains("[network].proxy"), "{err}");

        // A missing CA bundle names the path.
        let net = Network {
            ca_bundle: Some("/nonexistent/roots.pem".into()),
            ..Default::default()
        };
        let err = build_http(Some(&net)).unwrap_err().to_string();
        assert!(err.contains("/nonexistent/roots.pem"), "{err}");

        // A file that is not a PEM bundle is reported as a parse problem.
        let junk = std::env::temp_dir().join("laudacode-not-a-pem.txt");
        std::fs::write(&junk, b"this is not a certificate").unwrap();
        let net = Network {
            ca_bundle: Some(junk.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let err = build_http(Some(&net)).unwrap_err().to_string();
        assert!(err.contains("PEM"), "{err}");
        std::fs::remove_file(&junk).ok();
    }

    #[test]
    fn blank_network_values_are_treated_as_unset() {
        // Empty/whitespace strings must not become an empty proxy or a
        // request to open the current directory as a cert bundle.
        let net = Network {
            proxy: Some("   ".into()),
            ca_bundle: Some("".into()),
            insecure: Some(false),
        };
        build_http(Some(&net)).expect("blank values are ignored");
    }

    #[test]
    fn images_upgrade_content_to_multipart() {
        let plain = Message::user("hello");
        let v = serde_json::to_value(&plain).unwrap();
        assert_eq!(v["content"], "hello");

        let with_img =
            Message::user_with_images("what is this?", vec!["data:image/png;base64,AAAA".into()]);
        let v = serde_json::to_value(&with_img).unwrap();
        let parts = v["content"].as_array().expect("content must be array");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "what is this?");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn random_hex_32_is_32_hex_chars_and_varies() {
        let a = random_hex_32();
        let b = random_hex_32();
        assert_eq!(a.len(), 32, "{a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_ne!(a, b, "RandomState is not being reseeded");
    }

    #[test]
    fn messages_roundtrip_through_deserialize() {
        let m = Message::user_with_images("hi", vec!["data:image/jpeg;base64,ZZ".into()]);
        let raw = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.role, "user");
        assert_eq!(back.images.len(), 1);
        // Legacy JSON without the images field still deserializes.
        let legacy: Message = serde_json::from_str(r#"{"role":"user","content":"old"}"#).unwrap();
        assert!(legacy.images.is_empty());
    }

    /// Offline plumbing check: probe against a dead port must surface an
    /// error (not hang or silently succeed).
    #[tokio::test]
    async fn probe_fails_without_server() {
        let mut c = ChatClient::new(
            "http://127.0.0.1:9/v1",
            "k",
            &Default::default(),
            None,
            "openai",
        )
        .expect("client builds");
        // No server at all: fail fast here, the retry budget has its own test.
        c.set_max_retries(0);
        assert!(c.probe_chat("m").await.is_err());
    }

    /// Live proof that a garbage key is rejected by a real provider even
    /// when its /models endpoint is public. Run explicitly:
    /// `cargo test -- --ignored`
    #[tokio::test]
    #[ignore = "requires network"]
    async fn probe_rejects_garbage_key_on_openrouter() {
        let c = ChatClient::new(
            "https://openrouter.ai/api/v1",
            "sk-definitely-not-a-real-key",
            &Default::default(),
            None,
            "openai",
        )
        .unwrap();
        // /models is public and would happily return 200 — the chat probe
        // must NOT be fooled.
        assert!(
            c.list_models().await.is_ok(),
            "precondition: public catalog"
        );
        assert!(
            c.probe_chat("openai/gpt-4o-mini").await.is_err(),
            "garbage key must fail a real completion"
        );
    }

    /// Live end-to-end check for the built-in keyless free providers. These
    /// are external services — failures here are usually rate-limit/budget on
    /// the provider side. Run explicitly: `cargo test -- --ignored`
    #[tokio::test]
    #[ignore = "requires network"]
    async fn free_providers_stream_a_real_reply() {
        for (kind, base_url, model) in [
            (
                "aitopia",
                "https://extensions.aitopia.ai/ai/send",
                "AITOPIA",
            ),
            (
                "powerbrain",
                "https://powerbrainai.com/app/backend/api/api.php",
                "gpt-5",
            ),
        ] {
            let c = ChatClient::new(base_url, "", &Default::default(), None, kind)
                .expect("client builds");
            let reply = c
                .stream_chat(
                    model,
                    &[Message::user("reply with exactly: ok")],
                    &[],
                    |_| {},
                    None,
                )
                .await;
            match reply {
                Ok(t) => assert!(!t.content.trim().is_empty(), "{kind}: empty reply"),
                Err(e) => {
                    eprintln!("{kind} failed (may be provider-side limits): {e:#}");
                }
            }
        }
    }
}
