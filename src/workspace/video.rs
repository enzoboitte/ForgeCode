//! Video player tab. ffmpeg (installed on the machine) decodes the frames at their own pace
//! (`-re`) as raw BGRA through a pipe; ffplay, without a window, plays the sound alongside.
//! Seeking or pausing restarts both from the chosen time.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::*;

use crate::i18n::t;

pub fn is_video(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| ["mp4", "mov", "m4v", "webm", "mkv", "avi", "ogv"].contains(&e.to_ascii_lowercase().as_str()))
}

/// Longest side of the decoded frames: bigger videos are scaled down (the pane is smaller anyway).
const MAX_SIDE: u32 = 1280;

#[derive(Clone, Debug, PartialEq)]
pub struct Info {
    /// Decoded frame size (scaled, even numbers as the scaler wants).
    pub width: u32,
    pub height: u32,
    pub duration: f64,
    pub fps: f64,
    pub audio: bool,
}

/// Size, length and frame rate from ffprobe.
pub fn probe(path: &Path) -> Result<Info, String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_type,width,height,avg_frame_rate:format=duration", "-of", "json"])
        .arg(path)
        .output()
        .map_err(|e| format!("ffprobe: {e} (install ffmpeg to play videos)"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    let streams = v["streams"].as_array().cloned().unwrap_or_default();
    let video = streams.iter().find(|s| s["codec_type"] == "video").ok_or("no video stream")?;
    let (w, h) = (video["width"].as_u64().unwrap_or(0) as f64, video["height"].as_u64().unwrap_or(0) as f64);
    if w <= 0. || h <= 0. {
        return Err("unknown video size".into());
    }
    let scale = (MAX_SIDE as f64 / w.max(h)).min(1.);
    let even = |x: f64| ((x * scale / 2.).round() as u32 * 2).max(2);
    let fps = video["avg_frame_rate"].as_str().and_then(|r| r.split_once('/')).and_then(|(n, d)| Some(n.parse::<f64>().ok()? / d.parse::<f64>().ok()?)).filter(|f| f.is_finite() && *f > 0.).unwrap_or(25.);
    Ok(Info {
        width: even(w),
        height: even(h),
        duration: v["format"]["duration"].as_str().and_then(|d| d.parse().ok()).unwrap_or(0.),
        fps,
        audio: streams.iter().any(|s| s["codec_type"] == "audio"),
    })
}

/// ffmpeg writing frames from `at` seconds: at the video's pace when `live`, else just one.
fn decoder(path: &Path, info: &Info, at: f64, live: bool) -> std::io::Result<Child> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-nostdin"]);
    if live {
        cmd.arg("-re");
    }
    cmd.args(["-ss", &format!("{at:.3}"), "-i"]).arg(path).args(["-an", "-vf", &format!("scale={}:{}", info.width, info.height), "-pix_fmt", "bgra"]);
    if !live {
        cmd.args(["-frames:v", "1"]);
    }
    cmd.args(["-f", "rawvideo", "-"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
}

/// One BGRA frame as an image gpui can draw.
fn to_image(info: &Info, bytes: Vec<u8>) -> Option<Arc<RenderImage>> {
    let buffer = image::RgbaImage::from_raw(info.width, info.height, bytes)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

/// The frame at `at` seconds (for the first image, and seeking while paused).
pub fn frame_at(path: &Path, info: &Info, at: f64) -> Option<Vec<u8>> {
    let mut child = decoder(path, info, at, false).ok()?;
    let mut bytes = vec![0; (info.width * info.height * 4) as usize];
    let ok = child.stdout.take()?.read_exact(&mut bytes).is_ok();
    let _ = child.wait();
    ok.then_some(bytes)
}

pub struct VideoView {
    pub path: PathBuf,
    info: Result<Info, SharedString>,
    frame: Option<Arc<RenderImage>>,
    /// Seconds into the video of the frame shown.
    position: f64,
    playing: bool,
    decoder: Option<Child>,
    audio: Option<Child>,
    /// Where the progress bar was drawn, to turn a click into a time.
    bar: std::rc::Rc<std::cell::Cell<Bounds<Pixels>>>,
    _task: Option<Task<()>>,
}

impl VideoView {
    pub fn new(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let info = probe(&path).map_err(SharedString::from);
        let mut this = Self { path, info, frame: None, position: 0., playing: false, decoder: None, audio: None, bar: Default::default(), _task: None };
        this.show_frame(0., window, cx);
        this
    }

    fn stop_processes(&mut self) {
        for mut child in [self.decoder.take(), self.audio.take()].into_iter().flatten() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self._task = None;
    }

    /// Paused at `at`: decode that one frame.
    fn show_frame(&mut self, at: f64, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(info) = self.info.clone() else { return };
        self.position = at;
        let path = self.path.clone();
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let bytes = cx.background_spawn({
                let info = info.clone();
                async move { frame_at(&path, &info, at) }
            })
            .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(image) = bytes.and_then(|b| to_image(&info, b)) {
                    this.set_frame(image, window);
                }
                cx.notify();
            });
        }));
    }

    fn set_frame(&mut self, image: Arc<RenderImage>, window: &mut Window) {
        if let Some(old) = self.frame.replace(image) {
            let _ = window.drop_image(old);
        }
    }

    fn play(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(info) = self.info.clone() else { return };
        self.stop_processes();
        // From the start again once the end was reached.
        let start = if info.duration > 0. && self.position >= info.duration - 0.1 { 0. } else { self.position };
        let Ok(mut decoder) = decoder(&self.path, &info, start, true) else { return };
        let Some(mut stdout) = decoder.stdout.take() else { return };
        self.decoder = Some(decoder);
        if info.audio {
            self.audio = Command::new("ffplay")
                .args(["-v", "error", "-nodisp", "-autoexit", "-vn", "-ss", &format!("{start:.3}")])
                .arg(&self.path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok();
        }
        self.playing = true;
        self.position = start;
        // The reader keeps only the newest frame; the view shows it at the video's frame rate,
        // skipping frames when drawing falls behind.
        let latest: Arc<std::sync::Mutex<(Option<Vec<u8>>, usize, bool)>> = Default::default();
        let size = (info.width * info.height * 4) as usize;
        let shared = latest.clone();
        std::thread::spawn(move || {
            loop {
                let mut bytes = vec![0; size];
                let ok = stdout.read_exact(&mut bytes).is_ok();
                let mut l = shared.lock().unwrap();
                if !ok {
                    l.2 = true;
                    break;
                }
                l.0 = Some(bytes);
                l.1 += 1;
            }
        });
        let tick = std::time::Duration::from_secs_f64(1. / info.fps.min(60.));
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(tick).await;
                let (bytes, decoded, done) = {
                    let mut l = latest.lock().unwrap();
                    (l.0.take(), l.1, l.2)
                };
                let alive = this.update_in(cx, |this, window, cx| {
                    if let Some(image) = bytes.and_then(|b| to_image(&info, b)) {
                        this.set_frame(image, window);
                        this.position = start + decoded as f64 / info.fps;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
                if done {
                    break;
                }
            }
            // The end of the video.
            let _ = this.update(cx, |this, cx| {
                this.playing = false;
                this.stop_processes();
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn pause(&mut self, cx: &mut Context<Self>) {
        self.stop_processes();
        self.playing = false;
        cx.notify();
    }

    fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.playing { self.pause(cx) } else { self.play(window, cx) }
    }

    fn seek(&mut self, at: f64, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(info) = &self.info else { return };
        let at = at.clamp(0., info.duration.max(0.));
        if self.playing {
            self.position = at;
            self.play(window, cx);
        } else {
            self.stop_processes();
            self.show_frame(at, window, cx);
        }
    }
}

impl Drop for VideoView {
    fn drop(&mut self) {
        self.stop_processes();
    }
}

fn clock(s: f64) -> String {
    let s = s.max(0.) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

impl Render for VideoView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let info = match &self.info {
            Ok(info) => info.clone(),
            Err(e) => {
                return div().size_full().flex().items_center().justify_center().p_4().text_color(theme.muted_foreground).child(format!("{}: {e}", t("Cannot play this video"))).into_any_element();
            }
        };
        let progress = if info.duration > 0. { (self.position / info.duration).clamp(0., 1.) as f32 } else { 0. };
        let bar = self.bar.clone();
        v_flex()
            .size_full()
            .bg(black())
            .child(
                div()
                    .id("video-frame")
                    .flex_1()
                    .min_h_0()
                    .p_2()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, window, cx| this.toggle(window, cx)))
                    .children(self.frame.clone().map(|f| img(f).object_fit(ObjectFit::Contain).size_full())),
            )
            .child(
                h_flex()
                    .h(px(36.))
                    .px_2()
                    .gap_2()
                    .bg(theme.background)
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("video-play")
                            .ghost()
                            .small()
                            .icon(if self.playing { IconName::Pause } else { IconName::Play })
                            .tooltip(if self.playing { t("Pause") } else { t("Play") })
                            .on_click(cx.listener(|this, _, window, cx| this.toggle(window, cx))),
                    )
                    .child(div().text_xs().text_color(theme.muted_foreground).child(format!("{} / {}", clock(self.position), clock(info.duration))))
                    .child(
                        div()
                            .id("video-bar")
                            .flex_1()
                            .h(px(16.))
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .child(
                                div().relative().w_full().h(px(4.)).rounded_full().bg(theme.muted).child(div().absolute().top_0().left_0().h_full().rounded_full().bg(theme.primary).w(relative(progress))),
                            )
                            .child(canvas(move |bounds, _, _| bar.set(bounds), |_, _, _, _| ()).absolute().top_0().left_0().size_full())
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                                    let b = this.bar.get();
                                    if b.size.width > px(0.) {
                                        let fraction = ((e.position.x - b.origin.x) / b.size.width).clamp(0., 1.) as f64;
                                        this.seek(fraction * info.duration, window, cx);
                                    }
                                }),
                            ),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{frame_at, is_video, probe};

    /// Needs ffmpeg: a generated 1 s clip is measured and its first frame decoded.
    #[test]
    fn probes_and_decodes_a_clip() {
        assert!(is_video("a/b.MP4".as_ref()) && !is_video("a.png".as_ref()));
        if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("ffmpeg not installed: skipped");
            return;
        }
        let path = std::env::temp_dir().join(format!("forgecode-clip-{}.mp4", std::process::id()));
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x240:rate=10:duration=1", "-pix_fmt", "yuv420p"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(made.success());
        let info = probe(&path).unwrap();
        assert_eq!((info.width, info.height), (320, 240));
        assert!((info.fps - 10.).abs() < 0.01 && (info.duration - 1.).abs() < 0.2 && !info.audio, "{info:?}");
        let frame = frame_at(&path, &info, 0.5).unwrap();
        assert_eq!(frame.len(), 320 * 240 * 4);
        assert!(frame.chunks(4).any(|p| p[..3] != [0, 0, 0]), "not a black frame");
        let _ = std::fs::remove_file(path);
        assert!(probe("/nonexistent.mp4".as_ref()).is_err());
    }

    /// Needs ffmpeg: the tab shows the first frame, plays (frames advance), pauses, and seeks.
    #[gpui_kit::test]
    fn plays_pauses_and_seeks(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        let path = std::env::temp_dir().join(format!("forgecode-play-{}.mp4", std::process::id()));
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=160x120:rate=20:duration=3", "-pix_fmt", "yuv420p"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(made.success());
        cx.update(gpui_kit::init);
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let (s, p) = (slot.clone(), path.clone());
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let view = cx.new(|cx| super::VideoView::new(p.clone(), window, cx));
            *s.borrow_mut() = Some(view.clone());
            gpui_kit::base::Root::new(view, window, cx)
        });
        let view = slot.borrow().clone().unwrap();
        let wait = |cx: &mut gpui_kit::VisualTestContext, done: &dyn Fn(&super::VideoView) -> bool| {
            for _ in 0..100 {
                cx.executor().advance_clock(std::time::Duration::from_millis(50));
                cx.run_until_parked();
                if view.read_with(cx, |v, _| done(v)) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
            false
        };
        assert!(wait(cx, &|v| v.frame.is_some()), "first frame shown");
        cx.update(|window, cx| view.update(cx, |v, cx| v.play(window, cx)));
        assert!(wait(cx, &|v| v.position > 0.5), "frames advance while playing");
        view.update(cx, |v, cx| v.pause(cx));
        let paused_at = view.read_with(cx, |v, _| v.position);
        std::thread::sleep(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| (v.playing, v.position)), (false, paused_at), "pause holds the frame");
        cx.update(|window, cx| view.update(cx, |v, cx| v.seek(2.5, window, cx)));
        assert_eq!(view.read_with(cx, |v, _| v.position), 2.5);
        let _ = std::fs::remove_file(path);
    }
}
