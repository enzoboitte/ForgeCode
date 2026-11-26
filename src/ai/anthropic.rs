//! Claude Messages API over raw HTTP (Rust has no official SDK).
//! Converts the OpenAI-format conversation; Claude's own assistant blocks (including thinking)
//! are kept in each message's `native` field and replayed unchanged.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::llm::{Endpoint, Reply, ToolCall, send, sse_data};

const VERSION: &str = "2023-06-01";
pub const DEFAULT_URL: &str = "https://api.anthropic.com";

pub fn headers(req: ureq::Request, api_key: &str) -> ureq::Request {
    req.set("x-api-key", api_key).set("anthropic-version", VERSION).set("content-type", "application/json")
}

fn post(ep: &Endpoint) -> ureq::Request {
    let mut req = headers(ureq::post(&format!("{}/v1/messages", ep.base_url.trim_end_matches('/'))), &ep.api_key);
    // Server-side refusal fallback on the models that support it.
    if ep.model.starts_with("claude-opus-5") || ep.model.starts_with("claude-fable-5") {
        req = req.set("anthropic-beta", "server-side-fallback-2026-07-01");
    }
    req
}

fn body(ep: &Endpoint, messages: &[Value], tools: &[Value], max_tokens: u32, stream: bool) -> Value {
    let (system, messages) = convert(messages);
    let mut body = json!({"model": ep.model, "max_tokens": max_tokens, "messages": messages, "stream": stream});
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if !tools.is_empty() {
        body["tools"] = tools
            .iter()
            .map(|t| {
                let f = &t["function"];
                json!({"name": f["name"], "description": f["description"], "input_schema": f["parameters"], "eager_input_streaming": true})
            })
            .collect();
    }
    // Only values the user set (see `Resolver::backend_for`); Claude has no min_p or penalties.
    if let Some(s) = &ep.sampling {
        if let Some(t) = s.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(p) = s.top_p {
            body["top_p"] = json!(p);
        }
        if let Some(k) = s.top_k.filter(|k| *k > 0) {
            body["top_k"] = json!(k);
        }
        if let (Some(m), true) = (s.max_tokens, stream) {
            body["max_tokens"] = json!(m);
        }
    }
    if ep.model.starts_with("claude-opus-5") || ep.model.starts_with("claude-fable-5") {
        body["fallbacks"] = json!("default");
    }
    body
}

/// OpenAI content (string or parts) -> Claude content blocks.
fn blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::String(s) if s.is_empty() => vec![],
        Value::String(s) => vec![json!({"type": "text", "text": s})],
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match p["type"].as_str()? {
                "text" => Some(json!({"type": "text", "text": p["text"]})),
                "image_url" => {
                    let url = p["image_url"]["url"].as_str()?;
                    let (meta, data) = url.strip_prefix("data:")?.split_once(";base64,")?;
                    Some(json!({"type": "image", "source": {"type": "base64", "media_type": meta, "data": data}}))
                }
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

/// Returns (system prompt, messages). Consecutive same-role turns are merged so that all
/// tool results of one assistant turn land in a single user message.
pub fn convert(messages: &[Value]) -> (String, Vec<Value>) {
    let mut system = String::new();
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        let (role, content) = match m["role"].as_str().unwrap_or("user") {
            "system" => {
                system.push_str(m["content"].as_str().unwrap_or(""));
                continue;
            }
            "assistant" => {
                let content = match m.get("native") {
                    Some(native) => native.as_array().cloned().unwrap_or_default(),
                    None => {
                        let mut c = blocks(&m["content"]);
                        for call in m["tool_calls"].as_array().into_iter().flatten() {
                            let input: Value = serde_json::from_str(call["function"]["arguments"].as_str().unwrap_or("{}")).unwrap_or(json!({}));
                            c.push(json!({"type": "tool_use", "id": call["id"], "name": call["function"]["name"], "input": input}));
                        }
                        c
                    }
                };
                ("assistant", content)
            }
            "tool" => ("user", vec![json!({"type": "tool_result", "tool_use_id": m["tool_call_id"], "content": m["content"]})]),
            _ => ("user", blocks(&m["content"])),
        };
        if content.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last) if last["role"] == role => last["content"].as_array_mut().unwrap().extend(content),
            _ => out.push(json!({"role": role, "content": content})),
        }
    }
    (system, out)
}

