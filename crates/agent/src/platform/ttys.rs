//! Some terminals draw every window from one process (Terminal and iTerm2 on macOS, gnome-terminal, Konsole and Ptyxis
//! on Linux) and don't say which of their shells a window shows. Each shell reads its own terminal device, and the
//! system moves the device's access time when it does, so the device that moves while someone types in a window is
//! that window's. A window is learned the first time someone types in it and shows none of the terminal's sessions
//! until then.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::classify::{is_terminal, Procs};
use crate::now;
use crate::platform::Foreground;

const POLL: Duration = Duration::from_secs(1);
// The processes are looked at again this often (polls), the devices' times every poll.
const RESCAN: u32 = 5;
const IDLE: Duration = Duration::from_secs(3600);
// Reads right after a window comes to the front can be the terminal telling the programs in both windows about the
// switch.
const SETTLE: f64 = 2.0;
const MAX_LEARNED: usize = 256;

/// The terminal devices a terminal's windows run, each with the topmost processes on it. A program inside a shell that
/// opens a terminal of its own (`script`, an agent's pty) stays part of that shell's session.
pub fn sessions<P: Procs>(p: &P, owner: u32) -> Vec<(u64, Vec<u32>)> {
    let mut on: HashMap<u32, Option<u64>> = HashMap::from([(owner, None)]);
    let mut out: Vec<(u64, Vec<u32>)> = Vec::new();
    for (pid, _) in p.tree().descendants(owner) {
        let inherited = p.tree().parent(pid).and_then(|parent| on.get(&parent).copied().flatten());
        let session = inherited.or_else(|| {
            let tty = p.tty(pid)?;
            match out.iter_mut().find(|(t, _)| *t == tty) {
                Some((_, tops)) => tops.push(pid),
                None => out.push((tty, vec![pid])),
            }
            Some(tty)
        });
        on.insert(pid, session);
    }
    out
}

pub fn secs(t: SystemTime) -> Option<f64> {
    t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs_f64())
}

#[derive(Default)]
pub struct Learner {
    since: f64,
    times: HashMap<u64, f64>,
    learned: HashMap<u64, (u32, u64)>,
}

impl Learner {
    /// Reads from before the change can't be told apart from the switch itself.
    pub fn front_changed(&mut self, now: f64) {
        self.since = now;
        self.times.clear();
    }

    /// One look at the terminal `owner` with `window` in front: its devices and their access times. `typed` is whether
    /// a key was pressed since the last look, true where the system can't tell keys from other input.
    pub fn poll(&mut self, owner: u32, window: u64, typed: bool, ttys: &[(u64, Option<f64>)]) {
        let moved: Vec<u64> = ttys
            .iter()
            .filter_map(|&(t, at)| {
                let at = at?;
                (at > *self.times.get(&t)? && at >= self.since + SETTLE).then_some(t)
            })
            .collect();
        self.times = ttys.iter().filter_map(|&(t, at)| Some((t, at?))).collect();
        self.learned.retain(|_, (o, t)| *o != owner || ttys.iter().any(|(d, _)| d == t));
        if moved.len() > 1 {
            // A tab switch inside the window tells the programs in both tabs.
            self.learned.remove(&window);
            return;
        }
        let [tty] = moved[..] else { return };
        let elsewhere = self.learned.iter().any(|(w, (_, t))| *w != window && *t == tty);
        if !typed || window == 0 || elsewhere {
            return;
        }
        if self.learned.len() >= MAX_LEARNED {
            self.learned.clear();
        }
        self.learned.insert(window, (owner, tty));
    }

    pub fn shown(&self, owner: u32, window: u64) -> Option<u64> {
        self.learned.get(&window).filter(|(o, _)| *o == owner).map(|&(_, t)| t)
    }
}

pub struct Hooks<P> {
    pub scan: fn() -> P,
    pub atime: fn(u64) -> Option<f64>,
    /// Whether a key was pressed in the last this many seconds.
    pub typed: fn(f64) -> bool,
}

/// Sits between the focus backend and the tracker: reports pass through, and while a terminal is in front the
/// sessions of the window are filled in and sent again when they change.
pub fn spawn<P: Procs + 'static>(out: Sender<Option<Foreground>>, hooks: Hooks<P>) -> Sender<Option<Foreground>> {
    let (tx, rx) = channel();
    thread::spawn(move || run(&rx, &out, &hooks));
    tx
}

