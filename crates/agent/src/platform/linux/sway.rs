//! Sway puts the owning pid in the tree; i3 only has the X11 window id, so there the pid, class and title are read
//! from the X server.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread;

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;

use super::backend::{running, Reporter, Running};
use super::session::SessionEnv;
use super::x11::{self, X11};
use crate::platform::Foreground;

pub const SWAY: &str = "sway";
pub const I3: &str = "i3";

const MAGIC: &[u8; 6] = b"i3-ipc";
const SUBSCRIBE: u32 = 2;
const GET_TREE: u32 = 4;
const EVENT_WINDOW: u32 = 0x8000_0003;
const MAX_PAYLOAD: usize = 32 << 20;

fn encode(kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(14 + payload.len());
    out.extend(MAGIC);
    out.extend((payload.len() as u32).to_ne_bytes());
    out.extend(kind.to_ne_bytes());
    out.extend(payload);
    out
}

fn read_frame(r: &mut impl Read) -> io::Result<(u32, Vec<u8>)> {
    let mut header = [0u8; 14];
    r.read_exact(&mut header)?;
    if &header[..6] != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not an i3 IPC frame"));
    }
    let len = u32::from_ne_bytes([header[6], header[7], header[8], header[9]]) as usize;
    let kind = u32::from_ne_bytes([header[10], header[11], header[12], header[13]]);
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "IPC frame too large"));
    }
    let mut payload = vec![0; len];
    r.read_exact(&mut payload)?;
    Ok((kind, payload))
}

fn request(path: &std::path::Path, kind: u32, payload: &[u8]) -> io::Result<Vec<u8>> {
    let mut stream = UnixStream::connect(path)?;
    stream.write_all(&encode(kind, payload))?;
    let (_, reply) = read_frame(&mut stream)?;
    Ok(reply)
}

#[derive(Deserialize, Default)]
struct Node {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    focused: bool,
    pid: Option<i64>,
    app_id: Option<String>,
    name: Option<String>,
    window: Option<u32>,
    window_properties: Option<Properties>,
    #[serde(default)]
    nodes: Vec<Node>,
    #[serde(default)]
    floating_nodes: Vec<Node>,
}

#[derive(Deserialize)]
struct Properties {
    class: Option<String>,
}

#[derive(Deserialize)]
struct Ack {
    success: bool,
}

#[derive(Deserialize)]
struct WindowEvent {
    change: String,
    container: Node,
}

impl Node {
    fn focused_leaf(&self) -> Option<&Node> {
        if self.focused {
            return Some(self);
        }
        self.nodes.iter().chain(&self.floating_nodes).find_map(Node::focused_leaf)
    }

    /// Wayland-native windows have an `app_id`, XWayland windows a `window_properties.class`.
    fn sway_foreground(&self) -> Option<Foreground> {
        let pid = u32::try_from(self.pid?).ok().filter(|p| *p > 0)?;
        let wclass = self
            .app_id
            .clone()
            .filter(|a| !a.is_empty())
            .or_else(|| self.window_properties.as_ref().and_then(|p| p.class.clone()))
            .unwrap_or_default();
        Some(Foreground { pid, wclass, title: self.name.clone().unwrap_or_default(), sessions: Vec::new(), window: self.id })
    }
}

enum Flavor {
    Sway,
    I3(X11),
}

impl Flavor {
    fn foreground(&self, node: &Node) -> Option<Foreground> {
        match self {
            Flavor::Sway => node.sway_foreground(),
            Flavor::I3(x) => x.window_info(node.window?),
        }
    }
}

struct Ipc {
    path: PathBuf,
    flavor: Flavor,
}

impl Ipc {
    fn focused(&self) -> Option<Foreground> {
        let tree: Node = serde_json::from_slice(&request(&self.path, GET_TREE, b"").ok()?).ok()?;
        self.flavor.foreground(tree.focused_leaf()?)
    }

    /// Focus and title events carry the window; a closed window needs the tree to see what got focus.
    fn on_window_event(&self, payload: &[u8]) -> Option<Option<Foreground>> {
        let event: WindowEvent = serde_json::from_slice(payload).ok()?;
        match event.change.as_str() {
            "focus" => Some(self.flavor.foreground(&event.container)),
            "title" if event.container.focused => Some(self.flavor.foreground(&event.container)),
            "close" => Some(self.focused()),
            _ => None,
        }
    }
}

