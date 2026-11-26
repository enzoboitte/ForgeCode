//! In-app PDF viewer (pages rasterized with hayro) and the secondary+wheel zoom shared with the
//! image viewer and the editor.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::scroll::{ScrollableElement as _, ScrollbarAxis};
use gpui_kit::*;
use hayro::hayro_syntax::Pdf;

use crate::i18n::t;

const GAP: f32 = 16.;
/// Longest side of a rasterized page: past it, pages are stretched (and blur) instead of using
/// huge textures.
const MAX_SIDE: f32 = 4096.;

/// Catches cmd/ctrl+wheel and the trackpad pinch over its parent, which must be `relative`.
/// Runs in the capture phase so nothing scrolls, and reports the zoom factor and the cursor
/// position.
pub fn wheel_zoom(on_zoom: impl Fn(f32, Point<Pixels>, &mut Window, &mut App) + 'static) -> impl IntoElement {
    let on_zoom = std::rc::Rc::new(on_zoom);
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let wheel = on_zoom.clone();
            window.on_mouse_event(move |ev: &ScrollEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture && (ev.modifiers.secondary(cx) || ev.modifiers.control) && bounds.contains(&ev.position) {
                    cx.stop_propagation();
                    wheel((ev.delta.pixel_delta(px(20.)).y / px(200.)).exp(), ev.position, window, cx);
                }
            });
            let on_zoom = on_zoom.clone();
            window.on_mouse_event(move |ev: &PinchEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture && bounds.contains(&ev.position) {
                    cx.stop_propagation();
                    on_zoom(1. + ev.delta, ev.position, window, cx);
                }
            });
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// Scroll so the content point under `mouse` stays put while the content scales by `factor`.
pub fn zoom_scroll(scroll: &ScrollHandle, mouse: Point<Pixels>, factor: f32) {
    let local = mouse - scroll.bounds().origin;
    let content = local - scroll.offset();
    let offset = local - content * factor;
    scroll.set_offset(point(offset.x.min(px(0.)), offset.y.min(px(0.))));
}

pub struct PdfView {
    pub path: PathBuf,
    doc: Result<Arc<Pdf>, SharedString>,
    /// Page sizes at 100 % (one PDF point per pixel).
    sizes: Vec<Size<Pixels>>,
    /// Web links of each page: their area at 100 % from the page's top-left corner, and the URL.
    links: Vec<Vec<(Bounds<Pixels>, String)>>,
    pub zoom: f32,
    scroll: ScrollHandle,
    /// Rasterized pages near the viewport, with the scale they were rendered at.
    pages: HashMap<usize, (f32, Arc<RenderImage>)>,
    rendering: HashSet<usize>,
    /// The characters of each page read so far (with its first rendering).
    text: HashMap<usize, Arc<Vec<PdfChar>>>,
    /// Selected text: (anchor, head) as (page, character boundary).
    selection: Option<((usize, usize), (usize, usize))>,
    selecting: bool,
    /// The word (2 clicks) or line (3) a multi-click selected: dragging then extends by whole units.
    unit: Option<(usize, (usize, usize), (usize, usize))>,
    /// Where the mouse last was while selecting, and the task scrolling while it rests past an edge.
    last_mouse: Point<Pixels>,
    _autoscroll: Option<Task<()>>,
    /// Where each page frame was last painted, to find the character under the mouse.
    page_bounds: std::rc::Rc<std::cell::RefCell<HashMap<usize, Bounds<Pixels>>>>,
    focus: Option<FocusHandle>,
}

/// A character drawn on a page: its text and its box at 100 % from the page's top-left corner.
#[derive(Clone, Debug)]
pub struct PdfChar {
    text: String,
    rect: Bounds<Pixels>,
}

