//! Conscia's Main/Reel policy, hosted by niri's workspace and protocol machinery.
//!
//! `core`, `geometry`, and `scene` are vendored from ../Conscia. Columns are only
//! transport containers for niri's workspace/output actions, never layout authority.
pub(super) mod core;
pub(super) mod geometry;
pub(super) mod scene;
pub(super) mod tiling;

use super::*;
use self::core::{ReelSide, WindowId};
use geometry::{LayoutSnapshot, Rect, WindowRole};
use scene::{SceneItem, Transition};
use tiling::{Dock, Tree};

#[derive(Debug)]
pub(super) struct Drag {
    window: WindowId,
    start: Point<f64, Logical>,
    pointer: Point<f64, Logical>,
    origin: SceneItem,
    base: Tree,
    preview: Option<Tree>,
    moved: bool,
    hint: Option<SnapHint>,
}

#[derive(Debug)]
struct SnapHint {
    from: Rectangle<f64, Logical>,
    to: Rectangle<f64, Logical>,
    from_alpha: f32,
    to_alpha: f32,
    start: Duration,
    progress: f64,
}

impl SnapHint {
    fn sample(&self) -> (Rectangle<f64, Logical>, f32) {
        let t = self.progress.clamp(0., 1.);
        let t = t * t * (3. - 2. * t);
        let rect = Rectangle::new(self.from.loc + (self.to.loc - self.from.loc).upscale(t),
            self.from.size + (self.to.size - self.from.size).upscale(t));
        (rect, self.from_alpha + (self.to_alpha - self.from_alpha) * t as f32)
    }
}

impl Drag {
    fn set_hint(&mut self, target: Option<Rectangle<f64, Logical>>, now: Duration, animate: bool) {
        let Some(rect) = target.or_else(|| self.hint.as_ref().map(|hint| hint.to)) else { return; };
        let alpha = if target.is_some() { 1. } else { 0. };
        if self.hint.as_ref().is_some_and(|hint| hint.to == rect && hint.to_alpha == alpha) { return; }
        // Keep the landing geometry exact even when released mid-animation.
        // Only opacity animates; interpolating rectangles shows a false target.
        let from_alpha = self.hint.as_ref().filter(|hint| hint.to == rect)
            .map_or(0., |hint| hint.sample().1);
        self.hint = Some(SnapHint { from: rect, to: rect, from_alpha, to_alpha: alpha, start: now,
            progress: if animate { 0. } else { 1. } });
    }
}

#[derive(Debug)]
pub(super) struct State<I> {
    pub model: core::Workspace,
    pub(super) ids: Vec<(I, WindowId)>,
    pub layout: Option<LayoutSnapshot>,
    pub inset: i32,
    pub main_maximized: bool,
    pub transition: Option<(Duration, Transition)>,
    pub expanded: Option<(WindowId, bool)>,
    pub parked: Option<WindowId>,
    pub dialogs: Vec<(WindowId, WindowId)>,
    pub dialog_scene: Vec<SceneItem>,
    pub remainder: f64,
    pub last_scroll: Option<Duration>,
    pub reel_bounce: Option<(Duration, f64)>,
    pub reel_bounce_offset: f64,
    pub gesture: Option<bool>,
    pub resize: Option<(geometry::TileSplit, Point<f64, Logical>, i32)>,
    pub trees: Vec<Tree>,
    pub last_active: Option<WindowId>,
    pub drag: Option<Drag>,
}

impl<I: PartialEq + Clone> State<I> {
    pub fn new() -> Self {
        Self {
            model: core::Workspace::default(), ids: Vec::new(), layout: None, inset: 0, main_maximized: false,
            transition: None, expanded: None, parked: None, dialogs: Vec::new(), dialog_scene: Vec::new(), remainder: 0.,
            last_scroll: None, reel_bounce: None, reel_bounce_offset: 0., gesture: None, resize: None,
            trees: Vec::new(), last_active: None, drag: None,
        }
    }

    pub fn id(&self, id: &I) -> Option<WindowId> {
        self.ids.iter().find(|(native, _)| native == id).map(|(_, id)| *id)
    }

    pub(super) fn register(&mut self, id: &I) -> WindowId {
        if let Some(id) = self.id(id) { return id; }
        let key = WindowId(COLUMN_ID_COUNTER.next());
        self.ids.push((id.clone(), key));
        key
    }

    pub fn slots(&self) -> usize {
        self.layout.as_ref().map_or(3, |l| l.slot_count)
    }

    pub fn end_scroll(&mut self) {
        self.remainder = 0.;
        self.last_scroll = None;
    }

    pub fn tree(&self, anchor: WindowId) -> Option<&Tree> {
        self.trees.iter().find(|tree| tree.contains(anchor))
    }

    fn sync_trees(&mut self, area: Rect) {
        let previous = std::mem::take(&mut self.trees);
        for &anchor in self.model.windows() {
            let members = if Some(anchor) == self.model.main() { self.model.main_tiles() }
                else { self.model.reel_group(anchor) };
            let ids: Vec<_> = std::iter::once(anchor).chain(members.iter().copied()).collect();
            if let Some(tree) = Tree::reconcile(&ids, &previous, area, self.last_active) {
                self.trees.push(tree);
            }
        }
        self.last_active = self.model.active_main();
    }

    pub fn target_scene(&self) -> Vec<SceneItem> {
        let Some(layout) = &self.layout else { return Vec::new(); };
        self.scene_for_layout(layout)
    }

