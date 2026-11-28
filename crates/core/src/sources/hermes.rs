use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Map, Value};

use super::{snapshot, string_words, work, words, Found, Source};
use crate::event::{Ask, Kind};
use crate::{prompt_shape, prompt_stats};

// Sessions a person drives from this computer (Hermes's own attended set). Messaging platforms, cron jobs, one-shot
// `hermes chat -q` runs and subagents are not a person typing here, but their edits and commits still count.
const ATTENDED: [&str; 4] = ["cli", "tui", "desktop", "acp"];
const RESULT_TOOLS: &str = "'patch', 'write_file', 'terminal', 'clarify'";
const BATCH: i64 = 2000;
// User-role rows Hermes writes itself (session_rules/turns.py, _is_synthetic_compression_user_turn).
const SYNTHETIC: [&str; 5] = [
    "Continue from the compressed conversation context above. This marker exists because no human user turn was available.",
    "Continue from the compressed conversation context above. This marker exists because the compacted transcript contained no preserved user turn.",
    "You've reached the maximum number of tool-calling iterations allowed. Please provide a final response summarizing what you've found and accomplished so far, without calling any more tools.",
    "Your previous turn indicated a tool call but none was included. Do not narrate a plan or restate intent — issue the actual tool call now to continue the task.",
    "You just executed tool calls but returned an empty response. Please process the tool results above and continue with the task.",
];
const SYNTHETIC_STARTS: [&str; 5] = [
    "[System: ",
    "[IMPORTANT: Background process ",
    "[Your active task list was preserved across context compression]",
    "[CONTEXT COMPACTION",
    "[CONTEXT SUMMARY]:",
];
// Multimodal content is stored as this prefix and the JSON of its parts.
const JSON_CONTENT: &str = "\u{0}json:";

// $HERMES_HOME, else ~/.hermes (%LOCALAPPDATA%\hermes on Windows, where the desktop app may still use ~\.hermes). Each
// holds state.db and profiles/<name>/state.db.
pub fn default_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = std::env::var_os("HERMES_HOME").filter(|h| !h.is_empty()).map(PathBuf::from).into_iter().collect();
    if cfg!(windows) {
        roots.extend(dirs::data_local_dir().map(|d| d.join("hermes")));
    }
    roots.extend(dirs::home_dir().map(|h| h.join(".hermes")));
    roots
}

struct Row {
    id: i64,
    session: String,
    role: String,
    content: Option<String>,
    call_id: Option<String>,
    calls: Option<String>,
    ts: f64,
    finish: Option<String>,
    display: Option<String>,
    summary: bool,
    source: String,
    cwd: Option<String>,
    model: Option<String>,
    child: bool,
    copy: bool,
}

impl Row {
    fn attended(&self) -> bool {
        !self.child && ATTENDED.contains(&self.source.as_str())
    }

    fn cwd(&self) -> Option<PathBuf> {
        self.cwd.as_deref().filter(|c| !c.is_empty()).map(PathBuf::from)
    }
}

enum Call {
    Edit,
    Shell(String),
    Clarify { asked: f64, shown: usize },
}

#[derive(Default)]
struct Turn {
    open: bool,
    armed: bool,
    last_ts: f64,
    reply_words: usize,
    tools: usize,
    files: BTreeSet<String>,
    calls: HashMap<String, Call>,
    // The terminal's folder once a command moved it (the result says so).
    dir: Option<PathBuf>,
}

impl Turn {
    fn close(&mut self, ts: f64, r: &Row, out: &mut Vec<Found>) {
        if !self.open {
            return;
        }
        self.open = false;
        self.armed = false;
        out.push(Found {
            ts,
            cwd: r.cwd(),
            kind: Kind::ReplyDone {
                session: r.session.clone(),
                reply_words: self.reply_words,
                tools: self.tools,
                files_changed: self.files.len(),
                model: r.model.clone(),
            },
        });
    }
}

// The text parts and whether there was an image.
fn text(content: Option<&str>) -> (String, bool) {
    let Some(c) = content else { return (String::new(), false) };
    let Some(parts) = c.strip_prefix(JSON_CONTENT) else { return (c.to_string(), false) };
    let Ok(v) = serde_json::from_str::<Value>(parts) else { return (String::new(), false) };
    let parts: Vec<&Value> = v.as_array().map_or_else(|| vec![&v], |a| a.iter().collect());
    let kind = |p: &Value| p.get("type").and_then(Value::as_str).unwrap_or("").to_string();
    let text = parts
        .iter()
        .filter_map(|p| p.as_str().or_else(|| matches!(kind(p).as_str(), "text" | "input_text").then(|| p.get("text")?.as_str()).flatten()))
        .collect::<Vec<_>>()
        .join("\n");
    (text, parts.iter().any(|p| kind(p).contains("image")))
}

fn cut<'a>(s: &'a str, marker: &str, stop: &str, last: bool) -> Option<&'a str> {
    let at = if last { s.rfind(marker) } else { s.find(marker) }?;
    Some(s[at + marker.len()..].split(stop).next().unwrap_or("").trim())
}

