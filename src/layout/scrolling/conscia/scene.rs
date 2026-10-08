//! Backend-independent scene geometry and smooth transitions keyed by WindowId.
use super::core::WindowId;
use super::geometry::{LayoutSnapshot, WindowRole};
#[derive(Clone, Debug)]
pub struct SceneItem {
    pub window: WindowId,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub scale: f64,
    pub opacity: f32,
    pub role: WindowRole,
}
pub fn scene(layout: &LayoutSnapshot, offset: f64) -> Vec<SceneItem> {
    let mut reel_index = 0;
    layout
        .placements
        .iter()
        .map(|p| {
            let mut item = SceneItem {
                window: p.window,
                x: p.rect.x as f64 + layout.origin_offset.0,
                y: p.rect.y as f64 + layout.origin_offset.1,
                width: p.rect.width as f64,
                height: p.rect.height as f64,
                scale: p.scale,
                opacity: p.opacity,
                role: p.role,
            };
            if p.role != WindowRole::Main && layout.reel_hidden {
                item.role = WindowRole::Hidden;
                item.opacity = 0.;
            } else if p.role != WindowRole::Main {
                let index = reel_index;
                reel_index += 1;
                let pos = index as f64 - offset;
                item.y = layout.reel.y as f64 + layout.origin_offset.1 + pos * item.height;
                let visible = pos >= 0. && pos + 1. <= layout.slot_count as f64;
                item.opacity = if visible { 1. } else { 0. };
                item.role = if visible {
                    WindowRole::Reel { index }
                } else {
                    WindowRole::Hidden
                };
            }
            item
        })
        .collect()
}
#[derive(Clone, Debug)]
pub struct Transition {
    pub from: Vec<SceneItem>,
    pub to: Vec<SceneItem>,
    pub progress: f32,
}
impl Transition {
    pub fn sample(&self) -> Vec<SceneItem> {
        let t = self.progress.clamp(0., 1.) as f64;
        let e = t * t * (3. - 2. * t);
        self.to
            .iter()
            .map(|b| {
                let Some(a) = self.from.iter().find(|a| a.window == b.window) else {
                    let mut b = b.clone();
                    b.opacity *= e as f32;
                    return b;
                };
                // Invisible endpoints may be offscreen. Fade at the visible endpoint
                // instead of dragging a partly clipped surface through the output edge.
                if a.opacity == 0. || b.opacity == 0. {
                    let mut item = if a.opacity == 0. {
                        b.clone()
                    } else {
                        a.clone()
                    };
                    item.opacity = a.opacity + (b.opacity - a.opacity) * e as f32;
                    item.role = b.role;
                    return item;
                }
                let lerp = |a: f64, b: f64| a + (b - a) * e;
                SceneItem {
                    window: b.window,
                    x: lerp(a.x, b.x),
                    y: lerp(a.y, b.y),
                    width: lerp(a.width, b.width),
                    height: lerp(a.height, b.height),
                    scale: lerp(a.scale, b.scale),
                    opacity: lerp(a.opacity as f64, b.opacity as f64) as f32,
                    role: b.role,
                }
            })
            .collect()
    }
}
/// Exponential convergence followed by exact integer settling, independent of refresh rate.
pub fn snap_step(offset: f64, dt: f64) -> f64 {
    let target = offset.round();
    let next = target + (offset - target) * (-22. * dt.clamp(0., 0.1)).exp();
    if (next - target).abs() < 0.001 {
        target
    } else {
        next
    }
}
