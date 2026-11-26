//! Agents: the loop (model -> tools -> model), modes, the security reviewer and delegation to
//! sub-agents that each pick their own model. Runs on a background thread and reports through
//! `(agent id, AgentEvent)` messages; agent 0 is the one the user talks to.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::copilot::{Copilot, with_failover};
use super::llm::{Endpoint, Reply, ToolCall};
use super::models::{ModelInfo, Resolver, Tier, describe};
use super::sampling::{self, Purpose};
use super::tools::{READ_ONLY, SideEffect, Toolbox};

const MAX_STEPS: usize = 40;
const MAX_READS_IN_A_ROW: usize = 8;
const REPEATED_READ: &str = "You already ran this exact call; its result is above. Don't explore again: act now (edit or write files, delegate) or give your answer.";
const TOO_MANY_READS: &str = "\n\nNote: you have only been reading for a while. Stop exploring: make the change, delegate, or give your answer.";
const READS_BLOCKED: &str = "Refused: you keep reading/searching without acting. Reading is disabled until you act: make the change, delegate, or give your final answer now with what you know.";
/// Web searches per agent: past this, rephrasing the same query rarely finds anything new.
const MAX_SEARCHES: usize = 6;
const SEARCH_LIMIT: &str = "Refused: search limit reached. Use what you already found (or say it was not found) and give your answer.";
const SEARCH_FIRST: &str = "Error: `sampling` must not be guessed. First web_search the recommended settings (temperature, top_p, top_k…) of this exact model for this kind of task, then delegate again with them, or omit `sampling` to use good defaults.";
/// Sub-agents may delegate once more; deeper trees cost a lot and rarely help.
const MAX_DEPTH: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// Ask the user before writing or running anything.
    Ask,
    /// Run everything without checks.
    Auto,
    /// A security reviewer agent approves or refuses each risky action.
    Review,
    /// Read-only investigation ending with a plan.
    Plan,
    /// The agent plans, delegates and picks models itself; risky actions are reviewed.
    Smart,
}

impl Mode {
    pub const ALL: [Mode; 5] = [Mode::Ask, Mode::Auto, Mode::Review, Mode::Plan, Mode::Smart];

    pub fn id(self) -> &'static str {
        match self {
            Mode::Ask => "ask",
            Mode::Auto => "auto",
            Mode::Review => "review",
            Mode::Plan => "plan",
            Mode::Smart => "smart",
        }
    }

    pub fn parse(s: &str) -> Self {
        Self::ALL.into_iter().find(|m| m.id() == s).unwrap_or(Mode::Ask)
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Ask => "Ask",
            Mode::Auto => "Auto",
            Mode::Review => "Auto + security",
            Mode::Plan => "Plan",
            Mode::Smart => "Smart",
        }
    }
}

pub enum AgentEvent {
    /// A sub-agent (or the security reviewer) was created.
    Spawned { parent: Option<usize>, role: String, model: String, task: String },
    Text(String),
    ToolStart { name: String, args: String },
    ToolResult { output: String },
    /// The security reviewer's verdict on a tool call.
    Review { name: String, allowed: bool, reason: String },
    /// Ask the user; answer `true` to run the tool.
    Approval { name: String, args: String, reply: smol::channel::Sender<bool> },
    Effect(SideEffect),
    /// A sub-agent finished.
    Finished { summary: String },
    /// The main agent finished; carries the updated conversation for the next turn.
    Done(Vec<Value>),
    Error(String, Vec<Value>),
}

pub type Events = smol::channel::Sender<(usize, AgentEvent)>;

/// Where a conversation goes.
pub enum Backend {
    Direct(Endpoint),
    /// Copilot accounts in preference order; the next one takes over on rate limits.
    Copilot { accounts: Vec<Arc<Copilot>>, model: String },
    /// In order: the next takes over when one fails before answering anything.
    Chain(Vec<Backend>),
}

/// Try each backend until one answers; the error lists why each failed.
pub fn first_ok<T>(backends: &[Backend], mut f: impl FnMut(&Backend) -> anyhow::Result<T>) -> anyhow::Result<T> {
    let mut errors = vec![];
    for b in backends {
        match f(b) {
            Ok(v) => return Ok(v),
            Err(e) => errors.push(format!("{}: {e}", b.label())),
        }
    }
    Err(anyhow::anyhow!(errors.join("\n")))
}

