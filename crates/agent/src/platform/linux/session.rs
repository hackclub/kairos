//! `kairosd` may be started by systemd (or before login) without the variables a desktop sets, so they are looked up
//! in the process environment, then the systemd user manager, then the environment of the user's other processes.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

const WAYLAND_DISPLAY: &str = "WAYLAND_DISPLAY";
const DISPLAY: &str = "DISPLAY";
const XDG_RUNTIME_DIR: &str = "XDG_RUNTIME_DIR";
const XDG_CURRENT_DESKTOP: &str = "XDG_CURRENT_DESKTOP";
const XDG_SESSION_TYPE: &str = "XDG_SESSION_TYPE";
const DBUS_SESSION_BUS_ADDRESS: &str = "DBUS_SESSION_BUS_ADDRESS";
const HYPRLAND_INSTANCE_SIGNATURE: &str = "HYPRLAND_INSTANCE_SIGNATURE";
const SWAYSOCK: &str = "SWAYSOCK";
const I3SOCK: &str = "I3SOCK";
const NIRI_SOCKET: &str = "NIRI_SOCKET";
const XAUTHORITY: &str = "XAUTHORITY";

const VARS: [&str; 11] = [
    WAYLAND_DISPLAY,
    DISPLAY,
    XDG_RUNTIME_DIR,
    XDG_CURRENT_DESKTOP,
    XDG_SESSION_TYPE,
    DBUS_SESSION_BUS_ADDRESS,
    HYPRLAND_INSTANCE_SIGNATURE,
    SWAYSOCK,
    I3SOCK,
    NIRI_SOCKET,
    XAUTHORITY,
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionEnv {
    vars: BTreeMap<&'static str, String>,
}

impl SessionEnv {
    fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str).filter(|v| !v.is_empty())
    }

    fn set(&mut self, key: &'static str, value: String) {
        self.vars.insert(key, value);
    }

    #[cfg(test)]
    pub fn from_pairs(pairs: &[(&'static str, &str)]) -> SessionEnv {
        let mut e = SessionEnv::default();
        for (k, v) in pairs {
            e.set(k, (*v).into());
        }
        e
    }

    pub fn runtime_dir(&self) -> Option<PathBuf> {
        self.get(XDG_RUNTIME_DIR).map(PathBuf::from)
    }

    pub fn wayland_socket(&self) -> Option<PathBuf> {
        let name = self.get(WAYLAND_DISPLAY)?;
        let path = Path::new(name);
        if path.is_absolute() {
            Some(path.to_path_buf())
        } else {
            self.runtime_dir().map(|d| d.join(path))
        }
    }

    pub fn x_display(&self) -> Option<&str> {
        self.get(DISPLAY)
    }

    pub fn dbus_address(&self) -> Option<String> {
        self.get(DBUS_SESSION_BUS_ADDRESS)
            .map(str::to_owned)
            .or_else(|| self.runtime_dir().map(|d| format!("unix:path={}", d.join("bus").display())))
    }

    /// The running Hyprland's socket folder: `$XDG_RUNTIME_DIR/hypr/<sig>`, or `/tmp/hypr/<sig>` before Hyprland 0.40.
    pub fn hyprland_dir(&self) -> Option<PathBuf> {
        let sig = self.get(HYPRLAND_INSTANCE_SIGNATURE)?;
        let mut candidates = Vec::new();
        if let Some(rt) = self.runtime_dir() {
            candidates.push(rt.join("hypr").join(sig));
        }
        candidates.push(PathBuf::from("/tmp/hypr").join(sig));
        candidates.into_iter().find(|d| d.join(".socket2.sock").exists())
    }

    pub fn sway_socket(&self) -> Option<PathBuf> {
        self.get(SWAYSOCK).map(PathBuf::from)
    }

    pub fn i3_socket(&self) -> Option<PathBuf> {
        self.get(I3SOCK).map(PathBuf::from)
    }

    pub fn niri_socket(&self) -> Option<PathBuf> {
        self.get(NIRI_SOCKET).map(PathBuf::from)
    }

    pub fn xauthority(&self) -> Option<PathBuf> {
        self.get(XAUTHORITY).map(PathBuf::from)
    }

    /// X11 focus and idle are only trusted in a session that is not Wayland: under Wayland, X11
    /// clients only see other X11 clients.
    pub fn is_wayland(&self) -> bool {
        self.get(XDG_SESSION_TYPE) == Some("wayland") || self.wayland_socket().is_some_and(|p| p.exists())
    }

    pub fn has_display(&self) -> bool {
        self.wayland_socket().is_some_and(|p| p.exists()) || self.x_display().and_then(x_socket).is_some_and(|p| p.exists())
    }

    fn runs(&self, desktop: &str) -> bool {
        self.get(XDG_CURRENT_DESKTOP).is_none_or(|d| d.to_lowercase().contains(desktop))
    }

    fn fill_defaults(&mut self, uid: u32) {
        if self.get(XDG_RUNTIME_DIR).is_none() {
            let dir = PathBuf::from(format!("/run/user/{uid}"));
            if dir.is_dir() {
                self.set(XDG_RUNTIME_DIR, dir.to_string_lossy().into_owned());
            }
        }
        // kairosd keeps the environment it started with, so after the compositor restarts (or a new login) these still
        // name the old instance while WAYLAND_DISPLAY is reused. One whose socket is gone counts as unset.
        if self.get(HYPRLAND_INSTANCE_SIGNATURE).is_some() && self.hyprland_dir().is_none() {
            self.vars.remove(HYPRLAND_INSTANCE_SIGNATURE);
        }
        for key in [SWAYSOCK, NIRI_SOCKET] {
            if self.get(key).is_some_and(|p| !Path::new(p).exists()) {
                self.vars.remove(key);
            }
        }
        let Some(rt) = self.runtime_dir() else { return };
        if self.get(HYPRLAND_INSTANCE_SIGNATURE).is_none() && self.runs("hyprland") {
            if let Some(sig) = newest(hyprland_sockets(&rt)).and_then(|p| p.parent().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned())) {
                self.set(HYPRLAND_INSTANCE_SIGNATURE, sig);
            }
        }
        if self.get(SWAYSOCK).is_none() && self.runs("sway") {
            if let Some(p) = newest(sockets_named(&rt, "sway-ipc.")) {
                self.set(SWAYSOCK, p.to_string_lossy().into_owned());
            }
        }
        if self.get(NIRI_SOCKET).is_none() && self.runs("niri") {
            if let Some(p) = newest(sockets_named(&rt, "niri.")) {
                self.set(NIRI_SOCKET, p.to_string_lossy().into_owned());
            }
        }
    }
}

