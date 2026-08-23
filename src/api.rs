use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

/// A single conversation message (OpenAI chat format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: "system".into(), content: Some(content.into()), tool_calls: vec![], tool_call_id: None }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user".into(), content: Some(content.into()), tool_calls: vec![], tool_call_id: None }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: "assistant".into(), content: Some(content.into()), tool_calls: vec![], tool_call_id: None }
    }
    pub fn tool_result(tool_call_id: &str, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: vec![],
            tool_call_id: Some(tool_call_id.to_string()),
        }
    }
    /// Assistant message carrying pending tool calls.
    pub fn assistant_with_tools(tool_calls: Vec<ToolCall>, content: Option<String>) -> Self {
        Self { role: "assistant".into(), content, tool_calls, tool_call_id: None }
    }
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
}

/// Fully assembled turn returned after the stream ends.
#[derive(Debug, Default, Clone)]
pub struct Turn {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ChatClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    extra_headers: BTreeMap<String, String>,
    send_reasoning: bool,
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

impl ChatClient {
    pub fn new(
        base_url: &str,
        api_key: &str,
        headers: &BTreeMap<String, String>,
        send_reasoning: bool,
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
        })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.base_url.trim_end_matches('/'), path.trim_start_matches('/'))
    }

    fn headers(&self) -> Result<HeaderMap> {
        let mut map = HeaderMap::new();
        let auth = format!("Bearer {}", self.api_key);
        map.insert("authorization", HeaderValue::from_str(&auth)?);
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

    /// Stream a chat completion. Content/reasoning deltas go through `on_event`;
    /// the assembled turn is returned at the end.
    pub async fn stream_chat<F>(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolDef],
        mut on_event: F,
    ) -> Result<Turn>
    where
        F: FnMut(StreamEvent),
    {
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

        let url = self.endpoint("/chat/completions");
        let resp = self
            .http
            .post(&url)
            .headers(self.headers()?)
            .json(&body)
            .send()
            .await?;

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
            bail!("API error ({status}): {msg}");
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
        let mut turn = Turn::default();
        let mut acc: Vec<(usize, String, String, String)> = Vec::new(); // (index, id, name, args)

        while let Some(item) = stream.next().await {
            let chunk = item.context("connection lost while streaming")?;
            buf.extend_from_slice(&chunk);
            // SSE frames are separated by a blank line.
            while let Some(pos) = find_double_newline(&buf) {
                let frame: Vec<u8> = buf.drain(..pos + 2).collect();
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
            .map(|(_, id, name, args)| ToolCall {
                id,
                kind: "function".into(),
                function: FunctionCall { name, arguments: args },
            })
            .filter(|tc| !tc.id.is_empty())
            .collect();

        Ok(turn)
    }
}

fn find_double_newline(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n").map(|p| p)
}
