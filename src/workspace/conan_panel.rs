//! The Conan sidebar (Conan module): the project's dependencies, the remotes (added by their link)
//! and every package they offer, searchable, added with one click. The module's script does the
//! work and answers in JSON.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde::Deserialize;

use crate::i18n::t;
use crate::plugins::Plugins;

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub remote: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Remote {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Deserialize)]
struct Requires {
    file: Option<String>,
    requires: Vec<String>,
}

pub enum ConanEvent {
    /// Run the module's script in the Output panel (long: install, CMake): (title, arguments).
    Run(String, Vec<String>),
    Open(PathBuf),
}

/// The module's script and the Python to run it with.
pub fn script(cx: &App) -> Option<(PathBuf, PathBuf)> {
    let plugin = Plugins::global(cx).conan()?;
    let python = ["python3", "python", "py"].iter().find_map(|p| crate::lsp::install::which(p))?;
    Some((python, plugin.dir.join("conan_module.py")))
}

/// Run the script in the project; its standard output, or its error.
fn call(python: &Path, script: &Path, root: &Path, args: &[&str]) -> Result<String> {
    let out = crate::util::command(python).arg(script).args(args).current_dir(root).env("FORGE_WORKSPACE", root).output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("{}", err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("conan failed").trim_start_matches("Erreur : ")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The last line of the output is the JSON answer (Conan may print before it).
fn answer<T: serde::de::DeserializeOwned>(out: &str) -> Result<T> {
    Ok(serde_json::from_str(out.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("null"))?)
}

pub struct ConanPanel {
    pub root: PathBuf,
    file: Option<String>,
    requires: Vec<String>,
    remotes: Vec<Remote>,
    catalog: Vec<Package>,
    query: Entity<InputState>,
    link: Entity<InputState>,
    loading: bool,
    loading_catalog: bool,
    error: Option<String>,
    list: UniformListScrollHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<ConanEvent> for ConanPanel {}

impl ConanPanel {
    pub fn new(root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder(t("Search a package…")));
        let link = cx.new(|cx| InputState::new(window, cx).placeholder(t("Remote link (Conan server or Git recipes)")));
        let subs = vec![cx.subscribe(&query, |_, _, e: &InputEvent, cx| {
            if matches!(e, InputEvent::Change) {
                cx.notify();
            }
        })];
        Self { root, file: None, requires: vec![], remotes: vec![], catalog: vec![], query, link, loading: false, loading_catalog: false, error: None, list: UniformListScrollHandle::new(), _subs: subs }
    }

    pub fn set_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.root = root;
        self.refresh(false, cx);
    }

    /// Read the dependencies and remotes; the catalog too (from its day-long cache unless `fresh`).
    pub fn refresh(&mut self, fresh: bool, cx: &mut Context<Self>) {
        let Some((python, script)) = script(cx) else { return };
        let root = self.root.clone();
        self.loading = true;
        cx.notify();
        let (p2, s2, r2) = (python.clone(), script.clone(), root.clone());
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let requires: Requires = answer(&call(&python, &script, &root, &["requires-json"])?)?;
                    let remotes: Vec<Remote> = answer(&call(&python, &script, &root, &["remotes-json"])?)?;
                    Ok::<_, anyhow::Error>((requires, remotes))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok((requires, remotes)) => {
                        this.file = requires.file;
                        this.requires = requires.requires;
                        this.remotes = remotes;
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        self.loading_catalog = true;
        cx.spawn(async move |this, cx| {
            let args: &[&str] = if fresh { &["catalog-json", "refresh"] } else { &["catalog-json"] };
            let catalog = cx.background_spawn(async move { answer::<Vec<Package>>(&call(&p2, &s2, &r2, args)?) }).await;
            let _ = this.update(cx, |this, cx| {
                this.loading_catalog = false;
                match catalog {
                    Ok(c) => this.catalog = c,
                    Err(e) => this.error = Some(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Run a quick script command, then read everything again.
    fn act(&mut self, args: Vec<String>, fresh: bool, cx: &mut Context<Self>) {
        let Some((python, script)) = script(cx) else { return };
        let root = self.root.clone();
        self.loading = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { call(&python, &script, &root, &args.iter().map(String::as_str).collect::<Vec<_>>()) }).await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    this.error = Some(e.to_string());
                }
                this.refresh(fresh, cx);
            });
        })
        .detach();
    }

    fn add_remote(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let link = self.link.read(cx).value().trim().to_string();
        if link.is_empty() {
            return;
        }
        self.link.update(cx, |i, cx| i.set_value("", window, cx));
        self.act(vec!["remote-add".into(), link], true, cx);
    }

    fn section(title: String, cx: &App) -> Div {
        div().px_3().pt_3().pb_1().text_xs().font_weight(FontWeight::MEDIUM).text_color(cx.theme().muted_foreground).child(title)
    }
}

impl Render for ConanPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let query = self.query.read(cx).value().trim().to_lowercase();
        let shown: Vec<Package> = self.catalog.iter().filter(|p| query.is_empty() || p.name.to_lowercase().contains(&query)).cloned().collect();
        let count = shown.len();
        let view = cx.entity().downgrade();
        let requires = self.requires.clone();
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .h(px(36.))
                    .px_3()
                    .gap_1()
                    .flex_none()
                    .child(div().flex_1().text_xs().font_weight(FontWeight::MEDIUM).text_color(theme.muted_foreground).child("CONAN"))
                    .when(self.loading, |el| el.child(Spinner::new().xsmall()))
                    .child(Button::new("conan-install").ghost().xsmall().icon(IconName::Download).tooltip(t("Install and configure CMake"))
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(ConanEvent::Run(t("Conan: install").into(), vec!["configure".into()])))))
                    .child(Button::new("conan-refresh").ghost().xsmall().icon(IconName::RefreshCw).tooltip(t("Refresh"))
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(true, cx)))),
            )
            .when_some(self.error.clone(), |el, e| el.child(div().px_3().py_1().text_xs().text_color(theme.danger).child(e)))
            // The project's dependencies.
            .child(Self::section(format!("{} ({})", t("PROJECT DEPENDENCIES"), self.requires.len()), cx))
            .when(self.file.is_none() && !self.loading, |el| {
                el.child(
                    v_flex().px_3().gap_1().text_xs().text_color(theme.muted_foreground).child(t("No conanfile in this project.")).child(
                        Button::new("conan-new").xsmall().label(t("Create a conanfile.txt")).on_click(cx.listener(|this, _, _, cx| {
                            let path = this.root.join("conanfile.txt");
                            this.act(vec!["new".into()], false, cx);
                            cx.emit(ConanEvent::Open(path));
                        })),
                    ),
                )
            })
            .children(self.requires.iter().map(|r| {
                let name = r.split('/').next().unwrap_or(r).to_string();
                h_flex()
                    .id(SharedString::from(format!("req-{r}")))
                    .group("req")
                    .w_full()
                    .h(px(24.))
                    .px_3()
                    .gap_1()
                    .text_sm()
                    .hover(|s| s.bg(theme.list_hover))
                    .child(div().text_color(theme.muted_foreground).child(IconName::Package))
                    .child(div().flex_1().truncate().child(r.clone()))
                    .when(self.file.as_deref() == Some("conanfile.txt"), |el| {
                        el.child(Button::new(SharedString::from(format!("unreq-{r}"))).ghost().xsmall().icon(IconName::X).tooltip(t("Remove"))
                            .on_click(cx.listener(move |this, _, _, cx| this.act(vec!["remove".into(), name.clone()], false, cx))))
                    })
            }))
            // Remotes, and one more by its link.
            .child(Self::section(t("REMOTES").into(), cx))
            .children(self.remotes.iter().map(|r| {
                let name = r.name.clone();
                h_flex()
                    .px_3()
                    .gap_1()
                    .text_xs()
                    .child(v_flex().flex_1().min_w_0().child(div().text_sm().child(r.name.clone())).child(div().truncate().text_color(theme.muted_foreground).child(r.url.clone())))
                    .when(r.name != "conancenter", |el| {
                        el.child(Button::new(SharedString::from(format!("unremote-{}", r.name))).ghost().xsmall().icon(IconName::X).tooltip(t("Remove"))
                            .on_click(cx.listener(move |this, _, _, cx| this.act(vec!["remote-remove".into(), name.clone()], true, cx))))
                    })
            }))
            .child(
                h_flex().px_3().pt_1().gap_1().child(div().flex_1().child(Input::new(&self.link).xsmall())).child(
                    Button::new("conan-remote-add").xsmall().icon(IconName::Plus).tooltip(t("Add the remote")).on_click(cx.listener(|this, _, w, cx| this.add_remote(w, cx))),
                ),
            )
            // Every package available.
            .child(Self::section(format!("{} ({count})", t("AVAILABLE PACKAGES")), cx))
            .child(div().px_3().pb_1().child(Input::new(&self.query).xsmall()))
            .when(self.loading_catalog && self.catalog.is_empty(), |el| {
                el.child(h_flex().px_3().gap_2().text_xs().text_color(theme.muted_foreground).child(Spinner::new().xsmall()).child(t("Reading the remotes' packages…")))
            })
            .child(
                div().flex_1().min_h_0().child(
                    uniform_list("conan-catalog", count, move |range, _, cx| {
                        let theme = cx.theme().clone();
                        range
                            .map(|i| {
                                let p = shown[i].clone();
                                let added = requires.iter().any(|r| r.split('/').next() == Some(p.name.as_str()));
                                let (view, reference, page) = (view.clone(), format!("{}/{}", p.name, p.version), format!("https://conan.io/center/recipes/{}", p.name));
                                h_flex()
                                    .id(SharedString::from(format!("pkg-{}", p.name)))
                                    .debug_selector(move || format!("pkg-row-{i}"))
                                    .w_full()
                                    .h(px(26.))
                                    .px_3()
                                    .gap_1()
                                    .text_sm()
                                    .hover(|s| s.bg(theme.list_hover))
                                    .child(div().flex_1().min_w_0().truncate().child(p.name.clone()))
                                    .child(div().flex_none().text_xs().text_color(theme.muted_foreground).child(p.version.clone()))
                                    .when(p.remote == "conancenter", |el| {
                                        el.child(Button::new(SharedString::from(format!("page-{}", p.name))).ghost().xsmall().icon(IconName::ExternalLink).tooltip(t("Open its page"))
                                            .on_click(move |_, _, cx| cx.open_url(&page)))
                                    })
                                    .child(if added {
                                        div().w(px(22.)).text_color(theme.success).child(IconName::Check).into_any_element()
                                    } else {
                                        Button::new(SharedString::from(format!("add-{}", p.name))).ghost().xsmall().icon(IconName::Plus).tooltip(t("Add to the project"))
                                            .on_click(move |_, _, cx| {
                                                let _ = view.update(cx, |this, cx| this.act(vec!["add".into(), reference.clone()], false, cx));
                                            })
                                            .into_any_element()
                                    })
                            })
                            .collect()
                    })
                    .size_full()
                    .track_scroll(&self.list),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AppContext as _, TestAppContext};

    /// Every package row spans the panel: its buttons line up whatever the name's length.
    #[gpui_kit::test]
    fn package_rows_line_up(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            let base = crate::settings::Settings::default();
            cx.set_global(base.clone());
            cx.set_global(crate::settings::Layers { base, ..Default::default() });
            cx.set_global(crate::plugins::Plugins::default());
        });
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let s = slot.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let view = cx.new(|cx| super::ConanPanel::new(std::env::temp_dir(), window, cx));
            *s.borrow_mut() = Some(view.clone());
            gpui_kit::base::Root::new(view, window, cx)
        });
        let view = slot.borrow().clone().unwrap();
        let pkg = |name: &str| super::Package { name: name.into(), version: "1.0".into(), remote: "conancenter".into() };
        cx.update(|_, cx| view.update(cx, |v, cx| { v.catalog = vec![pkg("aaf"), pkg("accellera-uvm-systemc")]; cx.notify(); }));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let (a, b) = (cx.debug_bounds("pkg-row-0").unwrap(), cx.debug_bounds("pkg-row-1").unwrap());
        assert_eq!((a.left(), a.right()), (b.left(), b.right()), "short {a:?} long {b:?}");
    }

    #[test]
    fn reads_the_scripts_answers() {
        let out = "Conan n'est pas installé…\n[{\"name\": \"fmt\", \"version\": \"10.2.1\", \"remote\": \"conancenter\"}]\n";
        let p: Vec<super::Package> = super::answer(out).unwrap();
        assert_eq!((p[0].name.as_str(), p[0].version.as_str()), ("fmt", "10.2.1"));
        let r: super::Requires = super::answer("{\"file\": \"conanfile.txt\", \"requires\": [\"sfml/2.6.1\"]}").unwrap();
        assert_eq!(r.requires, ["sfml/2.6.1"]);
    }
}
