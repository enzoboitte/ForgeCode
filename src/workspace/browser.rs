//! Browser tab. On Windows and macOS the system's web engine (WebView2, WebKit) draws the page in
//! a native view laid over the tab's area; on Linux the page opens in the system browser.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::*;

use crate::i18n::t;

/// What the address bar typed means: a URL as is, a host or path given a scheme, else a search.
pub fn to_url(typed: &str) -> String {
    let s = typed.trim();
    if s.contains("://") || s.starts_with("about:") || s.starts_with("data:") {
        return s.to_string();
    }
    let host = s.split(['/', '?', '#']).next().unwrap_or("");
    let local = host.starts_with("localhost") || host.starts_with("127.") || host.starts_with("[::1]");
    if local {
        format!("http://{s}")
    } else if !s.contains(' ') && host.contains('.') {
        format!("https://{s}")
    } else {
        let q: String = s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
        format!("https://duckduckgo.com/?q={q}")
    }
}

pub struct BrowserView {
    pub url: String,
    pub title: String,
    address: Entity<InputState>,
    #[cfg(any(windows, target_os = "macos"))]
    web: Option<native::Web>,
    error: Option<String>,
    _subs: Vec<Subscription>,
    _events: Option<Task<()>>,
}

impl BrowserView {
    pub fn new(url: &str, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let url = to_url(url);
        let address = cx.new(|cx| InputState::new(window, cx).default_value(url.clone()).placeholder(t("Search or enter an address")));
        let sub = cx.subscribe_in(&address, window, |this, input, e: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { .. } = e {
                let typed = input.read(cx).value().to_string();
                this.navigate(&typed, cx);
            }
        });
        Self {
            title: url.clone(),
            url,
            address,
            #[cfg(any(windows, target_os = "macos"))]
            web: None,
            error: None,
            _subs: vec![sub],
            _events: None,
        }
    }

    pub fn navigate(&mut self, typed: &str, cx: &mut Context<Self>) {
        self.url = to_url(typed);
        #[cfg(any(windows, target_os = "macos"))]
        if let Some(web) = &self.web {
            let _ = web.view.load_url(&self.url);
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        cx.open_url(&self.url);
        cx.notify();
    }

    /// Hide the page when its tab is not the one shown; drawing the tab shows it again.
    pub fn set_shown(&mut self, shown: bool) {
        #[cfg(any(windows, target_os = "macos"))]
        if !shown && let Some(web) = &self.web {
            native::show(&web.handle(), false);
        }
        let _ = shown;
    }

    fn script(&self, js: &str) {
        #[cfg(any(windows, target_os = "macos"))]
        if let Some(web) = &self.web {
            let _ = web.view.evaluate_script(js);
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        let _ = js;
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        #[cfg(any(windows, target_os = "macos"))]
        if let Some(web) = &self.web {
            let _ = web.view.reload();
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        cx.open_url(&self.url);
        let _ = cx;
    }

    /// The page reports where it went: keep the address bar and the tab title in step.
    #[cfg(any(windows, target_os = "macos"))]
    fn on_page(&mut self, event: native::PageEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            native::PageEvent::Url(url) => {
                self.address.update(cx, |a, cx| a.set_value(url.clone(), window, cx));
                self.url = url;
            }
            native::PageEvent::Title(title) => self.title = title,
            native::PageEvent::Open(url) => return self.navigate(&url, cx),
        }
        cx.notify();
    }
}

impl Render for BrowserView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(any(windows, target_os = "macos"))]
        if self.web.is_none() && self.error.is_none() {
            let (tx, rx) = smol::channel::unbounded();
            match native::Web::new(&self.url, window, tx) {
                Ok(web) => {
                    self.web = Some(web);
                    self._events = Some(cx.spawn_in(window, async move |this, cx| {
                        while let Ok(e) = rx.recv().await {
                            if this.update_in(cx, |this, window, cx| this.on_page(e, window, cx)).is_err() {
                                break;
                            }
                        }
                    }));
                }
                Err(e) => self.error = Some(e.to_string()),
            }
        }
        let theme = cx.theme();
        let bar = h_flex()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(Button::new("web-back").ghost().xsmall().icon(IconName::ArrowLeft).tooltip(t("Back")).on_click(cx.listener(|t, _, _, _| t.script("history.back()"))))
            .child(Button::new("web-forward").ghost().xsmall().icon(IconName::ArrowRight).tooltip(t("Forward")).on_click(cx.listener(|t, _, _, _| t.script("history.forward()"))))
            .child(Button::new("web-reload").ghost().xsmall().icon(IconName::RefreshCw).tooltip(t("Reload")).on_click(cx.listener(|t, _, _, cx| t.reload(cx))))
            .child(div().flex_1().child(Input::new(&self.address).small()))
            .child(Button::new("web-external").ghost().xsmall().icon(IconName::ExternalLink).tooltip(t("Open in the system browser")).on_click(cx.listener(|t, _, _, cx| cx.open_url(&t.url))));

        let body: AnyElement = if let Some(e) = &self.error {
            v_flex().size_full().items_center().justify_center().gap_2().text_sm().text_color(theme.muted_foreground)
                .child(t("The web view could not start."))
                .child(div().text_xs().child(e.clone()))
                .into_any_element()
        } else if cfg!(any(windows, target_os = "macos")) {
            // The native page follows this area's bounds, every frame it is drawn.
            #[cfg(any(windows, target_os = "macos"))]
            {
                let view = self.web.as_ref().map(|w| w.handle());
                canvas(|_, _, _| {}, move |bounds, _, window, cx| {
                    if let Some(view) = &view {
                        // The native view covers whatever GPUI draws: palettes and dialogs too.
                        native::show(view, !window.has_active_dialog(cx));
                        native::place(view, bounds);
                    }
                })
                .size_full()
                .into_any_element()
            }
            #[cfg(not(any(windows, target_os = "macos")))]
            unreachable!()
        } else {
            v_flex().size_full().items_center().justify_center().gap_2().text_sm().text_color(theme.muted_foreground)
                .child(t("On Linux, pages open in the system browser."))
                .child(Button::new("web-open").small().primary().label(t("Open in the system browser")).on_click(cx.listener(|t, _, _, cx| cx.open_url(&t.url))))
                .into_any_element()
        };
        v_flex().size_full().child(bar).child(div().flex_1().min_h_0().child(body))
    }
}