impl Backend {
    /// Who answers: the model, and the account or server.
    pub fn label(&self) -> String {
        match self {
            Backend::Direct(e) => format!("{} ({})", e.model, e.base_url),
            Backend::Copilot { accounts, model } => format!("{model} ({})", accounts.iter().map(|a| a.login()).collect::<Vec<_>>().join(", ")),
            Backend::Chain(c) => c.first().map(Backend::label).unwrap_or_default(),
        }
    }

    fn chat(&self, messages: &[Value], tools: &[Value], on_text: &mut dyn FnMut(&str)) -> anyhow::Result<Reply> {
        match self {
            Backend::Direct(endpoint) => endpoint.chat(messages, tools, on_text),
            Backend::Copilot { accounts, model } => with_failover(accounts, |a| a.endpoint(model)?.chat(messages, tools, &mut *on_text)),
            Backend::Chain(chain) => {
                // Once text was shown, another model starting over would repeat it: stop there.
                let mut streamed = false;
                let mut errors = vec![];
                for b in chain {
                    match b.chat(messages, tools, &mut |t| { streamed = true; on_text(t) }) {
                        Err(e) if !streamed => errors.push(format!("{}: {e}", b.label())),
                        other => return other,
                    }
                }
                Err(anyhow::anyhow!(errors.join("\n")))
            }
        }
    }

    pub fn complete(&self, messages: &[Value], max_tokens: u32) -> anyhow::Result<String> {
        match self {
            Backend::Direct(endpoint) => endpoint.complete(messages, max_tokens),
            Backend::Copilot { accounts, model } => with_failover(accounts, |a| a.endpoint(model)?.complete(messages, max_tokens)),
            Backend::Chain(chain) => first_ok(chain, |b| b.complete(messages, max_tokens)),
        }
    }
}

/// What the user asks of single agents during a run (from the Agents view).
#[derive(Default)]
pub struct Control {
    stopped: Mutex<std::collections::HashSet<usize>>,
    inbox: Mutex<std::collections::HashMap<usize, Vec<String>>>,
}

impl Control {
    pub fn stop(&self, agent: usize) {
        self.stopped.lock().unwrap().insert(agent);
    }

    pub fn is_stopped(&self, agent: usize) -> bool {
        self.stopped.lock().unwrap().contains(&agent)
    }

    /// Queue a message; the agent reads it before its next step.
    pub fn message(&self, agent: usize, text: String) {
        self.inbox.lock().unwrap().entry(agent).or_default().push(text);
    }

    pub fn take_messages(&self, agent: usize) -> Vec<String> {
        self.inbox.lock().unwrap().remove(&agent).unwrap_or_default()
    }
}

/// Everything shared by the agents of one run.
pub struct Runtime {
    pub events: Events,
    pub cancel: Arc<AtomicBool>,
    pub mode: Mode,
    pub resolver: Resolver,
    pub catalog: Vec<ModelInfo>,
    pub main_model: String,
    /// Model of the security reviewer; empty = the main model.
    pub review_model: String,
    pub delegation: bool,
    pub context_chars: usize,
    pub control: Arc<Control>,
    next_id: AtomicUsize,
    reviewer: Mutex<Option<usize>>,
}

