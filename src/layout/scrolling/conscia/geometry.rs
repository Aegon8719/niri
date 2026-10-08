//! Pure strict integer layout. Output/layer-shell policy lives in the compositor.
use super::core::{ReelSide, WindowId};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}
impl Rect {
    /// Decorations consume space inside a solved cell, never change the solver.
    pub fn inset(self, padding: i32) -> Self {
        let x = padding.max(0).min((self.width - 1).max(0) / 2);
        let y = padding.max(0).min((self.height - 1).max(0) / 2);
        Self { x: self.x + x, y: self.y + y,
            width: self.width - x * 2, height: self.height - y * 2 }
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x as f64
            && y >= self.y as f64
            && x < (self.x as f64 + self.width as f64)
            && y < (self.y as f64 + self.height as f64)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowRole {
    Main,
    Reel { index: usize },
    Hidden,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    pub window: WindowId,
    pub role: WindowRole,
    pub rect: Rect,
    pub scale: f64,
    pub opacity: f32,
}
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutSnapshot {
    pub placements: Vec<Placement>,
    pub slot_count: usize,
    pub main: Rect,
    pub reel: Rect,
    pub work_area: Rect,
    pub origin_offset: (f64, f64),
    pub reel_hidden: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutError {
    InvalidArea,
    NoIntegerSolution,
    DuplicateWindow,
}
impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::InvalidArea => "invalid work area",
                Self::NoIntegerSolution =>
                    "no strict integer Main/Reel solution for current work area",
                Self::DuplicateWindow => "duplicate WindowId",
            }
        )
    }
}
impl std::error::Error for LayoutError {}
fn fitted_size(w: i32, h: i32) -> Option<(usize, i32, i32)> {
    if w <= 0 || h <= 0 {
        return None;
    }
    if let Some(n) = (3..=4).find(|&n| w % (n + 1) == 0 && h % n == 0) {
        return Some((n as usize, w, h));
    }
    (3..=4)
        .filter_map(|n| {
            let width = w / (n + 1) * (n + 1);
            let height = h / n * n;
            (width > 0 && height > 0).then_some((n as usize, width, height))
        })
        .max_by_key(|&(n, width, height)| (width as i64 * height as i64, std::cmp::Reverse(n)))
}
pub fn solve_slot_count(w: i32, h: i32) -> Option<usize> {
    fitted_size(w, h).map(|(n, _, _)| n)
}
pub fn solve_layout(
    area: Rect,
    windows: &[WindowId],
    side: ReelSide,
) -> Result<LayoutSnapshot, LayoutError> {
    if area.width < 0
        || area.height < 0
        || area.x.checked_add(area.width).is_none()
        || area.y.checked_add(area.height).is_none()
    {
        return Err(LayoutError::InvalidArea);
    }
    if windows
        .iter()
        .collect::<std::collections::HashSet<_>>()
        .len()
        != windows.len()
    {
        return Err(LayoutError::DuplicateWindow);
    }
    let Some((n, width, height)) = fitted_size(area.width, area.height) else {
        return Ok(LayoutSnapshot {
            placements: windows
                .iter()
                .enumerate()
                .map(|(i, &window)| Placement {
                    window,
                    role: if i == 0 {
                        WindowRole::Main
                    } else {
                        WindowRole::Hidden
                    },
                    rect: area,
                    scale: 1.,
                    opacity: if i == 0 && area.width > 0 && area.height > 0 {
                        1.
                    } else {
                        0.
                    },
                })
                .collect(),
            slot_count: 0,
            main: area,
            reel: Rect::default(),
            work_area: area,
            origin_offset: (0., 0.),
            reel_hidden: true,
        });
    };
    let gap_x = area.width - width;
    let gap_y = area.height - height;
    let origin_offset = ((gap_x % 2) as f64 / 2., (gap_y % 2) as f64 / 2.);
    let x = area.x + gap_x / 2;
    let y = area.y + gap_y / 2;
    let sw = width / (n as i32 + 1);
    let mw = width - sw;
    let sh = height / n as i32;
    let (mx, rx) = match side {
        ReelSide::Right => (x, x + mw),
        ReelSide::Left => (x + sw, x),
    };
    let main = Rect {
        x: mx,
        y,
        width: mw,
        height,
    };
    let reel = Rect {
        x: rx,
        y,
        width: sw,
        height,
    };
    let placements = windows
        .iter()
        .enumerate()
        .map(|(i, &window)| {
            if i == 0 {
                Placement {
                    window,
                    role: WindowRole::Main,
                    rect: main,
                    scale: 1.,
                    opacity: 1.,
                }
            } else {
                let visible = i <= n;
                Placement {
                    window,
                    role: if visible {
                        WindowRole::Reel { index: i - 1 }
                    } else {
                        WindowRole::Hidden
                    },
                    rect: Rect {
                        x: rx,
                        y: y + ((i - 1).min(n) as i32) * sh,
                        width: sw,
                        height: sh,
                    },
                    scale: 1. / n as f64,
                    opacity: if visible { 1. } else { 0. },
                }
            }
        })
        .collect();
    Ok(LayoutSnapshot {
        placements,
        slot_count: n,
        main,
        reel,
        work_area: area,
        origin_offset,
        reel_hidden: false,
    })
}
#[derive(Clone, Copy, Debug)]
pub struct TileSplit {
    pub index: usize,
    pub area: Rect,
    pub vertical: bool,
    pub position: i32,
}

