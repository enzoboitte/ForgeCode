//! Persistent user settings, stored as JSON in the config directory.

use std::path::PathBuf;

use gpui_kit::component::theme::{Theme, ThemeMode, ThemeRegistry};
use gpui_kit::{App, Global, px};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// "en" or "fr".
    pub language: String,
    /// "light", "dark" or "system".
    pub theme_mode: String,
    /// Theme names from the registry (built-in or plugin-provided); empty = default.
    pub light_theme: String,
    pub dark_theme: String,
    pub ui_font_size: f32,
    pub editor_font_size: f32,
    pub editor_font_family: String,
    pub tab_size: usize,
    pub soft_wrap: bool,
    pub format_on_save: bool,
    /// Save files this many ms after the last edit; 0 = off. (New name so older "off" settings don't carry over.)
    pub autosave_ms: u64,

    /// Copilot ghost-text suggestions while typing.
    pub copilot_inline: bool,
    /// Copilot model used for ghost text (a fast one).
    pub completion_model: String,
    /// GitHub account used for Copilot: a login, or "auto" to spread load across accounts.
    pub copilot_account: String,

    /// Chat model as "<provider id>/<model id>"; provider "copilot" is built in.
    pub chat_model: String,
    /// User-configured model servers (llama.cpp, Ollama, LM Studio, Claude API…).
    pub providers: Vec<Provider>,
    /// Character budget of the conversation sent to the model; small models need small contexts.
    pub ai_context_chars: usize,
    /// Agent mode id: ask, auto, review, plan, smart.
    pub agent_mode: String,
    /// Security reviewer model ("<provider>/<model>"); empty = the chat model.
    pub review_model: String,
    /// Let agents delegate sub-tasks to other agents/models.
    pub agent_delegation: bool,
    /// Folder the extension marketplace reads from; empty = `<config>/marketplace`.
    pub marketplace_dir: String,
    /// What the AI panel runs: "classic" (the built-in chat) or "agent" (an agent module, Hermes).
    pub chat_engine: String,
    /// Models agent modules (Hermes) may use, as "<provider>/<model>"; served by the local gateway.
    pub agent_models: Vec<String>,
    /// The agent's model when a conversation starts; empty = the first allowed one.
    pub agent_model: String,
    /// Context window given to the agent, in tokens: older turns are compressed beyond it.
    pub agent_context_tokens: u32,
    /// How much the agent asks: one of its own modes, or "auto_safe" (edits allowed, commands
    /// allowed when ForgeCode judges them harmless).
    pub agent_permission_mode: String,
    /// After each Hermes turn, a second model reviews what it did (hallucinations, tests not run…).
    pub agent_supervisor: bool,
    /// The supervisor's model ("<provider>/<model>", one Hermes may use); empty = Hermes' own.
    pub agent_supervisor_model: String,
    /// Per-language run, debug and type checking options, keyed by [`lang_key`].
    pub languages: std::collections::BTreeMap<String, LangSettings>,
}

/// Run, debug and analysis options of one language.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LangSettings {
    /// "off", "basic", "standard" or "strict".
    pub type_checking: String,
    /// Interpreter or runtime (python, php, node…); empty = found automatically.
    pub runtime: String,
    /// Arguments passed to the program when running or debugging it.
    pub args: String,
    pub stop_on_entry: bool,
    /// Python: step only through the project's code, not libraries.
    pub just_my_code: bool,
}

impl Default for LangSettings {
    fn default() -> Self {
        Self { type_checking: "standard".into(), runtime: String::new(), args: String::new(), stop_on_entry: false, just_my_code: true }
    }
}

/// Languages sharing options: JSX with JS, TSX with TS, headers with their language.
pub fn lang_key(lsp_id: &str) -> &str {
    match lsp_id {
        "javascriptreact" => "javascript",
        "typescriptreact" => "typescript",
        other => other,
    }
}

