use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Map, Value};

use super::{uri_path, work, words, Found, Source, Tail};
use crate::event::Kind;
use crate::{prompt_shape, prompt_stats};

const TRANSCRIPT: &str = ".system_generated/logs/transcript.jsonl";
const PSEUDO_TOOLS: [&str; 4] = ["finish", "manage_task", "schedule", "send_message"];
const HISTORY_TAIL: u64 = 512 * 1024;
const HISTORY_SLACK: f64 = 3.0;
// Steps wait this long before they are read, so the prompt history and subagent markers that are
// written just after a transcript line are there when it is classified.
const SETTLE: f64 = 5.0;
// Set on a line that showed up while the reader was already watching its file, to when it was read.
const SEEN: &str = "_kairos_seen";

// The 1.x IDE's encrypted .pb conversations are skipped; it only gets transcripts once it moves to the 2.0 format.
pub fn default_roots() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    ["antigravity", "antigravity-cli", "antigravity-ide"].iter().map(|p| home.join(".gemini").join(p)).collect()
}

pub(crate) fn list(root: &Path, files: &mut Vec<PathBuf>) {
    for e in fs::read_dir(root.join("brain")).into_iter().flatten().flatten() {
        let p = e.path().join(TRANSCRIPT);
        if p.is_file() {
            files.push(p);
        }
    }
}

pub(crate) fn ts(e: &Value) -> Option<f64> {
    let s = e.get("created_at")?.as_str()?;
    let d = chrono::DateTime::parse_from_rfc3339(s).ok()?;
    Some(d.timestamp_millis() as f64 / 1000.0)
}

fn request(content: &str) -> &str {
    let Some(i) = content.find("<USER_REQUEST>") else { return content };
    let rest = &content[i + "<USER_REQUEST>".len()..];
    let end = rest.find("</USER_REQUEST>").or_else(|| rest.find("<ADDITIONAL_METADATA>")).unwrap_or(rest.len());
    &rest[..end]
}

// "The user changed setting `Model Selection` from A to B. ..."
pub(crate) fn selected_model(content: &str) -> Option<String> {
    let rest = &content[content.find("`Model Selection`")?..];
    let rest = &rest[rest.find(" to ")? + 4..];
    let end = [rest.find(". "), rest.find('\n')].into_iter().flatten().min().unwrap_or(rest.len());
    let m = rest[..end].trim().trim_end_matches('.');
    (!m.is_empty()).then(|| m.to_string())
}

struct Summary {
    workspace: Option<PathBuf>,
    parent: bool,
}

fn summary(root: &Path, id: &str) -> Option<Summary> {
    let db = root.join("conversation_summaries.db");
    if !db.is_file() {
        return None;
    }
    let c = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;
    c.busy_timeout(Duration::from_millis(500)).ok()?;
    let (uris, parent): (Option<String>, Option<String>) = c
        .query_row(
            "SELECT workspace_uris, parent_conversation_id FROM conversation_summaries WHERE conversation_id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()?;
    let uris: Vec<String> = uris.and_then(|u| serde_json::from_str(&u).ok()).unwrap_or_default();
    Some(Summary {
        workspace: uris.iter().find_map(|u| uri_path(u)),
        parent: parent.is_some_and(|p| !p.is_empty()),
    })
}

fn spawned(root: &Path, id: &str) -> bool {
    let name = format!("{id}.json");
    fs::read_dir(root.join("brain"))
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.path().join(".system_generated/subagents").join(&name).is_file())
}

fn workspace_from_cache(root: &Path, id: &str) -> Option<PathBuf> {
    let text = fs::read_to_string(root.join("cache/last_conversations.json")).ok()?;
    let map: Map<String, Value> = serde_json::from_str(&text).ok()?;
    map.iter().find(|(_, v)| v.as_str() == Some(id)).and_then(|(k, _)| uri_path(k))
}

fn history(root: &Path) -> Option<Vec<Value>> {
    let mut f = fs::File::open(root.join("history.jsonl")).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(HISTORY_TAIL))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
}

fn workspace_from_history(root: &Path, id: &str) -> Option<PathBuf> {
    history(root)?.iter().rev().find_map(|row| {
        if row.get("conversationId").and_then(Value::as_str) != Some(id) {
            return None;
        }
        row.get("workspace").and_then(Value::as_str).and_then(uri_path)
    })
}

// The folder a conversation was opened in, from the same three places `Conv::refresh` looks.
pub(crate) fn workspace(root: &Path, id: &str) -> Option<PathBuf> {
    summary(root, id).and_then(|s| s.workspace).or_else(|| workspace_from_cache(root, id)).or_else(|| workspace_from_history(root, id))
}

// `agy -p` runs never reach the CLI's prompt history; prompts typed into the TUI land there within a second or two.
fn typed(root: &Path, ts: f64) -> bool {
    if root.as_os_str().is_empty() || !root.join("history.jsonl").is_file() {
        return true;
    }
    history(root).into_iter().flatten().any(|row| {
        row.get("timestamp").and_then(Value::as_f64).is_some_and(|t| (t / 1000.0 - ts).abs() <= HISTORY_SLACK)
    })
}