/// The characters of a page in drawing order, read by interpreting its content.
fn page_text(page: &hayro::hayro_syntax::page::Page) -> Vec<PdfChar> {
    use hayro::hayro_interpret::font::{Glyph, GlyphRun};
    use hayro::hayro_interpret::kurbo::{self, Shape as _};
    use hayro::hayro_interpret::{self as hi, TransformExt as _};
    struct Collect(Vec<PdfChar>);
    impl<'a> hi::Device<'a> for Collect {
        fn draw_path(&mut self, _: &kurbo::BezPath, _: hi::DrawProps<'a>, _: &hi::DrawMode) {}
        fn push_clip_path(&mut self, _: &hi::ClipPath) {}
        fn push_transparency_group(&mut self, _: f32, _: Option<hi::SoftMask<'a>>, _: hi::BlendMode) {}
        fn draw_image(&mut self, _: hi::Image<'a, '_>, _: hi::ImageDrawProps<'a>) {}
        fn pop_clip(&mut self) {}
        fn pop_transparency_group(&mut self) {}
        fn draw_glyph_run(&mut self, run: &GlyphRun<'_, 'a>, props: hi::DrawProps<'a>, _: &hi::DrawMode) {
            let glyphs = run.glyphs();
            for (i, g) in glyphs.iter().enumerate() {
                let Some(text) = g.as_unicode().map(|u| match u {
                    hayro::hayro_interpret::hayro_cmap::BfString::Char(c) => c.to_string(),
                    hayro::hayro_interpret::hayro_cmap::BfString::String(s) => s,
                }) else { continue };
                // Glyph space has 1000 units per em.
                let t = props.transform * g.transform();
                let o = t * kurbo::Point::ORIGIN;
                let em = (t * kurbo::Point::new(0., 1000.) - o).hypot();
                let next = glyphs.get(i + 1).map(|n| (props.transform * n.transform() * kurbo::Point::ORIGIN).x - o.x).filter(|d| *d > 0. && *d < 3. * em);
                let own = match &**g {
                    Glyph::Outline(outline) => Some((t * kurbo::Point::new(outline.outline().bounding_box().x1, 0.)).x - o.x),
                    Glyph::Type3(g3) => g3.advance_width().map(|w| (t * kurbo::Point::new(w as f64, 0.)).x - o.x),
                };
                let width = next.or(own.filter(|w| *w > 0.)).unwrap_or(em / 2.);
                let rect = Bounds::new(point(px(o.x as f32), px((o.y - 0.8 * em) as f32)), size(px(width as f32), px(em as f32)));
                self.0.push(PdfChar { text, rect });
            }
        }
    }
    let cache = hi::InterpreterCache::new();
    let (w, h) = page.render_dimensions();
    let bbox = kurbo::Rect::new(0., 0., w as f64, h as f64);
    let mut ctx = hi::Context::new(page.initial_transform(true).to_kurbo(), bbox, &cache, page.xref(), hi::InterpreterSettings::default());
    let mut device = Collect(vec![]);
    hi::interpret_page(page, &mut ctx, &mut device);
    device.0
}

/// The text of `chars[from..to]`: a line break where the text moves to another line, a space
/// where a gap stands for one.
fn chars_text(chars: &[PdfChar]) -> String {
    let mut out = String::new();
    let mut prev: Option<&PdfChar> = None;
    for c in chars {
        if let Some(p) = prev {
            let h = p.rect.size.height.max(c.rect.size.height);
            let ends_blank = out.ends_with(char::is_whitespace);
            if (c.rect.top() - p.rect.top()).abs() > h * 0.5 {
                if !ends_blank { out.push('\n') }
            } else if c.rect.left() - p.rect.right() > h * 0.15 && !ends_blank && !c.text.starts_with(char::is_whitespace) {
                out.push(' ');
            }
        }
        out.push_str(&c.text);
        prev = Some(c);
    }
    out
}

/// The character boundary nearest to `at` (page coordinates at 100 %): on the line under it, or
/// the nearest line.
/// The character nearest to `at`, and whether `at` is past its middle.
fn nearest(chars: &[PdfChar], at: Point<Pixels>) -> Option<(usize, bool)> {
    let dy = |c: &PdfChar| f32::from((c.rect.top() - at.y).max(at.y - c.rect.bottom()).max(px(0.)));
    let line = chars.iter().map(dy).min_by(f32::total_cmp)?;
    let dx = |c: &PdfChar| f32::from((c.rect.left() - at.x).max(at.x - c.rect.right()).max(px(0.)));
    let (i, c) = chars.iter().enumerate().filter(|(_, c)| dy(c) <= line + 0.5).min_by(|a, b| dx(a.1).total_cmp(&dx(b.1)))?;
    Some((i, at.x > c.rect.center().x))
}

/// The character boundaries around the word (2 clicks) or line (3 or more) holding character `i`.
fn unit_range(chars: &[PdfChar], i: usize, clicks: usize) -> (usize, usize) {
    let wordy = |c: &PdfChar| c.text.chars().all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '\'');
    let joined = |a: &PdfChar, b: &PdfChar| {
        let h = a.rect.size.height;
        (a.rect.top() - b.rect.top()).abs() < h * 0.5 && (clicks >= 3 || (wordy(a) && wordy(b) && b.rect.left() - a.rect.right() <= h * 0.15))
    };
    let Some(c) = chars.get(i) else { return (i, i) };
    if clicks < 3 && !wordy(c) {
        return (i, i + 1);
    }
    let (mut a, mut b) = (i, i);
    while a > 0 && joined(&chars[a - 1], &chars[a]) {
        a -= 1;
    }
    while b + 1 < chars.len() && joined(&chars[b], &chars[b + 1]) {
        b += 1;
    }
    (a, b + 1)
}

impl PdfView {
    pub fn new(path: PathBuf) -> Self {
        let doc = std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|data| Pdf::new(Arc::new(data)).map_err(|e| format!("{e:?}")));
        let sizes = match &doc {
            Ok(pdf) => pdf.pages().iter().map(|p| p.render_dimensions()).map(|(w, h)| size(px(w), px(h))).collect(),
            Err(_) => vec![],
        };
        let links = match &doc {
            Ok(pdf) => pdf.pages().iter().map(page_links).collect(),
            Err(_) => vec![],
        };
        let doc = doc.map(Arc::new).map_err(SharedString::from);
        Self {
            path,
            doc,
            sizes,
            links,
            zoom: 1.,
            scroll: ScrollHandle::new(),
            pages: HashMap::new(),
            rendering: HashSet::new(),
            text: HashMap::new(),
            selection: None,
            selecting: false,
            unit: None,
            last_mouse: Point::default(),
            _autoscroll: None,
            page_bounds: Default::default(),
            focus: None,
        }
    }

    /// The (page, character boundary) under a window position: on the nearest page.
    fn hit(&self, position: Point<Pixels>) -> Option<(usize, usize)> {
        self.hit_char(position).map(|(page, i, after)| (page, i + after as usize))
    }

    /// The (page, character, past its middle) under a window position: on the nearest page.
    fn hit_char(&self, position: Point<Pixels>) -> Option<(usize, usize, bool)> {
        let bounds = self.page_bounds.borrow();
        let gap = |b: &Bounds<Pixels>| f32::from((b.top() - position.y).max(position.y - b.bottom()).max(px(0.)));
        let (&page, b) = bounds.iter().min_by(|a, b| gap(a.1).total_cmp(&gap(b.1)))?;
        let chars = self.text.get(&page)?;
        let local = point((position.x - b.origin.x) / self.zoom, (position.y - b.origin.y) / self.zoom);
        let (i, after) = nearest(chars, local)?;
        Some((page, i, after))
    }

    /// The selection boundaries of the word or line under a window position.
    fn unit_at(&self, position: Point<Pixels>, clicks: usize) -> Option<((usize, usize), (usize, usize))> {
        let (page, i, _) = self.hit_char(position)?;
        let (a, b) = unit_range(self.text.get(&page)?, i, clicks);
        Some(((page, a), (page, b)))
    }

    /// The mouse moved with the button down, anywhere in the window: the selection follows it.
    fn drag_to(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.last_mouse = position;
        if let Some((clicks, start, end)) = self.unit {
            // By whole words or lines, keeping the first one selected whichever way it goes.
            if let Some((a, b)) = self.unit_at(position, clicks) {
                let next = Some(if a >= start { (start, b) } else { (end, a) });
                if next != self.selection {
                    self.selection = next;
                    cx.notify();
                }
            }
            return;
        }
        if let (Some(at), Some((_, head))) = (self.hit(position), self.selection.as_mut()) {
            if *head != at {
                *head = at;
                cx.notify();
            }
        }
    }

    /// While selecting with the mouse past an edge of the view, scroll towards it (faster the
    /// farther it is) and extend the selection.
    fn autoscroll(&mut self, cx: &mut Context<Self>) {
        let view = self.scroll.bounds();
        let m = self.last_mouse;
        let past = |lo: Pixels, hi: Pixels, v: Pixels| if v < lo { v - lo } else if v > hi { v - hi } else { px(0.) };
        let (dx, dy) = (past(view.left(), view.right(), m.x), past(view.top(), view.bottom(), m.y));
        if dx == px(0.) && dy == px(0.) {
            return;
        }
        let max = self.scroll.max_offset();
        let o = self.scroll.offset();
        let step = |d: Pixels| d.clamp(px(-60.), px(60.));
        self.scroll.set_offset(point((o.x - step(dx)).clamp(-max.x, px(0.)), (o.y - step(dy)).clamp(-max.y, px(0.))));
        cx.notify();
    }

    /// The selection in reading order, if it holds any character.
    fn ordered(&self) -> Option<((usize, usize), (usize, usize))> {
        let (a, b) = self.selection?;
        (a != b).then(|| if a <= b { (a, b) } else { (b, a) })
    }

    pub fn selected_text(&self) -> Option<String> {
        let ((p0, i0), (p1, i1)) = self.ordered()?;
        let pages: Vec<String> = (p0..=p1)
            .filter_map(|p| {
                let chars = self.text.get(&p)?;
                let from = if p == p0 { i0 } else { 0 };
                let to = if p == p1 { i1 } else { chars.len() };
                Some(chars_text(&chars[from.min(chars.len())..to.min(chars.len()).max(from.min(chars.len()))]))
            })
            .collect();
        Some(pages.join("\n"))
    }

    /// Highlight boxes of the selection on `page` (at 100 %), one per run of characters on a line.
    fn highlights(&self, page: usize) -> Vec<Bounds<Pixels>> {
        let (Some(((p0, i0), (p1, i1))), Some(chars)) = (self.ordered(), self.text.get(&page)) else { return vec![] };
        if page < p0 || page > p1 {
            return vec![];
        }
        let from = if page == p0 { i0 } else { 0 }.min(chars.len());
        let to = if page == p1 { i1 } else { chars.len() }.min(chars.len());
        let mut out: Vec<Bounds<Pixels>> = vec![];
        for c in chars.get(from..to).unwrap_or_default() {
            match out.last_mut() {
                Some(last) if (c.rect.top() - last.top()).abs() < last.size.height * 0.5 => *last = last.union(&c.rect),
                _ => out.push(c.rect),
            }
        }
        out
    }

    pub fn page_count(&self) -> usize {
        self.sizes.len()
    }

    /// Zoom around `mouse` (the viewport's top-left corner when `None`).
    pub fn set_zoom(&mut self, zoom: f32, mouse: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        let zoom = zoom.clamp(0.25, 5.);
        zoom_scroll(&self.scroll, mouse.unwrap_or(self.scroll.bounds().origin), zoom / self.zoom);
        self.zoom = zoom;
        cx.notify();
    }
}

/// The page's link annotations that open a URI.
// shortcut: rotated pages get their links at unrotated places; handle /Rotate if one shows up.
fn page_links(page: &hayro::hayro_syntax::page::Page) -> Vec<(Bounds<Pixels>, String)> {
    use hayro::hayro_syntax::object::{Array, Dict, Rect, String as PdfString};
    let crop = page.intersected_crop_box();
    let Some(annots) = page.raw().get::<Array>(b"Annots") else { return vec![] };
    annots
        .iter::<Dict>()
        .filter_map(|a| {
            let r = a.get::<Rect>(b"Rect")?;
            let uri = a.get::<Dict>(b"A")?.get::<PdfString>(b"URI")?;
            let uri = String::from_utf8_lossy(uri.as_bytes()).into_owned();
            let (x0, x1, y0, y1) = (r.x0.min(r.x1), r.x0.max(r.x1), r.y0.min(r.y1), r.y0.max(r.y1));
            // PDF space has y going up from the bottom of the page.
            let origin = point(px((x0 - crop.x0) as f32), px((crop.y1 - y1) as f32));
            Some((Bounds::new(origin, size(px((x1 - x0) as f32), px((y1 - y0) as f32))), uri))
        })
        .collect()
}

/// One page as a BGRA image at `scale` pixels per point, on a white background.
fn rasterize(pdf: &Pdf, ix: usize, scale: f32) -> Option<Arc<RenderImage>> {
    use hayro::{PixmapSettings, RenderCache, RenderSettings};
    let page = pdf.pages().get(ix)?;
    let settings = PixmapSettings { x_scale: scale, y_scale: scale, bg_color: hayro::vello_cpu::color::palette::css::WHITE };
    let pixmap = hayro::render(page, &RenderCache::new(), &Default::default(), &RenderSettings::default(), &settings);
    let mut bgra = pixmap.data_as_u8_slice().to_vec();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(pixmap.width() as u32, pixmap.height() as u32, bgra)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

impl Render for PdfView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let backdrop = cx.theme().secondary.opacity(0.3);
        let selection = cx.theme().primary.opacity(0.3);
        let focus = self.focus.get_or_insert_with(|| cx.focus_handle()).clone();
        let pdf = match &self.doc {
            Ok(pdf) => pdf.clone(),
            Err(e) => {
                return div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{}: {e}", t("Cannot open this PDF")))
                    .into_any_element();
            }
        };
        let scale = (self.zoom * window.scale_factor() * 4.).ceil() / 4.;
        // Before the first paint the viewport is unknown: assume a tall one.
        let height = Some(self.scroll.bounds().size.height).filter(|h| *h > px(0.)).unwrap_or(px(1200.));
        let top = -self.scroll.offset().y;
        let mut y = px(GAP);
        let mut near = HashSet::new();
        let mut pages = vec![];
        for (ix, page) in self.sizes.iter().enumerate() {
            let shown = size(page.width * self.zoom, page.height * self.zoom);
            // Pixels for pages within a screen of the viewport; the others are blank placeholders.
            if y + shown.height > top - height && y < top + 2. * height {
                near.insert(ix);
            }
            y += shown.height + px(GAP);
            let zoom = self.zoom;
            let links = self.links.get(ix).into_iter().flatten().enumerate().map(|(li, (b, url))| {
                let (url, tip) = (url.clone(), url.clone());
                div()
                    .id(SharedString::from(format!("pdf-link-{ix}-{li}")))
                    .absolute()
                    .left(b.origin.x * zoom)
                    .top(b.origin.y * zoom)
                    .w(b.size.width * zoom)
                    .h(b.size.height * zoom)
                    .cursor_pointer()
                    .tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(w, cx))
                    .on_click(move |_, _, cx| cx.open_url(&url))
            });
            let marks = self.highlights(ix).into_iter().map(|b| {
                div().absolute().left(b.origin.x * zoom).top(b.origin.y * zoom).w(b.size.width * zoom).h(b.size.height * zoom).bg(selection)
            });
            let page_bounds = self.page_bounds.clone();
            let frame = div()
                .flex_none()
                .relative()
                .w(shown.width)
                .h(shown.height)
                .bg(white())
                .shadow_md()
                .cursor(CursorStyle::IBeam)
                .child(canvas(move |b, _, _| { page_bounds.borrow_mut().insert(ix, b); }, |_, _, _, _| {}).absolute().size_full());
            pages.push(match self.pages.get(&ix) {
                Some((_, image)) => frame.child(img(image.clone()).size_full()),
                None => frame,
            }.children(marks).children(links));
        }
        for &ix in &near {
            let want = scale.min(MAX_SIDE / self.sizes[ix].width.max(self.sizes[ix].height).to_f64() as f32);
            if self.pages.get(&ix).is_some_and(|(s, _)| *s == want) || !self.rendering.insert(ix) {
                continue;
            }
            let pdf = pdf.clone();
            let read_text = !self.text.contains_key(&ix);
            cx.spawn_in(window, async move |this, cx| {
                let (image, text) = cx
                    .background_spawn(async move {
                        let text = read_text.then(|| pdf.pages().get(ix).map(page_text).unwrap_or_default());
                        (rasterize(&pdf, ix, want), text)
                    })
                    .await;
                this.update_in(cx, |v, window, cx| {
                    v.rendering.remove(&ix);
                    if let Some(text) = text {
                        v.text.insert(ix, Arc::new(text));
                    }
                    // A page that fails to render stays blank until it scrolls back into view.
                    if let Some(image) = image {
                        if let Some((_, old)) = v.pages.insert(ix, (want, image)) {
                            let _ = window.drop_image(old);
                        }
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
        let far: Vec<usize> = self.pages.keys().filter(|ix| !near.contains(ix)).copied().collect();
        for ix in far {
            if let Some((_, image)) = self.pages.remove(&ix) {
                let _ = window.drop_image(image);
            }
        }
        let this = cx.entity().downgrade();
        div()
            .size_full()
            .relative()
            .bg(backdrop)
            .track_focus(&focus)
            .on_key_down(cx.listener(|v, e: &KeyDownEvent, _, cx| {
                let k = &e.keystroke;
                if k.key == "c" && (k.modifiers.platform || k.modifiers.control) {
                    if let Some(text) = v.selected_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        cx.stop_propagation();
                    }
                }
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(move |v, e: &MouseDownEvent, window, cx| {
                window.focus(&focus, cx);
                let Some(at) = v.hit(e.position) else { return };
                v.unit = (e.click_count >= 2).then(|| v.unit_at(e.position, e.click_count)).flatten().map(|(a, b)| (e.click_count, a, b));
                // Shift-click extends the selection from where it started.
                v.selection = match (v.unit, v.selection) {
                    (Some((_, a, b)), _) => Some((a, b)),
                    (None, Some((anchor, _))) if e.modifiers.shift => Some((anchor, at)),
                    _ => Some((at, at)),
                };
                v.selecting = true;
                v.last_mouse = e.position;
                v._autoscroll = Some(cx.spawn(async move |this, cx| loop {
                    cx.background_executor().timer(std::time::Duration::from_millis(30)).await;
                    let going = this.update(cx, |v, cx| {
                        if v.selecting {
                            v.autoscroll(cx);
                            v.drag_to(v.last_mouse, cx);
                        }
                        v.selecting
                    });
                    if !going.unwrap_or(false) {
                        break;
                    }
                }));
                cx.notify();
            }))
            // Followed over the whole window: the mouse may leave the view, which is how it scrolls.
            .child({
                let (moved, released) = (cx.entity().downgrade(), cx.entity().downgrade());
                canvas(|_, _, _| (), move |_, _, window, _| {
                    window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
                        if phase == DispatchPhase::Bubble {
                            let _ = moved.update(cx, |v, cx| {
                                if v.selecting && e.pressed_button == Some(MouseButton::Left) {
                                    v.drag_to(e.position, cx);
                                } else {
                                    v.selecting = false;
                                }
                            });
                        }
                    });
                    window.on_mouse_event(move |e: &MouseUpEvent, phase, _, cx| {
                        if phase == DispatchPhase::Bubble && e.button == MouseButton::Left {
                            let _ = released.update(cx, |v, _| v.selecting = false);
                        }
                    });
                })
                .absolute()
                .size_0()
            })
            .child(
                div()
                    .id("pdf-pages")
                    .size_full()
                    .flex()
                    // Not stretched to the pane's height: the pages' own height is what scrolls.
                    .items_start()
                    .overflow_scroll()
                    .track_scroll(&self.scroll)
                    // At least as wide as the pane, wider when zoomed in: pages stay centred and reachable.
                    .child(div().flex_none().min_w(relative(1.)).flex().flex_col().items_center().gap(px(GAP)).p(px(GAP)).children(pages)),
            )
            .child(wheel_zoom(move |factor, mouse, _, cx| {
                let _ = this.update(cx, |v, cx| v.set_zoom(v.zoom * factor, Some(mouse), cx));
            }))
            .scrollbar(&self.scroll, ScrollbarAxis::Both)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{PdfView, rasterize};

    /// The characters are read with their place, and selecting them gives the text back.
    #[test]
    fn reads_and_selects_the_text() {
        let path = pdf_file("text", 50, 40);
        let mut view = PdfView::new(path.clone());
        let chars = super::page_text(view.doc.as_ref().unwrap().pages().get(0).unwrap());
        let text: String = chars.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(text.replace(' ', ""), "Hithereyo");
        let h = &chars[0].rect;
        assert!((f32::from(h.left()) - 2.).abs() < 1. && (f32::from(h.top()) - 2.).abs() < 1.5, "{h:?}");
        assert_eq!(super::nearest(&chars, gpui_kit::point(gpui_kit::px(0.), gpui_kit::px(5.))), Some((0, false)));
        view.text.insert(0, std::sync::Arc::new(chars.clone()));
        view.selection = Some(((0, chars.len()), (0, 0)));
        assert_eq!(view.selected_text().unwrap(), "Hi there\nyo");
        assert_eq!(view.highlights(0).len(), 2, "one box per line");
        // A double click takes the word, a triple click the line.
        let t = chars.iter().position(|c| c.text == "t").unwrap();
        let (a, b) = super::unit_range(&chars, t, 2);
        view.selection = Some(((0, a), (0, b)));
        assert_eq!(view.selected_text().unwrap(), "there");
        let (a, b) = super::unit_range(&chars, t, 3);
        view.selection = Some(((0, a), (0, b)));
        assert_eq!(view.selected_text().unwrap(), "Hi there");
        std::fs::remove_file(path).unwrap();
    }

    /// A one-page PDF of `w`×`h` points with a black square.
    fn pdf_file(name: &str, w: u32, h: u32) -> std::path::PathBuf {
        let content = "0 0 0 rg 10 10 30 30 re f BT /F1 10 Tf 2 30 Td (Hi there) Tj 0 -12 Td (yo) Tj ET";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Contents 4 0 R /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> /Annots [<< /Type /Annot /Subtype /Link /Rect [5 20 45 30] /A << /S /URI /URI (https://example.com/x) >> >>] >>"),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = vec![];
        for (i, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend(format!("{} 0 obj\n{body}\nendobj\n", i + 1).bytes());
        }
        let xref = pdf.len();
        pdf.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
        for o in offsets {
            pdf.extend(format!("{o:010} 00000 n \n").bytes());
        }
        pdf.extend(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).bytes());
        let path = std::env::temp_dir().join(format!("forgecode-{name}-{}.pdf", std::process::id()));
        std::fs::write(&path, pdf).unwrap();
        path
    }

    /// The page rasterized and swapped to BGRA.
    #[test]
    fn renders_pdf_pages() {
        let path = pdf_file("render", 50, 40);

        let view = PdfView::new(path.clone());
        assert_eq!(view.page_count(), 1);
        let image = rasterize(view.doc.as_ref().unwrap(), 0, 2.).unwrap();
        let size = image.size(0);
        assert_eq!((size.width.0, size.height.0), (100, 80));
        let bytes = image.as_bytes(0).unwrap();
        let at = |x: usize, y: usize| &bytes[(y * 100 + x) * 4..][..4];
        assert_eq!(at(2, 2), [255, 255, 255, 255], "white background");
        assert_eq!(at(50, 40), [0, 0, 0, 255], "the square (PDF y grows upwards)");
        let (area, url) = &view.links[0][0];
        assert_eq!(url, "https://example.com/x");
        use gpui_kit::{Bounds, point, px};
        assert_eq!(*area, Bounds::new(point(px(5.), px(10.)), gpui_kit::size(px(40.), px(10.))), "from the page's top-left corner");
        std::fs::remove_file(path).unwrap();
        assert!(PdfView::new("/nonexistent.pdf".into()).doc.is_err());
    }

    /// A page taller than the pane scrolls with the wheel; the pinch and ctrl+wheel zoom.
    #[gpui_kit::test]
    fn pdf_scrolls_and_zooms(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::{AppContext as _, Modifiers, PinchEvent, ScrollDelta, ScrollEvent, point, px};
        let path = pdf_file("scroll", 300, 3000);
        cx.update(gpui_kit::init);
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let s = slot.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let view = cx.new(|_| PdfView::new(path.clone()));
            *s.borrow_mut() = Some(view.clone());
            gpui_kit::base::Root::new(view, window, cx)
        });
        let view = slot.borrow().clone().unwrap();
        cx.run_until_parked();
        let at = point(px(100.), px(100.));
        cx.simulate_event(ScrollEvent { position: at, delta: ScrollDelta::Pixels(point(px(0.), px(-500.))), ..Default::default() });
        cx.run_until_parked();
        assert!(view.read_with(cx, |v, _| v.scroll.offset().y) < px(-100.), "scrolled down");
        cx.simulate_event(PinchEvent { position: at, delta: 0.5, ..Default::default() });
        assert!((view.read_with(cx, |v, _| v.zoom) - 1.5).abs() < 0.01, "pinch zooms in");
        let ctrl = Modifiers { control: true, ..Default::default() };
        cx.simulate_event(ScrollEvent { position: at, delta: ScrollDelta::Pixels(point(px(0.), px(-200.))), modifiers: ctrl, ..Default::default() });
        assert!(view.read_with(cx, |v, _| v.zoom) < 1.5, "ctrl+wheel zooms out");
    }

    /// Dragging past the bottom of the view scrolls and keeps selecting; shift-click extends.
    #[gpui_kit::test]
    fn selection_drags_past_the_edge_and_extends_with_shift(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::{AppContext as _, Modifiers, MouseButton, point, px};
        let path = pdf_file("select", 300, 3000);
        cx.update(gpui_kit::init);
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let s = slot.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let view = cx.new(|_| PdfView::new(path.clone()));
            *s.borrow_mut() = Some(view.clone());
            gpui_kit::base::Root::new(view, window, cx)
        });
        let view = slot.borrow().clone().unwrap();
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(view.read_with(cx, |v, _| v.text.contains_key(&0)), "text read");
        let height = cx.update(|window, _| window.viewport_size().height);
        let start = point(px(100.), px(100.));
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        let anchor = view.read_with(cx, |v, _| v.selection.unwrap().0);
        cx.simulate_mouse_move(point(px(100.), height + px(40.)), MouseButton::Left, Modifiers::none());
        for _ in 0..10 {
            cx.executor().advance_clock(std::time::Duration::from_millis(30));
            cx.run_until_parked();
        }
        assert!(view.read_with(cx, |v, _| v.scroll.offset().y) < px(-100.), "scrolled while selecting");
        cx.simulate_mouse_up(point(px(100.), height + px(40.)), MouseButton::Left, Modifiers::none());
        assert!(!view.read_with(cx, |v, _| v.selecting));
        cx.simulate_mouse_down(point(px(150.), px(50.)), MouseButton::Left, Modifiers::shift());
        cx.simulate_mouse_up(point(px(150.), px(50.)), MouseButton::Left, Modifiers::shift());
        assert_eq!(view.read_with(cx, |v, _| v.selection.unwrap().0), anchor, "shift-click keeps the anchor");
    }
}
