use std::iter::{self, zip};
use std::rc::Rc;
use std::time::Duration;

use niri_config::{CenterFocusedColumn, PresetSize, Struts};
use niri_ipc::{ColumnDisplay, SizeChange, WindowLayout};
use ordered_float::NotNan;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Point, Rectangle, Scale, Serial, Size};

use super::closing_window::{ClosingWindow, ClosingWindowRenderElement};
use super::insert_hint_element::{InsertHintElement, InsertHintRenderElement};
use super::monitor::InsertPosition;
use super::tab_indicator::{TabIndicator, TabIndicatorRenderElement, TabInfo};
use super::tile::{Tile, TileRenderElement};
use super::workspace::{InteractiveResize, ResolvedSize};
use super::{ConfigureIntent, HitType, InteractiveResizeData, LayoutElement, Options, RemovedTile};
use crate::animation::{Animation, Clock};
use crate::layout::{RenderLayer, SizingMode};
use crate::niri_render_elements;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::RenderCtx;
use crate::utils::id::IdCounter;
use crate::utils::transaction::{Transaction, TransactionBlocker};
use crate::utils::ResizeEdge;
use crate::window::ResolvedWindowRules;

mod conscia;

/// Conscia window layout with column containers for workspace transport.
///
/// Public layout adapters retain the parameters expected by workspace and IPC
/// callers; Conscia determines placement and animations from its own state.
#[derive(Debug)]
pub struct ScrollingSpace<W: LayoutElement> {
    conscia: conscia::State<W::Id>,
    snap_hint_element: InsertHintElement,
    reel_icons: crate::render_helpers::app_icon::AppIconCache,
    /// Columns of windows on this space.
    columns: Vec<Column<W>>,

    /// Extra per-column data.
    data: Vec<ColumnData>,

    /// Index of the currently active column, if any.
    active_column_idx: usize,

    /// Ongoing interactive resize.
    interactive_resize: Option<InteractiveResize<W>>,

    /// Windows in the closing animation.
    closing_windows: Vec<ClosingWindow>,

    /// View size for this space.
    view_size: Size<f64, Logical>,

    /// Working area for this space.
    ///
    /// Takes into account layer-shell exclusive zones and niri struts.
    working_area: Rectangle<f64, Logical>,

    /// Working area for this space excluding struts.
    ///
    /// Used for popup unconstraining. Popups can go over struts, but they shouldn't go over
    /// the layer-shell top layer (which renders on top of popups).
    parent_area: Rectangle<f64, Logical>,

    /// Scale of the output the space is on (and rounds its sizes to).
    scale: f64,

    /// Clock for driving animations.
    clock: Clock,

    /// Configurable properties of the layout.
    options: Rc<Options>,
}

niri_render_elements! {
    ScrollingSpaceRenderElement<R> => {
        Tile = TileRenderElement<R>,
        ClosingWindow = ClosingWindowRenderElement,
        TabIndicator = TabIndicatorRenderElement,
        SnapHint = InsertHintRenderElement,
        ReelIcon = smithay::backend::renderer::element::utils::CropRenderElement<
            smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement<R>>,
        Conscia = smithay::backend::renderer::element::utils::CropRenderElement<
            smithay::backend::renderer::element::utils::RelocateRenderElement<
                smithay::backend::renderer::element::utils::RescaleRenderElement<TileRenderElement<R>>>>,
    }
}

/// Extra per-column data.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ColumnData {
    /// Cached actual column width.
    width: f64,
}

static COLUMN_ID_COUNTER: IdCounter = IdCounter::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ColumnId(u64);

impl ColumnId {
    fn next() -> ColumnId {
        ColumnId(COLUMN_ID_COUNTER.next())
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub fn specific(id: u64) -> Self {
        Self(id)
    }
}

#[derive(Debug)]
pub struct Column<W: LayoutElement> {
    conscia_tree: Option<(Vec<conscia::core::WindowId>, conscia::tiling::Tree)>,
    /// Tiles in this column.
    ///
    /// Must be non-empty.
    tiles: Vec<Tile<W>>,

    /// Extra per-tile data.
    ///
    /// Must have the same number of elements as `tiles`.
    data: Vec<TileData>,

    /// Index of the currently active tile.
    active_tile_idx: usize,

    /// Desired width of this column.
    ///
    /// If the column is full-width or full-screened, this is the width that should be restored
    /// upon unfullscreening and untoggling full-width.
    width: ColumnWidth,

    /// Whether this column is full-width.
    is_full_width: bool,

    /// Whether this column is going to be fullscreen.
    ///
    /// This is the compositor-side fullscreen state, so it changes immediately upon
    /// set_fullscreen(). The actual tiles will take some time to respond to the fullscreen request
    /// and become fullscreen.
    ///
    /// Similarly, unsetting fullscreen will change this value to false immediately, and tiles will
    /// take some time to catch up and actually unfullscreen.
    is_pending_fullscreen: bool,

    /// Whether this column is going to be maximized.
    ///
    /// Can be `true` together with `is_pending_fullscreen`, which means that the column is
    /// effectively pending fullscreen, but unfullscreening should go back to maximized state,
    /// rather than normal.
    is_pending_maximized: bool,

    /// How this column displays and arranges windows.
    display_mode: ColumnDisplay,

    /// Tab indicator for the tabbed display mode.
    tab_indicator: TabIndicator,

    /// Animation of the render offset during window swapping.
    move_x_animation: Option<MoveAnimation>,

    /// Animation of a column visually moving vertically.
    move_y_animation: Option<MoveAnimation>,

    /// Latest known view size for this column's workspace.
    view_size: Size<f64, Logical>,

    /// Latest known working area for this column's workspace.
    working_area: Rectangle<f64, Logical>,

    /// Working area for this column's workspace excluding struts.
    ///
    /// Used for maximize-to-edges.
    parent_area: Rectangle<f64, Logical>,

    /// Scale of the output the column is on (and rounds its sizes to).
    scale: f64,

    /// Clock for driving animations.
    clock: Clock,

    /// Configurable properties of the layout.
    options: Rc<Options>,

    /// Unique ID of this column.
    id: ColumnId,
}

/// Extra per-tile data.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TileData {
    /// Requested height of the window.
    ///
    /// This is window height, not tile height, so it excludes tile decorations.
    height: WindowHeight,

    /// Cached actual size of the tile.
    size: Size<f64, Logical>,

    /// Cached whether the tile is being interactively resized by its left edge.
    interactively_resizing_by_left_edge: bool,
}

/// Width of a column.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColumnWidth {
    /// Proportion of the current view width.
    Proportion(f64),
    /// Fixed width in logical pixels.
    Fixed(f64),
}

/// Height of a window in a column.
///
/// Every window but one in a column must be `Auto`-sized so that the total height can add up to
/// the workspace height. Resizing a window converts all other windows to `Auto`, weighted to
/// preserve their visual heights at the moment of the conversion.
///
/// In contrast to column widths, proportional height changes are converted to, and stored as,
/// fixed height right away. With column widths you frequently want e.g. two columns side-by-side
/// with 50% width each, and you want them to remain this way when moving to a differently sized
/// monitor. Windows in a column, however, already auto-size to fill the available height, giving
/// you this behavior. The main reason to set a different window height, then, is when you want
/// something in the window to fit exactly, e.g. to fit 30 lines in a terminal, which corresponds
/// to the `Fixed` variant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowHeight {
    /// Automatically computed *tile* height, distributed across the column according to weights.
    ///
    /// This controls the tile height rather than the window height because it's easier in the auto
    /// height distribution algorithm.
    Auto { weight: f64 },
    /// Fixed *window* height in logical pixels.
    Fixed(f64),
    /// One of the preset heights (tile or window).
    Preset(usize),
}

/// Horizontal direction for an operation.
///
/// As operations often have a symmetrical counterpart, e.g. focus-right/focus-left, methods
/// on `Scrolling` can sometimes be factored using the direction of the operation as a parameter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrollDirection {
    Left,
    Right,
}

#[derive(Debug)]
struct MoveAnimation {
    anim: Animation,
    from: f64,
    /// Whether this animation is for moving the tile between workspaces.
    ///
    /// Controls whether the tile is rendered uncropped and above others.
    is_between_workspaces: bool,
}

impl<W: LayoutElement> ScrollingSpace<W> {
    pub fn new(
        view_size: Size<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        clock: Clock,
        options: Rc<Options>,
    ) -> Self {
        let working_area = compute_working_area(parent_area, scale, options.layout.struts);

        Self {
            conscia: conscia::State::new(),
            snap_hint_element: InsertHintElement::new(options.layout.insert_hint),
            reel_icons: Default::default(),
            columns: Vec::new(),
            data: Vec::new(),
            active_column_idx: 0,
            interactive_resize: None,
            closing_windows: Vec::new(),
            view_size,
            working_area,
            parent_area,
            scale,
            clock,
            options,
        }
    }

