//! Commit history as a branch graph: time runs left to right, each branch on its own line.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Commit, Repo};
use crate::i18n::t;

pub enum GraphEvent {
    OpenCommit { hash: String, subject: String },
}

pub struct GitGraph {
    repo: Repo,
    commits: Rc<Vec<Commit>>,
    graph: Rc<Graph>,
    scroll: ScrollHandle,
    selected: Option<usize>,
    hovered: Option<usize>,
    /// Show the newest commits (on the right) once the content is laid out.
    scroll_to_end: bool,
    error: Option<String>,
    /// Commit dots built by the last render (tests check it stays at what is on screen).
    #[cfg(test)]
    built: usize,
}

impl EventEmitter<GraphEvent> for GitGraph {}

const LANES: [u32; 6] = [0x4f9cf9, 0xf97583, 0x56d364, 0xe3b341, 0xbc8cff, 0x39c5cf];

impl GitGraph {
    pub fn new(repo: Repo, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            repo,
            commits: Rc::default(),
            graph: Rc::default(),
            scroll: ScrollHandle::new(),
            selected: None,
            hovered: None,
            scroll_to_end: true,
            error: None,
            #[cfg(test)]
            built: 0,
        };
        this.reload(cx);
        this
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let repo = self.repo.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { repo.log(1000) }).await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    // Refreshes are frequent: keep the selection when nothing changed.
                    Ok(c) if *this.commits == c => {}
                    Ok(c) => {
                        this.error = None;
                        this.graph = Rc::new(layout(&c));
                        this.commits = Rc::new(c);
                        this.selected = None;
                        this.hovered = None;
                    }
                    Err(e) => this.error = Some(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

/// Where each commit goes: column `x` (oldest on the left) and line `row` (one per branch).
#[derive(Debug, Default, PartialEq)]
pub struct Graph {
    /// Per commit, in the order of `commits` (newest first).
    pub x: Vec<usize>,
    pub row: Vec<usize>,
    pub rows: usize,
    pub edges: Vec<Edge>,
    /// Branches without commits of their own, drawn as a short line leaving their commit.
    pub stubs: Vec<Stub>,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum EdgeKind {
    Straight,
    /// A branch leaves its parent's line.
    Fork,
    /// A branch comes back into another line.
    Merge,
}

/// A line from a parent (`from`) to its child (`to`), as (column, row).
#[derive(Debug, PartialEq, Clone, Copy)]
pub struct Edge {
    pub from: (usize, usize),
    pub to: (usize, usize),
    pub kind: EdgeKind,
}

#[derive(Debug, PartialEq, Clone)]
pub struct Stub {
    pub name: String,
    pub x: usize,
    pub from_row: usize,
    pub row: usize,
}

/// Branch names in `git log --format=%D` (tags and `HEAD` pointers left out).
pub fn branch_refs(refs: &str) -> Vec<(String, bool)> {
    refs.split(", ")
        .filter_map(|r| {
            let head = r.starts_with("HEAD -> ");
            let r = r.trim_start_matches("HEAD -> ").trim();
            (!r.is_empty() && !r.starts_with("tag: ") && r != "HEAD" && !r.ends_with("/HEAD")).then(|| (r.to_string(), head))
        })
        .collect()
}

/// Columns the badge of a branch tip takes on its line, so nothing else is drawn under it.
const LABEL_COLS: usize = 4;

/// Lay out commits (newest first, as `git log --date-order` prints them).
///
/// Each branch gets a line, main/master first: a branch owns the commits on its first-parent
/// chain that no earlier branch took. Commits of deleted branches get lines of their own too.
/// Lines are reused where they don't overlap, so the graph stays compact.
pub fn layout(commits: &[Commit]) -> Graph {
    let n = commits.len();
    let index: std::collections::HashMap<&str, usize> = commits.iter().enumerate().map(|(i, c)| (c.hash.as_str(), i)).collect();
    let col = |i: usize| n - 1 - i;

    // Branches by priority: main/master, the checked-out one, other local ones, then remotes.
    let mut branches: Vec<(u8, String, usize)> = vec![];
    for (i, c) in commits.iter().enumerate() {
        for (name, head) in branch_refs(&c.refs) {
            let remote = name.starts_with("origin/") || name.starts_with("upstream/");
            let base = name.rsplit('/').next().unwrap_or(&name);
            let main = base == "main" || base == "master";
            let rank = match (main, remote, head) {
                (true, false, _) => 0,
                (true, true, _) => 1,
                (_, false, true) => 2,
                (_, false, false) => 3,
                _ => 4,
            };
            branches.push((rank, name, i));
        }
    }
    branches.sort_by_key(|(rank, _, i)| (*rank, *i));

    // Chains of commits, each will become a line.
    let mut owner: Vec<Option<usize>> = vec![None; n];
    let mut chains: Vec<(Option<String>, Vec<usize>)> = vec![];
    let mut stub_for: Vec<(String, usize)> = vec![];
    let walk = |start: usize, name: Option<String>, owner: &mut Vec<Option<usize>>, chains: &mut Vec<(Option<String>, Vec<usize>)>| {
        let id = chains.len();
        let mut members = vec![];
        let mut cur = Some(start);
        while let Some(i) = cur.filter(|i| owner[*i].is_none()) {
            owner[i] = Some(id);
            members.push(i);
            cur = commits[i].parents.first().and_then(|p| index.get(p.as_str()).copied());
        }
        chains.push((name, members));
    };
    for (_, name, tip) in &branches {
        match owner[*tip] {
            None => walk(*tip, Some(name.clone()), &mut owner, &mut chains),
            Some(chain) => {
                // A new branch with no commits yet: its own short line, unless it's the remote copy
                // of the branch that owns the line.
                let owner_name = chains[chain].0.clone().unwrap_or_default();
                let same = |a: &str, b: &str| a.rsplit('/').next() == b.rsplit('/').next();
                let remote = name.starts_with("origin/") || name.starts_with("upstream/");
                if !remote && !same(&owner_name, name) && !stub_for.iter().any(|(s, _)| same(s, name)) {
                    stub_for.push((name.clone(), *tip));
                }
            }
        }
    }
    for i in 0..n {
        if owner[i].is_none() {
            walk(i, None, &mut owner, &mut chains);
        }
    }

    // Where each line runs, from the commit it leaves to the commit it merges into.
    let mut children: Vec<Vec<usize>> = vec![vec![]; n];
    for (i, c) in commits.iter().enumerate() {
        for p in &c.parents {
            if let Some(&j) = index.get(p.as_str()) {
                children[j].push(i);
            }
        }
    }
    let span = |members: &[usize], tip_label: bool| {
        let mut lo = usize::MAX;
        let mut hi = 0;
        for &i in members {
            lo = lo.min(col(i));
            hi = hi.max(col(i));
            for p in commits[i].parents.iter().filter_map(|p| index.get(p.as_str())) {
                lo = lo.min(col(*p));
            }
            for &ch in &children[i] {
                hi = hi.max(col(ch));
            }
        }
        if tip_label {
            hi += LABEL_COLS;
        }
        (lo, hi)
    };
    let mut taken: Vec<Vec<(usize, usize)>> = vec![];
    let place = |(lo, hi): (usize, usize), taken: &mut Vec<Vec<(usize, usize)>>| {
        let row = taken.iter().position(|spans| spans.iter().all(|&(a, b)| hi + 1 < a || b + 1 < lo)).unwrap_or_else(|| {
            taken.push(vec![]);
            taken.len() - 1
        });
        taken[row].push((lo, hi));
        row
    };
    let mut chain_row = vec![0; chains.len()];
    for (id, (name, members)) in chains.iter().enumerate() {
        chain_row[id] = place(span(members, name.is_some()), &mut taken);
    }

    let row: Vec<usize> = (0..n).map(|i| chain_row[owner[i].unwrap()]).collect();
    let x: Vec<usize> = (0..n).map(col).collect();
    let mut edges = vec![];
    for (i, c) in commits.iter().enumerate() {
        for (k, p) in c.parents.iter().enumerate() {
            let Some(&j) = index.get(p.as_str()) else { continue };
            let kind = if row[i] == row[j] && owner[i] == owner[j] {
                EdgeKind::Straight
            } else if k == 0 {
                EdgeKind::Fork
            } else {
                EdgeKind::Merge
            };
            edges.push(Edge { from: (x[j], row[j]), to: (x[i], row[i]), kind });
        }
    }
    let stubs = stub_for
        .into_iter()
        .map(|(name, tip)| {
            let r = place((x[tip], x[tip] + 1 + LABEL_COLS), &mut taken);
            Stub { name, x: x[tip], from_row: row[tip], row: r }
        })
        .collect();
    Graph { x, row, rows: taken.len().max(1), edges, stubs }
}

const COL_W: f32 = 28.;
const ROW_H: f32 = 34.;
const PAD: f32 = 24.;

fn lane_color(lane: usize) -> Hsla {
    rgb(LANES[lane % LANES.len()]).into()
}

fn cx_of(x: usize) -> f32 {
    PAD + x as f32 * COL_W
}

fn cy_of(row: usize) -> f32 {
    PAD + row as f32 * ROW_H
}

/// The branch and tag names shown next to a commit.
fn refs_of(c: &Commit) -> Vec<String> {
    c.refs.split(", ").filter(|r| !r.is_empty() && !r.ends_with("/HEAD")).map(|r| r.trim_start_matches("HEAD -> ").to_string()).collect()
}

/// Width of a label badge: small text (generous per character, so the end is never cut) plus
/// its padding and border.
fn badge_width(text: &str) -> f32 {
    text.chars().count() as f32 * 8. + 14.
}

/// The part of the content worth building: what is on screen plus a screen on each side
/// (content coordinates, x then y). Before the first layout the size is unknown: a large guess.
fn in_view(scroll: &ScrollHandle) -> (std::ops::Range<f32>, std::ops::Range<f32>) {
    let size = scroll.bounds().size;
    let (w, h) = if size.width > px(0.) { (size.width.as_f32(), size.height.as_f32()) } else { (3000., 2000.) };
    let (left, top) = (-scroll.offset().x.as_f32(), -scroll.offset().y.as_f32());
    (left - w..left + 2. * w, top - h..top + 2. * h)
}

/// The lines of the graph crossing columns `xs`, in content coordinates from `origin`.
fn paint_edges(graph: &Graph, xs: std::ops::Range<f32>, origin: Point<Pixels>, window: &mut Window) {
    let p = |x: f32, y: f32| point(origin.x + px(x), origin.y + px(y));
    let stroke = |points: &[(f32, f32, bool)], color: Hsla, window: &mut Window| {
        let mut path = PathBuilder::stroke(px(2.));
        path.move_to(p(points[0].0, points[0].1));
        for w in points.windows(2) {
            let ((x0, y0, _), (x1, y1, curve)) = (w[0], w[1]);
            if curve {
                let mid = (x0 + x1) / 2.;
                path.cubic_bezier_to(p(x1, y1), p(mid, y0), p(mid, y1));
            } else {
                path.line_to(p(x1, y1));
            }
        }
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    };
    for e in &graph.edges {
        let (x0, y0, x1, y1) = (cx_of(e.from.0), cy_of(e.from.1), cx_of(e.to.0), cy_of(e.to.1));
        if x0.max(x1) < xs.start || x0.min(x1) > xs.end {
            continue;
        }
        match e.kind {
            EdgeKind::Straight => stroke(&[(x0, y0, false), (x1, y1, false)], lane_color(e.to.1), window),
            // Leave the parent's line right away, then run along the branch's line.
            EdgeKind::Fork => stroke(&[(x0, y0, false), ((x0 + COL_W).min(x1), y1, true), (x1, y1, false)], lane_color(e.to.1), window),
            // Run along the branch's line, then join the child at the last moment.
            EdgeKind::Merge => stroke(&[(x0, y0, false), ((x1 - COL_W).max(x0), y0, false), (x1, y1, true)], lane_color(e.from.1), window),
        }
    }
    for s in &graph.stubs {
        let (x0, y0, y1) = (cx_of(s.x), cy_of(s.from_row), cy_of(s.row));
        if !xs.contains(&x0) {
            continue;
        }
        stroke(&[(x0, y0, false), (x0 + COL_W, y1, true)], lane_color(s.row), window);
    }
}

impl Render for GitGraph {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (commits, graph) = (self.commits.clone(), self.graph.clone());
        let cols = commits.len();
        // Wide enough for the last labels, whatever the length of the branch names.
        let labels_end = commits
            .iter()
            .enumerate()
            .map(|(ix, c)| (cx_of(graph.x[ix]) + 10., refs_of(c)))
            .chain(graph.stubs.iter().map(|s| (cx_of(s.x) + COL_W + 10., vec![s.name.clone()])))
            .filter(|(_, refs)| !refs.is_empty())
            .map(|(x, refs)| x + refs.iter().map(|r| badge_width(r) + 4.).sum::<f32>())
            .fold(0., f32::max);
        let content_w = px((cx_of(cols) + COL_W * (LABEL_COLS as f32 + 2.)).max(labels_end) + PAD);
        let content_h = px(cy_of(graph.rows) + PAD);
        if self.scroll_to_end && cols > 0 {
            cx.on_next_frame(window, |this, _, cx| {
                let max = this.scroll.max_offset();
                if max.x > px(0.) {
                    this.scroll.set_offset(point(-max.x, px(0.)));
                    this.scroll_to_end = false;
                }
                cx.notify();
            });
        }
        let (muted, mono, bg, border) = (theme.muted_foreground, theme.mono_font_family.clone(), theme.background, theme.border);
        let selected = self.selected;
        // Only what is on screen (and around) is built and painted: a long history costs no more.
        let (xs, ys) = in_view(&self.scroll);
        let lines = {
            let (graph, xs) = (graph.clone(), xs.clone());
            canvas(|_, _, _| (), move |bounds, _, window, _| paint_edges(&graph, xs.clone(), bounds.origin, window)).absolute().top_0().left_0().w(content_w).h(content_h)
        };
        let badge = |text: String, color: Hsla| {
            let selector = format!("ref-{text}");
            div().debug_selector(move || selector.clone()).px_1p5().rounded_sm().text_xs().border_1().border_color(color).text_color(color).bg(bg).whitespace_nowrap().child(text)
        };
        let mut dots: Vec<AnyElement> = vec![];
        for (ix, c) in commits.iter().enumerate() {
            let (x, y) = (cx_of(graph.x[ix]), cy_of(graph.row[ix]));
            if !xs.contains(&x) || !ys.contains(&y) {
                continue;
            }
            let color = lane_color(graph.row[ix]);
            let r = if selected == Some(ix) { 7. } else { 5. };
            let (hash, subject) = (c.hash.clone(), c.subject.clone());
            dots.push(
                div()
                    .id(("commit", ix))
                    .absolute()
                    .left(px(x - r))
                    .top(px(y - r))
                    .size(px(r * 2.))
                    .rounded_full()
                    .bg(color)
                    .when(c.parents.len() > 1, |el| el.bg(bg).border_2().border_color(color))
                    .cursor_pointer()
                    .hover(|s| s.border_2().border_color(theme.foreground))
                    .on_hover(cx.listener(move |t, hovered: &bool, _, cx| {
                        t.hovered = hovered.then_some(ix).or(t.hovered.filter(|h| *h != ix));
                        cx.notify();
                    }))
                    .on_click(cx.listener(move |t, _, _, cx| {
                        t.selected = Some(ix);
                        cx.emit(GraphEvent::OpenCommit { hash: hash.clone(), subject: subject.clone() });
                        cx.notify();
                    }))
                    .into_any_element(),
            );
            let refs = refs_of(c);
            if !refs.is_empty() {
                dots.push(
                    h_flex()
                        .absolute()
                        .left(px(x + 10.))
                        .top(px(y - 9.))
                        .gap_1()
                        .children(refs.into_iter().map(|r| badge(r, color)))
                        .into_any_element(),
                );
            }
        }
        for s in &graph.stubs {
            let (x, y) = (cx_of(s.x) + COL_W, cy_of(s.row));
            if !xs.contains(&x) || !ys.contains(&y) {
                continue;
            }
            let color = lane_color(s.row);
            dots.push(div().absolute().left(px(x - 5.)).top(px(y - 5.)).size(px(10.)).rounded_full().bg(bg).border_2().border_color(color).into_any_element());
            dots.push(div().absolute().left(px(x + 10.)).top(px(y - 9.)).child(badge(s.name.clone(), color)).into_any_element());
        }
        #[cfg(test)]
        {
            self.built = dots.len();
        }
        let shown = self.hovered.or(self.selected).and_then(|ix| commits.get(ix));
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .h(px(32.))
                    .px_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(border)
                    .child(div().child(IconName::Network))
                    .child(div().text_sm().flex_1().child(format!("{} — {} {}", t("History"), commits.len(), t("commits"))))
                    .child(Button::new("graph-reload").ghost().xsmall().icon(IconName::RefreshCw).on_click(cx.listener(|t, _, _, cx| {
                        t.scroll_to_end = true;
                        t.reload(cx)
                    }))),
            )
            .when_some(self.error.clone(), |el, e| el.child(div().p_2().text_color(theme.danger).child(e)))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("git-graph")
                            .size_full()
                            .overflow_scroll()
                            .track_scroll(&self.scroll)
                            // What is built depends on the scroll position.
                            .on_scroll(cx.listener(|_, _, _, cx| cx.notify()))
                            .child(div().relative().w(content_w).h(content_h).child(lines).children(dots)),
                    )
                    .scrollbar(&self.scroll, gpui_kit::component::scroll::ScrollbarAxis::Both),
            )
            .child(
                h_flex()
                    .h(px(44.))
                    .px_3()
                    .gap_3()
                    .border_t_1()
                    .border_color(border)
                    .text_sm()
                    .map(|el| match shown {
                        Some(c) => el
                            .child(div().text_xs().font_family(mono.clone()).text_color(muted).child(c.short.clone()))
                            .child(div().flex_1().truncate().child(c.subject.clone()))
                            .child(div().text_xs().text_color(muted).child(c.author.clone()))
                            .child(div().text_xs().text_color(muted).child(c.date.clone())),
                        None => el.child(div().text_xs().text_color(muted).child(t("Hover a commit to see it, click to open it"))),
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{EdgeKind, layout};
    use crate::git::Commit;

    fn c(hash: &str, parents: &[&str], refs: &str) -> Commit {
        Commit { hash: hash.into(), parents: parents.iter().map(|p| p.to_string()).collect(), refs: refs.into(), ..Default::default() }
    }

    /// A branch made from main gets its own line, even when it is the newest; main stays on top.
    #[test]
    fn branches_get_their_own_line() {
        let g = layout(&[c("t", &["m"], "HEAD -> test2, origin/test2"), c("m", &["a"], "origin/main, main"), c("a", &[], "")]);
        assert_eq!(g.x, vec![2, 1, 0], "oldest on the left");
        assert_eq!(g.row, vec![1, 0, 0]);
        assert!(g.edges.contains(&super::Edge { from: (1, 0), to: (2, 1), kind: EdgeKind::Fork }));
        assert!(g.stubs.is_empty());
    }

    /// A merged feature branch leaves main and comes back; a deleted branch still gets a line.
    #[test]
    fn merged_branch_forks_and_joins() {
        let g = layout(&[c("m", &["a", "f"], "HEAD -> main"), c("f", &["b"], ""), c("a", &["b"], ""), c("b", &[], "")]);
        assert_eq!(g.row, vec![0, 1, 0, 0]);
        let kinds: Vec<EdgeKind> = g.edges.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, vec![EdgeKind::Straight, EdgeKind::Merge, EdgeKind::Fork, EdgeKind::Straight]);
    }

    /// A branch just created (no commits yet) still shows as a new line leaving its commit.
    #[test]
    fn new_branch_without_commits_is_a_stub() {
        let g = layout(&[c("m", &["a"], "HEAD -> new, origin/main, main"), c("a", &[], "")]);
        assert_eq!(g.row, vec![0, 0]);
        assert_eq!(g.stubs.len(), 1);
        assert_eq!((g.stubs[0].name.as_str(), g.stubs[0].x, g.stubs[0].from_row, g.stubs[0].row), ("new", 1, 0, 1));
    }

    /// A history of 1000 commits builds only the dots on screen, and scrolling builds the others.
    #[gpui_kit::test]
    fn long_histories_only_build_what_is_on_screen(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::{AppContext as _, ScrollDelta, ScrollEvent, point, px};
        let dir = std::env::temp_dir().join(format!("forgecode-graph-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str], input: Option<&str>| {
            use std::io::Write as _;
            let mut child = std::process::Command::new("git").args(args).current_dir(&dir).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).spawn().unwrap();
            if let Some(input) = input {
                child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
            }
            assert!(child.wait().unwrap().success());
        };
        git(&["init", "-q", "-b", "main"], None);
        // 1000 commits at once.
        let stream: String = (1..=1000)
            .map(|i| format!("commit refs/heads/main\ncommitter T <t@t> {} +0000\ndata 4\nc{:03}\n{}\n", 1_600_000_000 + i, i % 1000, if i == 1 { String::new() } else { format!("from :{}\n", i - 1) }).replacen("commit refs/heads/main\n", &format!("commit refs/heads/main\nmark :{i}\n"), 1))
            .collect();
        git(&["fast-import", "--quiet"], Some(&stream));
        // Long names on the newest commit: the scrollable width must reach their end.
        git(&["branch", "a_really_long_branch_name_for_the_label"], None);
        git(&["branch", "and_another_one_quite_long_as_well"], None);
        cx.update(gpui_kit::init);
        let repo = crate::git::Repo { root: dir.clone() };
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let s = slot.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let graph = cx.new(|cx| super::GitGraph::new(repo.clone(), cx));
            *s.borrow_mut() = Some(graph.clone());
            gpui_kit::base::Root::new(graph, window, cx)
        });
        let graph = slot.borrow().clone().unwrap();
        for _ in 0..50 {
            cx.run_until_parked();
            if graph.read_with(cx, |g, _| g.commits.len()) == 1000 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(graph.read_with(cx, |g, _| g.commits.len()), 1000);
        let built = graph.read_with(cx, |g, _| g.built);
        assert!(built > 0 && built < 300, "built {built} dots for 1000 commits");
        // Scrolled all the way right, the last label ends inside the view.
        let max = graph.read_with(cx, |g, _| g.scroll.max_offset());
        graph.update(cx, |g, cx| {
            g.scroll.set_offset(gpui_kit::point(-max.x, gpui_kit::px(0.)));
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let view = graph.read_with(cx, |g, _| g.scroll.bounds());
        let mut last = cx.debug_bounds("ref-and_another_one_quite_long_as_well").unwrap().right();
        last = last.max(cx.debug_bounds("ref-a_really_long_branch_name_for_the_label").unwrap().right());
        assert!(last <= view.right(), "label ends at {last:?}, view at {:?}", view.right());
        // Scrolling builds the commits that come into view.
        let first = graph.read_with(cx, |g, _| g.scroll.offset().x);
        cx.simulate_event(ScrollEvent { position: point(px(200.), px(100.)), delta: ScrollDelta::Pixels(point(px(8000.), px(0.))), ..Default::default() });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(graph.read_with(cx, |g, _| g.scroll.offset().x) > first, "scrolled");
        assert!(graph.read_with(cx, |g, _| g.built) > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