impl Settings {
    pub fn lang(&self, lsp_id: &str) -> LangSettings {
        self.languages.get(lang_key(lsp_id)).cloned().unwrap_or_default()
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: crate::i18n::system_language().into(),
            theme_mode: "dark".into(),
            light_theme: SOFT_LIGHT.into(),
            dark_theme: SOFT_DARK.into(),
            ui_font_size: 14.,
            editor_font_size: 14.,
            editor_font_family: "Menlo".into(),
            tab_size: 4,
            soft_wrap: false,
            format_on_save: false,
            autosave_ms: 1000,
            copilot_inline: true,
            completion_model: "gpt-4.1".into(),
            copilot_account: "auto".into(),
            chat_model: "copilot/gpt-4.1".into(),
            providers: vec![Provider {
                id: "ollama".into(),
                name: "Ollama".into(),
                kind: ProviderKind::OpenAi,
                base_url: "http://localhost:11434/v1".into(),
                api_key: String::new(),
                models: vec![],
                sampling: Sampling::default(),
            }],
            ai_context_chars: 24_000,
            agent_mode: "ask".into(),
            review_model: String::new(),
            agent_delegation: true,
            marketplace_dir: String::new(),
            chat_engine: "agent".into(),
            agent_models: vec![],
            agent_model: String::new(),
            agent_context_tokens: 250_000,
            agent_permission_mode: "default".into(),
            agent_supervisor: true,
            agent_supervisor_model: String::new(),
            languages: Default::default(),
        }
    }
}

impl Global for Settings {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// Any OpenAI-compatible `/chat/completions` server.
    #[default]
    OpenAi,
    /// Anthropic Messages API (Claude).
    Anthropic,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: ProviderKind,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    /// Last fetched model list.
    #[serde(default)]
    pub models: Vec<String>,
    /// Generation settings forced by the user; unset ones use the model's recommended values.
    #[serde(default, skip_serializing_if = "Sampling::is_empty")]
    pub sampling: Sampling,
}

/// Generation (sampling) parameters; `None` = not sent / chosen automatically.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sampling {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

impl Sampling {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Values of `self`, completed with `base` where unset.
    pub fn or(self, base: Sampling) -> Sampling {
        Sampling {
            temperature: self.temperature.or(base.temperature),
            top_p: self.top_p.or(base.top_p),
            top_k: self.top_k.or(base.top_k),
            min_p: self.min_p.or(base.min_p),
            repeat_penalty: self.repeat_penalty.or(base.repeat_penalty),
            presence_penalty: self.presence_penalty.or(base.presence_penalty),
            max_tokens: self.max_tokens.or(base.max_tokens),
        }
    }
}

/// Split "<provider>/<model>".
pub fn split_model(model_ref: &str) -> (&str, &str) {
    model_ref.split_once('/').unwrap_or(("copilot", model_ref))
}

pub const SOFT_DARK: &str = "ForgeCode Dark Soft";
pub const SOFT_LIGHT: &str = "ForgeCode Light Soft";