    pub fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        let changed = self.view_size != view_size || self.parent_area != parent_area || self.scale != scale
            || self.working_area != compute_working_area(parent_area, scale, options.layout.struts);
        self.view_size = view_size;
        self.parent_area = parent_area;
        self.working_area = compute_working_area(parent_area, scale, options.layout.struts);
        self.scale = scale;
        self.options = options;
        self.snap_hint_element.update_config(self.options.layout.insert_hint);
        for col in &mut self.columns {
            col.update_config(view_size, self.working_area, parent_area, scale, self.options.clone());
        }
        if changed || self.options.animations.off {
            self.conscia.drag = None;
            self.conscia.transition = None;
            self.conscia.reel_bounce = None;
            self.conscia.reel_bounce_offset = 0.;
            self.conscia.resize = None;
            self.conscia.end_scroll();
        }
        self.conscia_relayout();
    }

    pub fn update_shaders(&mut self) {
        self.snap_hint_element.update_shaders();
        for col in &mut self.columns {
            col.update_shaders();
        }
    }

    pub fn advance_animations(&mut self) {
        let now = self.clock.now();
        self.conscia.advance_snap_hint(now);
        if let Some((start, transition)) = &mut self.conscia.transition {
            transition.progress = (now.saturating_sub(*start).as_secs_f32() / 0.22).min(1.);
            if transition.progress >= 1. { self.conscia.transition = None; }
        }
        if let Some((start, direction)) = self.conscia.reel_bounce {
            let progress = now.saturating_sub(start).as_secs_f64() / 0.30;
            if progress >= 1. {
                self.conscia.reel_bounce = None;
                self.conscia.reel_bounce_offset = 0.;
            } else {
                let height = self.conscia.layout.as_ref().map_or(0., |l| l.reel.height as f64);
                let amplitude = (height * 0.08).min(32.);
                self.conscia.reel_bounce_offset = direction * amplitude
                    * (std::f64::consts::PI * progress).sin().powi(2);
            }
        }
        if !self.conscia.scroll_active && self.conscia.last_scroll.is_some_and(|last| now.saturating_sub(last) > Duration::from_millis(140)) {
            self.conscia_scroll_finish();
        }
        if let Some((start, from, to)) = self.conscia.reel_snap {
            let t = (now.saturating_sub(start).as_secs_f64() / 0.18).min(1.);
            let t = t * t * (3. - 2. * t);
            self.conscia.model.set_offset(from + (to - from) * t, self.conscia.slots());
            if t >= 1. { self.conscia.reel_snap = None; }
        }
        self.closing_windows.retain_mut(|closing| {
            closing.advance_animations();
            closing.are_animations_ongoing()
        });
        for tile in self.tiles_mut() { tile.advance_animations(); }
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.conscia.transition.is_some() || self.conscia.reel_bounce.is_some()
            || self.conscia.snap_hint_animating()
            || !self.closing_windows.is_empty()
            || self.conscia.reel_snap.is_some()
            || (!self.conscia.scroll_active && self.conscia.last_scroll.is_some())
            || self.reel_icons.is_loading()
            || self.tiles().any(Tile::are_animations_ongoing)
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        !self.closing_windows.is_empty() || self.conscia.transition.is_some()
    }

    pub fn update_render_elements(&mut self, is_active: bool, _layer: RenderLayer) {
        if let Some((rect, alpha)) = self.conscia.snap_hint() {
            self.snap_hint_element.update_render_elements_with_alpha(
                rect.size, Rectangle::new(Point::from((-rect.loc.x, -rect.loc.y)), self.view_size),
                niri_config::CornerRadius::from(12.), self.scale, alpha,
            );
        }
        let scene = self.conscia.current_scene();
        let active = self.conscia.active();
        let state = &self.conscia;
        for col in &mut self.columns {
            for tile in &mut col.tiles {
                let id = state.id(tile.window().id());
                if scene.iter().any(|p| Some(p.window) == id && p.opacity > 0.
                    && matches!(p.role, conscia::geometry::WindowRole::Reel { .. })) {
                    self.reel_icons.get(tile.window().app_id().as_deref(), self.scale);
                }
                let rect = scene.iter().find(|p| Some(p.window) == id)
                    .map(|p| Rectangle::new((-p.x, -p.y).into(), self.view_size))
                    .unwrap_or_else(|| Rectangle::from_size(self.view_size));
                let size = tile.tile_size();
                let visual_scale = scene.iter().find(|p| Some(p.window) == id)
                    .filter(|_| size.w > 0. && size.h > 0.)
                    .map_or(1., |p| (p.width / size.w).min(p.height / size.h));
                tile.update_decorated_render_elements(is_active && id == active, rect, visual_scale);
            }
        }
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        self.columns.iter().flat_map(|col| col.tiles.iter())
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        self.columns.iter_mut().flat_map(|col| col.tiles.iter_mut())
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    pub fn active_window(&self) -> Option<&W> {
        if self.conscia.transition.is_some() { return None; }
        let id = self.conscia.active()?;
        let native = self.conscia.ids.iter().find(|(_, key)| *key == id)?.0.clone();
        self.tiles().find(|tile| tile.window().id() == &native).map(|tile| tile.window())
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        if self.conscia.transition.is_some() { return None; }
        let id = self.conscia.active()?;
        let native = self.conscia.ids.iter().find(|(_, key)| *key == id)?.0.clone();
        self.tiles_mut().find(|tile| tile.window().id() == &native).map(|tile| tile.window_mut())
    }

    pub fn active_tile_mut(&mut self) -> Option<&mut Tile<W>> {
        if self.conscia.transition.is_some() { return None; }
        let id = self.conscia.active()?;
        let native = self.conscia.ids.iter().find(|(_, key)| *key == id)?.0.clone();
        self.tiles_mut().find(|tile| tile.window().id() == &native).map(|tile| tile)
    }

    pub fn is_active_pending_fullscreen(&self) -> bool {
        self.conscia.expanded.is_some_and(|(_, fullscreen)| fullscreen)
    }

    pub fn new_window_toplevel_bounds(&self, _rules: &ResolvedWindowRules) -> Size<i32, Logical> {
        let area = conscia::geometry::Rect { x: 0, y: 0,
            width: self.working_area.size.w.floor().max(0.) as i32,
            height: self.working_area.size.h.floor().max(0.) as i32 };
        let rect = conscia::geometry::solve_layout(area, &[], self.conscia.model.side)
            .map_or(area, |l| l.main);
        (rect.width.max(1), rect.height.max(1)).into()
    }

    pub fn new_window_size(
        &self,
        _width: Option<PresetSize>,
        _height: Option<PresetSize>,
        rules: &ResolvedWindowRules,
    ) -> Size<i32, Logical> {
        self.new_window_toplevel_bounds(rules)
    }

    pub fn is_centering_focused_column(&self) -> bool {
        false
    }

    pub(super) fn insert_position(&self, pos: Point<f64, Logical>) -> InsertPosition {
        if self.conscia.layout.as_ref().is_some_and(|l| l.main.contains(pos.x, pos.y)) && !self.columns.is_empty() {
            InsertPosition::InColumn(0, self.columns[0].tiles.len())
        } else { InsertPosition::NewColumn(self.columns.len()) }
    }

    pub fn add_tile(
        &mut self,
        col_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
        width: ColumnWidth,
        is_full_width: bool,
        anim: Option<niri_config::Animation>,
    ) {
        let column = Column::new_with_tile(
            tile,
            self.view_size,
            self.working_area,
            self.parent_area,
            self.scale,
            width,
            is_full_width,
        );

        self.add_column(col_idx, column, activate, anim);
    }

    pub fn add_tile_to_column(
        &mut self,
        col_idx: usize,
        _tile_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
    ) {
        let anchor = self.columns.get(col_idx).and_then(|c| c.tiles.first())
            .and_then(|t| self.conscia.id(t.window().id()));
        let id = self.conscia.register(tile.window().id());
        let from = self.conscia.current_scene();
        if let Some(anchor) = anchor {
            self.conscia.model.import_member(anchor, id, activate);
        } else { self.conscia.model.add(id); }
        let col = Column::new_with_tile(tile, self.view_size, self.working_area,
            self.parent_area, self.scale, ColumnWidth::Proportion(1.), false);
        self.columns.push(col);
        self.conscia_changed(from, false);
    }

    pub fn add_tile_right_of(
        &mut self,
        _right_of: &W::Id,
        tile: Tile<W>,
        activate: bool,
        width: ColumnWidth,
        is_full_width: bool,
        anim: Option<niri_config::Animation>,
    ) {
        self.add_tile(None, tile, activate, width, is_full_width, anim);
    }

    pub fn add_column(
        &mut self,
        _idx: Option<usize>,
        mut column: Column<W>,
        activate: bool,
        _anim_config: Option<niri_config::Animation>,
    ) {
        let from = self.conscia.current_scene();
        let native = column.tiles[column.active_tile_idx].window().id().clone();
        let owner = self.tiles().find(|parent| column.tiles[0].window().is_child_of(parent.window()))
            .and_then(|parent| self.conscia.id(parent.window().id()));
        let ids: Vec<_> = column.tiles.iter().map(|t| self.conscia.register(t.window().id())).collect();
        if let Some(owner) = owner {
            for &id in &ids { self.conscia.dialogs.push((id, owner)); }
        } else {
            self.conscia.model.import_group(&ids, activate, self.conscia.slots());
            if let Some((old_ids, mut tree)) = column.conscia_tree.take() {
                tree.remap(&old_ids, &ids);
                self.conscia.trees.push(tree);
            }
        }
        self.columns.push(column);
        self.conscia.transition = None;
        if activate {
            self.conscia.expanded = None;
            if let Some(id) = self.conscia.id(&native) { self.conscia.model.activate_main(id); }
        }
        self.conscia_changed(from, owner.is_none() && activate);
        if let Some((_, transition)) = &mut self.conscia.transition {
            let incoming = transition.to.iter().filter(|item| ids.contains(&item.window));
            let offset = match self.conscia.model.side {
                conscia::core::ReelSide::Left => incoming.map(|item| item.x)
                    .reduce(f64::min)
                    .map(|left| self.working_area.loc.x + self.working_area.size.w - left),
                conscia::core::ReelSide::Right => incoming.map(|item| item.x + item.width)
                    .reduce(f64::max)
                    .map(|right| self.working_area.loc.x - right),
            };
            if let Some(offset) = offset {
                // Enter from the focus area's outer edge while the previous
                // group moves into the reel on the opposite side.
                for item in &transition.to {
                    if !ids.contains(&item.window) { continue; }
                    let mut start = item.clone();
                    start.x += offset;
                    transition.from.push(start);
                }
            }
        }
    }

    pub fn remove_tile(&mut self, window: &W::Id, transaction: Transaction) -> RemovedTile<W> {
        let column_idx = self
            .columns
            .iter()
            .position(|col| col.contains(window))
            .unwrap();
        let column = &self.columns[column_idx];

        let tile_idx = column.position(window).unwrap();
        self.remove_tile_by_idx(column_idx, tile_idx, transaction, None)
    }

    pub fn remove_tile_by_idx(
        &mut self,
        column_idx: usize,
        tile_idx: usize,
        _transaction: Transaction,
        _anim_config: Option<niri_config::Animation>,
    ) -> RemovedTile<W> {
        let from = self.conscia.current_scene();
        let col = &mut self.columns[column_idx];
        let tile = col.tiles.remove(tile_idx);
        col.data.remove(tile_idx);
        let width = col.width;
        let is_full_width = col.is_full_width;
        let id = self.conscia.id(tile.window().id());
        if let Some(id) = id {
            self.conscia.model.remove(id, self.conscia.slots());
            self.conscia.ids.retain(|(_, key)| *key != id);
            self.conscia.dialogs.retain(|(dialog, _)| *dialog != id);
        }
        self.conscia_changed(from, true);
        RemovedTile { tile, width, is_full_width, is_floating: false }
    }

    pub fn active_column(&self) -> Option<&Column<W>> {
        if self.columns.is_empty() {
            return None;
        }

        Some(&self.columns[self.active_column_idx])
    }

    pub fn remove_active_column(&mut self) -> Option<Column<W>> {
        if self.columns.is_empty() {
            return None;
        }

        Some(self.remove_column_by_idx(self.active_column_idx, None))
    }

    pub fn remove_column_by_idx(
        &mut self,
        column_idx: usize,
        _anim_config: Option<niri_config::Animation>,
    ) -> Column<W> {
        let from = self.conscia.current_scene();
        let mut col = self.columns.remove(column_idx);
        if let Some(anchor) = col.tiles.first().and_then(|t| self.conscia.id(t.window().id())) {
            col.conscia_tree = self.conscia.tree(anchor).cloned().map(|tree| {
                let ids = col.tiles.iter().filter_map(|tile| self.conscia.id(tile.window().id())).collect();
                (ids, tree)
            });
        }
        for tile in &col.tiles {
            if let Some(id) = self.conscia.id(tile.window().id()) {
                self.conscia.model.remove(id, self.conscia.slots());
                self.conscia.ids.retain(|(_, key)| *key != id);
            self.conscia.dialogs.retain(|(dialog, _)| *dialog != id);
            }
        }
        self.conscia_changed(from, true);
        col
    }

    pub fn update_window(&mut self, window: &W::Id, serial: Option<Serial>) {
        if let Some(tile) = self.tiles_mut().find(|t| t.window().id() == window) {
            if let Some(serial) = serial { tile.window_mut().on_commit(serial); }
            tile.update_window();
        }
        for (col, data) in zip(&mut self.columns, &mut self.data) { data.update(col); }
        self.conscia_relayout();
    }

    pub fn scroll_amount_to_activate(&self, window: &W::Id) -> f64 {
        self.conscia.id(window).map_or(f64::INFINITY, |id| {
            if self.conscia.model.in_main(id) { 0. } else { 1. }
        })
    }

    pub fn activate_window(&mut self, window: &W::Id) -> bool {
        let Some(id) = self.conscia.id(window) else { return false; };
        // Hover/application activation must not promote a Reel group under a
        // docking gesture and demote the entire Main group in its place.
        if self.conscia.drag.is_some() { return self.conscia.model.in_main(id); }
        if self.conscia.dialogs.iter().any(|(dialog, _)| *dialog == id) { return true; }
        if self.conscia.model.in_main(id) {
            if self.conscia.expanded.is_some() || !self.conscia.dialog_scene.is_empty() { return true; }
            if self.conscia.transition.is_none() {
                self.conscia.model.activate_main(id);
                self.conscia_sync_active();
            }
            return true;
        }
        let index = self.conscia.model.windows().iter().position(|&anchor|
            anchor == id || self.conscia.model.reel_group(anchor).contains(&id));
        let Some(index) = index else { return false; };
        // IPC/application activation can address an offscreen thumbnail.
        let from = self.conscia.current_scene();
        if index < self.conscia.model.reel_front_index().unwrap_or(1) {
            self.conscia.model.set_offset(index.saturating_sub(1) as f64, self.conscia.slots());
        }
        self.conscia.model.promote(index);
        self.conscia.model.activate_main(id);
        self.conscia.expanded = None;
        self.conscia_changed(from, true);
        true
    }

    pub fn start_close_animation_for_window(
        &mut self,
        renderer: &mut GlesRenderer,
        window: &W::Id,
        blocker: TransactionBlocker,
    ) {
        if self.options.animations.off { return; }
        let Some(id) = self.conscia.id(window) else { return; };
        let Some(item) = self.conscia.current_scene().into_iter().find(|p| p.window == id && p.opacity > 0.) else { return; };
        let Some(tile) = self.tiles_mut().find(|t| t.window().id() == window) else { return; };
        let Some(snapshot) = tile.take_unmap_snapshot() else { return; };
        let size = tile.tile_size();
        if size.w <= 0. || size.h <= 0. { return; }
        let factor = (item.width / size.w).min(item.height / size.h);
        let pos = Point::from((item.x + (item.width - size.w * factor) / 2.,
            item.y + (item.height - size.h * factor) / 2.));
        let reel_left = self.conscia.model.side == conscia::core::ReelSide::Left;
        let outward_left = if self.conscia.model.in_main(id) { !reel_left } else { reel_left };
        let distance = if outward_left { -pos.x - size.w * factor - self.working_area.loc.x.abs() }
            else { self.view_size.w - pos.x };
        let anim = Animation::new(self.clock.clone(), 0., 1., 0., self.options.animations.window_close.anim);
        let blocker = if self.options.disable_transactions { TransactionBlocker::completed() } else { blocker };
        match ClosingWindow::new(renderer, snapshot, Scale::from(self.scale), size, pos, blocker, anim) {
            Ok(mut closing) => {
                closing.set_fly_out(factor, distance);
                self.closing_windows.push(closing);
            }
            Err(err) => warn!("error creating a closing window animation: {err:?}"),
        }
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        self.conscia.id(id).is_some()
    }

    pub fn focus_left(&mut self) -> bool {
        self.conscia_next()
    }

    pub fn focus_right(&mut self) -> bool {
        self.conscia_next()
    }

    pub fn focus_column_first(&mut self) {
        self.conscia_next();
    }

    pub fn focus_column_last(&mut self) {
        let index = self.conscia.model.windows().len().saturating_sub(1);
        self.conscia_promote(index);
    }

    pub fn focus_column(&mut self, index: usize) {
        self.conscia_promote(index);
    }

    pub fn focus_window_in_column(&mut self, index: u8) {
        self.conscia_main_index(index as usize);
    }

    pub fn focus_down(&mut self) -> bool {
        self.conscia_main_step(true)
    }

    pub fn focus_up(&mut self) -> bool {
        self.conscia_main_step(false)
    }

    pub fn focus_down_or_left(&mut self) {
        if !self.conscia_main_step(true) { self.conscia_next(); }
    }

    pub fn focus_down_or_right(&mut self) {
        if !self.conscia_main_step(true) { self.conscia_next(); }
    }

    pub fn focus_up_or_left(&mut self) {
        if !self.conscia_main_step(false) { self.conscia_next(); }
    }

    pub fn focus_up_or_right(&mut self) {
        if !self.conscia_main_step(false) { self.conscia_next(); }
    }

    pub fn focus_top(&mut self) {
        self.conscia_main_index(0);
    }

    pub fn focus_bottom(&mut self) {
        self.conscia_main_index(self.conscia.model.main_tiles().len());
    }

    pub fn move_column_to_index(&mut self, _index: usize) {
        self.conscia_mirror();
    }

    pub fn move_left(&mut self) -> bool {
        self.conscia_mirror();
        true
    }

    pub fn move_right(&mut self) -> bool {
        self.conscia_mirror();
        true
    }

    pub fn move_column_to_first(&mut self) {
        self.conscia_mirror();
    }

    pub fn move_column_to_last(&mut self) {
        self.conscia_mirror();
    }

    pub fn move_down(&mut self) -> bool {
        self.conscia_scroll(1., true);
        true
    }

    pub fn move_up(&mut self) -> bool {
        self.conscia_scroll(-1., true);
        true
    }

    pub fn consume_or_expel_window_left(&mut self, window: Option<&W::Id>) {
        if let Some(window) = window {
            if let Some(id) = self.conscia.id(window) {
                if self.conscia.model.in_main(id) {
                    self.conscia.model.activate_main(id);
                    self.conscia_release(false);
                } else if let Some(index) = self.conscia.model.windows().iter().position(|&w| w == id) {
                    self.conscia_add_main(index);
                }
            }
        } else { self.consume_into_column(); }
    }

    pub fn consume_or_expel_window_right(&mut self, window: Option<&W::Id>) {
        if let Some(window) = window {
            if let Some(id) = self.conscia.id(window) {
                if self.conscia.model.in_main(id) {
                    self.conscia.model.activate_main(id);
                    self.conscia_release(false);
                } else if let Some(index) = self.conscia.model.windows().iter().position(|&w| w == id) {
                    self.conscia_add_main(index);
                }
            }
        } else { self.consume_into_column(); }
    }

    pub fn consume_into_column(&mut self) {
        if let Some(index) = self.conscia.model.reel_front_index() { self.conscia_add_main(index); }
    }

    pub fn expel_from_column(&mut self) {
        self.conscia_release(false);
    }

    pub fn swap_window_in_direction(&mut self, _direction: ScrollDirection) {
        self.conscia_next();
    }

    pub fn toggle_column_tabbed_display(&mut self) {
        self.conscia_release(true);
    }

    pub fn set_column_display(&mut self, _display: ColumnDisplay) {
        self.conscia_release(true);
    }

    pub fn center_column(&mut self) {
        self.conscia_mirror();
    }

    pub fn center_window(&mut self, window: Option<&W::Id>) {
        if let Some(id) = window { self.activate_window(id); }
    }

    pub fn center_visible_columns(&mut self) {
        self.conscia_mirror();
    }

    pub fn view_pos(&self) -> f64 {
        0.
    }

    pub fn target_view_pos(&self) -> f64 {
        0.
    }

    pub fn columns(&self) -> impl Iterator<Item = &Column<W>> {
        self.columns.iter()
    }

    pub fn columns_with_render_positions(
        &self,
    ) -> impl Iterator<Item = (&Column<W>, Point<f64, Logical>)> {
        let scene = self.conscia.current_scene();
        self.columns.iter().map(move |col| {
            let pos = col.tiles.first().and_then(|t| self.conscia.id(t.window().id()))
                .and_then(|id| scene.iter().find(|p| p.window == id))
                .map_or(Point::from((0., 0.)), |p| (p.x, p.y).into());
            (col, pos)
        })
    }

    pub fn columns_with_render_positions_mut(
        &mut self,
    ) -> impl Iterator<Item = (&mut Column<W>, Point<f64, Logical>)> {
        let scene = self.conscia.current_scene();
        let state = &self.conscia;
        self.columns.iter_mut().map(move |col| {
            let pos = col.tiles.first().and_then(|t| state.id(t.window().id()))
                .and_then(|id| scene.iter().find(|p| p.window == id))
                .map_or(Point::from((0., 0.)), |p| (p.x, p.y).into());
            (col, pos)
        })
    }

    pub fn tiles_with_render_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>, bool)> {
        let scene = self.conscia.current_scene();
        self.tiles().map(move |tile| {
            let item = self.conscia.id(tile.window().id())
                .and_then(|id| scene.iter().find(|p| p.window == id));
            let (pos, visible) = item.map_or((Point::from((0., 0.)), false), |p|
                (Point::from((p.x, p.y)), p.opacity > 0. && p.role != conscia::geometry::WindowRole::Hidden));
            (tile, pos, visible)
        })
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        _round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> {
        let scene = self.conscia.current_scene();
        let ids = self.conscia.ids.clone();
        self.tiles_mut().map(move |tile| {
            let item = ids.iter().find(|(id, _)| id == tile.window().id())
                .and_then(|(_, key)| scene.iter().find(|p| p.window == *key));
            let pos = item.map_or(Point::from((0., 0.)), |p| Point::from((p.x, p.y)));
            (tile, pos)
        })
    }

    pub fn tiles_with_ipc_layouts(&self) -> impl Iterator<Item = (&Tile<W>, WindowLayout)> {
        self.columns.iter().enumerate().flat_map(move |(col_idx, col)| {
            let scene = self.conscia.visible_scene();
            col.tiles.iter().enumerate().map(move |(tile_idx, tile)| {
                let p = self.conscia.id(tile.window().id()).and_then(|id| scene.iter().find(|p| p.window == id));
                let mut layout = tile.ipc_layout_template();
                layout.pos_in_scrolling_layout = Some((col_idx + 1, tile_idx + 1));
                layout.window_offset_in_tile = (0., 0.);
                if let Some(p) = p {
                    layout.tile_size = (p.width, p.height);
                    layout.tile_pos_in_workspace_view = Some((p.x, p.y));
                    let size = tile.tile_size();
                    if size.w > 0. && size.h > 0. {
                        let factor = (p.width / size.w).min(p.height / size.h);
                        let offset = tile.window_loc().upscale(factor);
                        layout.window_offset_in_tile = ((p.width - size.w * factor) / 2. + offset.x,
                            (p.height - size.h * factor) / 2. + offset.y);
                    }
                }
                (tile, layout)
            })
        })
    }

    pub(super) fn insert_hint_area(
        &self,
        position: InsertPosition,
    ) -> Option<Rectangle<f64, Logical>> {
        self.conscia.layout.as_ref().map(|l| {
            let r = match position { InsertPosition::InColumn(..) => l.main, _ => l.reel };
            Rectangle::new((r.x as f64, r.y as f64).into(), (r.width as f64, r.height as f64).into())
        })
    }

    /// Returns the geometry of the active window relative to and clamped to the view.
    ///
    /// During animations, assumes the final view position.
    pub fn active_window_visual_rectangle(&self) -> Option<Rectangle<f64, Logical>> {
        let id = self.conscia.active()?;
        let p = self.conscia.visible_scene().into_iter().find(|p| p.window == id)?;
        Some(Rectangle::new((p.x, p.y).into(), (p.width, p.height).into()))
    }

    pub fn popup_target_rect(&self, id: &W::Id) -> Option<Rectangle<f64, Logical>> {
        let key = self.conscia.id(id)?;
        let p = self.conscia.visible_scene().into_iter().find(|p| p.window == key)?;
        Some(Rectangle::new(self.parent_area.loc - Point::from((p.x, p.y)), self.parent_area.size))
    }

    pub fn toggle_width(&mut self, _forwards: bool) {
        self.conscia_mirror();
    }

    pub fn toggle_full_width(&mut self) {
        if !self.conscia_can_edit() || self.conscia.model.main().is_none() { return; }
        if !self.conscia.main_maximized && self.conscia.model.windows().len() == 1
            && self.conscia.model.main_tiles().is_empty() { return; }
        let from = self.conscia.current_scene();
        self.conscia.main_maximized = !self.conscia.main_maximized;
        self.conscia_changed(from, true);
    }

    pub fn set_window_width(&mut self, window: Option<&W::Id>, change: SizeChange) {
        self.conscia_resize_dimension(window, change, true);
    }

    pub fn set_window_height(&mut self, window: Option<&W::Id>, change: SizeChange) {
        self.conscia_resize_dimension(window, change, false);
    }

    pub fn reset_window_height(&mut self, _window: Option<&W::Id>) {
        if !self.conscia_can_edit() { return; }
        let Some(anchor) = self.conscia.model.main() else { return; };
        let from = self.conscia.current_scene();
        if let Some(tree) = self.conscia.trees.iter_mut().find(|tree| tree.contains(anchor)) { tree.reset_ratios(); }
        self.conscia_changed(from, true);
    }

    pub fn toggle_window_width(&mut self, _window: Option<&W::Id>, _forwards: bool) {
        self.conscia_mirror();
    }

    pub fn toggle_window_height(&mut self, _window: Option<&W::Id>, _forwards: bool) {
        self.conscia_mirror();
    }

    pub fn expand_column_to_available_width(&mut self) {
        self.toggle_full_width();
    }

    pub fn set_fullscreen(&mut self, window: &W::Id, is_fullscreen: bool) -> bool {
        self.conscia_expand(window, is_fullscreen, true)
    }

    pub fn set_maximized(&mut self, window: &W::Id, maximize: bool) -> bool {
        self.conscia_expand(window, maximize, false)
    }

    pub fn render_above_top_layer(&self) -> bool {
        self.conscia.expanded.is_some_and(|(_, full)| full)
    }

    pub fn render<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        layer: RenderLayer,
        push: &mut dyn FnMut(ScrollingSpaceRenderElement<R>),
    ) {
        use smithay::backend::renderer::element::utils::{CropRenderElement, Relocate,
            RelocateRenderElement, RescaleRenderElement};
        if !layer.is_normal() { return; }
        let scale = Scale::from(self.scale);
        for closing in self.closing_windows.iter().rev() {
            push(closing.render(ctx.as_gles(), Rectangle::new(Point::from((0., 0.)), self.view_size), scale).into());
        }
        let mut scene = self.conscia.current_scene();
        let reel_clip = self.conscia.layout.as_ref().map(|layout| Rectangle::new(
            Point::from((layout.reel.x as f64 + layout.origin_offset.0, layout.reel.y as f64 + layout.origin_offset.1)),
            Size::from((layout.reel.width as f64, layout.reel.height as f64)),
        ).to_physical_precise_round(scale));
        let active = self.conscia.active();
        scene.sort_by_key(|p| (self.conscia.dialogs.iter().all(|(id, _)| *id != p.window),
            Some(p.window) != active, p.role != conscia::geometry::WindowRole::Main));
        // Render one foreground row per reel group. Membership order records
        // join order independently of the split tree's spatial arrangement.
        for &anchor in self.conscia.model.windows() {
            if self.conscia.model.in_main(anchor) { continue; }
            let members = std::iter::once(anchor)
                .chain(self.conscia.model.reel_group(anchor).iter().copied());
            let mut icons = Vec::new();
            let mut bounds: Option<Rectangle<f64, Logical>> = None;
            for id in members {
                let Some(p) = scene.iter().find(|p| p.window == id
                    && matches!(p.role, conscia::geometry::WindowRole::Reel { .. })
                    && p.opacity > 0. && p.width > 0. && p.height > 0.) else { continue; };
                let Some(tile) = self.tiles().find(|t| self.conscia.id(t.window().id()) == Some(id)) else { continue; };
                let size = tile.tile_size();
                if size.w <= 0. || size.h <= 0. { continue; }
                let factor = (p.width / size.w).min(p.height / size.h);
                let width = size.w * factor;
                let height = size.h * factor;
                let rect = Rectangle::new(
                    Point::from((p.x + (p.width - width) / 2., p.y + (p.height - height) / 2.)),
                    Size::from((width, height)),
                );
                bounds = Some(match bounds {
                    None => rect,
                    Some(previous) => {
                        let left = previous.loc.x.min(rect.loc.x);
                        let top = previous.loc.y.min(rect.loc.y);
                        let right = (previous.loc.x + previous.size.w).max(rect.loc.x + rect.size.w);
                        let bottom = (previous.loc.y + previous.size.h).max(rect.loc.y + rect.size.h);
                        Rectangle::new((left, top).into(), (right - left, bottom - top).into())
                    }
                });
                icons.push((tile.window().app_id(), p.opacity));
            }
            let Some(bounds) = bounds else { continue; };
            let icon_size = crate::render_helpers::app_icon::ICON_SIZE;
            let extent = f64::from(icon_size);
            let margin = 8.;
            if bounds.size.w < margin + extent || bounds.size.h < margin + extent { continue; }
            let row_width = icons.len() as f64 * (extent + margin) - margin;
            let x = if self.conscia.model.side == conscia::core::ReelSide::Left {
                bounds.loc.x + bounds.size.w - margin - row_width
            } else { bounds.loc.x + margin };
            let y = bounds.loc.y + bounds.size.h - margin - extent;
            let Some(crop) = bounds.to_physical_precise_round(scale)
                .intersection(self.working_area.to_physical_precise_round(scale)) else { continue; };
            let crop = if self.conscia.transition.is_none() {
                let Some(crop) = reel_clip.and_then(|reel| crop.intersection(reel)) else { continue; };
                crop
            } else { crop };
            for (index, (app_id, opacity)) in icons.into_iter().enumerate() {
                let location = Point::<f64, Logical>::from((x + index as f64 * (extent + margin), y));
                let buffer = self.reel_icons.get(app_id.as_deref(), self.scale);
                if let Ok(element) = smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement::from_buffer(
                    ctx.renderer, location.to_physical_precise_round::<_, i32>(scale).to_f64(),
                    &buffer, Some(opacity), None, Some(Size::from((icon_size, icon_size))),
                    smithay::backend::renderer::element::Kind::Unspecified,
                ) {
                    if let Some(element) = CropRenderElement::from_element(element, scale, crop) {
                        push(ScrollingSpaceRenderElement::ReelIcon(element));
                    }
                }
            }
        }
        for p in scene {
            if p.opacity <= 0. || p.width <= 0. || p.height <= 0. { continue; }
            let Some(tile) = self.tiles().find(|t| self.conscia.id(t.window().id()) == Some(p.window)) else { continue; };
            let window = tile.window();
            let size = tile.tile_size();
            if size.w <= 0. || size.h <= 0. { continue; }
            let factor = (p.width / size.w).min(p.height / size.h);
            let pos = Point::from((p.x + (p.width - size.w * factor) / 2.,
                p.y + (p.height - size.h * factor) / 2.));
            let ring = if focus_ring && Some(p.window) == active {
                tile.focus_ring().width() * factor
            } else { 0. };
            let crop = Rectangle::new(Point::from((p.x - ring, p.y - ring)),
                Size::from((p.width + 2. * ring, p.height + 2. * ring)))
                .to_physical_precise_round(scale);
            let crop = if matches!(p.role, conscia::geometry::WindowRole::Reel { .. }) && self.conscia.transition.is_none() {
                let Some(crop) = reel_clip.and_then(|reel| crop.intersection(reel)) else { continue; };
                crop
            } else { crop };
            // Reel never exposes client popups or input surfaces.
            if p.role == conscia::geometry::WindowRole::Main && self.conscia.transition.is_none() && self.conscia.drag.is_none() {
                window.render_popups(ctx.r(), tile.window_loc(), scale, p.opacity,
                    xray_pos.offset(pos), &mut |elem| {
                        let elem = RescaleRenderElement::from_element(TileRenderElement::from(elem), Point::from((0, 0)), factor);
                        let elem = RelocateRenderElement::from_element(elem,
                            pos.to_physical_precise_round(scale), Relocate::Relative);
                        let bounds = self.working_area.to_physical_precise_round(scale);
                        if let Some(elem) = CropRenderElement::from_element(elem, scale, bounds) {
                            push(ScrollingSpaceRenderElement::Conscia(elem));
                        }
                    });
            }
            let push_window = &mut |elem| {
                let elem = RescaleRenderElement::from_element(elem, Point::from((0, 0)), factor);
                let elem = RelocateRenderElement::from_element(elem,
                    pos.to_physical_precise_round(scale), Relocate::Relative);
                if let Some(elem) = CropRenderElement::from_element(elem, scale, crop) {
                    push(ScrollingSpaceRenderElement::Conscia(elem));
                }
            };
            tile.render_decorated(ctx.r(), Point::from((0., 0.)), xray_pos.offset(pos),
                focus_ring && Some(p.window) == active, p.opacity, false, factor, push_window);
            // Front-to-back: the landing hint sits below the dragged window,
            // but above stationary windows without changing their geometry.
            if self.conscia.drag_window() == Some(p.window) {
                if let Some((rect, _)) = self.conscia.snap_hint() {
                    self.snap_hint_element.render(ctx.renderer, rect.loc, &mut |elem| {
                        push(ScrollingSpaceRenderElement::SnapHint(elem));
                    });
                }
            }
        }
    }

    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<(&W, HitType)> {
        if self.conscia.transition.is_some() || self.conscia.resize.is_some() || self.conscia.drag.is_some() { return None; }
        let mut scene = self.conscia.visible_scene();
        let active = self.conscia.active();
        scene.sort_by_key(|p| Some(p.window) != active);
        for p in scene {
            if p.opacity <= 0. { continue; }
            if !self.conscia.dialog_scene.is_empty() && Some(p.window) != active { continue; }
            let Some(tile) = self.tiles().find(|t| self.conscia.id(t.window().id()) == Some(p.window)) else { continue; };
            let win = tile.window();
            let size = tile.tile_size();
            if size.w <= 0. || size.h <= 0. { continue; }
            let factor = (p.width / size.w).min(p.height / size.h);
            if factor <= 0. { continue; }
            let origin = Point::from((p.x + (p.width - size.w * factor) / 2.,
                p.y + (p.height - size.h * factor) / 2.));
            let local = (pos - origin).downscale(factor);
            let within = pos.x >= p.x && pos.y >= p.y && pos.x < p.x + p.width && pos.y < p.y + p.height;
            if !within { continue; }
            if p.role == conscia::geometry::WindowRole::Main {
                let local = local - tile.window_loc();
                if win.is_in_input_region(local) {
                    return Some((win, HitType::Input { win_pos: pos - local + win.buf_loc().to_f64() }));
                }
                if within { return Some((win, HitType::Activate { is_tab_indicator: false })); }
            } else if within {
                // Activation-only targets must never trigger focus-follows-mouse.
                return Some((win, HitType::Activate { is_tab_indicator: true }));
            }
        }
        // A thumbnail group's gaps belong to the group, not to the wallpaper
        // underneath. Return an activation-only hit so layer-shell backgrounds
        // cannot swallow these clicks before Conscia handles the button.
        if self.conscia_reel_at(pos) {
            if let Some(layout) = &self.conscia.layout {
                if let Some(slot) = conscia::scene::scene(layout, self.conscia.model.offset()).into_iter().find(|p|
                    matches!(p.role, conscia::geometry::WindowRole::Reel { .. }) && p.opacity > 0.
                        && pos.x >= p.x && pos.x < p.x + p.width
                        && pos.y >= p.y && pos.y < p.y + p.height) {
                    if let Some(tile) = self.tiles().find(|tile| self.conscia.id(tile.window().id()) == Some(slot.window)) {
                        return Some((tile.window(), HitType::Activate { is_tab_indicator: true }));
                    }
                }
            }
        }
        None
    }

    pub fn view_offset_gesture_begin(&mut self, is_touchpad: bool) {
        self.conscia.gesture = Some(is_touchpad);
        self.conscia.end_scroll();
    }

    pub fn dnd_scroll_gesture_begin(&mut self) {
        // Screen-edge drag scrolling is removed.
    }

    pub fn view_offset_gesture_update(
        &mut self,
        delta_x: f64,
        _timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<bool> {
        if self.conscia.gesture != Some(is_touchpad) { return None; }
        self.conscia_scroll(delta_x, false);
        Some(true)
    }

    pub fn dnd_scroll_gesture_scroll(&mut self, _delta: f64) -> bool {
        false
    }

    pub fn view_offset_gesture_end(&mut self, is_touchpad: Option<bool>) -> bool {
        if is_touchpad.is_some() && is_touchpad != self.conscia.gesture { return false; }
        let changed = self.conscia.gesture.take().is_some();
        self.conscia.end_scroll();
        changed
    }

    pub fn dnd_scroll_gesture_end(&mut self) {
        self.conscia.end_scroll();
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        let Some(id) = self.conscia.id(&window) else { return false; };
        let Some(p) = self.conscia.visible_scene().into_iter().find(|p| p.window == id) else { return false; };
        let pos = Point::from((
            if edges.contains(ResizeEdge::LEFT) { p.x } else if edges.contains(ResizeEdge::RIGHT) { p.x + p.width } else { p.x + p.width / 2. },
            if edges.contains(ResizeEdge::TOP) { p.y } else if edges.contains(ResizeEdge::BOTTOM) { p.y + p.height } else { p.y + p.height / 2. }));
        if !self.conscia_resize_start(pos) { return false; }
        self.interactive_resize = Some(InteractiveResize { window,
            original_window_size: (p.width, p.height).into(), data: InteractiveResizeData { edges } });
        true
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        let Some(resize) = &self.interactive_resize else { return false; };
        if &resize.window != window { return false; }
        let Some((_, start, _)) = self.conscia.resize else { return false; };
        self.conscia_resize_motion(start + delta)
    }

    pub fn interactive_resize_end(&mut self, window: Option<&W::Id>) {
        if window.is_some_and(|id| self.interactive_resize.as_ref().is_some_and(|r| &r.window != id)) { return; }
        self.interactive_resize = None;
        self.conscia_resize_end();
    }

    pub fn refresh(&mut self, is_active: bool, is_focused: bool) {
        if !is_focused { self.conscia.drag = None; }
        self.conscia_relayout();
        let active = if self.conscia.transition.is_none() { self.conscia.active() } else { None };
        let state = &self.conscia;
        for col in &mut self.columns {
            for tile in &mut col.tiles {
                let win = tile.window_mut();
                let id = state.id(win.id());
                let focused = is_active && is_focused && id == active;
                win.set_active_in_column(id == active);
                win.set_floating(id.is_some_and(|id| state.dialogs.iter().any(|(dialog, _)| *dialog == id)));
                win.set_activated(focused);
                let resizing = state.resize.map(|(split, _, _)| InteractiveResizeData {
                    edges: if split.vertical { ResizeEdge::LEFT_RIGHT } else { ResizeEdge::TOP_BOTTOM },
                });
                win.set_interactive_resize(resizing.filter(|_| id.is_some_and(|id| state.model.in_main(id))));
                let bounds = win.expected_size().unwrap_or_else(|| win.size());
                win.set_bounds(bounds);
                if matches!(win.configure_intent(), ConfigureIntent::CanSend | ConfigureIntent::ShouldSend) {
                    win.send_pending_configure();
                }
                win.refresh();
            }
        }
    }

    #[cfg(test)]
    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    #[cfg(test)]
    pub fn parent_area(&self) -> Rectangle<f64, Logical> {
        self.parent_area
    }

    #[cfg(test)]
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    #[cfg(test)]
    pub fn options(&self) -> &Rc<Options> {
        &self.options
    }

    #[cfg(test)]
    pub fn active_column_idx(&self) -> usize {
        self.active_column_idx
    }

    #[cfg(test)]
    pub fn verify_invariants(&self) {
        assert!(self.view_size.w > 0.);
        assert!(self.view_size.h > 0.);
        assert!(self.scale > 0.);
        assert!(self.scale.is_finite());
        assert_eq!(self.columns.len(), self.data.len());
        assert_eq!(
            self.working_area,
            compute_working_area(self.parent_area, self.scale, self.options.layout.struts)
        );

        if !self.columns.is_empty() {
            assert!(self.active_column_idx < self.columns.len());

            for (column, data) in zip(&self.columns, &self.data) {
                assert!(Rc::ptr_eq(&self.options, &column.options));
                assert_eq!(self.clock, column.clock);
                assert_eq!(self.scale, column.scale);
                column.verify_invariants();

                let mut data2 = *data;
                data2.update(column);
                assert_eq!(data, &data2, "column data must be up to date");
            }

        }

        if let Some(resize) = &self.interactive_resize {
            assert!(
                self.columns
                    .iter()
                    .flat_map(|col| &col.tiles)
                    .any(|tile| tile.window().id() == &resize.window),
                "interactive resize window must be present in the layout"
            );
        }
    }
}

