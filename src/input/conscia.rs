//! Pointer arbitration for Conscia. Never forward a thumbnail click to a client.
use super::*;
use smithay::desktop::Window;
use crate::layout::workspace::WorkspaceId;

#[derive(Default)]
pub(crate) struct PadReel {
    sequence: Option<PadReelSequence>,
}

struct PadReelSequence {
    output: Output,
    workspace: WorkspaceId,
    target: Option<Window>,
    outward: f64,
    x: f64,
    y: f64,
    active_x: bool,
    active_y: bool,
    // 0: undecided, 1: vertical scrolling, 2: horizontal window gesture.
    axis: u8,
    cancelled: bool,
}


impl State {
    fn conscia_pointer_allowed(&self) -> bool {
        !self.niri.is_locked() && !self.niri.layout.is_overview_open()
            && !self.niri.screenshot_ui.is_open() && !self.niri.window_mru_ui.is_open()
            && !self.niri.seat.get_pointer().unwrap().is_grabbed()
            && !self.niri.seat.get_keyboard().unwrap().is_grabbed()
    }

    /// libinput reports two-finger motion through Finger axis events, not swipe events.
    /// Preserve the initial target until both axes stop, even if the cursor moves.
    pub(super) fn conscia_touchpad_reel(
        &mut self, dx: Option<f64>, dy: Option<f64>, physical_x: f64,
    ) -> bool {
        let mut sequence = self.niri.conscia_pad_reel.sequence.take();
        if sequence.is_none() {
            if !self.conscia_pointer_allowed() || (dx.unwrap_or(0.) == 0. && dy.unwrap_or(0.) == 0.) {
                return false;
            }
            let pos = self.niri.seat.get_pointer().unwrap().current_location();
            if self.niri.contents_under(pos).layer.is_some() { return false; }
            let Some((output, local)) = self.niri.output_under(pos) else { return false; };
            let Some(mon) = self.niri.layout.monitor_for_output(output) else { return false; };
            let ws = mon.active_workspace_ref();
            let (target, reel, _, outward) = ws.scrolling().conscia_touch_target(local);
            if !reel { return false; }
            sequence = Some(PadReelSequence {
                output: output.clone(), workspace: ws.id(), target, outward,
                x: 0., y: 0., active_x: false, active_y: false, axis: 0, cancelled: false,
            });
        }
        let mut gesture = sequence.unwrap();
        if let Some(dx) = dx { gesture.active_x = dx != 0.; }
        if let Some(dy) = dy { gesture.active_y = dy != 0.; }
        gesture.x += physical_x;
        gesture.y += dy.unwrap_or(0.);
        let ended = !gesture.active_x && !gesture.active_y;
        let valid = self.conscia_pointer_allowed()
            && self.niri.layout.monitor_for_output(&gesture.output).is_some_and(|mon| {
                let ws = mon.active_workspace_ref();
                ws.id() == gesture.workspace && gesture.target.as_ref().is_none_or(|target|
                    ws.scrolling().conscia_touch_matches(target, true))
            });
        gesture.cancelled |= !valid || !gesture.x.is_finite() || !gesture.y.is_finite();
        if !gesture.cancelled {
            let was_vertical = gesture.axis == 1;
            if gesture.axis == 0 {
                if gesture.y.abs() >= 10. && gesture.y.abs() > gesture.x.abs() * 1.5 {
                    gesture.axis = 1;
                } else if gesture.x.abs() >= 10. && gesture.x.abs() > gesture.y.abs() * 1.5 {
                    gesture.axis = 2;
                }
            }
            if gesture.axis == 1 {
                let ws = self.niri.layout.monitor_for_output_mut(&gesture.output).unwrap().active_workspace();
                ws.conscia_scroll(if was_vertical { dy.unwrap_or(0.) } else { gesture.y }, false);
                self.niri.queue_redraw_all();
            } else if ended && gesture.axis == 2 && gesture.x * gesture.outward >= 80.
                && gesture.x.abs() > gesture.y.abs() * 2. {
                if let Some(target) = &gesture.target {
                    if let Some((_, mapped)) = self.niri.layout.windows().find(|(_, w)| &w.window == target) {
                        mapped.toplevel().send_close();
                    }
                }
            }
        }
        if !ended { self.niri.conscia_pad_reel.sequence = Some(gesture); }
        true
    }