/// `:1` and `:1.0` map to the socket X servers create in `/tmp/.X11-unix`; remote displays are not supported.
fn x_socket(display: &str) -> Option<PathBuf> {
    let number = display.strip_prefix(':')?.split('.').next()?;
    number.parse::<u16>().ok()?;
    Some(PathBuf::from(format!("/tmp/.X11-unix/X{number}")))
}

fn hyprland_sockets(runtime_dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(runtime_dir.join("hypr"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join(".socket2.sock"))
        .filter(|p| p.exists())
        .collect()
}

fn sockets_named(runtime_dir: &Path, prefix: &str) -> Vec<PathBuf> {
    fs::read_dir(runtime_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| is_ipc_socket_name(&e.file_name().to_string_lossy(), prefix))
        .map(|e| e.path())
        .collect()
}

fn is_ipc_socket_name(name: &str, prefix: &str) -> bool {
    name.starts_with(prefix) && name.ends_with(".sock")
}

fn newest(paths: Vec<PathBuf>) -> Option<PathBuf> {
    paths.into_iter().max_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH))
}

fn pick(vars: impl Iterator<Item = (String, String)>) -> SessionEnv {
    let mut env = SessionEnv::default();
    for (k, v) in vars {
        if let Some(key) = VARS.iter().find(|n| **n == k) {
            env.set(key, v);
        }
    }
    env
}

fn parse_environ(raw: &[u8]) -> SessionEnv {
    pick(raw.split(|&b| b == 0).filter_map(|entry| {
        let entry = String::from_utf8_lossy(entry);
        entry.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned()))
    }))
}

/// `systemctl --user show-environment`: `KEY=VALUE` lines. Values with special characters come out
/// shell-quoted (`$'...'`); none of the variables read here need that, so those lines are skipped.
fn parse_show_environment(text: &str) -> SessionEnv {
    pick(text.lines().filter_map(|l| l.split_once('=')).filter(|(_, v)| !v.starts_with("$'")).map(|(k, v)| (k.to_owned(), v.to_owned())))
}

/// Field 22 of `/proc/<pid>/stat`, clock ticks since boot at which the process started.
fn parse_start_time(stat: &str) -> Option<u64> {
    stat.rfind(')').map(|i| &stat[i + 1..])?.split_whitespace().nth(19)?.parse().ok()
}

fn systemd_env() -> Option<SessionEnv> {
    let out = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(parse_show_environment(&String::from_utf8_lossy(&out.stdout)))
}

fn proc_env(uid: u32) -> Option<SessionEnv> {
    fs::read_dir("/proc")
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
        .filter(|e| e.metadata().is_ok_and(|m| m.uid() == uid))
        .filter_map(|e| {
            let env = parse_environ(&fs::read(e.path().join("environ")).ok()?);
            if !env.has_display() {
                return None;
            }
            let started = parse_start_time(&fs::read_to_string(e.path().join("stat")).ok()?)?;
            Some((started, env))
        })
        .max_by_key(|(started, _)| *started)
        .map(|(_, env)| env)
}

