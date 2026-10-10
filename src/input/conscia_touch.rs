//! Touchscreen arbitration. Targets and directions are frozen at touch-down.
use super::*;
use smithay::backend::input::{InputTime, TouchSlot};
use smithay::desktop::Window;
use crate::layout::workspace::WorkspaceId;

const SLOP: f64 = 10.;
const SWIPE: f64 = 80.;
const HOLD_US: u64 = 250_000;

#[derive(Default)]
pub struct Controller {
    session: Option<Session>,
}

struct Finger {
    slot: TouchSlot,
    start: Point<f64, Logical>,
    down: Point<f64, Logical>,
    pos: Point<f64, Logical>,
}

struct Session {
    output: Output,
    workspace: WorkspaceId,
    origin: Point<f64, Logical>,
    fingers: Vec<Finger>,
    peak: usize,
    started: u64,
    target: Option<Window>,
    reel: bool,
    title: bool,
    main: bool,
    overview: bool,
    outward: f64,
    owned: bool,
    finished: bool,
    scrolling: bool,
    dragging: bool,
    swiping: bool,
    moved: bool,
    deferred: Option<(Option<(WlSurface, Point<f64, Logical>)>, DownEvent)>,
}

fn centroid(fingers: &[Finger], start: bool) -> Point<f64, Logical> {
    let sum = fingers.iter().fold(Point::from((0., 0.)), |sum, f| sum + if start { f.start } else { f.pos });
    sum.downscale(fingers.len() as f64)
}

fn spread(fingers: &[Finger], start: bool) -> f64 {
    let center = centroid(fingers, start);
    (fingers.iter().map(|f| {
        let d = (if start { f.start } else { f.pos }) - center;
        d.x * d.x + d.y * d.y
    }).sum::<f64>() / fingers.len() as f64).sqrt()
}

