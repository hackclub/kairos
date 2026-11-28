use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{snapshot, uri_path, work, words, Found, Source, Tail};
use crate::event::Kind;
use crate::parse::timestamp;
use crate::{prompt_shape, prompt_stats};

const SHELL_TOOLS: [&str; 2] = ["bash", "powershell"];
// Pi writes the context_edit that hides a failed reply it is about to retry right after that reply; a failed reply with
// none after this long ended its turn.
const RETRY_WAIT_SECS: f64 = 10.0;

// $PI_CODING_AGENT_DIR (default ~/.pi/agent)/sessions/--<cwd>--/<time>_<id>.jsonl, and $PI_CODING_AGENT_SESSION_DIR
// holding sessions of every folder side by side.
pub fn default_roots() -> Vec<PathBuf> {
    let agent = std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".pi").join("agent")));
    agent
        .map(|a| a.join("sessions"))
        .into_iter()
        .chain(std::env::var_os("PI_CODING_AGENT_SESSION_DIR").map(PathBuf::from))
        .collect()
}

#[derive(Default)]
struct Turn {
    known: bool,
    foreign: bool,
    session: String,
    cwd: Option<PathBuf>,
    // A fork copies the old session's entries, times and all, after its own header; anything older than the newest
    // entry read so far is such a copy.
    floor: f64,
    model: Option<String>,
    armed: bool,
    open: bool,
    last_ts: f64,
    failed: Option<(String, f64)>,
    reply_words: usize,
    tools: usize,
    files: BTreeSet<PathBuf>,
    calls: HashMap<String, Call>,
}

enum Call {
    Edit(String),
    Write(String),
    Shell(String),
}

impl Turn {
    fn header(&mut self, e: &Value) {
        self.known = true;
        if let Some(id) = e.get("id").and_then(Value::as_str) {
            self.session = id.to_string();
        }
        if let Some(c) = e.get("cwd").and_then(Value::as_str).filter(|c| !c.is_empty()) {
            self.cwd = Some(PathBuf::from(c));
        }
        if let Some(ts) = timestamp(e) {
            self.floor = self.floor.max(ts);
        }
    }

    // Tail skips history on first run, so the header line may never reach feed().
    fn head(&mut self, path: &Path) {
        self.known = true;
        let mut line = String::new();
        if let Ok(f) = File::open(path) {
            let _ = BufReader::new(f).read_line(&mut line);
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(e) if e.get("type").and_then(Value::as_str) == Some("session") => self.header(&e),
            _ => self.foreign = true,
        }
    }

    fn close(&mut self, ts: f64, out: &mut Vec<Found>) {
        self.failed = None;
        if !self.open {
            return;
        }
        self.open = false;
        self.armed = false;
        out.push(Found {
            ts,
            cwd: self.cwd.clone(),
            kind: Kind::ReplyDone {
                session: self.session.clone(),
                reply_words: self.reply_words,
                tools: self.tools,
                files_changed: self.files.len(),
                model: self.model.clone(),
            },
        });
    }

    fn settle(&mut self, now: f64, out: &mut Vec<Found>) {
        if let Some((_, ts)) = self.failed {
            if now - ts >= RETRY_WAIT_SECS {
                self.close(ts, out);
            }
        }
    }

    fn file(&mut self, p: &str) {
        self.files.insert(self.cwd.clone().unwrap_or_default().join(p));
    }
}