pub fn tile_geometry(
    area: Rect,
    count: usize,
    ratios: &[Option<f64>],
) -> (Vec<Rect>, Vec<TileSplit>) {
    fn cut(area: Rect, vertical: bool, size: i32) -> (Rect, Rect) {
        let mut a = area;
        let mut b = area;
        if vertical {
            a.width = size;
            b.x += size;
            b.width -= size;
        } else {
            a.height = size;
            b.y += size;
            b.height -= size;
        }
        (a, b)
    }
    fn split(
        area: Rect,
        reference: Rect,
        count: usize,
        ratios: &[Option<f64>],
        rects: &mut Vec<Rect>,
        splits: &mut Vec<TileSplit>,
    ) {
        if count == 1 {
            rects.push(area);
            return;
        }
        let first = count / 2;
        let index = splits.len();
        // Choose axes from the equal-share layout, so dragging a parent cannot flip child splits.
        let vertical = reference.width >= reference.height;
        let extent = if vertical { area.width } else { area.height };
        let reference_extent = if vertical {
            reference.width
        } else {
            reference.height
        };
        let default_ratio = first as f64 / count as f64;
        let ratio = ratios
            .get(index)
            .copied()
            .flatten()
            .filter(|r| r.is_finite())
            .unwrap_or(default_ratio)
            .clamp(0.01, 0.99);
        let size = if extent > 1 {
            ((extent as f64 * ratio).floor() as i32).clamp(1, extent - 1)
        } else {
            0
        };
        splits.push(TileSplit {
            index,
            area,
            vertical,
            position: if vertical {
                area.x + size
            } else {
                area.y + size
            },
        });
        let (a, b) = cut(area, vertical, size);
        let (ra, rb) = cut(
            reference,
            vertical,
            (reference_extent as i64 * first as i64 / count as i64) as i32,
        );
        split(a, ra, first, ratios, rects, splits);
        split(b, rb, count - first, ratios, rects, splits);
    }
    let mut rects = Vec::with_capacity(count);
    let mut splits = Vec::with_capacity(count.saturating_sub(1));
    if count > 0 {
        split(area, area, count, ratios, &mut rects, &mut splits);
    }
    (rects, splits)
}

pub fn tile_rects(area: Rect, count: usize) -> Vec<Rect> {
    tile_geometry(area, count, &[]).0
}

pub fn tile_main(layout: &mut LayoutSnapshot, extra: &[WindowId]) {
    tile_main_with_ratios(layout, extra, &[]);
}
pub fn tile_main_with_ratios(
    layout: &mut LayoutSnapshot,
    extra: &[WindowId],
    ratios: &[Option<f64>],
) {
    if extra.is_empty() {
        return;
    }
    let Some(main) = layout
        .placements
        .iter()
        .position(|p| p.role == WindowRole::Main)
    else {
        return;
    };
    let rects = tile_geometry(layout.main, extra.len() + 1, ratios).0;
    layout.placements[main].rect = rects[0];
    for (&window, rect) in extra.iter().zip(rects.into_iter().skip(1)) {
        layout.placements.push(Placement {
            window,
            role: WindowRole::Main,
            rect,
            scale: 1.,
            opacity: 1.,
        });
    }
}
/// Scroll changes presentation only. A fractional edge item is hidden, never cut.
pub fn with_offset(mut layout: LayoutSnapshot, offset: f64) -> LayoutSnapshot {
    if layout.reel_hidden || layout.slot_count == 0 {
        return layout;
    }
    let max = layout
        .placements
        .iter()
        .filter(|p| p.role != WindowRole::Main)
        .count()
        .saturating_sub(layout.slot_count) as f64;
    let offset = if offset.is_finite() {
        offset.clamp(0., max)
    } else {
        0.
    };
    let sh = layout.reel.height / layout.slot_count as i32;
    for (i, p) in layout.placements.iter_mut().enumerate().skip(1) {
        if p.role == WindowRole::Main {
            continue;
        }
        let pos = (i - 1) as f64 - offset;
        let visible = pos >= 0. && pos + 1. <= layout.slot_count as f64;
        p.role = if visible {
            WindowRole::Reel { index: i - 1 }
        } else {
            WindowRole::Hidden
        };
        // Snapshot geometry remains integral; scene scroll uses exact floating point positions.
        p.rect.y = (layout.reel.y as f64 + pos * sh as f64)
            .round()
            .clamp(i32::MIN as f64, i32::MAX as f64) as i32;
        p.opacity = if visible { 1. } else { 0. };
    }
    layout
}