// A skill slash command is logged as the whole skill with what was typed alongside it
// (extract_user_instruction_from_skill_message in session_rules/turns.py).
fn typed(text: &str) -> String {
    let Some(rest) = text.strip_prefix("[IMPORTANT: The user has invoked the ") else { return text.to_string() };
    let name = rest.strip_prefix('"').and_then(|r| r.split_once('"')).map_or("", |(n, _)| n);
    let said = if text.contains(" skill bundle,") {
        cut(text, "\nUser instruction: ", "\n\n[Loaded as part of the ", false)
    } else if text.contains("The full skill content is loaded below.]") {
        cut(text, "The user has provided the following instruction alongside the skill invocation: ", "\n\n[Runtime note:", true)
    } else {
        None
    };
    format!("/{} {}", name.trim_start_matches('/'), said.unwrap_or("")).trim_end().to_string()
}

fn synthetic(text: &str) -> bool {
    let t = text.trim();
    SYNTHETIC.contains(&t) || SYNTHETIC_STARTS.iter().any(|s| t.starts_with(s))
}

// Tool results are JSON, sometimes with notes appended after it.
fn result_json(content: Option<&str>) -> Option<Value> {
    serde_json::Deserializer::from_str(content?).into_iter::<Value>().next()?.ok()
}

// Lines added and removed per file in a patch result's diff: Python unified diffs (`--- a/<path>` then `+++ b/<path>`), one
// after another for a patch touching several files, `--- /dev/null` for a new file. Deleted files are left out.
fn diff_files(diff: &str) -> Vec<(String, u32, u32)> {
    let lines: Vec<&str> = diff.lines().collect();
    let mut files: Vec<(String, u32, u32)> = Vec::new();
    let mut cur: Option<(String, u32, u32)> = None;
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if l.starts_with("--- ") && lines.get(i + 1).is_some_and(|n| n.starts_with("+++ ")) {
            files.extend(cur.take());
            let to = lines[i + 1][4..].trim_end();
            if to != "/dev/null" {
                cur = Some((to.strip_prefix("b/").unwrap_or(to).to_string(), 0, 0));
            }
            i += 2;
            continue;
        }
        if let Some(c) = cur.as_mut() {
            match l.as_bytes().first() {
                Some(b'+') => c.1 = c.1.saturating_add(1),
                Some(b'-') => c.2 = c.2.saturating_add(1),
                _ => {}
            }
        }
        i += 1;
    }
    files.extend(cur);
    files
}

fn calls(r: &Row) -> Vec<Value> {
    r.calls.as_deref().and_then(|c| serde_json::from_str::<Value>(c).ok()).and_then(|v| v.as_array().cloned()).unwrap_or_default()
}

fn prompt(t: &mut Turn, r: &Row, out: &mut Vec<Found>) {
    if r.summary || !matches!(r.display.as_deref(), None | Some("steer")) {
        return;
    }
    let (text, image) = text(r.content.as_deref());
    if synthetic(&text) {
        return;
    }
    let text = typed(&text);
    if text.trim().is_empty() && !image {
        return;
    }
    let last = t.last_ts;
    t.close(last.min(r.ts), r, out);
    let (words, pasted) = prompt_stats(&text);
    t.armed = true;
    t.open = true;
    t.last_ts = r.ts;
    t.reply_words = 0;
    t.tools = 0;
    t.files.clear();
    out.push(Found {
        ts: r.ts,
        cwd: r.cwd(),
        kind: Kind::Prompt {
            words,
            pasted,
            model: r.model.clone(),
            session: r.session.clone(),
            shape: Some(prompt_shape(&text)),
            version: None,
            via: None,
            permissions: None,
        },
    });
}

fn reply(t: &mut Turn, r: &Row, out: &mut Vec<Found>) {
    let calls = calls(r);
    for c in &calls {
        let Some(id) = c.get("id").and_then(Value::as_str) else { continue };
        let name = c.pointer("/function/name").and_then(Value::as_str).unwrap_or("");
        let args = c.pointer("/function/arguments").and_then(Value::as_str).and_then(|a| serde_json::from_str::<Value>(a).ok()).unwrap_or(Value::Null);
        let call = match name {
            "patch" | "write_file" => Some(Call::Edit),
            "terminal" => args.get("command").and_then(Value::as_str).filter(|c| c.contains("commit")).map(|cmd| {
                Call::Shell(match args.get("workdir").and_then(Value::as_str).filter(|d| !d.is_empty()) {
                    Some(d) => format!("cd {} && {cmd}", work::shell_join([d])),
                    None => cmd.to_string(),
                })
            }),
            "clarify" if r.attended() => Some(Call::Clarify { asked: r.ts, shown: args.get("questions").map_or(0, |q| string_words(q, usize::MAX)) }),
            _ => None,
        };
        if let Some(call) = call {
            work::remember(&mut t.calls, id.to_string(), call);
        }
    }
    if !t.open {
        return;
    }
    t.last_ts = r.ts;
    t.reply_words += words(&text(r.content.as_deref()).0);
    if !calls.is_empty() && t.armed {
        t.armed = false;
        out.push(Found { ts: r.ts, cwd: r.cwd(), kind: Kind::AgentStart { session: r.session.clone() } });
    }
    t.tools += calls.len();
    // `incomplete` is commentary between tool rounds; `tool_calls` with no calls gets a nudge and goes on.
    if calls.is_empty() && !matches!(r.finish.as_deref(), Some("incomplete" | "tool_calls")) {
        t.close(r.ts, r, out);
    }
}