fn blocks(m: &Value) -> &[Value] {
    m.get("content").and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn text(m: &Value) -> String {
    if let Some(s) = m.get("content").and_then(Value::as_str) {
        return s.to_string();
    }
    blocks(m)
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

// `/skill:name args` is logged with the skill's whole file in front of the args.
fn typed(text: &str) -> String {
    let skill = text.strip_prefix("<skill name=\"").and_then(|r| Some((r.split_once('"')?.0, r.split_once("\n</skill>")?.1)));
    match skill {
        Some((name, args)) => format!("/skill:{name} {}", args.trim()).trim_end().to_string(),
        None => text.to_string(),
    }
}

// Pi's own reading of a tool's path (normalizePath in utils/paths.ts): a leading @ goes, Git Bash and WSL drive paths
// on Windows, ~ is the home folder, file:// URLs. A relative path is relative to the session's folder.
fn tool_path(p: &str) -> Option<String> {
    let p = p.strip_prefix('@').unwrap_or(p);
    if cfg!(windows) && p.starts_with('/') && !p.starts_with("//") && !p.contains('\\') {
        let rest = p[1..].strip_prefix("mnt/").or_else(|| p[1..].strip_prefix("cygdrive/")).unwrap_or(&p[1..]);
        let (drive, tail) = rest.split_once('/').unwrap_or((rest, ""));
        if drive.len() == 1 && drive.as_bytes()[0].is_ascii_alphabetic() {
            return Some(format!("{}:\\{}", drive.to_ascii_uppercase(), tail.replace('/', "\\")));
        }
    }
    let home = |rest: &str| dirs::home_dir().map(|h| h.join(rest).to_string_lossy().into_owned());
    if p == "~" {
        return home("");
    }
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\").filter(|_| cfg!(windows))) {
        return home(rest);
    }
    if p.starts_with("file://") {
        return uri_path(p).map(|p| p.to_string_lossy().into_owned());
    }
    (!p.is_empty()).then(|| p.to_string())
}

fn prompt(t: &mut Turn, m: &Value, ts: f64, out: &mut Vec<Found>) {
    let text = typed(&text(m));
    let image = blocks(m).iter().any(|b| b.get("type").and_then(Value::as_str) == Some("image"));
    if text.trim().is_empty() && !image {
        return;
    }
    let last = t.last_ts;
    t.close(last.min(ts), out);
    let (words, pasted) = prompt_stats(&text);
    t.armed = true;
    t.open = true;
    t.last_ts = ts;
    t.reply_words = 0;
    t.tools = 0;
    t.files.clear();
    out.push(Found {
        ts,
        cwd: t.cwd.clone(),
        kind: Kind::Prompt {
            words,
            pasted,
            model: t.model.clone(),
            session: t.session.clone(),
            shape: Some(prompt_shape(&text)),
            version: None,
            via: None,
            permissions: None,
        },
    });
}

fn reply(t: &mut Turn, id: Option<&str>, m: &Value, ts: f64, out: &mut Vec<Found>) {
    let calls: Vec<&Value> = blocks(m).iter().filter(|b| b.get("type").and_then(Value::as_str) == Some("toolCall")).collect();
    for c in &calls {
        let (Some(cid), Some(name)) = (c.get("id").and_then(Value::as_str), c.get("name").and_then(Value::as_str)) else { continue };
        let arg = |k: &str| c.pointer(&format!("/arguments/{k}")).and_then(Value::as_str);
        let call = match name {
            "edit" => arg("path").and_then(tool_path).map(Call::Edit),
            "write" => arg("path").and_then(tool_path).map(Call::Write),
            n if SHELL_TOOLS.contains(&n) => arg("command").filter(|c| c.contains("commit")).map(|c| Call::Shell(c.to_string())),
            _ => None,
        };
        if let Some(call) = call {
            work::remember(&mut t.calls, cid.to_string(), call);
        }
    }
    if !t.open {
        return;
    }
    if let Some(model) = m.get("model").and_then(Value::as_str).filter(|m| !m.is_empty()) {
        t.model = Some(model.to_string());
    }
    t.failed = None;
    t.last_ts = ts;
    t.reply_words += blocks(m)
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .map(words)
        .sum::<usize>();
    if !calls.is_empty() && t.armed {
        t.armed = false;
        out.push(Found { ts, cwd: t.cwd.clone(), kind: Kind::AgentStart { session: t.session.clone() } });
    }
    t.tools += calls.len();
    // A reply cut off by the length limit still answers each tool call it made (with an error) and goes on.
    match m.get("stopReason").and_then(Value::as_str) {
        Some("toolUse") => {}
        Some("length") if !calls.is_empty() => {}
        Some("error") => t.failed = Some((id.unwrap_or_default().to_string(), ts)),
        _ => t.close(ts, out),
    }
}

fn result(t: &mut Turn, m: &Value, ts: f64, out: &mut Vec<Found>) {
    if t.open {
        t.last_ts = ts;
    }
    let Some(call) = m.get("toolCallId").and_then(Value::as_str).and_then(|id| t.calls.remove(id)) else { return };
    let failed = m.get("isError").and_then(Value::as_bool).unwrap_or(false);
    match call {
        Call::Edit(path) if !failed => {
            let details = m.get("details");
            let patch = details.and_then(|d| d.get("patch")).and_then(Value::as_str);
            let counts = match patch {
                Some(p) => Some(work::patch_counts(p)),
                None => details.and_then(|d| d.get("diff")).and_then(Value::as_str).map(|d| work::hunk_counts(d.lines())),
            };
            if t.open {
                t.file(&path);
            }
            if let Some(counts) = counts {
                work::edit(ts, t.cwd.as_deref(), &t.session, &path, counts, out);
            }
        }
        // A write doesn't say whether the file was there before, so its lines can't be counted.
        Call::Write(path) if !failed => {
            if t.open {
                t.file(&path);
            }
        }
        Call::Shell(cmd) => {
            let output = text(m);
            let output = if output == "(no output)" { "" } else { output.as_str() };
            work::commits(ts, t.cwd.as_deref(), &t.session, &cmd, output, out);
        }
        _ => {}
    }
}

fn feed(t: &mut Turn, e: &Value, out: &mut Vec<Found>) {
    let ty = e.get("type").and_then(Value::as_str).unwrap_or("");
    if ty == "session" {
        t.header(e);
        return;
    }
    let Some(ts) = timestamp(e) else { return };
    if ts < t.floor {
        return;
    }
    t.floor = ts;
    match ty {
        "model_change" => {
            if let Some(m) = e.get("modelId").and_then(Value::as_str).filter(|m| !m.is_empty()) {
                t.model = Some(m.to_string());
            }
        }
        "context_edit" => {
            let target = e.get("targetId").and_then(Value::as_str);
            if e.get("replacement").is_some_and(Value::is_null) && t.failed.as_ref().is_some_and(|(id, _)| Some(id.as_str()) == target) {
                t.failed = None;
            }
        }
        "message" => {
            let Some(m) = e.get("message") else { return };
            match m.get("role").and_then(Value::as_str) {
                Some("user") => prompt(t, m, ts, out),
                Some("assistant") => reply(t, e.get("id").and_then(Value::as_str), m, ts, out),
                Some("toolResult") => result(t, m, ts, out),
                _ => {}
            }
        }
        _ => {}
    }
}

pub struct Pi {
    tail: Tail,
    turns: HashMap<PathBuf, Turn>,
}

impl Pi {
    pub fn new(roots: Vec<PathBuf>) -> Pi {
        Pi { tail: Tail::new(roots, "jsonl", |_| false), turns: HashMap::new() }
    }
}

impl Source for Pi {
    fn health(&mut self) -> super::Health {
        self.tail.health()
    }

    fn name(&self) -> &'static str {
        "pi"
    }

    fn poll(&mut self) -> Vec<Found> {
        let mut out = Vec::new();
        let turns = &mut self.turns;
        self.tail.poll(|path, line| {
            let t = turns.entry(path.to_path_buf()).or_default();
            if !t.known {
                t.head(path);
            }
            if t.foreign {
                return;
            }
            if let Ok(e) = serde_json::from_str::<Value>(line) {
                feed(t, &e, &mut out);
            }
        });
        let now = snapshot::now();
        for t in self.turns.values_mut() {
            t.settle(now, &mut out);
        }
        self.turns.retain(|p, _| p.exists());
        out.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        out
    }

    fn state(&self) -> Value {
        self.tail.state()
    }

    fn restore(&mut self, state: &Value) {
        self.tail.restore(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lines: &[&str]) -> Vec<Found> {
        let mut t = Turn::default();
        let mut out = Vec::new();
        for l in lines {
            feed(&mut t, &serde_json::from_str(l).unwrap(), &mut out);
        }
        out
    }

    fn names(out: &[Found]) -> Vec<&'static str> {
        out.iter().map(|f| f.kind.name()).collect()
    }

    fn summary(out: &[Found]) -> Vec<String> {
        out.iter()
            .filter_map(|f| match &f.kind {
                Kind::AgentEdit { path, added, removed, session } => Some(format!("{session} {path} +{added} -{removed}")),
                Kind::AgentCommit { sha, .. } => Some(format!("commit {}", sha.as_deref().unwrap_or("-"))),
                _ => None,
            })
            .collect()
    }

    // What Pi 1.6.0 writes (session format 3) for a typed prompt that edits a file, commits and answers; built from its
    // source (session-manager.ts, agent-session.ts, tools/edit.ts, tools/bash.ts).
    const HEADER: &str = r#"{"type":"session","version":3,"id":"019a9b2c-4d5e-7f60-8a1b-2c3d4e5f6a7b","timestamp":"2026-11-24T10:15:30.123Z","cwd":"/home/u/proj"}"#;
    const MODEL: &str = r#"{"type":"model_change","id":"a1b2c3d4","parentId":null,"timestamp":"2026-11-24T10:15:30.131Z","provider":"local","modelId":"fake-model"}"#;
    const THINKING: &str = r#"{"type":"thinking_level_change","id":"b2c3d4e5","parentId":"a1b2c3d4","timestamp":"2026-11-24T10:15:30.132Z","thinkingLevel":"off"}"#;
    const SYSTEM: &str = r#"{"type":"message","id":"c3d4e5f6","parentId":"b2c3d4e5","timestamp":"2026-11-24T10:15:41.002Z","message":{"role":"system","content":"","sections":{"preamble":"You are a coding assistant running inside pi.","cwd":"<cwd>\n/home/u/proj\n</cwd>"},"timestamp":1795515341001}}"#;
    const USER: &str = r#"{"type":"message","id":"d4e5f6a7","parentId":"c3d4e5f6","timestamp":"2026-11-24T10:15:41.003Z","message":{"role":"user","content":[{"type":"text","text":"add a --verbose flag to main.py and commit it"}],"timestamp":1795515341000}}"#;
    const EDIT: &str = r#"{"type":"message","id":"e5f6a7b8","parentId":"d4e5f6a7","timestamp":"2026-11-24T10:15:48.751Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"private plan here"},{"type":"text","text":"I'll add the flag to main.py."},{"type":"toolCall","id":"call_01","name":"edit","arguments":{"path":"main.py","edits":[{"oldText":"    parser = argparse.ArgumentParser()","newText":"    parser = argparse.ArgumentParser()\n    parser.add_argument(\"--verbose\", action=\"store_true\")"}]}}],"api":"openai-completions","provider":"local","model":"fake-model","usage":{"input":812,"output":96,"cacheRead":0,"cacheWrite":0,"totalTokens":908,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"toolUse","thinkingLevel":"off","timestamp":1795515341100,"durationMs":7650}}"#;
    const EDITED: &str = r#"{"type":"message","id":"f6a7b8c9","parentId":"e5f6a7b8","timestamp":"2026-11-24T10:15:48.801Z","message":{"role":"toolResult","toolCallId":"call_01","toolName":"edit","content":[{"type":"text","text":"Successfully replaced 1 block(s) in main.py."}],"details":{"diff":" 1 import argparse\n 2 \n 3 def main():\n 4     parser = argparse.ArgumentParser()\n+5     parser.add_argument(\"--verbose\", action=\"store_true\")\n 5     args = parser.parse_args()\n 6     print(\"hello\")","patch":"--- main.py\n+++ main.py\n@@ -1,6 +1,7 @@\n import argparse\n \n def main():\n     parser = argparse.ArgumentParser()\n+    parser.add_argument(\"--verbose\", action=\"store_true\")\n     args = parser.parse_args()\n     print(\"hello\")\n","firstChangedLine":5},"isError":false,"timestamp":1795515348800,"durationMs":12}}"#;
    const COMMIT: &str = r#"{"type":"message","id":"a7b8c9d0","parentId":"f6a7b8c9","timestamp":"2026-11-24T10:15:53.951Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_02","name":"bash","arguments":{"command":"git commit -am \"add verbose flag\""}}],"api":"openai-completions","provider":"local","model":"fake-model","stopReason":"toolUse","timestamp":1795515348850,"durationMs":5100}}"#;
    const COMMITTED: &str = r#"{"type":"message","id":"b8c9d0e1","parentId":"a7b8c9d0","timestamp":"2026-11-24T10:15:54.601Z","message":{"role":"toolResult","toolCallId":"call_02","toolName":"bash","content":[{"type":"text","text":"[main 3f2a1bc] add verbose flag\n 1 file changed, 1 insertion(+)\n"}],"isError":false,"timestamp":1795515354600,"durationMs":640}}"#;
    const DONE: &str = r#"{"type":"message","id":"c9d0e1f2","parentId":"b8c9d0e1","timestamp":"2026-11-24T10:15:56.951Z","message":{"role":"assistant","content":[{"type":"text","text":"Added the --verbose flag to main.py and committed it as 3f2a1bc."}],"api":"openai-completions","provider":"local","model":"fake-model","stopReason":"stop","timestamp":1795515354650,"durationMs":2300}}"#;

    fn session() -> Vec<&'static str> {
        vec![HEADER, MODEL, THINKING, SYSTEM, USER, EDIT, EDITED, COMMIT, COMMITTED, DONE]
    }

    #[test]
    fn a_turn_becomes_prompt_start_edit_commit_done() {
        let out = run(&session());
        assert_eq!(names(&out), ["prompt", "agent_start", "agent_edit", "agent_commit", "reply_done"]);
        assert!(out.iter().all(|f| f.cwd.as_deref() == Some(Path::new("/home/u/proj"))));
        assert_eq!(summary(&out), ["019a9b2c-4d5e-7f60-8a1b-2c3d4e5f6a7b main.py +1 -0", "commit 3f2a1bc"]);
        match &out[0].kind {
            Kind::Prompt { words, model, version, .. } => assert_eq!((*words, model.as_deref(), version), (9, Some("fake-model"), &None)),
            k => panic!("{k:?}"),
        }
        match &out[4].kind {
            Kind::ReplyDone { reply_words, tools, files_changed, model, .. } => {
                assert_eq!((*reply_words, *tools, *files_changed, model.as_deref()), (17, 2, 1, Some("fake-model")));
            }
            k => panic!("{k:?}"),
        }
        assert!((out[4].ts - out[0].ts - 15.948).abs() < 1e-6);
    }

    #[test]
    fn skills_count_what_was_typed_and_images_alone_are_prompts() {
        let skill = USER.replace(
            "add a --verbose flag to main.py and commit it",
            r#"<skill name=\"review\" location=\"/home/u/.pi/skills/review/SKILL.md\">\nReferences are relative to /home/u/.pi/skills/review.\n\nRead every changed file and list all the problems you find in them.\n</skill>\n\nthe parser"#,
        );
        match &run(&[HEADER, &skill])[0].kind {
            Kind::Prompt { words, .. } => assert_eq!(*words, 3),
            k => panic!("{k:?}"),
        }
        let image = USER.replace(r#"[{"type":"text","text":"add a --verbose flag to main.py and commit it"}]"#, r#"[{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"}]"#);
        assert_eq!(names(&run(&[HEADER, &image])), ["prompt"]);
        let empty = USER.replace("add a --verbose flag to main.py and commit it", " ");
        assert!(run(&[HEADER, &empty]).is_empty());
    }

    #[test]
    fn forks_skip_the_copied_history() {
        let fork = r#"{"type":"session","version":3,"id":"019a9c00-0000-7000-8000-000000000001","timestamp":"2026-11-24T11:00:00.000Z","cwd":"/home/u/proj","parentSession":"/home/u/.pi/agent/sessions/--home-u-proj--/2026-11-24T10-15-30-123Z_019a9b2c-4d5e-7f60-8a1b-2c3d4e5f6a7b.jsonl"}"#;
        let next = r#"{"type":"message","id":"0e1f2a3b","parentId":"c9d0e1f2","timestamp":"2026-11-24T11:00:05.000Z","message":{"role":"user","content":[{"type":"text","text":"now add a test"}],"timestamp":1795518005000}}"#;
        let mut lines = vec![fork];
        lines.extend(&session()[1..]);
        lines.push(next);
        let out = run(&lines);
        assert_eq!(names(&out), ["prompt"]);
        match &out[0].kind {
            Kind::Prompt { words, session, .. } => assert_eq!((*words, session.as_str()), (4, "019a9c00-0000-7000-8000-000000000001")),
            k => panic!("{k:?}"),
        }
    }

    fn failed(id: &str, at: &str) -> String {
        format!(
            r#"{{"type":"message","id":"{id}","parentId":"d4e5f6a7","timestamp":"2026-11-24T10:15:{at}Z","message":{{"role":"assistant","content":[],"api":"openai-completions","provider":"local","model":"fake-model","stopReason":"error","errorMessage":"overloaded","timestamp":1795515341100}}}}"#
        )
    }

    #[test]
    fn a_retried_failure_keeps_the_turn_and_a_final_one_ends_it() {
        let hidden = r#"{"type":"context_edit","id":"e0e0e0e0","parentId":"ffff0001","timestamp":"2026-11-24T10:15:45.001Z","targetId":"ffff0001","replacement":null}"#;
        let out = run(&[HEADER, MODEL, USER, &failed("ffff0001", "45.000"), hidden, EDIT, EDITED, DONE]);
        assert_eq!(names(&out), ["prompt", "agent_start", "agent_edit", "reply_done"]);

        let mut t = Turn::default();
        let mut out = Vec::new();
        for l in [HEADER, USER, &failed("ffff0002", "45.000")] {
            feed(&mut t, &serde_json::from_str(l).unwrap(), &mut out);
        }
        let at = timestamp(&serde_json::json!({ "timestamp": "2026-11-24T10:15:45.000Z" })).unwrap();
        t.settle(at + 5.0, &mut out);
        assert_eq!(names(&out), ["prompt"]);
        t.settle(at + RETRY_WAIT_SECS, &mut out);
        assert_eq!(names(&out), ["prompt", "reply_done"]);
        assert_eq!(out[1].ts, at);

        let again = USER.replace("10:15:41.003", "10:15:50.000");
        assert_eq!(names(&run(&[HEADER, USER, &failed("ffff0003", "45.000"), &again])), ["prompt", "reply_done", "prompt"]);
    }

    #[test]
    fn aborted_and_length_limited_replies() {
        let aborted = DONE.replace(r#""stopReason":"stop""#, r#""stopReason":"aborted""#);
        assert_eq!(names(&run(&[HEADER, USER, &aborted])), ["prompt", "reply_done"]);
        let cut = EDIT.replace(r#""stopReason":"toolUse""#, r#""stopReason":"length""#);
        assert_eq!(names(&run(&[HEADER, USER, &cut])), ["prompt", "agent_start"]);
        let cut_text = DONE.replace(r#""stopReason":"stop""#, r#""stopReason":"length""#);
        assert_eq!(names(&run(&[HEADER, USER, &cut_text])), ["prompt", "reply_done"]);
    }

    fn call(n: u32, id: &str, name: &str, args: &str) -> String {
        format!(
            r#"{{"type":"message","id":"m{id}","parentId":"x","timestamp":"2026-11-24T10:16:{n:02}.000Z","message":{{"role":"assistant","content":[{{"type":"toolCall","id":"{id}","name":"{name}","arguments":{args}}}],"model":"fake-model","stopReason":"toolUse","timestamp":1795515350000}}}}"#
        )
    }

    fn done(n: u32, id: &str, error: bool, text: &str, details: &str) -> String {
        format!(
            r#"{{"type":"message","id":"r{id}","parentId":"m{id}","timestamp":"2026-11-24T10:16:{n:02}.500Z","message":{{"role":"toolResult","toolCallId":"{id}","toolName":"x","content":[{{"type":"text","text":{text}}}]{details},"isError":{error},"timestamp":1795515351000}}}}"#
        )
    }

    #[test]
    fn edits_writes_and_commits_need_a_successful_result() {
        let patch = r#","details":{"diff":"","patch":"--- a.py\n+++ a.py\n@@ -1,2 +1,2 @@\n-old\n+new\n keep\n"}"#;
        let diff_only = r#","details":{"diff":"-3 gone\n+3 here\n+4 more\n 5 same"}"#;
        let out = run(&[
            HEADER,
            USER,
            &call(1, "e1", "edit", r#"{"path":"@src/a.py","edits":[]}"#),
            &done(1, "e1", false, r#""Successfully replaced 1 block(s) in @src/a.py.""#, patch),
            &call(2, "e2", "edit", r#"{"path":"/abs/b.py","oldText":"a","newText":"b"}"#),
            &done(2, "e2", false, r#""ok""#, diff_only),
            &call(3, "e3", "edit", r#"{"path":"c.py","edits":[]}"#),
            &done(3, "e3", true, r#""Could not find the exact text in c.py.""#, r#","details":{}"#),
            &call(4, "w1", "write", r#"{"path":"new.py","content":"1\n2\n"}"#),
            &done(4, "w1", false, r#""Successfully wrote to new.py""#, ""),
            &call(5, "r1", "read", r#"{"path":"a.py"}"#),
            &done(5, "r1", false, r#""old""#, patch),
            &call(6, "s1", "bash", r#"{"command":"git status"}"#),
            &done(6, "s1", false, r#""[main abc1234] stray""#, ""),
            &call(7, "s2", "bash", r#"{"command":"git commit -m x"}"#),
            &done(7, "s2", true, r#""On branch main\nnothing to commit, working tree clean\n\nCommand exited with code 1""#, ""),
            &call(8, "s3", "bash", r#"{"command":"git commit -qm x"}"#),
            &done(8, "s3", false, r#""(no output)""#, ""),
            &call(9, "s4", "powershell", r#"{"command":"git commit -m y; npm test"}"#),
            &done(9, "s4", true, r#""[main 1234abcd] y\n 1 file changed\nnpm ERR! test failed\n\nCommand exited with code 1""#, ""),
            &done(9, "zz", false, r#""x""#, patch),
            &DONE.replace("10:15:56.951", "10:16:30.000"),
        ]);
        let s = "019a9b2c-4d5e-7f60-8a1b-2c3d4e5f6a7b";
        assert_eq!(summary(&out), [format!("{s} src/a.py +1 -1"), format!("{s} /abs/b.py +2 -1"), "commit -".into(), "commit 1234abcd".into()]);
        match &out.last().unwrap().kind {
            Kind::ReplyDone { tools, files_changed, .. } => assert_eq!((*tools, *files_changed), (9, 3)),
            k => panic!("{k:?}"),
        }
    }

    #[test]
    fn tool_paths_follow_pi() {
        assert_eq!(tool_path("@src/a.py").as_deref(), Some("src/a.py"));
        assert_eq!(tool_path("file:///w/p/a.py").as_deref(), Some("/w/p/a.py"));
        let home = dirs::home_dir().unwrap();
        assert_eq!(tool_path("~/x/a.py"), Some(home.join("x/a.py").to_string_lossy().into_owned()));
        assert_eq!(tool_path(""), None);
        if cfg!(windows) {
            assert_eq!(tool_path("/c/Users/me/a.py").as_deref(), Some("C:\\Users\\me\\a.py"));
            assert_eq!(tool_path("/mnt/d/p").as_deref(), Some("D:\\p"));
        } else {
            assert_eq!(tool_path("/c/Users/me/a.py").as_deref(), Some("/c/Users/me/a.py"));
        }
    }

    #[test]
    fn tail_finds_the_header_after_a_restart_and_skips_other_files() {
        let dir = std::env::temp_dir().join(format!("kairos-pi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let f = dir.join("--home-u-proj--/2026-11-24T10-15-30-123Z_019a9b2c-4d5e-7f60-8a1b-2c3d4e5f6a7b.jsonl");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        let mut body = format!("{HEADER}\n{MODEL}\n{USER}\n");
        std::fs::write(&f, &body).unwrap();
        let other = dir.join("--home-u-proj--/notes.jsonl");
        std::fs::write(&other, "{\"type\":\"note\"}\n").unwrap();
        let mut src = Pi::new(vec![dir.clone()]);
        assert!(src.poll().is_empty());

        let mut again = Pi::new(vec![dir.clone()]);
        again.restore(&src.state());
        body.push_str(&format!("{EDIT}\n{EDITED}\n{}", &DONE[..40]));
        std::fs::write(&f, &body).unwrap();
        std::fs::write(&other, format!("{{\"type\":\"note\"}}\n{USER}\n")).unwrap();
        let out = again.poll();
        assert_eq!(names(&out), ["agent_edit"]);
        assert_eq!(out[0].cwd.as_deref(), Some(Path::new("/home/u/proj")));
        body.truncate(body.len() - 40);
        body.push_str(&format!("{DONE}\n"));
        std::fs::write(&f, &body).unwrap();
        assert!(again.poll().is_empty());

        let next = USER.replace("10:15:41.003", "10:20:00.000");
        body.push_str(&format!("{next}\n"));
        std::fs::write(&f, &body).unwrap();
        let out = again.poll();
        assert_eq!(names(&out), ["prompt"]);
        match &out[0].kind {
            Kind::Prompt { session, model, .. } => assert_eq!((session.as_str(), model.as_deref()), ("019a9b2c-4d5e-7f60-8a1b-2c3d4e5f6a7b", None)),
            k => panic!("{k:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A real session file: Pi 1.6.0 run twice (`pi -p`, then `pi -c -p`) against a fake model; only the system prompt is cut.
    #[test]
    fn a_real_pi_1_6_session() {
        let lines = [
            r#"{"type":"session","version":3,"id":"01a20dee-c4a7-773a-afd2-9d894fe0596d","timestamp":"2026-11-24T14:21:21.449Z","cwd":"/home/user/pproj"}"#,
            r#"{"type":"model_change","id":"61a70425","parentId":null,"timestamp":"2026-11-24T14:21:21.520Z","provider":"local","modelId":"fake-model"}"#,
            r#"{"type":"thinking_level_change","id":"34de035a","parentId":"61a70425","timestamp":"2026-11-24T14:21:21.520Z","thinkingLevel":"off"}"#,
            r#"{"type":"message","id":"b53f8203","parentId":"34de035a","timestamp":"2026-11-24T14:21:21.533Z","message":{"role":"system","content":"","sections":{"preamble":"(cut)"},"timestamp":1795530081531,"toolsAdded":[]}}"#,
            r#"{"type":"message","id":"0cdc125c","parentId":"b53f8203","timestamp":"2026-11-24T14:21:21.533Z","message":{"role":"user","content":[{"type":"text","text":"add a verbose line and commit"}],"timestamp":1795530081530}}"#,
            r#"{"type":"message","id":"3c84dbac","parentId":"0cdc125c","timestamp":"2026-11-24T14:21:22.626Z","message":{"role":"assistant","content":[{"type":"text","text":"Editing."},{"type":"toolCall","id":"call_0709b91d","name":"edit","arguments":{"path":"main.py","edits":[{"oldText":"print(\"hello\")","newText":"print(\"hello\")\nprint(\"verbose\")"}]}}],"api":"openai-completions","provider":"local","model":"fake-model","usage":{"input":100,"output":20,"cacheRead":0,"cacheWrite":0,"reasoning":0,"totalTokens":120,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0,"totalFromProvider":false}},"stopReason":"toolUse","timestamp":1795530081564,"responseId":"chatcmpl-86395f63bdf6","rawStopReason":"tool_calls","durationMs":1063,"thinkingLevel":"off"}}"#,
            r#"{"type":"message","id":"48c3f7c1","parentId":"3c84dbac","timestamp":"2026-11-24T14:21:22.635Z","message":{"role":"toolResult","toolCallId":"call_0709b91d","toolName":"edit","content":[{"type":"text","text":"Successfully replaced 1 block(s) in main.py."}],"details":{"diff":" 1 print(\"hello\")\n+2 print(\"verbose\")","patch":"--- main.py\n+++ main.py\n@@ -1,1 +1,2 @@\n print(\"hello\")\n+print(\"verbose\")\n","firstChangedLine":2},"isError":false,"durationMs":5,"timestamp":1795530082635}}"#,
            r#"{"type":"message","id":"d8624e0b","parentId":"48c3f7c1","timestamp":"2026-11-24T14:21:23.645Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_4eae96c6","name":"write","arguments":{"path":"notes.txt","content":"one\ntwo\n"}}],"api":"openai-completions","provider":"local","model":"fake-model","usage":{"input":100,"output":20,"cacheRead":0,"cacheWrite":0,"reasoning":0,"totalTokens":120,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0,"totalFromProvider":false}},"stopReason":"toolUse","timestamp":1795530082637,"responseId":"chatcmpl-ae3feb8d4d81","rawStopReason":"tool_calls","durationMs":1007,"thinkingLevel":"off"}}"#,
            r#"{"type":"message","id":"95746902","parentId":"d8624e0b","timestamp":"2026-11-24T14:21:23.647Z","message":{"role":"toolResult","toolCallId":"call_4eae96c6","toolName":"write","content":[{"type":"text","text":"Successfully wrote to notes.txt"}],"isError":false,"durationMs":1,"timestamp":1795530083647}}"#,
            r#"{"type":"message","id":"9f06d58b","parentId":"95746902","timestamp":"2026-11-24T14:21:24.654Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_aaf9764c","name":"bash","arguments":{"command":"git add -A && git commit -m \"add verbose\""}}],"api":"openai-completions","provider":"local","model":"fake-model","usage":{"input":100,"output":20,"cacheRead":0,"cacheWrite":0,"reasoning":0,"totalTokens":120,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0,"totalFromProvider":false}},"stopReason":"toolUse","timestamp":1795530083648,"responseId":"chatcmpl-771b709327e9","rawStopReason":"tool_calls","durationMs":1006,"thinkingLevel":"off"}}"#,
            r#"{"type":"message","id":"2d4b5f66","parentId":"9f06d58b","timestamp":"2026-11-24T14:21:24.671Z","message":{"role":"toolResult","toolCallId":"call_aaf9764c","toolName":"bash","content":[{"type":"text","text":"[main 6894a79] add verbose\n 2 files changed, 3 insertions(+)\n create mode 100644 notes.txt\n"}],"isError":false,"durationMs":16,"timestamp":1795530084671}}"#,
            r#"{"type":"message","id":"55d06ec6","parentId":"2d4b5f66","timestamp":"2026-11-24T14:21:25.683Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_8571bc32","name":"bash","arguments":{"command":"false"}}],"api":"openai-completions","provider":"local","model":"fake-model","usage":{"input":100,"output":20,"cacheRead":0,"cacheWrite":0,"reasoning":0,"totalTokens":120,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0,"totalFromProvider":false}},"stopReason":"toolUse","timestamp":1795530084673,"responseId":"chatcmpl-2d106012b985","rawStopReason":"tool_calls","durationMs":1009,"thinkingLevel":"off"}}"#,
            r#"{"type":"message","id":"fef7c906","parentId":"55d06ec6","timestamp":"2026-11-24T14:21:25.689Z","message":{"role":"toolResult","toolCallId":"call_8571bc32","toolName":"bash","content":[{"type":"text","text":"(no output)\n\nCommand exited with code 1"}],"isError":true,"durationMs":6,"timestamp":1795530085689}}"#,
            r#"{"type":"message","id":"d7974d98","parentId":"fef7c906","timestamp":"2026-11-24T14:21:26.697Z","message":{"role":"assistant","content":[{"type":"text","text":"All done here."}],"api":"openai-completions","provider":"local","model":"fake-model","usage":{"input":100,"output":20,"cacheRead":0,"cacheWrite":0,"reasoning":0,"totalTokens":120,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0,"totalFromProvider":false}},"stopReason":"stop","timestamp":1795530085690,"responseId":"chatcmpl-36b265a21ba2","rawStopReason":"stop","durationMs":1006,"thinkingLevel":"off"}}"#,
            r#"{"type":"message","id":"c3a178bc","parentId":"d7974d98","timestamp":"2026-11-24T14:21:27.216Z","message":{"role":"user","content":[{"type":"text","text":"now say hi"}],"timestamp":1795530087210}}"#,
            r#"{"type":"message","id":"f6684aaf","parentId":"c3a178bc","timestamp":"2026-11-24T14:21:28.283Z","message":{"role":"assistant","content":[{"type":"text","text":"Done."}],"api":"openai-completions","provider":"local","model":"fake-model","usage":{"input":100,"output":20,"cacheRead":0,"cacheWrite":0,"reasoning":0,"totalTokens":120,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0,"totalFromProvider":false}},"stopReason":"stop","timestamp":1795530087242,"responseId":"chatcmpl-75d58faac397","rawStopReason":"stop","durationMs":1041,"thinkingLevel":"off"}}"#,
        ];
        let out = run(&lines);
        assert_eq!(names(&out), ["prompt", "agent_start", "agent_edit", "agent_commit", "reply_done", "prompt", "reply_done"]);
        assert_eq!(summary(&out), ["01a20dee-c4a7-773a-afd2-9d894fe0596d main.py +1 -0", "commit 6894a79"]);
        assert!(out.iter().all(|f| f.cwd.as_deref() == Some(Path::new("/home/user/pproj"))));
        match &out[4].kind {
            Kind::ReplyDone { reply_words, tools, files_changed, .. } => assert_eq!((*reply_words, *tools, *files_changed), (4, 4, 2)),
            k => panic!("{k:?}"),
        }
        match &out[5].kind {
            Kind::Prompt { words, .. } => assert_eq!(*words, 3),
            k => panic!("{k:?}"),
        }
    }
}
