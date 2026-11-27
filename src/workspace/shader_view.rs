//! The shader preview beside the editor: the frame, play / pause, and an input per uniform.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::shader::{self, Auto, Compiled};
use crate::i18n::t;

/// The preview's size in pixels: shaders read it as `uResolution`.
const SIZE: (u32, u32) = (640, 360);
/// Height of the box the frame is shown in.
const FRAME_HEIGHT: f32 = 300.;

pub struct ShaderPreview {
    source: String,
    compiled: Option<Arc<Compiled>>,
    error: Option<String>,
    /// Per uniform of `compiled`: its value, and the inputs of its components.
    values: Vec<Vec<f32>>,
    inputs: Vec<Vec<Entity<InputState>>>,
    frame: Option<Arc<RenderImage>>,
    playing: bool,
    /// When `uTime` was 0.
    start: Instant,
    paused_at: f32,
    _subs: Vec<Subscription>,
    _anim: Option<Task<()>>,
}

impl ShaderPreview {
    pub fn new() -> Self {
        Self { source: String::new(), compiled: None, error: None, values: vec![], inputs: vec![], frame: None, playing: true, start: Instant::now(), paused_at: 0., _subs: vec![], _anim: None }
    }

    /// The shader changed in the editor: compile it again; its inputs keep their values.
    pub fn set_source(&mut self, source: String, window: &mut Window, cx: &mut Context<Self>) {
        if source == self.source {
            return;
        }
        self.source = source.clone();
        match shader::compile(&source) {
            Err(e) => self.error = Some(e.to_string()),
            Ok(c) => {
                self.error = None;
                let old: Vec<(String, Vec<f32>)> = self.compiled.as_ref().map(|c| c.uniforms.iter().map(|u| u.name.clone()).zip(self.values.clone()).collect()).unwrap_or_default();
                self.values = c.uniforms.iter().map(|u| old.iter().find(|(n, _)| *n == u.name).map_or_else(|| u.default_value(), |(_, v)| v.clone())).collect();
                self.compiled = Some(Arc::new(c));
                self.build_inputs(window, cx);
            }
        }
        self.draw(cx);
        self.animate(cx);
    }

    fn build_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._subs.clear();
        let Some(c) = self.compiled.clone() else { return };
        self.inputs = c
            .uniforms
            .iter()
            .enumerate()
            .map(|(ix, u)| {
                if !u.editable() {
                    return vec![];
                }
                (0..u.components())
                    .map(|i| {
                        let value = self.values[ix][i];
                        let input = cx.new(|cx| InputState::new(window, cx).default_value(format!("{value}")));
                        self._subs.push(cx.subscribe(&input, move |this, input, e: &InputEvent, cx| {
                            if let InputEvent::Change = e
                                && let Ok(v) = input.read(cx).value().trim().replace(',', ".").parse::<f32>()
                            {
                                this.values[ix][i] = v;
                                this.draw(cx);
                            }
                        }));
                        input
                    })
                    .collect()
            })
            .collect();
    }

    fn time(&self) -> f32 {
        if self.playing { self.start.elapsed().as_secs_f32() } else { self.paused_at }
    }

    /// Render one frame with the current values (time and resolution set here).
    fn draw(&mut self, cx: &mut Context<Self>) {
        let Some(c) = self.compiled.clone() else { return cx.notify() };
        let (w, h) = SIZE;
        let values: Vec<Vec<f32>> = c
            .uniforms
            .iter()
            .zip(&self.values)
            .map(|(u, v)| match u.automatic() {
                Some(Auto::Time) => vec![self.time()],
                Some(Auto::Resolution) => if u.components() == 3 { vec![w as f32, h as f32, 1.] } else { vec![w as f32, h as f32] },
                None => v.clone(),
            })
            .collect();
        match shader::render(&c, &values, w, h) {
            Ok(mut rgba) => {
                // gpui draws BGRA.
                for px in rgba.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }
                self.frame = image::RgbaImage::from_raw(w, h, rgba).map(|b| Arc::new(RenderImage::new(vec![image::Frame::new(b)])));
            }
            Err(e) => self.error = Some(e.to_string()),
        }
        cx.notify();
    }

    /// While playing a shader that reads the time: about 30 frames a second.
    fn animate(&mut self, cx: &mut Context<Self>) {
        let timed = self.compiled.as_ref().is_some_and(|c| c.uniforms.iter().any(|u| u.automatic() == Some(Auto::Time)));
        if !(timed && self.playing) {
            self._anim = None;
            return;
        }
        self._anim = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(33)).await;
            if this.update(cx, |this, cx| this.draw(cx)).is_err() {
                break;
            }
        }));
    }

    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        if self.playing {
            self.paused_at = self.time();
        } else {
            self.start = Instant::now() - Duration::from_secs_f32(self.paused_at);
        }
        self.playing = !self.playing;
        self.animate(cx);
        cx.notify();
    }

    fn restart(&mut self, cx: &mut Context<Self>) {
        self.start = Instant::now();
        self.paused_at = 0.;
        self.draw(cx);
    }
}

