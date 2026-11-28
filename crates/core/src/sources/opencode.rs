use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Map, Value};

use super::{work, words, Found, Source};
use crate::event::{App, Kind};
use crate::{prompt_shape, prompt_stats};

const EDIT_TOOLS: [&str; 4] = ["edit", "write", "multiedit", "patch"];
const PATCH_MARKERS: [&str; 4] = ["*** Update File: ", "*** Add File: ", "*** Delete File: ", "*** Move to: "];
// opencode 2.2 renamed apply_patch to patch and bash to shell.
const WORK_TOOLS: [&str; 6] = ["edit", "write", "apply_patch", "patch", "bash", "shell"];
const SHELL_TOOLS: [&str; 2] = ["bash", "shell"];

fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))
        .unwrap_or_default()
}

pub fn default_db() -> PathBuf {
    data_home().join("opencode/opencode.db")
}

// The Kilo CLI is an opencode fork with the same database schema.
pub fn kilo_db() -> PathBuf {
    std::env::var_os("KILO_DB").map(PathBuf::from).unwrap_or_else(|| data_home().join("kilo/kilo.db"))
}

// OpenCode desktop keeps its state in Electron's app data folder, named after the app's id, one per release channel
// (packages/desktop/src/main/identity.ts and lifecycle/configure.ts).
pub fn desktop_dirs() -> Vec<PathBuf> {
    let Some(base) = dirs::config_dir() else { return Vec::new() };
    ["ai.opencode.desktop", "ai.opencode.desktop.beta", "ai.opencode.desktop.dev"].iter().map(|id| base.join(id)).collect()
}

/// The folder of the tab open in OpenCode desktop. Its window shows every project and its title doesn't say which,
/// but the app writes each window's open tab to its own store a moment after a switch (packages/app/src/shell/tabs).
/// `None` unless every window the app has open shows the same folder, since which one is in front can't be told.
pub fn desktop_folder(dirs: &[PathBuf]) -> Option<PathBuf> {
    let mut found: Option<PathBuf> = None;
    for dir in dirs {
        let Ok(settings) = std::fs::read_to_string(dir.join("opencode.settings")) else { continue };
        let settings: Value = serde_json::from_str(&settings).ok()?;
        let ids: Vec<&str> = settings["windowIds"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        if ids.is_empty() {
            continue;
        }
        let conn = open(&dir.join("drafts.sqlite"))?;
        for id in ids {
            let folder = window_folder(&conn, id)?;
            if found.as_ref().is_some_and(|f| *f != folder) {
                return None;
            }
            found = Some(folder);
        }
    }
    found
}

// Only tabs on the app's own server: a folder on another machine says nothing about this one.
fn window_folder(conn: &Connection, id: &str) -> Option<PathBuf> {
    let name = format!("opencode.window.{id}.dat");
    let get = |key: &str| -> Option<Value> {
        let v: String = conn.query_row("SELECT value FROM state WHERE name = ?1 AND key = ?2", params![name, key], |r| r.get(0)).ok()?;
        serde_json::from_str(&v).ok()
    };
    let key = get("tabs.recent")?["key"].as_str()?.to_string();
    let folder = match key.strip_prefix("draft:") {
        Some(draft) => {
            let tabs = get("tabs")?;
            let tab = tabs.as_array()?.iter().find(|t| t["type"] == "draft" && t["draftID"] == draft)?;
            (tab["server"] == "sidecar").then(|| tab["directory"].as_str().map(str::to_string)).flatten()?
        }
        None => {
            let (server, _) = key.split_once('\n')?;
            (server == "sidecar").then_some(())?;
            get("tabs.info")?[&key]["directory"].as_str()?.to_string()
        }
    };
    Some(PathBuf::from(folder))
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn quoted(names: &[&str]) -> String {
    names.iter().map(|n| format!("'{n}'")).collect::<Vec<_>>().join(", ")
}

fn secs(ms: i64) -> f64 {
    ms as f64 / 1000.0
}

// opencode 1.x keeps message/part tables; 2.x writes session_v2/session_message (assistant parts
// live inside the message JSON) and leaves the old tables as a stale copy. 2.4 saves each assistant
// part as a session_part row instead and leaves the message's content empty.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Schema {
    Legacy,
    V2 { rows: bool },
}

// A session's assistant parts in either place. One message can hold both, the rows first.
fn v2_parts(rows: bool) -> String {
    let inline = "SELECT m.id AS msg, m.seq AS seq, 1 AS src, c.key AS ord, json_extract(c.value, '$.type') AS type,
            c.value AS value, m.time_created AS created, m.time_updated AS updated
         FROM session_message m, json_each(m.data, '$.content') c WHERE m.session_id = ?1 AND m.type = 'assistant'";
    if !rows {
        return format!("({inline})");
    }
    format!(
        "(SELECT m.id AS msg, m.seq AS seq, 0 AS src, p.ordinal AS ord, p.type AS type, p.data AS value,
            m.time_created AS created, p.time_updated AS updated
         FROM session_part p JOIN session_message m ON m.id = p.message_id WHERE p.session_id = ?1 AND m.type = 'assistant'
         UNION ALL {inline})"
    )
}

struct MsgRow {
    id: String,
    created: i64,
    user: bool,
    completed: Option<i64>,
    finish: Option<String>,
    error: bool,
    model: Option<String>,
    agent: Option<String>,
    text: Option<String>,
    app: bool,
}

struct PartRow {
    msg: String,
    kind: String,
    text: Option<String>,
    skip: bool,
    tool: Option<String>,
    path: Option<String>,
    patch: Option<String>,
    files: Option<String>,
    ts: Option<i64>,
}

struct WorkRow {
    id: String,
    tool: String,
    end: i64,
    state: Value,
}

impl Schema {
    pub(crate) fn detect(c: &Connection) -> Option<Schema> {
        let has = |t: &str| {
            c.query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1", [t], |_| Ok(())).is_ok()
        };
        if has("session_message") && has("session_v2") {
            Some(Schema::V2 { rows: has("session_part") })
        } else if has("session") && has("message") && has("part") {
            Some(Schema::Legacy)
        } else {
            None
        }
    }

    fn baseline(self, c: &Connection) -> rusqlite::Result<(Option<i64>, Option<i64>)> {
        let sql = match self {
            Schema::Legacy => {
                "SELECT MAX(time_created), MAX(time_updated) FROM message"
            }
            Schema::V2 { rows: false } => "SELECT MAX(time_created), MAX(time_updated) FROM session_message",
            Schema::V2 { rows: true } => {
                "SELECT MAX(time_created), MAX(MAX(time_updated), COALESCE((SELECT MAX(time_updated) FROM session_part), 0))
                 FROM session_message"
            }
        };
        c.query_row(sql, [], |r| Ok((r.get(0)?, r.get(1)?)))
    }