fn result(t: &mut Turn, r: &Row, out: &mut Vec<Found>) {
    if t.open {
        t.last_ts = r.ts;
    }
    let Some(call) = r.call_id.as_deref().and_then(|id| t.calls.remove(id)) else { return };
    let Some(res) = result_json(r.content.as_deref()) else { return };
    let cwd = r.cwd();
    match call {
        Call::Edit => {
            if res.get("success").and_then(Value::as_bool) == Some(false) || res.get("error").is_some_and(|e| !e.is_null()) {
                return;
            }
            let modified: Vec<&str> = res.get("files_modified").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            if t.open {
                t.files.extend(modified.iter().map(|p| p.to_string()));
            }
            let diffs = diff_files(res.get("diff").and_then(Value::as_str).unwrap_or(""));
            if let ([path], [(_, added, removed)]) = (modified.as_slice(), diffs.as_slice()) {
                work::edit(r.ts, cwd.as_deref(), &r.session, path, (*added, *removed), out);
            } else {
                for (path, added, removed) in &diffs {
                    work::edit(r.ts, cwd.as_deref(), &r.session, path, (*added, *removed), out);
                }
            }
        }
        Call::Shell(cmd) => {
            let dir = t.dir.clone().or(cwd);
            if let Some(d) = res.get("cwd").and_then(Value::as_str).filter(|d| !d.is_empty()) {
                t.dir = Some(PathBuf::from(d));
            }
            let Some(output) = res.get("output").and_then(Value::as_str) else { return };
            let failed = res.get("exit_code").and_then(Value::as_i64).is_some_and(|c| c != 0);
            if output.trim().is_empty() && failed {
                return;
            }
            work::commits(r.ts, dir.as_deref(), &r.session, &cmd, output, out);
        }
        Call::Clarify { asked, shown } => {
            if res.get("outcome").and_then(Value::as_str) != Some("submitted") {
                return;
            }
            let answer_words = res
                .get("responses")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|a| a.get("status").and_then(Value::as_str) == Some("answered"))
                .map(|a| {
                    let said = a.get("user_response").unwrap_or(&Value::Null);
                    let offered = a.get("choices_offered").and_then(Value::as_array).is_some_and(|c| c.contains(said));
                    if offered { 0 } else { string_words(said, usize::MAX) }
                })
                .sum();
            out.push(Found {
                ts: r.ts,
                cwd: r.cwd(),
                kind: Kind::Answered { session: r.session.clone(), asked, why: Ask::Question, shown_words: shown, answer_words },
            });
        }
    }
}

fn feed(turns: &mut HashMap<String, Turn>, r: &Row, out: &mut Vec<Found>) {
    if r.copy {
        return;
    }
    if !turns.contains_key(&r.session) && turns.len() >= 500 {
        turns.clear();
    }
    let t = turns.entry(r.session.clone()).or_default();
    match r.role.as_str() {
        "user" if r.attended() => prompt(t, r, out),
        "assistant" => reply(t, r, out),
        "tool" => result(t, r, out),
        _ => {}
    }
}

const NEEDED: [(&str, &[&str]); 2] = [
    ("sessions", &["source", "created_source", "cwd", "model", "model_config", "parent_session_id"]),
    ("messages", &["session_id", "role", "content", "tool_call_id", "tool_calls", "tool_name", "timestamp", "finish_reason", "display_kind", "_compressed_summary", "message_uid"]),
];

// Hermes adds columns as it goes; a store missing one of these hasn't been opened by a version this reader knows.
fn known(c: &Connection) -> bool {
    NEEDED.iter().all(|(table, cols)| {
        let Ok(mut st) = c.prepare(&format!("PRAGMA table_info({table})")) else { return false };
        let Ok(have) = st.query_map([], |r| r.get::<_, String>(1)).and_then(|rows| rows.collect::<rusqlite::Result<Vec<String>>>()) else { return false };
        cols.iter().all(|col| have.iter().any(|h| h == col))
    })
}

fn rows(c: &Connection, after: i64) -> rusqlite::Result<Vec<Row>> {
    // A row compaction copied into the new transcript keeps its message_uid; the first one with that uid was read already.
    let sql = format!(
        "SELECT m.id, m.session_id, m.role,
            CASE WHEN m.role = 'tool' AND m.tool_name IS NOT NULL AND m.tool_name NOT IN ({RESULT_TOOLS}) THEN NULL ELSE m.content END,
            m.tool_call_id, m.tool_calls, m.timestamp, m.finish_reason, m.display_kind, m._compressed_summary,
            COALESCE(s.created_source, s.source), s.cwd, s.model,
            s.source = 'subagent' OR instr(COALESCE(s.model_config, ''), '\"_delegate_from\"') > 0,
            m.message_uid IS NOT NULL AND (
                EXISTS (SELECT 1 FROM messages o WHERE o.session_id = m.session_id AND o.id < m.id AND o.message_uid = m.message_uid)
                OR EXISTS (SELECT 1 FROM messages o WHERE o.session_id = s.parent_session_id AND o.message_uid = m.message_uid))
         FROM messages m JOIN sessions s ON s.id = m.session_id
         WHERE m.id > ?1 ORDER BY m.id LIMIT {BATCH}"
    );
    let mut st = c.prepare(&sql)?;
    let rows = st.query_map([after], |r| {
        Ok(Row {
            id: r.get(0)?,
            session: r.get(1)?,
            role: r.get(2)?,
            content: r.get(3)?,
            call_id: r.get(4)?,
            calls: r.get(5)?,
            ts: r.get(6)?,
            finish: r.get(7)?,
            display: r.get(8)?,
            summary: r.get::<_, Option<i64>>(9)?.unwrap_or(0) != 0,
            source: r.get::<_, Option<String>>(10)?.unwrap_or_default(),
            cwd: r.get(11)?,
            model: r.get(12)?,
            child: r.get::<_, Option<bool>>(13)?.unwrap_or(false),
            copy: r.get::<_, Option<bool>>(14)?.unwrap_or(false),
        })
    })?;
    rows.collect()
}

