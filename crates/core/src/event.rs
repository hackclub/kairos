//! A device's record of focus time. Hack Club programs read it when they give out prizes, so events come only from what
//! the recorder saw.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::shape::Shape;

pub const VERSION: u32 = 1;
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    AgentTerminal,
    Editor,
    Testing,
    BrowserUnknown,
    Other,
    Terminal,
    AgentApp,
    WebAgent,
    Docs,
    #[serde(other)]
    Unknown,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Class::AgentTerminal => "agent_terminal",
            Class::Editor => "editor",
            Class::Testing => "testing",
            Class::BrowserUnknown => "browser_unknown",
            Class::Other => "other",
            Class::Terminal => "terminal",
            Class::AgentApp => "agent_app",
            Class::WebAgent => "web_agent",
            Class::Docs => "docs",
            Class::Unknown => "unknown",
        }
    }
}

/// Most files a commit or diff event lists, the largest changes first. `files` still counts all.
pub const MAX_CHANGES: usize = 300;
pub const MAX_PATHS: usize = 20;
/// Longer paths are counted but not listed.
pub const MAX_PATH_LEN: usize = 400;

/// Binary files count no lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub added: u32,
    pub removed: u32,
}

/// From git's reflog: the action only, never the message. `Arrived` is a commit made elsewhere (pulled, fast-forwarded, reset or checked out to).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    Commit,
    Amend,
    CherryPick,
    Revert,
    Rebase,
    Arrived,
    #[serde(other)]
    Unknown,
}

/// An agent app whose own log can say a prompt was typed in it. Only these names ever leave the computer, never a window's title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum App {
    ClaudeDesktop,
    CodexApp,
    OpencodeDesktop,
    #[serde(other)]
    Unknown,
}

/// What the agent was waiting on when it asked the person something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ask {
    Question,
    Plan,
    Permission,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    Start {
        version: String,
        os: String,
    },
    Backends {
        focus: String,
        idle: String,
    },
    Prompt {
        words: usize,
        pasted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shape: Option<Shape>,
        /// The harness's own version, where its logs say.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        /// The agent app the harness's log says the person typed this in.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<App>,
    },
    AgentStart {
        session: String,
    },
    ReplyDone {
        session: String,
        reply_words: usize,
        tools: usize,
        files_changed: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// The event's `ts` is when the person answered, `asked` when the agent asked. `shown_words` is how much the agent put in front of the person
    /// and `answer_words` how much they typed back (0 for a choice), counts only like `reply_words`.
    Answered {
        session: String,
        asked: f64,
        why: Ask,
        #[serde(default)]
        shown_words: usize,
        #[serde(default)]
        answer_words: usize,
    },
    Focus {
        class: Class,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app: Option<App>,
    },
    /// `input` is 15 hex digits of a u64 whose bit i is set when there was input in the second [ts - 60 + i, ts - 59 + i). Absent when the backend can't tell.
    Present {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<String>,
    },
    Idle {},
    FileSave {
        count: usize,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        paths: Vec<String>,
    },
    /// `authored` and `committed` are the dates git records, which whoever makes a commit can set to anything.
    Commit {
        sha: String,
        files: u32,
        added: u32,
        removed: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authored: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committed: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<Via>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        changes: Vec<FileChange>,
        /// For an amend, the commit it replaced (HEAD before it, by the reflog): both were made, but only one stays.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replaces: Option<String>,
        /// When kairosd last read HEAD before its reflog recorded this commit being made, so it was made between this and
        /// the event's `ts`. None when that can't be shown (pulled or checked out, or kairosd lost track of the reflog).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        not_before: Option<f64>,
    },
    Diff {
        files: u32,
        added: u32,
        removed: u32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        changes: Vec<FileChange>,
    },
    /// Never credited: reviewers compare commits against these.
    AgentEdit {
        session: String,
        path: String,
        added: u32,
        removed: u32,
    },
    /// No sha when the command printed nothing (`git commit -q`): the server matches it by time.
    AgentCommit {
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha: Option<String>,
    },
    /// `at` is when the editor saw the activity (later than the event's `ts` for heartbeats the plugin queued while kairosd was down).
    /// Adds no time on its own: a hand-typed save vouches for editor time while an agent runs, and credit_v8 takes an editor
    /// window's project from the heartbeats sent while it was in front.
    Heartbeat {
        at: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        editor: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        category: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        write: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ai_lines: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        human_lines: Option<i64>,
    },
    /// An "ai coding" heartbeat wakatime-cli made from an agent's log. `sent` ones came through the relay and went on to Hackatime. The
    /// others come from kairosd's own run of the person's wakatime-cli with AI sync on, which sends nothing anywhere: what Hackatime
    /// would get with AI sync on. `agent` is the agent's name from the heartbeat's user agent, `path` is relative to the project (none
    /// for heartbeats about the agent itself rather than a file). Never credited.
    AiHeartbeat {
        at: f64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        sent: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        write: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ai_lines: Option<i64>,
    },
    /// How Hackatime is set up on this computer, written at start and when it changes. `relay`: Kairos holds the WakaTime config, so
    /// editor heartbeats come through it. `cli`: the version of the person's wakatime-cli, none when there is none. `shadow`:
    /// kairosd can run it with AI sync on (it has the API port, there is a wakatime-cli and its last run worked). `ai_sync`: whether the person's own
    /// config leaves AI sync on, none without a config.
    WakatimeSetup {
        relay: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cli: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        shadow: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ai_sync: Option<bool>,
    },
    /// The project root `from` now resolves to the event's project (a remote was added or the repo renamed), so the server can keep the earlier time with it.
    ProjectAlias {
        from: String,
    },
    Pause {
        until: f64,
    },
    Resume {},
    #[serde(other)]
    Unknown,
}

