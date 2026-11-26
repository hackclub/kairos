use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::Sender;
use std::thread;

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;

use super::backend::{running, Reporter, Running};
use super::session::SessionEnv;
use crate::platform::Foreground;

pub const NAME: &str = "niri";

#[derive(Deserialize, Clone)]
struct Window {
    id: u64,
    title: Option<String>,
    app_id: Option<String>,
    pid: Option<i32>,
    #[serde(default)]
    is_focused: bool,
}

#[derive(Deserialize)]
struct WindowsChanged {
    windows: Vec<Window>,
}

#[derive(Deserialize)]
struct WindowOpenedOrChanged {
    window: Window,
}

#[derive(Deserialize)]
struct WindowClosed {
    id: u64,
}

#[derive(Deserialize)]
struct WindowFocusChanged {
    id: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Event {
    windows_changed: Option<WindowsChanged>,
    window_opened_or_changed: Option<WindowOpenedOrChanged>,
    window_closed: Option<WindowClosed>,
    window_focus_changed: Option<WindowFocusChanged>,
}

#[derive(Default)]
struct Windows {
    all: HashMap<u64, Window>,
    focused: Option<u64>,
}

impl Windows {
    fn apply(&mut self, event: Event) {
        if let Some(e) = event.windows_changed {
            self.all = e.windows.into_iter().map(|w| (w.id, w)).collect();
            self.focused = self.all.values().find(|w| w.is_focused).map(|w| w.id);
        }
        if let Some(e) = event.window_opened_or_changed {
            if e.window.is_focused {
                self.focused = Some(e.window.id);
            }
            self.all.insert(e.window.id, e.window);
        }
        if let Some(e) = event.window_closed {
            self.all.remove(&e.id);
            if self.focused == Some(e.id) {
                self.focused = None;
            }
        }
        if let Some(e) = event.window_focus_changed {
            self.focused = e.id;
        }
    }

    fn foreground(&self) -> Option<Foreground> {
        let w = self.all.get(&self.focused?)?;
        let pid = u32::try_from(w.pid?).ok().filter(|p| *p > 0)?;
        Some(Foreground {
            pid,
            wclass: w.app_id.clone().unwrap_or_default(),
            title: w.title.clone().unwrap_or_default(),
            sessions: Vec::new(),
            window: w.id,
        })
    }
}

pub fn start(env: &SessionEnv, fg: Sender<Option<Foreground>>) -> Result<Running> {
    let path = env.niri_socket().ok_or_else(|| anyhow!("no niri socket"))?;
    let mut stream = UnixStream::connect(path)?;
    stream.write_all(b"\"EventStream\"\n")?;
    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    if !reply.contains("\"Ok\"") {
        bail!("niri refused the event stream");
    }
    let (handle, life) = running(NAME);
    thread::spawn(move || {
        let _life = life;
        let mut reporter = Reporter::new(fg);
        let mut windows = Windows::default();
        let mut line = String::new();
        loop {
            line.clear();
            if !matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                return;
            }
            let Ok(event) = serde_json::from_str::<Event>(&line) else { continue };
            windows.apply(event);
            if !reporter.report(windows.foreground()) {
                return;
            }
        }
    });
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(w: &mut Windows, line: &str) {
        w.apply(serde_json::from_str(line).unwrap());
    }

    const OPEN: &str = r#"{"WindowsChanged":{"windows":[
        {"id":1,"title":"~","app_id":"foot","pid":100,"workspace_id":1,"is_focused":false,"is_floating":false,"is_urgent":false},
        {"id":2,"title":"docs","app_id":"firefox","pid":200,"workspace_id":1,"is_focused":true,"is_floating":false,"is_urgent":false}]}}"#;

    #[test]
    fn the_window_list_picks_the_focused_window() {
        let mut w = Windows::default();
        feed(&mut w, &OPEN.replace('\n', ""));
        assert_eq!(w.foreground(), Some(Foreground { pid: 200, wclass: "firefox".into(), title: "docs".into(), sessions: vec![], window: 2 }));
    }

    #[test]
    fn focus_moves_and_clears() {
        let mut w = Windows::default();
        feed(&mut w, &OPEN.replace('\n', ""));
        feed(&mut w, r#"{"WindowFocusChanged":{"id":1}}"#);
        assert_eq!(w.foreground().map(|f| f.pid), Some(100));
        feed(&mut w, r#"{"WindowFocusChanged":{"id":null}}"#);
        assert_eq!(w.foreground(), None);
    }

    #[test]
    fn closing_the_focused_window_leaves_no_foreground() {
        let mut w = Windows::default();
        feed(&mut w, &OPEN.replace('\n', ""));
        feed(&mut w, r#"{"WindowClosed":{"id":2}}"#);
        assert_eq!(w.foreground(), None);
        feed(&mut w, r#"{"WindowFocusChanged":{"id":1}}"#);
        assert_eq!(w.foreground().map(|f| f.pid), Some(100));
    }

    #[test]
    fn opened_and_changed_windows_update_title_and_focus() {
        let mut w = Windows::default();
        feed(&mut w, r#"{"WindowOpenedOrChanged":{"window":{"id":5,"title":"a","app_id":"foot","pid":50,"is_focused":true}}}"#);
        assert_eq!(w.foreground().map(|f| f.title), Some("a".into()));
        feed(&mut w, r#"{"WindowOpenedOrChanged":{"window":{"id":5,"title":"b","app_id":"foot","pid":50,"is_focused":true}}}"#);
        assert_eq!(w.foreground().map(|f| f.title), Some("b".into()));
    }

    #[test]
    fn windows_without_a_pid_and_unrelated_events_are_ignored() {
        let mut w = Windows::default();
        feed(&mut w, r#"{"WindowOpenedOrChanged":{"window":{"id":5,"title":"a","app_id":"x","pid":null,"is_focused":true}}}"#);
        assert_eq!(w.foreground(), None);
        feed(&mut w, r#"{"WorkspaceActivated":{"id":1,"focused":true}}"#);
        assert_eq!(w.foreground(), None);
    }
}