impl Runtime {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        events: Events,
        cancel: Arc<AtomicBool>,
        mode: Mode,
        resolver: Resolver,
        catalog: Vec<ModelInfo>,
        main_model: String,
        review_model: String,
        delegation: bool,
        context_chars: usize,
    ) -> Self {
        Self { events, cancel, mode, resolver, catalog, main_model, review_model, delegation, context_chars, control: Arc::default(), next_id: AtomicUsize::new(1), reviewer: Mutex::new(None) }
    }

    fn send(&self, agent: usize, event: AgentEvent) {
        let _ = self.events.send_blocking((agent, event));
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

pub fn system_prompt(toolbox: &Toolbox, active_file: Option<&str>, mode: Mode, catalog: &[ModelInfo], main_model: &str, delegation: bool) -> String {
    let mut p = format!(
        "You are ForgeCode, a coding agent inside an IDE. Workspace: {}.{}\n\
         Work step by step: explore with list_dir/glob/grep/read_file, change code with edit_file \
         (preferred) or write_file, verify with bash (build, tests). Read a file before editing it. \
         Use web_search/web_fetch for documentation. Keep answers short; use markdown.\n",
        toolbox.workspace.display(),
        active_file.map(|f| format!(" The user is viewing {f}.")).unwrap_or_default(),
    );
    match mode {
        Mode::Plan => p.push_str("PLAN MODE: you can only read. Investigate, then answer with a numbered plan of concrete steps (files, changes, commands). Change nothing.\n"),
        Mode::Smart => {
            p.push_str(
                "SMART MODE:\n\
                 - Small request (question, one function/file): do it yourself, no plan, no delegate.\n\
                 - Big request (new feature, several files): write a numbered plan, then `delegate` each part to its own \
                 sub-agent (several calls in one reply = parallel). Each task: goal, files, how to verify. Then build/test.\n\
                 - Don't read the whole project yourself: read only what the plan needs, let sub-agents read their files.\n\
                 - If the user asks for agents, delegate.\n",
            );
            if catalog.iter().any(|m| m.local) {
                p.push_str("- Local models are weak: give them tiny tasks (one file or function each).\n");
            }
        }
        _ => {}
    }
    if delegation && !catalog.is_empty() {
        p.push_str("\nYou can `delegate` sub-tasks to new agents. They use your model: omit `model`. Pick another model (e.g. copilot/…) only for a very complex task your model cannot handle. Other models:\n");
        for m in catalog.iter().take(25) {
            p.push_str(&m.line());
            p.push('\n');
        }
        if describe(main_model, false).tier == Tier::Small || catalog.iter().any(|m| m.model_ref == main_model && m.tier == Tier::Small) {
            p.push_str("You are a small model: split work into small tasks; use a stronger model only for very complex reasoning.\n");
        }
    }
    let project = crate::dotforge::instructions(&toolbox.workspace, Some("main"));
    if !project.is_empty() {
        p.push_str(&format!("\nProject instructions (.forgecode):\n{project}\n\n"));
    }
    p.push_str("If you cannot call tools natively, reply with exactly one block:\n```tool\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"src/main.rs\"}}\n```");
    p
}

fn role_prompt(role: &str, workspace: &std::path::Path) -> String {
    let job = match role {
        "researcher" => "You research: read code and docs (web_search/web_fetch), then report concise findings with file paths. You cannot modify files.",
        "reviewer" => "You review code for bugs, security issues and missing tests. Report findings with file:line. You cannot modify files.",
        "coder" => "You implement the task: edit files, then build/test to verify. Report what you changed.",
        _ => "You complete the task you are given and report the result concisely.",
    };
    let mut p = format!("You are a ForgeCode sub-agent. Workspace: {}. {job} Your final message is returned to the agent that delegated to you.", workspace.display());
    let project = crate::dotforge::instructions(workspace, Some(role));
    if !project.is_empty() {
        p.push_str(&format!("\n\nProject instructions (.forgecode):\n{project}"));
    }
    p
}

fn delegate_tool(catalog: &[ModelInfo]) -> Value {
    let models: Vec<&str> = catalog.iter().map(|m| m.model_ref.as_str()).collect();
    json!({"type": "function", "function": {
        "name": "delegate",
        "description": "Give a self-contained sub-task to a new agent and get its result. Roles: researcher (read-only, web), coder (edits files), reviewer (read-only review), general.",
        "parameters": {"type": "object", "properties": {
            "task": {"type": "string", "description": "Complete instructions; the agent sees nothing else"},
            "role": {"type": "string", "enum": ["researcher", "coder", "reviewer", "general"]},
            "model": {"type": "string", "description": format!("Omit to use your own model (default). Only for very complex tasks, one of: {}", models.join(", "))},
            "sampling": {"type": "object", "description": "Optional generation settings for this agent. Omit to use good defaults. Only set values you found with web_search for this exact model and task (never guess).",
                "properties": {"temperature": {"type": "number"}, "top_p": {"type": "number"}, "top_k": {"type": "integer"}, "min_p": {"type": "number"},
                    "repeat_penalty": {"type": "number"}, "presence_penalty": {"type": "number"}, "max_tokens": {"type": "integer"}}}
        }, "required": ["task"]}
    }})
}

/// Remove tool calls without a name or id (left by older versions) and their results: the APIs reject them.
pub fn sanitize(history: &mut Vec<Value>) {
    for m in history.iter_mut() {
        if let Some(calls) = m.get_mut("tool_calls").and_then(Value::as_array_mut) {
            calls.retain(|c| !c["id"].as_str().unwrap_or("").is_empty() && !c["function"]["name"].as_str().unwrap_or("").is_empty());
        }
    }
    history.retain(|m| m["role"] != "tool" || !m["tool_call_id"].as_str().unwrap_or("").is_empty());
}

/// Run the main agent (id 0) on `history`.
pub fn run(rt: Arc<Runtime>, mut toolbox: Toolbox, mut history: Vec<Value>) {
    sanitize(&mut history);
    if rt.mode == Mode::Plan {
        toolbox.allowed = Some(READ_ONLY.to_vec());
    }
    let purpose = if matches!(rt.mode, Mode::Smart | Mode::Plan) { Purpose::Orchestrate } else { Purpose::Code };
    let backend = match rt.resolver.backend_for(&rt.main_model, purpose, Default::default()) {
        Ok(b) => b,
        Err(e) => return rt.send(0, AgentEvent::Error(e.to_string(), history)),
    };
    match run_agent(&rt, 0, 0, &backend, &toolbox, history) {
        Ok(history) => rt.send(0, AgentEvent::Done(history)),
        Err((e, history)) => rt.send(0, AgentEvent::Error(e, history)),
    }
}

fn run_agent(rt: &Runtime, id: usize, depth: usize, backend: &Backend, toolbox: &Toolbox, mut history: Vec<Value>) -> Result<Vec<Value>, (String, Vec<Value>)> {
    let mut tools = toolbox.definitions();
    if rt.delegation && depth < MAX_DEPTH {
        tools.push(delegate_tool(&rt.catalog));
    }
    let mut native_tools = true;
    // Small models tend to read the same files over and over: track it to push them to act.
    let mut seen_reads = std::collections::HashSet::new();
    let mut reads_in_a_row = 0;
    let mut searches = 0;
    // Sub-agent settings may only be chosen after looking them up.
    let mut searched = false;

    for _ in 0..MAX_STEPS {
        if rt.cancelled() || rt.control.is_stopped(id) {
            return Ok(history);
        }
        for text in rt.control.take_messages(id) {
            history.push(json!({"role": "user", "content": format!("Message from the user while you work: {text}")}));
        }
        let messages = trim_history(&history, rt.context_chars);
        let tool_defs: &[Value] = if native_tools { &tools } else { &[] };
        let reply = match backend.chat(&messages, tool_defs, &mut |t| rt.send(id, AgentEvent::Text(t.into()))) {
            Ok(r) => r,
            // Some local servers reject the `tools` field: retry once with the text protocol.
            Err(e) if native_tools && e.to_string().contains("tool") => {
                native_tools = false;
                continue;
            }
            Err(e) => return Err((e.to_string(), history)),
        };

        let mut calls = reply.tool_calls.clone();
        let native = !calls.is_empty();
        let truncated = reply.truncated;
        if !native {
            calls.extend(parse_text_tool_call(&reply.content));
        }
        let mut msg = json!({"role": "assistant", "content": reply.content});
        if let Some(native) = reply.native {
            msg["native"] = native;
        }
        if native {
            msg["tool_calls"] = calls.iter().map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.arguments}})).collect();
        }
        history.push(msg);
        if calls.is_empty() {
            return Ok(history);
        }

        // Delegations of one reply run in parallel; other tools run in order.
        let can_delegate = rt.delegation && depth < MAX_DEPTH && !truncated;
        let parallel: Vec<(usize, String)> = {
            let jobs: Vec<(usize, Value)> = calls
                .iter()
                .enumerate()
                .filter(|(_, c)| c.name == "delegate" && can_delegate)
                .filter_map(|(i, c)| serde_json::from_str::<Value>(&c.arguments).ok().map(|a| (i, a)))
                .collect();
            for (i, _) in &jobs {
                rt.send(id, AgentEvent::ToolStart { name: calls[*i].name.clone(), args: calls[*i].arguments.clone() });
            }
            std::thread::scope(|scope| {
                let handles: Vec<_> = jobs.iter().map(|(i, args)| (*i, scope.spawn(move || delegate(rt, id, depth, toolbox, args, searched)))).collect();
                handles.into_iter().map(|(i, h)| (i, h.join().unwrap_or_else(|_| "Error: the sub-agent crashed".into()))).collect()
            })
        };
        for (i, call) in calls.into_iter().enumerate() {
            let output = match parallel.iter().find(|(j, _)| *j == i) {
                Some((_, out)) => out.clone(),
                None => {
                    rt.send(id, AgentEvent::ToolStart { name: call.name.clone(), args: call.arguments.clone() });
                    let read = READ_ONLY.contains(&call.name.as_str());
                    let search = call.name == "web_search";
                    if read && reads_in_a_row >= 2 * MAX_READS_IN_A_ROW {
                        READS_BLOCKED.to_string()
                    } else if read && !seen_reads.insert((call.name.clone(), call.arguments.split_whitespace().collect::<String>().to_lowercase())) {
                        reads_in_a_row += 1;
                        REPEATED_READ.to_string()
                    } else if search && searches >= MAX_SEARCHES {
                        reads_in_a_row += 1;
                        SEARCH_LIMIT.to_string()
                    } else {
                        searches += search as usize;
                        searched |= search || call.name == "web_fetch";
                        let out = run_call(rt, id, depth, toolbox, &call, truncated, searched);
                        if read {
                            reads_in_a_row += 1;
                        } else {
                            // Files may have changed: reading them again is legitimate.
                            seen_reads.clear();
                            reads_in_a_row = 0;
                        }
                        if reads_in_a_row == MAX_READS_IN_A_ROW {
                            reads_in_a_row = 0;
                            format!("{out}{TOO_MANY_READS}")
                        } else {
                            out
                        }
                    }
                }
            };
            rt.send(id, AgentEvent::ToolResult { output: output.clone() });
            history.push(if native {
                json!({"role": "tool", "tool_call_id": call.id, "content": output})
            } else {
                json!({"role": "user", "content": format!("Result of {}:\n{output}", call.name)})
            });
            if rt.cancelled() || rt.control.is_stopped(id) {
                return Ok(history);
            }
        }
    }
    Err((format!("stopped after {MAX_STEPS} steps"), history))
}