fn open(db: &Path) -> Option<Connection> {
    let c = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;
    c.busy_timeout(Duration::from_secs(5)).ok()?;
    Some(c)
}

pub struct Hermes {
    roots: Vec<PathBuf>,
    since: Option<f64>,
    cursors: HashMap<PathBuf, i64>,
    turns: HashMap<String, Turn>,
}

impl Hermes {
    pub fn new(roots: Vec<PathBuf>) -> Hermes {
        Hermes { roots, since: None, cursors: HashMap::new(), turns: HashMap::new() }
    }

    fn stores(&self) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = Vec::new();
        for root in &self.roots {
            let profiles = std::fs::read_dir(root.join("profiles")).into_iter().flatten().filter_map(Result::ok).map(|e| e.path().join("state.db"));
            for db in std::iter::once(root.join("state.db")).chain(profiles) {
                let db = std::fs::canonicalize(&db).unwrap_or(db);
                if db.is_file() && !found.contains(&db) {
                    found.push(db);
                }
            }
        }
        found
    }

    fn read(&mut self, db: &Path, out: &mut Vec<Found>) {
        let Some(c) = open(db) else { return };
        if !known(&c) {
            return;
        }
        let after = match self.cursors.get(db) {
            Some(&after) => after,
            None => {
                let fresh = self.since.is_some_and(|s| std::fs::metadata(db).map_or(0.0, |m| snapshot::mtime(&m)) >= s - 5.0);
                let last = c.query_row("SELECT COALESCE(MAX(id), 0) FROM messages", [], |r| r.get(0)).unwrap_or(0);
                self.cursors.insert(db.to_path_buf(), if fresh { 0 } else { last });
                if !fresh {
                    return;
                }
                0
            }
        };
        let Ok(rows) = rows(&c, after) else { return };
        let mut found = Vec::new();
        for r in &rows {
            feed(&mut self.turns, r, &mut found);
        }
        if let Some(last) = rows.last() {
            self.cursors.insert(db.to_path_buf(), last.id);
        }
        out.extend(found);
    }
}

