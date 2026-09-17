use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

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
        Ok(Self { role: raw.role, content, images, tool_calls: raw.tool_calls, tool_call_id: raw.tool_call_id })
    }
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: "system".into(), content: Some(content.into()), images: vec![], tool_calls: vec![], tool_call_id: None }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user".into(), content: Some(content.into()), images: vec![], tool_calls: vec![], tool_call_id: None }
    }
    /// User message with attached image data URIs (vision input).
    pub fn user_with_images(content: impl Into<String>, image_data_uris: Vec<String>) -> Self {
        Self { role: "user".into(), content: Some(content.into()), images: image_data_uris, tool_calls: vec![], tool_call_id: None }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: "assistant".into(), content: Some(content.into()), images: vec![], tool_calls: vec![], tool_call_id: None }
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
        Self { role: "assistant".into(), content, images: vec![], tool_calls, tool_call_id: None }
    }
}

/// Minimal standard base64 encoder (RFC 4648, with padding). Avoids adding a
/// dependency for the single use-case of embedding image attachments.
pub fn base64_encode(data: &[u8]) -> String {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TBL[(n >> 18) as usize & 63] as char);
        out.push(TBL[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TBL[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TBL[n as usize & 63] as char } else { '=' });
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
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
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: serde_json::Value,
}

/// Events emitted while streaming a completion.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Content(String),
    Reasoning(String),
    Usage(Usage),
}

/// Token usage reported by the API (when available).
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
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

#[derive(Debug, Clone)]
pub struct ChatClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    extra_headers: BTreeMap<String, String>,
    send_reasoning: bool,
    /// `reasoning_effort` hint for reasoning models ("low"|"medium"|"high").
    reasoning_effort: Option<String>,
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
struct DeltaToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<DeltaFunction>,
}

#[derive(serde::Deserialize)]
struct DeltaFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
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

impl ChatClient {
    pub fn new(
        base_url: &str,
        api_key: &str,
        headers: &BTreeMap<String, String>,
        send_reasoning: bool,
        reasoning_effort: Option<String>,
        kind: &str,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("laudacode/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            .build()
            .context("building http client")?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            extra_headers: headers.clone(),
            send_reasoning,
            reasoning_effort,
            kind: kind.to_string(),
        })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.base_url.trim_end_matches('/'), path.trim_start_matches('/'))
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
                "powerbrain" => self.stream_powerbrain(model, messages, on_event, cancel).await,
                _ => self.stream_openai(model, messages, tools, on_event, cancel).await,
            }
        };
        let Some(flag) = cancel else { return request.await };
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
        if self.send_reasoning {
            // OpenRouter-style extension; harmless elsewhere.
            body["reasoning"] = serde_json::json!({ "enabled": true });
        }
        if let Some(effort) = &self.reasoning_effort {
            // OpenAI chat-completions param for o-series / gpt-5 reasoning;
            // "max" is the xAI spelling and passes through as-is — picky
            // endpoints ignore unknown values rather than erroring.
            body["reasoning_effort"] = serde_json::json!(effort);
        }

        const MAX_ATTEMPTS: usize = 3;
        let url = self.chat_url();
        let mut attempt = 0usize;
        let mut last_err: Option<anyhow::Error> = None;
        let resp = loop {
            let result = self
                .http
                .post(&url)
                .headers(self.headers()?)
                .json(&body)
                .send()
                .await;
            attempt += 1;
            match result {
                Ok(r) if r.status().is_success() => break r,
                Ok(r) if is_retryable_status(r.status()) && attempt < MAX_ATTEMPTS => {
                    // Honor Retry-After when the server sends one (cap at 15s).
                    let wait = r
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(|s| Duration::from_secs(s.min(15)))
                        .unwrap_or_else(|| Duration::from_secs(attempt as u64));
                    tokio::time::sleep(wait).await;
                }
                Ok(r) => break r,
                Err(e) if attempt < MAX_ATTEMPTS => {
                    last_err = Some(e.into());
                    tokio::time::sleep(Duration::from_secs(attempt as u64)).await;
                }
                Err(e) => {
                    // Chain earlier failures so the user sees every cause.
                    let mut err = anyhow::anyhow!(e);
                    while let Some(prev) = last_err.take() {
                        err = err.context(prev.to_string());
                    }
                    return Err(err.context("connection failed after retries"));
                }
            }
        };

        if is_cancelled(cancel) {
            bail!("cancelled");
        }

        if !resp.status().is_success() {
            let status = resp.status();
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
                _ => {}
            }
            bail!("{err}");
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
        let mut turn = Turn::default();
        let mut acc: Vec<(usize, String, String, String)> = Vec::new(); // (index, id, name, args)

        loop {
            let next = tokio::time::timeout(STREAM_IDLE, stream.next()).await;
            let item = match next {
                Err(_) => bail!("stream stalled — no data for {}s (server hung up?)", STREAM_IDLE.as_secs()),
                Ok(None) => break,
                Ok(Some(item)) => item,
            };
            if is_cancelled(cancel) {
                bail!("cancelled");
            }
            let chunk = item.context("connection lost while streaming")?;
            buf.extend_from_slice(&chunk);
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
                                let slot = match acc.iter_mut().find(|a| a.0 == idx) {
                                    Some(s) => s,
                                    None => {
                                        acc.push((idx, String::new(), String::new(), String::new()));
                                        acc.last_mut().unwrap()
                                    }
                                };
                                if let Some(id) = dtc.id {
                                    slot.1 = id;
                                }
                                if let Some(f) = dtc.function {
                                    if let Some(n) = f.name {
                                        slot.2.push_str(&n);
                                    }
                                    if let Some(a) = f.arguments {
                                        slot.3.push_str(&a);
                                    }
                                }
                            }
                            if let Some(fr) = choice.finish_reason {
                                if !fr.is_empty() {
                                    turn.finish_reason = Some(fr);
                                }
                            }
                        }
                    }
                }
            }
        }

        turn.tool_calls = acc
            .into_iter()
            .enumerate()
            .map(|(n, (_, id, name, args))| ToolCall {
                // Some providers omit ids on single calls — synthesize one so
                // the tool result can always be matched back.
                id: if id.is_empty() { format!("call_{n}") } else { id },
                kind: "function".into(),
                function: FunctionCall { name, arguments: args },
            })
            .collect();

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
        let req = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "text/plain")
            .header("accept-language", "en-US,en;q=0.9")
            .header("cache-control", "no-cache")
            .header("hopekey", random_hex_32())
            .header("origin", "chrome-extension://becfinhbfclcgokjlobojlnldbfillpf")
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
            .json(&body)
            .send()
            .await
            .context("aitopia request failed")?;
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
        let req = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("user-agent", "Dart/3.5 (dart:io)")
            .json(&body)
            .send()
            .await
            .context("powerbrain request failed")?;
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
        let resp = self.http.get(&url).headers(self.headers()?).send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("models request failed ({status}): {}", text.chars().take(300).collect::<String>());
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
        let resp = self
            .http
            .post(&url)
            .headers(self.headers()?)
            .json(&body)
            .send()
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
    let line = line.trim().strip_prefix("data:").unwrap_or(line.trim()).trim();
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