#[cfg(any(windows, target_os = "macos"))]
mod native {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui_kit::{Bounds, Pixels, Window};
    use wry::dpi::{LogicalPosition, LogicalSize};

    pub enum PageEvent {
        Url(String),
        Title(String),
        /// A link asked for a new window: it opens in this tab instead.
        Open(String),
    }

    pub struct Web {
        pub view: Rc<wry::WebView>,
        placed: Rc<Cell<Option<Bounds<Pixels>>>>,
        visible: Rc<Cell<bool>>,
        // Kept alive as long as the view (cookies, cache, storage live there).
        _context: wry::WebContext,
    }

    /// What the paint step needs to move the view.
    pub type Handle = (Rc<wry::WebView>, Rc<Cell<Option<Bounds<Pixels>>>>, Rc<Cell<bool>>);

    impl Web {
        pub fn new(url: &str, window: &Window, events: smol::channel::Sender<PageEvent>) -> wry::Result<Self> {
            // Its own data folder: next to the program (WebView2's default) is often read-only.
            let mut context = wry::WebContext::new(Some(crate::settings::config_dir().join("browser")));
            let (loads, titles, opens) = (events.clone(), events.clone(), events);
            let view = wry::WebViewBuilder::new_with_web_context(&mut context)
                .with_url(url)
                .with_bounds(wry::Rect { position: LogicalPosition::new(0., 0.).into(), size: LogicalSize::new(1., 1.).into() })
                .with_on_page_load_handler(move |event, url| {
                    if matches!(event, wry::PageLoadEvent::Started | wry::PageLoadEvent::Finished) {
                        let _ = loads.try_send(PageEvent::Url(url));
                    }
                })
                .with_document_title_changed_handler(move |title| {
                    let _ = titles.try_send(PageEvent::Title(title));
                })
                .with_new_window_req_handler(move |url, _| {
                    let _ = opens.try_send(PageEvent::Open(url));
                    wry::NewWindowResponse::Deny
                })
                .build_as_child(window)?;
            Ok(Self { view: Rc::new(view), placed: Rc::default(), visible: Rc::new(Cell::new(true)), _context: context })
        }

        pub fn handle(&self) -> Handle {
            (self.view.clone(), self.placed.clone(), self.visible.clone())
        }
    }

    /// Move the native view over `bounds` (window coordinates, logical pixels) when they change.
    pub fn show((view, _, visible): &Handle, show: bool) {
        if visible.get() != show {
            visible.set(show);
            let _ = view.set_visible(show);
        }
    }

    pub fn place((view, placed, _): &Handle, bounds: Bounds<Pixels>) {
        if placed.get() == Some(bounds) {
            return;
        }
        placed.set(Some(bounds));
        let (x, y): (f32, f32) = (bounds.origin.x.into(), bounds.origin.y.into());
        let (w, h): (f32, f32) = (bounds.size.width.into(), bounds.size.height.into());
        let _ = view.set_bounds(wry::Rect { position: LogicalPosition::new(x as f64, y as f64).into(), size: LogicalSize::new(w as f64, h as f64).into() });
    }
}

#[cfg(test)]
mod tests {
    use super::to_url;

    #[test]
    fn address_bar_input_becomes_a_url() {
        assert_eq!(to_url("https://example.com/a"), "https://example.com/a");
        assert_eq!(to_url("example.com"), "https://example.com");
        assert_eq!(to_url("localhost:8080/app"), "http://localhost:8080/app");
        assert_eq!(to_url("127.0.0.1:3000"), "http://127.0.0.1:3000");
        assert_eq!(to_url("rust gpui"), "https://duckduckgo.com/?q=rust%20gpui");
        assert_eq!(to_url("  docs.rs/wry "), "https://docs.rs/wry");
    }
}
