use crate::terminal::Terminal;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDir {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone)]
pub struct Pane {
    pub id: u64,
    pub terminal: Arc<Mutex<Terminal>>,
}

#[derive(Clone)]
pub enum PaneTree {
    Leaf(Pane),
    Split {
        dir: SplitDir,
        ratio: f32,
        first: Box<PaneTree>,
        second: Box<PaneTree>,
    },
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Geometry {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl PaneTree {
    pub fn leaves(&self, bounds: Geometry) -> Vec<(&Pane, Geometry)> {
        let mut out = Vec::new();
        self.collect(bounds, &mut out);
        out
    }
    fn collect<'a>(&'a self, bounds: Geometry, out: &mut Vec<(&'a Pane, Geometry)>) {
        match self {
            Self::Leaf(pane) => out.push((pane, bounds)),
            Self::Split {
                dir,
                ratio,
                first,
                second,
            } => {
                let mut a = bounds;
                let mut b = bounds;
                match dir {
                    SplitDir::Vertical => {
                        a.w *= ratio;
                        b.x += a.w;
                        b.w -= a.w;
                    }
                    SplitDir::Horizontal => {
                        a.h *= ratio;
                        b.y += a.h;
                        b.h -= a.h;
                    }
                }
                first.collect(a, out);
                second.collect(b, out);
            }
        }
    }
    pub fn pane_by_id(&self, id: u64) -> Option<&Pane> {
        match self {
            Self::Leaf(pane) => (pane.id == id).then_some(pane),
            Self::Split { first, second, .. } => {
                first.pane_by_id(id).or_else(|| second.pane_by_id(id))
            }
        }
    }
    // VERIFY: pane split — preserve the old leaf, activate the new sibling at the caller.
    pub fn split(&mut self, id: u64, dir: SplitDir, pane: Pane) {
        match self {
            Self::Leaf(old) if old.id == id => {
                *self = Self::Split {
                    dir,
                    ratio: 0.5,
                    first: Box::new(Self::Leaf(old.clone())),
                    second: Box::new(Self::Leaf(pane)),
                };
            }
            Self::Split { first, second, .. } => {
                if first.pane_by_id(id).is_some() {
                    first.split(id, dir, pane);
                } else {
                    second.split(id, dir, pane);
                }
            }
            _ => {}
        }
    }
    pub fn sibling_focus(&self, id: u64) -> Option<u64> {
        match self {
            Self::Leaf(_) => None,
            Self::Split { first, second, .. } => {
                if matches!(first.as_ref(), Self::Leaf(pane) if pane.id == id) {
                    Some(second.leaves(Geometry::default())[0].0.id)
                } else if matches!(second.as_ref(), Self::Leaf(pane) if pane.id == id) {
                    Some(first.leaves(Geometry::default())[0].0.id)
                } else {
                    first.sibling_focus(id).or_else(|| second.sibling_focus(id))
                }
            }
        }
    }
    // VERIFY: pane close collapse — removing a child promotes the intact sibling.
    pub fn without(self, id: u64) -> Option<Self> {
        match self {
            Self::Leaf(pane) => (pane.id != id).then_some(Self::Leaf(pane)),
            Self::Split {
                dir,
                ratio,
                first,
                second,
            } => match (first.without(id), second.without(id)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    dir,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (remaining, None) | (None, remaining) => remaining,
            },
        }
    }
}

// VERIFY: focus_dir — nearest centre in the requested half-plane; in-order ties.
pub fn neighbour(active: u64, dir: Direction, leaves: &[(u64, Geometry)]) -> Option<u64> {
    let (_, a) = leaves.iter().find(|(id, _)| *id == active)?;
    let (ax, ay) = (a.x + a.w / 2.0, a.y + a.h / 2.0);
    leaves
        .iter()
        .filter_map(|(id, b)| {
            let dx = b.x + b.w / 2.0 - ax;
            let dy = b.y + b.h / 2.0 - ay;
            let eligible = match dir {
                Direction::Left => dx < 0.0,
                Direction::Right => dx > 0.0,
                Direction::Up => dy < 0.0,
                Direction::Down => dy > 0.0,
            };
            eligible.then_some((*id, dx * dx + dy * dy))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(id, _)| id)
}
