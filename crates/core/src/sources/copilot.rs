use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::snapshot::{mtime, now, Snapshots, Turn};
use super::{uri_path, words, Found, Source};
use crate::{prompt_shape, prompt_stats};

const MAX_FILE: u64 = 64 * 1024 * 1024;
const EDIT_KINDS: [&str; 3] = ["textEditGroup", "notebookEditGroup", "workspaceEdit"];

pub fn default_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(cfg) = dirs::config_dir() {
        for name in ["Code", "Code - Insiders", "VSCodium", "VSCodium - Insiders", "Code - OSS"] {
            roots.push(cfg.join(name));
        }
    }
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".var/app/com.visualstudio.code/config/Code"));
        roots.push(home.join("snap/code/current/.config/Code"));
        roots.push(home.join(".vscode-server/data"));
        roots.push(home.join(".vscode-server-insiders/data"));
    }
    if let Some(p) = std::env::var_os("VSCODE_PORTABLE") {
        roots.push(PathBuf::from(p).join("user-data"));
    }
    roots.retain(|r| r.is_dir());
    roots
}

fn sessions_in(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("json" | "jsonl")))
        .collect();
    // A session migrated to the patch log keeps its old .json next to it.
    let logs: BTreeSet<PathBuf> =
        found.iter().filter(|p| p.extension().is_some_and(|e| e == "jsonl")).map(|p| p.with_extension("")).collect();
    found.retain(|p| p.extension().is_some_and(|e| e == "jsonl") || !logs.contains(&p.with_extension("")));
    out.extend(found);
}

// Each window keeps its chats under its own workspace folder, so a session's project is that folder.
fn workspace_folder(storage: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(storage.join("workspace.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    if let Some(f) = v.get("folder").and_then(Value::as_str) {
        return uri_path(f);
    }
    let ws = uri_path(v.get("workspace")?.as_str()?)?;
    ws.parent().map(Path::to_path_buf)
}

fn key<'a>(k: &Value, v: &'a mut Value) -> Option<&'a mut Value> {
    match (k, v) {
        (Value::String(s), Value::Object(m)) => m.get_mut(s),
        (Value::Number(n), Value::Array(a)) => a.get_mut(n.as_u64()? as usize),
        _ => None,
    }
}

fn walk<'a>(doc: &'a mut Value, path: &[Value]) -> Option<&'a mut Value> {
    path.iter().try_fold(doc, |v, k| key(k, v))
}

// VS Code's ObjectMutationLog: 0 = initial value, 1 = set at path, 2 = push (truncate to `i` first), 3 = delete.
fn replay(text: &str) -> Option<Value> {
    let mut doc: Option<Value> = None;
    for line in text.lines() {
        let Ok(op) = serde_json::from_str::<Value>(line) else { continue };
        let path: Vec<Value> = op.get("k").and_then(Value::as_array).cloned().unwrap_or_default();
        match op.get("kind").and_then(Value::as_u64) {
            Some(0) => doc = op.get("v").cloned(),
            Some(1) => {
                let Some(d) = doc.as_mut() else { continue };
                let v = op.get("v").cloned().unwrap_or(Value::Null);
                let Some((last, parent)) = path.split_last() else {
                    *d = v;
                    continue;
                };
                match (walk(d, parent), last) {
                    (Some(Value::Object(m)), Value::String(s)) => {
                        m.insert(s.clone(), v);
                    }
                    (Some(Value::Array(a)), Value::Number(n)) => match n.as_u64().map(|n| n as usize) {
                        Some(i) if i < a.len() => a[i] = v,
                        Some(i) if i == a.len() => a.push(v),
                        _ => {}
                    },
                    _ => {}
                }
            }
            Some(2) => {
                let Some(Value::Array(a)) = doc.as_mut().and_then(|d| walk(d, &path)) else { continue };
                if let Some(i) = op.get("i").and_then(Value::as_u64) {
                    a.truncate(i as usize);
                }
                a.extend(op.get("v").and_then(Value::as_array).cloned().unwrap_or_default());
            }
            Some(3) => {
                let Some((last, parent)) = path.split_last() else { continue };
                if let (Some(Value::Object(m)), Value::String(s)) = (doc.as_mut().and_then(|d| walk(d, parent)), last) {
                    m.remove(s);
                }
            }
            _ => {}
        }
    }
    doc
}