    fn scene_for_layout(&self, layout: &LayoutSnapshot) -> Vec<SceneItem> {
        let canvas = Rect { x: 0, y: 0,
            width: layout.reel.width * layout.slot_count as i32,
            height: layout.reel.height };
        let mut result = Vec::new();
        let fullscreen = self.expanded.is_some_and(|(_, fullscreen)| fullscreen);
        for (mut item, placement) in scene::scene(layout, self.model.offset()).into_iter().zip(&layout.placements) {
            if fullscreen { result.push(item); continue; }
            let members = self.model.reel_group(item.window);
            if members.is_empty() || canvas.width <= 0 || canvas.height <= 0 {
                let group = if item.role == WindowRole::Main { layout.main } else { placement.rect };
                let rect = geometry::inset_in_group(placement.rect, group, (1., 1.),
                    self.inset as f64);
                item.x += rect.x - (placement.rect.x - group.x) as f64;
                item.y += rect.y - (placement.rect.y - group.y) as f64;
                item.width = rect.width;
                item.height = rect.height;
                result.push(item);
                continue;
            }
            let Some(tree) = self.tree(item.window) else { continue; };
            let rects = tree.geometry(canvas).0;
            let scale = (item.width / canvas.width as f64, item.height / canvas.height as f64);
            for (window, rect) in rects {
                let rect = geometry::inset_in_group(rect, canvas, scale,
                    self.inset as f64);
                result.push(SceneItem { window,
                    x: item.x + rect.x, y: item.y + rect.y,
                    width: rect.width, height: rect.height,
                    ..item.clone() });
            }
        }
        result
    }

    pub fn current_scene(&self) -> Vec<SceneItem> {
        let mut items = self.transition.as_ref().map_or_else(|| self.target_scene(), |(_, t)| t.sample());
        // Compress the reel towards the opposite edge, leaving a visible gap at
        // the boundary without moving thumbnails outside the work area.
        if let Some(layout) = &self.layout {
            if self.reel_bounce_offset != 0. && layout.reel.height > 0 {
                let scale = 1. - self.reel_bounce_offset.abs() / layout.reel.height as f64;
                let center_x = layout.reel.x as f64 + layout.origin_offset.0 + layout.reel.width as f64 / 2.;
                let pivot_y = layout.reel.y as f64 + layout.origin_offset.1
                    + if self.reel_bounce_offset > 0. { layout.reel.height as f64 } else { 0. };
                for item in &mut items {
                    if !matches!(item.role, WindowRole::Reel { .. }) { continue; }
                    item.x = center_x + (item.x - center_x) * scale;
                    item.y = pivot_y + (item.y - pivot_y) * scale;
                    item.width *= scale;
                    item.height *= scale;
                    item.scale *= scale;
                }
            }
        }
        if let Some(drag) = &self.drag {
            if drag.moved {
                // Only the dragged window follows the pointer. The candidate
                // tree is rendered as a separate snap hint, never as windows.
                items.retain(|item| item.window != drag.window);
                let mut ghost = drag.origin.clone();
                ghost.x += drag.pointer.x - drag.start.x;
                ghost.y += drag.pointer.y - drag.start.y;
                ghost.opacity = 0.8;
                items.insert(0, ghost);
            }
        }
        items.extend(self.dialog_scene.iter().cloned());
        items
    }

    pub fn snap_hint(&self) -> Option<(Rectangle<f64, Logical>, f32)> {
        let hint = self.drag.as_ref()?.hint.as_ref()?;
        let (rect, alpha) = hint.sample();
        (alpha > 0. && rect.size.w > 0. && rect.size.h > 0.).then_some((rect, alpha))
    }

    pub fn drag_window(&self) -> Option<WindowId> {
        self.drag.as_ref().filter(|drag| drag.moved).map(|drag| drag.window)
    }

    pub fn advance_snap_hint(&mut self, now: Duration) {
        if let Some(hint) = self.drag.as_mut().and_then(|drag| drag.hint.as_mut()) {
            hint.progress = (now.saturating_sub(hint.start).as_secs_f64() / 0.12).min(1.);
        }
    }

    pub fn snap_hint_animating(&self) -> bool {
        self.drag.as_ref().and_then(|drag| drag.hint.as_ref()).is_some_and(|hint| hint.progress < 1.)
    }

    pub fn active(&self) -> Option<WindowId> {
        self.dialog_scene.first().map(|p| p.window).or_else(|| self.model.active_main())
    }

    pub fn visible_scene(&self) -> Vec<SceneItem> {
        let mut items = self.target_scene();
        items.extend(self.dialog_scene.iter().cloned());
        items
    }
}

impl<W: LayoutElement> ScrollingSpace<W> {
    /// Rebuild niri's transport containers from the authoritative Conscia groups.
    /// Tile objects, Wayland identities and their commit bookkeeping are preserved.
    pub(super) fn conscia_sync_columns(&mut self) {
        let mut tiles: Vec<_> = self.columns.drain(..).flat_map(|c| c.tiles).collect();
        self.data.clear();
        let mut groups = Vec::new();
        for &anchor in self.conscia.model.windows() {
            let members = if Some(anchor) == self.conscia.model.main() {
                self.conscia.model.main_tiles()
            } else { self.conscia.model.reel_group(anchor) };
            groups.push((anchor, members.to_vec()));
        }
        if let Some(parked) = self.conscia.parked { groups.push((parked, Vec::new())); }
        for &(dialog, _) in &self.conscia.dialogs { groups.push((dialog, Vec::new())); }
        for (anchor, members) in groups {
            let mut group = Vec::new();
            for id in std::iter::once(anchor).chain(members) {
                if let Some(index) = tiles.iter().position(|t| self.conscia.id(t.window().id()) == Some(id)) {
                    group.push(tiles.remove(index));
                }
            }
            if group.is_empty() { continue; }
            let mut col = Column::new_with_tile(group.remove(0), self.view_size,
                self.working_area, self.parent_area, self.scale, ColumnWidth::Proportion(1.), false);
            col.id = ColumnId::specific(anchor.0);
            for tile in group {
                col.data.push(TileData::new(&tile, WindowHeight::Auto { weight: 1. }));
                col.tiles.push(tile);
            }
            col.display_mode = ColumnDisplay::Normal;
            col.conscia_splits = self.conscia.model.split_ratios(anchor).to_vec();
            col.conscia_tree = self.conscia.tree(anchor).cloned().map(|tree| {
                let ids = col.tiles.iter().filter_map(|tile| self.conscia.id(tile.window().id())).collect();
                (ids, tree)
            });
            col.move_x_animation = None;
            col.move_y_animation = None;
            col.is_pending_fullscreen = false;
            col.is_pending_maximized = false;
            self.data.push(ColumnData::new(&col));
            self.columns.push(col);
        }
        self.active_column_idx = 0;
        self.conscia_sync_active();
    }

