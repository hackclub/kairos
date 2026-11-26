use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use crate::classify::{normalize, titles_matter};
use crate::platform::Foreground;

pub struct Running {
    pub name: &'static str,
    alive: Arc<AtomicBool>,
}

impl Running {
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

pub struct Lifeline(Arc<AtomicBool>);

impl Drop for Lifeline {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

pub fn running(name: &'static str) -> (Running, Lifeline) {
    let alive = Arc::new(AtomicBool::new(true));
    (Running { name, alive: alive.clone() }, Lifeline(alive))
}

/// Filters what a focus backend observes so a chatty terminal title doesn't trigger a process-table scan per update.
pub struct Reporter {
    tx: Sender<Option<Foreground>>,
    last: Option<Foreground>,
}

impl Reporter {
    pub fn new(tx: Sender<Option<Foreground>>) -> Reporter {
        Reporter { tx, last: None }
    }

    pub fn report(&mut self, fg: Option<Foreground>) -> bool {
        let fg = fg.filter(|f| f.pid > 0);
        if fg == self.last || self.title_only_change(fg.as_ref()) {
            return true;
        }
        self.last.clone_from(&fg);
        self.tx.send(fg).is_ok()
    }

    // Titles only influence classification for browsers, editors and agent apps (see `titles_matter`).
    fn title_only_change(&mut self, fg: Option<&Foreground>) -> bool {
        let (Some(prev), Some(new)) = (self.last.as_mut(), fg) else { return false };
        if prev.pid != new.pid || prev.wclass != new.wclass || prev.window != new.window || titles_matter(&exe_name(new.pid), &new.wclass.to_lowercase()) {
            return false;
        }
        prev.title.clone_from(&new.title);
        true
    }
}

fn exe_name(pid: u32) -> String {
    fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|p| p.file_name().map(|n| normalize(n.to_string_lossy().trim_end_matches(" (deleted)"))))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn fg(pid: u32, class: &str, title: &str) -> Option<Foreground> {
        Some(Foreground { pid, wclass: class.into(), title: title.into(), sessions: Vec::new(), window: 0 })
    }

    #[test]
    fn lifeline_drop_marks_the_backend_dead() {
        let (run, life) = running("test");
        assert!(run.is_alive());
        drop(life);
        assert!(!run.is_alive());
    }

    #[test]
    fn reports_changes_once_and_drops_pidless_windows() {
        let (tx, rx) = mpsc::channel();
        let mut r = Reporter::new(tx);
        assert!(r.report(fg(7, "foot", "a")));
        assert!(r.report(fg(7, "foot", "a")));
        assert!(r.report(fg(0, "mystery", "")));
        assert!(r.report(None));
        assert!(r.report(fg(8, "foot", "b")));
        let got: Vec<_> = rx.try_iter().collect();
        assert_eq!(got, vec![fg(7, "foot", "a"), None, fg(8, "foot", "b")]);
    }

    #[test]
    fn title_changes_are_ignored_outside_browsers() {
        let (tx, rx) = mpsc::channel();
        let mut r = Reporter::new(tx);
        assert!(r.report(fg(7, "foot", "a")));
        assert!(r.report(fg(7, "foot", "b")));
        assert_eq!(rx.try_iter().count(), 1);
    }

    #[test]
    fn another_window_of_the_same_terminal_is_reported() {
        let (tx, rx) = mpsc::channel();
        let mut r = Reporter::new(tx);
        let in_window = |w: u64, title: &str| fg(7, "gnome-terminal-server", title).map(|f| Foreground { window: w, ..f });
        assert!(r.report(in_window(1, "a")));
        assert!(r.report(in_window(1, "b")));
        assert!(r.report(in_window(2, "b")));
        assert_eq!(rx.try_iter().count(), 2);
    }

    #[test]
    fn title_changes_are_kept_for_editors() {
        let (tx, rx) = mpsc::channel();
        let mut r = Reporter::new(tx);
        assert!(r.report(fg(7, "code", "a.rs - one - Visual Studio Code")));
        assert!(r.report(fg(7, "code", "a.rs - two - Visual Studio Code")));
        assert_eq!(rx.try_iter().count(), 2);
    }

    #[test]
    fn title_changes_are_kept_for_browsers() {
        let (tx, rx) = mpsc::channel();
        let mut r = Reporter::new(tx);
        assert!(r.report(fg(7, "firefox", "a")));
        assert!(r.report(fg(7, "firefox", "b")));
        assert_eq!(rx.try_iter().count(), 2);
    }

    #[test]
    fn reports_false_when_the_receiver_is_gone() {
        let (tx, rx) = mpsc::channel();
        drop(rx);
        assert!(!Reporter::new(tx).report(fg(7, "foot", "a")));
    }
}