fn flag(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool) == Some(true)
}

fn ms(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).map(|m| m / 1000.0)
}

fn uri_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(_) => ["fsPath", "path", "external"].iter().find_map(|k| v.get(*k)?.as_str().map(str::to_string)),
        _ => None,
    }
}

fn turns(session: &Value, file_time: f64) -> Vec<Turn> {
    let reqs: Vec<&Value> = session.get("requests").and_then(Value::as_array).into_iter().flatten().collect();
    let mut out = Vec::new();
    for (n, r) in reqs.iter().enumerate() {
        if ["isSystemInitiated", "isHidden", "hiddenFromTranscript", "requestHiddenFromTranscript"].iter().any(|k| flag(r, k))
            || r.get("agentHostMessageOrigin").is_some_and(|o| !o.is_null())
        {
            continue;
        }
        let text = match r.get("message") {
            Some(Value::String(s)) => s.as_str(),
            Some(m) => m.get("text").and_then(Value::as_str).unwrap_or(""),
            None => "",
        };
        let (words_typed, pasted) = prompt_stats(text);
        let Some(ts) = ms(r.get("timestamp")) else { continue };
        if words_typed == 0 {
            continue;
        }
        let mut t = Turn { ts, words: words_typed, pasted, shape: Some(prompt_shape(text)), ..Turn::default() };
        t.model = r.get("modelId").and_then(Value::as_str).filter(|m| !m.is_empty()).map(str::to_string);
        let mut files = BTreeSet::new();
        for item in r.get("response").and_then(Value::as_array).into_iter().flatten() {
            if item.get("subAgentInvocationId").is_some_and(|s| !s.is_null()) {
                continue;
            }
            match item.get("kind").and_then(Value::as_str) {
                None => t.reply_words += item.get("value").and_then(Value::as_str).map_or(0, words),
                Some("markdownContent") => {
                    t.reply_words += item.pointer("/content/value").and_then(Value::as_str).map_or(0, words);
                }
                Some("toolInvocationSerialized") => t.tools += 1,
                Some(k) if EDIT_KINDS.contains(&k) => {
                    if let Some(u) = item.get("uri").and_then(uri_of) {
                        files.insert(u);
                    }
                }
                _ => {}
            }
        }
        t.files = files.len();
        let result = r.get("result");
        if t.tools > 0 || t.files > 0 {
            let first = ms(result.and_then(|x| x.pointer("/timings/firstProgress"))).unwrap_or(0.0);
            t.started = Some(ts + first);
        }
        let state = r.get("modelState");
        let value = state.and_then(|s| s.get("value")).and_then(Value::as_u64);
        let next = reqs.get(n + 1).and_then(|x| ms(x.get("timestamp")));
        // ResponseModelState: 0 pending, 1 complete, 2 cancelled, 3 failed, 4 needs input (a tool call to approve).
        let finished = matches!(value, Some(1..=3)) || (state.is_none() && result.is_some());
        t.seen = file_time;
        t.waiting = value == Some(4);
        // VS Code cancels a reply it finds unfinished when it opens again, as of then, and records no working time.
        t.cut_off = value == Some(2) && r.get("elapsedMs").is_none_or(Value::is_null);
        t.done = ms(state.and_then(|s| s.get("completedAt")))
            .or_else(|| {
                finished.then(|| {
                    ms(r.get("responseTimestamp"))
                        .or_else(|| ms(r.get("elapsedMs")).map(|e| ts + e))
                        .or_else(|| ms(result.and_then(|x| x.pointer("/timings/totalElapsed"))).map(|e| ts + e))
                        .unwrap_or(file_time)
                })
            })
            .or(next);
        if let (Some(d), Some(nx)) = (t.done, next) {
            t.done = Some(d.min(nx));
        }
        out.push(t);
    }
    out
}

pub struct Copilot {
    roots: Vec<PathBuf>,
    snaps: Snapshots,
    folders: HashMap<PathBuf, Option<PathBuf>>,
}

impl Copilot {
    pub fn new(roots: Vec<PathBuf>) -> Copilot {
        Copilot { roots, snaps: Snapshots::default(), folders: HashMap::new() }
    }