pub fn config_dir() -> PathBuf {
    // Tests save settings, chats and accounts too: never over the user's own.
    #[cfg(test)]
    let base = std::env::temp_dir().join(format!("forgecode-test-config-{}", std::process::id()));
    #[cfg(not(test))]
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("forgecode");
    // Settings, accounts and modules from before the rename.
    if !dir.exists() && base.join("forge").is_dir() {
        let _ = std::fs::rename(base.join("forge"), &dir);
    }
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

/// The two layers behind the effective [`Settings`]: the global file and the project's
/// `.forgecode/settings.json`, whose keys win.
#[derive(Default)]
pub struct Layers {
    pub base: Settings,
    pub project_root: Option<PathBuf>,
    pub project: serde_json::Map<String, serde_json::Value>,
}

impl Global for Layers {}

/// `base` with the project's keys applied (unknown or invalid keys are ignored).
pub fn merge(base: &Settings, project: &serde_json::Map<String, serde_json::Value>) -> Settings {
    let mut v = serde_json::to_value(base).unwrap_or_default();
    if let Some(obj) = v.as_object_mut() {
        for (k, val) in project {
            if obj.contains_key(k) {
                obj.insert(k.clone(), val.clone());
            }
        }
    }
    serde_json::from_value(v).unwrap_or_else(|_| base.clone())
}

impl Settings {
    pub fn load() -> Self {
        std::fs::read_to_string(settings_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Install the global settings (startup).
    pub fn init(cx: &mut App) {
        let base = Self::load();
        cx.set_global(base.clone());
        cx.set_global(Layers { base, ..Default::default() });
    }

    /// Switch the project layer to `root`'s `.forgecode/settings.json`.
    pub fn set_project(cx: &mut App, root: Option<&std::path::Path>) {
        let project = root.map(crate::dotforge::overrides).unwrap_or_default();
        let layers = cx.global_mut::<Layers>();
        layers.project_root = root.map(PathBuf::from);
        layers.project = project;
        let effective = merge(&layers.base, &layers.project);
        cx.set_global(effective);
        apply_theme(cx);
        cx.refresh_windows();
    }

    pub fn get(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    /// Edit, persist and re-apply. A changed key goes to the project file if the project sets it,
    /// otherwise to the global file.
    pub fn update(cx: &mut App, f: impl FnOnce(&mut Self)) {
        let old = serde_json::to_value(cx.global::<Self>()).unwrap_or_default();
        let mut s = cx.global::<Self>().clone();
        f(&mut s);
        let new = serde_json::to_value(&s).unwrap_or_default();
        let layers = cx.global_mut::<Layers>();
        let mut base = serde_json::to_value(&layers.base).unwrap_or_default();
        let mut project_changed = false;
        if let (Some(old), Some(new)) = (old.as_object(), new.as_object()) {
            for (k, v) in new.iter().filter(|(k, v)| old.get(*k) != Some(*v)) {
                if layers.project.contains_key(k) {
                    layers.project.insert(k.clone(), v.clone());
                    project_changed = true;
                } else {
                    base[k] = v.clone();
                }
            }
        }
        layers.base = serde_json::from_value(base).unwrap_or_else(|_| layers.base.clone());
        if let Ok(json) = serde_json::to_string_pretty(&layers.base) {
            // API keys live here: keep the file private.
            let _ = crate::util::write_private(&settings_path(), json.as_bytes());
        }
        if let (true, Some(root)) = (project_changed, layers.project_root.clone()) {
            let _ = crate::dotforge::write_overrides(&root, &layers.project);
        }
        let effective = merge(&layers.base, &layers.project);
        cx.set_global(effective);
        apply_theme(cx);
        cx.refresh_windows();
    }
}

/// Push theme mode, named themes and font sizes into the component theme.
pub fn apply_theme(cx: &mut App) {
    // Labels the editor's popovers show, in the interface language.
    cx.set_global(gpui_kit::component::input::ProblemLabels { quick_fix: crate::i18n::t("Quick Fix…").into() });
    let s = Settings::get(cx).clone();
    crate::i18n::set_language(&s.language);
    let registry = ThemeRegistry::global(cx);
    let named = |name: &str, soft: &str| registry.themes().get(if name.is_empty() { soft } else { name }).cloned();
    let (light, dark) = (named(&s.light_theme, SOFT_LIGHT), named(&s.dark_theme, SOFT_DARK));
    {
        let theme = Theme::global_mut(cx);
        if let Some(light) = light {
            theme.light_theme = light;
        }
        if let Some(dark) = dark {
            theme.dark_theme = dark;
        }
    }
    match s.theme_mode.as_str() {
        "system" => Theme::sync_system_appearance(None, cx),
        "light" => Theme::change(ThemeMode::Light, None, cx),
        _ => Theme::change(ThemeMode::Dark, None, cx),
    }
    let theme = Theme::global_mut(cx);
    theme.font_size = px(s.ui_font_size);
    theme.mono_font_size = px(s.editor_font_size);
    theme.mono_font_family = s.editor_font_family.clone().into();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let s: Settings = serde_json::from_str(r#"{"theme_mode":"light"}"#).unwrap();
        assert_eq!(s.theme_mode, "light");
        assert_eq!(s.tab_size, Settings::default().tab_size);
    }

    #[test]
    fn project_keys_override_global_ones() {
        let base = Settings::default();
        let mut project = serde_json::Map::new();
        project.insert("agent_mode".into(), "plan".into());
        project.insert("not_a_setting".into(), 1.into());
        let s = merge(&base, &project);
        assert_eq!(s.agent_mode, "plan");
        assert_eq!(s.tab_size, base.tab_size);
    }

    #[test]
    fn splits_model_refs() {
        assert_eq!(split_model("copilot/gpt-4.1"), ("copilot", "gpt-4.1"));
        assert_eq!(split_model("lm/qwen/qwen3-8b"), ("lm", "qwen/qwen3-8b"));
    }
}
