//! Recording clock that understands pauses. Everything recorded (video frames,
//! cursor, clicks, keys, audio) is placed on the same "active" timeline, which
//! simply skips the paused stretches, so the result plays back seamlessly.

use std::sync::Mutex;

use crate::util::now_hns;

pub struct RecClock {
    start: i64,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    paused_at: Option<i64>,
    /// Finished pauses as (wall start, wall end), in 100ns QPC time.
    pauses: Vec<(i64, i64)>,
}

impl RecClock {
    pub fn new(start_hns: i64) -> RecClock {
        RecClock { start: start_hns, state: Mutex::new(State::default()) }
    }

    pub fn is_paused(&self) -> bool {
        self.state.lock().unwrap().paused_at.is_some()
    }

    pub fn set_paused(&self, paused: bool) {
        let mut s = self.state.lock().unwrap();
        let now = now_hns();
        match (paused, s.paused_at) {
            (true, None) => s.paused_at = Some(now),
            (false, Some(p)) => {
                s.pauses.push((p, now));
                s.paused_at = None;
            }
            _ => {}
        }
    }

    pub fn toggle(&self) {
        let p = self.is_paused();
        self.set_paused(!p);
    }

    /// Active-timeline position (100ns) of a wall-clock instant, or None if it
    /// fell inside a pause. Instants before the start map to negative values.
    pub fn active(&self, wall: i64) -> Option<i64> {
        let s = self.state.lock().unwrap();
        if s.paused_at.is_some_and(|p| wall >= p) {
            return None;
        }
        let mut skipped = 0;
        for &(a, b) in &s.pauses {
            if wall >= b {
                skipped += b - a;
            } else if wall >= a {
                return None;
            }
        }
        Some(wall - self.start - skipped)
    }

    /// Current active time; frozen at the pause point while paused.
    pub fn now(&self) -> i64 {
        let s = self.state.lock().unwrap();
        let wall = s.paused_at.unwrap_or_else(now_hns);
        let skipped: i64 = s.pauses.iter().map(|(a, b)| b - a).sum();
        wall - self.start - skipped
    }

    /// Active-timeline seconds where the recording was paused (shown as cuts).
    pub fn cuts(&self) -> Vec<f64> {
        let s = self.state.lock().unwrap();
        let mut skipped = 0;
        let mut out = Vec::new();
        for &(a, b) in &s.pauses {
            out.push((a - self.start - skipped) as f64 / 1e7);
            skipped += b - a;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pauses_are_skipped() {
        let c = RecClock::new(1000);
        {
            let mut s = c.state.lock().unwrap();
            s.pauses.push((2000, 5000));
            s.pauses.push((6000, 6500));
        }
        assert_eq!(c.active(1500), Some(500));
        assert_eq!(c.active(3000), None);
        assert_eq!(c.active(5500), Some(1500)); // 4500 wall - 3000 paused
        assert_eq!(c.active(7000), Some(2500));
        assert_eq!(c.cuts(), vec![1000.0 / 1e7, 2000.0 / 1e7]);
    }
}
