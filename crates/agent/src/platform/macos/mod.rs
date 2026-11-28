// Focus polls CGWindowList: NSWorkspace.frontmostApplication goes stale without NSApplication and lags even with it.
mod front;
mod procs;

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use kairos_core::exclude::Exclusions;
use objc2_core_graphics::{CGEventSource, CGEventSourceStateID, CGEventType};

use crate::classify::{classify_with, Classified};
use crate::diag::diag;
use crate::platform::{ttys, Backends, Foreground, InputWatch, Sinks};
use crate::{now, Msg};

const POLL: Duration = Duration::from_secs(1);
const IDLE_SECS: f64 = 60.0;
// kCGAnyInputEventType is the C macro `(CGEventType)(~0)`, which the bindings don't carry.
const ANY_INPUT: CGEventType = CGEventType(u32::MAX);

pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    procs::cwd(pid)
}

pub fn classify(fg: &Foreground, ex: &Exclusions) -> Classified {
    let mut snap = procs::LibProc::scan();
    if !fg.sessions.is_empty() {
        snap.show(fg.pid, &fg.sessions);
    }
    classify_with(&snap, fg.pid, &fg.wclass, &fg.title, ex)
}

pub fn spawn(sinks: Sinks) -> Backends {
    let hooks = ttys::Hooks { scan: procs::LibProc::scan, atime: procs::tty_atime, typed: typed_within };
    let foreground = ttys::spawn(sinks.foreground, hooks);
    thread::spawn(move || focus(foreground));
    let idle = if spawn_idle(sinks.events) { "cgeventsource" } else { "none" };
    Backends { focus: "cgwindowlist", idle }
}

fn foreground((pid, window): (u32, u64)) -> Foreground {
    let wclass = procs::exe(pid).and_then(|e| front::bundle_id(&e)).unwrap_or_default();
    Foreground { pid, wclass, title: String::new(), sessions: Vec::new(), window }
}

fn focus(out: Sender<Option<Foreground>>) {
    let mut seen = None;
    loop {
        let front = front::front_window();
        if front != seen {
            seen = front;
            if out.send(front.map(foreground)).is_err() {
                return;
            }
        }
        thread::sleep(POLL);
    }
}

fn typed_within(secs: f64) -> bool {
    since(CGEventType::KeyDown).is_some_and(|key| key <= secs + 0.2)
}

fn idle_secs() -> Option<f64> {
    since(ANY_INPUT)
}

fn since(kind: CGEventType) -> Option<f64> {
    let secs = CGEventSource::seconds_since_last_event_type(CGEventSourceStateID::HIDSystemState, kind);
    (secs.is_finite() && secs >= 0.0).then_some(secs)
}