impl ColumnData {
    pub fn new<W: LayoutElement>(column: &Column<W>) -> Self {
        let mut rv = Self { width: 0. };
        rv.update(column);
        rv
    }

    pub fn update<W: LayoutElement>(&mut self, column: &Column<W>) {
        self.width = column.width();
    }
}

impl TileData {
    pub fn new<W: LayoutElement>(tile: &Tile<W>, height: WindowHeight) -> Self {
        let mut rv = Self {
            height,
            size: Size::default(),
            interactively_resizing_by_left_edge: false,
        };
        rv.update(tile);
        rv
    }

    pub fn update<W: LayoutElement>(&mut self, tile: &Tile<W>) {
        self.size = tile.tile_size();
        self.interactively_resizing_by_left_edge = tile
            .window()
            .interactive_resize_data()
            .is_some_and(|data| data.edges.contains(ResizeEdge::LEFT));
    }
}

impl From<PresetSize> for ColumnWidth {
    fn from(value: PresetSize) -> Self {
        match value {
            PresetSize::Proportion(p) => Self::Proportion(p.clamp(0., 10000.)),
            PresetSize::Fixed(f) => Self::Fixed(f64::from(f.clamp(1, 100000))),
        }
    }
}

impl WindowHeight {
    const fn auto_1() -> Self {
        Self::Auto { weight: 1. }
    }
}