/// Execute one (non-parallel) tool call and return what goes back to the model.
fn run_call(rt: &Runtime, id: usize, depth: usize, toolbox: &Toolbox, call: &ToolCall, truncated: bool, searched: bool) -> String {
    // A cut-off reply would write half a file: refuse and ask for smaller steps.
    if truncated {
        return "Error: your reply hit the output limit, so this call is incomplete and was not run. Make smaller changes: create the file in parts or use edit_file for each section.".into();
    }
    let args = match serde_json::from_str::<Value>(if call.arguments.trim().is_empty() { "{}" } else { &call.arguments }) {
        Ok(a) => a,
        Err(e) => return format!("Error: invalid JSON arguments ({e}); call the tool again with valid JSON."),
    };
    if call.name == "delegate" && rt.delegation && depth < MAX_DEPTH {
        return delegate(rt, id, depth, toolbox, &args, searched);
    }
    if let Some(refusal) = gate(rt, id, toolbox, call) {
        return refusal;
    }
    let (out, effect) = toolbox.run(&call.name, &args);
    if effect != SideEffect::None {
        rt.send(id, AgentEvent::Effect(effect));
    }
    out
}

/// None = the call may run; Some(message) = refused, the message goes back to the model.
fn gate(rt: &Runtime, id: usize, toolbox: &Toolbox, call: &ToolCall) -> Option<String> {
    if !toolbox.needs_approval(&call.name) || !toolbox.is_allowed(&call.name) {
        return None; // read-only, or refused by `run` itself with a clear error
    }
    match rt.mode {
        Mode::Auto => None,
        // Like VS Code: file edits go in at once and wait in the editor for Keep / Undo,
        // which is the approval (commands and the rest are still asked first).
        Mode::Ask if matches!(call.name.as_str(), "edit_file" | "write_file") => None,
        Mode::Ask | Mode::Plan => {
            let (tx, rx) = smol::channel::bounded(1);
            rt.send(id, AgentEvent::Approval { name: call.name.clone(), args: call.arguments.clone(), reply: tx });
            match rx.recv_blocking() {
                Ok(true) => None,
                _ => Some("The user denied this action.".into()),
            }
        }
        Mode::Review | Mode::Smart => {
            let (allowed, reason) = review(rt, id, toolbox, call);
            rt.send(id, AgentEvent::Review { name: call.name.clone(), allowed, reason: reason.clone() });
            (!allowed).then(|| format!("Denied by the security reviewer: {reason}"))
        }
    }
}