pub fn complete(ep: &Endpoint, messages: &[Value], max_tokens: u32) -> Result<String> {
    let resp: Value = send(post(ep), body(ep, messages, &[], max_tokens, false), &ep.base_url)?.into_json()?;
    Ok(resp["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| (b["type"] == "text").then(|| b["text"].as_str().unwrap_or("")))
        .collect())
}

pub fn chat(ep: &Endpoint, messages: &[Value], tools: &[Value], on_text: &mut dyn FnMut(&str)) -> Result<Reply> {
    let resp = send(post(ep), body(ep, messages, tools, 64_000, true), &ep.base_url)?;
    let mut acc = Accumulator::default();
    for event in sse_data(resp) {
        if let Some(text) = acc.apply(&event)? {
            on_text(&text);
        }
    }
    acc.finish()
}

/// Rebuilds content blocks from stream events.
#[derive(Default)]
struct Accumulator {
    blocks: Vec<Value>,
    /// Partial JSON of each tool_use block's input, by block index.
    inputs: Vec<String>,
    stop_reason: Option<String>,
}

impl Accumulator {
    fn apply(&mut self, e: &Value) -> Result<Option<String>> {
        let ix = e["index"].as_u64().unwrap_or(0) as usize;
        match e["type"].as_str() {
            Some("content_block_start") => {
                if self.blocks.len() <= ix {
                    self.blocks.resize(ix + 1, Value::Null);
                    self.inputs.resize(ix + 1, String::new());
                }
                self.blocks[ix] = e["content_block"].clone();
            }
            Some("content_block_delta") => {
                let d = &e["delta"];
                let Some(block) = self.blocks.get_mut(ix) else { return Ok(None) };
                match d["type"].as_str() {
                    Some("text_delta") => {
                        let t = d["text"].as_str().unwrap_or("");
                        block["text"] = json!(format!("{}{t}", block["text"].as_str().unwrap_or("")));
                        return Ok(Some(t.to_string()));
                    }
                    Some("input_json_delta") => self.inputs[ix].push_str(d["partial_json"].as_str().unwrap_or("")),
                    Some("thinking_delta") => {
                        block["thinking"] = json!(format!("{}{}", block["thinking"].as_str().unwrap_or(""), d["thinking"].as_str().unwrap_or("")));
                    }
                    Some("signature_delta") => block["signature"] = d["signature"].clone(),
                    _ => {}
                }
            }
            Some("message_delta") => self.stop_reason = e["delta"]["stop_reason"].as_str().map(String::from),
            Some("error") => bail!("Claude API error: {}", e["error"]["message"].as_str().unwrap_or("unknown")),
            _ => {}
        }
        Ok(None)
    }

    fn finish(mut self) -> Result<Reply> {
        if self.stop_reason.as_deref() == Some("refusal") {
            bail!("Claude declined this request (refusal).");
        }
        let mut reply = Reply { truncated: self.stop_reason.as_deref() == Some("max_tokens"), ..Default::default() };
        for (block, input) in self.blocks.iter_mut().zip(&self.inputs) {
            match block["type"].as_str() {
                Some("text") => reply.content.push_str(block["text"].as_str().unwrap_or("")),
                Some("tool_use") => {
                    // Eager input streaming skips server-side validation: an invalid or truncated
                    // input reaches the agent as unparseable arguments and is reported back as an error.
                    let args = if input.is_empty() { "{}".to_string() } else { input.clone() };
                    block["input"] = serde_json::from_str(&args).unwrap_or(json!({}));
                    reply.tool_calls.push(ToolCall {
                        id: block["id"].as_str().unwrap_or("").into(),
                        name: block["name"].as_str().unwrap_or("").into(),
                        arguments: args,
                    });
                }
                _ => {}
            }
        }
        self.blocks.retain(|b| !b.is_null());
        reply.native = Some(Value::Array(self.blocks));
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_openai_history() {
        let history = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": [{"type": "text", "text": "hi"}, {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}}]}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "t1", "function": {"name": "a", "arguments": "{\"x\":1}"}},
                {"id": "t2", "function": {"name": "b", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "t1", "content": "r1"}),
            json!({"role": "tool", "tool_call_id": "t2", "content": "r2"}),
        ];
        let (system, msgs) = convert(&history);
        assert_eq!(system, "sys");
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["content"][1]["source"]["media_type"], "image/png");
        assert_eq!(msgs[1]["content"][0]["input"]["x"], 1);
        assert_eq!(msgs[2]["content"].as_array().unwrap().len(), 2, "tool results share one user turn");
    }

    #[test]
    fn accumulates_stream_with_thinking_and_tools() {
        let mut acc = Accumulator::default();
        for e in [
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Reading"}}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "tu", "name": "read_file", "input": {}}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "\"a\"}"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
        ] {
            acc.apply(&e).unwrap();
        }
        let reply = acc.finish().unwrap();
        assert_eq!(reply.content, "Reading");
        assert_eq!(reply.tool_calls[0].arguments, r#"{"path":"a"}"#);
        let native = reply.native.unwrap();
        assert_eq!(native[0]["signature"], "sig");
        assert_eq!(native[2]["input"]["path"], "a");
    }
}