    pub(super) fn conscia_sync_active(&mut self) {
        let active = self.conscia.active();
        for (ci, col) in self.columns.iter_mut().enumerate() {
            if let Some(ti) = col.tiles.iter().position(|t| self.conscia.id(t.window().id()) == active) {
                self.active_column_idx = ci;
                col.active_tile_idx = ti;
            }
        }
        self.active_column_idx = self.active_column_idx.min(self.columns.len().saturating_sub(1));
    }

    pub fn conscia_edge_gap_reference(&self) -> f64 {
        let ring = &self.options.layout.focus_ring;
        let width = if ring.off { 0. } else {
            (ring.width * self.scale).round().max(1.) / self.scale
        };
        (f64::from(self.conscia.inset) - width).max(0.)
    }

    pub(super) fn conscia_relayout(&mut self) {
        let ring = &self.options.layout.focus_ring;
        let ring_width = if ring.off { 0. } else { ring.width };
        self.conscia.inset = (self.options.layout.gaps / 2. + ring_width).ceil().max(0.) as i32;
        // xdg transient parents may be set after the first buffer commit.
        let dialogs: Vec<_> = self.tiles().filter_map(|tile| {
            let id = self.conscia.id(tile.window().id())?;
            if self.conscia.dialogs.iter().any(|(dialog, _)| *dialog == id) { return None; }
            let owner = self.tiles().find(|parent| tile.window().id() != parent.window().id()
                && tile.window().is_child_of(parent.window()))?;
            Some((id, self.conscia.id(owner.window().id())?))
        }).collect();
        for (id, owner) in dialogs {
            self.conscia.model.remove(id, self.conscia.slots());
            self.conscia.dialogs.push((id, owner));
            self.conscia.resize = None;
            self.interactive_resize = None;
            self.conscia.transition = None;
            self.conscia.end_scroll();
        }

        if self.conscia.expanded.is_some_and(|(id, _)| !self.conscia.model.in_main(id)) {
            self.conscia.expanded = None;
        }
        if let Some(id) = self.conscia.parked {
            if self.conscia.model.insert_reel_front(id) { self.conscia.parked = None; }
        }
        let area = Rect { x: self.working_area.loc.x.round() as i32,
            y: self.working_area.loc.y.round() as i32,
            width: self.working_area.size.w.floor().max(0.) as i32,
            height: self.working_area.size.h.floor().max(0.) as i32 };
        let ids: Vec<_> = self.conscia.model.windows().iter().copied().collect();
        let Ok(mut layout) = geometry::solve_layout(area, &ids, self.conscia.model.side) else {
            self.conscia.layout = None;
            return;
        };
        // A lone Main group owns the whole work area, whether it contains one
        // application or several split windows. Restore the reel only when an
        // actual group/window remains outside Main.
        if (ids.len() == 1 && self.conscia.parked.is_none()) || self.conscia.main_maximized {
            layout.main = area;
            layout.reel_hidden = true;
            layout.origin_offset = (0., 0.);
            for placement in &mut layout.placements {
                placement.rect = area;
            }
        }
        let main = layout.main;
        self.conscia.sync_trees(main);
        if let Some(anchor) = self.conscia.model.main() {
            if let Some(tree) = self.conscia.tree(anchor) {
                layout.placements.retain(|p| p.role != WindowRole::Main);
                for (window, rect) in tree.geometry(main).0.into_iter().rev() {
                    layout.placements.insert(0, geometry::Placement {
                        window, role: WindowRole::Main, rect, scale: 1., opacity: 1.,
                    });
                }
            }
        }
        if let Some(id) = self.conscia.parked {
            if layout.slot_count > 0 {
                layout.placements.push(geometry::Placement { window: id,
                    role: WindowRole::Reel { index: 0 },
                    rect: Rect { height: layout.reel.height / layout.slot_count as i32, ..layout.reel },
                    scale: 1. / layout.slot_count as f64, opacity: 1. });
            }
        }
        if let Some((expanded, _)) = self.conscia.expanded {
            layout.main = area;
            layout.reel_hidden = true;
            layout.origin_offset = (0., 0.);
            for p in &mut layout.placements {
                if p.window == expanded {
                    p.rect = area; p.scale = 1.; p.role = WindowRole::Main;
                } else { p.role = WindowRole::Hidden; p.opacity = 0.; }
            }
        }
        self.conscia.model.clamp_offset(layout.slot_count);
        let target_scene = self.conscia.scene_for_layout(&layout);
        for col in &mut self.columns {
            col.is_pending_fullscreen = false;
            col.is_pending_maximized = false;
            for tile in &mut col.tiles {
                let Some(id) = self.conscia.id(tile.window().id()) else { continue; };
                if self.conscia.dialogs.iter().any(|(dialog, _)| *dialog == id) {
                    // Dialogs keep client-chosen dimensions and never inherit tiled state.
                    tile.window_mut().request_size((0, 0).into(), SizingMode::Normal, false, None);
                    continue;
                }
                let mode = match self.conscia.expanded {
                    Some((key, true)) if key == id => { col.is_pending_fullscreen = true; SizingMode::Fullscreen }
                    Some((key, false)) if key == id => { col.is_pending_maximized = true; SizingMode::Maximized }
                    _ => SizingMode::Normal,
                };
                // Match the padded thumbnail's aspect ratio before uniform rendering scale.
                let size = target_scene.iter().find(|item| item.window == id && item.scale > 0.)
                    .map(|item| ((item.width / item.scale).round().max(1.),
                        (item.height / item.scale).round().max(1.)).into())
                    .unwrap_or_else(|| (main.width.max(1) as f64, main.height.max(1) as f64).into());
                match mode {
                    SizingMode::Normal => tile.request_tile_size(size, false, None),
                    SizingMode::Maximized => tile.request_maximized(size, false, None),
                    SizingMode::Fullscreen => tile.window_mut().request_size(
                        (area.width.max(1), area.height.max(1)).into(), mode, false, None),
                }
            }
        }
        self.conscia.dialog_scene.clear();
        for &(dialog, owner) in self.conscia.dialogs.iter().rev() {
            let mut root = owner;
            let mut remaining = self.conscia.dialogs.len() + 1;
            while let Some((_, parent)) = self.conscia.dialogs.iter().find(|(id, _)| *id == root) {
                if remaining == 0 { break; }
                remaining -= 1;
                root = *parent;
            }
            if remaining == 0 || !self.conscia.model.in_main(root)
                || self.conscia.expanded.is_some_and(|(id, _)| id != root) { continue; }
            let size = self.columns.iter().flat_map(|c| &c.tiles)
                .find(|t| self.conscia.id(t.window().id()) == Some(dialog))
                .map(|t| t.window().size());
            let Some(size) = size else { continue; };
            if size.w <= 0 || size.h <= 0 || area.width <= 0 || area.height <= 0 { continue; }
            let scale = 1.0_f64.min(area.width as f64 / size.w as f64).min(area.height as f64 / size.h as f64);
            let width = size.w as f64 * scale;
            let height = size.h as f64 * scale;
            let x = (layout.main.x as f64 + layout.origin_offset.0 + (layout.main.width as f64 - width) / 2.)
                .clamp(area.x as f64, area.x as f64 + area.width as f64 - width);
            let y = (layout.main.y as f64 + layout.origin_offset.1 + (layout.main.height as f64 - height) / 2.)
                .clamp(area.y as f64, area.y as f64 + area.height as f64 - height);
            self.conscia.dialog_scene.push(SceneItem { window: dialog, x, y, width, height,
                scale, opacity: 1., role: WindowRole::Main });
        }
        if !self.conscia.dialog_scene.is_empty() { self.conscia.drag = None; }
        self.conscia.layout = Some(layout);
        self.conscia_sync_active();
    }