    // Every session with something new, subagent (child) sessions included: only their edits and commits count.
    fn touched(self, c: &Connection, cursor: i64) -> rusqlite::Result<Vec<(String, Option<String>, bool)>> {
        let sql = match self {
            Schema::Legacy => {
                "SELECT s.id, s.directory, s.parent_id IS NOT NULL FROM session s WHERE (
                    EXISTS (SELECT 1 FROM message m WHERE m.session_id = s.id AND m.time_updated > ?1)
                    OR EXISTS (SELECT 1 FROM part p WHERE p.session_id = s.id AND p.time_updated > ?1))"
            }
            Schema::V2 { rows: false } => {
                "SELECT s.id, s.directory, s.parent_id IS NOT NULL FROM session_v2 s WHERE
                    EXISTS (SELECT 1 FROM session_message m WHERE m.session_id = s.id AND m.time_updated > ?1)"
            }
            Schema::V2 { rows: true } => {
                "SELECT s.id, s.directory, s.parent_id IS NOT NULL FROM session_v2 s WHERE (
                    EXISTS (SELECT 1 FROM session_message m WHERE m.session_id = s.id AND m.time_updated > ?1)
                    OR EXISTS (SELECT 1 FROM session_part p WHERE p.session_id = s.id AND p.time_updated > ?1))"
            }
        };
        let mut st = c.prepare(sql)?;
        let rows = st.query_map([cursor], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    // T3 Code names a thread or writes a commit message in a session of its own, titled "T3 Code generate...", which
    // it deletes when done. None of it was typed. False where the table has no title column.
    fn side_job(self, c: &Connection, sid: &str) -> bool {
        let table = if self == Schema::Legacy { "session" } else { "session_v2" };
        let title: Option<String> = c.query_row(&format!("SELECT title FROM {table} WHERE id = ?1"), [sid], |r| r.get(0)).ok().flatten();
        title.is_some_and(|t| t.strip_prefix("T3 Code ").is_some_and(|op| op.starts_with("generate")))
    }

    // None where the table has no version column (older builds, forks).
    fn version(self, c: &Connection, sid: &str) -> Option<String> {
        let table = if self == Schema::Legacy { "session" } else { "session_v2" };
        c.query_row(&format!("SELECT version FROM {table} WHERE id = ?1"), [sid], |r| r.get(0)).ok().flatten()
    }

    fn work(self, c: &Connection, sid: &str, from: i64) -> rusqlite::Result<Vec<WorkRow>> {
        let (col, id, tool, end, from_where) = match self {
            Schema::Legacy => (
                "p.data",
                "p.id",
                "json_extract(p.data, '$.tool')",
                "json_extract(p.data, '$.state.time.end')",
                "FROM part p WHERE p.session_id = ?1 AND p.time_updated >= ?2 AND json_extract(p.data, '$.type') = 'tool'".to_string(),
            ),
            Schema::V2 { rows } => (
                "c.value",
                "json_extract(c.value, '$.id')",
                "json_extract(c.value, '$.name')",
                "json_extract(c.value, '$.time.completed')",
                format!("FROM {} c WHERE c.updated >= ?2 AND c.type = 'tool'", v2_parts(rows)),
            ),
        };
        let sql = format!(
            "SELECT {id}, {tool}, {end}, json_extract({col}, '$.state')
             {from_where} AND json_extract({col}, '$.state.status') = 'completed' AND {tool} IN ({names})
             AND ({tool} NOT IN ({shells}) OR json_extract({col}, '$.state.input.command') LIKE '%commit%') ORDER BY {end}, {id}",
            names = quoted(&WORK_TOOLS),
            shells = quoted(&SHELL_TOOLS),
        );
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map(params![sid, from], |r| {
            Ok(WorkRow {
                id: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                tool: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                end: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                state: r.get::<_, Option<String>>(3)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null),
            })
        })?;
        rows.collect()
    }

    fn messages(self, c: &Connection, sid: &str, from: i64) -> rusqlite::Result<Vec<MsgRow>> {
        let sql = match self {
            Schema::Legacy => {
                "SELECT id, time_created, json_extract(data, '$.role') = 'user', json_extract(data, '$.time.completed'),
                    json_extract(data, '$.finish'), COALESCE(json_type(data, '$.error') = 'object', 0),
                    json_extract(data, '$.modelID'), json_extract(data, '$.agent'), NULL, 0
                 FROM message WHERE session_id = ?1 AND time_created >= ?2 ORDER BY time_created, id"
            }
            Schema::V2 { .. } => {
                "SELECT id, time_created, type = 'user', json_extract(data, '$.time.completed'),
                    json_extract(data, '$.finish'), COALESCE(json_type(data, '$.error') = 'object', 0),
                    json_extract(data, '$.model.id'), json_extract(data, '$.agent'), json_extract(data, '$.text'),
                    COALESCE(json_type(data, '$.metadata') = 'object', 0)
                 FROM session_message WHERE session_id = ?1 AND type IN ('user', 'assistant') AND time_created >= ?2
                 ORDER BY seq"
            }
        };
        let mut st = c.prepare(sql)?;
        let rows = st.query_map(params![sid, from], |r| {
            Ok(MsgRow {
                id: r.get(0)?,
                created: r.get(1)?,
                user: r.get(2)?,
                completed: r.get(3).ok().flatten(),
                finish: r.get(4).ok().flatten(),
                error: r.get(5)?,
                model: r.get(6).ok().flatten(),
                agent: r.get(7).ok().flatten(),
                text: r.get(8).ok().flatten(),
                app: r.get::<_, i64>(9).unwrap_or(0) != 0,
            })
        })?;
        rows.collect()
    }

    fn parts(self, c: &Connection, sid: &str, from: i64) -> rusqlite::Result<Vec<PartRow>> {
        let sql = match self {
            Schema::Legacy => {
                "SELECT message_id, json_extract(data, '$.type'),
                    CASE WHEN json_extract(data, '$.type') = 'text' THEN json_extract(data, '$.text') END,
                    COALESCE(json_extract(data, '$.synthetic') OR json_extract(data, '$.ignored'), 0),
                    json_extract(data, '$.tool'), COALESCE(json_extract(data, '$.state.input.filePath'), json_extract(data, '$.state.input.path')),
                    json_extract(data, '$.state.input.patchText'), json_extract(data, '$.files'),
                    json_extract(data, '$.state.time.start')
                 FROM part WHERE session_id = ?1 AND time_created >= ?2 ORDER BY time_created, id"
                    .to_string()
            }
            Schema::V2 { rows } => format!(
                "SELECT c.msg, c.type, CASE WHEN c.type = 'text' THEN json_extract(c.value, '$.text') END,
                    0, json_extract(c.value, '$.name'), COALESCE(json_extract(c.value, '$.state.input.filePath'), json_extract(c.value, '$.state.input.path')),
                    json_extract(c.value, '$.state.input.patchText'), NULL, json_extract(c.value, '$.time.created')
                 FROM {} c WHERE c.created >= ?2 ORDER BY c.seq, c.src, c.ord",
                v2_parts(rows)
            ),
        };
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map(params![sid, from], |r| {
            Ok(PartRow {
                msg: r.get(0)?,
                kind: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                text: r.get(2).ok().flatten(),
                skip: r.get::<_, i64>(3).unwrap_or(0) != 0,
                tool: r.get(4).ok().flatten(),
                path: r.get(5).ok().flatten(),
                patch: r.get(6).ok().flatten(),
                files: r.get(7).ok().flatten(),
                ts: r.get(8).ok().flatten(),
            })
        })?;
        rows.collect()
    }
}

