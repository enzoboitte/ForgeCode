//! Chat-completion clients. The conversation is kept in OpenAI format; [`anthropic`] converts it
//! for the Claude Messages API.

use std::io::BufRead;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use super::anthropic;
pub use crate::settings::ProviderKind as ApiKind;

#[derive(Clone, Debug)]
pub struct Endpoint {
    pub kind: ApiKind,
    /// e.g. `http://localhost:11434/v1` or `https://api.anthropic.com`
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub headers: Vec<(String, String)>,
    /// Generation settings; `None` = a low temperature, fine for cloud models.
    pub sampling: Option<super::sampling::Sampling>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Default)]
pub struct Reply {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    /// Provider-native assistant content to replay verbatim next turn (Claude thinking blocks).
    pub native: Option<Value>,
    /// The output limit cut the reply: tool arguments may be incomplete.
    pub truncated: bool,
}

/// Rate limit or exhausted quota: worth retrying on another account.
pub fn is_quota_error(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    ["HTTP 429", "HTTP 402", "HTTP 403"].iter().any(|p| s.starts_with(p)) || s.contains("quota")
}

pub(super) fn send(req: ureq::Request, body: Value, base_url: &str) -> Result<ureq::Response> {
    req.timeout(std::time::Duration::from_secs(600)).send_json(body).map_err(|e| match e {
        ureq::Error::Status(code, resp) => anyhow!("HTTP {code}: {}", resp.into_string().unwrap_or_default()),
        other => anyhow!("{other} (is the server at {base_url} running?)"),
    })
}

/// `data:` payloads of a server-sent-events stream.
pub(super) fn sse_data(resp: ureq::Response) -> impl Iterator<Item = Value> {
    std::io::BufReader::new(resp.into_reader())
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| line.strip_prefix("data:").map(|d| d.trim().to_string()))
        .take_while(|d| d != "[DONE]")
        .filter_map(|d| serde_json::from_str(&d).ok())
}

impl Endpoint {
    fn post(&self, path: &str) -> ureq::Request {
        let mut req = ureq::post(&format!("{}{path}", self.base_url.trim_end_matches('/'))).set("Content-Type", "application/json");
        if !self.api_key.is_empty() {
            req = req.set("Authorization", &format!("Bearer {}", self.api_key));
        }
        for (k, v) in &self.headers {
            req = req.set(k, v);
        }
        req
    }

    /// Non-streaming completion returning the text.
    pub fn complete(&self, messages: &[Value], max_tokens: u32) -> Result<String> {
        if self.kind == ApiKind::Anthropic {
            return anthropic::complete(self, messages, max_tokens);
        }
        let body = json!({"model": self.model, "messages": messages, "max_tokens": max_tokens, "temperature": 0.1});
        let resp: Value = send(self.post("/chat/completions"), body, &self.base_url)?.into_json()?;
        Ok(resp["choices"][0]["message"]["content"].as_str().unwrap_or("").to_string())
    }

    /// Streaming chat with tools; `on_text` receives content deltas as they arrive.
    pub fn chat(&self, messages: &[Value], tools: &[Value], mut on_text: impl FnMut(&str)) -> Result<Reply> {
        if self.kind == ApiKind::Anthropic {
            return anthropic::chat(self, messages, tools, &mut on_text);
        }
        // Drop fields only Claude understands.
        let messages: Vec<Value> = messages.iter().cloned().map(|mut m| { m.as_object_mut().map(|o| o.remove("native")); m }).collect();
        // Explicit limit: some local servers default to a few hundred tokens and cut files mid-write.
        let mut body = json!({"model": self.model, "messages": messages, "stream": true, "temperature": 0.2, "max_tokens": 16384});
        if let Some(sampling) = &self.sampling {
            super::sampling::apply(&mut body, sampling);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
        }
        let mut reply = Reply::default();
        for chunk in sse_data(send(self.post("/chat/completions"), body, &self.base_url)?) {
            apply_delta(&mut reply, &chunk["choices"][0]["delta"], &mut on_text);
            if chunk["choices"][0]["finish_reason"] == "length" {
                reply.truncated = true;
            }
        }
        finish_tool_calls(&mut reply);
        Ok(reply)
    }
}

