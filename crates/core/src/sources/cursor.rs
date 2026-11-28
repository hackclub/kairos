use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Value};

use super::snapshot::{mtime, now, Snapshots, Turn};
use super::{uri_path, work, Found, Source};
use crate::{prompt_shape, prompt_stats, Shape};

const BATCH: i64 = 2000;
const BATCHES: usize = 10;
const MAX_ROW: i64 = 10 * 1024 * 1024;
// A reply counts as finished once its last bubble has not changed for this long (Cursor rewrites the
// streaming bubble in place and writes no explicit end marker we can rely on).
const STABLE: f64 = 10.0;
const RESCAN: f64 = 60.0;
const FORGET: f64 = 6.0 * 3600.0;
// How long bubbles of a composer whose header row we have not seen wait (it marks subagents).
const HOLD: f64 = 30.0;
// Cursor 3 rewrites a bubble in place (same rowid) as a reply streams and as a tool call finishes, so the bubbles of a
// turn are read again by key until they have not changed for SETTLED, or a running tool call for up to REREAD.
const SETTLED: f64 = 60.0;
const REREAD: f64 = 1800.0;
const EMPTY_CONTENT: &str = "composer.content.e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const META: &str = "SELECT COALESCE(json_extract(v, '$.isSubagent'), 0) OR json_type(v, '$.subagentInfo') IS NOT NULL,
    json_extract(v, '$.status'), COALESCE(json_extract(v, '$.modelConfig.modelName'), json_extract(v, '$.modelName'))
FROM (SELECT CASE WHEN json_valid(CAST(value AS TEXT)) THEN CAST(value AS TEXT) END AS v
      FROM cursorDiskKV WHERE key = ?1 AND length(value) <= ?2)";
const PATCH_MARKERS: [&str; 3] = ["*** Update File: ", "*** Add File: ", "*** Delete File: "];

pub fn default_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = dirs::config_dir().map(|c| c.join("Cursor")).into_iter().collect();
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".config/cursor"));
    }
    roots.dedup();
    roots
}

// Only the fields we count, pulled out by SQLite so multi-megabyte bubbles never reach us whole.
// Rows that are not JSON still come back (all NULL) so the watermark moves past them.
macro_rules! rows_from {
    ($source:literal) => {
        concat!(
            "WITH r AS (",
            $source,
            "),
  j AS (SELECT id, key, CASE WHEN v IS NOT NULL AND json_valid(v) THEN v END AS v FROM r)
SELECT id, key,
    json_extract(v, '$.type'),
    json_extract(v, '$.text'),
    COALESCE(json_extract(v, '$.createdAt'), json_extract(v, '$.timestamp')),
    json_extract(v, '$.toolFormerData.name'),
    json_extract(v, '$.toolFormerData.status'),
    CASE WHEN json_valid(json_extract(v, '$.toolFormerData.params')) THEN COALESCE(
        json_extract(json_extract(v, '$.toolFormerData.params'), '$.relativeWorkspacePath'),
        json_extract(json_extract(v, '$.toolFormerData.params'), '$.targetFile'),
        json_extract(json_extract(v, '$.toolFormerData.params'), '$.target_file'),
        json_extract(json_extract(v, '$.toolFormerData.params'), '$.path')) END,
    CASE WHEN json_valid(json_extract(v, '$.toolFormerData.rawArgs')) THEN COALESCE(
        json_extract(json_extract(v, '$.toolFormerData.rawArgs'), '$.target_file'),
        json_extract(json_extract(v, '$.toolFormerData.rawArgs'), '$.path'),
        json_extract(json_extract(v, '$.toolFormerData.rawArgs'), '$.patch'),
        json_extract(json_extract(v, '$.toolFormerData.rawArgs'), '$.input'))
        ELSE json_extract(v, '$.toolFormerData.rawArgs') END,
    json_extract(v, '$.codeBlocks[0].uri._fsPath'),
    COALESCE(json_extract(v, '$.modelName'), json_extract(v, '$.modelInfo.modelName'), json_extract(v, '$.model_name'),
        json_extract(v, '$.modelId'), json_extract(v, '$.modelConfig.modelName'), json_extract(v, '$.selectedModel'),
        json_extract(v, '$.model')),
    json_extract(v, '$.timingInfo.clientEndTime'),
    COALESCE(json_extract(v, '$.isSubagent'), 0) OR json_type(v, '$.subagentInfo') IS NOT NULL,
    json_extract(v, '$.status'),
    v IS NOT NULL,
    CASE WHEN json_valid(json_extract(v, '$.toolFormerData.result')) THEN
        json_extract(json_extract(v, '$.toolFormerData.result'), '$.beforeContentId') END,
    CASE WHEN json_valid(json_extract(v, '$.toolFormerData.result')) THEN
        json_extract(json_extract(v, '$.toolFormerData.result'), '$.afterContentId') END,
    CASE WHEN json_valid(json_extract(v, '$.toolFormerData.params')) THEN
        json_extract(json_extract(v, '$.toolFormerData.params'), '$.command') END,
    CASE WHEN json_valid(json_extract(v, '$.toolFormerData.params')) THEN
        json_extract(json_extract(v, '$.toolFormerData.params'), '$.cwd') END,
    CASE WHEN json_valid(json_extract(v, '$.toolFormerData.result')) THEN
        substr(json_extract(json_extract(v, '$.toolFormerData.result'), '$.output'), 1, 4096) END
FROM j"
        )
    };
}

