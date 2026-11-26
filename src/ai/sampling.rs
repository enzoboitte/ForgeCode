//! Generation settings sent to self-hosted servers. Local models are very sensitive to them: Qwen3 at
//! a low temperature, for instance, repeats the same tool call forever. Without user overrides we
//! send the values each model family's authors recommend, adjusted to what the agent does.

use serde_json::Value;

pub use crate::settings::Sampling;

/// What an agent is doing, which shifts its settings slightly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Purpose {
    /// Writing code (classic chat, coder sub-agents): precise and deterministic.
    Code,
    /// Planning, delegating, researching (smart mode, researchers): precise, mostly tools,
    /// a little more varied so it does not run the same search again and again.
    Orchestrate,
    /// Reviewing: precise.
    Review,
}

impl Purpose {
    pub fn for_role(role: &str) -> Self {
        match role {
            "coder" => Purpose::Code,
            "reviewer" | "security" => Purpose::Review,
            _ => Purpose::Orchestrate,
        }
    }
}

fn s(temperature: f32, top_p: f32, top_k: u32, min_p: f32, repeat_penalty: f32, presence_penalty: f32, max_tokens: u32) -> Sampling {
    Sampling {
        temperature: Some(temperature),
        top_p: Some(top_p),
        top_k: Some(top_k),
        min_p: Some(min_p),
        repeat_penalty: Some(repeat_penalty),
        presence_penalty: Some(presence_penalty),
        max_tokens: Some(max_tokens),
    }
}

/// Recommended coding settings of a model family (from the model cards).
fn family(model: &str) -> Sampling {
    let m = model.to_lowercase();
    let has = |s: &str| m.contains(s);
    if has("qwen") && has("coder") {
        s(0.7, 0.8, 20, 0.0, 1.05, 0.0, 32_768)
    } else if has("qwen3") || has("qwq") {
        // Thinking mode, "precise coding tasks". Never greedy: low temperatures loop.
        s(0.6, 0.95, 20, 0.0, 1.0, 0.0, 32_768)
    } else if has("qwen") {
        s(0.7, 0.8, 20, 0.0, 1.05, 0.0, 16_384)
    } else if has("deepseek") && (has("r1") || has("reason")) {
        s(0.6, 0.95, 40, 0.0, 1.0, 0.0, 32_768)
    } else if has("devstral") || has("mistral") || has("magistral") || has("codestral") {
        s(0.15, 0.95, 40, 0.01, 1.0, 0.0, 16_384)
    } else if has("gemma") {
        s(1.0, 0.95, 64, 0.0, 1.0, 0.0, 16_384)
    } else if has("gpt-oss") {
        s(1.0, 1.0, 0, 0.0, 1.0, 0.0, 32_768)
    } else if has("glm") {
        s(0.7, 0.95, 40, 0.0, 1.0, 0.0, 32_768)
    } else if has("llama") || has("phi") {
        s(0.6, 0.9, 40, 0.05, 1.0, 0.0, 16_384)
    } else {
        s(0.6, 0.95, 40, 0.05, 1.0, 0.0, 16_384)
    }
}

/// Settings for `model` doing `purpose`: user overrides first, then the recommendation.
pub fn resolve(model: &str, purpose: Purpose, user: Sampling) -> Sampling {
    let mut rec = family(model);
    if purpose == Purpose::Orchestrate {
        rec.temperature = rec.temperature.map(|t| (t + 0.1).min(1.0));
        rec.presence_penalty = rec.presence_penalty.map(|p| p.max(0.5));
    }
    user.or(rec)
}

/// Model servers people run themselves (llama.cpp, Ollama, LM Studio, vLLM… locally or on their
/// own domain) accept the extra fields. Public APIs may reject them, so only user-chosen values
/// are sent there.
pub fn self_hosted(base_url: &str) -> bool {
    const PUBLIC: [&str; 12] = [
        "openai.com", "openai.azure.com", "openrouter.ai", "groq.com", "mistral.ai", "deepseek.com",
        "together.xyz", "together.ai", "googleapis.com", "x.ai", "fireworks.ai", "githubcopilot.com",
    ];
    let host = base_url.split("://").nth(1).unwrap_or(base_url).split(['/', ':']).next().unwrap_or("").to_lowercase();
    !PUBLIC.iter().any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// Add the settings to an OpenAI-style request body.
pub fn apply(body: &mut Value, sampling: &Sampling) {
    let mut set = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            body[k] = v;
        }
    };
    set("temperature", sampling.temperature.map(Value::from));
    set("top_p", sampling.top_p.map(Value::from));
    set("top_k", sampling.top_k.filter(|k| *k > 0).map(Value::from));
    set("min_p", sampling.min_p.map(Value::from));
    // llama.cpp / LM Studio / Ollama name it `repeat_penalty`, vLLM `repetition_penalty`.
    set("repeat_penalty", sampling.repeat_penalty.map(Value::from));
    set("repetition_penalty", sampling.repeat_penalty.map(Value::from));
    set("presence_penalty", sampling.presence_penalty.map(Value::from));
    set("max_tokens", sampling.max_tokens.map(Value::from));
}

/// Settings an agent asked for in a `delegate` call.
pub fn from_json(v: &Value) -> Sampling {
    let f = |k: &str| v[k].as_f64().map(|x| x as f32);
    let u = |k: &str| v[k].as_u64().map(|x| x as u32);
    Sampling {
        temperature: f("temperature").map(|t| t.clamp(0.0, 2.0)),
        top_p: f("top_p").map(|p| p.clamp(0.0, 1.0)),
        top_k: u("top_k"),
        min_p: f("min_p").map(|p| p.clamp(0.0, 1.0)),
        repeat_penalty: f("repeat_penalty").or(f("repetition_penalty")).map(|p| p.clamp(0.5, 2.0)),
        presence_penalty: f("presence_penalty").map(|p| p.clamp(-2.0, 2.0)),
        max_tokens: u("max_tokens").map(|m| m.clamp(256, 131_072)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwen3_never_gets_a_looping_temperature() {
        let code = resolve("qwen3.6-35b-a3b-uncensored", Purpose::Code, Sampling::default());
        assert_eq!((code.temperature, code.top_p, code.top_k), (Some(0.6), Some(0.95), Some(20)));
        let smart = resolve("Qwen3.6-35B-A3B", Purpose::Orchestrate, Sampling::default());
        assert!(smart.temperature.unwrap() > 0.6 && smart.presence_penalty.unwrap() > 0.0);
        let user = resolve("qwen3.6", Purpose::Code, Sampling { temperature: Some(0.3), ..Default::default() });
        assert_eq!((user.temperature, user.top_k), (Some(0.3), Some(20)), "user values win, the rest stays recommended");
    }

    #[test]
    fn extra_fields_are_not_sent_to_public_apis() {
        assert!(self_hosted("http://localhost:8080/v1"));
        assert!(self_hosted("http://192.168.1.20:1234/v1"));
        assert!(self_hosted("https://llm.example.fr/v1"), "a server on the user's own domain");
        assert!(!self_hosted("https://api.openai.com/v1"));
        assert!(!self_hosted("https://openrouter.ai/api/v1"));
    }

    #[test]
    fn body_uses_both_penalty_names() {
        let mut body = serde_json::json!({"model": "m"});
        apply(&mut body, &resolve("qwen3", Purpose::Code, Sampling::default()));
        assert_eq!((body["repeat_penalty"].as_f64(), body["repetition_penalty"].as_f64(), body["top_k"].as_u64()), (Some(1.0), Some(1.0), Some(20)));
    }
}