    pub(super) fn conscia_changed(&mut self, from: Vec<SceneItem>, animate: bool) {
        self.conscia.drag = None;
        self.conscia.end_scroll();
        self.conscia.reel_bounce = None;
        self.conscia.reel_bounce_offset = 0.;
        self.conscia.resize = None;
        self.interactive_resize = None;
        self.conscia_sync_columns();
        self.conscia_relayout();
        self.conscia.transition = if animate && !self.options.animations.off {
            Some((self.clock.now(), Transition { from, to: self.conscia.target_scene(), progress: 0. }))
        } else { None };
    }

    pub(super) fn conscia_can_edit(&self) -> bool {
        self.conscia.transition.is_none() && self.conscia.expanded.is_none()
            && self.conscia.resize.is_none() && self.conscia.drag.is_none() && self.conscia.dialog_scene.is_empty()
    }

    pub(super) fn conscia_promote(&mut self, index: usize) -> bool {
        if !self.conscia_can_edit() { return false; }
        let Some(front) = self.conscia.model.reel_front_index() else { return false; };
        if index < front || index - front >= self.conscia.slots() { return false; }
        let from = self.conscia.current_scene();
        if self.conscia.model.promote(index) {
            self.conscia_changed(from, true);
            true
        } else { false }
    }

    pub fn conscia_next(&mut self) -> bool {
        if !self.conscia_can_edit() { return false; }
        if let Some(id) = self.conscia.parked.take() {
            let from = self.conscia.current_scene();
            self.conscia.model.add(id);
            self.conscia.model.side = match self.conscia.model.side {
                ReelSide::Left => ReelSide::Right, ReelSide::Right => ReelSide::Left };
            self.conscia_changed(from, true);
            return true;
        }
        self.conscia.model.reel_front_index().is_some_and(|i| self.conscia_promote(i))
    }

    pub fn conscia_mirror(&mut self) {
        self.conscia.drag = None;
        self.conscia.resize = None;
        self.interactive_resize = None;
        self.conscia.model.side = match self.conscia.model.side {
            ReelSide::Left => ReelSide::Right, ReelSide::Right => ReelSide::Left };
        self.conscia.transition = None;
        self.conscia.end_scroll();
        self.conscia_relayout();
    }

    pub fn conscia_add_main(&mut self, index: usize) -> bool {
        if !self.conscia_can_edit() { return false; }
        let Some(&id) = self.conscia.model.windows().get(index) else { return false; };
        let Some(layout) = &self.conscia.layout else { return false; };
        let Some(anchor) = self.conscia.model.main() else { return false; };
        let mut ids: Vec<_> = std::iter::once(anchor).chain(self.conscia.model.main_tiles().iter().copied()).collect();
        ids.push(id);
        ids.extend_from_slice(self.conscia.model.reel_group(id));
        let preferred = self.conscia.model.active_main();
        let Some(tree) = Tree::reconcile(&ids, &self.conscia.trees, layout.main, preferred) else { return false; };
        if tree.geometry(layout.main).0.iter().any(|(_, r)| r.width <= 0 || r.height <= 0) { return false; }
        let from = self.conscia.current_scene();
        self.conscia.last_active = preferred;
        if !self.conscia.model.add_main(index, self.conscia.slots()) { return false; }
        self.conscia_changed(from, true);
        true
    }

