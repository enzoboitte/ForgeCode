//! Model catalog: what each available model is good at, so agents (even small local ones trained
//! before these models existed) can pick a model per task. Also resolves "<provider>/<model>".

use std::sync::Arc;

use anyhow::{Result, anyhow};

use super::agent::Backend;
use super::anthropic;
use super::copilot::Copilot;
use super::llm::{ApiKind, Endpoint};
use super::sampling::{self, Purpose, Sampling};
use crate::settings::{Provider, split_model};

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub enum Tier {
    Small,
    Medium,
    Large,
    Frontier,
}

#[derive(Clone, Debug)]
pub struct ModelInfo {
    /// "<provider>/<model>"
    pub model_ref: String,
    pub local: bool,
    pub tier: Tier,
    pub strengths: String,
}

impl ModelInfo {
    /// One catalog line for prompts.
    pub fn line(&self) -> String {
        let place = if self.local { "local" } else { "cloud" };
        format!("- {} ({place}, {:?}): {}", self.model_ref, self.tier, self.strengths)
    }
}

/// Parameter count in billions from names like `qwen2.5-coder:7b`, `llama-3.1-70b`, `35b-a3b`.
fn billions(name: &str) -> Option<f32> {
    let n = name.to_lowercase();
    n.split(|c: char| !c.is_ascii_alphanumeric() && c != '.')
        .filter_map(|part| part.strip_suffix('b'))
        .filter_map(|num| num.parse::<f32>().ok())
        .reduce(f32::max)
}

/// Best-effort profile from the model id.
pub fn describe(model_ref: &str, local: bool) -> ModelInfo {
    let (_, model) = split_model(model_ref);
    let m = model.to_lowercase();
    let has = |s: &str| m.contains(s);
    let (tier, strengths): (Tier, &str) = if has("opus") || has("fable") || has("mythos") {
        (Tier::Frontier, "deepest reasoning, long autonomous coding, large refactors, architecture")
    } else if has("sonnet") {
        (Tier::Large, "strong coding and tool use, good speed/quality balance")
    } else if has("haiku") {
        (Tier::Medium, "fast and cheap, simple edits, summaries, searches")
    } else if has("gpt-5") && !has("mini") && !has("nano") {
        (Tier::Frontier, "strong reasoning and coding")
    } else if has("o1") || has("o3") || has("o4") || has("-r1") || has("reason") || has("think") {
        (Tier::Large, "step-by-step reasoning, math, tricky bugs; slower")
    } else if has("gemini") && has("pro") {
        (Tier::Large, "long context, broad knowledge, coding")
    } else if has("mini") || has("nano") || has("flash") || has("lite") {
        (Tier::Medium, "fast, cheap; quick lookups, simple edits, classification")
    } else if has("gpt-4.1") || has("gpt-4o") || has("gpt-4") {
        (Tier::Large, "solid coding and instruction following, fast")
    } else if local {
        let b = billions(&m).unwrap_or(7.);
        let code = has("coder") || has("code") || has("devstral") || has("starcoder") || has("codestral");
        let tier = if b < 10. { Tier::Small } else if b < 40. { Tier::Medium } else { Tier::Large };
        let what = match (tier, code) {
            (Tier::Small, true) => "small local coder: completions, small focused edits, private data",
            (Tier::Small, false) => "small local model: short answers, summaries, private data",
            (_, true) => "local coding model: edits and reviews without sending code out",
            _ => "local general model: private data, offline work",
        };
        (tier, what)
    } else {
        (Tier::Medium, "general purpose")
    };
    ModelInfo { model_ref: model_ref.to_string(), local, tier, strengths: strengths.to_string() }
}

/// Every model the user can reach: Copilot models and each configured server's models.
pub fn catalog(providers: &[Provider], copilot_models: &[String], copilot_signed_in: bool) -> Vec<ModelInfo> {
    let mut out = vec![];
    if copilot_signed_in {
        out.extend(copilot_models.iter().map(|m| describe(&format!("copilot/{m}"), false)));
    }
    for p in providers {
        let local = p.kind == ApiKind::OpenAi && sampling::self_hosted(&p.base_url);
        out.extend(p.models.iter().map(|m| describe(&format!("{}/{m}", p.id), local)));
    }
    out
}

/// Model of the security reviewer: the user's choice, else a local model (the chat's if it is one),
/// else a Copilot model, else the chat model.
pub fn security_model(catalog: &[ModelInfo], chosen: &str, main_model: &str) -> String {
    if !chosen.is_empty() {
        return chosen.to_string();
    }
    let is_main = |m: &&ModelInfo| m.model_ref == main_model;
    let local = || catalog.iter().filter(|m| m.local);
    let copilot = || catalog.iter().filter(|m| m.model_ref.starts_with("copilot/"));
    local()
        .find(is_main)
        .or_else(|| local().next())
        .or_else(|| copilot().find(is_main))
        .or_else(|| copilot().find(|m| m.tier == Tier::Medium))
        .or_else(|| copilot().next())
        .map_or(main_model.to_string(), |m| m.model_ref.clone())
}

/// Turns model references into backends; lives on the agent thread.
#[derive(Clone)]
pub struct Resolver {
    pub providers: Vec<Provider>,
    pub copilot: Vec<Arc<Copilot>>,
}

impl Resolver {
    pub fn backend(&self, model_ref: &str) -> Result<Backend> {
        self.backend_for(model_ref, Purpose::Code, Sampling::default())
    }