/// Servers number tool calls by content block (Claude via Copilot starts at 1 after a text block),
/// which leaves empty slots: drop them, and give every call an id (the API rejects calls without one).
fn finish_tool_calls(reply: &mut Reply) {
    reply.tool_calls.retain(|c| !c.name.trim().is_empty());
    for (i, call) in reply.tool_calls.iter_mut().enumerate() {
        if call.id.is_empty() {
            call.id = format!("call_{i}");
        }
    }
}

/// Merge one streamed delta into the reply. Tool calls arrive in fragments keyed by `index`.
fn apply_delta(reply: &mut Reply, delta: &Value, on_text: &mut impl FnMut(&str)) {
    if let Some(text) = delta["content"].as_str() {
        reply.content.push_str(text);
        on_text(text);
    }
    for call in delta["tool_calls"].as_array().into_iter().flatten() {
        let index = call["index"].as_u64().unwrap_or(reply.tool_calls.len() as u64) as usize;
        if reply.tool_calls.len() <= index {
            reply.tool_calls.resize(index + 1, ToolCall::default());
        }
        let slot = &mut reply.tool_calls[index];
        if let Some(id) = call["id"].as_str() {
            slot.id = id.to_string();
        }
        if let Some(name) = call["function"]["name"].as_str() {
            slot.name.push_str(name);
        }
        match &call["function"]["arguments"] {
            Value::String(args) => slot.arguments.push_str(args),
            // Some servers (Ollama) send already-parsed objects.
            Value::Object(_) => slot.arguments = call["function"]["arguments"].to_string(),
            _ => {}
        }
    }
}

/// Model ids served by a provider.
pub fn list_models(kind: ApiKind, base_url: &str, api_key: &str) -> Result<Vec<String>> {
    let base = base_url.trim_end_matches('/');
    let resp: Value = match kind {
        ApiKind::OpenAi => {
            let mut req = ureq::get(&format!("{base}/models"));
            if !api_key.is_empty() {
                req = req.set("Authorization", &format!("Bearer {api_key}"));
            }
            req.call()?.into_json()?
        }
        ApiKind::Anthropic => anthropic::headers(ureq::get(&format!("{base}/v1/models?limit=100")), api_key).call()?.into_json()?,
    };
    // OpenAI and Anthropic both answer `{data: [{id}]}`; Ollama's native API answers `{models: [{name}]}`.
    let ids = resp["data"].as_array().or(resp["models"].as_array()).into_iter().flatten();
    Ok(ids.filter_map(|m| m["id"].as_str().or(m["name"].as_str()).map(String::from)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_fragmented_tool_calls() {
        let mut reply = Reply::default();
        let mut sink = |_: &str| {};
        apply_delta(&mut reply, &json!({"tool_calls":[{"index":0,"id":"c1","function":{"name":"read_file","arguments":"{\"pa"}}]}), &mut sink);
        apply_delta(&mut reply, &json!({"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a\"}"}}]}), &mut sink);
        apply_delta(&mut reply, &json!({"content":"ok"}), &mut sink);
        assert_eq!(reply.tool_calls[0], ToolCall { id: "c1".into(), name: "read_file".into(), arguments: r#"{"path":"a"}"#.into() });
        assert_eq!(reply.content, "ok");
    }

    #[test]
    fn drops_empty_slots_from_block_numbered_tool_calls() {
        let mut reply = Reply::default();
        let mut sink = |_: &str| {};
        apply_delta(&mut reply, &json!({"tool_calls":[{"index":1,"function":{"name":"bash","arguments":"{}"}}]}), &mut sink);
        apply_delta(&mut reply, &json!({"tool_calls":[{"index":3,"id":"t3","function":{"name":"grep","arguments":"{}"}}]}), &mut sink);
        finish_tool_calls(&mut reply);
        assert_eq!(reply.tool_calls.len(), 2);
        assert_eq!((reply.tool_calls[0].id.as_str(), reply.tool_calls[1].id.as_str()), ("call_0", "t3"));
    }

    #[test]
    fn quota_errors_are_detected() {
        assert!(is_quota_error(&anyhow!("HTTP 429: slow down")));
        assert!(!is_quota_error(&anyhow!("HTTP 400: bad request")));
    }
}