const SECURITY_PROMPT: &str = "You are a security reviewer for a coding agent working in a local project. \
Approve normal development actions (editing project files, building, running tests, git commits, installing project dependencies). \
Deny actions that: delete or overwrite data outside the project, read or send secrets/credentials, upload data to the network, \
disable security, rewrite git history on remotes (force push), run obfuscated or downloaded scripts, or are unrelated to the task. \
Answer with exactly one line: `ALLOW` or `DENY: <short reason>`.";

/// Ask the reviewer agent about a risky call. Fails closed: no verdict means no.
fn review(rt: &Runtime, id: usize, toolbox: &Toolbox, call: &ToolCall) -> (bool, String) {
    let model = super::models::security_model(&rt.catalog, &rt.review_model, &rt.main_model);
    let reviewer = *rt.reviewer.lock().unwrap().get_or_insert_with(|| {
        let rid = rt.next_id.fetch_add(1, Ordering::Relaxed);
        rt.send(rid, AgentEvent::Spawned { parent: Some(0), role: "security".into(), model: model.clone(), task: "Review risky actions".into() });
        rid
    });
    let rules = crate::dotforge::instructions(&toolbox.workspace, Some("security"));
    let question = format!(
        "Workspace: {}\n{}Agent #{id} wants to call `{}` with:\n{}",
        toolbox.workspace.display(),
        if rules.is_empty() { String::new() } else { format!("Project rules:\n{rules}\n") },
        call.name,
        call.arguments
    );
    rt.send(reviewer, AgentEvent::ToolStart { name: call.name.clone(), args: call.arguments.clone() });
    let verdict = rt
        .resolver
        .backend(&model)
        .and_then(|b| b.complete(&[json!({"role": "system", "content": SECURITY_PROMPT}), json!({"role": "user", "content": question})], 120));
    let (allowed, reason) = match verdict {
        Ok(text) => parse_verdict(&text),
        Err(e) => (false, format!("reviewer unavailable: {e}")),
    };
    rt.send(reviewer, AgentEvent::ToolResult { output: if allowed { "ALLOW".into() } else { format!("DENY: {reason}") } });
    (allowed, reason)
}