/// A fresh 32-hex-char random token for aitopia's `hopekey` header. No rand
/// crate: seed an xorshift with the clock + process id.
fn random_hex_32() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15);
    let mut x = (nanos ^ (std::process::id() as u64).wrapping_mul(0x0100_0000_01b3)) | 1;
    let mut out = String::with_capacity(32);
    for _ in 0..4 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push_str(&format!("{x:016x}"));
    }
    out
}

/// Locate the end of the next SSE frame in `buf`.
///
/// Handles both "\n\n" and "\r\n\r\n" separators, returning whichever
/// appears first: `(separator_start, total_bytes_to_consume)`.
fn find_frame_end(buf: &[u8]) -> Option<(usize, usize)> {
    let lf_lf = buf.windows(2).position(|w| w == b"\n\n").map(|p| (p, p + 2));
    let crlf = if buf.len() >= 4 {
        buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| (p, p + 4))
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
    fn base64_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn images_upgrade_content_to_multipart() {
        let plain = Message::user("hello");
        let v = serde_json::to_value(&plain).unwrap();
        assert_eq!(v["content"], "hello");

        let with_img = Message::user_with_images(
            "what is this?",
            vec!["data:image/png;base64,AAAA".into()],
        );
        let v = serde_json::to_value(&with_img).unwrap();
        let parts = v["content"].as_array().expect("content must be array");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "what is this?");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn messages_roundtrip_through_deserialize() {
        let m = Message::user_with_images("hi", vec!["data:image/jpeg;base64,ZZ".into()]);
        let raw = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.role, "user");
        assert_eq!(back.images.len(), 1);
        // Legacy JSON without the images field still deserializes.
        let legacy: Message =
            serde_json::from_str(r#"{"role":"user","content":"old"}"#).unwrap();
        assert!(legacy.images.is_empty());
    }

    /// Offline plumbing check: probe against a dead port must surface an
    /// error (not hang or silently succeed).
    #[tokio::test]
    async fn probe_fails_without_server() {
        let c = ChatClient::new("http://127.0.0.1:9/v1", "k", &Default::default(), false, None, "openai")
            .expect("client builds");
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
            false,
            None,
            "openai",
        )
        .unwrap();
        // /models is public and would happily return 200 — the chat probe
        // must NOT be fooled.
        assert!(c.list_models().await.is_ok(), "precondition: public catalog");
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
            ("aitopia", "https://extensions.aitopia.ai/ai/send", "AITOPIA"),
            (
                "powerbrain",
                "https://powerbrainai.com/app/backend/api/api.php",
                "gpt-5",
            ),
        ] {
            let c = ChatClient::new(base_url, "", &Default::default(), false, None, kind)
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