const ROWS: &str = rows_from!(
    "SELECT rowid AS id, key, CASE WHEN length(value) <= ?3 THEN CAST(value AS TEXT) END AS v
    FROM cursorDiskKV NOT INDEXED
    WHERE rowid > ?1 AND ((key >= 'bubbleId:' AND key < 'bubbleId;') OR (key >= 'composerData:' AND key < 'composerData;'))
    ORDER BY rowid LIMIT ?2"
);
const ROW: &str =
    rows_from!("SELECT rowid AS id, key, CASE WHEN length(value) <= ?2 THEN CAST(value AS TEXT) END AS v FROM cursorDiskKV WHERE key = ?1");
const CONTENT: &str = "SELECT CAST(value AS TEXT) FROM cursorDiskKV WHERE key = ?1 AND length(value) <= ?2";
const ORIGINALS: &str = "SELECT o.key, json_extract(o.value, '$.contentKey')
FROM cursorDiskKV, json_each(CAST(cursorDiskKV.value AS TEXT), '$.originalFileStates') AS o
WHERE cursorDiskKV.key = ?1 AND length(cursorDiskKV.value) <= ?2 AND json_valid(CAST(cursorDiskKV.value AS TEXT))";
// Cursor 3 keeps its chat list in a table, with the folder each chat works in.
const HEADER: &str = "SELECT json_extract(value, '$.workspaceIdentifier.uri.fsPath') FROM composerHeaders WHERE composerId = ?1";

struct Row {
    id: i64,
    key: String,
    ty: Option<i64>,
    text: Option<String>,
    created: Option<Value>,
    tool: Option<String>,
    tool_status: Option<String>,
    param_path: Option<String>,
    raw: Option<String>,
    block_path: Option<String>,
    model: Option<String>,
    end: Option<f64>,
    sub: bool,
    status: Option<String>,
    valid: bool,
    before: Option<String>,
    after: Option<String>,
    command: Option<String>,
    command_dir: Option<String>,
    output: Option<String>,
}

fn time_of(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64().map(|ms| ms / 1000.0),
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis() as f64 / 1000.0),
        _ => None,
    }
}

fn is_edit(tool: &str) -> bool {
    let t = tool.to_ascii_lowercase();
    ["edit", "write", "patch", "replace", "create", "delete"].iter().any(|k| t.contains(k))
}

/// A file an agent tool call changed. Edits name the content rows Cursor keeps of the file before and after.
#[derive(Debug, Clone, PartialEq)]
enum Change {
    Edit { path: String, before: String, after: String },
    Delete { path: String },
}

#[derive(Debug, Clone, Default)]
struct Bubble {
    ts: f64,
    user: bool,
    words: usize,
    pasted: bool,
    shape: Option<Shape>,
    tool: bool,
    running: bool,
    file: Vec<String>,
    change: Option<Change>,
    counts: Option<(u32, u32)>,
    counted: bool,
    commit: Option<(String, String)>,
    model: Option<String>,
    end: Option<f64>,
    seen: f64,
}

impl Bubble {
    // No end time on the bubble: when we last saw it change, unless that was a catch-up read long after.
    fn finished(&self) -> f64 {
        self.end.unwrap_or(if self.seen - self.ts < 300.0 { self.seen } else { self.ts }).max(self.ts)
    }

    fn same(&self, o: &Bubble) -> bool {
        (self.user, self.words, self.pasted, self.tool, self.running, &self.file, &self.change, &self.commit, &self.model, self.end)
            == (o.user, o.words, o.pasted, o.tool, o.running, &o.file, &o.change, &o.commit, &o.model, o.end)
    }
}

#[derive(Default)]
struct Composer {
    bubbles: HashMap<String, Bubble>,
    meta: bool,
    first_seen: f64,
    sub: bool,
    generating: bool,
    model: Option<String>,
    cwd: Option<PathBuf>,
    touched: f64,
    db: Option<PathBuf>,
    // The last content row of each file this chat's edits left, to count the lines of a later delete.
    latest: HashMap<String, String>,
}

impl Composer {
    fn header(&mut self, sub: bool, status: Option<String>, model: Option<String>) {
        self.meta = true;
        self.sub |= sub;
        self.generating = status.as_deref() == Some("generating");
        if model.is_some() {
            self.model = model;
        }
    }

    fn turns(&self, now: f64) -> Vec<Turn> {
        let mut bs: Vec<&Bubble> = self.bubbles.values().collect();
        bs.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        let mut out: Vec<Turn> = Vec::new();
        let mut files: BTreeSet<String> = BTreeSet::new();
        let mut last: Option<&Bubble> = None;
        let close = |t: &mut Turn, files: &BTreeSet<String>, last: Option<&Bubble>, until: Option<f64>| {
            t.files = files.len();
            let end = last.map_or(t.ts, Bubble::finished);
            t.done = Some(until.map_or(end, |u| end.min(u)));
        };
        for b in bs {
            if b.user {
                if let Some(t) = out.last_mut().filter(|t| t.done.is_none()) {
                    close(t, &files, last, Some(b.ts));
                }
                files.clear();
                last = None;
                out.push(Turn {
                    ts: b.ts,
                    words: b.words,
                    pasted: b.pasted,
                    shape: b.shape.clone(),
                    model: b.model.clone().or_else(|| self.model.clone()),
                    ..Turn::default()
                });
                continue;
            }
            let Some(t) = out.last_mut() else { continue };
            t.reply_words += b.words;
            if b.tool {
                t.started.get_or_insert(b.ts);
                t.tools += 1;
                files.extend(b.file.iter().cloned());
            }
            if let Some(m) = &b.model {
                t.model = Some(m.clone());
            }
            last = Some(b);
        }
        if let Some(t) = out.last_mut().filter(|t| t.done.is_none()) {
            t.files = files.len();
            if let Some(b) = last.filter(|b| !b.tool && !self.generating && now - b.seen >= STABLE) {
                close(t, &files, Some(b), None);
            }
        }
        out
    }

