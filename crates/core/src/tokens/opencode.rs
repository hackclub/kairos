use std::path::PathBuf;

use rusqlite::Connection;

use super::{keep, Counts, Reader, Use};
use crate::sources::opencode::{open, Schema};

// What to look back past the newest row seen, for one written a moment after it. Reading a row twice is harmless.
const OVERLAP_MS: i64 = 5000;

pub struct OpenCode {
    db: PathBuf,
    cursor: Option<i64>,
}

impl OpenCode {
    pub fn new(db: PathBuf) -> OpenCode {
        OpenCode { db, cursor: None }
    }
}

const V2: &str = "SELECT m.id, json_extract(m.data, '$.time.completed'), json_extract(m.data, '$.model.id'),
        json_extract(m.data, '$.tokens.input'), json_extract(m.data, '$.tokens.output'), json_extract(m.data, '$.tokens.reasoning'),
        json_extract(m.data, '$.tokens.cache.read'), json_extract(m.data, '$.tokens.cache.write'), s.directory, m.time_updated
    FROM session_message m LEFT JOIN session_v2 s ON s.id = m.session_id
    WHERE m.time_updated >= ?1 AND m.type = 'assistant' AND json_extract(m.data, '$.time.completed') >= ?1";

const LEGACY: &str = "SELECT m.id, json_extract(m.data, '$.time.completed'), json_extract(m.data, '$.modelID'),
        json_extract(m.data, '$.tokens.input'), json_extract(m.data, '$.tokens.output'), json_extract(m.data, '$.tokens.reasoning'),
        json_extract(m.data, '$.tokens.cache.read'), json_extract(m.data, '$.tokens.cache.write'),
        COALESCE(json_extract(m.data, '$.path.cwd'), s.directory), m.time_updated
    FROM message m LEFT JOIN session s ON s.id = m.session_id
    WHERE m.time_updated >= ?1 AND json_extract(m.data, '$.role') = 'assistant' AND json_extract(m.data, '$.time.completed') >= ?1";

fn count(r: &rusqlite::Row, i: usize) -> rusqlite::Result<u64> {
    Ok(r.get::<_, Option<f64>>(i)?.map_or(0, |n| n.max(0.0) as u64))
}

fn query(c: &Connection, schema: Schema, since: f64, from_ms: i64) -> rusqlite::Result<(Vec<Use>, i64)> {
    let mut st = c.prepare(match schema {
        Schema::Legacy => LEGACY,
        Schema::V2 { .. } => V2,
    })?;
    let mut newest = 0;
    let mut out = Vec::new();
    let mut rows = st.query([from_ms])?;
    while let Some(r) = rows.next()? {
        let (id, done, model): (String, Option<f64>, Option<String>) = (r.get(0)?, r.get(1)?, r.get(2)?);
        let (cwd, updated): (Option<String>, i64) = (r.get(8)?, r.get(9)?);
        newest = newest.max(updated);
        // Reasoning is billed as output. 2.x copies 1.x messages under the same ids, and a row is rewritten while it runs.
        let n = Counts { input: count(r, 3)?, output: count(r, 4)? + count(r, 5)?, cache_read: count(r, 6)?, cache_write: count(r, 7)? };
        let (Some(done), Some(model)) = (done, model) else { continue };
        out.extend(keep(Use { ts: done / 1000.0, harness: "opencode", model, cwd: cwd.map(PathBuf::from), key: format!("opencode:{id}"), n }, since));
    }
    Ok((out, newest))
}