// Each run logs "Language server version: X" near its top to cli.log (a link to its newest run's log). The apps run agy
// too since 2.x; before that the app logged to language_server.log in its settings folder. That is the version running
// when the prompt is read, the prompt's own unless it waited in the transcript for kairosd to start.
fn version(root: &Path) -> Option<String> {
    const MARK: &str = "Language server version: ";
    let mut logs = vec![root.join("cli.log")];
    match root.file_name()?.to_str()? {
        "antigravity-cli" => {}
        "antigravity" => logs.extend(dirs::config_dir().map(|c| c.join("Antigravity/logs/language_server.log"))),
        _ => return None,
    }
    logs.into_iter().find_map(|log| {
        let mut head = Vec::new();
        fs::File::open(log).ok()?.take(4096).read_to_end(&mut head).ok()?;
        let text = String::from_utf8_lossy(&head);
        text[text.find(MARK)? + MARK.len()..].split_whitespace().next().map(str::to_string)
    })
}

#[derive(Default)]
struct Conv {
    known: bool,
    root: PathBuf,
    id: String,
    sub: bool,
    cwd: Option<PathBuf>,
    model: Option<String>,
    armed: bool,
    open: bool,
    last_ts: f64,
    reply_words: usize,
    tools: usize,
    files: BTreeSet<String>,
    turn_model: Option<String>,
    pending: Vec<Value>,
    // From the latest planner response until the steps that answer it arrive: the files it asked to write, with the
    // lines a write adds when that's known, and its commands.
    writes: VecDeque<(String, Option<u32>)>,
    commands: VecDeque<String>,
    // Subagents this turn started that haven't messaged back yet. The parent stops while they work and a message from
    // one wakes it, so the turn goes on until they all have.
    waiting: BTreeSet<String>,
}

impl Conv {
    fn init(&mut self, path: &Path) {
        self.known = true;
        let mut up = path.ancestors();
        self.id = up.nth(3).and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.root = up.nth(1).map(Path::to_path_buf).unwrap_or_default();
        self.refresh();
    }

    // Subagents are their own conversations whose first USER_INPUT is the parent's delegated task.
    // The markers can appear a moment after the transcript, so look again before every prompt.
    fn refresh(&mut self) {
        if self.root.as_os_str().is_empty() || self.sub {
            return;
        }
        let s = summary(&self.root, &self.id);
        self.sub = s.as_ref().is_some_and(|s| s.parent) || spawned(&self.root, &self.id);
        if self.cwd.is_none() {
            self.cwd = s
                .and_then(|s| s.workspace)
                .or_else(|| workspace_from_cache(&self.root, &self.id))
                .or_else(|| workspace_from_history(&self.root, &self.id));
        }
    }

    fn close(&mut self, ts: f64, out: &mut Vec<Found>) {
        if !self.open {
            return;
        }
        self.open = false;
        self.armed = false;
        out.push(Found {
            ts,
            cwd: self.cwd.clone(),
            kind: Kind::ReplyDone {
                session: self.id.clone(),
                reply_words: self.reply_words,
                tools: self.tools,
                files_changed: self.files.len(),
                model: self.turn_model.clone().or_else(|| self.model.clone()),
            },
        });
    }

    // Projects for conversations no index knows: the first working directory or absolute path a tool names.
    fn guess_cwd(&mut self, call: &Value) {
        if self.cwd.is_some() {
            return;
        }
        let arg = |k: &str| arg(call, k).as_deref().and_then(uri_path);
        let found = arg("Cwd").or_else(|| arg("DirectoryPath")).or_else(|| {
            ["TargetFile", "AbsolutePath", "Path"].iter().find_map(|k| arg(k)).and_then(|p| p.parent().map(Path::to_path_buf))
        });
        if let Some(p) = found.filter(|p| p.is_absolute() && !p.components().any(|c| c.as_os_str() == ".gemini")) {
            self.cwd = Some(p);
        }
    }
}

const DIFF_FOR: &str = "The following changes were made by the ";

// A tool call's argument. The 2.x app and the 1.5 CLI write each one as JSON, so a string arrives quoted.
fn arg(call: &Value, k: &str) -> Option<String> {
    let raw = call.get("args")?.get(k)?.as_str()?;
    Some(if raw.starts_with('"') { serde_json::from_str(raw).unwrap_or_else(|_| raw.to_string()) } else { raw.to_string() })
}

// "Made 1 replacement in /w/p/a.rs starting at line 4 (+4/-0 lines)."
fn replaced(line: &str) -> Option<(&str, (u32, u32))> {
    let rest = line.strip_prefix("Made ")?;
    let (_, rest) = rest.split_once(" in ")?;
    let (path, rest) = rest.rsplit_once(" starting at line ")?;
    let (_, counts) = rest.split_once("(+")?;
    let (added, rest) = counts.split_once("/-")?;
    let (removed, _) = rest.split_once(" lines)")?;
    Some((path, (added.parse().ok()?, removed.parse().ok()?)))
}