    // Edits and commits in time order, up to the first tool call still running, so none is passed over. Calls from
    // before `turn` (an earlier turn, about to be let go) don't hold the rest back.
    fn work(&self, session: &str, turn: f64) -> Vec<Found> {
        let mut tools: Vec<&Bubble> = self.bubbles.values().filter(|b| b.tool).collect();
        tools.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        let cwd = self.cwd.as_deref();
        let mut out = Vec::new();
        for b in tools {
            if b.running || (b.change.is_some() && !b.counted) {
                if b.ts >= turn {
                    break;
                }
                continue;
            }
            if let (Some(Change::Edit { path, .. } | Change::Delete { path }), Some(counts)) = (&b.change, b.counts) {
                work::edit(b.ts, cwd, session, path, counts, &mut out);
            }
            if let Some((command, output)) = &b.commit {
                work::commits(b.ts, cwd, session, command, output, &mut out);
            }
        }
        out
    }
}

pub struct Cursor {
    roots: Vec<PathBuf>,
    snaps: Snapshots,
    watermark: HashMap<PathBuf, i64>,
    stamp: HashMap<PathBuf, (f64, u64, f64, u64)>,
    composers: HashMap<String, Composer>,
    folders: HashMap<String, PathBuf>,
    // The Cursor that last started, per state.vscdb. VS Code writes its version there at each start for release notes.
    versions: HashMap<PathBuf, String>,
    scanned: f64,
}

fn open(db: &Path) -> Option<Connection> {
    let c = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;
    c.busy_timeout(Duration::from_millis(500)).ok()?;
    Some(c)
}

fn stamp(db: &Path) -> (f64, u64, f64, u64) {
    let of = |p: &Path| fs::metadata(p).map_or((0.0, 0), |m| (mtime(&m), m.len()));
    let (a, b) = of(db);
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    let (c, d) = of(Path::new(&wal));
    (a, b, c, d)
}

fn row_of(r: &rusqlite::Row) -> rusqlite::Result<Row> {
    Ok(Row {
        id: r.get(0)?,
        key: r.get(1)?,
        ty: r.get(2).ok().flatten(),
        text: r.get(3).ok().flatten(),
        created: r
            .get::<_, Option<String>>(4)
            .map(|s| s.map(Value::String))
            .or_else(|_| r.get::<_, Option<f64>>(4).map(|n| n.map(|n| json!(n))))
            .ok()
            .flatten(),
        tool: r.get(5).ok().flatten(),
        tool_status: r.get(6).ok().flatten(),
        param_path: r.get(7).ok().flatten(),
        raw: r.get(8).ok().flatten(),
        block_path: r.get(9).ok().flatten(),
        model: r.get::<_, Option<String>>(10).ok().flatten().filter(|m| !m.is_empty() && !m.starts_with('{')),
        end: r.get::<_, Option<f64>>(11).ok().flatten().map(|ms| ms / 1000.0),
        sub: r.get::<_, Option<bool>>(12).ok().flatten().unwrap_or(false),
        status: r.get(13).ok().flatten(),
        valid: r.get::<_, Option<bool>>(14).ok().flatten().unwrap_or(false),
        before: r.get(15).ok().flatten(),
        after: r.get(16).ok().flatten(),
        command: r.get(17).ok().flatten(),
        command_dir: r.get(18).ok().flatten(),
        output: r.get(19).ok().flatten(),
    })
}

fn rows(c: &Connection, after: i64) -> rusqlite::Result<Vec<Row>> {
    let mut st = c.prepare_cached(ROWS)?;
    let rows = st.query_map(params![after, BATCH, MAX_ROW], row_of)?;
    rows.collect()
}

fn row(c: &Connection, key: &str) -> Option<Row> {
    c.prepare_cached(ROW).ok()?.query_row(params![key, MAX_ROW], row_of).ok()
}

fn content(c: &Connection, key: &str) -> Option<String> {
    if key == EMPTY_CONTENT {
        return Some(String::new());
    }
    c.prepare_cached(CONTENT).ok()?.query_row(params![key, MAX_ROW], |r| r.get(0)).ok()
}

// What the file held before this chat first touched it, which Cursor keeps so it can undo the chat.
fn original(c: &Connection, composer: &str, path: &str) -> Option<String> {
    let mut st = c.prepare_cached(ORIGINALS).ok()?;
    let rows = st.query_map(params![format!("composerData:{composer}"), MAX_ROW], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    });
    let key = rows.ok()?.flatten().find(|(uri, _)| uri_path(uri).is_some_and(|p| p == Path::new(path)))?.1?;
    content(c, &key)
}

fn header_folder(c: &Connection, composer: &str) -> Option<PathBuf> {
    let path: Option<String> = c.prepare_cached(HEADER).ok()?.query_row(params![composer], |r| r.get(0)).ok()?;
    path.filter(|p| !p.is_empty()).map(PathBuf::from)
}