fn start(path: PathBuf, flavor: Flavor, name: &'static str, fg: Sender<Option<Foreground>>) -> Result<Running> {
    let mut events = UnixStream::connect(&path)?;
    events.write_all(&encode(SUBSCRIBE, br#"["window"]"#))?;
    let (_, ack) = read_frame(&mut events)?;
    if !serde_json::from_slice::<Ack>(&ack).is_ok_and(|a| a.success) {
        bail!("{name} refused the window subscription");
    }
    let ipc = Ipc { path, flavor };
    let (handle, life) = running(name);
    thread::spawn(move || {
        let _life = life;
        let mut reporter = Reporter::new(fg);
        if !reporter.report(ipc.focused()) {
            return;
        }
        while let Ok((kind, payload)) = read_frame(&mut events) {
            if kind != EVENT_WINDOW {
                continue;
            }
            if let Some(foreground) = ipc.on_window_event(&payload) {
                if !reporter.report(foreground) {
                    return;
                }
            }
        }
    });
    Ok(handle)
}

pub fn start_sway(env: &SessionEnv, fg: Sender<Option<Foreground>>) -> Result<Running> {
    let path = env.sway_socket().ok_or_else(|| anyhow!("no Sway socket"))?;
    start(path, Flavor::Sway, SWAY, fg)
}

pub fn start_i3(env: &SessionEnv, fg: Sender<Option<Foreground>>) -> Result<Running> {
    if env.is_wayland() {
        bail!("Wayland session");
    }
    let x = x11::connect(env)?;
    let path = env
        .i3_socket()
        .or_else(|| x.i3_socket_path().map(PathBuf::from))
        .ok_or_else(|| anyhow!("no i3 socket"))?;
    start(path, Flavor::I3(x), I3, fg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_magic_length_type_payload_in_native_order() {
        let frame = encode(SUBSCRIBE, br#"["window"]"#);
        assert_eq!(&frame[..6], b"i3-ipc");
        assert_eq!(u32::from_ne_bytes(frame[6..10].try_into().unwrap()), 10);
        assert_eq!(u32::from_ne_bytes(frame[10..14].try_into().unwrap()), 2);
        assert_eq!(&frame[14..], br#"["window"]"#);
    }

    #[test]
    fn decodes_what_it_encodes_back_to_back() {
        let mut wire = encode(EVENT_WINDOW, b"{\"a\":1}");
        wire.extend(encode(0, b""));
        let mut r = wire.as_slice();
        assert_eq!(read_frame(&mut r).unwrap(), (EVENT_WINDOW, b"{\"a\":1}".to_vec()));
        assert_eq!(read_frame(&mut r).unwrap(), (0, Vec::new()));
        assert!(read_frame(&mut r).is_err());
    }

    #[test]
    fn rejects_bad_magic_truncation_and_oversized_frames() {
        let mut bad = encode(0, b"x");
        bad[0] = b'x';
        assert!(read_frame(&mut bad.as_slice()).is_err());
        let full = encode(0, b"hello");
        assert!(read_frame(&mut &full[..full.len() - 1]).is_err());
        let mut huge = encode(0, b"");
        huge[6..10].copy_from_slice(&u32::MAX.to_ne_bytes());
        assert!(read_frame(&mut huge.as_slice()).is_err());
    }

    const TREE: &str = r#"{"type":"root","focused":false,"nodes":[{"type":"output","focused":false,"nodes":[
        {"type":"workspace","focused":false,"nodes":[
          {"type":"con","focused":false,"pid":10,"app_id":"foot","name":"a"}],
         "floating_nodes":[
          {"type":"floating_con","id":31,"focused":true,"pid":2222,"app_id":null,"name":"x","window":77,"window_properties":{"class":"Xterm","instance":"xterm"}}]}]}]}"#;

    #[test]
    fn finds_the_focused_window_including_floating_ones() {
        let tree: Node = serde_json::from_str(TREE).unwrap();
        let node = tree.focused_leaf().unwrap();
        assert_eq!(node.window, Some(77));
        assert_eq!(node.sway_foreground(), Some(Foreground { pid: 2222, wclass: "Xterm".into(), title: "x".into(), sessions: vec![], window: 31 }));
    }

    #[test]
    fn native_windows_use_the_app_id() {
        let node: Node = serde_json::from_str(r#"{"focused":true,"pid":10,"app_id":"foot","name":"~","window":null}"#).unwrap();
        assert_eq!(node.sway_foreground(), Some(Foreground { pid: 10, wclass: "foot".into(), title: "~".into(), sessions: vec![], window: 0 }));
    }

    #[test]
    fn containers_without_a_pid_are_not_windows() {
        let tree: Node = serde_json::from_str(r#"{"focused":true,"name":"1","nodes":[]}"#).unwrap();
        assert_eq!(tree.focused_leaf().unwrap().sway_foreground(), None);
    }

    #[test]
    fn window_events_parse_and_only_the_relevant_changes_report() {
        let ipc = Ipc { path: PathBuf::from("/nonexistent"), flavor: Flavor::Sway };
        let focus = br#"{"change":"focus","container":{"focused":true,"pid":5,"app_id":"foot","name":"t"}}"#;
        assert_eq!(ipc.on_window_event(focus), Some(Some(Foreground { pid: 5, wclass: "foot".into(), title: "t".into(), sessions: vec![], window: 0 })));
        let background_title = br#"{"change":"title","container":{"focused":false,"pid":6,"app_id":"foot","name":"t"}}"#;
        assert_eq!(ipc.on_window_event(background_title), None);
        let urgent = br#"{"change":"urgent","container":{"focused":true,"pid":5}}"#;
        assert_eq!(ipc.on_window_event(urgent), None);
        assert_eq!(ipc.on_window_event(b"not json"), None);
    }
}
