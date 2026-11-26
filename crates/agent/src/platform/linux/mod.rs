mod backend;
mod dbus;
mod dbus_idle;
#[cfg(test)]
mod desktop_tests;
mod gnome;
mod hyprland;
mod kwin;
mod niri;
mod presence;
mod procs;
mod session;
mod supervisor;
mod sway;
mod wayland_idle;
mod x11;

use std::path::PathBuf;

use kairos_core::exclude::Exclusions;

use crate::classify::{classify_with, Classified};
use crate::platform::{ttys, Backends, Foreground, Sinks};

pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

pub fn classify(fg: &Foreground, ex: &Exclusions) -> Classified {
    let mut snap = procs::ProcFs::scan();
    if !fg.sessions.is_empty() {
        snap.show(fg.pid, &fg.sessions);
    }
    classify_with(&snap, fg.pid, &fg.wclass, &fg.title, ex)
}

pub fn spawn(mut sinks: Sinks) -> Backends {
    let _ = kairos_core::gnome_extension::refresh_gnome_extension();
    // No backend tells keys from other input.
    let hooks = ttys::Hooks { scan: procs::ProcFs::scan, atime: procs::tty_atime, typed: |_| true };
    sinks.foreground = ttys::spawn(sinks.foreground, hooks);
    supervisor::spawn(sinks)
}