// A step that edits a file shows the diff; a new file's content, and every command line, only appear in the planner response
// before the step. The 2.x app and the 1.5 CLI answer every tool with a GENERIC step: an edit says where and how many lines,
// a write and a command only that they happened, in the order they were asked for.
fn agent_work(c: &mut Conv, e: &Value, ts: f64, out: &mut Vec<Found>) {
    let content = e.get("content").and_then(Value::as_str).unwrap_or("");
    match e.get("type").and_then(Value::as_str) {
        Some("PLANNER_RESPONSE") => {
            c.writes.clear();
            c.commands.clear();
            for call in e.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                let arg = |k: &str| arg(call, k);
                let set = |k: &str| arg(k).is_some_and(|v| v == "true");
                let write = |file: Option<String>, body: Option<String>, lines: &dyn Fn(u32) -> Option<u32>| {
                    let file = file.as_deref().and_then(uri_path)?;
                    Some((file.to_string_lossy().into_owned(), lines(work::line_count(&body?))))
                };
                let pending = match call.get("name").and_then(Value::as_str) {
                    // The IDE's write_to_file overwrites unless told not to, the newer write_file only when told to.
                    Some("write_to_file") => {
                        let overwrite = arg("Overwrite").is_none_or(|o| o != "false");
                        write(arg("TargetFile"), arg("CodeContent"), &|n| (!overwrite).then_some(n))
                    }
                    Some("write_file") => {
                        let overwrite = set("Overwrite");
                        write(arg("Path"), arg("Content"), &|n| (!overwrite).then_some(n))
                    }
                    Some("run_command") => {
                        let line = arg("CommandLine").unwrap_or_default();
                        let cmd = match arg("Cwd") {
                            Some(dir) => format!("cd {} && {line}", work::shell_join([dir.as_str()])),
                            None => line,
                        };
                        c.commands.push_back(cmd);
                        None
                    }
                    _ => None,
                };
                c.writes.extend(pending);
            }
        }
        Some("CODE_ACTION") => {
            let mut lines = content.lines();
            while let Some(l) = lines.next() {
                if let Some(rest) = l.strip_prefix(DIFF_FOR) {
                    let Some((_, path)) = rest.split_once(" tool to: ") else { return };
                    let diff: Vec<&str> = lines.by_ref().skip_while(|l| !l.contains("[diff_block_start]")).skip(1).take_while(|l| !l.contains("[diff_block_end]")).collect();
                    work::edit(ts, c.cwd.as_deref(), &c.id, path.trim(), work::hunk_counts(diff.into_iter().skip_while(|l| !l.starts_with("@@"))), out);
                    return;
                }
                if let Some(rest) = l.strip_prefix("Created file ") {
                    created(c, rest, ts, out);
                    return;
                }
            }
        }
        Some("RUN_COMMAND") => {
            if let Some(cmd) = c.commands.pop_front().filter(|cmd| cmd.contains("commit")) {
                work::commits(ts, c.cwd.as_deref(), &c.id, &cmd, content, out);
            }
        }
        Some("GENERIC") => {
            let Some(first) = content.lines().find(|l| !l.starts_with("Created At: ") && !l.starts_with("Completed At: ")) else { return };
            if first.starts_with("Created the following subagent:") {
                if let Some(id) = content.split("\"conversationId\": \"").nth(1).and_then(|r| r.split('"').next()) {
                    c.waiting.insert(id.to_string());
                }
            } else if let Some((path, counts)) = replaced(first) {
                work::edit(ts, c.cwd.as_deref(), &c.id, path, counts, out);
            } else if let Some(rest) = first.strip_prefix("Created file") {
                created(c, rest.trim(), ts, out);
            } else if first.starts_with("Command exited with code") {
                if let Some(cmd) = c.commands.pop_front().filter(|cmd| cmd.contains("commit")) {
                    work::commits(ts, c.cwd.as_deref(), &c.id, &cmd, content, out);
                }
            }
        }
        _ => {}
    }
}

// A write is done: "Created file file:///w/p/a.rs" names it, a bare "Created file." is the next one asked for.
fn created(c: &mut Conv, rest: &str, ts: f64, out: &mut Vec<Found>) {
    let rest = rest.trim().trim_end_matches('.');
    let at = if rest.is_empty() {
        (!c.writes.is_empty()).then_some(0)
    } else {
        let named = rest.split_whitespace().next().and_then(uri_path).map(|p| p.to_string_lossy().into_owned());
        named.and_then(|path| c.writes.iter().position(|w| w.0 == path))
    };
    if let Some((path, Some(n))) = at.and_then(|i| c.writes.remove(i)) {
        work::edit(ts, c.cwd.as_deref(), &c.id, &path, (n, 0), out);
    }
}

