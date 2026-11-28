//! Ignored by default: they need real desktop components, which `tests/linux-desktops/` starts in containers.

use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zbus::blocking::connection::Builder;
use zbus::blocking::Connection;
use zbus::interface;

use super::backend::Running;
use super::session::{self, SessionEnv};
use super::{dbus_idle, gnome, kwin, sway, wayland_idle, x11};
use crate::platform::{Backends, Foreground, Sinks};
use crate::Msg;

const WAIT: Duration = Duration::from_secs(10);

struct Client(Child);

impl Client {
    fn spawn(program: &str, args: &[&str]) -> Client {
        Client(Command::new(program).args(args).stdin(Stdio::null()).spawn().unwrap_or_else(|e| panic!("cannot start {program}: {e}")))
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn tool(program: &str, args: &[&str]) -> String {
    let out = Command::new(program).args(args).stdin(Stdio::null()).output().unwrap_or_else(|e| panic!("cannot run {program}: {e}"));
    assert!(out.status.success(), "{program} {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn session_env() -> SessionEnv {
    session::discover().expect("a graphical session")
}

fn bus_env() -> SessionEnv {
    let address = std::env::var("DBUS_SESSION_BUS_ADDRESS").expect("run under dbus-run-session");
    SessionEnv::from_pairs(&[("DBUS_SESSION_BUS_ADDRESS", &address)])
}

fn foreground_with_pid(rx: &Receiver<Option<Foreground>>, pid: u32) -> Foreground {
    let deadline = Instant::now() + WAIT;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if let Ok(Some(fg)) = rx.recv_timeout(left) {
            if fg.pid == pid {
                return fg;
            }
        }
    }
    panic!("no focus report for pid {pid}");
}

fn expect_msg(rx: &Receiver<Msg>, want: fn(&Msg) -> bool, what: &str) {
    let deadline = Instant::now() + WAIT;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if rx.recv_timeout(left).is_ok_and(|m| want(&m)) {
            return;
        }
    }
    panic!("no {what} message");
}

fn is_ready(m: &Msg) -> bool {
    matches!(m, Msg::PresenceReady)
}

fn is_idled(m: &Msg) -> bool {
    matches!(m, Msg::Idled)
}

fn is_resumed(m: &Msg) -> bool {
    matches!(m, Msg::Resumed)
}

fn wait_dead(run: &Running) {
    let deadline = Instant::now() + WAIT;
    while run.is_alive() {
        assert!(Instant::now() < deadline, "{} did not stop", run.name);
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn xterm(title: &str) -> Client {
    Client::spawn("xterm", &["-T", title, "-e", "sleep", "600"])
}

fn activate_x11(pid: u32) {
    let windows = tool("xdotool", &["search", "--sync", "--onlyvisible", "--pid", &pid.to_string()]);
    let window = windows.lines().next().expect("a window for the pid");
    tool("xdotool", &["windowactivate", "--sync", window]);
}

#[test]
#[ignore = "needs Xvfb, see tests/linux-desktops"]
fn x11_requires_a_window_manager() {
    let (tx, _rx) = mpsc::channel();
    assert!(x11::start(&session_env(), tx).is_err());
}

#[test]
#[ignore = "needs Xvfb and a window manager, see tests/linux-desktops"]
fn x11_focus_follows_the_active_window() {
    let (tx, rx) = mpsc::channel();
    let run = x11::start(&session_env(), tx).expect("x11 backend");
    assert_eq!(run.name, "x11");
    let (a, b) = (xterm("kairos-a"), xterm("kairos-b"));
    activate_x11(a.pid());
    let fg = foreground_with_pid(&rx, a.pid());
    assert_eq!((fg.wclass.as_str(), fg.title.as_str()), ("XTerm", "kairos-a"));
    activate_x11(b.pid());
    foreground_with_pid(&rx, b.pid());
    activate_x11(a.pid());
    foreground_with_pid(&rx, a.pid());
    assert!(run.is_alive());
}

#[test]
#[ignore = "needs Xvfb, see tests/linux-desktops"]
fn x11_idle_follows_input() {
    let (tx, rx) = mpsc::channel();
    let run = x11::start_idle(&session_env(), tx, Duration::from_secs(2)).expect("x11 idle backend");
    assert_eq!(run.name, "x11-screensaver");
    expect_msg(&rx, is_ready, "PresenceReady");
    expect_msg(&rx, is_idled, "Idled");
    tool("xdotool", &["mousemove_relative", "--", "5", "5"]);
    expect_msg(&rx, is_resumed, "Resumed");
}

#[test]
#[ignore = "needs Xvfb, see tests/linux-desktops"]
fn x11_idle_tells_xtest_input_from_key_presses() {
    let (tx, rx) = mpsc::channel();
    let _run = x11::start_idle(&session_env(), tx, Duration::from_secs(60)).expect("x11 idle backend");
    expect_msg(&rx, |m| matches!(m, Msg::InputKinds { keys: true, injected: true }), "InputKinds");
    std::thread::sleep(Duration::from_secs(1));
    while rx.try_recv().is_ok() {}
    let before = crate::now() - 0.5;
    tool("xdotool", &["key", "a"]);
    tool("xdotool", &["mousemove_relative", "--", "5", "5"]);
    let (mut injected, mut keys) = (0, 0);
    let deadline = Instant::now() + Duration::from_secs(3);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(Msg::Injected(t)) if t >= before => injected += 1,
            Ok(Msg::Keys(t)) if t >= before => keys += 1,
            _ => {}
        }
    }
    assert!(injected > 0, "xdotool's input comes from the XTEST devices");
    assert_eq!(keys, 0, "and is no key press on a keyboard");
}

#[test]
#[ignore = "needs a Sway session, see tests/linux-desktops"]
fn sway_focus_follows_focused_windows() {
    let env = session_env();
    let socket = env.sway_socket().expect("a Sway socket found in the runtime dir");
    let (tx, rx) = mpsc::channel();
    let run = sway::start_sway(&env, tx).expect("sway backend");
    assert_eq!(run.name, "sway");
    let a = Client::spawn("foot", &["-a", "kairos-a", "sleep", "600"]);
    foreground_with_pid(&rx, a.pid());
    let b = Client::spawn("foot", &["-a", "kairos-b", "sleep", "600"]);
    let fg = foreground_with_pid(&rx, b.pid());
    assert_eq!(fg.wclass, "kairos-b");
    tool("swaymsg", &["-s", socket.to_str().unwrap(), "[app_id=\"kairos-a\"] focus"]);
    assert_eq!(foreground_with_pid(&rx, a.pid()).wclass, "kairos-a");
    drop(b);
    assert!(run.is_alive());
}

#[test]
#[ignore = "needs a Wayland compositor with ext-idle-notify-v1, see tests/linux-desktops"]
fn ext_idle_notify_reports_idle() {
    let (tx, rx) = mpsc::channel();
    let run = wayland_idle::start(&session_env(), tx, Duration::from_secs(2)).expect("ext-idle-notify backend");
    assert_eq!(run.name, "ext-idle-notify");
    expect_msg(&rx, is_ready, "PresenceReady");
    expect_msg(&rx, is_idled, "Idled");
}

#[test]
#[ignore = "needs a graphical session found only through other processes, see tests/linux-desktops"]
fn session_is_found_from_other_processes() {
    let env = session::discover().expect("a session from /proc");
    assert!(env.has_display());
}

#[test]
#[ignore = "needs a desktop, set KAIROS_EXPECT_BACKENDS=focus,idle, see tests/linux-desktops"]
fn supervisor_selects_the_desktops_backends() {
    let expect = std::env::var("KAIROS_EXPECT_BACKENDS").expect("KAIROS_EXPECT_BACKENDS");
    let (ftx, _frx) = mpsc::channel();
    let (etx, _erx) = mpsc::channel();
    let found = super::supervisor::spawn(Sinks { foreground: ftx, events: etx });
    assert_eq!(format!("{},{}", found.focus, found.idle), expect);
}

#[test]
#[ignore = "needs DISPLAY pointing at an X server that starts later, see tests/linux-desktops"]
fn supervisor_picks_up_a_session_that_starts_late() {
    let (ftx, _frx) = mpsc::channel();
    let (etx, erx) = mpsc::channel();
    let found = super::supervisor::spawn(Sinks { foreground: ftx, events: etx });
    assert_eq!(found, Backends::NONE);
    let deadline = Instant::now() + Duration::from_secs(90);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if let Ok(Msg::Backends(b)) = erx.recv_timeout(left) {
            assert_eq!(b, Backends { focus: "x11", idle: "x11-screensaver" });
            return;
        }
    }
    panic!("the session was never found");
}

