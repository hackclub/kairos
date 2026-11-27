use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::snapshot::{now, Snapshots, Turn};
use super::{work, words, Found, Source};
use crate::{prompt_shape, prompt_stats};

const MAX_FILE: u64 = 64 * 1024 * 1024;
const EDIT_TOOLS: [&str; 5] = ["write_file", "replace", "edit", "WriteFile", "Edit"];
const SHELL_TOOLS: [&str; 2] = ["run_shell_command", "shell"];

pub fn default_root() -> PathBuf {
    dirs::home_dir().map(|h| h.join(".gemini")).unwrap_or_default()
}

fn text_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| p.get("thought").and_then(Value::as_bool) != Some(true))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn time(v: Option<&Value>) -> Option<f64> {
    let d = chrono::DateTime::parse_from_rfc3339(v?.as_str()?).ok()?;
    Some(d.timestamp_millis() as f64 / 1000.0)
}

// .jsonl sessions: one message per line (upserted by id), `$set` for session fields, `$rewindTo` to cut history.
fn replay(text: &str) -> Value {
    let mut doc = Map::new();
    let mut msgs: Vec<Value> = Vec::new();
    for line in text.lines() {
        let Ok(Value::Object(op)) = serde_json::from_str::<Value>(line) else { continue };
        if let Some(Value::Object(set)) = op.get("$set") {
            for (k, v) in set {
                if k == "messages" {
                    msgs = v.as_array().cloned().unwrap_or_default();
                } else {
                    doc.insert(k.clone(), v.clone());
                }
            }
        } else if let Some(id) = op.get("$rewindTo").and_then(Value::as_str) {
            if let Some(i) = msgs.iter().position(|m| m.get("id").and_then(Value::as_str) == Some(id)) {
                msgs.truncate(i);
            }
        } else if let Some(id) = op.get("id").and_then(Value::as_str) {
            let m = Value::Object(op.clone());
            match msgs.iter_mut().find(|x| x.get("id").and_then(Value::as_str) == Some(id)) {
                Some(old) => *old = m,
                None => msgs.push(m),
            }
        }
    }
    doc.insert("messages".into(), Value::Array(msgs));
    Value::Object(doc)
}

fn turns(doc: &Value) -> Vec<Turn> {
    let mut out: Vec<Turn> = Vec::new();
    let mut files: BTreeSet<String> = BTreeSet::new();
    let mut last = 0.0;
    for m in doc.get("messages").and_then(Value::as_array).into_iter().flatten() {
        let Some(ts) = time(m.get("timestamp")) else { continue };
        match m.get("type").and_then(Value::as_str) {
            Some("user") => {
                let prompt = text_of(m.get("content"));
                // Context the CLI adds on its own (0.71 saves it), which it also skips when resuming.
                if ["<session_context>", "<hook_context>"].iter().any(|tag| prompt.trim_start().starts_with(tag)) {
                    continue;
                }
                let (w, pasted) = prompt_stats(&prompt);
                if w == 0 {
                    continue;
                }
                if let Some(t) = out.last_mut().filter(|t| t.done.is_none()) {
                    t.files = files.len();
                    t.done = Some(f64::min(last, ts));
                }
                files.clear();
                last = ts;
                out.push(Turn { ts, words: w, pasted, shape: Some(prompt_shape(&prompt)), ..Turn::default() });
            }
            Some(ty @ ("gemini" | "error")) => {
                let Some(t) = out.last_mut().filter(|t| t.done.is_none()) else { continue };
                last = ts;
                if let Some(model) = m.get("model").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    t.model = Some(model.to_string());
                }
                t.reply_words += words(&text_of(m.get("content")));
                let calls: Vec<&Value> = m.get("toolCalls").and_then(Value::as_array).into_iter().flatten().collect();
                for c in &calls {
                    let at = time(c.get("timestamp")).unwrap_or(ts);
                    t.started = Some(t.started.map_or(at, |s| s.min(at)));
                    t.tools += 1;
                    let name = c.get("name").and_then(Value::as_str).unwrap_or("");
                    if EDIT_TOOLS.contains(&name) && c.get("status").and_then(Value::as_str) == Some("success") {
                        if let Some(p) = c.pointer("/args/file_path").and_then(Value::as_str) {
                            files.insert(p.to_string());
                        }
                    }
                }
                if calls.is_empty() || ty == "error" {
                    t.files = files.len();
                    t.done = Some(ts);
                }
            }
            // What the CLI notes when the person stops a reply with Esc.
            Some("info") if text_of(m.get("content")) == "Request cancelled." => {
                if let Some(t) = out.last_mut().filter(|t| t.done.is_none()) {
                    t.files = files.len();
                    t.done = Some(ts);
                    last = ts;
                }
            }
            _ => {}
        }
    }
    if let Some(t) = out.last_mut().filter(|t| t.done.is_none()) {
        t.files = files.len();
    }
    out
}