    pub fn conscia_release(&mut self, all: bool) {
        if !self.conscia_can_edit() { return; }
        let from = self.conscia.current_scene();
        let slots = self.conscia.slots();
        let changed = if all { self.conscia.model.release_all_main(slots) }
            else { self.conscia.model.active_main().is_some_and(|id| self.conscia.model.release_main(id, slots)) };
        if changed { self.conscia_changed(from, true); }
    }

    pub fn conscia_minimize(&mut self, window: &W::Id) -> bool {
        if self.conscia.drag.is_some() { return false; }
        let Some(id) = self.conscia.id(window) else { return false; };
        if self.conscia.dialogs.iter().any(|(dialog, _)| *dialog == id) { return false; }
        let from = self.conscia.current_scene();
        if self.conscia.model.main() == Some(id) && self.conscia.model.windows().len() == 1
            && self.conscia.model.main_tiles().is_empty() {
            self.conscia.model.remove(id, self.conscia.slots());
            self.conscia.parked = Some(id);
        } else if !self.conscia.model.minimize(id) { return false; }
        self.conscia.expanded = None;
        self.conscia_changed(from, true);
        true
    }

    pub fn conscia_reel_at(&self, pos: Point<f64, Logical>) -> bool {
        if self.conscia.drag.is_some() || self.conscia.resize.is_some() { return false; }
        self.conscia.layout.as_ref().is_some_and(|l| !l.reel_hidden &&
            l.reel.contains(pos.x - l.origin_offset.0, pos.y - l.origin_offset.1))
    }

    pub fn conscia_click(&mut self, pos: Point<f64, Logical>, add: bool) -> bool {
        if !self.conscia_reel_at(pos) { return false; }
        if !self.conscia_can_edit() { return true; }
        // Hit the entire group slot, including its internal and outer gaps.
        let Some(layout) = &self.conscia.layout else { return true; };
        let scene = scene::scene(layout, self.conscia.model.offset());
        if let Some(item) = scene.iter().find(|p| matches!(p.role, WindowRole::Reel { .. })
            && p.opacity > 0. && pos.x >= p.x && pos.y >= p.y
            && pos.x < p.x + p.width && pos.y < p.y + p.height) {
            if self.conscia.parked == Some(item.window) { self.conscia_next(); return true; }
            let anchor = self.conscia.model.windows().iter().copied().find(|&id|
                id == item.window || self.conscia.model.reel_group(id).contains(&item.window));
            if let Some(index) = anchor.and_then(|id| self.conscia.model.windows().iter().position(|&w| w == id)) {
                if add { self.conscia_add_main(index); } else { self.conscia_promote(index); }
            }
        }
        true
    }

    pub fn conscia_scroll(&mut self, delta: f64, slots: bool) {
        if self.conscia.transition.is_some() || self.conscia.drag.is_some() || !self.conscia.dialog_scene.is_empty() || !delta.is_finite() || delta == 0. { return; }
        let Some(l) = &self.conscia.layout else { return; };
        if l.reel_hidden || l.slot_count == 0 { return; }
        let delta = if slots { delta } else { delta / (l.reel.height as f64 / l.slot_count as f64) };
        let count = l.slot_count;
        let offset = self.conscia.model.offset();
        let maximum = self.conscia.model.max_offset(count);
        if (delta < 0. && offset <= 0.) || (delta > 0. && offset >= maximum) {
            self.conscia_bounce_reel(delta);
            self.conscia.end_scroll(); return;
        }
        self.conscia.reel_bounce = None;
        self.conscia.reel_bounce_offset = 0.;
        let now = self.clock.now();
        if self.conscia.last_scroll.is_none_or(|last| now.saturating_sub(last) > Duration::from_millis(140))
            || self.conscia.remainder * delta < 0. { self.conscia.remainder = 0.; }
        self.conscia.last_scroll = Some(now);
        self.conscia.remainder += delta;
        let steps = self.conscia.remainder.trunc();
        self.conscia.remainder -= steps;
        self.conscia.model.set_offset(offset + steps, count);
        if steps != 0. && (self.conscia.model.offset() == 0. || self.conscia.model.offset() == maximum) {
            // Reaching the boundary only stops scrolling. A subsequent outward
            // scroll event triggers the bounce in the boundary branch above.
            self.conscia.end_scroll();
        }
    }

    fn conscia_bounce_reel(&mut self, delta: f64) {
        if self.options.animations.off { return; }
        let direction = -delta.signum();
        // Don't restart on every wheel/touchpad event: the pulse must be able
        // to finish even while the user keeps pushing against the boundary.
        if self.conscia.reel_bounce.is_some_and(|(_, current)| current == direction) { return; }
        self.conscia.reel_bounce = Some((self.clock.now(), direction));
        self.conscia.reel_bounce_offset = 0.;
    }

    pub(super) fn conscia_main_index(&mut self, index: usize) -> bool {
        let Some(tree) = self.conscia.model.main().and_then(|id| self.conscia.tree(id)) else { return false; };
        let ids = tree.leaves();
        let Some(&id) = ids.get(index) else { return false; };
        if self.conscia.transition.is_some() { return false; }
        self.conscia.model.activate_main(id);
        self.conscia_sync_active();
        true
    }

    pub(super) fn conscia_main_step(&mut self, forward: bool) -> bool {
        let Some(tree) = self.conscia.model.main().and_then(|id| self.conscia.tree(id)) else { return false; };
        let ids = tree.leaves();
        let Some(index) = ids.iter().position(|id| Some(*id) == self.conscia.model.active_main()) else { return false; };
        let next = if forward { index + 1 } else { index.wrapping_sub(1) };
        self.conscia_main_index(next)
    }