#[test]
#[ignore = "needs a Sway session that restarts during the test, see tests/linux-desktops"]
fn supervisor_finds_the_compositor_again_after_it_restarts() {
    let (ftx, _frx) = mpsc::channel();
    let (etx, erx) = mpsc::channel();
    let found = super::supervisor::spawn(Sinks { foreground: ftx, events: etx });
    assert_eq!(found, Backends { focus: "sway", idle: "ext-idle-notify" });
    let mut lost = false;
    let deadline = Instant::now() + Duration::from_secs(120);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if let Ok(Msg::Backends(b)) = erx.recv_timeout(left) {
            if b.focus == "none" {
                lost = true;
            } else if lost {
                assert_eq!(b.focus, "sway");
                return;
            }
        }
    }
    panic!("focus never came back after Sway restarted (lost it: {lost})");
}

struct FakeKwin {
    loaded: Arc<Mutex<Option<String>>>,
}

#[interface(name = "org.kde.kwin.Scripting")]
impl FakeKwin {
    #[zbus(name = "unloadScript")]
    fn unload_script(&self, _name: &str) -> bool {
        false
    }

    #[zbus(name = "loadScript")]
    fn load_script(&self, path: &str, _name: &str) -> i32 {
        *self.loaded.lock().unwrap() = Some(path.to_owned());
        1
    }
}