    fn files(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for ud in &self.roots {
            for e in fs::read_dir(ud.join("User/workspaceStorage")).into_iter().flatten().flatten() {
                sessions_in(&e.path().join("chatSessions"), &mut out);
            }
            sessions_in(&ud.join("User/globalStorage/emptyWindowChatSessions"), &mut out);
        }
        out
    }
}

impl Source for Copilot {
    fn name(&self) -> &'static str {
        "vscode_copilot"
    }

    fn harness(&self) -> &'static str {
        "copilot"
    }

    fn poll(&mut self) -> Vec<Found> {
        let started = now();
        let mut out = Vec::new();
        for path in self.snaps.changed(self.files()) {
            let Ok(meta) = fs::metadata(&path) else { continue };
            if meta.len() > MAX_FILE {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else { continue };
            let doc = if path.extension().is_some_and(|e| e == "jsonl") {
                replay(&text)
            } else {
                serde_json::from_str(&text).ok()
            };
            drop(text);
            let Some(doc) = doc else { continue };
            let session = doc
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_default();
            let storage = path.parent().and_then(Path::parent).map(Path::to_path_buf).unwrap_or_default();
            let cwd = match doc.get("workingDirectory").and_then(uri_of).and_then(|u| uri_path(&u)) {
                Some(dir) => Some(dir),
                None if path.components().any(|c| c.as_os_str() == "workspaceStorage") => {
                    self.folders.entry(storage).or_insert_with_key(|s| workspace_folder(s)).clone()
                }
                None => None,
            };
            let turns = turns(&doc, mtime(&meta));
            self.snaps.emit(&session, cwd.as_deref(), &turns, &mut out);
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

    fn line(v: Value) -> String {
        format!("{v}\n")
    }

    fn request(id: &str, at: f64, text: &str) -> Value {
        json!({ "requestId": id, "timestamp": at * 1000.0, "message": { "text": text, "parts": [] },
                "modelId": "copilot/gpt-x", "response": [], "modelState": { "value": 0 } })
    }

    fn response() -> Value {
        json!([
            { "value": "Looking at it." },
            { "kind": "thinking", "value": "private reasoning words here" },
            { "kind": "toolInvocationSerialized", "toolId": "read" },
            { "kind": "toolInvocationSerialized", "toolId": "edit", "subAgentInvocationId": "sub-1" },
            { "kind": "textEditGroup", "uri": { "$mid": 1, "fsPath": "/w/p/a.rs", "scheme": "file" }, "edits": [] },
            { "kind": "textEditGroup", "uri": { "fsPath": "/w/p/sub.rs" }, "subAgentInvocationId": "sub-1" },
            { "value": "Fixed it in a.rs." }
        ])
    }

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let d = std::env::temp_dir().join(format!("kairos-copilot-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&d);
            let ws = d.join("User/workspaceStorage/abc");
            fs::create_dir_all(ws.join("chatSessions")).unwrap();
            fs::write(ws.join("workspace.json"), r#"{"folder":"file:///w/my%20proj"}"#).unwrap();
            Dir(d)
        }

        fn session(&self) -> PathBuf {
            self.0.join("User/workspaceStorage/abc/chatSessions/s1.jsonl")
        }

        fn append(&self, text: &str) {
            let mut all = fs::read_to_string(self.session()).unwrap_or_default();
            all.push_str(text);
            fs::write(self.session(), all).unwrap();
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn patch_log_turn_with_resume_partial_lines_and_compaction() {
        let dir = Dir::new("flow");
        let t0 = now().ceil() + 1.0;
        fs::write(dir.session(), line(json!({ "kind": 0, "v": { "version": 3, "sessionId": "s1", "requests": [] } })))
            .unwrap();
        let mut src = Copilot::new(vec![dir.0.clone()]);
        assert!(src.poll().is_empty());

        dir.append(&line(
            json!({ "kind": 2, "k": ["requests"], "v": [request("r1", t0, "please fix the failing parser test now")] }),
        ));
        dir.append(r#"{"kind":2,"k":["requests",0,"respo"#);
        let out = src.poll();
        assert_eq!(names(&out), ["prompt"]);
        assert_eq!(out[0].cwd.as_deref(), Some(Path::new("/w/my proj")));
        assert!(
            matches!(&out[0].kind, Kind::Prompt { words: 7, model: Some(m), session, .. } if m == "copilot/gpt-x" && session == "s1")
        );

        let text = fs::read_to_string(dir.session()).unwrap();
        fs::write(dir.session(), &text[..text.rfind('\n').unwrap() + 1]).unwrap();
        dir.append(&line(json!({ "kind": 2, "k": ["requests", 0, "response"], "v": response(), "i": 0 })));
        dir.append(&line(json!({ "kind": 1, "k": ["requests", 0, "result"], "v": { "timings": { "firstProgress": 2000 } } })));
        let out = src.poll();
        assert_eq!(names(&out), ["agent_start"]);
        assert_eq!(out[0].ts, t0 + 2.0);

        let mut again = Copilot::new(vec![dir.0.clone()]);
        again.restore(&src.state());
        dir.append(&line(
            json!({ "kind": 1, "k": ["requests", 0, "modelState"], "v": { "value": 1, "completedAt": (t0 + 60.0) * 1000.0 } }),
        ));
        let out = again.poll();
        assert_eq!(names(&out), ["reply_done"]);
        assert_eq!(out[0].ts, t0 + 60.0);
        match &out[0].kind {
            Kind::ReplyDone { reply_words, tools, files_changed, .. } => {
                assert_eq!((*reply_words, *tools, *files_changed), (7, 1, 1));
            }
            k => panic!("{k:?}"),
        }

        let mut whole = replay(&fs::read_to_string(dir.session()).unwrap()).unwrap();
        whole["requests"].as_array_mut().unwrap().push(request("r2", t0 + 120.0, "now add a test"));
        fs::write(dir.session(), line(json!({ "kind": 0, "v": whole }))).unwrap();
        assert_eq!(names(&again.poll()), ["prompt"]);
    }

    fn written(path: &Path, at: f64) {
        let f = fs::File::options().write(true).open(path).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs_f64(at)).unwrap();
    }

    fn state(value: u64) -> String {
        line(json!({ "kind": 1, "k": ["requests", 0, "modelState"], "v": { "value": value } }))
    }

    fn reply_counts(f: &Found) -> (usize, usize, usize) {
        match &f.kind {
            Kind::ReplyDone { reply_words, tools, files_changed, .. } => (*reply_words, *tools, *files_changed),
            k => panic!("{k:?}"),
        }
    }

    #[test]
    fn a_tool_call_to_approve_ends_the_run_until_the_person_answers() {
        let dir = Dir::new("approve");
        let t0 = now().ceil() + 1.0;
        fs::write(dir.session(), line(json!({ "kind": 0, "v": { "version": 3, "sessionId": "s1", "requests": [] } })))
            .unwrap();
        let mut src = Copilot::new(vec![dir.0.clone()]);
        assert!(src.poll().is_empty());

        let mut r = request("r1", t0, "please fix the failing parser test now");
        r["response"] = response();
        r["result"] = json!({ "timings": { "firstProgress": 2000 } });
        dir.append(&line(json!({ "kind": 2, "k": ["requests"], "v": [r] })));
        written(&dir.session(), t0 + 5.0);
        assert_eq!(names(&src.poll()), ["prompt", "agent_start"]);

        dir.append(&state(4));
        written(&dir.session(), t0 + 20.0);
        let out = src.poll();
        assert_eq!(names(&out), ["reply_done"]);
        assert_eq!((out[0].ts, reply_counts(&out[0])), (t0 + 20.0, (7, 1, 1)));

        let mut again = Copilot::new(vec![dir.0.clone()]);
        again.restore(&src.state());
        dir.append(&state(0));
        written(&dir.session(), t0 + 80.0);
        let out = again.poll();
        assert_eq!(names(&out), ["agent_start"]);
        assert_eq!(out[0].ts, t0 + 80.0);

        let mut more = response();
        more.as_array_mut().unwrap().push(json!({ "value": "All tests pass now." }));
        dir.append(&line(json!({ "kind": 1, "k": ["requests", 0, "response"], "v": more })));
        dir.append(&line(json!({ "kind": 1, "k": ["requests", 0, "modelState"], "v": { "value": 1, "completedAt": (t0 + 100.0) * 1000.0 } })));
        written(&dir.session(), t0 + 100.0);
        let out = again.poll();
        assert_eq!(names(&out), ["reply_done"]);
        assert_eq!((out[0].ts, reply_counts(&out[0])), (t0 + 100.0, (4, 0, 0)));
    }

    #[test]
    fn a_reply_cut_off_by_closing_vs_code_ends_at_its_last_write() {
        for (elapsed, end) in [(None, 30.0), (Some(25_000.0), 3600.0)] {
            let dir = Dir::new("cut");
            let t0 = now().ceil() + 1.0;
            fs::write(dir.session(), line(json!({ "kind": 0, "v": { "version": 3, "sessionId": "s1", "requests": [] } })))
                .unwrap();
            let mut src = Copilot::new(vec![dir.0.clone()]);
            assert!(src.poll().is_empty());

            let mut r = request("r1", t0, "please fix the failing parser test now");
            r["response"] = response();
            dir.append(&line(json!({ "kind": 2, "k": ["requests"], "v": [r] })));
            written(&dir.session(), t0 + 30.0);
            assert_eq!(names(&src.poll()), ["prompt", "agent_start"]);

            // Opened again an hour later, VS Code marks it cancelled as of then. A cancel it timed has elapsedMs.
            if let Some(e) = elapsed {
                dir.append(&line(json!({ "kind": 1, "k": ["requests", 0, "elapsedMs"], "v": e })));
            }
            let cancelled = json!({ "value": 2, "completedAt": (t0 + 3600.0) * 1000.0 });
            dir.append(&line(json!({ "kind": 1, "k": ["requests", 0, "modelState"], "v": cancelled })));
            written(&dir.session(), t0 + 3600.0);
            let out = src.poll();
            assert_eq!(names(&out), ["reply_done"]);
            assert_eq!(out[0].ts, t0 + end);
        }
    }

    #[test]
    fn mid_turn_interrupted_and_injected_requests() {
        let t0 = now().ceil() + 1.0;
        let mut first = request("r1", t0, "please fix the failing parser test now");
        first["response"] = json!([{ "kind": "toolInvocationSerialized" }]);
        let mut cancelled = request("r2", t0 + 30.0, "stop and look at the docs instead");
        cancelled["modelState"] = json!({ "value": 2, "completedAt": (t0 + 40.0) * 1000.0 });
        let mut system = request("r3", t0 + 50.0, "terminal command finished with exit code 0");
        system["isSystemInitiated"] = json!(true);
        let mut legacy = request("r4", t0 + 70.0, "explain this function to me");
        legacy.as_object_mut().unwrap().remove("modelState");
        legacy["result"] = json!({ "timings": { "totalElapsed": 5000 } });
        let doc = json!({ "sessionId": "s1", "requests": [first, cancelled, system, legacy] });
        let turns = turns(&doc, t0 + 100.0);
        assert_eq!(turns.len(), 3);
        assert_eq!((turns[0].started, turns[0].done), (Some(t0), Some(t0 + 30.0)));
        assert_eq!((turns[1].started, turns[1].done), (None, Some(t0 + 40.0)));
        assert_eq!(turns[2].done, Some(t0 + 75.0));
    }

    #[test]
    fn legacy_json_session_in_an_empty_window() {
        let dir = Dir::new("legacy");
        let empty = dir.0.join("User/globalStorage/emptyWindowChatSessions");
        fs::create_dir_all(&empty).unwrap();
        let mut src = Copilot::new(vec![dir.0.clone()]);
        assert!(src.poll().is_empty());
        let t0 = now().ceil() + 1.0;
        let mut r = request("r1", t0, "what does this regex match exactly");
        r["modelState"] = json!({ "value": 1, "completedAt": (t0 + 9.0) * 1000.0 });
        r["response"] = json!([{ "value": "It matches digits." }]);
        fs::write(empty.join("e1.json"), json!({ "sessionId": "e1", "requests": [r] }).to_string()).unwrap();
        let out = src.poll();
        assert_eq!(names(&out), ["prompt", "reply_done"]);
        assert_eq!(out[0].cwd, None);
        assert!(matches!(&out[1].kind, Kind::ReplyDone { reply_words: 3, tools: 0, .. }));
        assert!(src.poll().is_empty());
    }
}