fn run<P: Procs>(rx: &Receiver<Option<Foreground>>, out: &Sender<Option<Foreground>>, hooks: &Hooks<P>) {
    let mut learner = Learner::default();
    let mut current: Option<Foreground> = None;
    let mut sent: Option<Foreground> = None;
    // Whether the app in front is a terminal, and its sessions when it is.
    let mut seen: Option<(bool, Vec<(u64, Vec<u32>)>)> = None;
    let mut polls = 0;
    let mut last = now();
    loop {
        let terminal = seen.as_ref().is_some_and(|(t, _)| *t);
        match rx.recv_timeout(if terminal { POLL } else { IDLE }) {
            Ok(fg) => {
                current = fg;
                while let Ok(newer) = rx.try_recv() {
                    current = newer;
                }
                learner.front_changed(now());
                seen = None;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let fg = current.clone().map(|fg| {
            polls += 1;
            if seen.is_none() || polls % RESCAN == 0 {
                let p = (hooks.scan)();
                let terminal = is_terminal(&p.name(fg.pid), &fg.wclass);
                seen = Some((terminal, if terminal { sessions(&p, fg.pid) } else { Vec::new() }));
            }
            let Some((true, sessions)) = &seen else { return fg };
            let ttys: Vec<(u64, Option<f64>)> = sessions.iter().map(|(t, _)| (*t, (hooks.atime)(*t))).collect();
            let at = now();
            learner.poll(fg.pid, fg.window, (hooks.typed)(at - last), &ttys);
            last = at;
            if sessions.len() < 2 {
                return fg;
            }
            let shown = learner.shown(fg.pid, fg.window).and_then(|t| sessions.iter().find(|(d, _)| *d == t));
            Foreground { sessions: shown.map_or_else(|| vec![fg.pid], |(_, tops)| tops.clone()), ..fg }
        });
        if fg != sent {
            if out.send(fg.clone()).is_err() {
                return;
            }
            sent = fg;
        }
    }
}

/// Shared by the macOS and Linux tests, on a real terminal device.
#[cfg(test)]
pub fn check_a_real_terminal(atime: fn(u64) -> Option<f64>, tty: fn(u32) -> Option<u64>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static STEP: AtomicUsize = AtomicUsize::new(0);
    let (done, finished) = channel();
    thread::spawn(move || {
        terminal_steps(atime, tty, &|n| STEP.store(n, Ordering::Relaxed));
        let _ = done.send(());
    });
    match finished.recv_timeout(Duration::from_secs(20)) {
        Ok(()) => {}
        Err(RecvTimeoutError::Timeout) => panic!("stuck after step {}", STEP.load(Ordering::Relaxed)),
        Err(RecvTimeoutError::Disconnected) => panic!("failed after step {}", STEP.load(Ordering::Relaxed)),
    }
}

#[cfg(test)]
fn terminal_steps(atime: fn(u64) -> Option<f64>, tty: fn(u32) -> Option<u64>, step: &dyn Fn(usize)) {
    use std::ffi::CStr;
    use std::fs::{File, FileTimes, OpenOptions};
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    // _IO('t', 97), which the libc crate leaves out for macOS.
    #[cfg(target_os = "macos")]
    const TIOCSCTTY: libc::c_ulong = 0x2000_7461;
    #[cfg(not(target_os = "macos"))]
    const TIOCSCTTY: libc::Ioctl = libc::TIOCSCTTY;

    // SAFETY: plain libc calls on a descriptor this test owns; ptsname's buffer is copied before anything else runs.
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(master >= 0, "no pty");
    step(1);
    assert_eq!(unsafe { libc::grantpt(master) }, 0);
    assert_eq!(unsafe { libc::unlockpt(master) }, 0);
    let path = unsafe { CStr::from_ptr(libc::ptsname(master)) }.to_str().unwrap().to_owned();
    step(2);
    // SAFETY: the descriptor was just opened and nothing else owns it.
    let mut master = unsafe { File::from_raw_fd(master) };
    // On macOS a process that owned the terminal can't finish exiting while the echo of what was typed sits unread.
    let mut echo = master.try_clone().unwrap();
    thread::spawn(move || while let Ok(1..) = echo.read(&mut [0u8; 64]) {});
    let slave = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOCTTY).open(&path).unwrap();
    step(3);
    let dev = slave.metadata().unwrap().rdev();
    let long_ago = UNIX_EPOCH + Duration::from_secs(1_000_000);
    slave.set_times(FileTimes::new().set_accessed(long_ago)).unwrap();
    step(4);
    let before = atime(dev);
    step(5);
    assert!(before.is_some_and(|a| a < 1_000_001.0), "{path} reads back the time set: {before:?}");

    master.write_all(b"x\n").unwrap();
    step(6);
    let mut got = [0u8; 2];
    (&slave).read_exact(&mut got).unwrap();
    step(7);
    let after = atime(dev).unwrap();
    step(8);
    println!("{path}: access time {after:.3} after a read, now {:.3}", now());
    assert!(after > 1_000_001.0 && (now() - after).abs() < 10.0, "a read moves the access time: {after}");

    // A process whose terminal this is (it never reads, so job control can't stop it).
    let mut on_it = Command::new("sleep");
    on_it.arg("30").stdin(slave.try_clone().unwrap()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        on_it.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = on_it.spawn().unwrap();
    step(9);
    let seen = tty(child.id());
    step(10);
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(seen, Some(dev), "the process runs on {path}");
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::classify::Tree;

    const OWNER: u32 = 10;
    const W1: u64 = 101;
    const W2: u64 = 102;
    const T1: u64 = 7001;
    const T2: u64 = 7002;

    struct Fake {
        tree: Tree,
        ttys: HashMap<u32, u64>,
    }

    impl Procs for Fake {
        const TITLE_HINTS: bool = false;
        fn tree(&self) -> &Tree {
            &self.tree
        }
        fn name(&self, pid: u32) -> String {
            if pid == OWNER { "gnome-terminal-server".into() } else { String::new() }
        }
        fn exe(&self, _pid: u32) -> Option<PathBuf> {
            None
        }
        fn cwd(&self, _pid: u32) -> Option<PathBuf> {
            None
        }
        fn cmdline(&self, _pid: u32) -> Vec<String> {
            Vec::new()
        }
        fn tty(&self, pid: u32) -> Option<u64> {
            self.ttys.get(&pid).copied()
        }
    }

    #[test]
    fn sessions_group_by_the_topmost_process_on_each_device() {
        let mut tree = Tree::default();
        // Terminal -> login (root's, its device unreadable) -> zsh -> lab -> claude on the lab's own pty.
        for (pid, parent) in [(20, OWNER), (21, 20), (22, 21), (23, 22), (30, OWNER), (31, 30), (40, OWNER)] {
            tree.insert(pid, parent);
        }
        let ttys = HashMap::from([(21, T1), (22, T1), (23, 9999), (31, T2), (30, T2)]);
        let got = sessions(&Fake { tree, ttys }, OWNER);
        assert_eq!(got, vec![(T2, vec![30]), (T1, vec![21])]);
    }

    fn poll(l: &mut Learner, window: u64, typed: bool, t1: f64, t2: f64) {
        l.poll(OWNER, window, typed, &[(T1, Some(t1)), (T2, Some(t2))]);
    }

    #[test]
    fn typing_in_a_window_teaches_which_session_it_shows() {
        let mut l = Learner::default();
        l.front_changed(100.0);
        poll(&mut l, W1, false, 50.0, 60.0);
        assert_eq!(l.shown(OWNER, W1), None, "nothing known before anyone types");
        poll(&mut l, W1, true, 105.0, 60.0);
        assert_eq!(l.shown(OWNER, W1), Some(T1));

        l.front_changed(110.0);
        poll(&mut l, W2, true, 105.0, 60.0);
        poll(&mut l, W2, true, 111.0, 111.0);
        assert_eq!(l.shown(OWNER, W2), None, "the switch itself moved both");
        poll(&mut l, W2, true, 111.0, 118.0);
        assert_eq!(l.shown(OWNER, W2), Some(T2));

        l.front_changed(120.0);
        poll(&mut l, W1, false, 111.0, 118.0);
        assert_eq!(l.shown(OWNER, W1), Some(T1), "coming back needs no typing");
    }

    #[test]
    fn reads_that_can_be_the_switch_teach_nothing() {
        let mut l = Learner::default();
        l.front_changed(100.0);
        poll(&mut l, W2, true, 50.0, 60.0);
        poll(&mut l, W2, true, 101.0, 60.0);
        assert_eq!(l.shown(OWNER, W2), None, "the window left behind read right after the switch");
        poll(&mut l, W2, false, 130.0, 60.0);
        assert_eq!(l.shown(OWNER, W2), None, "a read with no key pressed, like a scroll over a window behind");
    }

    #[test]
    fn a_session_known_for_one_window_is_never_taken_for_another() {
        let mut l = Learner::default();
        l.front_changed(100.0);
        poll(&mut l, W1, true, 50.0, 60.0);
        poll(&mut l, W1, true, 105.0, 60.0);
        l.front_changed(110.0);
        poll(&mut l, W2, true, 105.0, 60.0);
        poll(&mut l, W2, true, 120.0, 60.0);
        assert_eq!(l.shown(OWNER, W2), None);
        assert_eq!(l.shown(OWNER, W1), Some(T1));
    }

    #[test]
    fn a_tab_switch_inside_one_window_moves_to_the_tab_typed_in() {
        let mut l = Learner::default();
        l.front_changed(100.0);
        poll(&mut l, W1, true, 50.0, 60.0);
        poll(&mut l, W1, true, 105.0, 60.0);
        poll(&mut l, W1, true, 105.0, 109.0);
        assert_eq!(l.shown(OWNER, W1), Some(T2));
    }

    #[test]
    fn a_tab_switch_that_both_tabs_hear_forgets_the_window_until_typing() {
        let mut l = Learner::default();
        l.front_changed(100.0);
        poll(&mut l, W1, true, 50.0, 60.0);
        poll(&mut l, W1, true, 105.0, 60.0);
        poll(&mut l, W1, false, 120.0, 120.0);
        assert_eq!(l.shown(OWNER, W1), None);
        poll(&mut l, W1, true, 120.0, 125.0);
        assert_eq!(l.shown(OWNER, W1), Some(T2));
    }

    #[test]
    fn a_closed_session_or_an_unknown_window_shows_nothing() {
        let mut l = Learner::default();
        l.front_changed(100.0);
        poll(&mut l, W1, true, 50.0, 60.0);
        poll(&mut l, W1, true, 105.0, 60.0);
        l.poll(OWNER, W1, true, &[(T2, Some(60.0))]);
        assert_eq!(l.shown(OWNER, W1), None);

        let mut l = Learner::default();
        l.front_changed(100.0);
        poll(&mut l, 0, true, 50.0, 60.0);
        poll(&mut l, 0, true, 105.0, 60.0);
        assert_eq!(l.shown(OWNER, 0), None, "a backend that can't name the window");
    }

    #[test]
    fn another_terminal_keeps_its_own_windows() {
        let mut l = Learner::default();
        l.front_changed(100.0);
        poll(&mut l, W1, true, 50.0, 60.0);
        poll(&mut l, W1, true, 105.0, 60.0);
        l.front_changed(110.0);
        l.poll(99, W2, true, &[(8001, Some(1.0)), (8002, Some(1.0))]);
        assert_eq!(l.shown(OWNER, W1), Some(T1));
        assert_eq!(l.shown(99, W1), None);
    }

    static TIMES: std::sync::Mutex<[f64; 2]> = std::sync::Mutex::new([50.0, 60.0]);

    fn two_windows() -> Fake {
        let mut tree = Tree::default();
        tree.insert(20, OWNER);
        tree.insert(30, OWNER);
        Fake { tree, ttys: HashMap::from([(20, T1), (30, T2)]) }
    }

    fn time_of(tty: u64) -> Option<f64> {
        let t = TIMES.lock().unwrap();
        Some(if tty == T1 { t[0] } else { t[1] })
    }

    #[test]
    fn the_filter_fills_in_the_window_once_someone_types_in_it() {
        let (out, reports) = channel();
        let tx = spawn(out, Hooks { scan: two_windows, atime: time_of, typed: |_| true });
        let fg = Foreground { pid: OWNER, wclass: String::new(), title: String::new(), sessions: Vec::new(), window: W1 };
        tx.send(Some(fg.clone())).unwrap();
        let first = reports.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(first.sessions, vec![OWNER], "two sessions, none known: the window shows none");
        TIMES.lock().unwrap()[0] = now() + 3.0;
        let learned = reports.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(learned.sessions, vec![20]);

        let other = Foreground { pid: 77, ..fg };
        tx.send(Some(other.clone())).unwrap();
        assert_eq!(reports.recv_timeout(Duration::from_secs(5)).unwrap(), Some(other), "other apps pass through");
    }
}