/// `ALLOW` / `DENY: reason`, tolerant of formatting from small models.
pub fn parse_verdict(text: &str) -> (bool, String) {
    let line = text.trim().trim_matches(|c| c == '`' || c == '*').trim();
    let upper = line.to_uppercase();
    if upper.starts_with("ALLOW") || upper.starts_with("APPROVE") {
        return (true, "approved".into());
    }
    let reason = line.split_once(':').map_or(line, |(_, r)| r).trim();
    (false, if reason.is_empty() { "no reason given".into() } else { reason.to_string() })
}

/// Run a sub-agent for `args.task` and return its final answer.
fn delegate(rt: &Runtime, parent: usize, depth: usize, toolbox: &Toolbox, args: &Value, searched: bool) -> String {
    let Some(task) = args["task"].as_str().filter(|t| !t.trim().is_empty()) else {
        return "Error: `task` is required".into();
    };
    let chosen = sampling::from_json(&args["sampling"]);
    if !chosen.is_empty() && !searched {
        return SEARCH_FIRST.into();
    }
    let role = args["role"].as_str().unwrap_or("general").to_string();
    let model = args["model"]
        .as_str()
        .filter(|m| rt.catalog.iter().any(|c| c.model_ref == *m) || rt.resolver.backend(m).is_ok())
        .map(String::from)
        .unwrap_or_else(|| rt.main_model.clone());
    let id = rt.next_id.fetch_add(1, Ordering::Relaxed);
    rt.send(id, AgentEvent::Spawned { parent: Some(parent), role: role.clone(), model: model.clone(), task: task.to_string() });
    let backend = match rt.resolver.backend_for(&model, Purpose::for_role(&role), chosen) {
        Ok(b) => b,
        Err(e) => {
            rt.send(id, AgentEvent::Finished { summary: format!("error: {e}") });
            return format!("Error: cannot start the {role} agent on {model}: {e}");
        }
    };
    let mut sub_tools = toolbox.clone();
    if matches!(role.as_str(), "researcher" | "reviewer") {
        sub_tools.allowed = Some(READ_ONLY.to_vec());
    }
    let history = vec![json!({"role": "system", "content": role_prompt(&role, &toolbox.workspace)}), json!({"role": "user", "content": task})];
    let (history, error) = match run_agent(rt, id, depth + 1, &backend, &sub_tools, history) {
        Ok(h) => (h, None),
        Err((e, h)) => (h, Some(e)),
    };
    let answer = history.iter().rev().find(|m| m["role"] == "assistant").and_then(|m| m["content"].as_str()).unwrap_or("").to_string();
    if rt.control.is_stopped(id) {
        rt.send(id, AgentEvent::Finished { summary: "stopped".into() });
        return format!("The user stopped the {role} agent ({model}) before it finished. Don't restart it unless asked.\nPartial answer:\n{answer}");
    }
    rt.send(id, AgentEvent::Finished { summary: error.clone().unwrap_or_else(|| answer.chars().take(160).collect()) });
    match error {
        Some(e) => format!("The {role} agent ({model}) failed: {e}\nPartial answer:\n{answer}"),
        None => format!("Result from the {role} agent ({model}):\n{answer}"),
    }
}