// Only tool calls that succeeded. The record's `resultDisplay` has the diff of an edit and the output of a shell command.
fn agent_work(doc: &Value, session: &str, cwd: Option<&Path>) -> Vec<Found> {
    let mut out = Vec::new();
    for m in doc.get("messages").and_then(Value::as_array).into_iter().flatten() {
        let Some(mts) = time(m.get("timestamp")) else { continue };
        for c in m.get("toolCalls").and_then(Value::as_array).into_iter().flatten() {
            if c.get("status").and_then(Value::as_str) != Some("success") {
                continue;
            }
            let ts = time(c.get("timestamp")).unwrap_or(mts);
            let name = c.get("name").and_then(Value::as_str).unwrap_or("");
            let rd = c.get("resultDisplay");
            if EDIT_TOOLS.contains(&name) {
                let path = rd
                    .and_then(|r| r.get("filePath"))
                    .or_else(|| c.pointer("/args/file_path"))
                    .and_then(Value::as_str);
                if let (Some(path), Some(counts)) = (path, rd.and_then(work::display_counts)) {
                    work::edit(ts, cwd, session, path, counts, &mut out);
                }
            } else if SHELL_TOOLS.contains(&name) {
                let Some(cmd) = c.pointer("/args/command").and_then(Value::as_str).filter(|c| c.contains("commit")) else { continue };
                let screen = rd.and_then(screen_text);
                let output = rd
                    .and_then(Value::as_str)
                    .or(screen.as_deref())
                    .or_else(|| c.pointer("/result/0/functionResponse/response/output").and_then(Value::as_str))
                    .unwrap_or("");
                let cmd = match c.pointer("/args/dir_path").and_then(Value::as_str) {
                    Some(d) => format!("cd {} && {cmd}", work::shell_join([d])),
                    None => cmd.to_string(),
                };
                work::commits(ts, cwd, session, &cmd, output, &mut out);
            }
        }
    }
    out.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    out
}

// 0.71 keeps a command's output as the terminal showed it: a list of lines, each a list of styled pieces.
fn screen_text(rd: &Value) -> Option<String> {
    let lines = rd.as_array()?.iter().map(|line| {
        let text: String = line.as_array().into_iter().flatten().filter_map(|p| p.get("text").and_then(Value::as_str)).collect();
        text.trim_end().to_string()
    });
    Some(lines.collect::<Vec<_>>().join("\n")).filter(|t| !t.trim().is_empty())
}

pub struct Gemini {
    root: PathBuf,
    snaps: Snapshots,
}

impl Gemini {
    pub fn new(root: PathBuf) -> Gemini {
        Gemini { root, snaps: Snapshots::default() }
    }

    // tmp/<slug>/chats/session-*.json[l]; subagent sessions sit one folder deeper and are skipped.
    fn files(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for slug in fs::read_dir(self.root.join("tmp")).into_iter().flatten().flatten() {
            let mut found: Vec<PathBuf> = fs::read_dir(slug.path().join("chats"))
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && matches!(p.extension().and_then(|e| e.to_str()), Some("json" | "jsonl")))
                .collect();
            let logs: BTreeSet<PathBuf> =
                found.iter().filter(|p| p.extension().is_some_and(|e| e == "jsonl")).map(|p| p.with_extension("")).collect();
            found.retain(|p| p.extension().is_some_and(|e| e == "jsonl") || !logs.contains(&p.with_extension("")));
            out.extend(found);
        }
        out
    }

    fn project(&self, slug: &Path) -> Option<PathBuf> {
        if let Ok(p) = fs::read_to_string(slug.join(".project_root")) {
            let p = p.trim();
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
        let name = slug.file_name()?.to_str()?;
        let text = fs::read_to_string(self.root.join("projects.json")).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        let (dir, _) = v.get("projects")?.as_object()?.iter().find(|(_, s)| s.as_str() == Some(name))?;
        Some(PathBuf::from(dir))
    }
}

