use std::sync::mpsc::Sender;
use std::thread;

use anyhow::{Context, Result};
use zbus::blocking::{Connection, Proxy};

use super::backend::{running, Reporter, Running};
use super::dbus::{self, Notice, SignalSpec, Watch};
use super::session::SessionEnv;
use crate::platform::Foreground;

pub const NAME: &str = "gnome-shell";

const SHELL: &str = "org.gnome.Shell";
const PATH: &str = "/dev/obayy/Kairos";
const INTERFACE: &str = "dev.obayy.Kairos.Focus1";

// An extension loaded before 0.1.22 only has `Get`, and GNOME loads the new one at the next login.
fn focused(conn: &Connection) -> Result<Option<Foreground>> {
    let proxy = Proxy::new(conn, SHELL, PATH, INTERFACE)?;
    let (pid, wclass, title, window) = match proxy.call::<_, _, (u32, String, String, u64)>("GetWindow", &()) {
        Ok(got) => got,
        Err(_) => {
            let (pid, wclass, title): (u32, String, String) = proxy.call("Get", &())?;
            (pid, wclass, title, 0)
        }
    };
    Ok((pid > 0).then_some(Foreground { pid, wclass, title, sessions: Vec::new(), window }))
}

pub fn start(env: &SessionEnv, fg: Sender<Option<Foreground>>) -> Result<Running> {
    let conn = dbus::connect(env)?;
    let mut watch = Watch::new(&conn, SHELL, Some(SignalSpec { path: PATH, interface: INTERFACE, member: "Changed" }))?;
    let first = focused(&conn).context("the Kairos GNOME Shell extension is not running")?;
    let (handle, life) = running(NAME);
    thread::spawn(move || {
        let _life = life;
        let mut reporter = Reporter::new(fg);
        if !reporter.report(first) {
            return;
        }
        while watch.next() == Notice::Signal {
            let Ok(now) = focused(&conn) else { return };
            if !reporter.report(now) {
                return;
            }
        }
    });
    Ok(handle)
}
