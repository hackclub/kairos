use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread;

use anyhow::{anyhow, Result};
use serde::Deserialize;

use super::backend::{running, Reporter, Running};
use super::session::SessionEnv;
use crate::platform::Foreground;

pub const NAME: &str = "hyprland";

fn socket_dir(env: &SessionEnv) -> Option<PathBuf> {
    let sig = env.hyprland_signature()?;
    let mut candidates = Vec::new();
    if let Some(rt) = env.runtime_dir() {
        candidates.push(rt.join("hypr").join(sig));
    }
    candidates.push(PathBuf::from("/tmp/hypr").join(sig));
    candidates.into_iter().find(|d| d.join(".socket2.sock").exists())
}

#[derive(Deserialize)]
struct Window {
    address: Option<String>,
    pid: Option<i64>,
    class: Option<String>,
    title: Option<String>,
}

fn parse_window(json: &[u8]) -> Option<Foreground> {
    let w: Window = serde_json::from_slice(json).ok()?;
    let pid = u32::try_from(w.pid?).ok().filter(|p| *p > 0)?;
    let window = w.address.as_deref().and_then(|a| u64::from_str_radix(a.trim_start_matches("0x"), 16).ok()).unwrap_or(0);
    Some(Foreground { pid, wclass: w.class.unwrap_or_default(), title: w.title.unwrap_or_default(), sessions: Vec::new(), window })
}

fn active_window(dir: &std::path::Path) -> Option<Foreground> {
    let mut stream = UnixStream::connect(dir.join(".socket.sock")).ok()?;
    stream.write_all(b"j/activewindow").ok()?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).ok()?;
    parse_window(&reply)
}

fn changes_focus(line: &[u8]) -> bool {
    line.starts_with(b"activewindow>>") || line.starts_with(b"windowtitle>>") || line.starts_with(b"windowtitlev2>>")
}

pub fn start(env: &SessionEnv, fg: Sender<Option<Foreground>>) -> Result<Running> {
    let dir = socket_dir(env).ok_or_else(|| anyhow!("no Hyprland socket"))?;
    let events = UnixStream::connect(dir.join(".socket2.sock"))?;
    let (handle, life) = running(NAME);
    thread::spawn(move || {
        let _life = life;
        let mut reporter = Reporter::new(fg);
        if !reporter.report(active_window(&dir)) {
            return;
        }
        let mut reader = BufReader::new(events);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) if changes_focus(&line) => {
                    if !reporter.report(active_window(&dir)) {
                        return;
                    }
                }
                Ok(_) => {}
            }
        }
    });
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_active_window() {
        let fg = parse_window(br#"{"address":"0x1","class":"kitty","title":"~","pid":4242}"#).unwrap();
        assert_eq!(fg, Foreground { pid: 4242, wclass: "kitty".into(), title: "~".into(), sessions: vec![], window: 1 });
    }

    #[test]
    fn no_window_means_no_foreground() {
        assert_eq!(parse_window(b"{}"), None);
        assert_eq!(parse_window(b""), None);
        assert_eq!(parse_window(br#"{"class":"kitty","pid":-1}"#), None);
    }

    #[test]
    fn focus_and_title_events_trigger_a_refresh() {
        assert!(changes_focus(b"activewindow>>kitty,~\n"));
        assert!(changes_focus(b"windowtitle>>80a6f50\n"));
        assert!(!changes_focus(b"openwindow>>80a6f50,1,kitty,~\n"));
        assert!(!changes_focus(b"activewindowv2>>80a6f50\n"));
    }
}