    pub(super) fn conscia_expand(&mut self, window: &W::Id, enabled: bool, fullscreen: bool) -> bool {
        let Some(id) = self.conscia.id(window) else { return false; };
        if !self.conscia.model.in_main(id) { return false; }
        if !enabled && self.conscia.expanded.is_none_or(|(key, _)| key != id) { return false; }
        let next = if enabled { Some((id, fullscreen)) } else { None };
        if self.conscia.expanded == next { return false; }
        self.conscia.drag = None;
        if enabled { self.conscia.model.activate_main(id); }
        self.conscia.expanded = next;
        self.conscia.transition = None;
        self.conscia.end_scroll();
        self.conscia.resize = None;
        self.conscia_relayout();
        true
    }

    pub fn conscia_divider(&self, pos: Point<f64, Logical>) -> Option<geometry::TileSplit> {
        if !self.conscia_can_edit() { return None; }
        let l = self.conscia.layout.as_ref()?;
        let anchor = self.conscia.model.main()?;
        let x = pos.x - l.origin_offset.0;
        let y = pos.y - l.origin_offset.1;
        if !l.main.contains(x, y) { return None; }
        self.conscia.tree(anchor)?.geometry(l.main).1.into_iter().filter(|s| {
                if s.vertical { (x - s.position as f64).abs() <= 4. && y >= s.area.y as f64
                    && y < (s.area.y + s.area.height) as f64 }
                else { (y - s.position as f64).abs() <= 4. && x >= s.area.x as f64
                    && x < (s.area.x + s.area.width) as f64 }
            }).min_by(|a, b| {
                let distance = |s: &geometry::TileSplit| (if s.vertical { x } else { y } - s.position as f64).abs();
                distance(a).total_cmp(&distance(b)).then(a.index.cmp(&b.index))
            })
    }

    pub fn conscia_resize_start(&mut self, pos: Point<f64, Logical>) -> bool {
        let Some(split) = self.conscia_divider(pos) else { return false; };
        let l = self.conscia.layout.as_ref().unwrap();
        let anchor = self.conscia.model.main().unwrap();
        let minimum = self.conscia.tree(anchor).unwrap().geometry(l.main).0.iter().map(|(_, r)| r.width.min(r.height))
            .min().unwrap_or(1).clamp(1, 32);
        self.conscia.resize = Some((split, pos, minimum));
        self.conscia.end_scroll();
        true
    }

    pub fn conscia_resize_motion(&mut self, pos: Point<f64, Logical>) -> bool {
        let Some((split, start_pos, minimum)) = self.conscia.resize else { return false; };
        let Some(l) = &self.conscia.layout else { return false; };
        let Some(anchor) = self.conscia.model.main() else { return false; };
        let (delta, start, extent) = if split.vertical {
            (pos.x - start_pos.x, split.area.x, split.area.width)
        } else { (pos.y - start_pos.y, split.area.y, split.area.height) };
        if extent <= 1 { return true; }
        let desired = ((split.position as f64 + delta - start as f64) / extent as f64).clamp(0.01, 0.99);
        let Some(tree) = self.conscia.tree(anchor) else { return false; };
        let fits = |ratio: f64| {
            let mut candidate = tree.clone();
            candidate.set_ratio(split.index, ratio);
            candidate.geometry(l.main).0.iter()
                .all(|(_, r)| r.width >= minimum && r.height >= minimum)
        };
        let Some(current) = tree.geometry(l.main).1.get(split.index).copied() else { return false; };
        let current = (current.position - start) as f64 / extent as f64;
        let ratio = if fits(desired) { desired } else {
            let (mut good, mut bad) = (current, desired);
            for _ in 0..24 {
                let middle = (good + bad) / 2.;
                if fits(middle) { good = middle; } else { bad = middle; }
            }
            good
        };
        if let Some(tree) = self.conscia.trees.iter_mut().find(|tree| tree.contains(anchor)) {
            tree.set_ratio(split.index, ratio);
        }
        self.conscia_relayout();
        true
    }

    pub fn conscia_resize_end(&mut self) -> bool {
        self.conscia.resize.take().is_some()
    }

    pub(super) fn conscia_resize_dimension(&mut self, window: Option<&W::Id>, change: SizeChange, vertical: bool) {
        if !self.conscia_can_edit() { return; }
        let Some(id) = window.and_then(|w| self.conscia.id(w)).or_else(|| self.conscia.model.active_main()) else { return; };
        let Some(layout) = &self.conscia.layout else { return; };
        let Some(anchor) = self.conscia.model.main() else { return; };
        let Some(tree) = self.conscia.tree(anchor) else { return; };
        let (rects, splits) = tree.geometry(layout.main);
        let Some((_, rect)) = rects.iter().find(|(key, _)| *key == id) else { return; };
        let x = rect.x as f64 + rect.width as f64 / 2.;
        let y = rect.y as f64 + rect.height as f64 / 2.;
        let Some(split) = splits.iter().rev().find(|s| s.vertical == vertical && s.area.contains(x, y)) else { return; };
        let (extent, start, current, coordinate) = if vertical {
            (split.area.width as f64, split.area.x, rect.width as f64, x)
        } else { (split.area.height as f64, split.area.y, rect.height as f64, y) };
        if extent <= 1. { return; }
        let desired = match change {
            SizeChange::SetFixed(v) => v as f64,
            SizeChange::AdjustFixed(v) => current + v as f64,
            SizeChange::SetProportion(v) => extent * v / 100.,
            SizeChange::AdjustProportion(v) => current + extent * v / 100.,
        };
        let sign = if coordinate < split.position as f64 { 1. } else { -1. };
        let initial = (split.position - start) as f64 / extent;
        let wanted = (initial + sign * (desired - current) / extent).clamp(0.01, 0.99);
        let minimum = rects.iter().map(|(_, r)| r.width.min(r.height)).min().unwrap_or(1).clamp(1, 32);
        let fits = |ratio| {
            let mut candidate = tree.clone();
            candidate.set_ratio(split.index, ratio);
            candidate.geometry(layout.main).0.iter().all(|(_, r)| r.width >= minimum && r.height >= minimum)
        };
        let (mut good, mut bad) = (initial, wanted);
        if fits(wanted) { good = wanted; }
        else {
            for _ in 0..24 {
                let middle = (good + bad) / 2.;
                if fits(middle) { good = middle; } else { bad = middle; }
            }
        }
        let index = split.index;
        let from = self.conscia.current_scene();
        if let Some(tree) = self.conscia.trees.iter_mut().find(|tree| tree.contains(anchor)) { tree.set_ratio(index, good); }
        self.conscia_changed(from, true);
    }

