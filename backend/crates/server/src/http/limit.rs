//! Fixed-window hit counters kept in memory and bounded: expired windows are pruned as the map
//! fills, and once it holds `cap` live windows new keys are not tracked (they pass), so a flood
//! of distinct keys (IPs, user names) cannot grow it without limit.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

pub struct Windows {
    map: Mutex<HashMap<String, (u64, Instant)>>,
    cap: usize,
}

impl Windows {
    pub fn new(cap: usize) -> Self {
        Self { map: Mutex::new(HashMap::new()), cap }
    }

    /// Count a hit for `key` in its current window: the hits so far (including this one) and
    /// the time until the window resets. An untracked key reports zero hits.
    pub fn hit(&self, key: &str, window: Duration) -> (u64, Duration) {
        let now = Instant::now();
        let mut m = self.map.lock();
        if let Some((count, reset)) = m.get_mut(key)
            && now <= *reset
        {
            *count += 1;
            return (*count, *reset - now);
        }
        if m.len() >= self.cap {
            m.retain(|_, (_, reset)| now <= *reset);
            if m.len() >= self.cap {
                return (0, window);
            }
        }
        m.insert(key.to_owned(), (1, now + window));
        (1, window)
    }

    pub fn reset(&self, key: &str) {
        self.map.lock().remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_per_window_and_stays_bounded() {
        let w = Windows::new(3);
        let minute = Duration::from_secs(60);
        assert_eq!(w.hit("a", minute).0, 1);
        assert_eq!(w.hit("a", minute).0, 2);
        w.hit("b", minute);
        w.hit("c", minute);
        assert_eq!(w.hit("d", minute).0, 0, "a full map stops tracking new keys");
        assert_eq!(w.map.lock().len(), 3);
        assert_eq!(w.hit("a", minute).0, 3, "tracked keys keep counting");
        let short = Windows::new(1);
        short.hit("x", Duration::ZERO);
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(short.hit("y", minute).0, 1, "expired windows make room");
        w.reset("a");
        assert_eq!(w.hit("a", minute).0, 1);
    }
}