struct FakeKwinScript(mpsc::Sender<()>);

#[interface(name = "org.kde.kwin.Script")]
impl FakeKwinScript {
    #[zbus(name = "run")]
    fn run(&self) {
        let _ = self.0.send(());
    }
}

#[test]
#[ignore = "needs a session bus, run under dbus-run-session, see tests/linux-desktops"]
fn dbus_kwin_service_receives_focus_and_notices_kwin_leaving() {
    let (ran_tx, ran) = mpsc::channel();
    let loaded = Arc::new(Mutex::new(None));
    let kwin_conn = Builder::session()
        .unwrap()
        .name("org.kde.KWin")
        .unwrap()
        .serve_at("/Scripting", FakeKwin { loaded: loaded.clone() })
        .unwrap()
        .serve_at("/Scripting/Script1", FakeKwinScript(ran_tx))
        .unwrap()
        .build()
        .unwrap();

    let (tx, rx) = mpsc::channel();
    let run = kwin::start(&bus_env(), tx).expect("kwin backend");
    assert_eq!(run.name, "kwin");
    ran.recv_timeout(WAIT).expect("the script was run");
    let script = std::fs::read_to_string(loaded.lock().unwrap().clone().expect("a script path")).unwrap();
    assert!(script.contains("callDBus(\"dev.obayy.Kairos\""));

    let client = Connection::session().unwrap();
    let call = |pid: &str, class: &str, caption: &str, window: &str| {
        client.call_method(Some("dev.obayy.Kairos"), "/Focus", Some("dev.obayy.Kairos.Focus"), "Activated", &(pid, class, caption, window)).unwrap();
    };
    call("4242", "kitty", "~", "{0b5e}");
    let fg = foreground_with_pid(&rx, 4242);
    assert_eq!((fg.wclass.as_str(), fg.title.as_str()), ("kitty", "~"));
    assert_ne!(fg.window, 0);
    call("0", "", "", "");
    assert_eq!(rx.recv_timeout(WAIT).unwrap(), None);

    drop(kwin_conn);
    wait_dead(&run);
}