impl<W: LayoutElement> Column<W> {
    #[allow(clippy::too_many_arguments)]
    fn new_with_tile(
        tile: Tile<W>,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        width: ColumnWidth,
        is_full_width: bool,
    ) -> Self {
        let options = tile.options.clone();

        let display_mode = tile
            .window()
            .rules()
            .default_column_display
            .unwrap_or(options.layout.default_column_display);

        let mut rv = Self {
            conscia_tree: None,
            tiles: vec![],
            data: vec![],
            active_tile_idx: 0,
            width,
            is_full_width,
            is_pending_maximized: false,
            is_pending_fullscreen: false,
            display_mode,
            tab_indicator: TabIndicator::new(options.layout.tab_indicator),
            move_x_animation: None,
            move_y_animation: None,
            view_size,
            working_area,
            parent_area,
            scale,
            clock: tile.clock.clone(),
            options,
            id: ColumnId::next(),
        };

        let pending_sizing_mode = tile.window().pending_sizing_mode();

        rv.add_tile_at(0, tile);

        match pending_sizing_mode {
            SizingMode::Normal => (),
            SizingMode::Maximized => rv.set_maximized(true),
            SizingMode::Fullscreen => rv.set_fullscreen(true),
        }

        // Animate the tab indicator for new columns.
        if display_mode == ColumnDisplay::Tabbed
            && !rv.options.layout.tab_indicator.hide_when_single_tab
            && rv.sizing_mode().is_normal()
        {
            // Usually new columns are created together with window movement actions. For new
            // windows, we handle that in start_open_animation().
            rv.tab_indicator
                .start_open_animation(rv.clock.clone(), rv.options.animations.window_movement.0);
        }

        rv
    }

