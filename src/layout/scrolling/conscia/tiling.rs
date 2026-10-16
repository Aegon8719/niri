//! Persistent binary splits for the windows inside a Main/Reel group.
use super::core::{ReelSide, WindowId};
use super::geometry::{Rect, TileSplit};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dock { Left, Right, Top, Bottom, Center }

impl Dock {
    pub fn inner(side: ReelSide) -> Self {
        match side {
            ReelSide::Left => Self::Left,
            ReelSide::Right => Self::Right,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Tree {
    Leaf(WindowId),
    Split { vertical: bool, ratio: f64, first: Box<Tree>, second: Box<Tree> },
}

impl Tree {
    pub fn leaves(&self) -> Vec<WindowId> {
        match self {
            Self::Leaf(id) => vec![*id],
            Self::Split { first, second, .. } => {
                let mut ids = first.leaves();
                ids.extend(second.leaves());
                ids
            }
        }
    }

    pub fn contains(&self, id: WindowId) -> bool {
        match self {
            Self::Leaf(leaf) => *leaf == id,
            Self::Split { first, second, .. } => first.contains(id) || second.contains(id),
        }
    }

    pub fn retain(self, ids: &[WindowId]) -> Option<Self> {
        match self {
            Self::Leaf(id) => ids.contains(&id).then_some(Self::Leaf(id)),
            Self::Split { vertical, ratio, first, second } => {
                match (first.retain(ids), second.retain(ids)) {
                    (Some(first), Some(second)) => Some(Self::Split {
                        vertical, ratio, first: Box::new(first), second: Box::new(second),
                    }),
                    (one, None) | (None, one) => one,
                }
            }
        }
    }

    pub fn remap(&mut self, old: &[WindowId], new: &[WindowId]) {
        match self {
            Self::Leaf(id) => {
                if let Some(index) = old.iter().position(|key| key == id) {
                    if let Some(key) = new.get(index) { *id = *key; }
                }
            }
            Self::Split { first, second, .. } => { first.remap(old, new); second.remap(old, new); }
        }
    }

    pub fn insert(&mut self, target: WindowId, incoming: Self, dock: Dock) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                let previous = Self::Leaf(*id);
                let before = matches!(dock, Dock::Left | Dock::Top);
                let (first, second) = if before { (incoming, previous) } else { (previous, incoming) };
                *self = Self::Split { vertical: matches!(dock, Dock::Left | Dock::Right), ratio: 0.5,
                    first: Box::new(first), second: Box::new(second) };
                true
            }
            Self::Split { first, second, .. } => {
                if first.contains(target) { first.insert(target, incoming, dock) }
                else { second.insert(target, incoming, dock) }
            }
            _ => false,
        }
    }

    pub fn rearrange(&self, source: WindowId, target: WindowId, dock: Dock) -> Option<Self> {
        if source == target || !self.contains(source) || !self.contains(target) { return None; }
        let mut tree = self.clone();
        if dock == Dock::Center {
            // Remapping both leaves in one traversal avoids temporary duplicate IDs.
            tree.remap(&[source, target], &[target, source]);
        } else {
            let ids: Vec<_> = tree.leaves().into_iter().filter(|id| *id != source).collect();
            tree = tree.retain(&ids)?;
            tree.insert(target, Self::Leaf(source), dock);
        }
        Some(tree)
    }

    /// A central drop on an equally sized sibling region swaps the whole
    /// region, preserving the nested splits inside it.
    pub fn swap_group_at(&self, source: WindowId, area: Rect, x: f64, y: f64) -> Option<Self> {
        let source_rect = self.geometry(area).0.into_iter().find(|(id, _)| *id == source)?.1;
        fn target(tree: &Tree, area: Rect, source: WindowId, size: Rect, x: f64, y: f64) -> Option<Tree> {
            if !area.contains(x, y) { return None; }
            if !tree.contains(source) && matches!(tree, Tree::Split { .. })
                && (area.width - size.width).abs() <= 1 && (area.height - size.height).abs() <= 1
                && x >= area.x as f64 + area.width as f64 * 0.25
                && x <= area.x as f64 + area.width as f64 * 0.75
                && y >= area.y as f64 + area.height as f64 * 0.25
                && y <= area.y as f64 + area.height as f64 * 0.75 {
                return Some(tree.clone());
            }
            let Tree::Split { vertical, ratio, first, second } = tree else { return None; };
            let (a, b) = area.split(*vertical, *ratio);
            target(first, a, source, size, x, y).or_else(|| target(second, b, source, size, x, y))
        }
        let group = target(self, area, source, source_rect, x, y)?;
        let ids = group.leaves();
        fn exchange(tree: &mut Tree, source: WindowId, ids: &[WindowId], group: &Tree) {
            if matches!(tree, Tree::Leaf(id) if *id == source) {
                *tree = group.clone();
            } else if tree.leaves() == ids {
                *tree = Tree::Leaf(source);
            } else if let Tree::Split { first, second, .. } = tree {
                exchange(first, source, ids, group);
                exchange(second, source, ids, group);
            }
        }
        let mut result = self.clone();
        exchange(&mut result, source, &ids, &group);
        Some(result)
    }