#[derive(Default)]
struct Msg {
    user: bool,
    // Sent from OpenCode's own app (desktop, or `opencode web`): only its composer gives a message `metadata`, the TUI never does.
    app: bool,
    created: i64,
    text: String,
    completed: Option<i64>,
    finish: Option<String>,
    errored: bool,
    model: Option<String>,
    words: usize,
    tools: Vec<i64>,
    files: BTreeSet<String>,
}

fn patch_files(text: &str, files: &mut BTreeSet<String>) {
    for line in text.lines() {
        if let Some(p) = PATCH_MARKERS.iter().find_map(|m| line.trim().strip_prefix(m)) {
            files.insert(p.trim().to_string());
        }
    }
}

fn build(rows: Vec<MsgRow>, parts: Vec<PartRow>) -> Vec<Msg> {
    let mut index = HashMap::new();
    let mut msgs: Vec<Msg> = Vec::new();
    for r in rows {
        if r.agent.as_deref() == Some("compaction") {
            continue;
        }
        index.insert(r.id, msgs.len());
        msgs.push(Msg {
            user: r.user,
            app: r.app,
            created: r.created,
            text: r.text.unwrap_or_default(),
            completed: r.completed,
            finish: r.finish,
            errored: r.error,
            model: r.model.filter(|m| !m.is_empty()),
            ..Msg::default()
        });
    }
    for p in parts {
        let Some(&i) = index.get(&p.msg) else { continue };
        let m = &mut msgs[i];
        match p.kind.as_str() {
            "text" if !p.skip => {
                let text = p.text.unwrap_or_default();
                if m.user {
                    if !m.text.is_empty() {
                        m.text.push('\n');
                    }
                    m.text.push_str(&text);
                } else {
                    m.words += words(&text);
                }
            }
            "tool" => {
                m.tools.push(p.ts.unwrap_or(m.created));
                let name = p.tool.as_deref().unwrap_or("");
                if let Some(patch) = p.patch.as_deref() {
                    patch_files(patch, &mut m.files);
                } else if EDIT_TOOLS.contains(&name) {
                    m.files.extend(p.path);
                }
            }
            "patch" => {
                let files: Vec<String> = p.files.and_then(|f| serde_json::from_str(&f).ok()).unwrap_or_default();
                m.files.extend(files);
            }
            _ => {}
        }
    }
    msgs.retain(|m| !m.user || !m.text.trim().is_empty());
    msgs
}

// opencode 2.2 names the file `path` (it was `filePath`), counts each file an edit or patch changed in `metadata.files`,
// says whether a write made the file only in its reply, and puts a shell command's output there too.
fn tool_work(ts: f64, cwd: Option<&Path>, sid: &str, tool: &str, state: &Value, out: &mut Vec<Found>) {
    let input = &state["input"];
    let meta = &state["metadata"];
    let count = |v: &Value| v.as_u64().and_then(|n| u32::try_from(n).ok());
    let reply = || -> String {
        let parts = state["content"].as_array().into_iter().flatten();
        parts.filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n")
    };
    if SHELL_TOOLS.contains(&tool) {
        let command = input["command"].as_str().unwrap_or("");
        let cmd = match input["workdir"].as_str() {
            Some(d) => format!("cd {} && {command}", work::shell_join([d])),
            None => command.to_string(),
        };
        let output = meta["output"].as_str().map_or_else(reply, str::to_string);
        work::commits(ts, cwd, sid, &cmd, &output, out);
        return;
    }
    if let Some(files) = meta["files"].as_array() {
        for f in files {
            if let (Some(path), Some(a), Some(d)) = (f["file"].as_str(), count(&f["additions"]), count(&f["deletions"])) {
                work::edit(ts, cwd, sid, path, (a, d), out);
            }
        }
        return;
    }
    let path = input["filePath"].as_str().or(input["path"].as_str());
    match tool {
        "edit" => {
            let diff = &meta["filediff"];
            if let (Some(path), Some(a), Some(d)) = (path.or(diff["file"].as_str()), count(&diff["additions"]), count(&diff["deletions"])) {
                work::edit(ts, cwd, sid, path, (a, d), out);
            }
        }
        "write" => {
            let new = match meta["exists"].as_bool() {
                Some(exists) => !exists,
                None => reply().starts_with("Created file"),
            };
            if let (Some(path), true) = (path, new) {
                work::edit(ts, cwd, sid, path, (input["content"].as_str().map_or(0, work::line_count), 0), out);
            }
        }
        _ => {
            for (path, a, d) in work::envelope_counts(input["patchText"].as_str().unwrap_or("")) {
                work::edit(ts, cwd, sid, &path, (a, d), out);
            }
        }
    }
}

#[derive(Default)]
struct Progress {
    u: i64,
    started: bool,
    done: bool,
    w: i64,
    wid: String,
}

pub struct OpenCode {
    name: &'static str,
    harness: &'static str,
    db: PathBuf,
    since: Option<i64>,
    cursor: i64,
    progress: HashMap<String, Progress>,
    health: super::Health,
}

impl OpenCode {
    pub fn new(db: PathBuf) -> OpenCode {
        OpenCode::named("opencode", "opencode", db)
    }

    pub fn named(name: &'static str, harness: &'static str, db: PathBuf) -> OpenCode {
        OpenCode { name, harness, db, since: None, cursor: 0, progress: HashMap::new(), health: super::Health::default() }
    }