    pub fn id(&self) -> ColumnId {
        self.id
    }

    fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        let mut update_sizes = false;

        if self.view_size != view_size
            || self.working_area != working_area
            || self.parent_area != parent_area
        {
            update_sizes = true;
        }

        // If preset heights changed, make our heights non-preset.
        if self.options.layout.preset_window_heights != options.layout.preset_window_heights {
            self.convert_heights_to_auto();
            update_sizes = true;
        }

        if self.options.layout.gaps != options.layout.gaps {
            update_sizes = true;
        }

        if self.options.layout.border.off != options.layout.border.off
            || self.options.layout.border.width != options.layout.border.width
        {
            update_sizes = true;
        }

        if self.options.layout.tab_indicator != options.layout.tab_indicator {
            update_sizes = true;
        }

        for (tile, data) in zip(&mut self.tiles, &mut self.data) {
            tile.update_config(view_size, scale, options.clone());
            data.update(tile);
        }

        self.tab_indicator
            .update_config(options.layout.tab_indicator);
        self.view_size = view_size;
        self.working_area = working_area;
        self.parent_area = parent_area;
        self.scale = scale;
        self.options = options;

        if update_sizes {
            self.update_tile_sizes(false);
        }
    }

    pub fn update_shaders(&mut self) {
        for tile in &mut self.tiles {
            tile.update_shaders();
        }

        self.tab_indicator.update_shaders();
    }

    pub fn advance_animations(&mut self) {
        if let Some(move_) = &mut self.move_x_animation {
            if move_.anim.is_done() {
                self.move_x_animation = None;
            }
        }
        if let Some(move_) = &mut self.move_y_animation {
            if move_.anim.is_done() {
                self.move_y_animation = None;
            }
        }

        for tile in &mut self.tiles {
            tile.advance_animations();
        }

        self.tab_indicator.advance_animations();
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.move_x_animation.is_some()
            || self.move_y_animation.is_some()
            || self.tab_indicator.are_animations_ongoing()
            || self.tiles.iter().any(Tile::are_animations_ongoing)
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.move_x_animation.is_some()
            || self.move_y_animation.is_some()
            || self.tab_indicator.are_animations_ongoing()
            || self.tiles.iter().any(Tile::are_transitions_ongoing)
    }

    pub fn is_moving_between_workspaces(&self) -> bool {
        self.move_y_animation
            .as_ref()
            .is_some_and(|anim| anim.is_between_workspaces)
            || self
                .move_x_animation
                .as_ref()
                .is_some_and(|anim| anim.is_between_workspaces)
            // When moving a window between workspaces into the scrolling layout, it will be
            // immediately put into a new column, however the Y move animation and the
            // moving-between-workspaces flag will be set on the tile rather than on that new
            // column. Since the scrolling layout rendering checks moving between workspaces
            // per-column rather than per-tile, we need to include the tiles here.
            //
            // This is not always visually correct: for example, interactive-moving a tile into an
            // otherwise stationary column will now cause that entire column to draw unculled. I'm
            // not sure there's a good solution overall for all edge cases here. Consider that the
            // column tab indicator needs to draw on top of the tiles, but in this counterexample
            // above, the column is stationary, so the single moving tile would instead need to draw
            // on top of the tab indicator.
            //
            // I'm going with this simple check for now that should look ok in most cases.
            || self
                .tiles
                .iter()
                .any(|tile| tile.is_moving_between_workspaces())
    }

    pub fn update_render_elements(&mut self, is_active: bool, view_rect: Rectangle<f64, Logical>) {
        let active_idx = self.active_tile_idx;
        for (tile_idx, (tile, tile_off)) in self.tiles_mut().enumerate() {
            let is_active = is_active && tile_idx == active_idx;

            let mut tile_view_rect = view_rect;
            tile_view_rect.loc -= tile_off + tile.render_offset();
            tile.update_render_elements(is_active, tile_view_rect);
        }

        let config = self.tab_indicator.config();
        let offsets = self.tile_offsets_iter(self.data.iter().copied());
        let tabs = zip(&self.tiles, offsets)
            .enumerate()
            .map(|(tile_idx, (tile, tile_off))| {
                let is_active = tile_idx == active_idx;
                let is_urgent = tile.window().is_urgent();
                let tile_pos = tile_off + tile.render_offset();
                TabInfo::from_tile(tile, tile_pos, is_active, is_urgent, &config)
            });

        // Hide the tab indicator in fullscreen. If you have it configured to overlap the window,
        // you don't want that to happen in fullscreen. Also, laying things out correctly when the
        // tab indicator is within the column and the column goes fullscreen, would require too
        // many changes to the code for too little benefit (it's mostly invisible anyway).
        let enabled = self.display_mode == ColumnDisplay::Tabbed && self.sizing_mode().is_normal();

        self.tab_indicator.update_render_elements(
            enabled,
            self.tab_indicator_area(),
            view_rect,
            self.tiles.len(),
            tabs,
            is_active,
            self.scale,
        );
    }

    pub fn is_pending_fullscreen(&self) -> bool {
        self.is_pending_fullscreen
    }

    pub fn is_pending_maximized(&self) -> bool {
        self.is_pending_maximized
    }

    pub fn pending_sizing_mode(&self) -> SizingMode {
        if self.is_pending_fullscreen {
            SizingMode::Fullscreen
        } else if self.is_pending_maximized {
            SizingMode::Maximized
        } else {
            SizingMode::Normal
        }
    }

    pub fn render_offset(&self) -> Point<f64, Logical> {
        let mut offset = Point::from((0., 0.));

        if let Some(move_) = &self.move_x_animation {
            offset.x += move_.from * move_.anim.value();
        }
        if let Some(move_) = &self.move_y_animation {
            offset.y += move_.from * move_.anim.value();
        }

        offset
    }

    pub fn animate_move_from(&mut self, from: Point<f64, Logical>) {
        self.animate_move_from_with_config(from, self.options.animations.window_movement.0);
    }

    pub fn animate_move_from_with_config(
        &mut self,
        from: Point<f64, Logical>,
        config: niri_config::Animation,
    ) {
        self.animate_move_x_from_with_config(from.x, config);
        self.animate_move_y_from_with_config(from.y, config);
    }

    pub fn animate_move_x_from(&mut self, from_x_offset: f64) {
        self.animate_move_x_from_with_config(
            from_x_offset,
            self.options.animations.window_movement.0,
        );
    }

    pub fn animate_move_x_from_with_config(
        &mut self,
        from_x_offset: f64,
        config: niri_config::Animation,
    ) {
        let (current_offset, current_between) =
            self.move_x_animation.as_ref().map_or((0., false), |move_| {
                (move_.from * move_.anim.value(), move_.is_between_workspaces)
            });

        let anim = Animation::new(self.clock.clone(), 1., 0., 0., config);
        self.move_x_animation = Some(MoveAnimation {
            anim,
            from: from_x_offset + current_offset,
            is_between_workspaces: current_between,
        });
    }

    pub fn animate_move_y_from(&mut self, from_y_offset: f64) {
        self.animate_move_y_from_with_config(
            from_y_offset,
            self.options.animations.window_movement.0,
        );
    }

    pub fn animate_move_y_from_with_config(
        &mut self,
        from_y_offset: f64,
        config: niri_config::Animation,
    ) {
        let (current_offset, current_between) =
            self.move_y_animation.as_ref().map_or((0., false), |move_| {
                (move_.from * move_.anim.value(), move_.is_between_workspaces)
            });

        let anim = Animation::new(self.clock.clone(), 1., 0., 0., config);
        self.move_y_animation = Some(MoveAnimation {
            anim,
            from: from_y_offset + current_offset,
            is_between_workspaces: current_between,
        });
    }

    pub fn offset_move_anim_current(&mut self, offset: f64) {
        if let Some(move_) = self.move_x_animation.as_mut() {
            // If the anim is almost done, there's little point trying to offset it; we can let
            // things jump. If it turns out like a bad idea, we could restart the anim instead.
            let value = move_.anim.value();
            if value > 0.001 {
                move_.from += offset / value;
            }
        }
    }

    pub fn set_anim_y_between_workspaces(&mut self) {
        if let Some(anim) = &mut self.move_y_animation {
            anim.is_between_workspaces = true;
        }
    }

    /// Returns whether this column is currently fullscreen.
    ///
    /// As in, if it contains one currently-fullscreen tile, or in tabbed mode, if it contains at
    /// least one currently-fullscreen tile.
    ///
    /// This will lag behind is_pending_fullscreen, depending on when the tiles actually respond to
    /// the un/fullscreen request. But, it's possible for is_fullscreen() to flip instantly, for
    /// example when consuming a fullscreen tile into a non-pending-fullscreen column.
    ///
    /// This controls things like:
    ///
    /// - whether the column draws at the top of the screen or at the start of the working area
    /// - whether the column draws above the top layer-shell layer
    /// - whether the tab indicator is shown
    /// - restoring view_offset_before_fullscreen
    ///
    /// Edge cases to watch out for:
    ///
    /// - Consuming a fullscreen tile into a non-tabbed column will keep that tile fullscreen until
    ///   it responds to the unfullscreen request. This tile may be anywhere in the column,
    ///   including at the active position.
    ///
    /// - Changing a fullscreen tabbed column into normal mode is an easy way to get randomly
    ///   delayed unfullscreening tiles in a normal column.
    ///
    /// - is_fullscreen() can suddenly change when consuming/expelling a fullscreen tile into/from a
    ///   non-fullscreen column. This can influence the code that saves/restores the unfullscreen
    ///   view offset.
    fn sizing_mode(&self) -> SizingMode {
        // Behaviors that we want:
        //
        // 1. The common case: single tile in a column. Assume no animations. Fullscreening the tile
        //    should make it jump to the top-left of the screen only when the tile finishes
        //    fullscreening. Similarly, unfullscreening should keep it at the top-left until the
        //    tile had unfullscreened.
        //
        // 2. Unfullscreening a tabbed column with multiple tiles should restore the view offset
        //    correctly. This means waiting for *all* tiles to unfullscreen, because otherwise the
        //    restored view offset will immediately get overwritten by the still screen-wide column
        //    (it uses the largest tile's width).
        //
        // 3. Changing a fullscreen tabbed column to normal should probably also restore the view
        //    offset correctly. Same problem as above, but now for normal columns (since display
        //    mode change applies instantly).
        //
        // The logic that satisfies these behaviors is to check if *any* tile is fullscreen.
        let mut any_fullscreen = false;
        let mut any_maximized = false;
        for tile in &self.tiles {
            match tile.sizing_mode() {
                SizingMode::Normal => (),
                SizingMode::Maximized => any_maximized = true,
                SizingMode::Fullscreen => any_fullscreen = true,
            }
        }

        if any_fullscreen {
            SizingMode::Fullscreen
        } else if any_maximized {
            SizingMode::Maximized
        } else {
            SizingMode::Normal
        }
    }

    pub fn contains(&self, window: &W::Id) -> bool {
        self.tiles
            .iter()
            .map(Tile::window)
            .any(|win| win.id() == window)
    }

    pub fn position(&self, window: &W::Id) -> Option<usize> {
        self.tiles
            .iter()
            .map(Tile::window)
            .position(|win| win.id() == window)
    }

    fn add_tile_at(&mut self, idx: usize, mut tile: Tile<W>) {
        tile.update_config(self.view_size, self.scale, self.options.clone());

        // Inserting a tile pushes down all tiles below it, but also in always-centering mode it
        // will affect the X position of all tiles in the column.
        let mut prev_offsets = Vec::with_capacity(self.tiles.len() + 1);
        prev_offsets.extend(self.tile_offsets().take(self.tiles.len()));

        if self.display_mode != ColumnDisplay::Tabbed {
            self.is_pending_fullscreen = false;
            self.is_pending_maximized = false;
        }

        self.data
            .insert(idx, TileData::new(&tile, WindowHeight::auto_1()));
        self.tiles.insert(idx, tile);
        self.update_tile_sizes(true);

        // Animate tiles according to the offset changes.
        prev_offsets.insert(idx, Point::default());
        for (i, ((tile, offset), prev)) in zip(self.tiles_mut(), prev_offsets).enumerate() {
            if i == idx {
                continue;
            }

            tile.animate_move_from(prev - offset);
        }
    }

    /// Extra size taken up by elements in the column such as the tab indicator.
    fn extra_size(&self) -> Size<f64, Logical> {
        if self.display_mode == ColumnDisplay::Tabbed {
            self.tab_indicator.extra_size(self.tiles.len(), self.scale)
        } else {
            Size::from((0., 0.))
        }
    }

    fn resolve_preset_height(&self, preset: PresetSize) -> ResolvedSize {
        let extra = self.extra_size();
        resolve_preset_size(preset, &self.options, self.working_area.size.h, extra.h)
    }

    fn resolve_column_width(&self, width: ColumnWidth) -> f64 {
        let working_size = self.working_area.size;
        let gaps = self.options.layout.gaps;
        let extra = self.extra_size();

        match width {
            ColumnWidth::Proportion(proportion) => {
                (working_size.w - gaps) * proportion - gaps - extra.w
            }
            ColumnWidth::Fixed(width) => width,
        }
    }

    fn update_tile_sizes(&mut self, animate: bool) {
        self.update_tile_sizes_with_transaction(animate, Transaction::new());
    }

    fn update_tile_sizes_with_transaction(&mut self, animate: bool, transaction: Transaction) {
        let sizing_mode = self.pending_sizing_mode();
        if matches!(sizing_mode, SizingMode::Fullscreen | SizingMode::Maximized) {
            for (tile_idx, tile) in self.tiles.iter_mut().enumerate() {
                // In tabbed mode, only the visible window participates in the transaction.
                let is_active = tile_idx == self.active_tile_idx;
                let transaction = if self.display_mode == ColumnDisplay::Tabbed && !is_active {
                    None
                } else {
                    Some(transaction.clone())
                };

                if matches!(sizing_mode, SizingMode::Fullscreen) {
                    tile.request_fullscreen(animate, transaction);
                } else {
                    tile.request_maximized(self.parent_area.size, animate, transaction);
                }
            }
            return;
        }

        let is_tabbed = self.display_mode == ColumnDisplay::Tabbed;

        let min_size: Vec<_> = self
            .tiles
            .iter()
            .map(Tile::min_size_nonfullscreen)
            .map(|mut size| {
                size.w = size.w.max(1.);
                size.h = size.h.max(1.);
                size
            })
            .collect();
        let max_size: Vec<_> = self
            .tiles
            .iter()
            .map(Tile::max_size_nonfullscreen)
            .collect();

        // Compute the column width.
        let min_width = min_size
            .iter()
            .map(|size| NotNan::new(size.w).unwrap())
            .max()
            .map(NotNan::into_inner)
            .unwrap();
        let max_width = max_size
            .iter()
            .filter_map(|size| {
                let w = size.w;
                if w == 0. {
                    None
                } else {
                    Some(NotNan::new(w).unwrap())
                }
            })
            .min()
            .map(NotNan::into_inner)
            .unwrap_or(f64::from(i32::MAX));
        let max_width = f64::max(max_width, min_width);

        let width = if self.is_full_width {
            ColumnWidth::Proportion(1.)
        } else {
            self.width
        };

        let working_size = self.working_area.size;
        let extra_size = self.extra_size();

        let width = self.resolve_column_width(width);
        let width = f64::max(f64::min(width, max_width), min_width);
        let max_tile_height = working_size.h - self.options.layout.gaps * 2. - extra_size.h;

        // If there are multiple windows in a column, clamp the non-auto window's height according
        // to other windows' min sizes.
        let mut max_non_auto_window_height = None;
        if self.tiles.len() > 1 && !is_tabbed {
            if let Some(non_auto_idx) = self
                .data
                .iter()
                .position(|data| !matches!(data.height, WindowHeight::Auto { .. }))
            {
                let min_height_taken = min_size
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| *idx != non_auto_idx)
                    .map(|(_, min_size)| min_size.h + self.options.layout.gaps)
                    .sum::<f64>();

                let tile = &self.tiles[non_auto_idx];
                let height_left = max_tile_height - min_height_taken;
                max_non_auto_window_height = Some(f64::max(
                    1.,
                    tile.window_height_for_tile_height(height_left).round(),
                ));
            }
        }

        // Compute the tile heights. Start by converting window heights to tile heights.
        let mut heights = zip(&self.tiles, &self.data)
            .map(|(tile, data)| match data.height {
                auto @ WindowHeight::Auto { .. } => auto,
                WindowHeight::Fixed(height) => {
                    let mut window_height = height.round().max(1.);
                    if let Some(max) = max_non_auto_window_height {
                        window_height = f64::min(window_height, max);
                    } else {
                        // In any case, clamp to the working area height.
                        let max = tile.window_height_for_tile_height(max_tile_height).round();
                        window_height = f64::min(window_height, max);
                    }

                    WindowHeight::Fixed(tile.tile_height_for_window_height(window_height))
                }
                WindowHeight::Preset(idx) => {
                    let preset = self.options.layout.preset_window_heights[idx];
                    let window_height = match self.resolve_preset_height(preset) {
                        ResolvedSize::Tile(h) => tile.window_height_for_tile_height(h),
                        ResolvedSize::Window(h) => h,
                    };

                    let mut window_height = window_height.round().clamp(1., 100000.);
                    if let Some(max) = max_non_auto_window_height {
                        window_height = f64::min(window_height, max);
                    }

                    let tile_height = tile.tile_height_for_window_height(window_height);
                    WindowHeight::Fixed(tile_height)
                }
            })
            .collect::<Vec<_>>();

        // In tabbed display mode, fill fixed heights right away.
        if is_tabbed {
            // All tiles have the same height, equal to the height of the only fixed tile (if any).
            let tabbed_height = heights
                .iter()
                .find_map(|h| {
                    if let WindowHeight::Fixed(h) = h {
                        Some(*h)
                    } else {
                        None
                    }
                })
                .unwrap_or(max_tile_height);

            // We also take min height of all tabs into account.
            let min_height = min_size
                .iter()
                .map(|size| NotNan::new(size.h).unwrap())
                .max()
                .map(NotNan::into_inner)
                .unwrap();
            // But, if there's a larger-than-workspace tab, we don't want to force all tabs to that
            // size.
            let min_height = f64::min(max_tile_height, min_height);
            let tabbed_height = f64::max(tabbed_height, min_height);

            heights.fill(WindowHeight::Fixed(tabbed_height));

            // The following logic will apply individual min/max height, etc.
        }

        let gaps_left = self.options.layout.gaps * (self.tiles.len() + 1) as f64;
        let mut height_left = working_size.h - gaps_left;
        let mut auto_tiles_left = self.tiles.len();

        // Subtract all fixed-height tiles.
        for (h, (min_size, max_size)) in zip(&mut heights, zip(&min_size, &max_size)) {
            // Check if the tile has an exact height constraint.
            if min_size.h == max_size.h {
                *h = WindowHeight::Fixed(min_size.h);
            }

            if let WindowHeight::Fixed(h) = h {
                if max_size.h > 0. {
                    *h = f64::min(*h, max_size.h);
                }
                *h = f64::max(*h, min_size.h);

                height_left -= *h;
                auto_tiles_left -= 1;
            }
        }

        let mut total_weight: f64 = heights
            .iter()
            .filter_map(|h| {
                if let WindowHeight::Auto { weight } = *h {
                    Some(weight)
                } else {
                    None
                }
            })
            .sum();

        // Iteratively try to distribute the remaining height, checking against tile min heights.
        // Pick an auto height according to the current sizes, then check if it satisfies all
        // remaining min heights. If not, allocate fixed height to those tiles and repeat the
        // loop. On each iteration the auto height will get smaller.
        //
        // NOTE: we do not respect max height here. Doing so would complicate things: if the current
        // auto height is above some tile's max height, then the auto height can become larger.
        // Combining this with the min height loop is where the complexity appears.
        //
        // However, most max height uses are for fixed-size dialogs, where min height == max_height.
        // This case is separately handled above.
        'outer: while auto_tiles_left > 0 {
            // Wayland requires us to round the requested size for a window to integer logical
            // pixels, therefore we compute the remaining auto height dynamically.
            let mut height_left_2 = height_left;
            let mut total_weight_2 = total_weight;
            for ((h, tile), min_size) in zip(zip(&mut heights, &self.tiles), &min_size) {
                let weight = match *h {
                    WindowHeight::Auto { weight } => weight,
                    WindowHeight::Fixed(_) => continue,
                    WindowHeight::Preset(_) => unreachable!(),
                };
                let factor = weight / total_weight_2;

                // Compute the current auto height.
                let mut auto = height_left_2 * factor;

                // Check if the auto height satisfies the min height.
                if min_size.h > auto {
                    auto = min_size.h;
                    *h = WindowHeight::Fixed(auto);
                    height_left -= auto;
                    total_weight -= weight;
                    auto_tiles_left -= 1;

                    // If a min height was unsatisfied, then we allocate the tile more than the
                    // auto height, which means that the remaining auto tiles now have less height
                    // to work with, and the loop must run again.
                    //
                    // If we keep going in this loop and break out later, we may allocate less
                    // height to the subsequent tiles than would be available next iteration and
                    // potentially trip their min height check earlier than necessary, leading to
                    // visible snapping.
                    continue 'outer;
                }

                auto = tile.tile_height_for_window_height(
                    tile.window_height_for_tile_height(auto).round().max(1.),
                );

                height_left_2 -= auto;
                total_weight_2 -= weight;
            }

            // All min heights were satisfied, fill them in.
            for (h, tile) in zip(&mut heights, &self.tiles) {
                let weight = match *h {
                    WindowHeight::Auto { weight } => weight,
                    WindowHeight::Fixed(_) => continue,
                    WindowHeight::Preset(_) => unreachable!(),
                };
                let factor = weight / total_weight;

                // Compute the current auto height.
                let auto = height_left * factor;
                let auto = tile.tile_height_for_window_height(
                    tile.window_height_for_tile_height(auto).round().max(1.),
                );

                *h = WindowHeight::Fixed(auto);
                height_left -= auto;
                total_weight -= weight;
                auto_tiles_left -= 1;
            }

            assert_eq!(auto_tiles_left, 0);
        }

        for (tile_idx, (tile, h)) in zip(&mut self.tiles, heights).enumerate() {
            let WindowHeight::Fixed(height) = h else {
                unreachable!()
            };

            let size = Size::from((width, height));

            // In tabbed mode, only the visible window participates in the transaction.
            let is_active = tile_idx == self.active_tile_idx;
            let transaction = if self.display_mode == ColumnDisplay::Tabbed && !is_active {
                None
            } else {
                Some(transaction.clone())
            };

            tile.request_tile_size(size, animate, transaction);
        }
    }

    fn width(&self) -> f64 {
        let mut tiles_width = self
            .data
            .iter()
            .map(|data| NotNan::new(data.size.w).unwrap())
            .max()
            .map(NotNan::into_inner)
            .unwrap();

        if self.display_mode == ColumnDisplay::Tabbed && self.sizing_mode().is_normal() {
            let extra_size = self.tab_indicator.extra_size(self.tiles.len(), self.scale);
            tiles_width += extra_size.w;
        }

        tiles_width
    }

    /// Converts all heights in the column to automatic, preserving the apparent heights.
    ///
    /// All weights are recomputed to preserve the current tile heights while "centering" the
    /// weights at the median window height (it gets weight = 1).
    ///
    /// One case where apparent heights will not be preserved is when the column is taller than the
    /// working area.
    fn convert_heights_to_auto(&mut self) {
        let heights: Vec<_> = self.tiles.iter().map(|tile| tile.tile_size().h).collect();

        // Weights are invariant to multiplication: a column with weights 2, 2, 1 is equivalent to
        // a column with weights 4, 4, 2. So we find the median window height and use that as 1.
        let mut sorted = heights.clone();
        sorted.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
        let median = sorted[sorted.len() / 2];

        for (data, height) in zip(&mut self.data, heights) {
            let weight = height / median;
            data.height = WindowHeight::Auto { weight };
        }
    }

    fn set_fullscreen(&mut self, is_fullscreen: bool) {
        if self.is_pending_fullscreen == is_fullscreen {
            return;
        }

        if is_fullscreen {
            assert!(self.tiles.len() == 1 || self.display_mode == ColumnDisplay::Tabbed);
        }

        self.is_pending_fullscreen = is_fullscreen;
        self.update_tile_sizes(true);
    }

    fn set_maximized(&mut self, maximize: bool) {
        if self.is_pending_maximized == maximize {
            return;
        }

        if maximize {
            assert!(self.tiles.len() == 1 || self.display_mode == ColumnDisplay::Tabbed);
        }

        self.is_pending_maximized = maximize;
        self.update_tile_sizes(true);
    }

    fn tiles_origin(&self) -> Point<f64, Logical> {
        let mut origin = Point::from((0., 0.));

        match self.sizing_mode() {
            SizingMode::Normal => (),
            SizingMode::Maximized => {
                origin.y += self.parent_area.loc.y;
                return origin;
            }
            SizingMode::Fullscreen => return origin,
        }

        origin.y += self.working_area.loc.y + self.options.layout.gaps;

        if self.display_mode == ColumnDisplay::Tabbed {
            origin += self
                .tab_indicator
                .content_offset(self.tiles.len(), self.scale);
        }

        origin
    }

    fn tile_offsets_iter(
        &self,
        data: impl Iterator<Item = TileData>,
    ) -> impl Iterator<Item = Point<f64, Logical>> {
        // FIXME: this should take into account always-center-single-column, which means that
        // Column should somehow know when it is being centered due to being the single column on
        // the workspace or some other reason.
        let center = self.options.layout.center_focused_column == CenterFocusedColumn::Always;
        let gaps = self.options.layout.gaps;
        let tabbed = self.display_mode == ColumnDisplay::Tabbed;

        // Does not include extra size from the tab indicator.
        let tiles_width = self
            .data
            .iter()
            .map(|data| NotNan::new(data.size.w).unwrap())
            .max()
            .map(NotNan::into_inner)
            .unwrap_or(0.);

        let mut origin = self.tiles_origin();

        // Chain with a dummy value to be able to get one past all tiles' Y.
        let dummy = TileData {
            height: WindowHeight::auto_1(),
            size: Size::default(),
            interactively_resizing_by_left_edge: false,
        };
        let data = data.chain(iter::once(dummy));

        data.map(move |data| {
            let mut pos = origin;

            if center {
                pos.x += (tiles_width - data.size.w) / 2.;
            } else if data.interactively_resizing_by_left_edge {
                pos.x += tiles_width - data.size.w;
            }

            if !tabbed {
                origin.y += data.size.h + gaps;
            }

            pos
        })
    }

    fn tile_offsets(&self) -> impl Iterator<Item = Point<f64, Logical>> + '_ {
        self.tile_offsets_iter(self.data.iter().copied())
    }

    pub fn tiles(&self) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>)> + '_ {
        let offsets = self.tile_offsets_iter(self.data.iter().copied());
        zip(&self.tiles, offsets)
    }

    fn tiles_mut(&mut self) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> + '_ {
        let offsets = self.tile_offsets_iter(self.data.iter().copied());
        zip(&mut self.tiles, offsets)
    }

    fn tab_indicator_area(&self) -> Rectangle<f64, Logical> {
        // We'd like to use the active tile's animated size for the tab indicator, however we need
        // to be mindful of the case where the active tile is smaller than some other tile in the
        // column. The column assumes the size of the largest tile.
        //
        // We expect users to mainly resize tabbed columns by width, so matching the animated size
        // is more important here. Besides, we always try to resize all windows in a column to the
        // same width when possible, and also the animation for going into tabbed mode doesn't move
        // tiles horizontally as much.
        //
        // For height though, it's a different story. First, users probably aren't resizing a
        // tabbed column by height. Second, we don't match windows by height, so it's easy to have
        // a smaller active tile than the rest of the column, e.g. by adding a fixed-size dialog.
        // Then, switching to that dialog and back should ideally keep the tab indicator position
        // fixed. Third, the animation for making a column tabbed moves tiles vertically, and using
        // the active tile's animated size in this case only works for the topmost tile, and looks
        // broken otherwise.
        let mut max_height = 0.;
        for tile in &self.tiles {
            max_height = f64::max(max_height, tile.tile_size().h);
        }

        let tile = &self.tiles[self.active_tile_idx];
        let area_size = Size::from((tile.animated_tile_size().w, max_height));

        Rectangle::new(self.tiles_origin(), area_size)
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        for tile in &mut self.tiles {
            if tile.window().id() == id {
                tile.start_open_animation();

                // Animate the appearance of the tab indicator.
                if self.display_mode == ColumnDisplay::Tabbed
                    && self.sizing_mode().is_normal()
                    && self.tiles.len() == 1
                    && !self.tab_indicator.config().hide_when_single_tab
                {
                    self.tab_indicator.start_open_animation(
                        self.clock.clone(),
                        self.options.animations.window_open.anim,
                    );
                }

                return true;
            }
        }

        false
    }

    #[cfg(test)]
    fn verify_invariants(&self) {
        assert!(!self.tiles.is_empty(), "columns can't be empty");
        assert!(self.active_tile_idx < self.tiles.len());
        assert_eq!(self.tiles.len(), self.data.len());

        if !self.pending_sizing_mode().is_normal() {
            assert!(self.tiles.len() == 1 || self.display_mode == ColumnDisplay::Tabbed);
        }

        let is_tabbed = self.display_mode == ColumnDisplay::Tabbed;

        let tile_count = self.tiles.len();
        if tile_count == 1 {
            if let WindowHeight::Auto { weight } = self.data[0].height {
                assert_eq!(
                    weight, 1.,
                    "auto height weight must reset to 1 for a single window"
                );
            }
        }

        let working_size = self.working_area.size;
        let extra_size = self.extra_size();
        let gaps = self.options.layout.gaps;

        let mut found_fixed = false;
        let mut total_height = 0.;
        let mut total_min_height = 0.;
        for (tile, data) in zip(&self.tiles, &self.data) {
            assert!(Rc::ptr_eq(&self.options, &tile.options));
            assert_eq!(self.clock, tile.clock);
            assert_eq!(self.scale, tile.scale());
            assert_eq!(
                self.pending_sizing_mode(),
                tile.window().pending_sizing_mode()
            );
            assert_eq!(self.view_size, tile.view_size());
            tile.verify_invariants();

            let mut data2 = *data;
            data2.update(tile);
            assert_eq!(data, &data2, "tile data must be up to date");

            if matches!(data.height, WindowHeight::Fixed(_)) {
                assert!(
                    !found_fixed,
                    "there can only be one fixed-height window in a column"
                );
                found_fixed = true;
            }

            if let WindowHeight::Preset(idx) = data.height {
                assert!(self.options.layout.preset_window_heights.len() > idx);
            }

            let requested_size = tile.window().requested_size().unwrap();
            let requested_tile_height =
                tile.tile_height_for_window_height(f64::from(requested_size.h));
            let min_tile_height = f64::max(1., tile.min_size_nonfullscreen().h);

            if !is_tabbed
                && self.pending_sizing_mode().is_normal()
                && self.scale.round() == self.scale
                && working_size.h.round() == working_size.h
                && gaps.round() == gaps
            {
                let total_height = requested_tile_height + gaps * 2. + extra_size.h;
                let total_min_height = min_tile_height + gaps * 2. + extra_size.h;
                let max_height = f64::max(total_min_height, working_size.h);
                assert!(
                    total_height <= max_height,
                    "each tile in a column mustn't go beyond working area height \
                     (tile height {total_height} > max height {max_height})"
                );
            }

            total_height += requested_tile_height;
            total_min_height += min_tile_height;
        }

        if !is_tabbed
            && tile_count > 1
            && self.scale.round() == self.scale
            && working_size.h.round() == working_size.h
            && gaps.round() == gaps
        {
            total_height += gaps * (tile_count + 1) as f64 + extra_size.h;
            total_min_height += gaps * (tile_count + 1) as f64 + extra_size.h;
            let max_height = f64::max(total_min_height, working_size.h);
            assert!(
                total_height <= max_height,
                "multiple tiles in a column mustn't go beyond working area height \
                 (total height {total_height} > max height {max_height})"
            );
        }
    }
}