fn feed(c: &mut Conv, e: &Value, out: &mut Vec<Found>) {
    let Some(created) = ts(e) else { return };
    let source = e.get("source").and_then(Value::as_str).unwrap_or("");
    let ty = e.get("type").and_then(Value::as_str).unwrap_or("");
    // A model step is stamped with when it began, but its line is written once the model is done with it, which for a
    // long answer is much later. When the reader saw the line arrive, that is closer to when the reply was done.
    let seen = (source == "MODEL" && ty == "PLANNER_RESPONSE").then(|| e.get(SEEN).and_then(Value::as_f64)).flatten();
    let ts = seen.map_or(created, |s| s.max(created));
    let done = e.get("status").and_then(Value::as_str) == Some("DONE");
    if source == "MODEL" && done {
        agent_work(c, e, ts, out);
    }
    if source == "USER_EXPLICIT" && ty == "USER_INPUT" {
        if !done {
            return;
        }
        c.refresh();
        let last = c.last_ts;
        c.close(last.min(ts), out);
        c.waiting.clear();
        if c.sub || !typed(&c.root, ts) {
            return;
        }
        let content = e.get("content").and_then(Value::as_str).unwrap_or("");
        if let Some(m) = selected_model(content) {
            c.model = Some(m);
        }
        let (words, pasted) = prompt_stats(request(content));
        let shape = Some(prompt_shape(request(content)));
        if words == 0 {
            return;
        }
        c.armed = true;
        c.open = true;
        c.last_ts = ts;
        c.reply_words = 0;
        c.tools = 0;
        c.files.clear();
        c.turn_model = None;
        out.push(Found {
            ts,
            cwd: c.cwd.clone(),
            kind: Kind::Prompt { words, pasted, model: c.model.clone(), session: c.id.clone(), shape, version: version(&c.root), via: None, permissions: None },
        });
        return;
    }
    if source == "SYSTEM" && ty == "SYSTEM_MESSAGE" {
        let content = e.get("content").and_then(Value::as_str).unwrap_or("");
        for sender in content.split("[Message] ").skip(1).filter_map(|m| m.split(" sender=").nth(1)?.split_whitespace().next()) {
            c.waiting.remove(sender);
        }
        return;
    }
    if !c.open || source != "MODEL" {
        return;
    }
    c.last_ts = ts;
    if ty != "PLANNER_RESPONSE" {
        return;
    }
    if let Some(m) = e.get("model").and_then(Value::as_str).filter(|m| !m.is_empty()) {
        c.turn_model = Some(m.to_string());
    }
    c.reply_words += e.get("content").and_then(Value::as_str).map_or(0, words);
    let calls: Vec<&Value> = e.get("tool_calls").and_then(Value::as_array).into_iter().flatten().collect();
    let mut finished = calls.is_empty();
    for call in calls {
        let name = call.get("name").and_then(Value::as_str).unwrap_or("");
        c.guess_cwd(call);
        if name == "finish" {
            finished = true;
        }
        if PSEUDO_TOOLS.contains(&name) {
            continue;
        }
        if c.armed {
            c.armed = false;
            out.push(Found { ts, cwd: c.cwd.clone(), kind: Kind::AgentStart { session: c.id.clone() } });
        }
        c.tools += 1;
        let key = match name {
            "write_to_file" | "replace_file_content" | "multi_replace_file_content" => "TargetFile",
            "edit_file" | "write_file" => "Path",
            _ => continue,
        };
        if let Some(p) = arg(call, key) {
            c.files.insert(p);
        }
    }
    if finished && done && c.waiting.is_empty() {
        c.close(ts, out);
    }
}

pub struct Antigravity {
    tail: Tail,
    convs: HashMap<PathBuf, Conv>,
    models: HashMap<PathBuf, String>,
    watching: bool,
}

impl Antigravity {
    pub fn new(roots: Vec<PathBuf>) -> Antigravity {
        Antigravity { tail: Tail::with(roots, list), convs: HashMap::new(), models: HashMap::new(), watching: false }
    }
}

impl Source for Antigravity {
    fn health(&mut self) -> super::Health {
        self.tail.health()
    }