impl Source for Gemini {
    fn name(&self) -> &'static str {
        "gemini_cli"
    }

    fn harness(&self) -> &'static str {
        "gemini-cli"
    }

    fn poll(&mut self) -> Vec<Found> {
        let started = now();
        let mut out = Vec::new();
        for path in self.snaps.changed(self.files()) {
            if fs::metadata(&path).map_or(true, |m| m.len() > MAX_FILE) {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else { continue };
            let jsonl = path.extension().is_some_and(|e| e == "jsonl");
            let doc = if jsonl {
                replay(&text)
            } else {
                match serde_json::from_str::<Value>(&text) {
                    Ok(d) => d,
                    Err(_) => continue,
                }
            };
            drop(text);
            // A .jsonl file names its session in a header line, and again in a `$set` only once it's resumed, so the
            // file name is the key that stays the same.
            let session = doc
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|_| !jsonl)
                .map(str::to_string)
                .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_default();
            let cwd = path.parent().and_then(Path::parent).and_then(|slug| self.project(slug));
            self.snaps.emit(&session, cwd.as_deref(), &turns(&doc), &mut out);
            self.snaps.emit_work(&session, agent_work(&doc, &session, cwd.as_deref()), &mut out);
        }
        self.snaps.finish(started);
        out.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        out
    }

    fn state(&self) -> Value {
        self.snaps.state()
    }

    fn restore(&mut self, state: &Value) {
        self.snaps.restore(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Kind;
    use serde_json::json;

    fn names(out: &[Found]) -> Vec<&'static str> {
        out.iter().map(|f| f.kind.name()).collect()
    }

    fn iso(t: f64) -> String {
        chrono::DateTime::from_timestamp_millis((t * 1000.0) as i64).unwrap().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    fn user(id: &str, t: f64, text: &str) -> Value {
        json!({ "id": id, "type": "user", "timestamp": iso(t), "content": [{ "text": text }] })
    }

    fn gemini(id: &str, t: f64, text: &str, calls: Value) -> Value {
        json!({ "id": id, "type": "gemini", "timestamp": iso(t), "content": text, "model": "gemini-x",
                "thoughts": [{ "subject": "s", "description": "private thinking" }], "toolCalls": calls })
    }

    fn edit(t: f64) -> Value {
        json!([{ "id": "c1", "name": "replace", "status": "success", "timestamp": iso(t), "args": { "file_path": "/w/p/a.rs" } },
               { "id": "c2", "name": "read_file", "status": "success", "timestamp": iso(t + 1.0), "args": { "file_path": "/w/p/b.rs" } }])
    }

    #[test]
    fn turns_mid_turn_prompts_and_errors() {
        let t0 = 1_000.0;
        let doc = json!({ "sessionId": "g1", "messages": [
            { "id": "i0", "type": "info", "timestamp": iso(t0 - 5.0), "content": "Loaded settings" },
            user("u1", t0, "please fix the failing parser test now"),
            gemini("g1", t0 + 3.0, "Looking at it.", edit(t0 + 5.0)),
            gemini("g2", t0 + 60.0, "Fixed it in a.rs.", json!([])),
            user("u2", t0 + 100.0, "now run all the tests"),
            gemini("g3", t0 + 102.0, "", json!([{ "name": "run_shell_command", "status": "success", "timestamp": iso(t0 + 103.0) }])),
            user("u3", t0 + 130.0, "stop that and explain instead"),
            { "id": "e1", "type": "error", "timestamp": iso(t0 + 140.0), "content": "quota exceeded" },
        ]});
        let ts = turns(&doc);
        assert_eq!(ts.len(), 3);
        assert_eq!((ts[0].started, ts[0].done), (Some(t0 + 5.0), Some(t0 + 60.0)));
        assert_eq!((ts[0].reply_words, ts[0].tools, ts[0].files), (7, 2, 1));
        assert_eq!(ts[0].model.as_deref(), Some("gemini-x"));
        assert_eq!(ts[1].done, Some(t0 + 102.0));
        assert_eq!(ts[2].done, Some(t0 + 140.0));
    }

    #[test]
    fn reads_sessions_skips_subagents_partial_lines_rewinds_and_resumes() {
        let dir = std::env::temp_dir().join(format!("kairos-gemini-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let chats = dir.join("tmp/slug1/chats");
        fs::create_dir_all(chats.join("session-1")).unwrap();
        fs::write(dir.join("projects.json"), r#"{"projects":{"/w/p":"slug1"}}"#).unwrap();
        let f = chats.join("session-1.jsonl");
        fs::write(&f, format!("{}\n", json!({ "$set": { "sessionId": "g1", "messages": [] } }))).unwrap();
        let mut src = Gemini::new(dir.clone());
        assert!(src.poll().is_empty());

        let t0 = now().ceil() + 1.0;
        let lines = |extra: &[Value]| {
            let mut s = format!("{}\n", json!({ "$set": { "sessionId": "g1", "messages": [] } }));
            for v in extra {
                s.push_str(&format!("{v}\n"));
            }
            s
        };
        let wrong = user("u0", t0 - 0.5, "a prompt that gets rewound away");
        let rewind = json!({ "$rewindTo": "u0" });
        let mut text = lines(&[wrong, rewind, user("u1", t0, "please fix the failing parser test now"), gemini("g1", t0 + 3.0, "Looking at it.", edit(t0 + 5.0))]);
        text.push_str(r#"{"id":"g2","type":"gem"#);
        fs::write(&f, &text).unwrap();
        fs::write(chats.join("session-1/sub.json"), json!({ "sessionId": "sub", "messages": [user("s1", t0 + 1.0, "delegated task for the subagent")] }).to_string()).unwrap();
        let out = src.poll();
        assert_eq!(names(&out), ["prompt", "agent_start"]);
        assert_eq!(out[0].cwd.as_deref(), Some(Path::new("/w/p")));
        assert!(matches!(&out[0].kind, Kind::Prompt { session, words: 7, .. } if session == "session-1"));

        let mut again = Gemini::new(dir.clone());
        again.restore(&src.state());
        let text = lines(&[user("u1", t0, "please fix the failing parser test now"), gemini("g1", t0 + 3.0, "Looking at it.", edit(t0 + 5.0)), gemini("g2", t0 + 60.0, "Fixed it in a.rs.", json!([]))]);
        fs::write(&f, text).unwrap();
        let out = again.poll();
        assert_eq!(names(&out), ["reply_done"]);
        assert!(matches!(&out[0].kind, Kind::ReplyDone { reply_words: 7, tools: 2, files_changed: 1, .. }));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn gemini_0_71_sessions_keep_their_key_skip_added_context_and_end_at_esc() {
        let dir = std::env::temp_dir().join(format!("kairos-gemini-71-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let chats = dir.join("tmp/slug1/chats");
        fs::create_dir_all(&chats).unwrap();
        fs::write(dir.join("projects.json"), r#"{"projects":{"/w/p":"slug1"}}"#).unwrap();
        let f = chats.join("session-2.jsonl");
        let mut text = format!("{}\n", json!({ "sessionId": "g2", "projectHash": "h", "startTime": iso(1_000.0), "lastUpdated": iso(1_000.0), "kind": "main" }));
        fs::write(&f, &text).unwrap();
        let mut src = Gemini::new(dir.clone());
        assert!(src.poll().is_empty());

        let t0 = now().ceil() + 2.0;
        let context = "<session_context>\nThis is the Gemini CLI. We are setting up the context for our chat.\nMy operating system is: linux";
        let shell = json!([{ "id": "c3", "name": "run_shell_command", "status": "success", "timestamp": iso(t0 + 12.0), "args": { "command": "cargo test" } }]);
        let cancelled = json!({ "id": "i1", "type": "info", "timestamp": iso(t0 + 15.0), "content": "Request cancelled." });
        for v in [user("c0", t0 - 1.0, context), user("u1", t0, "please fix the failing parser test now"), gemini("g1", t0 + 3.0, "", edit(t0 + 5.0)),
                  gemini("g2", t0 + 8.0, "Fixed it.", json!([])), user("u2", t0 + 10.0, "now run all the tests"), gemini("g3", t0 + 11.0, "", shell), cancelled] {
            text.push_str(&format!("{v}\n"));
        }
        fs::write(&f, &text).unwrap();
        let out = src.poll();
        assert_eq!(names(&out), ["prompt", "agent_start", "reply_done", "prompt", "agent_start", "reply_done"]);
        assert!(matches!(&out[0].kind, Kind::Prompt { session, words: 7, .. } if session == "session-2"));
        assert_eq!(out[5].ts, t0 + 15.0);

        // Resuming names the session again and rewrites old records to match the model's history.
        let patch = json!({ "$patch": { "updates": [{ "id": "c0", "content": [{ "text": context }] }] } });
        for v in [json!({ "$set": { "sessionId": "g2" } }), patch, user("u3", t0 + 20.0, "thanks, that is all")] {
            text.push_str(&format!("{v}\n"));
        }
        fs::write(&f, &text).unwrap();
        let out = src.poll();
        assert_eq!(names(&out), ["prompt"]);
        assert!(matches!(&out[0].kind, Kind::Prompt { session, words: 4, .. } if session == "session-2"));
        let _ = fs::remove_dir_all(&dir);
    }

    fn call(id: &str, name: &str, status: &str, t: f64, args: Value, display: Value) -> Value {
        json!({ "id": id, "name": name, "status": status, "timestamp": iso(t), "args": args, "resultDisplay": display,
                "result": [{ "functionResponse": { "id": id, "name": name, "response": { "output": "hidden" } } }] })
    }

    fn work_of(calls: Vec<Value>) -> Vec<Found> {
        let doc = json!({ "sessionId": "g1", "messages": [gemini("m1", 1_000.0, "", Value::Array(calls))] });
        agent_work(&doc, "g1", Some(Path::new("/w/p")))
    }

    fn summary(out: &[Found]) -> Vec<String> {
        out.iter()
            .map(|f| match &f.kind {
                Kind::AgentEdit { path, added, removed, .. } => format!("{path} +{added} -{removed}"),
                Kind::AgentCommit { sha, .. } => format!("commit {} in {}", sha.as_deref().unwrap_or("-"), f.cwd.as_ref().unwrap().display().to_string().replace('\\', "/")),
                k => panic!("{k:?}"),
            })
            .collect()
    }

    #[test]
    fn edits_count_the_written_diff_and_commits_need_a_printed_sha() {
        let diff = "Index: a.rs\n===\n--- a.rs\tCurrent\n+++ a.rs\tProposed\n@@ -1,3 +1,3 @@\n keep\n-old\n+new\n keep\n@@ -20 +20,2 @@\n x\n+y\n";
        let out = work_of(vec![
            call("c1", "replace", "success", 1_001.0, json!({ "file_path": "/w/p/a.rs" }), json!({ "fileDiff": diff, "fileName": "a.rs", "filePath": "/w/p/a.rs", "diffStat": { "model_added_lines": 9 } })),
            call("c2", "write_file", "success", 1_002.0, json!({ "file_path": "/w/p/new.rs" }), json!({ "fileName": "new.rs", "originalContent": null, "newContent": "a\nb\nc\n", "isNewFile": true })),
            call("c3", "write_file", "success", 1_003.0, json!({ "file_path": "/w/p/over.rs" }), json!({ "originalContent": "a\nb\nc\n", "newContent": "a\nx\nc\nd\n" })),
            call("c4", "replace", "success", 1_004.0, json!({ "file_path": "/w/p/stat.rs" }), json!({ "diffStat": { "model_added_lines": 2, "model_removed_lines": 1, "user_added_lines": 0, "user_removed_lines": 0 } })),
            call("c5", "run_shell_command", "success", 1_005.0, json!({ "command": "git commit -am 'fix it'", "dir_path": "sub" }), json!("[main abc1234] fix it\n 1 file changed")),
            call("c6", "run_shell_command", "success", 1_006.0, json!({ "command": "git commit -m 'two'" }),
                 json!([[{ "text": "[main def5678] two    ", "bold": false }], [{ "text": " 1 file changed    " }], [{ "text": " ", "inverse": true }, { "text": "    " }]])),
        ]);
        assert_eq!(
            summary(&out),
            ["/w/p/a.rs +2 -1", "/w/p/new.rs +3 -0", "/w/p/over.rs +2 -1", "/w/p/stat.rs +2 -1", "commit abc1234 in /w/p/sub", "commit def5678 in /w/p"]
        );
        assert!(matches!(&out[0].kind, Kind::AgentEdit { session, .. } if session == "g1"));
    }

    #[test]
    fn rejected_edits_other_commands_and_failed_commits_make_nothing() {
        let shown = json!({ "fileDiff": "@@ -1 +1 @@\n-a\n+b\n", "filePath": "/w/p/a.rs" });
        let out = work_of(vec![
            call("c1", "replace", "cancelled", 1_001.0, json!({ "file_path": "/w/p/a.rs" }), shown.clone()),
            call("c2", "replace", "error", 1_002.0, json!({ "file_path": "/w/p/a.rs" }), json!("Error: no match")),
            call("c3", "replace", "success", 1_003.0, json!({ "file_path": "/w/p/a.rs" }), json!("Successfully modified")),
            call("c4", "run_shell_command", "success", 1_004.0, json!({ "command": "git status" }), json!("[main abc1234] stray")),
            call("c5", "run_shell_command", "success", 1_005.0, json!({ "command": "git commit -m x" }), json!("nothing to commit, working tree clean")),
            call("c6", "run_shell_command", "error", 1_006.0, json!({ "command": "git commit -m x" }), json!("hook failed")),
            call("c7", "read_file", "success", 1_007.0, json!({ "file_path": "/w/p/a.rs" }), shown),
        ]);
        assert!(out.is_empty(), "{out:?}");
    }
}