fn compute_working_area(
    parent_area: Rectangle<f64, Logical>,
    scale: f64,
    struts: Struts,
) -> Rectangle<f64, Logical> {
    let mut working_area = parent_area;

    // Add struts.
    working_area.size.w = f64::max(0., working_area.size.w - struts.left.0 - struts.right.0);
    working_area.loc.x += struts.left.0;

    working_area.size.h = f64::max(0., working_area.size.h - struts.top.0 - struts.bottom.0);
    working_area.loc.y += struts.top.0;

    // Round location to start at a physical pixel.
    let loc = working_area
        .loc
        .to_physical_precise_ceil(scale)
        .to_logical(scale);

    let mut size_diff = (loc - working_area.loc).to_size();
    size_diff.w = f64::min(working_area.size.w, size_diff.w);
    size_diff.h = f64::min(working_area.size.h, size_diff.h);

    working_area.size -= size_diff;
    working_area.loc = loc;

    working_area
}

fn resolve_preset_size(
    preset: PresetSize,
    options: &Options,
    view_size: f64,
    extra_size: f64,
) -> ResolvedSize {
    match preset {
        PresetSize::Proportion(proportion) => ResolvedSize::Tile(
            (view_size - options.layout.gaps) * proportion - options.layout.gaps - extra_size,
        ),
        PresetSize::Fixed(width) => ResolvedSize::Window(f64::from(width)),
    }
}

#[cfg(test)]
mod tests {
    use niri_config::FloatOrInt;

    use super::*;
    use crate::utils::round_logical_in_physical;

    #[test]
    fn working_area_starts_at_physical_pixel() {
        let struts = Struts {
            left: FloatOrInt(0.5),
            right: FloatOrInt(1.),
            top: FloatOrInt(0.75),
            bottom: FloatOrInt(1.),
        };

        let parent_area = Rectangle::from_size(Size::from((1280., 720.)));
        let area = compute_working_area(parent_area, 1., struts);

        assert_eq!(round_logical_in_physical(1., area.loc.x), area.loc.x);
        assert_eq!(round_logical_in_physical(1., area.loc.y), area.loc.y);
    }

    #[test]
    fn large_fractional_strut() {
        let struts = Struts {
            left: FloatOrInt(0.),
            right: FloatOrInt(0.),
            top: FloatOrInt(50000.5),
            bottom: FloatOrInt(0.),
        };

        let parent_area = Rectangle::from_size(Size::from((1280., 720.)));
        compute_working_area(parent_area, 1., struts);
    }
}