impl Source for Hermes {
    fn name(&self) -> &'static str {
        "hermes"
    }

    fn poll(&mut self) -> Vec<Found> {
        let started = snapshot::now();
        let mut out = Vec::new();
        for db in self.stores() {
            self.read(&db, &mut out);
        }
        self.since = Some(started);
        out.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        out
    }

    fn state(&self) -> Value {
        let stores: Map<String, Value> = self.cursors.iter().map(|(p, c)| (p.to_string_lossy().into_owned(), json!(c))).collect();
        json!({ "since": self.since, "stores": stores })
    }

    fn restore(&mut self, state: &Value) {
        if let Some(s) = state.get("since").and_then(Value::as_f64) {
            self.since = Some(s);
        }
        for (p, c) in state.get("stores").and_then(Value::as_object).into_iter().flatten() {
            if let Some(c) = c.as_i64() {
                self.cursors.insert(PathBuf::from(p), c);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The columns this reader uses, as Hermes 0.23.4 creates them (persistence/sqlite/_schema_ddl.py).
    const SCHEMA: &str = "
        CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT NOT NULL, created_source TEXT, model TEXT, model_config TEXT,
            parent_session_id TEXT, started_at REAL NOT NULL, cwd TEXT);
        CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, role TEXT NOT NULL, content TEXT,
            tool_call_id TEXT, tool_calls TEXT, tool_name TEXT, timestamp REAL NOT NULL, finish_reason TEXT, display_kind TEXT,
            _compressed_summary INTEGER NOT NULL DEFAULT 0, active INTEGER NOT NULL DEFAULT 1, message_uid TEXT);";

    struct Db {
        dir: PathBuf,
        c: Connection,
        n: u32,
    }

    impl Db {
        fn new(name: &str) -> Db {
            let dir = std::env::temp_dir().join(format!("kairos-hermes-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let c = Connection::open(dir.join("state.db")).unwrap();
            c.execute_batch(SCHEMA).unwrap();
            Db { dir, c, n: 0 }
        }

        fn session(&self, id: &str, source: &str, cwd: Option<&str>, config: &str, parent: Option<&str>) {
            self.c
                .execute(
                    "INSERT INTO sessions (id, source, created_source, model, model_config, parent_session_id, started_at, cwd) VALUES (?1, ?2, ?2, 'anthropic/claude-opus-4.6', ?3, ?4, 1795534212.0, ?5)",
                    rusqlite::params![id, source, config, parent, cwd],
                )
                .unwrap();
        }

        fn row(&mut self, s: &str, role: &str, content: Option<&str>, extra: &[(&str, &str)]) -> String {
            self.n += 1;
            let uid = format!("{:032x}", self.n);
            self.insert(s, role, content, extra, &uid);
            uid
        }

        fn insert(&self, s: &str, role: &str, content: Option<&str>, extra: &[(&str, &str)], uid: &str) {
            let get = |k: &str| extra.iter().find(|(n, _)| *n == k).map(|(_, v)| *v);
            let ts: f64 = get("ts").map_or(1795534212.0, |t| t.parse().unwrap());
            self.c
                .execute(
                    "INSERT INTO messages (session_id, role, content, tool_call_id, tool_calls, tool_name, timestamp, finish_reason, display_kind, _compressed_summary, message_uid)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    rusqlite::params![s, role, content, get("call"), get("calls"), get("tool"), ts, get("finish"), get("kind"), get("summary").map_or(0, |_| 1), uid],
                )
                .unwrap();
        }
    }

    impl Drop for Db {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn names(out: &[Found]) -> Vec<&'static str> {
        out.iter().map(|f| f.kind.name()).collect()
    }

    fn summary(out: &[Found]) -> Vec<String> {
        out.iter()
            .filter_map(|f| match &f.kind {
                Kind::AgentEdit { path, added, removed, session } => Some(format!("{session} {path} +{added} -{removed}")),
                Kind::AgentCommit { sha, .. } => Some(format!("commit {} in {}", sha.as_deref().unwrap_or("-"), f.cwd.as_deref().map_or("-".into(), |c| c.display().to_string().replace('\\', "/")))),
                _ => None,
            })
            .collect()
    }

    fn call(id: &str, name: &str, args: Value) -> String {
        json!([{ "id": id, "call_id": id, "response_item_id": format!("fc_{id}"), "type": "function", "function": { "name": name, "arguments": args.to_string() } }]).to_string()
    }

    // The session from Hermes 0.23.4's example (a typed prompt, a patch, `git commit` in the terminal, a reply), every value
    // as its source writes it. Returns the source after its first poll, so only rows added later are read.
    fn started(db: &mut Db, source: &str) -> Hermes {
        db.session("20261124_153012_a1b2c3", source, Some("/home/u/proj"), r#"{"max_iterations": 90}"#, None);
        db.row("20261124_153012_a1b2c3", "user", Some("an old prompt from before Kairos"), &[("ts", "1795534000.0")]);
        let mut src = Hermes::new(vec![db.dir.clone()]);
        assert!(src.poll().is_empty());
        src
    }

    fn turn(db: &mut Db, s: &str) {
        db.row(s, "user", Some("fix the typo in README.md then commit it"), &[("ts", "1795534212.0")]);
        let patch = call("call_01", "patch", json!({ "path": "README.md", "old_string": "teh", "new_string": "the" }));
        db.row(s, "assistant", Some(""), &[("ts", "1795534216.4"), ("calls", &patch), ("finish", "tool_calls")]);
        let diff = json!({ "success": true, "diff": "--- a//home/u/proj/README.md\n+++ b//home/u/proj/README.md\n@@ -3 +3 @@\n-Some teh text\n+Some the text\n", "files_modified": ["/home/u/proj/README.md"], "resolved_path": "/home/u/proj/README.md" });
        db.row(s, "tool", Some(&diff.to_string()), &[("ts", "1795534216.9"), ("call", "call_01"), ("tool", "patch")]);
        let commit = call("call_02", "terminal", json!({ "command": "git add -A && git commit -m x" }));
        db.row(s, "assistant", Some(""), &[("ts", "1795534219.2"), ("calls", &commit), ("finish", "tool_calls")]);
        let out = json!({ "output": "[main 9f8e7d6] x\n 1 file changed, 1 insertion(+), 1 deletion(-)\n", "exit_code": 0, "error": null });
        db.row(s, "tool", Some(&out.to_string()), &[("ts", "1795534219.8"), ("call", "call_02"), ("tool", "terminal")]);
        db.row(s, "assistant", Some("Fixed the typo (\"teh\" -> \"the\") in README.md and committed it as `x`."), &[("ts", "1795534222.1"), ("finish", "stop")]);
    }

    #[test]
    fn a_cli_turn_becomes_prompt_start_edit_commit_done() {
        let mut db = Db::new("turn");
        let mut src = started(&mut db, "cli");
        turn(&mut db, "20261124_153012_a1b2c3");
        let out = src.poll();
        assert_eq!(names(&out), ["prompt", "agent_start", "agent_edit", "agent_commit", "reply_done"]);
        assert_eq!(summary(&out), ["20261124_153012_a1b2c3 /home/u/proj/README.md +1 -1", "commit 9f8e7d6 in /home/u/proj"]);
        assert!(out.iter().all(|f| f.cwd.as_deref() == Some(Path::new("/home/u/proj"))));
        match &out[0].kind {
            Kind::Prompt { words, model, .. } => assert_eq!((*words, model.as_deref()), (8, Some("anthropic/claude-opus-4.6"))),
            k => panic!("{k:?}"),
        }
        match &out[4].kind {
            Kind::ReplyDone { reply_words, tools, files_changed, .. } => assert_eq!((*reply_words, *tools, *files_changed), (13, 2, 1)),
            k => panic!("{k:?}"),
        }
        assert!((out[4].ts - out[0].ts - 10.1).abs() < 1e-6);
        assert!(src.poll().is_empty());

        let mut again = Hermes::new(vec![db.dir.clone()]);
        again.restore(&src.state());
        db.row("20261124_153012_a1b2c3", "user", Some("thanks"), &[("ts", "1795534300.0")]);
        assert_eq!(names(&again.poll()), ["prompt"]);
    }

    #[test]
    fn only_people_at_this_computer_make_prompts_but_every_session_makes_edits_and_commits() {
        for (source, config) in [("telegram", "{}"), ("cron", "{}"), ("oneshot", "{}"), ("subagent", "{}"), ("cli", r#"{"_delegate_from": "20261124_153012_a1b2c3"}"#)] {
            let mut db = Db::new(source);
            db.session("20261124_153012_a1b2c3", source, Some("/home/u/proj"), config, None);
            let mut src = Hermes::new(vec![db.dir.clone()]);
            src.poll();
            turn(&mut db, "20261124_153012_a1b2c3");
            assert_eq!(names(&src.poll()), ["agent_edit", "agent_commit"], "{source} {config}");
        }
    }

    #[test]
    fn hermes_own_rows_and_compaction_copies_are_not_prompts() {
        let mut db = Db::new("synthetic");
        let mut src = started(&mut db, "tui");
        let s = "20261124_153012_a1b2c3";
        db.row(s, "user", Some(SYNTHETIC[0]), &[]);
        db.row(s, "user", Some("[System: Your previous response was truncated by the output length limit. Continue exactly where you left off.]"), &[]);
        db.row(s, "user", Some("[CONTEXT COMPACTION — REFERENCE ONLY] Earlier turns were compacted"), &[("summary", "1"), ("kind", "hidden")]);
        db.row(s, "user", Some("Background job finished"), &[("kind", "internal_notification")]);
        db.row(s, "user", Some("   "), &[]);
        let typed = db.row(s, "user", Some("also update the changelog"), &[("ts", "1795534250.0")]);
        db.row(s, "user", Some("stop and run the tests first"), &[("ts", "1795534260.0"), ("kind", "steer")]);
        db.insert(s, "user", Some("also update the changelog"), &[("ts", "1795534250.0")], &typed);
        db.session("20261124_160000_d4e5f6", "tui", Some("/home/u/proj"), "{}", Some(s));
        db.insert("20261124_160000_d4e5f6", "user", Some("also update the changelog"), &[("ts", "1795534250.0")], &typed);
        let out = src.poll();
        assert_eq!(names(&out), ["prompt", "reply_done", "prompt"]);
        match (&out[0].kind, &out[2].kind) {
            (Kind::Prompt { words: a, .. }, Kind::Prompt { words: b, .. }) => assert_eq!((*a, *b), (4, 6)),
            k => panic!("{k:?}"),
        }
    }

    #[test]
    fn skill_commands_count_what_was_typed() {
        let single = "[IMPORTANT: The user has invoked the \"code-review\" skill, indicating they want you to follow its instructions. The full skill content is loaded below.]\n\nReview every changed file carefully and list each problem.\n\nThe user has provided the following instruction alongside the skill invocation: check the parser\n\n[Runtime note: cwd is /home/u/proj]";
        assert_eq!(typed(single), "/code-review check the parser");
        let bare = "[IMPORTANT: The user has invoked the \"code-review\" skill, indicating they want you to follow its instructions. The full skill content is loaded below.]\n\nReview every changed file.";
        assert_eq!(typed(bare), "/code-review");
        let bundle = "[IMPORTANT: The user has invoked the \"/review /test\" stacked skill bundle, loading 2 skills together. Treat every skill below as active guidance for this turn.]\n\nSkills loaded: review, test\n\nUser instruction: do both\n\n[Loaded as part of the stacked skill invocation \"review\".]\nbody";
        assert_eq!(typed(bundle), "/review /test do both");
        assert_eq!(typed("plain words"), "plain words");
        assert_eq!(text(Some("\u{0}json:[{\"type\":\"text\",\"text\":\"look at this\"},{\"type\":\"image_url\",\"image_url\":{\"url\":\"data:x\"}}]")), ("look at this".to_string(), true));
    }

    #[test]
    fn patches_writes_and_commands() {
        let mut db = Db::new("work");
        let mut src = started(&mut db, "cli");
        let s = "20261124_153012_a1b2c3";
        db.row(s, "user", Some("do the work"), &[("ts", "1795534212.0")]);
        let mut n = 0;
        let mut step = |db: &mut Db, name: &str, args: Value, result: Value| {
            n += 1;
            let id = format!("c{n}");
            db.row(s, "assistant", Some(""), &[("calls", &call(&id, name, args)), ("finish", "tool_calls")]);
            db.row(s, "tool", Some(&format!("{result}\n\n[Subdirectory hint: see AGENTS.md]")), &[("call", &id), ("tool", name)]);
        };
        let v4a = "--- a/src/a.py\n+++ b/src/a.py\n@@ -1,2 +1,2 @@\n-old\n+new\n keep\n\n--- /dev/null\n+++ b/src/new.py\n+one\n+two\n\n--- a/src/gone.py\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye";
        step(&mut db, "patch", json!({ "mode": "patch", "patch": "*** Begin Patch" }), json!({ "success": true, "diff": v4a, "files_modified": ["/home/u/proj/src/a.py", "/home/u/proj/src/new.py"] }));
        step(&mut db, "patch", json!({ "path": "b.py", "old_string": "x", "new_string": "y" }), json!({ "success": false, "error": "Could not find a match for old_string in b.py" }));
        step(&mut db, "write_file", json!({ "path": "notes.md", "content": "a\nb\n" }), json!({ "bytes_written": 4, "dirs_created": false, "files_modified": ["/home/u/proj/notes.md"] }));
        step(&mut db, "terminal", json!({ "command": "git status" }), json!({ "output": "[main abc1234] stray", "exit_code": 0 }));
        step(&mut db, "terminal", json!({ "command": "cd sub && git commit -qm x" }), json!({ "output": "", "exit_code": 0, "cwd": "/home/u/proj/sub" }));
        step(&mut db, "terminal", json!({ "command": "git commit -m y" }), json!({ "output": "[main 1234abcd] y", "exit_code": 0 }));
        step(&mut db, "terminal", json!({ "command": "git commit -m z", "workdir": "/home/u/other" }), json!({ "output": "[main 5678abcd] z", "exit_code": 0 }));
        step(&mut db, "terminal", json!({ "command": "git commit -m w" }), json!({ "status": "blocked", "error": "Command denied: commit" }));
        step(&mut db, "terminal", json!({ "command": "git commit -qm v" }), json!({ "output": "", "exit_code": 1 }));
        db.row(s, "assistant", Some("Done."), &[("finish", "stop")]);
        let out = src.poll();
        assert_eq!(
            summary(&out),
            [
                format!("{s} src/a.py +1 -1"),
                format!("{s} src/new.py +2 -0"),
                "commit - in /home/u/proj/sub".into(),
                "commit 1234abcd in /home/u/proj/sub".into(),
                "commit 5678abcd in /home/u/other".into(),
            ]
        );
        match &out.last().unwrap().kind {
            Kind::ReplyDone { tools, files_changed, .. } => assert_eq!((*tools, *files_changed), (9, 3)),
            k => panic!("{k:?}"),
        }
    }

    #[test]
    fn a_submitted_clarify_is_an_answer() {
        let mut db = Db::new("clarify");
        let mut src = started(&mut db, "desktop");
        let s = "20261124_153012_a1b2c3";
        db.row(s, "user", Some("set up the project"), &[("ts", "1795534212.0")]);
        let ask = call("q1", "clarify", json!({ "questions": [{ "question": "Which test runner?", "choices": ["pytest", "unittest"] }, { "question": "Any notes?" }] }));
        db.row(s, "assistant", Some(""), &[("ts", "1795534215.0"), ("calls", &ask), ("finish", "tool_calls")]);
        let answered = json!({ "responses": [
            { "question": "Which test runner?", "choices_offered": ["pytest", "unittest"], "status": "answered", "user_response": "pytest" },
            { "question": "Any notes?", "choices_offered": [], "status": "answered", "user_response": "keep it small please" }
        ], "outcome": "submitted" });
        db.row(s, "tool", Some(&answered.to_string()), &[("ts", "1795534240.0"), ("call", "q1"), ("tool", "clarify")]);
        let ask2 = call("q2", "clarify", json!({ "questions": [{ "question": "Deploy?" }] }));
        db.row(s, "assistant", Some(""), &[("ts", "1795534241.0"), ("calls", &ask2), ("finish", "tool_calls")]);
        db.row(s, "tool", Some(r#"{"responses": [], "outcome": "timed_out"}"#), &[("ts", "1795534541.0"), ("call", "q2"), ("tool", "clarify")]);
        let out = src.poll();
        let answers: Vec<&Found> = out.iter().filter(|f| f.kind.name() == "answered").collect();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].ts, 1795534240.0);
        assert_eq!(
            answers[0].kind,
            Kind::Answered { session: s.into(), asked: 1795534215.0, why: Ask::Question, shown_words: 7, answer_words: 4 }
        );
    }

    #[test]
    fn profiles_and_stores_that_appear_later_are_read() {
        let mut db = Db::new("profiles");
        let mut src = started(&mut db, "cli");
        let work = db.dir.join("profiles/work");
        std::fs::create_dir_all(&work).unwrap();
        let c = Connection::open(work.join("state.db")).unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c.execute("INSERT INTO sessions (id, source, started_at, cwd) VALUES ('w1', 'cli', 1795534212.0, '/home/u/w')", []).unwrap();
        c.execute("INSERT INTO messages (session_id, role, content, timestamp, message_uid) VALUES ('w1', 'user', 'hello there', 1795534300.0, 'u1')", []).unwrap();
        let out = src.poll();
        assert_eq!(names(&out), ["prompt"]);
        assert_eq!(out[0].cwd.as_deref(), Some(Path::new("/home/u/w")));

        let old = Db::new("old");
        old.c.execute_batch("ALTER TABLE messages DROP COLUMN message_uid").unwrap();
        let mut src = Hermes::new(vec![old.dir.clone()]);
        assert!(src.poll().is_empty() && src.state()["stores"].as_object().unwrap().is_empty());
    }

    // Real rows: Hermes 0.23.4 driven against a fake model in its classic CLI, its TUI and `hermes chat -q`. Interactive
    // sessions are saved with source 'local' and the surface in created_source.
    #[test]
    fn real_hermes_0_23_sessions() {
        let mut db = Db::new("real");
        for (id, source, created, cwd) in [
            ("local-14a3f762", "local", "cli", "/home/user/cliproj"),
            ("local-3b5a1716", "local", "tui", "/home/user/tuiproj"),
            ("local-4386100b", "oneshot", "oneshot", "/home/user/hproj2"),
        ] {
            db.c.execute(
                "INSERT INTO sessions (id, source, created_source, model, started_at, cwd) VALUES (?1, ?2, ?3, 'fake-model', 1795530600.0, ?4)",
                rusqlite::params![id, source, created, cwd],
            )
            .unwrap();
        }
        let mut src = Hermes::new(vec![db.dir.clone()]);
        assert!(src.poll().is_empty());
        let patch = |dir: &str| {
            let path = format!("{dir}/main.py");
            json!({ "success": true, "diff": format!("--- a/{path}\n+++ b/{path}\n@@ -1 +1,2 @@\n print(\"hello\")\n+print(\"verbose\")\n"), "files_modified": [path], "lint": { "status": "ok", "output": "" }, "resolved_path": path }).to_string()
        };
        let patch_call = call("call_f6bfe9b5", "patch", json!({ "path": "main.py", "old_string": "print(\"hello\")", "new_string": "print(\"hello\")\nprint(\"verbose\")" }));
        let commit_call = call("call_61fb28a4", "terminal", json!({ "command": "git add -A && git commit -m \"add verbose\"" }));
        let committed = |sha: &str| json!({ "output": format!("[main {sha}] add verbose\n 1 file changed, 1 insertion(+)"), "exit_code": 0, "error": null }).to_string();
        let turn = |db: &mut Db, s: &str, dir: &str, sha: &str, t: f64| {
            db.row(s, "assistant", Some("Editing."), &[("ts", &(t + 2.8).to_string()), ("calls", &patch_call), ("finish", "tool_calls")]);
            db.row(s, "tool", Some(&patch(dir)), &[("ts", &(t + 3.0).to_string()), ("call", "call_f6bfe9b5"), ("tool", "patch")]);
            db.row(s, "assistant", Some(""), &[("ts", &(t + 4.0).to_string()), ("calls", &commit_call), ("finish", "tool_calls")]);
            db.row(s, "tool", Some(&committed(sha)), &[("ts", &(t + 4.1).to_string()), ("call", "call_61fb28a4"), ("tool", "terminal")]);
            db.row(s, "assistant", Some("All done here."), &[("ts", &(t + 5.1).to_string()), ("finish", "stop")]);
        };

        db.row("local-14a3f762", "user", Some("now say hi"), &[("ts", "1795533344.498673")]);
        turn(&mut db, "local-14a3f762", "/home/user/cliproj", "ba5e751", 1795533344.5);
        db.row("local-3b5a1716", "user", Some("add a verbose line and commit"), &[("ts", "1795533490.807779")]);
        turn(&mut db, "local-3b5a1716", "/home/user/tuiproj", "4f2d839", 1795533490.8);
        db.row("local-3b5a1716", "user", Some("now say hi"), &[("ts", "1795533501.284043")]);
        db.row("local-3b5a1716", "assistant", Some("Done."), &[("ts", "1795533502.3558424"), ("finish", "stop")]);
        db.row("local-4386100b", "user", Some("add a verbose line and commit"), &[("ts", "1795530600.469422")]);
        turn(&mut db, "local-4386100b", "/home/user/hproj2", "00e8fcd", 1795530600.9);
        let write = call("call_7fe5564b", "write_file", json!({ "path": "notes.txt", "content": "one\ntwo\n" }));
        db.row("local-4386100b", "assistant", None, &[("ts", "1795530605.088169"), ("calls", &write), ("finish", "tool_calls")]);
        let wrote = r#"{"bytes_written": 8, "dirs_created": true, "verified": true, "lint": {"status": "skipped", "message": "No linter for .txt files"}, "resolved_path": "/home/user/hproj2/notes.txt", "files_modified": ["/home/user/hproj2/notes.txt"]}"#;
        db.row("local-4386100b", "tool", Some(wrote), &[("ts", "1795530605.1098795"), ("call", "call_7fe5564b"), ("tool", "write_file")]);
        let fail = call("call_3ef375e0", "terminal", json!({ "command": "false" }));
        db.row("local-4386100b", "assistant", None, &[("ts", "1795530607.2816188"), ("calls", &fail), ("finish", "tool_calls")]);
        db.row("local-4386100b", "tool", Some(r#"{"output": "", "exit_code": 1, "error": null}"#), &[("ts", "1795530607.3301506"), ("call", "call_3ef375e0"), ("tool", "terminal")]);

        let out = src.poll();
        let names_in = |dir: &str| names(&out.iter().filter(|f| f.cwd.as_deref() == Some(Path::new(dir))).cloned().collect::<Vec<_>>());
        assert_eq!(names_in("/home/user/cliproj"), ["prompt", "agent_start", "agent_edit", "agent_commit", "reply_done"]);
        assert_eq!(names_in("/home/user/tuiproj"), ["prompt", "agent_start", "agent_edit", "agent_commit", "reply_done", "prompt", "reply_done"]);
        assert_eq!(names_in("/home/user/hproj2"), ["agent_edit", "agent_commit"]);
        assert_eq!(
            summary(&out),
            [
                "local-4386100b /home/user/hproj2/main.py +1 -0",
                "commit 00e8fcd in /home/user/hproj2",
                "local-14a3f762 /home/user/cliproj/main.py +1 -0",
                "commit ba5e751 in /home/user/cliproj",
                "local-3b5a1716 /home/user/tuiproj/main.py +1 -0",
                "commit 4f2d839 in /home/user/tuiproj",
            ]
        );
        match &out.iter().find(|f| f.kind.name() == "reply_done").unwrap().kind {
            Kind::ReplyDone { reply_words, tools, files_changed, .. } => assert_eq!((*reply_words, *tools, *files_changed), (4, 2, 1)),
            k => panic!("{k:?}"),
        }
    }
}