    fn replay(&mut self, sid: &str, cwd: Option<PathBuf>, version: Option<String>, msgs: &[Msg], since: i64, out: &mut Vec<Found>) {
        let st = self.progress.entry(sid.to_string()).or_default();
        let users: Vec<usize> = (0..msgs.len()).filter(|&i| msgs[i].user).collect();
        for (n, &i) in users.iter().enumerate() {
            let next = users.get(n + 1).copied();
            let (u, unit) = (&msgs[i], &msgs[i + 1..next.unwrap_or(msgs.len())]);
            if u.created <= since || u.created < st.u {
                continue;
            }
            if u.created > st.u {
                *st = Progress { u: u.created, w: st.w, wid: std::mem::take(&mut st.wid), ..Progress::default() };
                let (words, pasted) = prompt_stats(&u.text);
                out.push(Found {
                    ts: secs(u.created),
                    cwd: cwd.clone(),
                    kind: Kind::Prompt {
                        words,
                        pasted,
                        model: None,
                        session: sid.to_string(),
                        shape: Some(prompt_shape(&u.text)),
                        version: version.clone(),
                        via: (u.app && self.harness == "opencode").then_some(App::OpencodeDesktop),
                        permissions: None,
                    },
                });
            }
            if !st.started {
                if let Some(&ts) = unit.iter().find_map(|m| m.tools.first()) {
                    st.started = true;
                    out.push(Found {
                        ts: secs(ts),
                        cwd: cwd.clone(),
                        kind: Kind::AgentStart { session: sid.to_string() },
                    });
                }
            }
            let finished =
                unit.last().is_some_and(|m| m.errored || (m.completed.is_some() && m.finish.as_deref() != Some("tool-calls")));
            if (next.is_some() || finished) && !st.done {
                st.done = true;
                let activity = unit.iter().map(|m| m.completed.unwrap_or(m.created)).max().unwrap_or(u.created);
                let end = next.map_or(activity, |j| activity.min(msgs[j].created));
                // On Windows opencode 1 names a file with backslashes in the tool call and with slashes in its patch part.
                let files: BTreeSet<String> = unit.iter().flat_map(|m| &m.files).map(|f| f.replace('\\', "/")).collect();
                out.push(Found {
                    ts: secs(end),
                    cwd: cwd.clone(),
                    kind: Kind::ReplyDone {
                        session: sid.to_string(),
                        reply_words: unit.iter().map(|m| m.words).sum(),
                        tools: unit.iter().map(|m| m.tools.len()).sum(),
                        files_changed: files.len(),
                        model: unit.iter().rev().find_map(|m| m.model.clone()),
                    },
                });
            }
        }
    }

    fn work(&mut self, c: &Connection, schema: Schema, sid: &str, cwd: Option<&Path>, since: i64, out: &mut Vec<Found>) -> rusqlite::Result<()> {
        let st = self.progress.entry(sid.to_string()).or_default();
        let from = if st.w > 0 { st.w } else { since };
        for r in schema.work(c, sid, from)? {
            if r.end <= since || (r.end, r.id.as_str()) <= (st.w, st.wid.as_str()) {
                continue;
            }
            (st.w, st.wid) = (r.end, r.id.clone());
            tool_work(secs(r.end), cwd, sid, &r.tool, &r.state, out);
        }
        Ok(())
    }

    fn scan(&mut self, c: &Connection, schema: Schema, since: i64, out: &mut Vec<Found>) -> rusqlite::Result<()> {
        let (_, updated) = schema.baseline(c)?;
        let mut read = 0;
        for (sid, dir, child) in schema.touched(c, self.cursor)? {
            if schema.side_job(c, &sid) {
                continue;
            }
            let cwd = dir.map(PathBuf::from);
            if !child {
                let from = self.progress.get(&sid).map_or(since, |p| p.u.max(since));
                let msgs = build(schema.messages(c, &sid, from)?, schema.parts(c, &sid, from)?);
                read += msgs.len() as u64;
                self.replay(&sid, cwd.clone(), schema.version(c, &sid), &msgs, since, out);
            }
            self.work(c, schema, &sid, cwd.as_deref(), since, out)?;
        }
        self.cursor = self.cursor.max(updated.unwrap_or(0));
        self.health.read += read;
        Ok(())
    }
}

pub(crate) fn open(db: &Path) -> Option<Connection> {
    let c = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;
    c.busy_timeout(Duration::from_secs(2)).ok()?;
    Some(c)
}