struct FakeIdle(Arc<Mutex<u64>>);

#[interface(name = "org.gnome.Mutter.IdleMonitor")]
impl FakeIdle {
    #[zbus(name = "GetIdletime")]
    fn get_idletime(&self) -> u64 {
        *self.0.lock().unwrap()
    }
}

struct FakeScreenSaver(Arc<Mutex<u64>>);

#[interface(name = "org.freedesktop.ScreenSaver")]
impl FakeScreenSaver {
    #[zbus(name = "GetSessionIdleTime")]
    fn get_session_idle_time(&self) -> u32 {
        *self.0.lock().unwrap() as u32
    }
}

fn follows_idle_time(run: &Running, name: &str, idle_ms: &Mutex<u64>, rx: &Receiver<Msg>) {
    assert_eq!(run.name, name);
    expect_msg(rx, is_ready, "PresenceReady");
    *idle_ms.lock().unwrap() = 5_000;
    expect_msg(rx, is_idled, "Idled");
    *idle_ms.lock().unwrap() = 0;
    expect_msg(rx, is_resumed, "Resumed");
}

#[test]
#[ignore = "needs a session bus, run under dbus-run-session, see tests/linux-desktops"]
fn dbus_mutter_idle_monitor() {
    let idle_ms = Arc::new(Mutex::new(0));
    let _conn = Builder::session()
        .unwrap()
        .name("org.gnome.Mutter.IdleMonitor")
        .unwrap()
        .serve_at("/org/gnome/Mutter/IdleMonitor/Core", FakeIdle(idle_ms.clone()))
        .unwrap()
        .build()
        .unwrap();
    let (tx, rx) = mpsc::channel();
    let run = dbus_idle::start_mutter(&bus_env(), tx, Duration::from_secs(2)).expect("mutter backend");
    follows_idle_time(&run, "mutter", &idle_ms, &rx);
}

#[test]
#[ignore = "needs a session bus, run under dbus-run-session, see tests/linux-desktops"]
fn dbus_screensaver_idle_time() {
    let idle_ms = Arc::new(Mutex::new(0));
    let _conn = Builder::session()
        .unwrap()
        .name("org.freedesktop.ScreenSaver")
        .unwrap()
        .serve_at("/ScreenSaver", FakeScreenSaver(idle_ms.clone()))
        .unwrap()
        .build()
        .unwrap();
    let (tx, rx) = mpsc::channel();
    let run = dbus_idle::start_screensaver(&bus_env(), tx, Duration::from_secs(2)).expect("screensaver backend");
    follows_idle_time(&run, "screensaver-dbus", &idle_ms, &rx);
}

#[test]
#[ignore = "needs the extension served by gjs, set KAIROS_EXPECT_PIDS, see tests/linux-desktops"]
fn dbus_gnome_extension_focus() {
    let pids: Vec<u32> = std::env::var("KAIROS_EXPECT_PIDS").expect("KAIROS_EXPECT_PIDS").split(',').map(|p| p.parse().unwrap()).collect();
    let (tx, rx) = mpsc::channel();
    let run = gnome::start(&bus_env(), tx).expect("gnome-shell backend");
    assert_eq!(run.name, "gnome-shell");
    for pid in pids {
        let fg = foreground_with_pid(&rx, pid);
        assert_eq!(fg.wclass, format!("class-{pid}"));
        assert_eq!(fg.title, format!("title-{pid}"));
    }
}
