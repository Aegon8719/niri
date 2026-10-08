use std::collections::HashSet;
use std::time::Duration;

use smithay::backend::input::Keycode;

/// Two short, unchorded Super presses. Normal modifier events still reach clients.
#[derive(Default)]
pub struct SuperTap {
    down: Option<(Keycode, Duration)>,
    released: Option<Duration>,
    other_keys: HashSet<Keycode>,
}

impl SuperTap {
    pub fn cancel(&mut self) {
        self.down = None;
        self.released = None;
    }

    pub fn key(&mut self, code: Keycode, is_super: bool, pressed: bool, now: Duration, allowed: bool) -> bool {
        if !is_super {
            if pressed { self.other_keys.insert(code); } else { self.other_keys.remove(&code); }
            self.cancel();
            return false;
        }
        if !allowed || !self.other_keys.is_empty() {
            self.cancel();
            return false;
        }
        if pressed {
            if self.down.is_some_and(|(key, _)| key == code) { return false; }
            if self.down.is_some() { self.cancel(); return false; }
            self.down = Some((code, now));
            return false;
        }
        let Some((key, start)) = self.down.take() else { return false; };
        if key != code || now.saturating_sub(start) > Duration::from_millis(250) {
            self.cancel();
            return false;
        }
        if self.released.take().is_some_and(|last| now.saturating_sub(last) <= Duration::from_millis(350)) {
            return true;
        }
        self.released = Some(now);
        false
    }
}