fn workspace_folders(root: &Path, into: &mut HashMap<String, PathBuf>) {
    for e in fs::read_dir(root.join("User/workspaceStorage")).into_iter().flatten().flatten() {
        let dir = e.path();
        let Some(folder) = fs::read_to_string(dir.join("workspace.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.get("folder").and_then(Value::as_str).and_then(uri_path))
        else {
            continue;
        };
        let Some(c) = open(&dir.join("state.vscdb")) else { continue };
        let Ok(text) = c.query_row("SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = 'composer.composerData'", [], |r| {
            r.get::<_, String>(0)
        }) else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        for comp in v.get("allComposers").and_then(Value::as_array).into_iter().flatten() {
            if let Some(id) = comp.get("composerId").and_then(Value::as_str) {
                into.insert(id.to_string(), folder.clone());
            }
        }
    }
}

impl Cursor {
    pub fn new(roots: Vec<PathBuf>) -> Cursor {
        Cursor {
            roots,
            snaps: Snapshots::default(),
            watermark: HashMap::new(),
            stamp: HashMap::new(),
            composers: HashMap::new(),
            folders: HashMap::new(),
            versions: HashMap::new(),
            scanned: f64::MIN,
        }
    }

    fn apply(&mut self, r: Row, db: &Path, at: f64) {
        let mut parts = r.key.splitn(3, ':');
        let (family, cid, bid) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        if cid.is_empty() || !r.valid {
            return;
        }
        let comp = self.composers.entry(cid.to_string()).or_insert_with(|| Composer { first_seen: at, ..Composer::default() });
        comp.db.get_or_insert_with(|| db.to_path_buf());
        if family == "composerData" {
            comp.touched = at;
            comp.header(r.sub, r.status, r.model);
            return;
        }
        let ts = r.created.as_ref().and_then(time_of).unwrap_or(at);
        let mut b = Bubble { ts, seen: at, model: r.model, end: r.end, ..Bubble::default() };
        let text = r.text.unwrap_or_default();
        match r.ty {
            Some(1) => {
                let (words, pasted) = prompt_stats(&text);
                if words == 0 {
                    return;
                }
                b.user = true;
                b.words = words;
                b.pasted = pasted;
                b.shape = Some(prompt_shape(&text));
            }
            Some(2) => {
                b.words = super::words(&text);
                if let Some(tool) = r.tool {
                    b.tool = true;
                    let status = r.tool_status.as_deref();
                    b.running = !matches!(status, Some("completed" | "error" | "cancelled" | "rejected" | "aborted"));
                    if status == Some("completed") {
                        if is_edit(&tool) {
                            if let Some(p) = r.param_path.clone().or(r.block_path) {
                                b.file.push(p);
                            } else if let Some(raw) = r.raw {
                                for line in raw.lines() {
                                    if let Some(p) = PATCH_MARKERS.iter().find_map(|m| line.trim().strip_prefix(m)) {
                                        b.file.push(p.trim().to_string());
                                    }
                                }
                                if b.file.is_empty() && !raw.contains('\n') && !raw.is_empty() {
                                    b.file.push(raw);
                                }
                            }
                        }
                        b.change = match (r.param_path, r.before, r.after) {
                            (Some(path), Some(before), Some(after)) => Some(Change::Edit { path, before, after }),
                            (Some(path), _, _) if tool.to_ascii_lowercase().contains("delete") => Some(Change::Delete { path }),
                            _ => None,
                        };
                        if let Some(command) = r.command.filter(|c| c.contains("commit")) {
                            let command = match r.command_dir.filter(|d| !d.is_empty()) {
                                Some(d) => format!("cd {} && {command}", work::shell_join([d.as_str()])),
                                None => command,
                            };
                            b.commit = Some((command, r.output.unwrap_or_default()));
                        }
                    }
                }
            }
            _ => return,
        }
        // While a reply streams the same bubble is written again; keep its first creation time, and when it last changed.
        match comp.bubbles.get(bid) {
            Some(old) if b.same(old) => return,
            Some(old) => b.ts = b.ts.min(old.ts),
            None => {}
        }
        comp.touched = at;
        if comp.cwd.is_none() && !b.file.is_empty() {
            comp.cwd = b.file.iter().map(PathBuf::from).find(|p| p.is_absolute()).and_then(|p| p.parent().map(Path::to_path_buf));
        }
        comp.bubbles.insert(bid.to_string(), b);
    }

    fn read(&mut self, db: &Path, at: f64) {
        let st = stamp(db);
        if self.stamp.get(db) == Some(&st) {
            return;
        }
        let Some(c) = open(db) else { return };
        if let Ok(v) = c.query_row("SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = 'releaseNotes/lastVersion'", [], |r| r.get::<_, String>(0)) {
            self.versions.insert(db.to_path_buf(), v);
        }
        let Some(&after) = self.watermark.get(db) else {
            if let Ok(max) = c.query_row("SELECT COALESCE(MAX(rowid), 0) FROM cursorDiskKV", [], |r| r.get::<_, i64>(0)) {
                self.watermark.insert(db.to_path_buf(), max);
                self.stamp.insert(db.to_path_buf(), st);
            }
            return;
        };
        let mut after = after;
        for _ in 0..BATCHES {
            let Ok(batch) = rows(&c, after) else { return };
            let n = batch.len();
            for r in batch {
                after = after.max(r.id);
                self.apply(r, db, at);
            }
            for (cid, comp) in self.composers.iter_mut().filter(|(_, c)| !c.meta) {
                if let Ok((sub, status, model)) = c.query_row(META, params![format!("composerData:{cid}"), MAX_ROW], |r| {
                    Ok((r.get::<_, Option<bool>>(0)?.unwrap_or(false), r.get(1)?, r.get(2)?))
                }) {
                    comp.header(sub, status, model);
                }
            }
            if (n as i64) < BATCH {
                self.stamp.insert(db.to_path_buf(), st);
                break;
            }
        }
        self.watermark.insert(db.to_path_buf(), after);
        self.reread(&c, db, at);
        self.count(&c, db);
        let unmapped: Vec<String> = self
            .composers
            .iter()
            .filter(|(cid, comp)| !comp.sub && comp.db.as_deref() == Some(db) && !self.folders.contains_key(*cid))
            .map(|(cid, _)| cid.clone())
            .collect();
        for cid in unmapped {
            if let Some(f) = header_folder(&c, &cid) {
                self.folders.insert(cid, f);
            }
        }
    }

    fn reread(&mut self, c: &Connection, db: &Path, at: f64) {
        let mut keys = Vec::new();
        for (cid, comp) in &self.composers {
            if comp.sub || comp.db.as_deref() != Some(db) || at - comp.touched >= REREAD {
                continue;
            }
            let moving = comp.bubbles.iter().filter(|(_, b)| (b.running && at - b.ts < REREAD) || at - b.seen < SETTLED);
            keys.extend(moving.map(|(bid, _)| format!("bubbleId:{cid}:{bid}")));
        }
        for key in keys {
            if let Some(r) = row(c, &key) {
                self.apply(r, db, at);
            }
        }
        let active = self.composers.iter_mut().filter(|(_, comp)| !comp.sub && comp.db.as_deref() == Some(db) && at - comp.touched < SETTLED);
        for (cid, comp) in active {
            if let Ok((sub, status, model)) = c.query_row(META, params![format!("composerData:{cid}"), MAX_ROW], |r| {
                Ok((r.get::<_, Option<bool>>(0)?.unwrap_or(false), r.get(1)?, r.get(2)?))
            }) {
                comp.header(sub, status, model);
            }
        }
    }

    fn count(&mut self, c: &Connection, db: &Path) {
        for (cid, comp) in &mut self.composers {
            if comp.db.as_deref() != Some(db) {
                continue;
            }
            let Composer { bubbles, latest, .. } = comp;
            let mut todo: Vec<&mut Bubble> = bubbles.values_mut().filter(|b| b.change.is_some() && !b.counted && !b.running).collect();
            todo.sort_by(|a, b| a.ts.total_cmp(&b.ts));
            for b in todo {
                b.counted = true;
                b.counts = match &b.change {
                    Some(Change::Edit { path, before, after }) => {
                        latest.insert(path.clone(), after.clone());
                        content(c, before).zip(content(c, after)).and_then(|(old, new)| work::diff_counts(&old, &new))
                    }
                    Some(Change::Delete { path }) => {
                        let text = match latest.remove(path) {
                            Some(key) => content(c, &key),
                            None => original(c, cid, path),
                        };
                        text.map(|t| (0, work::line_count(&t)))
                    }
                    None => None,
                };
            }
        }
    }
}

impl Source for Cursor {
    fn name(&self) -> &'static str {
        "cursor"
    }

    fn poll(&mut self) -> Vec<Found> {
        let at = now();
        for root in self.roots.clone() {
            let db = root.join("User/globalStorage/state.vscdb");
            if db.is_file() {
                self.read(&db, at);
            }
        }
        // Windows save their state lazily, so a new composer may take a few scans to show up there.
        let unmapped = self
            .composers
            .iter()
            .any(|(id, c)| !c.sub && c.cwd.is_none() && !self.folders.contains_key(id) && c.first_seen > self.scanned - 300.0);
        if unmapped && at - self.scanned >= RESCAN {
            self.scanned = at;
            for root in &self.roots {
                workspace_folders(root, &mut self.folders);
            }
        }
        let mut out = Vec::new();
        for (cid, comp) in &mut self.composers {
            if cid.starts_with("task-") {
                comp.sub = true;
            }
            if comp.sub || (!comp.meta && at - comp.first_seen < HOLD) {
                continue;
            }
            if let Some(f) = self.folders.get(cid) {
                comp.cwd = Some(f.clone());
            }
            let mut turns = comp.turns(at);
            let version = comp.db.as_ref().and_then(|d| self.versions.get(d));
            turns.iter_mut().for_each(|t| t.version = version.cloned());
            self.snaps.emit(cid, comp.cwd.as_deref(), &turns, &mut out);
            let last_user = comp.bubbles.values().filter(|b| b.user).map(|b| b.ts).reduce(f64::max);
            self.snaps.emit_work(cid, comp.work(cid, last_user.unwrap_or(f64::MIN)), &mut out);
            if let Some(last_user) = last_user {
                comp.bubbles.retain(|_, b| b.ts >= last_user);
            }
        }
        self.composers.retain(|_, c| at - c.touched < FORGET);
        self.snaps.finish(at);
        out.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        out
    }

    fn state(&self) -> Value {
        let mut state = self.snaps.state();
        let marks: serde_json::Map<String, Value> =
            self.watermark.iter().map(|(p, w)| (p.to_string_lossy().into_owned(), json!(w))).collect();
        state["rowids"] = Value::Object(marks);
        state
    }

    fn restore(&mut self, state: &Value) {
        self.snaps.restore(state);
        for (p, w) in state.get("rowids").and_then(Value::as_object).into_iter().flatten() {
            if let Some(w) = w.as_i64() {
                self.watermark.insert(PathBuf::from(p), w);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Kind;

    fn names(out: &[Found]) -> Vec<&'static str> {
        out.iter().map(|f| f.kind.name()).collect()
    }

    struct Fixture {
        root: PathBuf,
        c: Connection,
    }

    impl Fixture {
        fn new(name: &str) -> Fixture {
            let root = std::env::temp_dir().join(format!("kairos-cursor-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("User/globalStorage")).unwrap();
            let ws = root.join("User/workspaceStorage/w1");
            fs::create_dir_all(&ws).unwrap();
            fs::write(ws.join("workspace.json"), r#"{"folder":"file:///w/p"}"#).unwrap();
            let w = Connection::open(ws.join("state.vscdb")).unwrap();
            w.execute_batch(
                "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
                 INSERT INTO ItemTable VALUES ('composer.composerData', '{\"allComposers\":[{\"composerId\":\"c1\"}]}');",
            )
            .unwrap();
            let c = Connection::open(root.join("User/globalStorage/state.vscdb")).unwrap();
            c.execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
                 INSERT INTO ItemTable VALUES ('releaseNotes/lastVersion', '3.31.15');
                 CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
                 CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER,
                     lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER, recency INTEGER, checkpointAt INTEGER,
                     subagentTypeName TEXT, value TEXT);",
            )
            .unwrap();
            Fixture { root, c }
        }

        fn put(&self, key: &str, v: Value) {
            self.c.execute("INSERT INTO cursorDiskKV VALUES (?1, ?2)", params![key, v.to_string().into_bytes()]).unwrap();
        }

        // Cursor 3 writes a bubble again under the same rowid.
        fn update(&self, key: &str, v: Value) {
            assert_eq!(self.c.execute("UPDATE cursorDiskKV SET value = ?2 WHERE key = ?1", params![key, v.to_string().into_bytes()]).unwrap(), 1);
        }

        fn text(&self, key: &str, text: &str) {
            self.c.execute("INSERT INTO cursorDiskKV VALUES (?1, ?2)", params![key, text]).unwrap();
        }

        fn user(&self, cid: &str, bid: &str, at: f64, text: &str) {
            self.put(&format!("bubbleId:{cid}:{bid}"), json!({ "_v": 3, "type": 1, "text": text, "createdAt": at * 1000.0 }));
        }

        fn tool(&self, cid: &str, bid: &str, at: f64, name: &str, status: &str, params: Value, result: Value) -> (String, Value) {
            let v = json!({ "_v": 3, "type": 2, "text": "", "createdAt": at * 1000.0,
                "toolFormerData": { "name": name, "status": status, "params": params.to_string(), "result": result.to_string() } });
            (format!("bubbleId:{cid}:{bid}"), v)
        }
    }

    fn edits(out: &[Found]) -> Vec<(String, u32, u32)> {
        out.iter()
            .filter_map(|f| match &f.kind {
                Kind::AgentEdit { path, added, removed, .. } => Some((path.clone(), *added, *removed)),
                _ => None,
            })
            .collect()
    }

    fn edit_ids(before: &str, after: &str) -> Value {
        json!({ "beforeContentId": before, "afterContentId": after })
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn settle(src: &mut Cursor) {
        for c in src.composers.values_mut() {
            c.bubbles.values_mut().for_each(|b| b.seen -= STABLE + 1.0);
        }
    }

    #[test]
    fn agent_turn_with_subagents_bad_rows_mid_turn_prompt_and_resume() {
        let f = Fixture::new("flow");
        f.put("composerData:old", json!({ "composerId": "old", "status": "completed" }));
        f.user("old", "b0", 1_000.0, "an old prompt from before install");
        let mut src = Cursor::new(vec![f.root.clone()]);
        assert!(src.poll().is_empty());

        let t0 = now().ceil() + 1.0;
        f.put("composerData:c1", json!({ "composerId": "c1", "status": "generating", "modelConfig": { "modelName": "composer-2.5" } }));
        f.put("bubbleId:c1:u1", json!({ "_v": 3, "type": 1, "text": "please fix the failing parser test now", "createdAt": "2099-01-01T00:00:00Z" }));
        f.user("c1", "u1", t0, "please fix the failing parser test now");
        f.put("composerData:task-9", json!({ "composerId": "task-9", "isSubagent": true, "subagentInfo": { "toolCallId": "t1" } }));
        f.user("task-9", "s1", t0 + 1.0, "look into the parser module for me");
        f.c.execute("INSERT INTO cursorDiskKV VALUES ('bubbleId:c1:bad', X'7b6e6f74206a736f6e')", []).unwrap();
        let out = src.poll();
        assert_eq!(names(&out), ["prompt"]);
        assert_eq!(out[0].ts, t0);
        assert_eq!(out[0].cwd.as_deref(), Some(Path::new("/w/p")));
        assert!(matches!(&out[0].kind, Kind::Prompt { words: 7, model: Some(m), session, version: Some(v), .. } if m == "composer-2.5" && session == "c1" && v == "3.31.15"));

        let params = json!({ "relativeWorkspacePath": "/w/p/a.rs", "streamingContent": "fn main() {}" }).to_string();
        f.put(
            "bubbleId:c1:a1",
            json!({ "_v": 3, "type": 2, "text": "Looking at it.", "createdAt": (t0 + 5.0) * 1000.0,
                    "toolFormerData": { "name": "edit_file_v2", "status": "completed", "params": params } }),
        );
        f.put("bubbleId:c1:a2", json!({ "_v": 3, "type": 2, "text": "", "createdAt": (t0 + 6.0) * 1000.0,
                    "toolFormerData": { "name": "read_file_v2", "status": "completed", "params": "{}" } }));
        let out = src.poll();
        assert_eq!(names(&out), ["agent_start"]);
        assert_eq!(out[0].ts, t0 + 5.0);

        f.put("bubbleId:c1:a3", json!({ "_v": 3, "type": 2, "text": "Fixed it", "createdAt": (t0 + 7.0) * 1000.0 }));
        f.put("composerData:c1", json!({ "composerId": "c1", "status": "completed" }));
        assert!(src.poll().is_empty());
        f.put("bubbleId:c1:a3", json!({ "_v": 3, "type": 2, "text": "Fixed it in a.rs.", "createdAt": (t0 + 9.0) * 1000.0,
                    "timingInfo": { "clientEndTime": (t0 + 60.0) * 1000.0 } }));
        assert!(src.poll().is_empty());
        settle(&mut src);
        let out = src.poll();
        assert_eq!(names(&out), ["reply_done"]);
        assert_eq!(out[0].ts, t0 + 60.0);
        match &out[0].kind {
            Kind::ReplyDone { reply_words, tools, files_changed, model, .. } => {
                assert_eq!((*reply_words, *tools, *files_changed), (7, 2, 1));
                assert_eq!(model.as_deref(), Some("composer-2.5"));
            }
            k => panic!("{k:?}"),
        }

        f.user("c1", "u2", t0 + 100.0, "now run the whole test suite");
        f.put("bubbleId:c1:a4", json!({ "_v": 3, "type": 2, "text": "", "createdAt": (t0 + 101.0) * 1000.0,
                    "toolFormerData": { "name": "run_terminal_cmd", "status": "completed" } }));
        f.user("c1", "u3", t0 + 130.0, "stop, only the parser tests");
        let out = src.poll();
        assert_eq!(names(&out), ["prompt", "agent_start", "reply_done", "prompt"]);
        assert_eq!(out[2].ts, t0 + 101.0);

        let mut again = Cursor::new(vec![f.root.clone()]);
        again.restore(&src.state());
        assert!(again.poll().is_empty());
        f.user("c1", "u4", t0 + 200.0, "thanks, now commit it");
        assert_eq!(names(&again.poll()), ["prompt"]);
    }

    #[test]
    fn cursor_3_rewrites_rows_in_place_and_keeps_a_copy_of_the_file_on_each_side_of_an_edit() {
        let f = Fixture::new("inplace");
        let mut src = Cursor::new(vec![f.root.clone()]);
        assert!(src.poll().is_empty());

        let t0 = now().ceil() + 1.0;
        let head = json!({ "workspaceIdentifier": { "id": "w9", "uri": { "fsPath": "/w/dots", "scheme": "file" } } });
        f.c.execute("INSERT INTO composerHeaders (composerId, workspaceId, value) VALUES ('c2', 'w9', ?1)", params![head.to_string()])
            .unwrap();
        f.put("composerData:c2", json!({ "composerId": "c2", "status": "generating" }));
        f.user("c2", "u1", t0, "add a greeting line to the readme");
        let (key, loading) = f.tool("c2", "e1", t0 + 2.0, "edit_file_v2", "loading", json!({}), json!({}));
        f.put(&key, loading);
        f.put("bubbleId:c2:r1", json!({ "_v": 3, "type": 2, "text": "", "createdAt": (t0 + 3.0) * 1000.0 }));
        let out = src.poll();
        assert_eq!(names(&out), ["prompt", "agent_start"]);
        assert_eq!(out[0].cwd.as_deref(), Some(Path::new("/w/dots")));

        f.text("composer.content.b1", "# dots\nmy setup\n");
        f.text("composer.content.a1", "hi\n\n# dots\nmy setup\n");
        let params = json!({ "relativeWorkspacePath": "/w/dots/README.md" });
        let (key, done) = f.tool("c2", "e1", t0 + 2.0, "edit_file_v2", "completed", params, edit_ids("composer.content.b1", "composer.content.a1"));
        f.update(&key, done);
        f.update("bubbleId:c2:r1", json!({ "_v": 3, "type": 2, "text": "Added a greeting at the top.", "createdAt": (t0 + 3.0) * 1000.0 }));
        f.update("composerData:c2", json!({ "composerId": "c2", "status": "completed" }));
        let out = src.poll();
        assert_eq!(edits(&out), [("/w/dots/README.md".to_string(), 2, 0)]);
        assert_eq!((out[0].ts, out[0].cwd.as_deref()), (t0 + 2.0, Some(Path::new("/w/dots"))));
        assert!(matches!(&out[0].kind, Kind::AgentEdit { session, .. } if session == "c2"));

        settle(&mut src);
        let out = src.poll();
        assert_eq!(names(&out), ["reply_done"]);
        assert!(matches!(&out[0].kind, Kind::ReplyDone { reply_words: 6, tools: 1, files_changed: 1, .. }), "{:?}", out[0].kind);
        assert!(src.poll().is_empty());
    }

    #[test]
    fn a_turn_that_looked_done_before_its_first_tool_call_runs_again() {
        let f = Fixture::new("thinks");
        let mut src = Cursor::new(vec![f.root.clone()]);
        assert!(src.poll().is_empty());

        // Cursor 3.13 (berke-volkan, 11-26): an empty reply sits still while the model thinks, the chat not marked generating.
        let t0 = now().ceil() + 1.0;
        f.put("composerData:c5", json!({ "composerId": "c5", "status": "completed" }));
        f.user("c5", "u1", t0, "why does the build fail on windows");
        f.put("bubbleId:c5:r1", json!({ "_v": 3, "type": 2, "text": "", "createdAt": (t0 + 1.0) * 1000.0 }));
        assert_eq!(names(&src.poll()), ["prompt"]);
        settle(&mut src);
        let out = src.poll();
        assert_eq!(names(&out), ["reply_done"]);
        assert!(matches!(&out[0].kind, Kind::ReplyDone { reply_words: 0, tools: 0, .. }), "{:?}", out[0].kind);

        let (key, v) = f.tool("c5", "t1", t0 + 20.0, "read_file_v2", "completed", json!({}), json!({}));
        f.put(&key, v);
        let out = src.poll();
        assert_eq!(names(&out), ["agent_start"]);
        assert_eq!(out[0].ts, t0 + 20.0);

        f.put("bubbleId:c5:r2", json!({ "_v": 3, "type": 2, "text": "The path uses a forward slash.", "createdAt": (t0 + 25.0) * 1000.0 }));
        assert!(src.poll().is_empty());
        settle(&mut src);
        let out = src.poll();
        assert_eq!(names(&out), ["reply_done"], "the run the agent started ends");
        assert!(matches!(&out[0].kind, Kind::ReplyDone { reply_words: 6, tools: 1, .. }), "{:?}", out[0].kind);
        assert!(src.poll().is_empty());
    }

    #[test]
    fn deletes_count_the_last_copy_the_chat_left_or_the_file_before_the_chat() {
        let f = Fixture::new("delete");
        let mut src = Cursor::new(vec![f.root.clone()]);
        assert!(src.poll().is_empty());

        let t0 = now().ceil() + 1.0;
        f.text("composer.content.n1", "one\ntwo\nthree\n");
        f.text("ofsContent:c3:file:///w/p/old.txt", "a\nb\n");
        let states = json!({ "file:///w/p/old.txt": { "isNewlyCreated": false, "contentKey": "ofsContent:c3:file:///w/p/old.txt" } });
        f.put("composerData:c3", json!({ "composerId": "c3", "status": "completed", "originalFileStates": states }));
        f.user("c3", "u1", t0, "write the notes, then clear out the old files");
        let steps = [
            ("e1", "edit_file_v2", json!({ "relativeWorkspacePath": "/w/p/notes.txt" }), edit_ids(EMPTY_CONTENT, "composer.content.n1")),
            ("d1", "delete_file", json!({ "relativeWorkspacePath": "/w/p/notes.txt" }), json!({})),
            ("d2", "delete_file", json!({ "relativeWorkspacePath": "/w/p/old.txt" }), json!({})),
            ("d3", "delete_file", json!({ "relativeWorkspacePath": "/w/p/never-seen.txt" }), json!({})),
        ];
        for (i, (bid, name, params, result)) in steps.into_iter().enumerate() {
            let (key, v) = f.tool("c3", bid, t0 + 1.0 + i as f64, name, "completed", params, result);
            f.put(&key, v);
        }
        let out = src.poll();
        let want = [("/w/p/notes.txt", 3, 0), ("/w/p/notes.txt", 0, 3), ("/w/p/old.txt", 0, 2)];
        assert_eq!(edits(&out), want.map(|(p, a, r)| (p.to_string(), a, r)));
    }

    #[test]
    fn edits_go_out_in_order_and_a_call_that_never_ends_holds_up_only_its_own_turn() {
        let f = Fixture::new("order");
        let mut src = Cursor::new(vec![f.root.clone()]);
        assert!(src.poll().is_empty());

        let t0 = now().ceil() + 1.0;
        f.text("composer.content.x", "x\n");
        let at = |p: &str| json!({ "relativeWorkspacePath": p });
        f.put("composerData:c4", json!({ "composerId": "c4", "status": "generating" }));
        f.user("c4", "u1", t0, "change both files and commit");
        let (k1, v) = f.tool("c4", "e1", t0 + 1.0, "edit_file_v2", "loading", json!({}), json!({}));
        f.put(&k1, v);
        let (k2, v) = f.tool("c4", "e2", t0 + 2.0, "edit_file_v2", "completed", at("/w/p/b.rs"), edit_ids(EMPTY_CONTENT, "composer.content.x"));
        f.put(&k2, v);
        assert!(edits(&src.poll()).is_empty(), "the later edit waits for the earlier one");

        let (_, v) = f.tool("c4", "e1", t0 + 1.0, "edit_file_v2", "completed", at("/w/p/a.rs"), edit_ids(EMPTY_CONTENT, "composer.content.x"));
        f.update(&k1, v);
        let commit = json!({ "output": "[main 1a2b3c4] change both\n 2 files changed" });
        let (k3, v) = f.tool("c4", "t1", t0 + 3.0, "run_terminal_command_v2", "completed", json!({ "command": "git commit -am 'change both'", "cwd": "" }), commit);
        f.put(&k3, v);
        let out = src.poll();
        assert_eq!(edits(&out), [("/w/p/a.rs".to_string(), 1, 0), ("/w/p/b.rs".to_string(), 1, 0)]);
        assert!(out.iter().any(|f| matches!(&f.kind, Kind::AgentCommit { sha: Some(s), .. } if s == "1a2b3c4")));

        let (k4, v) = f.tool("c4", "e3", t0 + 10.0, "edit_file_v2", "loading", json!({}), json!({}));
        f.put(&k4, v);
        let (k5, v) = f.tool("c4", "e4", t0 + 11.0, "edit_file_v2", "completed", at("/w/p/c.rs"), edit_ids(EMPTY_CONTENT, "composer.content.x"));
        f.put(&k5, v);
        assert!(edits(&src.poll()).is_empty());
        f.user("c4", "u2", t0 + 20.0, "never mind, stop there");
        assert_eq!(edits(&src.poll()), [("/w/p/c.rs".to_string(), 1, 0)]);
    }
}
