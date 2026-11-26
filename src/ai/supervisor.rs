//! The supervisor: after each Hermes turn, a second model reads what Hermes did (its answer,
//! the tools it ran and what they returned) the way the user would, and flags what looks
//! wrong: claims no tool output supports, tests or builds not run, failures ignored, parts of
//! the request left undone.

use anyhow::Result;
use serde_json::json;

use super::gateway::Gateway;
use super::llm::{ApiKind, Endpoint};

const PROMPT: &str = "You supervise an AI coding agent (Hermes) the way a careful senior developer reads over its shoulder. \
You get the user's request and everything the agent did in this turn: its messages, and each tool call with its status and output. \
Look for: claims the tool outputs do not support (it says it ran, tested, fixed or verified something it did not; invented files, APIs, options or results); \
code changed without running the tests, the build or the linter, or without writing tests when they were needed; \
failed tools or errors ignored; parts of the request not done; risky actions taken without saying so. \
Judge only from what you see; do not ask for things that are irrelevant to the request. \
Answer in the language of the user's request. First line: exactly `OK` when nothing important is wrong, else exactly `ISSUES`. \
Then, for ISSUES, at most 6 short bullet points, each concrete and citing the evidence (tool, file, message). No praise, no summary.";

/// Longest transcript sent (characters): the end of the turn is kept.
const LIMIT: usize = 30_000;

/// What the supervisor reads: one line (or block) per step of the turn.
pub fn transcript(request: &str, steps: &[String]) -> String {
    let mut body = steps.join("\n\n");
    if let Some((cut, _)) = body.char_indices().rev().nth(LIMIT) {
        body = format!("[… earlier steps cut …]\n{}", &body[cut..]);
    }
    format!("USER REQUEST:\n{request}\n\nWHAT THE AGENT DID:\n{body}")
}

/// The verdict: `None` when all is well, else the issues (Markdown bullets).
pub fn verdict(reply: &str) -> Option<String> {
    let reply = reply.trim();
    let (first, rest) = reply.split_once('\n').unwrap_or((reply, ""));
    let first = first.trim().trim_matches(['*', '`', '#', ' ']).to_uppercase();
    if first == "OK" || (first.starts_with("OK") && rest.trim().is_empty()) {
        return None;
    }
    let issues = if first.starts_with("ISSUES") { rest.trim() } else { reply };
    Some(issues.to_string()).filter(|s| !s.is_empty())
}

/// Ask `model` (served by ForgeCode's gateway) to review the turn.
pub fn review(model: &str, transcript: &str) -> Result<Option<String>> {
    let gateway = Gateway::running().ok_or_else(|| anyhow::anyhow!("gateway not started"))?;
    let endpoint = Endpoint { kind: ApiKind::OpenAi, base_url: gateway.base_url(), api_key: gateway.secret.clone(), model: model.to_string(), headers: vec![], sampling: None };
    let reply = endpoint.complete(&[json!({"role": "system", "content": PROMPT}), json!({"role": "user", "content": transcript})], 1200)?;
    Ok(verdict(&reply))
}

#[cfg(test)]
mod tests {
    use super::{transcript, verdict};

    #[test]
    fn reads_the_verdict() {
        assert_eq!(verdict("OK"), None);
        assert_eq!(verdict("**OK**\n"), None);
        assert_eq!(verdict("ISSUES\n- tests were not run"), Some("- tests were not run".into()));
        // A model that skips the header still gets heard.
        assert_eq!(verdict("- claims the build passed, no build ran"), Some("- claims the build passed, no build ran".into()));
        assert_eq!(verdict("ISSUES"), None, "no issue listed: nothing to show");
    }

    #[test]
    fn keeps_the_end_of_long_turns() {
        let steps: Vec<String> = (0..5000).map(|i| format!("step {i}")).collect();
        let t = transcript("fix it", &steps);
        assert!(t.starts_with("USER REQUEST:\nfix it") && t.contains("earlier steps cut") && t.ends_with("step 4999"));
        assert!(t.len() < 31_000);
    }
}