/// Keep what fits in `budget` characters: the system prompt, the request being worked on (servers
/// reject conversations without a user message), and the newest steps. Never starts on an orphan
/// tool result; the latest step is always kept, its tool outputs shortened if they are too long.
pub fn trim_history(history: &[Value], budget: usize) -> Vec<Value> {
    let size = |m: &Value| m.to_string().len();
    let Some((system, rest)) = history.split_first() else { return vec![] };
    let is_request = |m: &Value| m["role"] == "user" && !m["content"].as_str().is_some_and(|c| c.starts_with("Result of "));
    let anchor = rest.iter().rposition(is_request);
    let first = anchor.map_or(0, |a| a + 1);
    let mut used = size(system) + anchor.map_or(0, |a| size(&rest[a]));
    let mut start = rest.len();
    while start > first && used + size(&rest[start - 1]) <= budget {
        start -= 1;
        used += size(&rest[start]);
    }
    while start < rest.len() && rest[start]["role"] == "tool" {
        start += 1;
    }
    if start == rest.len() && rest.len() > first {
        // Even the latest step is too big: keep it whole (from its assistant message) and shorten it.
        start = rest[first..].iter().rposition(|m| m["role"] != "tool").map_or(first, |i| first + i);
    }
    let mut out = vec![system.clone()];
    if let Some(a) = anchor.filter(|a| *a < start) {
        out.push(rest[a].clone());
    }
    out.extend(rest[start..].iter().cloned());
    shorten_tool_outputs(&mut out, budget);
    out
}

/// Cut tool results evenly until the conversation fits `budget` (with a floor so they stay useful).
fn shorten_tool_outputs(messages: &mut [Value], budget: usize) {
    let total: usize = messages.iter().map(|m| m.to_string().len()).sum();
    let tools: Vec<usize> = (0..messages.len()).filter(|&i| messages[i]["role"] == "tool").collect();
    if total <= budget || tools.is_empty() {
        return;
    }
    let tool_size: usize = tools.iter().map(|&i| messages[i]["content"].as_str().map_or(0, str::len)).sum();
    let room = budget.saturating_sub(total - tool_size);
    let cap = (room / tools.len()).max(1500);
    for i in tools {
        if let Some(c) = messages[i]["content"].as_str().filter(|c| c.len() > cap) {
            let cut = (0..=cap).rev().find(|&n| c.is_char_boundary(n)).unwrap_or(0);
            messages[i]["content"] = json!(format!("{}\n… (cut to fit the context; read a smaller range if you need the rest)", &c[..cut]));
        }
    }
}

