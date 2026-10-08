//! The sole window ordering authority. No Wayland or layout dependencies.
use std::collections::{HashMap, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(pub u64);
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReelSide {
    #[default]
    Left,
    Right,
}
#[derive(Clone, Debug, Default)]
pub struct Workspace {
    windows: VecDeque<WindowId>,
    reel_offset: f64,
    main_tiles: Vec<WindowId>,
    reel_groups: HashMap<WindowId, Vec<WindowId>>,
    active_main: Option<WindowId>,
    split_ratios: HashMap<WindowId, Vec<Option<f64>>>,
    pub side: ReelSide,
}
impl Workspace {
    pub fn windows(&self) -> &VecDeque<WindowId> {
        &self.windows
    }
    pub fn main(&self) -> Option<WindowId> {
        self.windows.front().copied()
    }
    pub fn main_tiles(&self) -> &[WindowId] {
        &self.main_tiles
    }
    pub fn reel_group(&self, anchor: WindowId) -> &[WindowId] {
        self.reel_groups
            .get(&anchor)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
    pub fn split_ratios(&self, anchor: WindowId) -> &[Option<f64>] {
        self.split_ratios
            .get(&anchor)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
    pub fn set_main_split(&mut self, index: usize, ratio: f64) -> bool {
        if index >= self.main_tiles.len() || !ratio.is_finite() {
            return false;
        }
        let Some(anchor) = self.main() else {
            return false;
        };
        let ratios = self.split_ratios.entry(anchor).or_default();
        ratios.resize(self.main_tiles.len(), None);
        ratios[index] = Some(ratio.clamp(0.01, 0.99));
        true
    }
    fn reset_main_splits(&mut self) {
        if let Some(anchor) = self.main() {
            self.split_ratios.remove(&anchor);
        }
    }
    fn group_owner(&self, id: WindowId) -> Option<WindowId> {
        self.reel_groups
            .iter()
            .find(|(_, members)| members.contains(&id))
            .map(|(&anchor, _)| anchor)
    }
    fn contains(&self, id: WindowId) -> bool {
        self.windows.contains(&id)
            || self.main_tiles.contains(&id)
            || self.group_owner(id).is_some()
    }
    pub fn in_main(&self, id: WindowId) -> bool {
        self.main() == Some(id) || self.main_tiles.contains(&id)
    }
    pub fn active_main(&self) -> Option<WindowId> {
        self.active_main
            .filter(|&id| self.in_main(id))
            .or_else(|| self.main())
    }
    pub fn activate_main(&mut self, id: WindowId) -> bool {
        if !self.in_main(id) {
            return false;
        }
        self.active_main = Some(id);
        true
    }
    pub fn add_main(&mut self, index: usize, slots: usize) -> bool {
        let Some(front) = self.reel_front_index() else {
            return false;
        };
        if index < front || index - front >= slots || index >= self.windows.len() {
            return false;
        }
        self.reset_main_splits();
        let id = self.windows.remove(index).unwrap();
        self.split_ratios.remove(&id);
        self.main_tiles.push(id);
        if let Some(members) = self.reel_groups.remove(&id) {
            self.main_tiles.extend(members);
        }
        self.active_main = Some(id);
        self.clamp_offset(slots);
        self.check();
        true
    }
    pub fn release_main(&mut self, id: WindowId, slots: usize) -> bool {
        if self.main_tiles.is_empty() || !self.in_main(id) {
            return false;
        }
        self.reset_main_splits();
        if self.main() == Some(id) {
            self.windows[0] = self.main_tiles.remove(0);
        } else {
            self.main_tiles.retain(|&tile| tile != id);
        }
        self.insert_reel_front(id);
        if self.active_main == Some(id) {
            self.active_main = self.main();
        }
        self.clamp_offset(slots);
        self.check();
        true
    }
    pub fn release_all_main(&mut self, slots: usize) -> bool {
        if self.main_tiles.is_empty() {
            return false;
        }
        self.reset_main_splits();
        let tiles = std::mem::take(&mut self.main_tiles);
        for id in tiles.into_iter().rev() {
            self.insert_reel_front(id);
        }
        self.active_main = self.main();
        self.clamp_offset(slots);
        self.check();
        true
    }
    pub fn offset(&self) -> f64 {
        self.reel_offset
    }
    pub fn reel_front_index(&self) -> Option<usize> {
        (self.reel_offset as usize)
            .checked_add(1)
            .filter(|&index| index < self.windows.len())
    }
    /// Register at the tail; the compositor decides whether to promote the window.
    pub fn add(&mut self, id: WindowId) -> bool {
        if self.contains(id) {
            return false;
        }
        self.windows.push_back(id);
        self.check();
        true
    }
    pub fn remove(&mut self, id: WindowId, slots: usize) -> bool {
        if let Some(anchor) = self.group_owner(id) {
            self.split_ratios.remove(&anchor);
            let members = self.reel_groups.get_mut(&anchor).unwrap();
            members.retain(|&member| member != id);
            if members.is_empty() {
                self.reel_groups.remove(&anchor);
            }
            self.check();
            return true;
        }
        if let Some(mut members) = self.reel_groups.remove(&id) {
            self.split_ratios.remove(&id);
            let index = self
                .windows
                .iter()
                .position(|&window| window == id)
                .unwrap();
            let replacement = members.remove(0);
            self.windows[index] = replacement;
            if !members.is_empty() {
                self.reel_groups.insert(replacement, members);
            }
            self.check();
            return true;
        }
        if let Some(i) = self.main_tiles.iter().position(|&w| w == id) {
            self.reset_main_splits();
            self.main_tiles.remove(i);
            if self.active_main == Some(id) {
                self.active_main = self.main();
            }
            self.check();
            return true;
        }
        let Some(i) = self.windows.iter().position(|&w| w == id) else {
            return false;
        };
        self.split_ratios.remove(&id);
        self.windows.remove(i);
        if i == 0 {
            if !self.main_tiles.is_empty() {
                self.windows.push_front(self.main_tiles.remove(0));
            } else if let Some(anchor) = self.main() {
                self.main_tiles = self.reel_groups.remove(&anchor).unwrap_or_default();
            }
        }
        if self.active_main == Some(id) {
            self.active_main = self.main();
        }
        self.clamp_offset(slots);
        self.check();
        true
    }
    pub fn promote(&mut self, deque_index: usize) -> bool {
        if deque_index >= self.windows.len() {
            return false;
        }
        if deque_index == 0 {
            return true;
        }
        let Some(reel_front) = self.reel_front_index() else {
            return false;
        };
        if deque_index < reel_front {
            return false;
        }
        let selected = self.windows.remove(deque_index).unwrap();
        let previous = std::mem::replace(&mut self.windows[0], selected);
        let incoming = self.reel_groups.remove(&selected).unwrap_or_default();
        let outgoing = std::mem::replace(&mut self.main_tiles, incoming);
        if !outgoing.is_empty() {
            self.reel_groups.insert(previous, outgoing);
        }
        self.active_main = Some(selected);
        self.windows.insert(reel_front, previous);
        self.side = match self.side {
            ReelSide::Left => ReelSide::Right,
            ReelSide::Right => ReelSide::Left,
        };
        self.check();
        true
    }
    pub fn minimize(&mut self, id: WindowId) -> bool {
        if self.group_owner(id).is_some() || self.reel_groups.contains_key(&id) {
            self.remove(id, 0);
            return self.insert_reel_front(id);
        }
        if self.in_main(id) && !self.main_tiles.is_empty() {
            return self.release_main(id, 0);
        }
        let Some(index) = self.windows.iter().position(|&window| window == id) else {
            return false;
        };
        if index == 0 {
            let Some(front) = self.reel_front_index() else {
                return false;
            };
            return self.promote(front);
        }
        let front = (self.reel_offset as usize + 1).min(self.windows.len() - 1);
        self.windows.remove(index);
        self.windows.insert(front, id);
        self.check();
        true
    }
    pub fn insert_reel_front(&mut self, id: WindowId) -> bool {
        if self.windows.is_empty() || self.contains(id) {
            return false;
        }
        let front = (self.reel_offset as usize + 1).min(self.windows.len());
        self.windows.insert(front, id);
        self.check();
        true
    }
    pub fn max_offset(&self, slots: usize) -> f64 {
        self.windows.len().saturating_sub(1).saturating_sub(slots) as f64
    }
    pub fn set_offset(&mut self, offset: f64, slots: usize) {
        if offset.is_finite() {
            self.reel_offset = offset.round().clamp(0., self.max_offset(slots));
        }
    }
    pub fn scroll(&mut self, delta: f64, slots: usize) {
        self.set_offset(self.reel_offset + delta, slots);
    }
    pub fn clamp_offset(&mut self, slots: usize) {
        self.set_offset(self.reel_offset, slots);
    }
    fn check(&self) {
        debug_assert_eq!(
            self.windows
                .iter()
                .chain(self.main_tiles.iter())
                .chain(self.reel_groups.values().flatten())
                .collect::<std::collections::HashSet<_>>()
                .len(),
            self.windows.len()
                + self.main_tiles.len()
                + self.reel_groups.values().map(Vec::len).sum::<usize>()
        );
        debug_assert!(self.reel_groups.iter().all(|(anchor, members)| {
            !members.is_empty() && self.windows.contains(anchor) && self.main() != Some(*anchor)
        }));
    }
}

// niri workspace transport: importing a group preserves its split tree. These
// entry points do not introduce a second ordering policy.
impl Workspace {
    pub fn import_group(&mut self, ids: &[WindowId], ratios: &[Option<f64>], activate: bool, slots: usize) {
        let Some((&anchor, members)) = ids.split_first() else { return; };
        if ids.iter().any(|&id| self.contains(id)) { return; }
        let empty = self.windows.is_empty();
        self.windows.push_back(anchor);
        if empty { self.main_tiles = members.to_vec(); }
        else if !members.is_empty() { self.reel_groups.insert(anchor, members.to_vec()); }
        if !ratios.is_empty() { self.split_ratios.insert(anchor, ratios.to_vec()); }
        if activate {
            // Incoming windows always take the right-hand focus area. Reset
            // scrolling so the outgoing focus group is first in the left reel.
            self.reel_offset = 0.;
            if !empty { self.promote(self.windows.len() - 1); }
            self.side = ReelSide::Left;
        }
        if activate || empty { self.active_main = Some(anchor); }
        self.clamp_offset(slots);
        self.check();
    }

    pub fn import_member(&mut self, anchor: WindowId, id: WindowId, activate: bool) {
        if self.contains(id) { return; }
        self.split_ratios.remove(&anchor);
        if self.main() == Some(anchor) {
            self.main_tiles.push(id);
            if activate { self.active_main = Some(id); }
        } else if self.windows.contains(&anchor) {
            self.reel_groups.entry(anchor).or_default().push(id);
            if activate {
                let index = self.windows.iter().position(|&w| w == anchor).unwrap();
                if index < self.reel_front_index().unwrap_or(1) { self.reel_offset = (index - 1) as f64; }
                self.promote(index);
                self.active_main = Some(id);
            }
        } else { self.add(id); }
        self.check();
    }

    pub fn reset_splits(&mut self) { self.reset_main_splits(); }
}
