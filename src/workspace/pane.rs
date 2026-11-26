//! Editor split layout: a tree of panes, edited by drag & drop like VS Code editor groups.

use gpui_kit::*;

pub use crate::dnd::{DraggedFile, DraggedTab};

/// Where a dragged tab or file lands on a pane.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Zone {
    Center,
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Layout {
    Pane(usize),
    Split { axis: Axis, children: Vec<Layout> },
}

impl Layout {
    /// Put pane `new` next to `target` on the side given by `zone`. Returns false if `target` is absent.
    pub fn split(&mut self, target: usize, new: usize, zone: Zone) -> bool {
        let (axis, before) = match zone {
            Zone::Left => (Axis::Horizontal, true),
            Zone::Right => (Axis::Horizontal, false),
            Zone::Top => (Axis::Vertical, true),
            Zone::Bottom => (Axis::Vertical, false),
            Zone::Center => return false,
        };
        match self {
            Layout::Pane(id) if *id == target => {
                let pair = if before { vec![Layout::Pane(new), Layout::Pane(target)] } else { vec![Layout::Pane(target), Layout::Pane(new)] };
                *self = Layout::Split { axis, children: pair };
                true
            }
            Layout::Pane(_) => false,
            Layout::Split { axis: a, children } => {
                // Same direction: add a sibling instead of nesting.
                if *a == axis {
                    if let Some(i) = children.iter().position(|c| *c == Layout::Pane(target)) {
                        children.insert(if before { i } else { i + 1 }, Layout::Pane(new));
                        return true;
                    }
                }
                children.iter_mut().any(|c| c.split(target, new, zone))
            }
        }
    }

    /// Remove pane `id`, collapsing splits left with a single child.
    pub fn remove(&mut self, id: usize) -> bool {
        let Layout::Split { children, .. } = self else { return false };
        if let Some(i) = children.iter().position(|c| *c == Layout::Pane(id)) {
            children.remove(i);
        } else if !children.iter_mut().any(|c| c.remove(id)) {
            return false;
        }
        if children.len() == 1 {
            *self = children.pop().unwrap();
        }
        true
    }

    /// Pane ids in reading order.
    pub fn panes(&self) -> Vec<usize> {
        match self {
            Layout::Pane(id) => vec![*id],
            Layout::Split { children, .. } => children.iter().flat_map(Layout::panes).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Layout, Zone}; // not `*`: gpui exports its own `test` macro
    use gpui_kit::Axis;

    #[test]
    fn splits_nest_and_collapse() {
        let mut l = Layout::Pane(0);
        assert!(l.split(0, 1, Zone::Right));
        assert!(l.split(1, 2, Zone::Right), "same axis adds a sibling");
        assert_eq!(l.panes(), vec![0, 1, 2]);
        assert!(matches!(&l, Layout::Split { children, .. } if children.len() == 3));
        assert!(l.split(1, 3, Zone::Bottom), "other axis nests");
        assert_eq!(l.panes(), vec![0, 1, 3, 2]);
        assert!(l.remove(3));
        assert_eq!(l, Layout::Split { axis: Axis::Horizontal, children: vec![Layout::Pane(0), Layout::Pane(1), Layout::Pane(2)] });
        assert!(l.remove(0) && l.remove(1));
        assert_eq!(l, Layout::Pane(2));
        assert!(!l.remove(2), "the last pane stays");
        assert!(!l.split(9, 4, Zone::Left));
    }
}