/// Small models without native tool calling answer with a ```tool {json}``` block (or bare JSON).
pub fn parse_text_tool_call(content: &str) -> Option<ToolCall> {
    let body = content
        .split("```tool")
        .nth(1)
        .or_else(|| content.split("```json").nth(1))
        .map(|b| b.split("```").next().unwrap_or(b))
        .unwrap_or(content);
    let start = body.find('{')?;
    let end = body.rfind('}')?;
    let v: Value = serde_json::from_str(body.get(start..=end)?).ok()?;
    let name = v["name"].as_str().or(v["tool"].as_str())?.to_string();
    let args = v.get("arguments").or(v.get("args")).cloned().unwrap_or(json!({}));
    Some(ToolCall { id: "text".into(), name, arguments: args.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Agents may tune a sub-agent's sampling only after looking the values up.
    #[test]
    fn chosen_sampling_requires_a_search() {
        let (tx, rx) = smol::channel::unbounded();
        let resolver = Resolver { providers: vec![], copilot: vec![] };
        let rt = Runtime::new(tx, Default::default(), Mode::Auto, resolver, vec![], "x/m".into(), String::new(), true, 10_000);
        let toolbox = Toolbox { workspace: std::env::temp_dir(), plugin_tools: vec![], allowed: None, checkpoint: None };
        let args = json!({"task": "t", "model": "x/m", "sampling": {"temperature": 0.1}});
        assert_eq!(delegate(&rt, 0, 0, &toolbox, &args, false), SEARCH_FIRST);
        assert!(rx.try_recv().is_err(), "no agent was started");
        // After a search it goes ahead (and fails here only because the provider does not exist).
        assert!(delegate(&rt, 0, 0, &toolbox, &args, true).contains("cannot start"));
    }

    #[test]
    fn parses_text_tool_calls() {
        let c = parse_text_tool_call("Sure.\n```tool\n{\"name\":\"grep\",\"arguments\":{\"pattern\":\"fn main\"}}\n```").unwrap();
        assert_eq!(c.name, "grep");
        assert_eq!(c.arguments, r#"{"pattern":"fn main"}"#);
        assert!(parse_text_tool_call("Just an answer, no tools.").is_none());
    }

    #[test]
    fn trims_from_the_front_and_skips_orphan_tool_results() {
        let h = vec![
            json!({"role":"system","content":"s"}),
            json!({"role":"user","content":"x".repeat(100)}),
            json!({"role":"tool","content":"y"}),
            json!({"role":"user","content":"last"}),
        ];
        let t = trim_history(&h, 80);
        assert_eq!(t.len(), 2);
        assert_eq!(t[1]["content"], "last");
    }

    /// A batch of big file reads used to leave only the system prompt, which servers reject (HTTP 400).
    #[test]
    fn trimming_keeps_the_request_and_the_latest_step() {
        let call = |id: &str| json!({"id": id, "type": "function", "function": {"name": "read_file", "arguments": "{}"}});
        let h = vec![
            json!({"role":"system","content":"s"}),
            json!({"role":"user","content":"analyse the project"}),
            json!({"role":"assistant","content":"","tool_calls":[call("a")]}),
            json!({"role":"tool","tool_call_id":"a","content":"old ".repeat(2000)}),
            json!({"role":"assistant","content":"","tool_calls":[call("b"), call("c")]}),
            json!({"role":"tool","tool_call_id":"b","content":"x".repeat(20_000)}),
            json!({"role":"tool","tool_call_id":"c","content":"y".repeat(20_000)}),
        ];
        let t = trim_history(&h, 10_000);
        let roles: Vec<&str> = t.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool", "tool"]);
        assert_eq!(t[1]["content"], "analyse the project");
        assert!(t[3]["content"].as_str().unwrap().len() < 5_000, "outputs were shortened");
    }

    #[test]
    fn reviewer_verdicts_fail_closed() {
        assert_eq!(parse_verdict("ALLOW").0, true);
        assert_eq!(parse_verdict("**Allow**").0, true);
        assert_eq!(parse_verdict("DENY: deletes ~/.ssh"), (false, "deletes ~/.ssh".to_string()));
        assert_eq!(parse_verdict("I think this is fine").0, false, "anything unclear is a no");
    }

    #[test]
    fn sanitizes_broken_tool_calls() {
        let mut h = vec![
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "", "function": {"name": ""}}, {"id": "a", "function": {"name": "bash"}}]}),
            json!({"role": "tool", "tool_call_id": "", "content": "x"}),
            json!({"role": "tool", "tool_call_id": "a", "content": "ok"}),
        ];
        sanitize(&mut h);
        assert_eq!(h[0]["tool_calls"].as_array().unwrap().len(), 1);
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn modes_roundtrip() {
        for m in Mode::ALL {
            assert_eq!(Mode::parse(m.id()), m);
        }
        assert_eq!(Mode::parse("bogus"), Mode::Ask);
    }
}