impl Source for OpenCode {
    fn name(&self) -> &'static str {
        self.name
    }

    fn harness(&self) -> &'static str {
        self.harness
    }

    fn health(&mut self) -> super::Health {
        std::mem::take(&mut self.health)
    }

    fn poll(&mut self) -> Vec<Found> {
        let mut out = Vec::new();
        let Some(c) = open(&self.db) else { return out };
        let Some(schema) = Schema::detect(&c) else { return out };
        let Some(since) = self.since else {
            if let Ok((created, updated)) = schema.baseline(&c) {
                let now = now_ms();
                self.since = Some(created.unwrap_or(now));
                self.cursor = updated.unwrap_or(now);
            }
            return out;
        };
        if self.scan(&c, schema, since, &mut out).is_err() {
            self.health.unreadable += 1;
            out.clear();
        }
        out.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        out
    }

    fn state(&self) -> Value {
        let sessions: Map<String, Value> = self
            .progress
            .iter()
            .map(|(k, p)| (k.clone(), json!({ "u": p.u, "s": p.started, "d": p.done, "w": p.w, "wid": p.wid })))
            .collect();
        json!({ "since": self.since, "cursor": self.cursor, "sessions": sessions })
    }

    fn restore(&mut self, state: &Value) {
        if let Some(s) = state.get("since").and_then(Value::as_i64) {
            self.since = Some(s);
        }
        if let Some(c) = state.get("cursor").and_then(Value::as_i64) {
            self.cursor = c;
        }
        for (k, p) in state.get("sessions").and_then(Value::as_object).into_iter().flatten() {
            self.progress.insert(
                k.clone(),
                Progress {
                    u: p.get("u").and_then(Value::as_i64).unwrap_or(0),
                    started: p.get("s").and_then(Value::as_bool).unwrap_or(false),
                    done: p.get("d").and_then(Value::as_bool).unwrap_or(false),
                    w: p.get("w").and_then(Value::as_i64).unwrap_or(0),
                    wid: p.get("wid").and_then(Value::as_str).unwrap_or_default().to_string(),
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Db {
        path: PathBuf,
        c: Connection,
        v2: bool,
        rows: bool,
    }

    impl Db {
        fn new(name: &str, v2: bool) -> Db {
            let path = std::env::temp_dir().join(format!("kairos-opencode-{}-{name}.db", std::process::id()));
            let _ = std::fs::remove_file(&path);
            let c = Connection::open(&path).unwrap();
            if v2 {
                c.execute_batch(
                    "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, version TEXT DEFAULT '2.2.5');
                     CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER,
                        time_created INTEGER, time_updated INTEGER, data TEXT);",
                )
                .unwrap();
            } else {
                c.execute_batch(
                    "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT);
                     CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER,
                        time_updated INTEGER, data TEXT);
                     CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER,
                        time_updated INTEGER, data TEXT);",
                )
                .unwrap();
            }
            Db { path, c, v2, rows: false }
        }

        fn rows(name: &str) -> Db {
            let mut db = Db::new(name, true);
            db.c.execute_batch(
                "CREATE TABLE session_part (message_id TEXT, session_id TEXT, ordinal INTEGER, type TEXT, call_id TEXT,
                    time_created INTEGER, time_updated INTEGER, data TEXT, PRIMARY KEY (message_id, ordinal));",
            )
            .unwrap();
            db.rows = true;
            db
        }

        fn session(&self, id: &str, parent: Option<&str>) {
            let table = if self.v2 { "session_v2" } else { "session" };
            self.c
                .execute(&format!("INSERT INTO {table} (id, parent_id, directory) VALUES (?1, ?2, '/w/p')"), params![id, parent])
                .unwrap();
        }

        fn msg(&self, sid: &str, id: &str, at: i64, data: &str) {
            if self.v2 {
                let ty = if data.contains(r#""text":"#) && !data.contains("content") { "user" } else { "assistant" };
                self.c
                    .execute(
                        "INSERT OR REPLACE INTO session_message VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
                        params![id, sid, ty, at, at, data],
                    )
                    .unwrap();
            } else {
                self.c
                    .execute("INSERT OR REPLACE INTO message VALUES (?1, ?2, ?3, ?3, ?4)", params![id, sid, at, data])
                    .unwrap();
            }
        }

        fn touch(&self, id: &str, at: i64, data: &str) {
            let (table, id_col) = if self.v2 { ("session_message", "id") } else { ("message", "id") };
            self.c
                .execute(
                    &format!("UPDATE {table} SET data = ?1, time_updated = ?2 WHERE {id_col} = ?3"),
                    params![data, at, id],
                )
                .unwrap();
        }

        fn part(&self, sid: &str, msg: &str, id: &str, at: i64, data: &str) {
            self.c
                .execute("INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?4, ?5)", params![id, msg, sid, at, data])
                .unwrap();
        }

        fn user(&self, sid: &str, id: &str, at: i64, text: &str) {
            if self.v2 {
                self.msg(sid, id, at, &json!({ "text": text, "time": { "created": at } }).to_string());
            } else {
                self.msg(sid, id, at, &json!({ "role": "user", "time": { "created": at } }).to_string());
                self.part(sid, id, &format!("{id}-t"), at, &json!({ "type": "text", "text": text }).to_string());
            }
        }

        fn assistant(&self, sid: &str, id: &str, at: i64, done: Option<(i64, &str)>) {
            let mut time = json!({ "created": at });
            let mut data = json!({ "role": "assistant", "modelID": "m-1", "time": time.clone() });
            if let Some((end, finish)) = done {
                time["completed"] = json!(end);
                data["time"] = time;
                data["finish"] = json!(finish);
            }
            if self.v2 {
                data["content"] = json!([]);
                data["model"] = json!({ "id": "m-1" });
            }
            self.msg(sid, id, at, &data.to_string());
        }

        fn finish(&self, id: &str, at: i64, end: i64, finish: &str) {
            let mut data = json!({ "role": "assistant", "modelID": "m-1", "time": { "created": at, "completed": end }, "finish": finish });
            if self.v2 {
                data["content"] = self.content(id);
                data["model"] = json!({ "id": "m-1" });
            }
            self.touch(id, end + 1, &data.to_string());
        }

        fn content(&self, id: &str) -> Value {
            let raw: String =
                self.c.query_row("SELECT data FROM session_message WHERE id = ?1", [id], |r| r.get(0)).unwrap();
            serde_json::from_str::<Value>(&raw).unwrap()["content"].clone()
        }

        fn add_part(&self, sid: &str, msg: &str, n: usize, at: i64, part: Value) {
            if self.rows {
                self.c
                    .execute(
                        "INSERT INTO session_part VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
                        params![msg, sid, n as i64, part["type"].as_str(), part["id"].as_str(), at, part.to_string()],
                    )
                    .unwrap();
            } else if self.v2 {
                let raw: String = self
                    .c
                    .query_row("SELECT data FROM session_message WHERE id = ?1", [msg], |r| r.get(0))
                    .unwrap();
                let mut data: Value = serde_json::from_str(&raw).unwrap();
                data["content"].as_array_mut().unwrap().push(part);
                self.touch(msg, at, &data.to_string());
            } else {
                self.part(sid, msg, &format!("{msg}-{n}"), at, &part.to_string());
            }
        }

        fn text(&self, sid: &str, msg: &str, n: usize, at: i64, text: &str) {
            self.add_part(sid, msg, n, at, json!({ "type": "text", "text": text }));
        }

        fn tool(&self, sid: &str, msg: &str, n: usize, at: i64, name: &str, path: Option<&str>) {
            let input = json!({ "filePath": path });
            let part = if self.v2 {
                json!({ "type": "tool", "name": name, "time": { "created": at }, "state": { "input": input } })
            } else {
                json!({ "type": "tool", "tool": name, "state": { "time": { "start": at }, "input": input } })
            };
            self.add_part(sid, msg, n, at, part);
        }
    }

    impl Drop for Db {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn names(out: &[Found]) -> Vec<&'static str> {
        out.iter().map(|f| f.kind.name()).collect()
    }

    fn history(db: &Db) {
        db.session("s1", None);
        db.user("s1", "m1", 1_000_000, "an old prompt from before install");
        db.assistant("s1", "m2", 1_000_500, Some((1_005_000, "stop")));
        db.text("s1", "m2", 1, 1_000_600, "old reply");
    }

    fn turn_flow(db: Db) {
        history(&db);
        let mut src = OpenCode::new(db.path.clone());
        assert!(src.poll().is_empty());
        assert!(src.poll().is_empty());

        db.user("s1", "m3", 2_000_000, "please fix the failing parser test now");
        let out = src.poll();
        assert_eq!(names(&out), ["prompt"]);
        assert_eq!(out[0].cwd.as_deref(), Some(Path::new("/w/p")));
        assert!(matches!(&out[0].kind, Kind::Prompt { words: 7, pasted: false, model: None, .. }));
        let version = if db.v2 { Some("2.2.5") } else { None };
        assert!(matches!(&out[0].kind, Kind::Prompt { version: v, .. } if v.as_deref() == version));
        assert_eq!(out[0].ts, 2000.0);

        db.assistant("s1", "m4", 2_000_100, None);
        db.text("s1", "m4", 1, 2_000_200, "Looking at it.");
        db.tool("s1", "m4", 2, 2_005_000, "edit", Some("/w/p/a.rs"));
        let out = src.poll();
        assert_eq!(names(&out), ["agent_start"]);
        assert_eq!(out[0].ts, 2005.0);

        db.tool("s1", "m4", 3, 2_006_000, "bash", None);
        db.finish("m4", 2_000_100, 2_010_000, "tool-calls");
        assert!(src.poll().is_empty());

        db.assistant("s1", "m5", 2_010_100, None);
        db.text("s1", "m5", 1, 2_020_000, "Fixed it in a.rs.");
        db.tool("s1", "m5", 2, 2_020_001, "write", Some("/w/p/a.rs"));
        db.finish("m5", 2_010_100, 2_060_000, "stop");
        let out = src.poll();
        assert_eq!(names(&out), ["reply_done"]);
        assert_eq!(out[0].ts, 2060.0);
        match &out[0].kind {
            Kind::ReplyDone { reply_words, tools, files_changed, model, session } => {
                assert_eq!((*reply_words, *tools, *files_changed), (7, 3, 1));
                assert_eq!((model.as_deref(), session.as_str()), (Some("m-1"), "s1"));
            }
            k => panic!("{k:?}"),
        }
        assert!(src.poll().is_empty());

        let mut again = OpenCode::new(db.path.clone());
        again.restore(&src.state());
        assert!(again.poll().is_empty());
        db.user("s1", "m6", 3_000_000, "now add a test for the parser");
        assert_eq!(names(&again.poll()), ["prompt"]);
    }

    #[test]
    fn a_turn_becomes_prompt_start_done_legacy() {
        turn_flow(Db::new("legacy-turn", false));
    }

    #[test]
    fn a_turn_becomes_prompt_start_done_v2() {
        turn_flow(Db::new("v2-turn", true));
    }

    #[test]
    fn a_turn_becomes_prompt_start_done_part_rows() {
        turn_flow(Db::rows("rows-turn"));
    }

    #[test]
    fn a_prompt_sent_from_the_app_says_so_and_one_from_the_tui_does_not() {
        let db = Db::new("sent-from-app", true);
        history(&db);
        let mut src = OpenCode::new(db.path.clone());
        src.poll();
        let meta = json!({ "displayText": "write a hello program", "comments": [], "attachments": [], "agent": "build" });
        db.msg("s1", "m3", 2_000_000, &json!({ "text": "write a hello program", "metadata": meta, "time": { "created": 2_000_000 } }).to_string());
        db.user("s1", "m5", 2_100_000, "now add a test for it");
        let via: Vec<_> = src.poll().into_iter().filter_map(|f| match f.kind {
            Kind::Prompt { via, .. } => Some(via),
            _ => None,
        }).collect();
        assert_eq!(via, [Some(App::OpencodeDesktop), None]);
    }

    #[test]
    fn a_file_named_with_either_slash_on_windows_is_one_file() {
        let db = Db::new("windows-slashes", false);
        history(&db);
        let mut src = OpenCode::new(db.path.clone());
        src.poll();
        db.user("s1", "m3", 2_000_000, "write a hello program");
        db.assistant("s1", "m4", 2_000_100, None);
        db.tool("s1", "m4", 1, 2_001_000, "write", Some("D:\\w\\p\\hello.py"));
        db.add_part("s1", "m4", 2, 2_002_000, json!({ "type": "patch", "hash": "abc", "files": ["D:/w/p/hello.py"] }));
        db.finish("m4", 2_000_100, 2_010_000, "stop");
        let files = src.poll().iter().find_map(|f| match f.kind {
            Kind::ReplyDone { files_changed, .. } => Some(files_changed),
            _ => None,
        });
        assert_eq!(files, Some(1));
    }

    #[test]
    fn interrupted_turn_closes_when_next_prompt_arrives() {
        let db = Db::new("interrupted", false);
        history(&db);
        let mut src = OpenCode::new(db.path.clone());
        src.poll();
        db.user("s1", "m3", 2_000_000, "please fix the failing parser test now");
        db.assistant("s1", "m4", 2_000_100, None);
        db.tool("s1", "m4", 1, 2_003_000, "bash", None);
        assert_eq!(names(&src.poll()), ["prompt", "agent_start"]);
        db.user("s1", "m5", 2_100_000, "stop, do something else instead");
        let out = src.poll();
        assert_eq!(names(&out), ["reply_done", "prompt"]);
        assert_eq!(out[0].ts, 2_000.1);

        let mut aborted = json!({ "role": "assistant", "time": { "created": 2_100_100, "completed": 2_101_000 } });
        aborted["error"] = json!({ "name": "MessageAbortedError" });
        db.msg("s1", "m6", 2_100_100, &aborted.to_string());
        let out = src.poll();
        assert_eq!(names(&out), ["reply_done"]);
        assert_eq!(out[0].ts, 2_101.0);
    }

    #[test]
    fn injected_messages_and_child_sessions_are_not_prompts() {
        let db = Db::new("injected", false);
        history(&db);
        db.session("child", Some("s1"));
        let mut src = OpenCode::new(db.path.clone());
        src.poll();

        db.msg("s1", "m3", 2_000_000, r#"{"role":"user","time":{"created":2000000}}"#);
        db.part("s1", "m3", "m3-t", 2_000_000, r#"{"type":"text","text":"Called the Read tool","synthetic":true}"#);
        db.part("s1", "m3", "m3-c", 2_000_001, r#"{"type":"compaction","auto":true}"#);
        db.assistant("s1", "m4", 2_000_100, Some((2_001_000, "stop")));
        db.text("s1", "m4", 1, 2_000_200, "summary of the conversation so far");
        db.msg("s1", "m5", 2_000_300, r#"{"role":"assistant","agent":"compaction","time":{"created":2000300,"completed":2000400},"finish":"stop"}"#);
        db.user("child", "c1", 2_002_000, "look into the parser module");
        db.assistant("child", "c2", 2_002_100, Some((2_003_000, "stop")));
        assert!(src.poll().is_empty());
    }

    #[test]
    fn t3_codes_own_jobs_are_not_prompts() {
        for v2 in [false, true] {
            let db = Db::new(&format!("t3-{v2}"), v2);
            let table = if v2 { "session_v2" } else { "session" };
            db.c.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN title TEXT")).unwrap();
            history(&db);
            let mut src = OpenCode::new(db.path.clone());
            assert!(src.poll().is_empty());
            db.session("job", None);
            db.c.execute(&format!("UPDATE {table} SET title = 'T3 Code generateThreadTitle' WHERE id = 'job'"), []).unwrap();
            db.user("job", "j1", 2_000_000, "Write a short title for this thread. User message: please write a small hello program");
            db.assistant("job", "j2", 2_000_100, Some((2_001_000, "stop")));
            db.session("thread", None);
            db.c.execute(&format!("UPDATE {table} SET title = 'T3 Code thread' WHERE id = 'thread'"), []).unwrap();
            db.user("thread", "t1", 2_002_000, "please write a small hello program in python");
            let out = src.poll();
            let prompts: Vec<&str> = out
                .iter()
                .filter_map(|f| match &f.kind {
                    Kind::Prompt { session, .. } => Some(session.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(prompts, ["thread"], "v2 {v2}");
        }
    }

    impl Db {
        fn finished(&self, sid: &str, msg: &str, n: usize, end: i64, name: &str, status: &str, input: Value, metadata: Value) {
            let id = format!("{msg}-{n}");
            let state = json!({ "status": status, "input": input, "metadata": metadata });
            let part = if self.v2 {
                json!({ "type": "tool", "id": id, "name": name, "time": { "created": end - 50, "completed": end }, "state": state })
            } else {
                let mut state = state;
                state["time"] = json!({ "start": end - 50, "end": end });
                json!({ "type": "tool", "tool": name, "state": state })
            };
            self.add_part(sid, msg, n, end, part);
        }
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

    fn work_flow(db: Db) {
        history(&db);
        db.session("child", Some("s1"));
        let mut src = OpenCode::new(db.path.clone());
        assert!(src.poll().is_empty());

        db.user("s1", "m3", 2_000_000, "please fix the failing parser test now");
        db.assistant("s1", "m4", 2_000_100, None);
        let filediff = |a: i64, d: i64| json!({ "filediff": { "file": "/w/p/a.rs", "additions": a, "deletions": d, "patch": "ignored" } });
        db.finished("s1", "m4", 1, 2_001_000, "edit", "completed", json!({ "filePath": "/w/p/a.rs" }), filediff(3, 1));
        db.finished("s1", "m4", 2, 2_002_000, "write", "completed", json!({ "filePath": "/w/p/new.rs", "content": "a\nb\n" }), json!({ "exists": false }));
        db.finished("s1", "m4", 3, 2_003_000, "write", "completed", json!({ "filePath": "/w/p/old.rs", "content": "a\nb\n" }), json!({ "exists": true }));
        db.finished("s1", "m4", 4, 2_004_000, "edit", "error", json!({ "filePath": "/w/p/a.rs" }), json!({}));
        let patch = "*** Begin Patch\n*** Update File: src/x.rs\n@@\n keep\n-a\n+b\n+c\n*** Add File: n.rs\n+1\n*** Delete File: d.rs\n*** End Patch";
        db.finished("s1", "m4", 5, 2_005_000, "apply_patch", "completed", json!({ "patchText": patch }), json!({}));
        let commit = |at: i64, n: usize, cmd: &str, out: &str| {
            db.finished("s1", "m4", n, at, "bash", "completed", json!({ "command": cmd, "workdir": "sub" }), json!({ "output": out, "exit": 0 }));
        };
        commit(2_006_000, 6, "git add -A && git commit -m 'fix it'", "[main abc1234] fix it\n 1 file changed\n");
        commit(2_007_000, 7, "git commit -m again", "nothing to commit, working tree clean\n");
        commit(2_008_000, 8, "git status", "[main fffffff] not a commit\n");
        db.assistant("child", "c2", 2_008_500, None);
        db.finished("child", "c2", 1, 2_009_000, "edit", "completed", json!({ "filePath": "/w/p/b.rs" }), json!({ "filediff": { "file": "/w/p/b.rs", "additions": 5, "deletions": 0 } }));
        let out = src.poll();
        let expected = [
            "s1 /w/p/a.rs +3 -1",
            "s1 /w/p/new.rs +2 -0",
            "s1 src/x.rs +2 -1",
            "s1 n.rs +1 -0",
            "s1 commit abc1234 in /w/p/sub",
            "child /w/p/b.rs +5 -0",
        ];
        assert_eq!(work(&out), expected);
        assert_eq!(names(&out).iter().filter(|n| matches!(**n, "prompt" | "agent_start")).count(), 2);
        assert!(src.poll().is_empty());

        let mut again = OpenCode::new(db.path.clone());
        again.restore(&src.state());
        assert!(again.poll().is_empty());
        db.finished("s1", "m4", 9, 2_020_000, "edit", "completed", json!({ "filePath": "/w/p/c.rs" }), filediff(1, 0));
        assert_eq!(work(&again.poll()), ["s1 /w/p/c.rs +1 -0"]);
    }

    #[test]
    fn edits_and_commits_from_tool_results_legacy() {
        work_flow(Db::new("legacy-work", false));
    }

    #[test]
    fn edits_and_commits_from_tool_results_v2() {
        work_flow(Db::new("v2-work", true));
    }

    #[test]
    fn edits_and_commits_from_tool_results_part_rows() {
        work_flow(Db::rows("rows-work"));
    }

    // Rows opencode 2.4.1 wrote for one prompt (write, edit, write over a file, commit), shortened.
    #[test]
    fn a_real_opencode_2_4_session() {
        let db = Db::rows("real-24");
        history(&db);
        let mut src = OpenCode::new(db.path.clone());
        assert!(src.poll().is_empty());

        let t = 1_795_548_259_000;
        let user = json!({ "time": { "created": t + 223 }, "text": "add a small vector helper module with a dot product and a norm, then commit it", "files": [] });
        db.msg("s1", "m3", t + 223, &user.to_string());
        let assistant = |id: &str, at: i64, end: i64, finish: &str| {
            let data = json!({
                "time": { "created": at, "firstToken": at + 1008, "streamed": at + 1017, "completed": end },
                "agent": "build", "model": { "id": "test-model", "providerID": "fake" }, "content": [],
                "finish": finish, "rawFinish": finish.replace('-', "_"), "cost": 0
            });
            db.msg("s1", id, at, &data.to_string());
        };
        let tool = |msg: &str, n: usize, id: &str, at: i64, end: i64, name: &str, input: Value, reply: &str, metadata: Value| {
            let state = json!({ "status": "completed", "input": input, "content": [{ "type": "text", "text": reply }], "metadata": metadata });
            let part = json!({ "type": "tool", "id": id, "name": name, "executed": false, "state": state, "time": { "created": at, "ran": at + 4, "completed": end } });
            db.add_part("s1", msg, n, end, part);
        };
        let plain = json!({ "truncated": false });

        assistant("m4", t + 242, t + 1284, "tool-calls");
        db.text("s1", "m4", 0, t + 1258, "I'll add the vector helpers first.");
        let vec_py = "def dot(a, b):\n    return sum(x * y for x, y in zip(a, b))\n\n\ndef norm(a):\n    return dot(a, a)\n";
        tool("m4", 1, "call_0", t + 1253, t + 1264, "write", json!({ "path": "vec.py", "content": vec_py }), "Created file successfully: vec.py", plain.clone());
        assistant("m5", t + 1296, t + 2333, "tool-calls");
        let edited = json!({ "files": [{ "file": "vec.py", "patch": "Index: vec.py", "status": "modified", "additions": 2, "deletions": 1 }], "truncated": false });
        let edit = json!({ "path": "vec.py", "oldString": "    return dot(a, a)\n", "newString": "    import math\n    return math.sqrt(dot(a, a))\n" });
        tool("m5", 0, "call_1", t + 2305, t + 2314, "edit", edit, "Edited vec.py (1 replacement)", edited);
        assistant("m6", t + 2347, t + 3388, "tool-calls");
        tool("m6", 0, "call_2", t + 3357, t + 3367, "write", json!({ "path": "README.md", "content": "# demo\n" }), "Wrote file successfully: README.md", plain);
        assistant("m7", t + 3404, t + 4504, "tool-calls");
        let ran = json!({ "status": "completed", "truncated": false, "exit": 0 });
        let commit = "[main b782208] add vec helpers\n 2 files changed, 9 insertions(+)\n create mode 100644 vec.py\n";
        tool("m7", 0, "call_3", t + 4416, t + 4498, "shell", json!({ "command": "git add -A && git commit -m 'add vec helpers'" }), commit, ran);
        assistant("m8", t + 4516, t + 5535, "stop");
        db.text("s1", "m8", 0, t + 5527, "Added vec.py with dot and norm, updated the README and committed it all.");

        let out = src.poll();
        assert_eq!(names(&out), ["prompt", "agent_start", "agent_edit", "agent_edit", "agent_commit", "reply_done"]);
        assert_eq!(work(&out), ["s1 vec.py +6 -0", "s1 vec.py +2 -1", "s1 commit b782208 in /w/p"]);
        assert!(matches!(&out[0].kind, Kind::Prompt { words: 16, pasted: false, .. }));
        match &out[5].kind {
            Kind::ReplyDone { reply_words, tools, files_changed, model, .. } => {
                assert_eq!((*reply_words, *tools, *files_changed, model.as_deref()), (19, 4, 2, Some("test-model")));
            }
            k => panic!("{k:?}"),
        }
        assert!(src.poll().is_empty());
    }

    #[test]
    fn opencode_2_2_names_the_file_path_and_counts_each_file_an_edit_or_patch_changed() {
        let db = Db::new("v22-work", true);
        history(&db);
        let mut src = OpenCode::new(db.path.clone());
        assert!(src.poll().is_empty());

        db.user("s1", "m3", 2_000_000, "add the vector helpers to the project please");
        db.assistant("s1", "m4", 2_000_100, None);
        let tool = |n: usize, end: i64, name: &str, input: Value, metadata: Value, reply: &str| {
            let state = json!({ "status": "completed", "input": input, "content": [{ "type": "text", "text": reply }], "metadata": metadata });
            let part = json!({ "type": "tool", "id": format!("m4-{n}"), "name": name, "time": { "created": end - 50, "completed": end }, "state": state });
            db.add_part("s1", "m4", n, end, part);
        };
        let file = |file: &str, a: u32, d: u32| json!({ "file": file, "patch": "ignored", "status": "modified", "additions": a, "deletions": d });
        let unknown = json!({ "truncated": false });
        tool(1, 2_001_000, "write", json!({ "path": "/w/p/vec.py", "content": "a\nb\n" }), unknown.clone(), "Created file successfully: vec.py");
        tool(2, 2_002_000, "write", json!({ "path": "/w/p/old.txt", "content": "a\n" }), unknown.clone(), "Wrote file successfully: old.txt");
        tool(3, 2_003_000, "edit", json!({ "path": "/w/p/vec.py", "oldString": "a", "newString": "b" }), json!({ "files": [file("vec.py", 7, 1)] }), "Edited vec.py (1 replacement)");
        let patch = "*** Begin Patch\n*** Update File: x.rs\n@@\n-a\n+b\n+c\n*** Add File: n.rs\n+1\n*** End Patch";
        let applied = json!({ "applied": [], "files": [file("x.rs", 2, 1), file("n.rs", 1, 0)] });
        tool(4, 2_004_000, "patch", json!({ "patchText": patch }), applied, "Success. Updated the following files:\nM x.rs\nA n.rs");
        let shell = json!({ "status": "completed", "exit": 0, "truncated": false });
        tool(5, 2_005_000, "shell", json!({ "command": "git commit -am 'add vec'", "workdir": "/w/p" }), shell.clone(), "[main abc1234] add vec\n 1 file changed\n");
        tool(6, 2_005_500, "shell", json!({ "command": "python3 vec.py", "workdir": "/w/p" }), shell, "[main fffffff] not a commit\n");
        db.finish("m4", 2_000_100, 2_006_000, "stop");

        let out = src.poll();
        assert_eq!(work(&out), ["s1 /w/p/vec.py +2 -0", "s1 vec.py +7 -1", "s1 x.rs +2 -1", "s1 n.rs +1 -0", "s1 commit abc1234 in /w/p"]);
        let files = out.iter().find_map(|f| match f.kind {
            Kind::ReplyDone { files_changed, .. } => Some(files_changed),
            _ => None,
        });
        assert_eq!(files, Some(4));
    }

    struct Desktop(PathBuf);

    impl Desktop {
        fn new(name: &str, windows: &[&str]) -> Desktop {
            let dir = std::env::temp_dir().join(format!("kairos-opencode-desktop-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("opencode.settings"), json!({ "windowIds": windows, "backgroundColor": "#fff" }).to_string()).unwrap();
            Connection::open(dir.join("drafts.sqlite"))
                .unwrap()
                .execute_batch("CREATE TABLE state (name TEXT, key TEXT, value TEXT, updated_at INTEGER, PRIMARY KEY (name, key));")
                .unwrap();
            Desktop(dir)
        }

        fn set(&self, window: &str, key: &str, value: Value) {
            Connection::open(self.0.join("drafts.sqlite"))
                .unwrap()
                .execute("INSERT OR REPLACE INTO state VALUES (?1, ?2, ?3, 0)", params![format!("opencode.window.{window}.dat"), key, value.to_string()])
                .unwrap();
        }

        // A window on a session tab in `folder`, with a draft tab open next to it.
        fn window(&self, id: &str, server: &str, folder: &str) {
            let key = format!("{server}\n/server/c2lkZWNhcg/session/ses_{id}");
            self.set(id, "tabs", json!([{ "type": "session", "server": server, "sessionId": format!("ses_{id}") },
                                        { "type": "draft", "draftID": format!("d{id}"), "server": server, "directory": "/w/draft" }]));
            let mut info = Map::new();
            info.insert(key.clone(), json!({ "title": "t", "directory": folder, "prompted": true }));
            self.set(id, "tabs.info", Value::Object(info));
            self.set(id, "tabs.recent", json!({ "key": key }));
        }
    }

    #[test]
    fn the_desktop_window_is_in_the_folder_of_its_open_tab() {
        let d = Desktop::new("one", &["w1"]);
        d.window("w1", "sidecar", "/w/p");
        assert_eq!(desktop_folder(&[d.0.clone()]), Some(PathBuf::from("/w/p")));
        d.set("w1", "tabs.recent", json!({ "key": "draft:dw1" }));
        assert_eq!(desktop_folder(&[d.0.clone()]), Some(PathBuf::from("/w/draft")));
        assert_eq!(desktop_folder(&[d.0.join("nothing-here")]), None);
    }

    #[test]
    fn the_desktop_window_has_no_folder_when_its_windows_disagree_or_the_tab_is_elsewhere() {
        let two = Desktop::new("two", &["w1", "w2"]);
        two.window("w1", "sidecar", "/w/p");
        two.window("w2", "sidecar", "/w/q");
        assert_eq!(desktop_folder(&[two.0.clone()]), None);
        two.window("w2", "sidecar", "/w/p");
        assert_eq!(desktop_folder(&[two.0.clone()]), Some(PathBuf::from("/w/p")));

        // An open window whose tab can't be read might be the one in front.
        let blank = Desktop::new("blank", &["w1", "w2"]);
        blank.window("w1", "sidecar", "/w/p");
        assert_eq!(desktop_folder(&[blank.0.clone()]), None);

        let remote = Desktop::new("remote", &["w1"]);
        remote.window("w1", "work-box", "/w/p");
        assert_eq!(desktop_folder(&[remote.0.clone()]), None);
    }
}
