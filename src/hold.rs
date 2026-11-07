use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use evdev::KeyCode;

pub const HOLD_TIMEOUT: Duration = Duration::from_millis(300);

#[derive(Default)]
pub struct HoldDetector {
    presses: BTreeMap<(usize, u16), Instant>,
}

impl HoldDetector {
    pub fn update(&mut self, device: usize, code: u16, value: i32, now: Instant) {
        if matches!(
            KeyCode::new(code),
            KeyCode::KEY_LEFTSHIFT | KeyCode::KEY_RIGHTSHIFT
        ) {
            return;
        }
        if value == 0 {
            self.presses.remove(&(device, code));
        } else if matches!(value, 1 | 2) {
            self.presses.entry((device, code)).or_insert(now);
        }
    }

    pub fn snapshot(&mut self, device: usize, held: &BTreeSet<u16>, now: Instant) {
        self.presses
            .retain(|&(index, code), _| index != device || held.contains(&code));
        for &code in held {
            self.update(device, code, 1, now);
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.presses
            .values()
            .map(|&since| since + HOLD_TIMEOUT + Duration::from_nanos(1))
            .min()
    }

    pub fn expire(&mut self, now: Instant) -> bool {
        let count = self.presses.len();
        self.presses
            .retain(|_, since| now.saturating_duration_since(*since) <= HOLD_TIMEOUT);
        self.presses.len() != count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeats_and_snapshots_preserve_deadlines_and_devices_are_independent() {
        let mut detector = HoldDetector::default();
        let now = Instant::now();
        let code = KeyCode::KEY_A.0;
        detector.update(0, code, 1, now);
        detector.update(1, code, 1, now + HOLD_TIMEOUT);
        let deadline = detector.deadline().unwrap();
        detector.update(0, code, 2, now + HOLD_TIMEOUT / 2);
        detector.snapshot(0, &BTreeSet::from([code]), now + HOLD_TIMEOUT / 2);
        assert_eq!(detector.deadline(), Some(deadline));
        assert!(!detector.expire(now + HOLD_TIMEOUT));
        assert!(detector.expire(deadline));
        assert!(!detector.expire(deadline));
        assert!(detector.deadline().unwrap() > deadline);
        detector.snapshot(1, &BTreeSet::new(), deadline);
        assert!(detector.deadline().is_none());
    }

    #[test]
    fn shift_and_cancelled_timers_do_not_wake_the_reader() {
        let mut detector = HoldDetector::default();
        let now = Instant::now();
        detector.snapshot(
            0,
            &BTreeSet::from([KeyCode::KEY_LEFTSHIFT.0, KeyCode::KEY_A.0]),
            now,
        );
        detector.update(0, KeyCode::KEY_A.0, 0, now);
        assert!(detector.deadline().is_none());
        assert!(!detector.expire(now + HOLD_TIMEOUT * 2));
    }
}