impl Render for ShaderPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let uniforms = self.compiled.as_ref().map(|c| c.uniforms.clone()).unwrap_or_default();
        let timed = uniforms.iter().any(|u| u.automatic() == Some(Auto::Time));
        let rows = uniforms.iter().enumerate().map(|(ix, u)| {
            let inputs = self.inputs.get(ix).cloned().unwrap_or_default();
            let note = match u.automatic() {
                Some(Auto::Time) => format!("{:.2} s", self.time()),
                Some(Auto::Resolution) => format!("{} × {}", SIZE.0, SIZE.1),
                None if u.is_sampler() => t("white texture").into(),
                None if inputs.is_empty() => t("not editable here").into(),
                None => String::new(),
            };
            v_flex()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .child(div().w(px(150.)).flex_none().truncate().font_family(theme.mono_font_family.clone()).child(u.name.clone()))
                        .child(div().w(px(44.)).flex_none().text_color(theme.muted_foreground).child(u.ty.clone()))
                        .children(inputs.into_iter().map(|i| div().w(px(64.)).child(Input::new(&i).xsmall())))
                        .when(!note.is_empty(), |el| el.child(div().text_color(theme.muted_foreground).child(note))),
                )
                .when(!u.comment.is_empty(), |el| el.child(div().pl(px(158.)).text_xs().text_color(theme.muted_foreground).child(u.comment.clone())))
        });
        v_flex()
            .size_full()
            .bg(theme.background)
            .child(
                h_flex()
                    .flex_none()
                    .h(px(32.))
                    .px_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_xs()
                    .child(div().flex_1().text_color(theme.muted_foreground).child(t("SHADER PREVIEW")))
                    .when(timed, |el| {
                        el.child(Button::new("shader-play").ghost().xsmall().icon(if self.playing { IconName::Pause } else { IconName::Play }).tooltip(t(if self.playing { "Pause" } else { "Play" }))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_play(cx))))
                            .child(Button::new("shader-restart").ghost().xsmall().icon(IconName::Undo2).tooltip(t("Restart the time"))
                                .on_click(cx.listener(|this, _, _, cx| this.restart(cx))))
                    }),
            )
            .child(
                // An image takes its own size unless told one: the box's height, 16:9, never
                // wider than the panel (then it shrinks inside, proportions kept).
                h_flex()
                    .debug_selector(|| "shader-frame".into())
                    .flex_none()
                    .w_full()
                    .h(px(FRAME_HEIGHT))
                    .p_2()
                    .justify_center()
                    .overflow_hidden()
                    .bg(gpui_kit::black())
                    .when_some(self.frame.clone(), |el, frame| {
                        let h = FRAME_HEIGHT - 16.;
                        el.child(img(frame).debug_selector(|| "shader-image".into()).h(px(h)).w(px(h * SIZE.0 as f32 / SIZE.1 as f32)).max_w_full().object_fit(ObjectFit::Contain))
                    }),
            )
            .when_some(self.error.clone(), |el, e| {
                el.child(div().id("shader-error").max_h(px(140.)).overflow_y_scroll().p_2().text_xs().font_family(theme.mono_font_family.clone()).text_color(theme.danger).bg(theme.danger.opacity(0.08)).child(e))
            })
            .child(
                v_flex()
                    .id("shader-uniforms")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_2()
                    .gap_2()
                    .child(div().text_xs().font_weight(FontWeight::MEDIUM).text_color(theme.muted_foreground).child(t("UNIFORMS")))
                    .when(uniforms.is_empty(), |el| el.child(div().text_xs().text_color(theme.muted_foreground).child(t("No uniform in this shader."))))
                    .children(rows),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AppContext as _, TestAppContext};

    /// In a wide panel the frame stays in its box, above the uniforms.
    #[gpui_kit::test]
    fn the_frame_stays_in_its_box(cx: &mut TestAppContext) {
        if super::shader::compile("#version 330\nout vec4 c;\nvoid main() { c = vec4(1.0); }\n").is_err() {
            return eprintln!("no GPU here: skipped");
        }
        cx.update(|cx| {
            gpui_kit::init(cx);
            let base = crate::settings::Settings::default();
            cx.set_global(base.clone());
            cx.set_global(crate::settings::Layers { base, ..Default::default() });
        });
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let s = slot.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let view = cx.new(|_| super::ShaderPreview::new());
            *s.borrow_mut() = Some(view.clone());
            gpui_kit::base::Root::new(view, window, cx)
        });
        let view = slot.borrow().clone().unwrap();
        cx.update(|window, cx| view.update(cx, |v, cx| v.set_source("#version 330\nout vec4 c;\nuniform float uA; // a\nvoid main() { c = vec4(uA, 0.0, 0.0, 1.0); }\n".into(), window, cx)));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let (frame, image) = (cx.debug_bounds("shader-frame").unwrap(), cx.debug_bounds("shader-image").expect("a frame is drawn"));
        assert!(image.bottom() <= frame.bottom() && image.right() <= frame.right(), "image {image:?} out of {frame:?}");
        // A narrow panel: it shrinks instead of going past the right edge.
        cx.simulate_resize(gpui_kit::size(gpui_kit::px(300.), gpui_kit::px(800.)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let (frame, image) = (cx.debug_bounds("shader-frame").unwrap(), cx.debug_bounds("shader-image").unwrap());
        assert!(image.right() <= frame.right() && image.bottom() <= frame.bottom(), "narrow: image {image:?} out of {frame:?}");
    }
}