    pub fn conscia_keyboard_direction(&mut self, dx: i32, dy: i32, swap: bool) -> bool {
        if !self.conscia_can_edit() { return false; }
        let Some(active) = self.conscia.model.active_main() else { return false; };
        let Some(tree) = self.conscia.model.main().and_then(|id| self.conscia.tree(id)) else { return false; };
        let Some(layout) = &self.conscia.layout else { return false; };
        let rects = tree.geometry(layout.main).0;
        let Some((_, origin)) = rects.iter().find(|(id, _)| *id == active) else { return false; };
        let cx = origin.x as f64 + origin.width as f64 / 2.;
        let cy = origin.y as f64 + origin.height as f64 / 2.;
        let target = rects.iter().filter(|(id, _)| *id != active).filter_map(|(id, rect)| {
            let x = rect.x as f64 + rect.width as f64 / 2.;
            let y = rect.y as f64 + rect.height as f64 / 2.;
            let forward = (x - cx) * dx as f64 + (y - cy) * dy as f64;
            if forward <= 0. { return None; }
            let aligned = if dx != 0 {
                rect.y < origin.y + origin.height && origin.y < rect.y + rect.height
            } else { rect.x < origin.x + origin.width && origin.x < rect.x + rect.width };
            let cross = if dx != 0 { (y - cy).abs() } else { (x - cx).abs() };
            Some((*id, (!aligned, forward, cross)))
        }).min_by(|a, b| a.1.0.cmp(&b.1.0).then(a.1.1.total_cmp(&b.1.1)).then(a.1.2.total_cmp(&b.1.2)))
            .map(|(id, _)| id);
        let Some(target) = target else { return false; };
        if swap {
            let Some(next) = tree.rearrange(active, target, Dock::Center) else { return false; };
            let from = self.conscia.current_scene();
            self.conscia.trees.retain(|tree| !tree.contains(active));
            self.conscia.trees.push(next);
            self.conscia_changed(from, true);
        } else {
            self.conscia.model.activate_main(target);
            self.conscia_sync_active();
        }
        true
    }

    pub fn conscia_keyboard_split(&mut self, swap: bool) -> bool {
        if !self.conscia_can_edit() { return false; }
        let Some(active) = self.conscia.model.active_main() else { return false; };
        let Some(layout) = &self.conscia.layout else { return false; };
        let Some(mut candidate) = self.conscia.tree(active).cloned() else { return false; };
        let minimum = candidate.geometry(layout.main).0.iter()
            .map(|(_, r)| r.width.min(r.height)).min().unwrap_or(1).clamp(1, 32);
        if !candidate.edit_parent_split(active, swap)
            || candidate.geometry(layout.main).0.iter().any(|(_, r)| r.width < minimum || r.height < minimum) {
            return false;
        }
        let from = self.conscia.current_scene();
        let Some(tree) = self.conscia.trees.iter_mut().find(|tree| tree.contains(active)) else { return false; };
        *tree = candidate;
        self.conscia_changed(from, true);
        true
    }

    /// Stable touch target, role, title strip and outward direction in output coordinates.
    pub fn conscia_touch_target(&self, pos: Point<f64, Logical>) -> (Option<W::Id>, bool, bool, f64) {
        if !self.conscia.dialog_scene.is_empty() || self.conscia.expanded.is_some() {
            return (None, false, false, 0.);
        }
        let reel = self.conscia_reel_at(pos);
        let side = if self.conscia.model.side == ReelSide::Left { -1. } else { 1. };
        let item = self.conscia.current_scene().into_iter().find(|p| p.opacity > 0.
            && pos.x >= p.x && pos.x < p.x + p.width && pos.y >= p.y && pos.y < p.y + p.height);
        // Retain the existing group-slot hit area for gaps between Reel tiles.
        let item = item.or_else(|| {
            if !reel { return None; }
            let layout = self.conscia.layout.as_ref()?;
            scene::scene(layout, self.conscia.model.offset()).into_iter().find(|p|
                matches!(p.role, WindowRole::Reel { .. }) && p.opacity > 0.
                && pos.x >= p.x && pos.x < p.x + p.width && pos.y >= p.y && pos.y < p.y + p.height)
        });
        let title = !reel && item.as_ref().is_some_and(|p| p.role == WindowRole::Main && pos.y < p.y + 32.);
        let id = item.and_then(|p| self.conscia.ids.iter().find(|(_, id)| *id == p.window).map(|(id, _)| id.clone()));
        (id, reel, title, if reel { side } else { -side })
    }

    pub fn conscia_touch_matches(&self, window: &W::Id, reel: bool) -> bool {
        if !self.conscia.dialog_scene.is_empty() || self.conscia.expanded.is_some() { return false; }
        self.conscia.id(window).is_some_and(|id| {
            if reel { self.conscia.parked == Some(id) || (self.conscia.model.windows().iter().any(|&w| w == id || self.conscia.model.reel_group(w).contains(&id)) && !self.conscia.model.in_main(id)) }
            else { self.conscia.model.in_main(id) }
        })
    }

