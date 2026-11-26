use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, AtomEnum, ConnectionExt, EventMask, Window};
use x11rb::protocol::{screensaver, Event};
use x11rb::reexports::x11rb_protocol::parse_display::parse_display;
use x11rb::reexports::x11rb_protocol::xauth::Family;
use x11rb::rust_connection::{DefaultStream, RustConnection};

use super::backend::{running, Reporter, Running};
use super::presence::poll;
use super::session::SessionEnv;
use crate::platform::Foreground;
use crate::Msg;

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        _NET_ACTIVE_WINDOW,
        _NET_SUPPORTING_WM_CHECK,
        _NET_WM_PID,
        _NET_WM_NAME,
        UTF8_STRING,
        WM_NAME,
        WM_CLASS,
        I3_SOCKET_PATH,
    }
}

pub struct X11 {
    pub conn: RustConnection,
    pub root: Window,
    pub atoms: Atoms,
}

const MIT_MAGIC_COOKIE: &[u8] = b"MIT-MAGIC-COOKIE-1";

/// Connects to `DISPLAY`, authenticating with the session's `XAUTHORITY` (or `~/.Xauthority`), not the process
/// environment, which may belong to another session.
pub fn connect(env: &SessionEnv) -> Result<X11> {
    let display = env.x_display().ok_or_else(|| anyhow!("no X11 display"))?;
    let parsed = parse_display(Some(display)).map_err(|e| anyhow!("bad DISPLAY: {e:?}"))?;
    let screen = usize::from(parsed.screen);
    let mut last = None;
    for addr in parsed.connect_instruction() {
        match DefaultStream::connect(&addr) {
            Ok((stream, (family, address))) => {
                let cookie = env
                    .xauthority()
                    .or_else(|| dirs::home_dir().map(|h| h.join(".Xauthority")))
                    .and_then(|path| cookie_for(&path, family, &address, parsed.display));
                let (auth_name, auth_data) = cookie.map_or_else(Default::default, |c| (MIT_MAGIC_COOKIE.to_vec(), c));
                let conn = RustConnection::connect_to_stream_with_auth_info(stream, screen, auth_name, auth_data)
                    .context("X11 handshake failed")?;
                let root = conn.setup().roots.get(screen).map(|s| s.root).ok_or_else(|| anyhow!("no such X11 screen"))?;
                let atoms = Atoms::new(&conn)?.reply()?;
                return Ok(X11 { conn, root, atoms });
            }
            Err(e) => last = Some(e),
        }
    }
    Err(anyhow!("cannot reach the X server: {}", last.map_or_else(|| "no address".into(), |e| e.to_string())))
}

/// The MIT-MAGIC-COOKIE-1 for this server from an Xauthority file: entries are big-endian
/// `family:u16, address, display number, name, data` with u16-length-prefixed strings.
fn cookie_for(path: &Path, family: Family, address: &[u8], display: u16) -> Option<Vec<u8>> {
    let mut file = BufReader::new(File::open(path).ok()?);
    let number = display.to_string();
    loop {
        let entry_family = Family::from(read_u16(&mut file).ok()?);
        let entry_address = read_string(&mut file)?;
        let entry_number = read_string(&mut file)?;
        let name = read_string(&mut file)?;
        let data = read_string(&mut file)?;
        let family_matches = entry_family == Family::WILD || family == Family::WILD || (entry_family == family && entry_address == address);
        if family_matches && (entry_number.is_empty() || entry_number == number.as_bytes()) && name == MIT_MAGIC_COOKIE {
            return Some(data);
        }
    }
}

fn read_u16(r: &mut impl Read) -> std::io::Result<u16> {
    let mut buf = [0; 2];
    r.read_exact(&mut buf)?;
    Ok(u16::from_be_bytes(buf))
}

fn read_string(r: &mut impl Read) -> Option<Vec<u8>> {
    let mut buf = vec![0; usize::from(read_u16(r).ok()?)];
    r.read_exact(&mut buf).ok()?;
    Some(buf)
}

impl X11 {
    fn property(&self, window: Window, property: u32, kind: impl Into<u32>) -> Option<xproto::GetPropertyReply> {
        let reply = self.conn.get_property(false, window, property, kind, 0, 1024).ok()?.reply().ok()?;
        (reply.value_len > 0).then_some(reply)
    }

    fn first_u32(&self, window: Window, property: u32, kind: AtomEnum) -> Option<u32> {
        let reply = self.property(window, property, kind)?;
        let first = reply.value32()?.next();
        first
    }

    pub fn active_window(&self) -> Option<Window> {
        self.first_u32(self.root, self.atoms._NET_ACTIVE_WINDOW, AtomEnum::WINDOW).filter(|w| *w != 0)
    }

    fn title(&self, window: Window) -> String {
        let utf8 = self.property(window, self.atoms._NET_WM_NAME, self.atoms.UTF8_STRING);
        let reply = utf8.or_else(|| self.property(window, self.atoms.WM_NAME, AtomEnum::STRING));
        reply.map(|r| String::from_utf8_lossy(&r.value).into_owned()).unwrap_or_default()
    }

    /// `WM_CLASS` is `instance\0class\0`; the class is the stable name of the application.
    fn class(&self, window: Window) -> String {
        let Some(reply) = self.property(window, self.atoms.WM_CLASS, AtomEnum::STRING) else { return String::new() };
        let mut parts = reply.value.split(|b| *b == 0).filter(|p| !p.is_empty());
        let instance = parts.next();
        String::from_utf8_lossy(parts.next().or(instance).unwrap_or_default()).into_owned()
    }