fn spawn_idle(tx: Sender<Msg>) -> bool {
    if idle_secs().is_none() {
        diag!("kairosd: warning: presence unknown, emitting no present events (CGEventSource gave no idle time)");
        return false;
    }
    thread::spawn(move || {
        let _ = tx.send(Msg::PresenceReady);
        let _ = tx.send(Msg::InputReady);
        let _ = tx.send(Msg::InputKinds { keys: true, injected: false });
        let mut idle = false;
        let (mut input, mut keys) = (InputWatch::default(), InputWatch::default());
        loop {
            thread::sleep(InputWatch::poll_every(idle));
            let Some(secs) = idle_secs() else { continue };
            let at = now();
            if input.observe(at, secs).is_some_and(|t| tx.send(Msg::Input(t)).is_err()) {
                return;
            }
            let key = since(CGEventType::KeyDown).and_then(|k| keys.observe(at, k));
            if key.is_some_and(|t| tx.send(Msg::Keys(t)).is_err()) {
                return;
            }
            let now_idle = secs >= IDLE_SECS;
            if now_idle != idle {
                idle = now_idle;
                if tx.send(if idle { Msg::Idled } else { Msg::Resumed }).is_err() {
                    return;
                }
            }
        }
    });
    true
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::mpsc::{self, Receiver};
    use std::time::Instant;

    use kairos_core::event::Class;

    use super::*;

    fn classify(fg: &Foreground) -> Classified {
        super::classify(fg, &Exclusions::default())
    }

    const WAIT: Duration = Duration::from_secs(30);

    // `open` can block on an app's first launch, so every command gets a deadline.
    fn run(cmd: &str, args: &[&str]) {
        let mut child = Command::new(cmd).args(args).spawn().expect("spawn");
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().expect("wait") {
                assert!(status.success(), "{cmd} {args:?} failed");
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        panic!("{cmd} {args:?} did not finish");
    }

    fn pid_of(name: &str) -> u32 {
        let out = Command::new("pgrep").args(["-nx", name]).output().expect("pgrep");
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or_else(|_| panic!("{name} is not running"))
    }

    fn front_app(rx: &Receiver<Option<Foreground>>, bundle: &str) -> Foreground {
        let deadline = Instant::now() + WAIT;
        let mut seen = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(Some(fg)) if fg.wclass == bundle => {
                    println!("front: {bundle} pid {}", fg.pid);
                    return fg;
                }
                Ok(other) => seen.push(other),
                Err(_) => break,
            }
        }
        panic!("{bundle} never came to the front, reports were {seen:?}");
    }

    fn classify_until(fg: &Foreground, want: Class) -> Classified {
        let deadline = Instant::now() + WAIT;
        loop {
            let c = classify(fg);
            if c.class == want || Instant::now() > deadline {
                println!("classified {}: {c:?}", fg.wclass);
                return c;
            }
            thread::sleep(POLL);
        }
    }

    #[test]
    fn reads_own_process() {
        use crate::classify::Procs;

        let pid = std::process::id();
        let snap = procs::LibProc::scan();
        assert_eq!(snap.tree().parent(pid), Some(std::os::unix::process::parent_id()));
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert_eq!(process_cwd(pid).unwrap().canonicalize().unwrap(), cwd);
        let exe = std::env::current_exe().unwrap();
        assert_eq!(snap.exe(pid).unwrap().canonicalize().unwrap(), exe.canonicalize().unwrap());
        let args: Vec<String> = std::env::args().collect();
        assert_eq!(snap.cmdline(pid), args);
        assert!(!snap.name(pid).is_empty());
        assert_eq!(snap.name(1), "launchd");
        assert_eq!(snap.comm(1), "launchd");
    }

    // Needs a logged-in GUI session; .github/workflows/macos-check.yml runs it on a real Mac.
    #[test]
    #[ignore = "launches apps, needs a macOS GUI session"]
    fn macos_backends_on_real_apps() {
        let secs = idle_secs().expect("idle time is readable");
        println!("idle: {secs:.1} s since last input");

        let tmp = std::env::temp_dir().join(format!("kairos-macos-{}", std::process::id()));
        let repo = tmp.join(format!("kairos-mac-proj-{}", std::process::id()));
        std::fs::create_dir_all(repo.join(".git")).expect("temp repo");
        let note = tmp.join("note.txt");
        std::fs::write(&note, "kairos").expect("temp note");

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || focus(tx));

        run("open", &["-a", "TextEdit", note.to_str().unwrap()]);
        let fg = front_app(&rx, "com.apple.TextEdit");
        assert_eq!(fg.pid, pid_of("TextEdit"));

        run("open", &["-a", "Terminal", repo.to_str().unwrap()]);
        let fg = front_app(&rx, "com.apple.Terminal");
        assert_eq!(fg.pid, pid_of("Terminal"));
        let c = classify_until(&fg, Class::Terminal);
        assert_eq!(c.class, Class::Terminal);
        let project = c.project.expect("terminal project");
        assert_eq!(project.id, repo.file_name().unwrap().to_string_lossy());
        assert_eq!(project.root.canonicalize().unwrap(), repo.canonicalize().unwrap());

        // A Finder window keeps Finder in the window list once it is activated again.
        run("open", &[tmp.to_str().unwrap()]);
        run("open", &["-a", "TextEdit"]);
        front_app(&rx, "com.apple.TextEdit");
        run("osascript", &["-e", "tell application \"Finder\" to activate"]);
        let fg = front_app(&rx, "com.apple.finder");
        assert_eq!(fg.pid, pid_of("Finder"));
        assert_eq!(classify(&fg).class, Class::Other);

        run("open", &["-a", "Safari"]);
        let fg = front_app(&rx, "com.apple.Safari");
        let c = classify_until(&fg, Class::BrowserUnknown);
        assert_eq!((c.class, c.app.as_str()), (Class::BrowserUnknown, "safari"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