    pub(super) fn conscia_pointer_click(&mut self, left: bool, right: bool) -> bool {
        if !self.conscia_pointer_allowed() { return false; }
        let pos = self.niri.seat.get_pointer().unwrap().current_location();
        let under = self.niri.contents_under(pos);
        if under.layer.is_some() { return false; }
        let Some((output, local)) = self.niri.output_under(pos) else { return false; };
        let output = output.clone();
        let Some(mon) = self.niri.layout.monitor_for_output_mut(&output) else { return false; };
        let ws = mon.active_workspace();
        let handled = if left && ws.conscia_resize_start(local) { true }
            else if ws.conscia_reel_at(local) {
                if left || right { ws.conscia_click(local, right); }
                true
            } else { false };
        if handled {
            self.niri.focus_layer_surface_if_on_demand(None);
            self.niri.layout.focus_output(&output);
            self.niri.queue_redraw_all();
        }
        handled
    }

    pub(super) fn conscia_pointer_scroll(&mut self, delta: f64, slots: bool) -> bool {
        // A tiling drag owns the gesture even if the pointer crosses the reel.
        if self.niri.layout.workspaces().any(|(_, _, ws)| ws.scrolling().conscia_drag_active()) { return true; }
        if !self.conscia_pointer_allowed() { return false; }
        let pos = self.niri.seat.get_pointer().unwrap().current_location();
        if self.niri.contents_under(pos).layer.is_some() { return false; }
        let Some((output, local)) = self.niri.output_under(pos) else { return false; };
        let output = output.clone();
        let Some(mon) = self.niri.layout.monitor_for_output_mut(&output) else { return false; };
        let ws = mon.active_workspace();
        if !ws.conscia_reel_at(local) { return false; }
        ws.conscia_scroll(delta, slots);
        self.niri.queue_redraw_all();
        true
    }

    pub(super) fn conscia_pointer_resize_motion(&mut self, pos: Point<f64, Logical>) {
        if self.niri.is_locked() || self.niri.layout.is_overview_open() || self.niri.screenshot_ui.is_open() {
            self.conscia_pointer_drag_cancel();
            self.conscia_pointer_resize_end();
            return;
        }
        let outputs: Vec<_> = self.niri.global_space.outputs().filter_map(|output| {
            self.niri.global_space.output_geometry(output).map(|geo| (output.clone(), geo.loc.to_f64()))
        }).collect();
        let mut changed = false;
        for (output, origin) in outputs {
            if let Some(mon) = self.niri.layout.monitor_for_output_mut(&output) {
                let ws = mon.active_workspace();
                changed |= ws.conscia_resize_motion(pos - origin);
                changed |= ws.scrolling_mut().conscia_drag_motion(pos - origin);
            }
        }
        if changed { self.niri.queue_redraw_all(); }
    }

    pub(super) fn conscia_pointer_resize_end(&mut self) {
        let mut changed = false;
        for ws in self.niri.layout.workspaces_mut() {
            changed |= ws.conscia_resize_end();
            changed |= ws.scrolling_mut().conscia_drag_end(false);
        }
        if changed { self.niri.queue_redraw_all(); }
    }

    pub(super) fn conscia_pointer_drag_cancel(&mut self) -> bool {
        let mut changed = false;
        for ws in self.niri.layout.workspaces_mut() { changed |= ws.scrolling_mut().conscia_drag_end(true); }
        if changed { self.niri.queue_redraw_all(); }
        changed
    }
}