    fn name(&self) -> &'static str {
        "antigravity"
    }

    fn poll(&mut self) -> Vec<Found> {
        let convs = &mut self.convs;
        // Lines from the first poll were written any time while kairosd was not running.
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let seen = self.watching.then_some(now);
        self.tail.poll(|path, line| {
            if let Ok(mut e) = serde_json::from_str::<Value>(line) {
                if let (Some(seen), Some(o)) = (seen, e.as_object_mut()) {
                    o.insert(SEEN.to_string(), json!(seen));
                }
                convs.entry(path.to_path_buf()).or_default().pending.push(e);
            }
        });
        self.watching = true;
        let ready = now - SETTLE;
        let mut out = Vec::new();
        for (path, c) in self.convs.iter_mut().filter(|(_, c)| !c.pending.is_empty()) {
            if !c.known {
                c.model = self.models.get(path).cloned();
                c.init(path);
            }
            c.pending.sort_by_key(|e| e.get("step_index").and_then(Value::as_i64).unwrap_or(i64::MAX));
            // Several lines read at once can't have been done after the step that follows them began.
            for i in 1..c.pending.len() {
                if let (Some(next), Some(seen)) = (ts(&c.pending[i]), c.pending[i - 1].get(SEEN).and_then(Value::as_f64)) {
                    c.pending[i - 1][SEEN] = json!(seen.min(next));
                }
            }
            let n = c.pending.iter().position(|e| ts(e).is_some_and(|t| t > ready)).unwrap_or(c.pending.len());
            let steps: Vec<Value> = c.pending.drain(..n).collect();
            for e in &steps {
                feed(c, e, &mut out);
            }
            if let Some(m) = &c.model {
                self.models.insert(path.clone(), m.clone());
            }
        }
        self.convs.retain(|p, _| p.exists());
        self.models.retain(|p, _| p.exists());
        out.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        out
    }

    fn state(&self) -> Value {
        let models: Map<String, Value> =
            self.models.iter().map(|(p, m)| (p.to_string_lossy().into_owned(), json!(m))).collect();
        let mut state = self.tail.state();
        state["models"] = Value::Object(models);
        state
    }

    fn restore(&mut self, state: &Value) {
        self.tail.restore(state);
        for (p, m) in state.get("models").and_then(Value::as_object).into_iter().flatten() {
            if let Some(m) = m.as_str() {
                self.models.insert(PathBuf::from(p), m.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lines: &[&str]) -> Vec<Found> {
        let mut c = Conv { id: "c1".into(), cwd: Some(PathBuf::from("/w/p")), ..Conv::default() };
        let mut out = Vec::new();
        for l in lines {
            feed(&mut c, &serde_json::from_str(l).unwrap(), &mut out);
        }
        out
    }

    fn names(out: &[Found]) -> Vec<&'static str> {
        out.iter().map(|f| f.kind.name()).collect()
    }

    const USER: &str = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-19T10:00:00Z","content":"<USER_REQUEST>\nplease fix the failing parser test now\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-11-19T12:00:00+02:00.\n</ADDITIONAL_METADATA>\n<USER_SETTINGS_CHANGE>\nThe user changed setting `Model Selection` from Gemini 3.6 Flash to Gemini 3.8 Flash (High). Use it from now on.\n</USER_SETTINGS_CHANGE>"}"#;
    const SYS: &str = r#"{"step_index":1,"source":"SYSTEM","type":"EPHEMERAL_MESSAGE","status":"DONE","created_at":"2026-11-19T10:00:01Z","content":"background note"}"#;
    const PLAN: &str = r#"{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-19T10:00:05Z","content":"Looking at it.","thinking":"lots of private thoughts here","tool_calls":[{"name":"view_file","args":{"AbsolutePath":"/w/p/a.rs"}},{"name":"replace_file_content","args":{"TargetFile":"/w/p/a.rs"}},{"name":"manage_task","args":{}}]}"#;
    const VIEW: &str = r#"{"step_index":3,"source":"MODEL","type":"VIEW_FILE","status":"DONE","created_at":"2026-11-19T10:00:06Z"}"#;
    const DONE: &str = r#"{"step_index":4,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-19T10:01:00Z","content":"Fixed it in a.rs.","model":"gemini-3.8-flash-high"}"#;

    #[test]
    fn a_turn_becomes_prompt_start_done() {
        let out = run(&[USER, SYS, PLAN, VIEW, DONE]);
        assert_eq!(names(&out), ["prompt", "agent_start", "reply_done"]);
        assert_eq!(out[0].cwd.as_deref(), Some(Path::new("/w/p")));
        match &out[0].kind {
            Kind::Prompt { words, pasted, model, session, shape, .. } => {
                assert_eq!((*words, *pasted, session.as_str()), (7, false, "c1"));
                assert_eq!(shape.as_ref().map(|s| (s.lines, s.sentences)), Some((1, 1)));
                assert_eq!(model.as_deref(), Some("Gemini 3.8 Flash (High)"));
            }
            k => panic!("{k:?}"),
        }
        assert_eq!(out[1].ts - out[0].ts, 5.0);
        match &out[2].kind {
            Kind::ReplyDone { reply_words, tools, files_changed, model, .. } => {
                assert_eq!((*reply_words, *tools, *files_changed), (7, 2, 1));
                assert_eq!(model.as_deref(), Some("gemini-3.8-flash-high"));
            }
            k => panic!("{k:?}"),
        }
        assert_eq!(out[2].ts - out[0].ts, 60.0);
    }

    #[test]
    fn finish_tool_ends_the_turn() {
        let fin = DONE.replace(r#""model":"gemini-3.8-flash-high""#, r#""tool_calls":[{"name":"finish","args":{}}]"#);
        let out = run(&[USER, PLAN, &fin]);
        assert_eq!(names(&out), ["prompt", "agent_start", "reply_done"]);
        assert!(matches!(&out[2].kind, Kind::ReplyDone { tools: 2, .. }));
    }

    #[test]
    fn prompt_typed_mid_turn_closes_the_running_turn() {
        let next = USER.replace(r#""step_index":0"#, r#""step_index":5"#).replace("10:00:00", "10:00:30");
        let out = run(&[USER, PLAN, VIEW, &next]);
        assert_eq!(names(&out), ["prompt", "agent_start", "reply_done", "prompt"]);
        assert_eq!(out[2].ts, out[0].ts + 6.0);
    }

    #[test]
    fn interrupted_and_running_steps_never_close_a_turn() {
        let running = DONE.replace(r#""status":"DONE""#, r#""status":"RUNNING""#);
        let out = run(&[USER, PLAN, &running]);
        assert_eq!(names(&out), ["prompt", "agent_start"]);
    }

    #[test]
    fn system_lines_and_bare_metadata_are_not_prompts() {
        let meta = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-19T10:00:00Z","content":"<USER_REQUEST>\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nlocal time\n</ADDITIONAL_METADATA>"}"#;
        assert!(run(&[SYS, meta, PLAN, DONE]).is_empty());
    }

    fn write(p: &Path, text: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    #[test]
    fn the_ide_reads_its_version_from_agys_log() {
        let dir = std::env::temp_dir().join(format!("kairos-agy-version-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("antigravity");
        write(&root.join("cli.log"), "Log file created at: 2026/11/25 20:06:29\nI1125 20:06:29.120000      12 server.go:2711] Language server version: 2.35.1\n");
        assert_eq!(version(&root).as_deref(), Some("2.35.1"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_appends_skips_subagents_headless_runs_and_partial_lines_and_resumes() {
        let dir = std::env::temp_dir().join(format!("kairos-agy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("antigravity-cli");
        let main = root.join("brain/c1").join(TRANSCRIPT);
        let sub = root.join("brain/s1").join(TRANSCRIPT);
        write(&main, &format!("{USER}\n"));
        write(&root.join("cache/last_conversations.json"), r#"{"/w/p":"c1"}"#);
        write(&root.join("cli.log"), "Log file created at: 2026/11/22 20:32:04\nI1122 20:32:04.585758      66 server.go:2711] Language server version: 1.5.14\n");
        let mut src = Antigravity::new(vec![root.clone()]);
        assert!(src.poll().is_empty());

        let p2 = USER.replace(r#""step_index":0"#, r#""step_index":5"#).replace("10:00:00", "10:05:00");
        let plan = PLAN.replace(r#""step_index":2"#, r#""step_index":6"#).replace("10:00:05", "10:05:05");
        let done = DONE.replace(r#""step_index":4"#, r#""step_index":7"#).replace("10:01:00", "10:06:00");
        let half = &done[..40];
        write(&main, &format!("{USER}\n{p2}\n{plan}\n{half}"));
        write(&root.join("brain/c1/.system_generated/subagents/s1.json"), r#"{"conversationId":"s1"}"#);
        write(&sub, &format!("{USER}\n{PLAN}\n{DONE}\n"));
        let headless = p2.replace("10:05:00", "10:07:00");
        write(&root.join("brain/h1").join(TRANSCRIPT), &format!("{headless}\n{plan}\n{done}\n"));
        write(
            &root.join("history.jsonl"),
            "{\"display\":\"x\",\"timestamp\":1795082701000,\"workspace\":\"/w/p\",\"conversationId\":\"c1\"}\n\
             {\"display\":\"y\",\"timestamp\":1795082940000,\"workspace\":\"/w/p\"}\n",
        );
        let out = src.poll();
        assert_eq!(names(&out), ["prompt", "agent_start"]);
        assert_eq!(out[0].cwd.as_deref(), Some(Path::new("/w/p")));
        assert!(matches!(&out[0].kind, Kind::Prompt { session, version: Some(v), .. } if session == "c1" && v == "1.5.14"));

        write(&main, &format!("{USER}\n{p2}\n{plan}\n{done}\n"));
        let out = src.poll();
        assert_eq!(names(&out), ["reply_done"]);
        assert!(matches!(&out[0].kind, Kind::ReplyDone { model: Some(m), .. } if m == "gemini-3.8-flash-high"));

        let mut again = Antigravity::new(vec![root.clone()]);
        again.restore(&src.state());
        let p3 = r#"{"step_index":9,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-19T10:09:00Z","content":"<USER_REQUEST>now add a test</USER_REQUEST>"}"#;
        write(&main, &format!("{USER}\n{p2}\n{plan}\n{done}\n{p3}\n"));
        let out = again.poll();
        assert_eq!(names(&out), ["prompt"]);
        assert!(matches!(&out[0].kind, Kind::Prompt { words: 4, model: Some(m), .. } if m == "Gemini 3.8 Flash (High)"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reply_read_as_it_arrives_is_done_when_its_line_showed_up() {
        let dir = std::env::temp_dir().join(format!("kairos-agy-seen-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("antigravity-cli");
        let main = root.join("brain/c1").join(TRANSCRIPT);
        write(&main, "");
        write(&root.join("cache/last_conversations.json"), r#"{"/w/p":"c1"}"#);
        let mut src = Antigravity::new(vec![root.clone()]);
        assert!(src.poll().is_empty());
        let now = chrono::Utc::now().timestamp() as f64;
        let at = |ago: f64| chrono::DateTime::from_timestamp((now - ago) as i64, 0).unwrap().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let user = USER.replace("2026-11-19T10:00:00Z", &at(120.0));
        let plan = PLAN.replace("2026-11-19T10:00:05Z", &at(115.0));
        // The model began its closing answer 100 s ago, and the line is written once the answer is done.
        let done = DONE.replace("2026-11-19T10:01:00Z", &at(100.0));
        write(&main, &format!("{user}\n{plan}\n{done}\n"));
        let out = src.poll();
        assert_eq!(names(&out), ["prompt", "agent_start", "reply_done"]);
        assert!(out[2].ts >= now - 2.0, "reply_done at {} for a line read at {now}", out[2].ts);
        // Read in the same poll, the step after it began at once, so that's as late as the answer can have been done.
        assert_eq!(out[1].ts, now - 100.0);

        // On the first poll after a start the lines could have been written any time kairosd was not running.
        let mut fresh = Antigravity::new(vec![root.clone()]);
        fresh.restore(&json!({ "since": now - 200.0, "files": { (main.to_string_lossy().to_string()): 0 } }));
        let out = fresh.poll();
        assert_eq!(names(&out), ["prompt", "agent_start", "reply_done"]);
        assert_eq!(out[2].ts, now - 100.0);
        let _ = fs::remove_dir_all(&dir);
    }

    fn planner(idx: i64, calls: &str) -> String {
        format!(
            r#"{{"step_index":{idx},"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-19T10:00:05Z","content":"","tool_calls":{calls}}}"#
        )
    }

    fn step(idx: i64, ty: &str, status: &str, content: &str) -> String {
        format!(
            r#"{{"step_index":{idx},"source":"MODEL","type":"{ty}","status":"{status}","created_at":"2026-11-19T10:00:06Z","content":{content:?}}}"#
        )
    }

    const STAMP: &str = "Created At: 2026-11-19T10:00:05+00:00\nCompleted At: 2026-11-19T10:00:06+00:00\n";

    fn diff_step(idx: i64, tool: &str, path: &str, body: &str) -> String {
        step(idx, "CODE_ACTION", "DONE", &format!("{STAMP}The following changes were made by the {tool} tool to: {path}\n[diff_block_start]\n{body}[diff_block_end]\n\nPlease note that the above snippet only shows the MODIFIED lines."))
    }

    fn command_step(idx: i64, status: &str, output: &str) -> String {
        step(idx, "RUN_COMMAND", "DONE", &format!("{STAMP}\n\t\t\t\tThe command {status}\n\t\t\t\tOutput:\n\t\t\t\t{}", output.replace('\n', "\n\t\t\t\t")))
    }

    fn work(out: &[Found]) -> Vec<String> {
        out.iter()
            .filter_map(|f| match &f.kind {
                Kind::AgentEdit { session, path, added, removed } => Some(format!("{session} {path} +{added} -{removed}")),
                Kind::AgentCommit { session, sha } => Some(format!("{session} commit {} in {}", sha.as_deref().unwrap_or("-"), f.cwd.as_ref().unwrap().display().to_string().replace('\\', "/"))),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn edits_count_the_diff_and_new_files_their_content() {
        let hunks = "@@ -1,3 +1,3 @@\n keep\n-old\n+new\n keep\n@@ -20 +20,2 @@\n x\n+y\n";
        let out = run(&[
            USER,
            &planner(2, r#"[{"name":"replace_file_content","args":{"TargetFile":"/w/p/a.rs"}}]"#),
            &diff_step(3, "replace_file_content", "/w/p/a.rs", hunks),
            &planner(4, r#"[{"name":"write_to_file","args":{"TargetFile":"/w/p/new.rs","CodeContent":"1\n2\n3\n","Overwrite":"false"}}]"#),
            &step(5, "CODE_ACTION", "DONE", &format!("{STAMP}Created file file:///w/p/new.rs\nIf relevant, proactively run terminal commands.")),
            &planner(6, r#"[{"name":"write_to_file","args":{"TargetFile":"/w/p/over.rs","CodeContent":"1\n","Overwrite":"true"}}]"#),
            &step(7, "CODE_ACTION", "DONE", &format!("{STAMP}Created file file:///w/p/over.rs\n")),
            DONE,
        ]);
        assert_eq!(work(&out), ["c1 /w/p/a.rs +2 -1", "c1 /w/p/new.rs +3 -0"]);
        assert_eq!(names(&out).iter().filter(|n| !n.starts_with("agent_")).count(), 2);
    }

    #[test]
    fn the_2x_app_and_1_5_cli_quote_their_arguments_and_answer_every_tool_with_a_generic_step() {
        let q = |s: &str| serde_json::to_string(s).unwrap();
        let writes = json!([
            { "name": "write_file", "args": { "Content": q("1\n2\n3\n"), "Path": q("/w/p/geo.py") } },
            { "name": "write_file", "args": { "Content": q("a\n"), "Path": q("/w/p/list.txt") } },
            { "name": "write_file", "args": { "Content": q("x\n"), "Path": q("/w/p/old.txt"), "Overwrite": "true" } },
        ]);
        let edit = json!([{ "name": "edit_file", "args": { "Path": q("/w/p/geo.py"), "Replacement": q("b"), "Target": q("a") } }]);
        let commands = json!([
            { "name": "run_command", "args": { "CommandLine": q("git status"), "IsDaemon": "false" } },
            { "name": "run_command", "args": { "CommandLine": q("git add -A && git commit -m 'add geo'"), "Cwd": q("/w/p/sub") } },
        ]);
        let generic = |idx: i64, text: &str| step(idx, "GENERIC", "DONE", &format!("{STAMP}{STAMP}{text}"));
        let out = run(&[
            USER,
            &planner(2, &writes.to_string()),
            &generic(3, "Created file."),
            &generic(4, "Created file."),
            &generic(5, "Created file."),
            &planner(6, &edit.to_string()),
            &generic(7, "Made 1 replacement in /w/p/geo.py starting at line 4 (+4/-1 lines)."),
            &planner(8, &commands.to_string()),
            &generic(9, "Command exited with code 0.\n\nCommand output:\n[main fffffff] not a commit\n"),
            &generic(10, "Command exited with code 0.\n\nCommand output:\n[main abc1234] add geo\n 1 file changed\n"),
            DONE,
        ]);
        assert_eq!(work(&out), ["c1 /w/p/geo.py +3 -0", "c1 /w/p/list.txt +1 -0", "c1 /w/p/geo.py +4 -1", "c1 commit abc1234 in /w/p/sub"]);
        let files = out.iter().find_map(|f| match f.kind {
            Kind::ReplyDone { files_changed, .. } => Some(files_changed),
            _ => None,
        });
        assert_eq!(files, Some(3));
    }

    #[test]
    fn a_turn_that_started_a_subagent_goes_on_until_the_subagent_messages_back() {
        let invoke = json!([{ "name": "invoke_subagent", "args": { "Prompt": "\"add a docstring\"", "Role": "\"Code Editor\"" } }]);
        let created = step(3, "GENERIC", "DONE", &format!("{STAMP}Created the following subagent:\n{{\n  \"conversationId\": \"sub-1\"\n}}\nThe subagent will send you a message when it has completed its task."));
        let waits = r#"{"step_index":4,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-19T10:00:10Z","content":"Started subagent `sub-1`."}"#;
        let other = r#"{"step_index":5,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-11-19T10:00:20Z","content":"<SYSTEM_MESSAGE>\nReminder: work efficiently.\n</SYSTEM_MESSAGE>"}"#;
        let message = r#"{"step_index":6,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-11-19T10:00:40Z","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-19T10:00:40Z sender=sub-1 priority=MESSAGE_PRIORITY_HIGH content=Added it.\n</SYSTEM_MESSAGE>"}"#;
        let ends = |lines: &[&str]| -> Vec<f64> { run(lines).iter().filter(|f| f.kind.name() == "reply_done").map(|f| f.ts).collect() };
        let done = ts(&serde_json::from_str(DONE).unwrap()).unwrap();
        assert_eq!(ends(&[USER, &planner(2, &invoke.to_string()), &created, waits, other, message, DONE]), [done]);
        let waited = ts(&serde_json::from_str(waits).unwrap()).unwrap();
        assert_eq!(ends(&[USER, &planner(2, &invoke.to_string()), waits]), [waited], "a subagent that never started holds nothing up");
    }

    #[test]
    fn failed_edits_other_commands_and_failed_commits_make_nothing() {
        let hunks = "@@ -1 +1 @@\n-a\n+b\n";
        let out = run(&[
            USER,
            &planner(2, r#"[{"name":"replace_file_content","args":{"TargetFile":"/w/p/a.rs"}}]"#),
            &step(3, "CODE_ACTION", "ERROR", "target content not found"),
            &diff_step(4, "replace_file_content", "/w/p/a.rs", hunks).replace(r#""status":"DONE""#, r#""status":"RUNNING""#),
            &planner(5, r#"[{"name":"run_command","args":{"CommandLine":"git status"}},{"name":"run_command","args":{"CommandLine":"git commit -m x"}}]"#),
            &command_step(6, "completed successfully.", "[main fffffff] not a commit"),
            &command_step(7, "exited with code 1", "nothing to commit, working tree clean"),
            DONE,
        ]);
        assert!(work(&out).is_empty(), "{:?}", work(&out));
    }

    #[test]
    fn a_commit_is_a_run_command_whose_output_printed_a_sha() {
        let out = run(&[
            USER,
            &planner(2, r#"[{"name":"run_command","args":{"CommandLine":"git add -A"}},{"name":"run_command","args":{"CommandLine":"cd sub && git commit -m 'fix it'"}}]"#),
            &command_step(3, "completed successfully.", ""),
            &command_step(4, "completed successfully.", "[main abc1234] fix it\n 1 file changed"),
            DONE,
        ]);
        assert_eq!(work(&out), ["c1 commit abc1234 in /w/p/sub"]);
    }

    #[test]
    fn subagent_conversations_count_for_their_work() {
        let mut c = Conv { id: "sub1".into(), cwd: Some(PathBuf::from("/w/p")), sub: true, ..Conv::default() };
        let mut out = Vec::new();
        for l in [diff_step(3, "replace_file_content", "/w/p/a.rs", "@@ -1 +1 @@\n-a\n+b\n")] {
            feed(&mut c, &serde_json::from_str(&l).unwrap(), &mut out);
        }
        assert_eq!(work(&out), ["sub1 /w/p/a.rs +1 -1"]);
    }
}