fn horizontal(delta: Point<f64, Logical>) -> bool {
    delta.x.abs() >= SWIPE && delta.x.abs() > delta.y.abs() * 2.
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum MultiAction { Up, Down, Expand, Restore, OverviewOpen, OverviewClose }

fn multi_action(fingers: &[Finger], peak: usize, main: bool) -> Option<MultiAction> {
    if fingers.len() != peak { return None; }
    let delta = centroid(fingers, false) - centroid(fingers, true);
    if peak == 4 && delta.y.abs() >= SWIPE && delta.y.abs() > delta.x.abs() * 2. {
        return Some(if delta.y < 0. { MultiAction::OverviewOpen } else { MultiAction::OverviewClose });
    }
    if peak == 3 && main {
        let before = spread(fingers, true);
        let after = spread(fingers, false);
        if before >= 20. && (after - before).abs() >= 30. {
            if after / before >= 1.25 { return Some(MultiAction::Expand); }
            if after / before <= 0.75 { return Some(MultiAction::Restore); }
        }
    }
    if peak == 3 && delta.y.abs() >= SWIPE && delta.y.abs() > delta.x.abs() * 2. {
        return Some(if delta.y < 0. { MultiAction::Up } else { MultiAction::Down });
    }
    None
}

impl State {
    fn conscia_touch_allowed(&self) -> bool {
        !self.niri.is_locked()
            && !self.niri.screenshot_ui.is_open() && !self.niri.window_mru_ui.is_open()
            && !self.niri.seat.get_keyboard().unwrap().is_grabbed()
    }

    fn conscia_touch_valid(&self, s: &Session) -> bool {
        self.conscia_touch_allowed() && self.niri.layout.is_overview_open() == s.overview
            && self.niri.layout.monitor_for_output(&s.output)
            .is_some_and(|m| m.active_workspace_ref().id() == s.workspace
                && (s.overview || s.peak > 1 || s.target.as_ref().is_none_or(|target|
                    m.active_workspace_ref().scrolling().conscia_touch_matches(target, s.reel))))
            && self.niri.global_space.output_geometry(&s.output).is_some_and(|geo| geo.loc.to_f64() == s.origin)
    }

    pub(super) fn conscia_touch_cancel(&mut self) {
        if let Some(s) = self.niri.conscia_touch.session.take() {
            self.conscia_touch_abort_drag(&s);
            self.niri.queue_redraw_all();
        }
    }

    fn conscia_touch_abort_drag(&mut self, s: &Session) {
        for ws in self.niri.layout.workspaces_mut() {
            if ws.id() == s.workspace { ws.scrolling_mut().conscia_drag_end(true); }
        }
    }

    pub(super) fn conscia_touch_down(&mut self, slot: TouchSlot, pos: Point<f64, Logical>, time: InputTime) -> bool {
        if let Some(mut s) = self.niri.conscia_touch.session.take() {
            if !self.conscia_touch_valid(&s) {
                self.conscia_touch_abort_drag(&s); s.finished = true; s.deferred = None;
            }
            s.fingers.push(Finger { slot, start: pos, down: pos, pos });
            s.peak = s.peak.max(s.fingers.len());
            if s.peak >= 2 { s.deferred = None; self.conscia_touch_abort_drag(&s); s.dragging = false; }
            if s.peak >= 3 && !s.owned {
                self.niri.seat.get_touch().unwrap().cancel(self);
                s.owned = true;
            }
            // Reset the multi-finger baseline when another finger lands.
            for f in &mut s.fingers { f.start = f.pos; }
            let owned = s.owned;
            self.niri.conscia_touch.session = Some(s);
            return owned;
        }
        let handle = self.niri.seat.get_touch().unwrap();
        if !self.conscia_touch_allowed() || handle.is_grabbed() { return false; }
        let under = self.niri.contents_under(pos);
        if under.layer.is_some() { return false; }
        let Some((output, local)) = self.niri.output_under(pos) else { return false; };
        let Some(mon) = self.niri.layout.monitor_for_output(output) else { return false; };
        let ws = mon.active_workspace_ref();
        let (target, reel, title, outward) = ws.scrolling().conscia_touch_target(local);
        let overview = self.niri.layout.is_overview_open();
        let owned = !overview && (reel || title);
        let deferred = (title && !overview).then(|| (under.surface, DownEvent {
            slot, location: pos, serial: SERIAL_COUNTER.next_serial(), time,
        }));
        self.niri.conscia_touch.session = Some(Session {
            output: output.clone(), workspace: ws.id(), origin: pos - local,
            fingers: vec![Finger { slot, start: pos, down: pos, pos }], peak: 1,
            started: time.micros(), target, reel, title,
            main: !overview && ws.scrolling().conscia_touch_main_at(local), overview, outward,
            owned, finished: false, scrolling: false, dragging: false, swiping: false, moved: false, deferred,
        });
        self.niri.pointer_visibility = PointerVisibility::Disabled;
        owned
    }

    pub(super) fn conscia_touch_motion(&mut self, slot: TouchSlot, pos: Point<f64, Logical>, time: InputTime) -> bool {
        let Some(mut s) = self.niri.conscia_touch.session.take() else { return false; };
        let Some(index) = s.fingers.iter().position(|f| f.slot == slot) else {
            self.niri.conscia_touch.session = Some(s); return false;
        };
        let last = s.fingers[index].pos;
        s.fingers[index].pos = pos;
        if !self.conscia_touch_valid(&s) {
            self.conscia_touch_abort_drag(&s); s.finished = true; s.deferred = None;
        }
        if !s.finished && s.peak == 1 && s.owned {
            let delta = pos - s.fingers[0].start;
            s.moved |= delta.x.hypot(delta.y) >= SLOP;
            let ws = self.niri.layout.monitor_for_output_mut(&s.output).unwrap().active_workspace();
            if s.reel {
                if !s.scrolling && delta.y.abs() >= SLOP && delta.y.abs() > delta.x.abs() * 1.5 {
                    s.scrolling = true;
                    ws.conscia_scroll(-delta.y, false);
                } else if s.scrolling { ws.conscia_scroll(last.y - pos.y, false); }
            } else if s.title {
                if !s.dragging && time.micros().saturating_sub(s.started) < HOLD_US && horizontal(delta) {
                    s.swiping = true;
                }
                if !s.swiping && !s.dragging && s.moved && time.micros().saturating_sub(s.started) >= HOLD_US {
                    if let Some(target) = &s.target {
                        s.dragging = ws.scrolling_mut().conscia_drag_start_window(target, s.fingers[0].start - s.origin);
                    }
                }
                if s.dragging { ws.scrolling_mut().conscia_drag_motion(pos - s.origin); }
            }
            self.niri.queue_redraw_all();
        }
        let owned = s.owned;
        self.niri.conscia_touch.session = Some(s);
        owned
    }

    pub(super) fn conscia_touch_up(&mut self, slot: TouchSlot, time: InputTime) -> bool {
        let Some(mut s) = self.niri.conscia_touch.session.take() else { return false; };
        let Some(index) = s.fingers.iter().position(|f| f.slot == slot) else {
            self.niri.conscia_touch.session = Some(s); return false;
        };
        let owned = s.owned;
        if !s.finished && self.conscia_touch_valid(&s) {
            let delta = centroid(&s.fingers, false) - centroid(&s.fingers, true);
            let mon = self.niri.layout.monitor_for_output_mut(&s.output).unwrap();
            let main = s.main && s.fingers.iter().all(|f|
                mon.active_workspace_ref().scrolling().conscia_touch_main_at(f.down - s.origin));
            let action = multi_action(&s.fingers, s.peak, main);
            match action {
                Some(MultiAction::Up) => mon.switch_workspace_up(),
                Some(MultiAction::Down) => mon.switch_workspace_down(),
                Some(MultiAction::Expand) => mon.active_workspace().scrolling_mut().conscia_touch_expand(true),
                Some(MultiAction::Restore) => mon.active_workspace().scrolling_mut().conscia_touch_expand(false),
                Some(MultiAction::OverviewOpen | MultiAction::OverviewClose) | None => (),
            }
            if s.peak == 1 && s.owned {
                let ws = mon.active_workspace();
                if s.dragging { ws.scrolling_mut().conscia_drag_end(false); }
                else if !s.scrolling && horizontal(delta) && (s.reel || s.swiping || time.micros().saturating_sub(s.started) < HOLD_US) {
                    if let Some(target) = &s.target {
                        if delta.x * s.outward > 0. {
                            if let Some((_, mapped)) = self.niri.layout.windows().find(|(_, w)| &w.window == target) {
                                mapped.toplevel().send_close();
                            }
                        } else if s.reel { ws.scrolling_mut().conscia_touch_add(target); }
                        else { ws.scrolling_mut().conscia_touch_release(target); }
                    }
                } else if !s.moved && s.reel {
                    if let Some(target) = &s.target { ws.scrolling_mut().conscia_touch_promote(target); }
                } else if !s.moved {
                    if let Some((focus, event)) = s.deferred.take() {
                        if let Some(target) = &s.target { self.niri.layout.activate_window(target); }
                        let handle = self.niri.seat.get_touch().unwrap();
                        handle.down(self, focus, &event);
                        handle.frame(self);
                        handle.up(self, &UpEvent { slot, serial: SERIAL_COUNTER.next_serial(), time });
                        handle.frame(self);
                    }
                }
            }
            match action {
                Some(MultiAction::OverviewOpen) => { self.niri.layout.open_overview(); }
                Some(MultiAction::OverviewClose) => { self.niri.layout.close_overview(); }
                _ => (),
            }
            if s.owned {
                self.niri.layout.focus_output(&s.output);
                self.niri.focus_layer_surface_if_on_demand(None);
                self.niri.queue_redraw_all();
            }
        } else { self.conscia_touch_abort_drag(&s); }
        s.finished = true;
        s.fingers.remove(index);
        if !s.fingers.is_empty() { self.niri.conscia_touch.session = Some(s); }
        owned
    }
}