    /// Backend tuned for `purpose`; `chosen` are settings the agent picked (the user's still win).
    pub fn backend_for(&self, model_ref: &str, purpose: Purpose, chosen: Sampling) -> Result<Backend> {
        let (provider, model) = split_model(model_ref);
        let (primary, used) = if provider == "copilot" {
            if self.copilot.is_empty() && self.providers.iter().all(|p| p.models.is_empty()) {
                return Err(anyhow!("Sign in to GitHub Copilot in Settings › AI first."));
            }
            let accounts = super::copilot::supporting(&self.copilot, model);
            ((!accounts.is_empty()).then(|| Backend::Copilot { accounts: accounts.clone(), model: model.into() }), accounts)
        } else {
            let p = self.providers.iter().find(|p| p.id == provider).ok_or_else(|| anyhow!("Unknown provider `{provider}`"))?;
            (Some(self.direct(p, model, purpose, chosen)), vec![])
        };
        // When it fails (quota, rate limit, server down), the other accounts and servers take over,
        // each with a model it has: the same one when it can.
        let mut chain: Vec<Backend> = primary.into_iter().collect();
        for a in self.copilot.iter().filter(|a| !used.iter().any(|u| Arc::ptr_eq(u, a))) {
            let listed = a.models.lock().unwrap().clone();
            let model = if listed.is_empty() || listed.iter().any(|m| m == model) {
                model.to_string()
            } else {
                listed.iter().find(|m| *m == "gpt-4.1").unwrap_or(&listed[0]).clone()
            };
            chain.push(Backend::Copilot { accounts: vec![a.clone()], model });
        }
        for p in self.providers.iter().filter(|p| p.id != provider) {
            if let Some(m) = p.models.first() {
                chain.push(self.direct(p, m, purpose, chosen));
            }
        }
        Ok(if chain.len() == 1 { chain.remove(0) } else { Backend::Chain(chain) })
    }

    fn direct(&self, p: &Provider, model: &str, purpose: Purpose, chosen: Sampling) -> Backend {
        let base_url = if p.base_url.is_empty() && p.kind == ApiKind::Anthropic { anthropic::DEFAULT_URL.into() } else { p.base_url.clone() };
        let sampling = if p.kind == ApiKind::OpenAi && sampling::self_hosted(&base_url) {
            Some(sampling::resolve(model, purpose, p.sampling.or(chosen)))
        } else {
            // Public APIs may reject unknown fields: only send what the user set.
            let mine = p.sampling.or(chosen);
            (!mine.is_empty()).then_some(mine)
        };
        Backend::Direct(Endpoint { kind: p.kind, base_url, api_key: p.api_key.clone(), model: model.into(), headers: vec![], sampling })
    }
}

/// Default model for a sub-agent role: cheap and fast for research, the strongest for code.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_known_and_local_models() {
        assert_eq!(describe("anthropic/claude-opus-5", false).tier, Tier::Frontier);
        assert_eq!(describe("copilot/gpt-4.1-mini", false).tier, Tier::Medium);
        assert_eq!(describe("ollama/qwen2.5-coder:7b", true).tier, Tier::Small);
        assert!(describe("ollama/qwen2.5-coder:7b", true).strengths.contains("coder"));
        assert_eq!(describe("lm/qwen3.6-35b-a3b-instruct", true).tier, Tier::Medium);
        assert_eq!(billions("llama-3.1-70b-instruct"), Some(70.));
    }

    /// Every account and server backs the chosen model up, each with a model it has.
    #[test]
    fn other_accounts_and_servers_take_over() {
        let (a, b) = (Copilot::new("a".into()), Copilot::new("b".into()));
        *a.models.lock().unwrap() = vec!["claude-sonnet-5.5".into()];
        *b.models.lock().unwrap() = vec!["gpt-5".into(), "gpt-4.1".into()];
        let server = Provider { id: "tyche".into(), name: "Tyche".into(), kind: Default::default(), base_url: "http://localhost:1/v1".into(), api_key: String::new(), models: vec!["qwen".into()], sampling: Default::default() };
        let resolver = Resolver { providers: vec![server], copilot: vec![a, b] };
        let Backend::Chain(chain) = resolver.backend("copilot/claude-sonnet-5.5").unwrap() else { panic!("a chain") };
        let models: Vec<String> = chain.iter().map(|b| b.label().split(' ').next().unwrap().to_string()).collect();
        assert_eq!(models, ["claude-sonnet-5.5", "gpt-4.1", "qwen"]);
        // From the server, Copilot is the backup.
        let Backend::Chain(chain) = resolver.backend("tyche/qwen").unwrap() else { panic!("a chain") };
        assert_eq!(chain.len(), 3);
    }

    #[test]
    fn security_model_prefers_local_then_copilot() {
        let local = describe("tyche/qwen", true);
        let fast = describe("copilot/gpt-4.1-mini", false);
        let big = describe("copilot/claude-opus-5", false);
        assert_eq!(security_model(&[big.clone(), fast.clone(), local], "", "copilot/claude-opus-5"), "tyche/qwen");
        assert_eq!(security_model(&[big.clone(), fast], "", "x/y"), "copilot/gpt-4.1-mini");
        assert_eq!(security_model(&[big], "", "copilot/claude-opus-5"), "copilot/claude-opus-5");
        assert_eq!(security_model(&[], "", "x/y"), "x/y");
        assert_eq!(security_model(&[], "a/b", "x/y"), "a/b");
    }
}