    /// `None` when the window is gone. Windows that do not set `_NET_WM_PID` get pid 0, which the
    /// reporter treats as no usable window.
    pub fn window_info(&self, window: Window) -> Option<Foreground> {
        self.conn.get_window_attributes(window).ok()?.reply().ok()?;
        let pid = self.first_u32(window, self.atoms._NET_WM_PID, AtomEnum::CARDINAL).unwrap_or(0);
        Some(Foreground { pid, wclass: self.class(window), title: self.title(window), sessions: Vec::new(), window: u64::from(window) })
    }

    fn watch(&self, window: Window, mask: EventMask) {
        let _ = self.conn.change_window_attributes(window, &xproto::ChangeWindowAttributesAux::new().event_mask(mask));
    }

    fn has_window_manager(&self) -> bool {
        self.property(self.root, self.atoms._NET_SUPPORTING_WM_CHECK, AtomEnum::WINDOW).is_some()
    }

    pub fn i3_socket_path(&self) -> Option<String> {
        let reply = self.property(self.root, self.atoms.I3_SOCKET_PATH, self.atoms.UTF8_STRING)?;
        Some(String::from_utf8_lossy(&reply.value).into_owned())
    }
}

pub const NAME: &str = "x11";

pub fn start(env: &SessionEnv, fg: Sender<Option<Foreground>>) -> Result<Running> {
    if env.is_wayland() {
        bail!("Wayland session");
    }
    let x = connect(env)?;
    if !x.has_window_manager() {
        bail!("the window manager does not implement EWMH");
    }
    x.watch(x.root, EventMask::PROPERTY_CHANGE);
    x.conn.flush()?;
    let (handle, life) = running(NAME);
    thread::spawn(move || {
        let _life = life;
        let mut reporter = Reporter::new(fg);
        let mut active = x.active_window();
        if let Some(w) = active {
            x.watch(w, EventMask::PROPERTY_CHANGE);
        }
        if !reporter.report(active.and_then(|w| x.window_info(w))) {
            return;
        }
        let _ = x.conn.flush();
        while let Ok(event) = x.conn.wait_for_event() {
            let Event::PropertyNotify(change) = event else { continue };
            let refresh = if change.window == x.root && change.atom == x.atoms._NET_ACTIVE_WINDOW {
                if let Some(old) = active {
                    x.watch(old, EventMask::NO_EVENT);
                }
                active = x.active_window();
                if let Some(w) = active {
                    x.watch(w, EventMask::PROPERTY_CHANGE);
                }
                true
            } else {
                Some(change.window) == active && (change.atom == x.atoms._NET_WM_NAME || change.atom == x.atoms.WM_NAME)
            };
            if refresh && !reporter.report(active.and_then(|w| x.window_info(w))) {
                return;
            }
            let _ = x.conn.flush();
        }
    });
    Ok(handle)
}

pub const IDLE_NAME: &str = "x11-screensaver";

pub fn start_idle(env: &SessionEnv, tx: Sender<Msg>, threshold: Duration) -> Result<Running> {
    if env.is_wayland() {
        bail!("Wayland session");
    }
    let x = connect(env)?;
    screensaver::query_version(&x.conn, 1, 1)?.reply().context("MIT-SCREEN-SAVER is not available")?;
    poll(IDLE_NAME, threshold, tx, move || {
        let ms = screensaver::query_info(&x.conn, x.root)?.reply()?.ms_since_user_input;
        Ok(Duration::from_millis(ms.into()))
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn entry(family: u16, address: &[u8], number: &[u8], name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = family.to_be_bytes().to_vec();
        for field in [address, number, name, data] {
            out.extend((field.len() as u16).to_be_bytes());
            out.extend(field);
        }
        out
    }

    fn file(entries: &[Vec<u8>]) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("kairos-xauth-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, entries.concat()).unwrap();
        path
    }

    #[test]
    fn finds_the_cookie_for_this_host_and_display() {
        let path = file(&[
            entry(256, b"otherhost", b"0", MIT_MAGIC_COOKIE, b"wrong"),
            entry(256, b"myhost", b"1", MIT_MAGIC_COOKIE, b"wrong"),
            entry(256, b"myhost", b"0", b"XDM-AUTHORIZATION-1", b"wrong"),
            entry(256, b"myhost", b"0", MIT_MAGIC_COOKIE, b"right"),
        ]);
        assert_eq!(cookie_for(&path, Family::LOCAL, b"myhost", 0), Some(b"right".to_vec()));
        assert_eq!(cookie_for(&path, Family::LOCAL, b"myhost", 7), None);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn wildcard_family_and_empty_number_match_anything() {
        let path = file(&[entry(65535, b"", b"", MIT_MAGIC_COOKIE, b"any")]);
        assert_eq!(cookie_for(&path, Family::LOCAL, b"myhost", 3), Some(b"any".to_vec()));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn missing_or_truncated_files_give_no_cookie() {
        assert_eq!(cookie_for(Path::new("/nonexistent/xauth"), Family::LOCAL, b"h", 0), None);
        let mut cut = entry(256, b"h", b"0", MIT_MAGIC_COOKIE, b"data");
        cut.truncate(cut.len() - 2);
        let path = file(&[cut]);
        assert_eq!(cookie_for(&path, Family::LOCAL, b"h", 0), None);
        let _ = fs::remove_file(path);
    }
}
