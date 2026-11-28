use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{work, words, Found, Source, Tail};
use crate::event::Kind;
use crate::parse::timestamp;
use crate::{prompt_shape, prompt_stats};

const EDIT_TOOLS: [&str; 5] = ["edit", "create", "write", "str_replace_editor", "apply_patch"];
const SHELL_TOOLS: [&str; 3] = ["bash", "powershell", "shell"];

pub fn default_roots() -> Vec<PathBuf> {
    std::env::var_os("COPILOT_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".copilot")))
        .map(|h| h.join("session-state"))
        .into_iter()
        .collect()
}

// session-state/<id>/events.jsonl, or session-state/<id>.jsonl from older versions.
fn list(root: &Path, files: &mut Vec<PathBuf>) {
    for e in fs::read_dir(root).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "jsonl") {
            files.push(p);
        } else if p.join("events.jsonl").is_file() {
            files.push(p.join("events.jsonl"));
        }
    }
}

fn session_of(path: &Path) -> String {
    let p = if path.file_name().is_some_and(|n| n == "events.jsonl") { path.parent() } else { Some(path) };
    p.and_then(Path::file_stem).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

// VS Code's chat runs on the same runtime and keeps its sessions here too. The session's workspace.yaml names the
// client that last opened it (the CLI says github/cli).
fn in_vscode(events: &Path) -> bool {
    let Some(dir) = events.parent().filter(|_| events.file_name().is_some_and(|n| n == "events.jsonl")) else { return false };
    fs::read_to_string(dir.join("workspace.yaml")).is_ok_and(|y| {
        y.lines().any(|l| l.strip_prefix("last_client_name:").is_some_and(|v| v.trim().trim_matches(['"', '\'']) == "vscode-agent-host"))
    })
}

fn is_plan(p: &str) -> bool {
    let p = p.replace('\\', "/");
    p.contains(".copilot/session-state/") && p.ends_with("/plan.md")
}

#[derive(Default)]
struct Sess {
    known: bool,
    path: PathBuf,
    vscode: bool,
    session: String,
    cwd: Option<PathBuf>,
    model: Option<String>,
    version: Option<String>,
    armed: bool,
    open: bool,
    /// A chunk of the response being read asked for tools.
    asked: bool,
    /// It marks where each model call starts and ends.
    marked: bool,
    last_ts: f64,
    reply_words: usize,
    tools: usize,
    files: BTreeSet<String>,
    commands: HashMap<String, String>,
}

impl Sess {
    // Tail skips history on first run, so read session.start from the head of the file.
    fn head(&mut self, path: &Path) {
        self.known = true;
        self.path = path.to_path_buf();
        self.vscode = in_vscode(path);
        self.session = session_of(path);
        let mut line = String::new();
        let Ok(f) = File::open(path) else { return };
        if BufReader::new(f).read_line(&mut line).is_err() {
            return;
        }
        if let Ok(e) = serde_json::from_str::<Value>(&line) {
            self.context(&e);
        }
    }

    fn context(&mut self, e: &Value) {
        let Some(d) = e.get("data") else { return };
        if let Some(id) = d.get("sessionId").and_then(Value::as_str) {
            self.session = id.to_string();
        }
        if let Some(c) = d.pointer("/context/cwd").or_else(|| d.get("cwd")).and_then(Value::as_str) {
            self.cwd = Some(PathBuf::from(c));
        }
        if let Some(v) = d.get("copilotVersion").and_then(Value::as_str) {
            self.version = Some(v.to_string());
        }
    }

    fn close(&mut self, ts: f64, out: &mut Vec<Found>) {
        if !self.open {
            return;
        }
        self.open = false;
        self.armed = false;
        self.asked = false;
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
}

// The completion event's telemetry counts the lines a tool changed and names its files; counts are used only when the call touched exactly one file.
fn agent_work(s: &mut Sess, ts: f64, ty: &str, d: &Value, out: &mut Vec<Found>) {
    let id = d.get("toolCallId").and_then(Value::as_str);
    match (ty, id) {
        ("tool.execution_start", Some(id)) => {
            let name = d.get("toolName").and_then(Value::as_str).unwrap_or("");
            if let (true, Some(cmd)) = (SHELL_TOOLS.contains(&name), d.pointer("/arguments/command").and_then(Value::as_str).filter(|c| c.contains("commit"))) {
                work::remember(&mut s.commands, id.to_string(), cmd.to_string());
            }
        }
        ("tool.execution_complete", Some(id)) => {
            if let Some(cmd) = s.commands.remove(id) {
                let output = ["/result/content", "/result/detailedContent"].iter().find_map(|k| d.pointer(k).and_then(Value::as_str)).unwrap_or("");
                work::commits(ts, s.cwd.as_deref(), &s.session, &cmd, output, out);
            }
            if d.get("success").and_then(Value::as_bool) != Some(true) {
                return;
            }
            let t = d.get("toolTelemetry");
            let count = |k: &str| t.and_then(|t| t.pointer(&format!("/metrics/{k}"))).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok());
            let props = t.and_then(|t| t.get("restrictedProperties"));
            let mut paths: Vec<String> = Vec::new();
            for k in ["filePaths", "addedPaths", "deletedPaths"] {
                let found: Vec<String> =
                    props.and_then(|p| p.get(k)).and_then(Value::as_str).and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default();
                for p in found.into_iter().filter(|p| !is_plan(p)) {
                    if !paths.contains(&p) {
                        paths.push(p);
                    }
                }
            }
            if let ([path], Some(added), Some(removed)) = (paths.as_slice(), count("linesAdded"), count("linesRemoved")) {
                work::edit(ts, s.cwd.as_deref(), &s.session, path, (added, removed), out);
            }
        }
        _ => {}
    }
}

fn feed(s: &mut Sess, e: &Value, out: &mut Vec<Found>) {
    let Some(ts) = timestamp(e) else { return };
    let ty = e.get("type").and_then(Value::as_str).unwrap_or("");
    let null = Value::Null;
    let d = e.get("data").unwrap_or(&null);
    agent_work(s, ts, ty, d, out);
    // Subagent events carry the subagent's id on the envelope (older builds: parentToolCallId).
    if e.get("agentId").is_some_and(|a| !a.is_null()) || d.get("parentToolCallId").is_some_and(|a| !a.is_null()) {
        return;
    }
    if let Some(m) = d.get("newModel").or_else(|| d.get("model")).and_then(Value::as_str).filter(|m| !m.is_empty()) {
        s.model = Some(m.to_string());
    }
    match ty {
        "session.start" | "session.resume" | "session.context_changed" => s.context(e),
        "user.message" => {
            let text = d.get("content").or_else(|| d.get("message")).and_then(Value::as_str).unwrap_or("");
            if d.get("agentMode").and_then(Value::as_str) == Some("shell") {
                return;
            }
            let (words, pasted) = prompt_stats(text);
            if words == 0 {
                return;
            }
            // A session can be picked up again in the other one.
            s.vscode = in_vscode(&s.path);
            let last = s.last_ts;
            s.close(last.min(ts), out);
            s.armed = true;
            s.open = true;
            s.last_ts = ts;
            s.reply_words = 0;
            s.tools = 0;
            s.files.clear();
            out.push(Found {
                ts,
                cwd: s.cwd.clone(),
                kind: Kind::Prompt {
                    words,
                    pasted,
                    model: s.model.clone(),
                    session: s.session.clone(),
                    shape: Some(prompt_shape(text)),
                    version: s.version.clone(),
                    via: None,
                    permissions: None,
                },
            });
        }
        _ if !s.open => {}
        "assistant.message" => {
            s.last_ts = ts;
            s.reply_words += d.get("content").and_then(Value::as_str).map_or(0, words);
            s.asked |= d.get("toolRequests").and_then(Value::as_array).is_some_and(|t| !t.is_empty());
            // A reasoning model's response comes split into one message per reasoning boundary, and the turn only
            // ends after a response that asked for no tools. Versions before chunkIndex only say so at the call's end.
            if s.marked {
                return;
            }
            let last_chunk = match (d.get("chunkIndex").and_then(Value::as_u64), d.get("chunkCount").and_then(Value::as_u64)) {
                (Some(i), Some(n)) => i + 1 >= n,
                _ => true,
            };
            if last_chunk && !std::mem::take(&mut s.asked) {
                s.close(ts, out);
            }
        }
        "tool.execution_start" => {
            s.last_ts = ts;
            if s.armed {
                s.armed = false;
                out.push(Found { ts, cwd: s.cwd.clone(), kind: Kind::AgentStart { session: s.session.clone() } });
            }
            s.tools += 1;
            let name = d.get("toolName").and_then(Value::as_str).unwrap_or("");
            if EDIT_TOOLS.contains(&name) {
                if let Some(p) = d.pointer("/arguments/path").and_then(Value::as_str).filter(|p| !is_plan(p)) {
                    s.files.insert(p.to_string());
                }
            }
        }
        "tool.execution_complete" => {
            s.last_ts = ts;
            if d.get("success").and_then(Value::as_bool) == Some(true) {
                let props = d.pointer("/toolTelemetry/restrictedProperties");
                for k in ["filePaths", "addedPaths", "deletedPaths"] {
                    let paths: Vec<String> = props
                        .and_then(|p| p.get(k))
                        .and_then(Value::as_str)
                        .and_then(|j| serde_json::from_str(j).ok())
                        .unwrap_or_default();
                    s.files.extend(paths.into_iter().filter(|p| !is_plan(p)));
                }
            }
        }
        "assistant.turn_start" => s.marked = true,
        "assistant.turn_end" => {
            s.last_ts = ts;
            if s.marked && !std::mem::take(&mut s.asked) {
                s.close(ts, out);
            }
        }
        "abort" | "session.task_complete" => s.close(ts, out),
        "session.shutdown" | "session.error" => {
            let last = s.last_ts;
            s.close(last, out);
        }
        _ => {}
    }
}

pub struct CopilotCli {
    tail: Tail,
    sessions: HashMap<PathBuf, Sess>,
}

impl CopilotCli {
    pub fn new(roots: Vec<PathBuf>) -> CopilotCli {
        CopilotCli { tail: Tail::with(roots, list), sessions: HashMap::new() }
    }
}

impl Source for CopilotCli {
    fn health(&mut self) -> super::Health {
        self.tail.health()
    }

    fn name(&self) -> &'static str {
        "copilot_cli"
    }

    fn harness(&self) -> &'static str {
        "copilot-cli"
    }

    fn harness_of(&self, found: &Found) -> &'static str {
        let id = found.kind.session();
        if self.sessions.values().any(|s| s.vscode && Some(s.session.as_str()) == id) {
            "copilot"
        } else {
            self.harness()
        }
    }

    fn poll(&mut self) -> Vec<Found> {
        let mut out = Vec::new();
        let sessions = &mut self.sessions;
        self.tail.poll(|path, line| {
            if let Ok(e) = serde_json::from_str::<Value>(line) {
                let s = sessions.entry(path.to_path_buf()).or_default();
                if !s.known {
                    s.head(path);
                }
                feed(s, &e, &mut out);
            }
        });
        self.sessions.retain(|p, _| p.exists());
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
        let mut s = Sess { session: "cli-1".into(), ..Sess::default() };
        let mut out = Vec::new();
        for l in lines {
            feed(&mut s, &serde_json::from_str(l).unwrap(), &mut out);
        }
        out
    }

    fn names(out: &[Found]) -> Vec<&'static str> {
        out.iter().map(|f| f.kind.name()).collect()
    }

    const START: &str = r#"{"type":"session.start","timestamp":"2026-11-19T09:59:00Z","data":{"sessionId":"cli-1","copilotVersion":"1.0.62","context":{"cwd":"/w/p","gitRoot":"/w/p"}}}"#;
    const MODEL: &str = r#"{"type":"session.model_change","timestamp":"2026-11-19T09:59:30Z","data":{"newModel":"gpt-x"}}"#;
    const USER: &str = r#"{"type":"user.message","timestamp":"2026-11-19T10:00:00Z","data":{"content":"please fix the failing parser test now","agentMode":"interactive"}}"#;
    const ASK: &str = r#"{"type":"assistant.message","timestamp":"2026-11-19T10:00:03Z","data":{"messageId":"m1","content":"Looking at it.","toolRequests":[{"toolCallId":"t1","name":"edit"}]}}"#;
    const TOOL: &str = r#"{"type":"tool.execution_start","timestamp":"2026-11-19T10:00:04Z","data":{"toolCallId":"t1","toolName":"edit","arguments":{"path":"/w/p/a.rs"}}}"#;
    const DONE_TOOL: &str = r#"{"type":"tool.execution_complete","timestamp":"2026-11-19T10:00:05Z","data":{"toolCallId":"t1","success":true,"toolTelemetry":{"restrictedProperties":{"filePaths":"[\"/w/p/a.rs\",\"/home/u/.copilot/session-state/x/plan.md\"]"}}}}"#;
    const SUB: &str = r#"{"type":"tool.execution_start","timestamp":"2026-11-19T10:00:06Z","agentId":"sub-1","data":{"toolCallId":"t2","toolName":"create","arguments":{"path":"/w/p/sub.rs"}}}"#;
    const SUB_SAY: &str = r#"{"type":"assistant.message","timestamp":"2026-11-19T10:00:07Z","data":{"messageId":"m2","content":"subagent says done","parentToolCallId":"t9"}}"#;
    const FINAL: &str = r#"{"type":"assistant.message","timestamp":"2026-11-19T10:01:00Z","data":{"messageId":"m3","content":"Fixed it in a.rs.","toolRequests":[]}}"#;

    #[test]
    fn a_turn_becomes_prompt_start_done_and_subagents_are_ignored() {
        let out = run(&[START, MODEL, USER, ASK, TOOL, DONE_TOOL, SUB, SUB_SAY, FINAL]);
        assert_eq!(names(&out), ["prompt", "agent_start", "reply_done"]);
        assert_eq!(out[0].cwd.as_deref(), Some(Path::new("/w/p")));
        assert!(matches!(&out[0].kind, Kind::Prompt { words: 7, model: Some(m), version: Some(v), .. } if m == "gpt-x" && v == "1.0.62"));
        match &out[2].kind {
            Kind::ReplyDone { reply_words, tools, files_changed, .. } => {
                assert_eq!((*reply_words, *tools, *files_changed), (7, 1, 1));
            }
            k => panic!("{k:?}"),
        }
        assert_eq!(out[2].ts - out[0].ts, 60.0);
    }

    #[test]
    fn a_response_split_at_reasoning_ends_the_turn_only_after_its_last_chunk() {
        let think = r#"{"type":"assistant.message","timestamp":"2026-11-19T10:00:02Z","data":{"messageId":"m0","content":"","reasoningText":"Let me look.","chunkIndex":0,"chunkCount":2}}"#;
        let ask = ASK.replace(r#""messageId":"m1","#, r#""messageId":"m1","chunkIndex":1,"chunkCount":2,"#);
        let out = run(&[START, USER, think, &ask, TOOL, DONE_TOOL, FINAL]);
        assert_eq!(names(&out), ["prompt", "agent_start", "reply_done"]);
        assert!(matches!(out[2].kind, Kind::ReplyDone { reply_words: 7, tools: 1, .. }));
        assert_eq!(out[2].ts - out[0].ts, 60.0);

        // Tools asked for in an early chunk still keep the turn open.
        let ask = ASK.replace(r#""messageId":"m1","#, r#""messageId":"m1","chunkIndex":0,"chunkCount":2,"#);
        let say = r#"{"type":"assistant.message","timestamp":"2026-11-19T10:00:03Z","data":{"messageId":"m1b","content":"Editing now.","chunkIndex":1,"chunkCount":2}}"#;
        let out = run(&[START, USER, &ask, say, TOOL, DONE_TOOL, FINAL]);
        assert_eq!(names(&out), ["prompt", "agent_start", "reply_done"]);
        assert_eq!(out[2].ts - out[0].ts, 60.0);
    }

    #[test]
    fn without_chunk_numbers_the_turn_ends_with_the_call_that_asked_for_no_tools() {
        let call = |n: u32, at: &str| format!(r#"{{"type":"assistant.turn_{at}","timestamp":"2026-11-19T10:00:{n:02}Z","data":{{"turnId":"0"}}}}"#);
        let say = |n: u32, tools: &str| {
            format!(r#"{{"type":"assistant.message","timestamp":"2026-11-19T10:00:{n:02}Z","data":{{"messageId":"m{n}","content":"","toolRequests":[{tools}]}}}}"#)
        };
        let ask = r#"{"toolCallId":"t1","name":"edit"}"#;
        let tool = TOOL.replace("10:00:04", "10:00:13");
        let done = DONE_TOOL.replace("10:00:05", "10:00:14");
        let lines = [
            START.to_string(),
            USER.to_string(),
            call(10, "start"),
            say(11, ""),
            say(12, ask),
            tool,
            done,
            call(15, "end"),
            call(16, "start"),
            say(17, ""),
            say(18, ""),
            call(19, "end"),
        ];
        let out = run(&lines.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(names(&out), ["prompt", "agent_start", "reply_done"]);
        assert_eq!(out[2].ts, out[0].ts + 19.0);
        assert!(matches!(out[2].kind, Kind::ReplyDone { tools: 1, .. }));
    }

    #[test]
    fn mid_turn_prompt_and_abort_close_the_turn() {
        let next = USER.replace("10:00:00", "10:00:30");
        let out = run(&[START, USER, ASK, TOOL, &next]);
        assert_eq!(names(&out), ["prompt", "agent_start", "reply_done", "prompt"]);
        assert_eq!(out[2].ts, out[1].ts);

        let abort = r#"{"type":"abort","timestamp":"2026-11-19T10:00:20Z","data":{"reason":"user initiated"}}"#;
        let out = run(&[START, USER, ASK, TOOL, abort]);
        assert_eq!(names(&out), ["prompt", "agent_start", "reply_done"]);
        assert_eq!(out[2].ts - out[0].ts, 20.0);

        let shell = r#"{"type":"user.message","timestamp":"2026-11-19T10:00:00Z","data":{"content":"ls -la src","agentMode":"shell"}}"#;
        assert!(run(&[START, shell]).is_empty());
    }

    #[test]
    fn tail_reads_appends_with_cwd_from_the_head_partial_lines_and_resume() {
        let dir = std::env::temp_dir().join(format!("kairos-copilot-cli-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let f = dir.join("cli-1/events.jsonl");
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(&f, format!("{START}\n{MODEL}\n")).unwrap();
        let mut src = CopilotCli::new(vec![dir.clone()]);
        assert!(src.poll().is_empty());
        let half = &FINAL[..30];
        fs::write(&f, format!("{START}\n{MODEL}\n{USER}\n{ASK}\n{TOOL}\n{half}")).unwrap();
        let out = src.poll();
        assert_eq!(names(&out), ["prompt", "agent_start"]);
        assert_eq!(out[0].cwd.as_deref(), Some(Path::new("/w/p")));
        assert!(matches!(&out[0].kind, Kind::Prompt { session, .. } if session == "cli-1"));
        fs::write(&f, format!("{START}\n{MODEL}\n{USER}\n{ASK}\n{TOOL}\n{FINAL}\n")).unwrap();
        assert_eq!(names(&src.poll()), ["reply_done"]);

        let mut again = CopilotCli::new(vec![dir.clone()]);
        again.restore(&src.state());
        let next = USER.replace("10:00:00", "10:05:00");
        fs::write(&f, format!("{START}\n{MODEL}\n{USER}\n{ASK}\n{TOOL}\n{FINAL}\n{next}\n")).unwrap();
        let out = again.poll();
        assert_eq!(names(&out), ["prompt"]);
        assert_eq!(out[0].cwd.as_deref(), Some(Path::new("/w/p")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_vs_code_last_opened_is_vs_code_chat() {
        let dir = std::env::temp_dir().join(format!("kairos-copilot-vscode-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let f = dir.join("cli-1/events.jsonl");
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        let yaml = |client: &str| fs::write(dir.join("cli-1/workspace.yaml"), format!("id: cli-1\ncwd: /w/p\nlast_client_name: {client}\n")).unwrap();
        yaml("vscode-agent-host");
        fs::write(&f, format!("{START}\n")).unwrap();
        let mut src = CopilotCli::new(vec![dir.clone()]);
        assert!(src.poll().is_empty());
        fs::write(&f, format!("{START}\n{USER}\n{FINAL}\n")).unwrap();
        let out = src.poll();
        assert_eq!(names(&out), ["prompt", "reply_done"]);
        assert!(out.iter().all(|f| src.harness_of(f) == "copilot"));

        yaml("github/cli");
        let next = USER.replace("10:00:00", "10:05:00");
        fs::write(&f, format!("{START}\n{USER}\n{FINAL}\n{next}\n")).unwrap();
        let out = src.poll();
        assert_eq!(names(&out), ["prompt"]);
        assert_eq!(src.harness_of(&out[0]), "copilot-cli");
        let _ = fs::remove_dir_all(&dir);
    }

    fn complete(id: &str, extra: &str) -> String {
        format!(r#"{{"type":"tool.execution_complete","timestamp":"2026-11-19T10:00:05Z","data":{{"toolCallId":"{id}",{extra}}}}}"#)
    }

    fn started(id: &str, name: &str, args: &str) -> String {
        format!(r#"{{"type":"tool.execution_start","timestamp":"2026-11-19T10:00:04Z","data":{{"toolCallId":"{id}","toolName":"{name}","arguments":{args}}}}}"#)
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
    fn edits_use_the_line_metrics_of_single_file_calls_only() {
        let one = complete("t1", r#""success":true,"toolTelemetry":{"restrictedProperties":{"filePaths":"[\"/w/p/a.rs\",\"/home/u/.copilot/session-state/x/plan.md\"]"},"metrics":{"linesAdded":4,"linesRemoved":1}}"#);
        let created = complete("t2", r#""success":true,"toolTelemetry":{"restrictedProperties":{"addedPaths":"[\"/w/p/new.rs\"]"},"metrics":{"linesAdded":7,"linesRemoved":0}}"#);
        let two = complete("t3", r#""success":true,"toolTelemetry":{"restrictedProperties":{"filePaths":"[\"/w/p/a.rs\",\"/w/p/b.rs\"]"},"metrics":{"linesAdded":4,"linesRemoved":1}}"#);
        let failed = complete("t4", r#""success":false,"toolTelemetry":{"restrictedProperties":{"filePaths":"[\"/w/p/a.rs\"]"},"metrics":{"linesAdded":4,"linesRemoved":1}}"#);
        let nometrics = complete("t5", r#""success":true,"toolTelemetry":{"restrictedProperties":{"filePaths":"[\"/w/p/a.rs\"]"}}"#);
        let sub = one.replace(r#"{"type":"tool.execution_complete","#, r#"{"type":"tool.execution_complete","agentId":"sub-1","#);
        let out = run(&[START, USER, ASK, TOOL, &one, &created, &two, &failed, &nometrics, &sub, FINAL]);
        assert_eq!(work(&out), ["cli-1 /w/p/a.rs +4 -1", "cli-1 /w/p/new.rs +7 -0", "cli-1 /w/p/a.rs +4 -1"]);
        let rest: Vec<&str> = names(&out).into_iter().filter(|n| !n.starts_with("agent_edit")).collect();
        assert_eq!(rest, ["prompt", "agent_start", "reply_done"]);
    }

    #[test]
    fn a_commit_is_a_shell_call_whose_result_printed_a_sha() {
        let mut s = Sess { session: "cli-1".into(), cwd: Some(PathBuf::from("/w/p")), ..Sess::default() };
        let mut out = Vec::new();
        for l in [
            started("c1", "bash", r#"{"command":"git add -A && git commit -m 'fix it'"}"#),
            complete("c1", r#""success":true,"result":{"content":"[main abc1234] fix it\n 1 file changed"}"#),
            started("c2", "bash", r#"{"command":"git status"}"#),
            complete("c2", r#""success":true,"result":{"content":"[main fffffff] stray"}"#),
            started("c3", "bash", r#"{"command":"git commit -m x"}"#),
            complete("c3", r#""success":true,"result":{"content":"nothing to commit, working tree clean"}"#),
        ] {
            feed(&mut s, &serde_json::from_str(&l).unwrap(), &mut out);
        }
        assert_eq!(work(&out), ["cli-1 commit abc1234 in /w/p"]);
    }
}