    pub fn conscia_touch_main_at(&self, pos: Point<f64, Logical>) -> bool {
        self.conscia.layout.as_ref().is_some_and(|l| l.main.contains(pos.x - l.origin_offset.0, pos.y - l.origin_offset.1))
    }

    pub fn conscia_touch_expand(&mut self, expanded: bool) {
        if self.conscia.main_maximized != expanded { self.toggle_full_width(); }
    }

    pub fn conscia_touch_promote(&mut self, window: &W::Id) {
        let Some(id) = self.conscia.id(window) else { return; };
        if self.conscia.parked == Some(id) { self.conscia_next(); return; }
        let anchor = self.conscia.model.windows().iter().position(|&w| w == id || self.conscia.model.reel_group(w).contains(&id));
        if let Some(index) = anchor { self.conscia_promote(index); }
    }

    pub fn conscia_touch_add(&mut self, window: &W::Id) {
        let Some(id) = self.conscia.id(window) else { return; };
        let anchor = self.conscia.model.windows().iter().position(|&w| w == id || self.conscia.model.reel_group(w).contains(&id));
        if let Some(index) = anchor { self.conscia_add_main(index); }
    }

    pub fn conscia_touch_release(&mut self, window: &W::Id) {
        if self.conscia.model.main_tiles().is_empty() || !self.conscia_can_edit() { return; }
        let Some(id) = self.conscia.id(window) else { return; };
        let from = self.conscia.current_scene();
        if self.conscia.model.release_main(id, self.conscia.slots()) { self.conscia_changed(from, true); }
    }

    pub fn conscia_drag_start(&mut self, pos: Point<f64, Logical>) -> bool {
        if !self.conscia_can_edit() { return false; }
        let Some(origin) = self.conscia.target_scene().into_iter().find(|item|
            item.role == WindowRole::Main && item.opacity > 0.
                && pos.x >= item.x && pos.x < item.x + item.width
                && pos.y >= item.y && pos.y < item.y + item.height) else { return false; };
        let Some(anchor) = self.conscia.model.main() else { return false; };
        let Some(base) = self.conscia.tree(anchor).cloned() else { return false; };
        self.conscia.model.activate_main(origin.window);
        self.conscia.last_active = Some(origin.window);
        self.conscia_sync_active();
        self.conscia.drag = Some(Drag { window: origin.window, start: pos, pointer: pos,
            origin, base, preview: None, moved: false, hint: None });
        true
    }

    pub fn conscia_drag_start_window(&mut self, window: &W::Id, pos: Point<f64, Logical>) -> bool {
        let Some(id) = self.conscia.id(window) else { return false; };
        if self.conscia.model.main_tiles().is_empty() || !self.conscia.model.in_main(id) { return false; }
        self.conscia_drag_start(pos)
    }

    pub fn conscia_drag_active(&self) -> bool {
        self.conscia.drag.is_some()
    }

    pub fn conscia_drag_motion(&mut self, pos: Point<f64, Logical>) -> bool {
        let now = self.clock.now();
        let animate = !self.options.animations.off;
        let inset = self.conscia.inset;
        let Some(layout) = &self.conscia.layout else { return false; };
        let Some(drag) = &mut self.conscia.drag else { return false; };
        drag.pointer = pos;
        let distance = pos - drag.start;
        drag.moved |= distance.x * distance.x + distance.y * distance.y >= 64.;
        if !drag.moved { return true; }
        drag.preview = None;
        let (x, y) = (pos.x - layout.origin_offset.0, pos.y - layout.origin_offset.1);
        if !layout.main.contains(x, y) { drag.set_hint(None, now, animate); return true; }
        let geometry = drag.base.geometry(layout.main).0;
        let Some(&(target, rect)) = geometry.iter().find(|(id, rect)| *id != drag.window && rect.contains(x, y))
            else { drag.set_hint(None, now, animate); return true; };
        let nx = (x - rect.x as f64) / rect.width.max(1) as f64;
        let ny = (y - rect.y as f64) / rect.height.max(1) as f64;
        let dock = if (0.25..=0.75).contains(&nx) && (0.25..=0.75).contains(&ny) { Dock::Center }
            else if nx.min(1. - nx) < ny.min(1. - ny) {
                if nx < 0.5 { Dock::Left } else { Dock::Right }
            } else if ny < 0.5 { Dock::Top } else { Dock::Bottom };
        let Some(preview) = drag.base.swap_group_at(drag.window, layout.main, x, y)
            .or_else(|| drag.base.rearrange(drag.window, target, dock)) else {
            drag.set_hint(None, now, animate); return true;
        };
        let minimum = geometry.iter().map(|(_, rect)| rect.width.min(rect.height)).min().unwrap_or(1).clamp(1, 32);
        let placements = preview.geometry(layout.main).0;
        if placements.iter().all(|(_, r)| r.width >= minimum && r.height >= minimum) {
            let hint = placements.iter().find(|(id, _)| *id == drag.window).map(|(_, rect)| {
                let rect = geometry::inset_in_group(*rect, layout.main, (1., 1.), inset as f64);
                Rectangle::new((layout.main.x as f64 + rect.x + layout.origin_offset.0,
                    layout.main.y as f64 + rect.y + layout.origin_offset.1).into(),
                    (rect.width, rect.height).into())
            });
            drag.set_hint(hint, now, animate);
            drag.preview = Some(preview);
        } else { drag.set_hint(None, now, animate); }
        true
    }

    pub fn conscia_drag_end(&mut self, cancel: bool) -> bool {
        if self.conscia.drag.is_none() { return false; }
        let from = self.conscia.current_scene();
        let drag = self.conscia.drag.take().unwrap();
        if !cancel {
            if let Some(tree) = drag.preview {
                self.conscia.trees.retain(|tree| !tree.contains(drag.window));
                self.conscia.trees.push(tree);
            }
        }
        if drag.moved { self.conscia_changed(from, true); }
        true
    }
}
