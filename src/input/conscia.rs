//! Pointer arbitration for Conscia. Never forward a thumbnail click to a client.
use super::*;

impl State {
    fn conscia_pointer_allowed(&self) -> bool {
        !self.niri.is_locked() && !self.niri.layout.is_overview_open()
            && !self.niri.screenshot_ui.is_open() && !self.niri.window_mru_ui.is_open()
            && !self.niri.seat.get_pointer().unwrap().is_grabbed()
            && !self.niri.seat.get_keyboard().unwrap().is_grabbed()
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