fn current_uid() -> Option<u32> {
    fs::metadata("/proc/self").ok().map(|m| m.uid())
}

pub fn discover() -> Option<SessionEnv> {
    let uid = current_uid()?;
    let filled = |mut e: SessionEnv| {
        e.fill_defaults(uid);
        e
    };
    let own = filled(pick(std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))));
    if own.has_display() {
        return Some(own);
    }
    systemd_env().map(filled).filter(SessionEnv::has_display).or_else(|| proc_env(uid).map(filled))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environ_keeps_only_the_session_variables() {
        let raw = b"PATH=/usr/bin\0WAYLAND_DISPLAY=wayland-1\0HOME=/home/a\0DISPLAY=:0\0EMPTY\0XDG_RUNTIME_DIR=/run/user/1000\0";
        let e = parse_environ(raw);
        assert_eq!(e.get(WAYLAND_DISPLAY), Some("wayland-1"));
        assert_eq!(e.get(DISPLAY), Some(":0"));
        assert_eq!(e.runtime_dir(), Some(PathBuf::from("/run/user/1000")));
        assert_eq!(e.vars.len(), 3);
    }

    #[test]
    fn show_environment_skips_quoted_values_and_blank_lines() {
        let text = "LANG=en_US.UTF-8\nWAYLAND_DISPLAY=wayland-0\nXDG_CURRENT_DESKTOP=$'a b\\nc'\n\nDISPLAY=:1\nbroken\n";
        let e = parse_show_environment(text);
        assert_eq!(e.get(WAYLAND_DISPLAY), Some("wayland-0"));
        assert_eq!(e.x_display(), Some(":1"));
        assert_eq!(e.get(XDG_CURRENT_DESKTOP), None);
    }

    #[test]
    fn start_time_survives_spaces_and_parens_in_the_command_name() {
        let stat = "1234 (my) prog) S 1 1234 1234 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10 18446744073709551615";
        assert_eq!(parse_start_time(stat), Some(987_654));
        assert_eq!(parse_start_time("garbage"), None);
    }

    #[test]
    fn x_sockets_come_from_local_display_names_only() {
        assert_eq!(x_socket(":0"), Some(PathBuf::from("/tmp/.X11-unix/X0")));
        assert_eq!(x_socket(":12.1"), Some(PathBuf::from("/tmp/.X11-unix/X12")));
        assert_eq!(x_socket("localhost:10.0"), None);
        assert_eq!(x_socket(":x"), None);
    }

    #[test]
    fn wayland_socket_resolves_against_the_runtime_dir() {
        let e = SessionEnv::from_pairs(&[(WAYLAND_DISPLAY, "wayland-1"), (XDG_RUNTIME_DIR, "/run/user/1000")]);
        assert_eq!(e.wayland_socket(), Some(PathBuf::from("/run/user/1000/wayland-1")));
        let abs = SessionEnv::from_pairs(&[(WAYLAND_DISPLAY, "/tmp/wl-test")]);
        assert_eq!(abs.wayland_socket(), Some(PathBuf::from("/tmp/wl-test")));
        assert_eq!(SessionEnv::from_pairs(&[(WAYLAND_DISPLAY, "wayland-1")]).wayland_socket(), None);
    }

    #[test]
    fn dbus_address_defaults_to_the_runtime_dir_bus() {
        assert_eq!(SessionEnv::from_pairs(&[(XDG_RUNTIME_DIR, "/run/user/1000")]).dbus_address().as_deref(), Some("unix:path=/run/user/1000/bus"));
        let set = SessionEnv::from_pairs(&[(DBUS_SESSION_BUS_ADDRESS, "unix:abstract=x"), (XDG_RUNTIME_DIR, "/run/user/1000")]);
        assert_eq!(set.dbus_address().as_deref(), Some("unix:abstract=x"));
        assert_eq!(SessionEnv::default().dbus_address(), None);
    }

    #[test]
    fn compositor_sockets_are_only_guessed_for_the_running_desktop() {
        assert!(SessionEnv::from_pairs(&[]).runs("sway"));
        assert!(SessionEnv::from_pairs(&[(XDG_CURRENT_DESKTOP, "sway")]).runs("sway"));
        assert!(SessionEnv::from_pairs(&[(XDG_CURRENT_DESKTOP, "Hyprland")]).runs("hyprland"));
        assert!(!SessionEnv::from_pairs(&[(XDG_CURRENT_DESKTOP, "GNOME")]).runs("sway"));
    }

    #[test]
    fn ipc_socket_names() {
        assert!(is_ipc_socket_name("sway-ipc.1000.4321.sock", "sway-ipc."));
        assert!(is_ipc_socket_name("niri.wayland-1.999.sock", "niri."));
        assert!(!is_ipc_socket_name("wayland-1.lock", "sway-ipc."));
        assert!(!is_ipc_socket_name("sway-ipc.1000.4321.sock.bak", "sway-ipc."));
    }

    #[test]
    fn newest_picks_the_most_recently_modified_socket() {
        let dir = std::env::temp_dir().join(format!("kairos-session-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let old = dir.join("sway-ipc.1.1.sock");
        let new = dir.join("sway-ipc.1.2.sock");
        fs::write(&old, "").unwrap();
        fs::File::options().write(true).open(&old).unwrap().set_modified(SystemTime::UNIX_EPOCH).unwrap();
        fs::write(&new, "").unwrap();
        assert_eq!(newest(sockets_named(&dir, "sway-ipc.")), Some(new));
        let _ = fs::remove_dir_all(&dir);
    }

    fn runtime_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kairos-session-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_hyprland_signature_from_before_a_restart_gives_way_to_the_running_one() {
        let rt = runtime_dir("hypr-restart");
        fs::create_dir_all(rt.join("hypr/new")).unwrap();
        fs::write(rt.join("hypr/new/.socket2.sock"), "").unwrap();
        let rts = rt.to_string_lossy().into_owned();
        let mut e = SessionEnv::from_pairs(&[(XDG_RUNTIME_DIR, &rts), (XDG_CURRENT_DESKTOP, "Hyprland"), (HYPRLAND_INSTANCE_SIGNATURE, "old")]);
        e.fill_defaults(0);
        assert_eq!(e.hyprland_dir(), Some(rt.join("hypr/new")));
        let _ = fs::remove_dir_all(&rt);
    }

    #[test]
    fn a_hyprland_signature_that_still_runs_is_kept() {
        let rt = runtime_dir("hypr-alive");
        for sig in ["mine", "later"] {
            fs::create_dir_all(rt.join("hypr").join(sig)).unwrap();
            fs::write(rt.join("hypr").join(sig).join(".socket2.sock"), "").unwrap();
        }
        let rts = rt.to_string_lossy().into_owned();
        let mut e = SessionEnv::from_pairs(&[(XDG_RUNTIME_DIR, &rts), (XDG_CURRENT_DESKTOP, "Hyprland"), (HYPRLAND_INSTANCE_SIGNATURE, "mine")]);
        e.fill_defaults(0);
        assert_eq!(e.hyprland_dir(), Some(rt.join("hypr/mine")));
        let _ = fs::remove_dir_all(&rt);
    }

    #[test]
    fn a_hyprland_signature_with_nothing_running_is_dropped() {
        let rt = runtime_dir("hypr-gone");
        let rts = rt.to_string_lossy().into_owned();
        let mut e = SessionEnv::from_pairs(&[(XDG_RUNTIME_DIR, &rts), (XDG_CURRENT_DESKTOP, "Hyprland"), (HYPRLAND_INSTANCE_SIGNATURE, "old")]);
        e.fill_defaults(0);
        assert_eq!(e.get(HYPRLAND_INSTANCE_SIGNATURE), None);
        let _ = fs::remove_dir_all(&rt);
    }

    #[test]
    fn a_sway_or_niri_socket_from_before_a_restart_gives_way_to_the_running_one() {
        let rt = runtime_dir("sway-restart");
        fs::write(rt.join("sway-ipc.1000.20.sock"), "").unwrap();
        fs::write(rt.join("niri.wayland-1.30.sock"), "").unwrap();
        let rts = rt.to_string_lossy().into_owned();
        let old_sway = rt.join("sway-ipc.1000.10.sock").to_string_lossy().into_owned();
        let old_niri = rt.join("niri.wayland-1.10.sock").to_string_lossy().into_owned();
        let mut sway = SessionEnv::from_pairs(&[(XDG_RUNTIME_DIR, &rts), (XDG_CURRENT_DESKTOP, "sway"), (SWAYSOCK, &old_sway)]);
        sway.fill_defaults(0);
        assert_eq!(sway.sway_socket(), Some(rt.join("sway-ipc.1000.20.sock")));
        let mut niri = SessionEnv::from_pairs(&[(XDG_RUNTIME_DIR, &rts), (XDG_CURRENT_DESKTOP, "niri"), (NIRI_SOCKET, &old_niri)]);
        niri.fill_defaults(0);
        assert_eq!(niri.niri_socket(), Some(rt.join("niri.wayland-1.30.sock")));
        let _ = fs::remove_dir_all(&rt);
    }
}