pub const INPUT_WINDOW_SECS: f64 = 60.0;

/// Which of the 60 seconds before `ts` had input, from the unix seconds of the inputs.
pub fn input_bits(ts: f64, inputs: &[f64]) -> String {
    let start = ts - INPUT_WINDOW_SECS;
    let mut bits = 0u64;
    for &t in inputs {
        let i = (t - start).floor();
        if (0.0..INPUT_WINDOW_SECS).contains(&i) {
            bits |= 1 << i as u32;
        }
    }
    format!("{bits:015x}")
}

impl Kind {
    pub fn name(&self) -> &'static str {
        match self {
            Kind::Start { .. } => "start",
            Kind::Backends { .. } => "backends",
            Kind::Prompt { .. } => "prompt",
            Kind::AgentStart { .. } => "agent_start",
            Kind::ReplyDone { .. } => "reply_done",
            Kind::Answered { .. } => "answered",
            Kind::Focus { .. } => "focus",
            Kind::Present { .. } => "present",
            Kind::Idle {} => "idle",
            Kind::FileSave { .. } => "file_save",
            Kind::Commit { .. } => "commit",
            Kind::Diff { .. } => "diff",
            Kind::AgentEdit { .. } => "agent_edit",
            Kind::AgentCommit { .. } => "agent_commit",
            Kind::Heartbeat { .. } => "heartbeat",
            Kind::AiHeartbeat { .. } => "ai_heartbeat",
            Kind::WakatimeSetup { .. } => "wakatime_setup",
            Kind::ProjectAlias { .. } => "project_alias",
            Kind::Pause { .. } => "pause",
            Kind::Resume {} => "resume",
            Kind::Unknown => "unknown",
        }
    }

    /// The agent session an agent's event belongs to.
    pub fn session(&self) -> Option<&str> {
        match self {
            Kind::Prompt { session, .. }
            | Kind::AgentStart { session }
            | Kind::ReplyDone { session, .. }
            | Kind::Answered { session, .. }
            | Kind::AgentEdit { session, .. }
            | Kind::AgentCommit { session, .. } => Some(session),
            _ => None,
        }
    }

    /// Something the person or their agents did, not a fact recorded about a project (commits, diffs, renames, what an agent's tool calls changed) or a kind this version doesn't know.
    pub fn is_activity(&self) -> bool {
        matches!(
            self,
            Kind::Prompt { .. }
                | Kind::AgentStart { .. }
                | Kind::ReplyDone { .. }
                | Kind::Answered { .. }
                | Kind::Focus { .. }
                | Kind::Present { .. }
                | Kind::Idle {}
                | Kind::FileSave { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub v: u32,
    pub ts: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(flatten)]
    pub kind: Kind,
    #[serde(default)]
    pub prev: String,
    #[serde(default)]
    pub hash: String,
}

impl Event {
    pub fn new(ts: f64, project: Option<String>, kind: Kind) -> Event {
        Event { v: VERSION, ts, project, harness: None, kind, prev: String::new(), hash: String::new() }
    }

    pub fn seal(&mut self, prev: &str) {
        self.prev = prev.to_string();
        self.hash = String::new();
        let mut body = serde_json::to_value(&*self).expect("event serializes");
        // The server can't store a NUL, and a chain it refuses one event of can never upload again.
        if strip_nul(&mut body) {
            *self = serde_json::from_value(body.clone()).expect("event deserializes");
        }
        if let Value::Object(m) = &mut body {
            m.remove("hash");
        }
        self.hash = digest(prev, &body);
    }
}

fn strip_nul(v: &mut Value) -> bool {
    match v {
        Value::String(s) if s.contains('\0') => {
            s.retain(|c| c != '\0');
            true
        }
        Value::Array(a) => a.iter_mut().fold(false, |any, x| strip_nul(x) | any),
        Value::Object(m) => m.values_mut().fold(false, |any, x| strip_nul(x) | any),
        _ => false,
    }
}

fn canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                canonical(&m[k], out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(x, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

fn digest(prev: &str, body: &Value) -> String {
    let mut text = String::new();
    canonical(body, &mut text);
    let mut h = blake3::Hasher::new();
    h.update(prev.as_bytes());
    h.update(text.as_bytes());
    h.finalize().to_hex().to_string()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Verified {
    pub events: usize,
    pub last_hash: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Broken {
    pub line: usize,
    pub reason: String,
}

pub fn verify_chain<'a>(
    lines: impl Iterator<Item = &'a str>,
    anchor: Option<&str>,
) -> Result<Verified, Broken> {
    let mut expected: Option<String> = anchor.map(str::to_string);
    let mut events = 0;
    for (i, line) in lines.enumerate() {
        let line_no = i + 1;
        if line.trim().is_empty() {
            continue;
        }
        let broken = |reason: &str| Broken { line: line_no, reason: reason.to_string() };
        let mut body: Value = serde_json::from_str(line).map_err(|_| broken("not valid JSON"))?;
        let Value::Object(m) = &mut body else {
            return Err(broken("not a JSON object"));
        };
        let hash = match m.remove("hash") {
            Some(Value::String(h)) => h,
            _ => return Err(broken("missing hash")),
        };
        let prev = match m.get("prev") {
            Some(Value::String(p)) => p.clone(),
            _ => return Err(broken("missing prev")),
        };
        if let Some(exp) = &expected {
            if &prev != exp {
                return Err(broken("prev does not match the previous event's hash"));
            }
        }
        if digest(&prev, &body) != hash {
            return Err(broken("hash does not match the event contents"));
        }
        expected = Some(hash);
        events += 1;
    }
    Ok(Verified { events, last_hash: expected.unwrap_or_else(|| GENESIS.to_string()) })
}

pub fn load_events(path: &Path) -> std::io::Result<Vec<Event>> {
    let text = fs::read_to_string(path)?;
    Ok(text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
}

pub fn log_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files: Vec<_> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("events-") && n.ends_with(".jsonl"))
        })
        .collect();
    files.sort();
    files
}