impl Reader for OpenCode {
    fn read(&mut self, since: f64) -> Vec<Use> {
        let Some(c) = open(&self.db) else { return Vec::new() };
        let Some(schema) = Schema::detect(&c) else { return Vec::new() };
        let from = self.cursor.unwrap_or((since * 1000.0) as i64);
        match query(&c, schema, since, from) {
            Ok((out, newest)) => {
                self.cursor = Some(from.max(newest - OVERLAP_MS));
                out
            }
            Err(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rusqlite::params;
    use serde_json::json;

    use super::super::testing::dir;
    use super::*;

    const SINCE: f64 = 1_700_000_000.0;
    const T0: i64 = 1_795_512_000_000;

    fn v2(root: &Path) -> Connection {
        let c = Connection::open(root.join("opencode.db")).unwrap();
        c.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER,
                time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .unwrap();
        c.execute("INSERT INTO session_v2 (id, directory) VALUES ('s1', '/w/p')", []).unwrap();
        c.execute("INSERT INTO session (id, directory) VALUES ('s1', '/w/legacy')", []).unwrap();
        c
    }

    fn assistant(model: &str, tokens: Option<(u64, u64, u64, u64, u64)>, done: Option<i64>) -> String {
        let mut d = json!({ "type": "assistant", "model": { "id": model, "providerID": "opencode" }, "time": { "created": T0 } });
        if let Some((input, output, reasoning, read, write)) = tokens {
            d["tokens"] = json!({ "input": input, "output": output, "reasoning": reasoning, "cache": { "read": read, "write": write } });
        }
        if let Some(done) = done {
            d["time"]["completed"] = json!(done);
        }
        d.to_string()
    }

    fn put(c: &Connection, id: &str, at: i64, data: &str) {
        c.execute("INSERT OR REPLACE INTO session_message VALUES (?1, 's1', 'assistant', ?2, ?3, ?3, ?4)", params![id, at - T0, at, data]).unwrap();
    }

    #[test]
    fn a_2x_database_is_read_alone_and_a_row_rewritten_later_comes_again_with_its_new_counts() {
        let root = dir("opencode-v2");
        let c = v2(&root);
        put(&c, "m1", T0 + 1000, &assistant("deepseek-v4", Some((36, 2, 15, 18536, 4)), Some(T0 + 1000)));
        put(&c, "m2", T0 + 2000, &assistant("deepseek-v4", Some((9, 9, 9, 9, 9)), None));
        put(&c, "m3", T0 + 3000, &assistant("deepseek-v4", None, Some(T0 + 3000)));
        let migrated = json!({ "role": "assistant", "modelID": "old", "tokens": { "input": 1000, "output": 1, "reasoning": 0, "cache": { "read": 0, "write": 0 } }, "time": { "completed": T0 } });
        c.execute("INSERT INTO message VALUES ('m1', 's1', ?1, ?1, ?2)", params![T0, migrated.to_string()]).unwrap();
        let mut r = OpenCode::new(root.join("opencode.db"));
        let got = r.read(SINCE);
        assert_eq!(got.len(), 1, "unfinished and count-less messages and the stale 1.x copy are left out");
        let u = &got[0];
        assert_eq!((u.harness, u.model.as_str(), u.key.as_str()), ("opencode", "deepseek-v4", "opencode:m1"));
        assert_eq!(u.n, Counts { input: 36, output: 17, cache_read: 18536, cache_write: 4 });
        assert_eq!((u.ts, u.cwd.as_deref()), (1_795_512_001.0, Some(Path::new("/w/p"))));

        put(&c, "m2", T0 + 5000, &assistant("deepseek-v4", Some((9, 9, 9, 9, 9)), Some(T0 + 4900)));
        put(&c, "m1", T0 + 6000, &assistant("deepseek-v4", Some((40, 2, 15, 18536, 4)), Some(T0 + 1000)));
        let mut again = r.read(SINCE);
        again.sort_by(|a, b| a.key.cmp(&b.key));
        assert_eq!(again.iter().map(|u| (u.key.as_str(), u.n.input, u.n.output)).collect::<Vec<_>>(), [("opencode:m1", 40, 17), ("opencode:m2", 9, 18)]);
        assert!(r.read(SINCE).iter().all(|u| u.key == "opencode:m1" || u.key == "opencode:m2"), "only the rewritten rows come back");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_1x_database_reads_the_message_table() {
        let root = dir("opencode-legacy");
        let c = Connection::open(root.join("opencode.db")).unwrap();
        c.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             INSERT INTO session (id, directory) VALUES ('s1', '/w/session');",
        )
        .unwrap();
        let msg = |role: &str, cwd: Option<&str>| {
            let mut d = json!({ "role": role, "modelID": "m-1", "providerID": "p", "tokens": { "total": 9, "input": 24, "output": 233, "reasoning": 1224, "cache": { "write": 0, "read": 37248 } }, "time": { "created": T0, "completed": T0 + 500 } });
            if let Some(cwd) = cwd {
                d["path"] = json!({ "cwd": cwd, "root": "/" });
            }
            d.to_string()
        };
        for (id, role, cwd) in [("a", "assistant", Some("/w/p")), ("b", "assistant", None), ("c", "user", None)] {
            c.execute("INSERT INTO message VALUES (?1, 's1', ?2, ?2, ?3)", params![id, T0, msg(role, cwd)]).unwrap();
        }
        let mut got = OpenCode::new(root.join("opencode.db")).read(SINCE);
        got.sort_by(|a, b| a.key.cmp(&b.key));
        assert_eq!(got.iter().map(|u| (u.key.as_str(), u.cwd.clone().unwrap())).collect::<Vec<_>>(), [("opencode:a", "/w/p".into()), ("opencode:b", "/w/session".into())]);
        assert_eq!(got[0].n, Counts { input: 24, output: 1457, cache_read: 37248, cache_write: 0 });
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rows_before_since_and_a_missing_database_give_nothing() {
        let root = dir("opencode-since");
        assert!(OpenCode::new(root.join("none.db")).read(SINCE).is_empty());
        let c = v2(&root);
        put(&c, "m1", T0 + 1000, &assistant("m", Some((1, 1, 0, 0, 0)), Some(T0 + 1000)));
        assert!(OpenCode::new(root.join("opencode.db")).read((T0 + 2000) as f64 / 1000.0).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
