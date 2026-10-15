//! Panels (each with tabs) arranged in a binary split tree.

use std::path::PathBuf;

use egui::{Rect, pos2};

use crate::config::{NodeSnap, PanelSnap, TabSnap};
use crate::inspector::Inspector;
use crate::tab::Tab;

pub struct Panel {
    pub id: u64,
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub inspector: Option<Inspector>,
    pub inspector_ratio: f32,
    pub focus_filter: bool,
    pub filter_has_focus: bool,
    pub rect: Rect,
    pub list_rect: Rect,
    /// Where the inspector was drawn last frame (None: closed).
    pub inspector_rect: Option<Rect>,
}

impl Panel {
    pub fn new(id: u64, tab: Tab) -> Panel {
        Panel {
            id,
            tabs: vec![tab],
            active: 0,
            inspector: None,
            inspector_ratio: 0.42,
            focus_filter: false,
            filter_has_focus: false,
            rect: Rect::NOTHING,
            list_rect: Rect::NOTHING,
            inspector_rect: None,
        }
    }
    pub fn tab(&self) -> &Tab {
        &self.tabs[self.active.min(self.tabs.len() - 1)]
    }
    pub fn tab_mut(&mut self) -> &mut Tab {
        let i = self.active.min(self.tabs.len() - 1);
        &mut self.tabs[i]
    }
    pub fn snap(&self) -> PanelSnap {
        PanelSnap {
            tabs: self
                .tabs
                .iter()
                .map(|t| TabSnap { path: t.path.clone(), view: t.settings.clone(), flatten: t.flatten })
                .collect(),
            active: self.active,
            inspector: self.inspector.is_some(),
        }
    }
    pub fn path(&self) -> PathBuf {
        self.tab().path.clone()
    }
}

/// Smallest panel a split produces when the space allows (tabs + toolbar + a few rows + status bar).
pub const MIN_PANEL_W: f32 = 150.0;
pub const MIN_PANEL_H: f32 = 110.0;

#[derive(Clone, Debug)]
pub enum Node {
    Leaf(u64),
    /// `vertical` = children side by side (a split line that is vertical).
    Split { vertical: bool, ratio: f32, a: Box<Node>, b: Box<Node> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Node {
    pub fn leaves(&self, out: &mut Vec<u64>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { a, b, .. } => {
                a.leaves(out);
                b.leaves(out);
            }
        }
    }

    pub fn first_leaf(&self) -> u64 {
        match self {
            Node::Leaf(id) => *id,
            Node::Split { a, .. } => a.first_leaf(),
        }
    }

    /// Split the leaf `target`, placing `new` on `side`.
    pub fn split(&mut self, target: u64, new: u64, side: Side) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                let old = Node::Leaf(*id);
                let neu = Node::Leaf(new);
                let vertical = matches!(side, Side::Left | Side::Right);
                let (a, b) = if matches!(side, Side::Right | Side::Bottom) { (old, neu) } else { (neu, old) };
                *self = Node::Split { vertical, ratio: 0.5, a: Box::new(a), b: Box::new(b) };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => a.split(target, new, side) || b.split(target, new, side),
        }
    }

    /// Remove a leaf; its sibling takes the parent's place.
    pub fn remove(&mut self, target: u64) -> bool {
        match self {
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => {
                if matches!(**a, Node::Leaf(id) if id == target) {
                    *self = (**b).clone();
                    return true;
                }
                if matches!(**b, Node::Leaf(id) if id == target) {
                    *self = (**a).clone();
                    return true;
                }
                a.remove(target) || b.remove(target)
            }
        }
    }

    /// Compute leaf rects and divider handles.
    pub fn layout(&self, rect: Rect, gap: f32, out: &mut Vec<(u64, Rect)>, dividers: &mut Vec<(Vec<bool>, Rect, bool, Rect)>, path: &mut Vec<bool>) {
        match self {
            Node::Leaf(id) => out.push((*id, rect)),
            Node::Split { vertical, ratio, a, b } => {
                // Each side keeps a usable minimum (when the parent has room for both), so a
                // dragged divider or a small window can't squeeze a panel's toolbar into its
                // status bar.
                let clamp = |len: f32, min: f32| {
                    let t = len * ratio;
                    if len >= 2.0 * min { t.clamp(min, len - min) } else { t }
                };
                let (ra, rb, handle) = if *vertical {
                    let x = rect.left() + clamp(rect.width(), MIN_PANEL_W);
                    (
                        Rect::from_min_max(rect.min, pos2(x - gap / 2.0, rect.bottom())),
                        Rect::from_min_max(pos2(x + gap / 2.0, rect.top()), rect.max),
                        Rect::from_min_max(pos2(x - 3.0, rect.top()), pos2(x + 3.0, rect.bottom())),
                    )
                } else {
                    let y = rect.top() + clamp(rect.height(), MIN_PANEL_H);
                    (
                        Rect::from_min_max(rect.min, pos2(rect.right(), y - gap / 2.0)),
                        Rect::from_min_max(pos2(rect.left(), y + gap / 2.0), rect.max),
                        Rect::from_min_max(pos2(rect.left(), y - 3.0), pos2(rect.right(), y + 3.0)),
                    )
                };
                dividers.push((path.clone(), handle, *vertical, rect));
                path.push(false);
                a.layout(ra, gap, out, dividers, path);
                path.pop();
                path.push(true);
                b.layout(rb, gap, out, dividers, path);
                path.pop();
            }
        }
    }

    pub fn at_path(&mut self, path: &[bool]) -> Option<&mut Node> {
        match (path.first(), self) {
            (None, n) => Some(n),
            (Some(&right), Node::Split { a, b, .. }) => if right { b } else { a }.at_path(&path[1..]),
            _ => None,
        }
    }

    pub fn snap(&self, index_of: &dyn Fn(u64) -> usize) -> NodeSnap {
        match self {
            Node::Leaf(id) => NodeSnap::Leaf(index_of(*id)),
            Node::Split { vertical, ratio, a, b } => NodeSnap::Split {
                vertical: *vertical,
                ratio: *ratio,
                a: Box::new(a.snap(index_of)),
                b: Box::new(b.snap(index_of)),
            },
        }
    }

    pub fn from_snap(s: &NodeSnap, id_of: &dyn Fn(usize) -> Option<u64>) -> Option<Node> {
        Some(match s {
            NodeSnap::Leaf(i) => Node::Leaf(id_of(*i)?),
            NodeSnap::Split { vertical, ratio, a, b } => {
                let a = Node::from_snap(a, id_of);
                let b = Node::from_snap(b, id_of);
                match (a, b) {
                    (Some(a), Some(b)) => Node::Split { vertical: *vertical, ratio: ratio.clamp(0.1, 0.9), a: Box::new(a), b: Box::new(b) },
                    (Some(x), None) | (None, Some(x)) => x,
                    (None, None) => return None,
                }
            }
        })
    }
}
