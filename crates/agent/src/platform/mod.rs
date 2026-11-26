//! Backends only report facts. Classification happens once, in `focus::Tracker`.

use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::Msg;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(windows, test))]
mod tabs;
#[cfg(unix)]
mod ttys;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
#[allow(unused_imports)]
pub use linux::{classify, process_cwd, spawn};
#[cfg(target_os = "macos")]
#[allow(unused_imports)]
pub use macos::{classify, process_cwd, spawn};
#[cfg(windows)]
#[allow(unused_imports)]
pub use windows::{classify, process_cwd, spawn};

/// Turns idle-time readings into the instant of the newest input, once for each reading that shows new input.
#[derive(Default)]
pub struct InputWatch {
    last: Option<f64>,
}

impl InputWatch {
    // Two readings of the same input differ by the jitter between the clocks they are taken against.
    const MARGIN: f64 = 0.05;

    /// How long to wait before the next idle-time reading. A reading only shows the newest input, so while someone is at
    /// the keyboard they come well inside every second, or a second whose input was overtaken by the next one goes unseen.
    pub fn poll_every(idle: bool) -> Duration {
        Duration::from_millis(if idle { 1000 } else { 200 })
    }

    pub fn observe(&mut self, now: f64, idle_secs: f64) -> Option<f64> {
        let at = now - idle_secs;
        self.last.is_none_or(|last| at > last + Self::MARGIN).then(|| {
            self.last = Some(at);
            at
        })
    }
}

/// `title` is only held in memory for classification and dropped right after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Foreground {
    pub pid: u32,
    pub wclass: String,
    pub title: String,
    /// For terminals that run every window and tab in one process (Windows Terminal): the sessions this window shows,
    /// all of the window's when the one in use is unknown. Empty when everything the process runs is the window's. On
    /// macOS and Linux the terminal's own pid alone means none of its sessions is known to be this window's.
    pub sessions: Vec<u32>,
    /// The window in front, 0 when the backend can't say. Only compared, never shown.
    pub window: u64,
}

#[derive(Clone)]
pub struct Sinks {
    pub foreground: Sender<Option<Foreground>>,
    pub events: Sender<Msg>,
}

/// Names of the backends that are running, recorded in the `backends` event. `none` means that signal is unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backends {
    pub focus: &'static str,
    pub idle: &'static str,
}

impl Backends {
    pub const NONE: Backends = Backends { focus: "none", idle: "none" };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_an_input_once_and_the_next_one_when_it_comes() {
        let mut w = InputWatch::default();
        assert_eq!(w.observe(100.0, 0.4), Some(99.6));
        assert_eq!(w.observe(101.0, 1.4), None);
        assert_eq!(w.observe(102.0005, 2.4), None);
        assert_eq!(w.observe(103.0, 3.4), None);
        assert_eq!(w.observe(104.0, 0.2), Some(103.8));
        assert_eq!(w.observe(105.0, 0.9), Some(104.1));
        assert_eq!(w.observe(106.0, 1.9), None);
    }

    // Seconds that had a keystroke but no reported input, typing 4 to 6 keys a second for ten minutes while the readings
    // come `every` seconds apart.
    fn seconds_missed(every: f64) -> usize {
        let mut seed = 7u64;
        let mut rand = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut keys = vec![1000.0];
        while *keys.last().unwrap() < 1600.0 {
            let gap = 0.2 + rand() * 0.17;
            keys.push(keys.last().unwrap() + gap);
        }
        let mut watch = InputWatch::default();
        let mut seen = std::collections::HashSet::new();
        let mut at = 1000.0 + rand();
        while at < 1600.0 {
            if let Some(last) = keys.iter().rev().find(|&&k| k <= at) {
                if let Some(t) = watch.observe(at, at - last) {
                    seen.insert(t.floor() as i64);
                }
            }
            at += every;
        }
        let typed: std::collections::HashSet<i64> = keys.iter().filter(|&&k| k < at).map(|k| k.floor() as i64).collect();
        typed.difference(&seen).count()
    }

    #[test]
    fn every_second_with_input_is_seen_even_when_readings_drift() {
        let slow = seconds_missed(1.02);
        let fast = InputWatch::poll_every(false).as_secs_f64();
        println!("seconds missed in ten minutes of typing: {slow} reading every 1.02 s, {} every {:.3} s", seconds_missed(fast * 1.02), fast * 1.02);
        assert!(slow > 20, "readings a second apart miss seconds, which is the bug this guards");
        assert_eq!(seconds_missed(fast * 1.02), 0);
        assert_eq!(seconds_missed(fast * 1.5), 0);
        assert_eq!(InputWatch::poll_every(true), Duration::from_secs(1));
    }

    #[test]
    fn an_input_found_on_the_first_reading_is_reported_with_its_own_time() {
        assert_eq!(InputWatch::default().observe(100.0, 30.0), Some(70.0));
    }
}
