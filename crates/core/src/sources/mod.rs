pub mod amp;
pub mod antigravity;
pub mod claude;
mod claude_history;
pub mod cline;
pub mod codex;
pub mod copilot;
pub mod copilot_cli;
pub mod cursor;
pub mod gemini;
pub mod hermes;
pub mod opencode;
pub mod pi;
pub mod qwen;
pub mod t3code;
mod snapshot;
mod tail;
mod work;

use std::path::PathBuf;

use serde_json::Value;

use crate::event::Kind;

pub use tail::Tail;

#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub ts: f64,
    pub cwd: Option<PathBuf>,
    pub kind: Kind,
}

pub trait Source: Send {
    fn name(&self) -> &'static str;
    fn harness(&self) -> &'static str {
        self.name()
    }
    /// For a source whose files more than one harness writes.
    fn harness_of(&self, _found: &Found) -> &'static str {
        self.harness()
    }
    fn poll(&mut self) -> Vec<Found>;
    fn state(&self) -> Value;
    fn restore(&mut self, state: &Value);
}

pub fn all() -> Vec<Box<dyn Source>> {
    vec![
        Box::new(claude::Claude::new(claude::default_roots())),
        Box::new(codex::Codex::new(codex::default_roots())),
        Box::new(opencode::OpenCode::new(opencode::default_db())),
        Box::new(opencode::OpenCode::named("kilo_cli", "kilo", opencode::kilo_db())),
        Box::new(antigravity::Antigravity::new(antigravity::default_roots())),
        Box::new(copilot::Copilot::new(copilot::default_roots())),
        Box::new(copilot_cli::CopilotCli::new(copilot_cli::default_roots())),
        Box::new(cursor::Cursor::new(cursor::default_roots())),
        Box::new(cline::cline()),
        Box::new(cline::roo()),
        Box::new(cline::kilo()),
        Box::new(gemini::Gemini::new(gemini::default_root())),
        Box::new(qwen::Qwen::new(qwen::default_roots())),
        Box::new(pi::Pi::new(pi::default_roots())),
        Box::new(hermes::Hermes::new(hermes::default_roots())),
        Box::new(amp::Amp::with_plugin(amp::default_dir(), amp::installed_cli())),
    ]
}

pub(crate) fn words(s: &str) -> usize {
    s.split_whitespace().count()
}

// The words of every string value in `v` (keys are not counted), up to `cap`.
pub(crate) fn string_words(v: &Value, cap: usize) -> usize {
    fn add(v: &Value, cap: usize, n: &mut usize) {
        match v {
            Value::String(s) => *n += s.split_whitespace().take(cap - *n).count(),
            Value::Array(a) => a.iter().for_each(|x| add(x, cap, n)),
            Value::Object(m) => m.values().for_each(|x| add(x, cap, n)),
            _ => {}
        }
    }
    let mut n = 0;
    add(v, cap, &mut n);
    n
}

// `file:///home/u/p`, `file:///c%3A/Users/u/p` or a bare `/c:/Users/u/p` to a local path.
pub(crate) fn uri_path(s: &str) -> Option<PathBuf> {
    let s = match s.strip_prefix("file://") {
        Some(rest) => rest.strip_prefix("localhost").unwrap_or(rest),
        None if s.contains("://") => return None,
        None => s,
    };
    let b = s.as_bytes();
    let mut raw = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'%')
            .then(|| b.get(i + 1..i + 3))
            .flatten()
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(x) => {
                raw.push(x);
                i += 3;
            }
            None => {
                raw.push(b[i]);
                i += 1;
            }
        }
    }
    let path = String::from_utf8(raw).ok()?;
    let p = path.as_bytes();
    let path = if p.len() >= 3 && p[0] == b'/' && p[1].is_ascii_alphabetic() && p[2] == b':' { &path[1..] } else { &path };
    (!path.is_empty()).then(|| PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_become_paths() {
        assert_eq!(uri_path("file:///home/u/my%20proj"), Some(PathBuf::from("/home/u/my proj")));
        assert_eq!(uri_path("file:///c%3A/Users/u/p"), Some(PathBuf::from("c:/Users/u/p")));
        assert_eq!(uri_path("/c:/Users/u/p"), Some(PathBuf::from("c:/Users/u/p")));
        assert_eq!(uri_path("/w/p"), Some(PathBuf::from("/w/p")));
        assert_eq!(uri_path("vscode-remote://wsl+Ubuntu/home/u/p"), None);
        assert_eq!(uri_path("file:///bad%zz"), Some(PathBuf::from("/bad%zz")));
    }
}