    pub fn edit_parent_split(&mut self, window: WindowId, swap: bool) -> bool {
        let Self::Split { vertical, ratio, first, second } = self else { return false; };
        if matches!(first.as_ref(), Self::Leaf(id) if *id == window)
            || matches!(second.as_ref(), Self::Leaf(id) if *id == window) {
            if swap {
                std::mem::swap(first, second);
                *ratio = 1. - *ratio;
            } else { *vertical = !*vertical; }
            return true;
        }
        if first.contains(window) { first.edit_parent_split(window, swap) }
        else { second.edit_parent_split(window, swap) }
    }

    pub fn set_ratio(&mut self, index: usize, ratio: f64) -> bool {
        fn visit(tree: &mut Tree, next: &mut usize, index: usize, value: f64) -> bool {
            let Tree::Split { ratio, first, second, .. } = tree else { return false; };
            let current = *next;
            *next += 1;
            if current == index { *ratio = value.clamp(0.01, 0.99); return true; }
            visit(first, next, index, value) || visit(second, next, index, value)
        }
        ratio.is_finite() && visit(self, &mut 0, index, ratio)
    }

    pub fn reset_ratios(&mut self) {
        if let Self::Split { ratio, first, second, .. } = self {
            *ratio = 0.5;
            first.reset_ratios();
            second.reset_ratios();
        }
    }

    pub fn geometry(&self, area: Rect) -> (Vec<(WindowId, Rect)>, Vec<TileSplit>) {
        fn visit(tree: &Tree, area: Rect, rects: &mut Vec<(WindowId, Rect)>, splits: &mut Vec<TileSplit>) {
            match tree {
                Tree::Leaf(id) => rects.push((*id, area)),
                Tree::Split { vertical, ratio, first, second } => {
                    let (a, b) = area.split(*vertical, *ratio);
                    splits.push(TileSplit { index: splits.len(), area, vertical: *vertical,
                        position: if *vertical { b.x } else { b.y } });
                    visit(first, a, rects, splits);
                    visit(second, b, rects, splits);
                }
            }
        }
        let (mut rects, mut splits) = (Vec::new(), Vec::new());
        visit(self, area, &mut rects, &mut splits);
        (rects, splits)
    }

    fn orient_to_area(&mut self, area: Rect) {
        let Self::Split { vertical, ratio, first, second } = self else { return; };
        *vertical = area.width >= area.height;
        let (a, b) = area.split(*vertical, *ratio);
        first.orient_to_area(a);
        second.orient_to_area(b);
    }

    /// Reconcile membership while preserving surviving branches and their ratios.
    /// Merging groups continues at the newest leaf, independent of keyboard focus.
    /// Keep the aspect-driven split axis; only horizontal placement follows the reel side.
    pub fn reconcile(ids: &[WindowId], previous: &[Self], area: Rect, insertion: Option<Dock>) -> Option<Self> {
        let anchor = *ids.first()?;
        let mut tree = previous.iter().find(|t| t.contains(anchor))
            .cloned().and_then(|t| t.retain(ids)).unwrap_or(Self::Leaf(anchor));
        for &id in ids {
            if tree.contains(id) { continue; }
            let missing: Vec<_> = ids.iter().copied().filter(|id| !tree.contains(*id)).collect();
            let mut incoming = previous.iter().find(|t| t.contains(id)).cloned()
                .and_then(|t| t.retain(&missing)).unwrap_or(Self::Leaf(id));
            let target = ids.iter().rev().copied().find(|id| tree.contains(*id)).unwrap();
            let rect = tree.geometry(area).0.into_iter().find(|(id, _)| *id == target).unwrap().1;
            let dock = if rect.width >= rect.height {
                insertion.unwrap_or(Dock::Right)
            } else {
                Dock::Bottom
            };
            if insertion.is_some() {
                let mut destination = rect;
                match dock {
                    Dock::Left => destination.width /= 2,
                    Dock::Right => {
                        destination.x += rect.width / 2;
                        destination.width -= rect.width / 2;
                    }
                    Dock::Top => destination.height /= 2,
                    Dock::Bottom => {
                        destination.y += rect.height / 2;
                        destination.height -= rect.height / 2;
                    }
                    Dock::Center => (),
                }
                incoming.orient_to_area(destination);
            }
            tree.insert(target, incoming, dock);
        }
        Some(tree)
    }
}
