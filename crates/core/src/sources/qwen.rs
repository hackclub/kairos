use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{work, words, Found, Source, Tail};
use crate::event::Kind;
use crate::parse::timestamp;
use crate::{prompt_shape, prompt_stats};

const EDIT_TOOLS: [&str; 4] = ["write_file", "edit", "replace", "multi_edit"];
const SHELL_TOOLS: [&str; 2] = ["run_shell_command", "shell"];

// $QWEN_RUNTIME_DIR, else $QWEN_HOME, else ~/.qwen; sessions in projects/<sanitized cwd>/chats/<id>.jsonl.
pub fn default_roots() -> Vec<PathBuf> {
    std::env::var_os("QWEN_RUNTIME_DIR")
        .or_else(|| std::env::var_os("QWEN_HOME"))
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".qwen")))
        .map(|r| r.join("projects"))
        .into_iter()
        .collect()
}

fn skip(p: &Path) -> bool {
    p.parent().and_then(Path::file_name).is_none_or(|n| n != "chats")
}

#[derive(Default)]
struct Turn {
    session: String,
    cwd: Option<PathBuf>,
    model: Option<String>,
    armed: bool,
    open: bool,
    last_ts: f64,
    reply_words: usize,
    tools: usize,
    files: BTreeSet<PathBuf>,
    calls: HashMap<String, Call>,
}

enum Call {
    Edit(Option<String>),
    Shell(String),
}

impl Turn {
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
                session: self.session.clone(),
                reply_words: self.reply_words,
                tools: self.tools,
                files_changed: self.files.len(),
                model: self.model.clone(),
            },
        });
    }

    fn file(&mut self, p: &str) {
        self.files.insert(self.cwd.clone().unwrap_or_default().join(p));
    }
}

fn parts(e: &Value) -> Vec<&Value> {
    e.pointer("/message/parts").and_then(Value::as_array).into_iter().flatten().collect()
}

fn text(parts: &[&Value]) -> String {
    parts
        .iter()
        .filter(|p| p.get("thought").and_then(Value::as_bool) != Some(true))
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

// A paste goes in between <clipboard-paste> marks, with a note of Qwen Code's own for the model after the last one.
fn unmarked(prompt: &str) -> String {
    const MARK: &str = "</clipboard-paste>";
    let Some(end) = prompt.rfind(MARK) else { return prompt.to_string() };
    let (head, tail) = prompt.split_at(end + MARK.len());
    let tail = tail.find("Each block delimited by <clipboard-paste> markers").map_or(tail, |i| &tail[..i]);
    format!("{head}{tail}").replace("<clipboard-paste>", "").replace(MARK, "")
}

fn agent_work(t: &mut Turn, e: &Value, ts: f64, ps: &[&Value], out: &mut Vec<Found>) {
    match e.get("type").and_then(Value::as_str) {
        Some("assistant") => {
            for c in ps.iter().filter_map(|p| p.get("functionCall")) {
                let (Some(id), Some(name)) = (c.get("id").and_then(Value::as_str), c.get("name").and_then(Value::as_str)) else { continue };
                let arg = |k: &str| c.pointer(&format!("/args/{k}")).and_then(Value::as_str);
                if EDIT_TOOLS.contains(&name) {
                    let path = ["file_path", "absolute_path", "path"].iter().find_map(|k| arg(k));
                    work::remember(&mut t.calls, id.to_string(), Call::Edit(path.map(str::to_string)));
                } else if let (true, Some(cmd)) = (SHELL_TOOLS.contains(&name), arg("command").filter(|c| c.contains("commit"))) {
                    let cmd = match arg("directory").or_else(|| arg("dir_path")) {
                        Some(d) => format!("cd {} && {cmd}", work::shell_join([d])),
                        None => cmd.to_string(),
                    };
                    work::remember(&mut t.calls, id.to_string(), Call::Shell(cmd));
                }
            }
        }
        Some("tool_result") => {
            let Some(r) = e.get("toolCallResult") else { return };
            let Some(call) = r.get("callId").and_then(Value::as_str).and_then(|id| t.calls.remove(id)) else { return };
            if r.get("status").and_then(Value::as_str).is_some_and(|s| s != "success") {
                return;
            }
            let rd = r.get("resultDisplay");
            match call {
                Call::Edit(path) => {
                    let path = rd.and_then(|d| d.get("filePath")).and_then(Value::as_str).or(path.as_deref());
                    if let (Some(path), Some(counts)) = (path, rd.and_then(work::display_counts)) {
                        work::edit(ts, t.cwd.as_deref(), &t.session, path, counts, out);
                    }
                }
                Call::Shell(cmd) => {
                    // 0.28 shows an object with the command's own output; the model's copy puts a label before it.
                    let output = rd
                        .and_then(|d| d.as_str().or_else(|| d.get("output").and_then(Value::as_str)))
                        .or_else(|| ps.iter().find_map(|p| p.pointer("/functionResponse/response/output")?.as_str()))
                        .unwrap_or("");
                    work::commits(ts, t.cwd.as_deref(), &t.session, &cmd, output, out);
                }
            }
        }
        _ => {}
    }
}

fn feed(t: &mut Turn, e: &Value, out: &mut Vec<Found>) {
    if e.get("forkedFrom").is_some_and(|f| !f.is_null()) {
        return;
    }
    let Some(ts) = timestamp(e) else { return };
    if let Some(c) = e.get("cwd").and_then(Value::as_str) {
        t.cwd = Some(PathBuf::from(c));
    }
    if let Some(s) = e.get("sessionId").and_then(Value::as_str) {
        t.session = s.to_string();
    }
    if let Some(m) = e.get("model").and_then(Value::as_str).filter(|m| !m.is_empty()) {
        t.model = Some(m.to_string());
    }
    let ps = parts(e);
    agent_work(t, e, ts, &ps, out);
    match e.get("type").and_then(Value::as_str) {
        Some("user") => {
            if matches!(e.get("subtype").and_then(Value::as_str), Some("cron" | "notification"))
                || ps.iter().any(|p| p.get("functionResponse").is_some())
            {
                return;
            }
            let prompt = unmarked(&text(&ps));
            let (w, pasted) = prompt_stats(&prompt);
            if w == 0 {
                return;
            }
            let last = t.last_ts;
            t.close(last.min(ts), out);
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
                    words: w,
                    pasted,
                    model: t.model.clone(),
                    session: t.session.clone(),
                    shape: Some(prompt_shape(&prompt)),
                    version: None,
                    via: None,
                },
            });
        }
        Some("assistant") if t.open => {
            t.last_ts = ts;
            t.reply_words += words(&text(&ps));
            let calls: Vec<&Value> = ps.iter().filter_map(|p| p.get("functionCall")).collect();
            for c in &calls {
                if t.armed {
                    t.armed = false;
                    out.push(Found { ts, cwd: t.cwd.clone(), kind: Kind::AgentStart { session: t.session.clone() } });
                }
                t.tools += 1;
                let name = c.get("name").and_then(Value::as_str).unwrap_or("");
                if EDIT_TOOLS.contains(&name) {
                    if let Some(p) = ["file_path", "absolute_path", "path"].iter().find_map(|k| c.pointer(&format!("/args/{k}"))?.as_str()) {
                        t.file(p);
                    }
                }
            }
            if calls.is_empty() {
                t.close(ts, out);
            }
        }
        Some("tool_result") if t.open => {
            t.last_ts = ts;
            let r = e.get("toolCallResult");
            let ok = r.and_then(|r| r.get("status")).and_then(Value::as_str).is_none_or(|s| s == "success");
            // fileName is the name alone, without its folders.
            let f = r.and_then(|r| r.pointer("/resultDisplay/filePath").or_else(|| r.pointer("/resultDisplay/fileName")));
            if let Some(f) = f.and_then(Value::as_str).filter(|_| ok) {
                if r.and_then(|r| r.pointer("/resultDisplay/diffStat")).is_some() {
                    t.file(f);
                }
            }
        }
        _ => {}
    }
}

pub struct Qwen {
    tail: Tail,
    turns: HashMap<PathBuf, Turn>,
}

impl Qwen {
    pub fn new(roots: Vec<PathBuf>) -> Qwen {
        Qwen { tail: Tail::new(roots, "jsonl", skip), turns: HashMap::new() }
    }
}

impl Source for Qwen {
    fn name(&self) -> &'static str {
        "qwen_code"
    }

    fn harness(&self) -> &'static str {
        "qwen-code"
    }

    fn poll(&mut self) -> Vec<Found> {
        let mut out = Vec::new();
        let turns = &mut self.turns;
        self.tail.poll(|path, line| {
            if let Ok(e) = serde_json::from_str::<Value>(line) {
                feed(turns.entry(path.to_path_buf()).or_default(), &e, &mut out);
            }
        });
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

    const USER: &str = r#"{"type":"user","sessionId":"q1","cwd":"/w/p","timestamp":"2026-11-19T10:00:00Z","message":{"role":"user","parts":[{"text":"please fix the failing parser test now"}]}}"#;
    const CALL: &str = r#"{"type":"assistant","sessionId":"q1","cwd":"/w/p","timestamp":"2026-11-19T10:00:05Z","model":"qwen3-coder","message":{"role":"model","parts":[{"text":"private thoughts","thought":true},{"text":"Looking at it."},{"functionCall":{"id":"c1","name":"edit","args":{"file_path":"/w/p/a.rs"}}}]}}"#;
    const RESULT: &str = r#"{"type":"tool_result","sessionId":"q1","cwd":"/w/p","timestamp":"2026-11-19T10:00:06Z","message":{"role":"user","parts":[{"functionResponse":{"id":"c1","name":"edit"}}]},"toolCallResult":{"callId":"c1","status":"success","resultDisplay":{"fileName":"a.rs","diffStat":{"model_added_lines":1}}}}"#;
    const DONE: &str = r#"{"type":"assistant","sessionId":"q1","cwd":"/w/p","timestamp":"2026-11-19T10:01:00Z","model":"qwen3-coder","message":{"role":"model","parts":[{"text":"Fixed it in a.rs."}]}}"#;

    #[test]
    fn a_turn_becomes_prompt_start_done() {
        let out = run(&[USER, CALL, RESULT, DONE]);
        assert_eq!(names(&out), ["prompt", "agent_start", "reply_done"]);
        assert_eq!(out[0].cwd.as_deref(), Some(Path::new("/w/p")));
        match &out[2].kind {
            Kind::ReplyDone { reply_words, tools, files_changed, model, session } => {
                assert_eq!((*reply_words, *tools, *files_changed), (7, 1, 1));
                assert_eq!((model.as_deref(), session.as_str()), (Some("qwen3-coder"), "q1"));
            }
            k => panic!("{k:?}"),
        }
        assert_eq!(out[2].ts - out[0].ts, 60.0);
    }

    #[test]
    fn mid_turn_prompts_forks_and_injected_records() {
        let next = USER.replace("10:00:00", "10:00:30");
        let out = run(&[USER, CALL, RESULT, &next]);
        assert_eq!(names(&out), ["prompt", "agent_start", "reply_done", "prompt"]);
        assert_eq!(out[2].ts - out[0].ts, 6.0);

        let forked = USER.replace(r#""type":"user","#, r#""type":"user","forkedFrom":"q0","#);
        let cron = USER.replace(r#""type":"user","#, r#""type":"user","subtype":"cron","#);
        assert!(run(&[&forked, &cron, CALL, DONE]).is_empty());
    }

    #[test]
    fn tail_reads_appends_partial_lines_and_resumes() {
        let dir = std::env::temp_dir().join(format!("kairos-qwen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let f = dir.join("-w-p/chats/q1.jsonl");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, format!("{USER}\n")).unwrap();
        let mut src = Qwen::new(vec![dir.clone()]);
        assert!(src.poll().is_empty());
        let next = USER.replace("10:00:00", "10:05:00");
        let call = CALL.replace("10:00:05", "10:05:05");
        let done = DONE.replace("10:01:00", "10:06:00");
        std::fs::write(&f, format!("{USER}\n{next}\n{call}\n{}", &done[..30])).unwrap();
        assert_eq!(names(&src.poll()), ["prompt", "agent_start"]);
        std::fs::write(&f, format!("{USER}\n{next}\n{call}\n{done}\n")).unwrap();
        assert_eq!(names(&src.poll()), ["reply_done"]);

        let mut again = Qwen::new(vec![dir.clone()]);
        again.restore(&src.state());
        let third = USER.replace("10:00:00", "10:09:00");
        std::fs::write(&f, format!("{USER}\n{next}\n{call}\n{done}\n{third}\n")).unwrap();
        let out = again.poll();
        assert_eq!(names(&out), ["prompt"]);
        assert_eq!(out[0].cwd.as_deref(), Some(Path::new("/w/p")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn call(id: &str, name: &str, args: &str) -> String {
        format!(
            r#"{{"type":"assistant","sessionId":"q1","cwd":"/w/p","timestamp":"2026-11-19T10:00:05Z","message":{{"role":"model","parts":[{{"functionCall":{{"id":"{id}","name":"{name}","args":{args}}}}}]}}}}"#
        )
    }

    fn done(id: &str, status: &str, display: &str) -> String {
        format!(
            r#"{{"type":"tool_result","sessionId":"q1","cwd":"/w/p","timestamp":"2026-11-19T10:00:06Z","message":{{"role":"user","parts":[{{"functionResponse":{{"id":"{id}","name":"x","response":{{"output":"ignored"}}}}}}]}},"toolCallResult":{{"callId":"{id}","status":"{status}","resultDisplay":{display}}}}}"#
        )
    }

    fn summary(out: &[Found]) -> Vec<String> {
        out.iter()
            .filter_map(|f| match &f.kind {
                Kind::AgentEdit { path, added, removed, session } => Some(format!("{session} {path} +{added} -{removed}")),
                Kind::AgentCommit { sha, .. } => Some(format!("commit {} in {}", sha.as_deref().unwrap_or("-"), f.cwd.as_ref().unwrap().display().to_string().replace('\\', "/"))),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn edits_count_the_written_diff_and_commits_need_a_printed_sha() {
        let diff = r#"{"fileName":"a.rs","fileDiff":"--- a.rs\n+++ a.rs\n@@ -1,2 +1,3 @@\n-old\n+new\n+more\n keep\n","diffStat":{"model_added_lines":7}}"#;
        let new = r#"{"fileName":"b.rs","originalContent":"","newContent":"1\n2\n","diffStat":{"model_added_lines":2,"model_removed_lines":0}}"#;
        let out = run(&[
            USER,
            &call("e1", "edit", r#"{"file_path":"/w/p/a.rs"}"#),
            &done("e1", "success", diff),
            &call("e2", "write_file", r#"{"file_path":"/w/p/b.rs","content":"1\n2\n"}"#),
            &done("e2", "success", new),
            &call("s1", "run_shell_command", r#"{"command":"git add -A && git commit -m done"}"#),
            &done("s1", "success", r#""[main abc1234] done\n 2 files changed""#),
            DONE,
        ]);
        assert_eq!(summary(&out), ["q1 /w/p/a.rs +2 -1", "q1 /w/p/b.rs +2 -0", "commit abc1234 in /w/p"]);
        assert_eq!(names(&out).iter().filter(|n| !n.starts_with("agent_edit") && !n.starts_with("agent_commit")).count(), 3);
    }

    #[test]
    fn rejected_edits_and_other_commands_make_nothing() {
        let diff = r#"{"fileName":"a.rs","fileDiff":"@@ -1 +1 @@\n-a\n+b\n"}"#;
        let out = run(&[
            USER,
            &call("e1", "edit", r#"{"file_path":"/w/p/a.rs"}"#),
            &done("e1", "cancelled", diff),
            &call("e2", "edit", r#"{"file_path":"/w/p/a.rs"}"#),
            &done("e2", "error", r#""Error: no match""#),
            &call("e3", "read_file", r#"{"file_path":"/w/p/a.rs"}"#),
            &done("e3", "success", diff),
            &call("s1", "run_shell_command", r#"{"command":"git status"}"#),
            &done("s1", "success", r#""[main abc1234] stray""#),
            &call("s2", "run_shell_command", r#"{"command":"git commit -m x"}"#),
            &done("s2", "success", r#""nothing to commit""#),
            &done("zz", "success", diff),
        ]);
        assert!(summary(&out).is_empty());
    }

    // As Qwen Code 0.28.7 writes them (lab run, 2026-11-27), cut down.
    #[test]
    fn qwen_0_28_shell_results_file_names_and_pastes() {
        let write = r#"{"fileDiff":"@@ -0,0 +1,1 @@\n+NAMES = []\n","fileName":"util.py","filePath":"/w/p/pkg/util.py","originalContent":"","newContent":"NAMES = []\n","diffStat":{"model_added_lines":1,"model_removed_lines":0}}"#;
        let edit = r#"{"fileDiff":"@@ -4 +4,2 @@\n-a\n+b\n+c\n","fileName":"hello.py","filePath":"/w/p/hello.py","diffStat":{"model_added_lines":2,"model_removed_lines":1}}"#;
        let shell = r#"{"type":"shell_result","version":1,"directory":"/w/p","exitCode":0,"outcome":"completed","output":"[main 4958dbe] add util\n 2 files changed, 3 insertions(+), 1 deletion(-)"}"#;
        let commit = done("s1", "success", shell).replace(
            r#""response":{"output":"ignored"}"#,
            r#""response":{"output":"Command: git commit -m \"add util\"\nDirectory: (root)\nOutput (stdout and stderr combined): [main 4958dbe] add util\n 2 files changed"}"#,
        );
        let out = run(&[
            USER,
            &call("e1", "write_file", r#"{"file_path":"/w/p/pkg/util.py","content":"NAMES = []\n"}"#),
            &done("e1", "success", write),
            &call("e2", "edit", r#"{"file_path":"/w/p/hello.py","old_string":"a","new_string":"b\nc"}"#),
            &done("e2", "success", edit),
            &call("s1", "run_shell_command", r#"{"command":"git commit -m \"add util\""}"#),
            &commit,
            DONE,
        ]);
        assert_eq!(summary(&out), ["q1 /w/p/pkg/util.py +1 -0", "q1 /w/p/hello.py +2 -1", "commit 4958dbe in /w/p"]);
        match &out.iter().find(|f| f.kind.name() == "reply_done").unwrap().kind {
            Kind::ReplyDone { files_changed, .. } => assert_eq!(*files_changed, 2),
            k => panic!("{k:?}"),
        }

        let typed = "keep this in a notes file please";
        let pasted = "$ python hello.py\nhello kairos\nhello lab";
        let note = "Each block delimited by <clipboard-paste> markers in this message was pasted from the system clipboard, and it was too large to display in the editor, so the user may not have read it. Text outside those markers is the user's own typed input.";
        let prompt = format!("{typed}\n<clipboard-paste>\n{pasted}\n</clipboard-paste>\n{note}");
        let user = USER.replace("please fix the failing parser test now", &prompt.replace('\n', "\\n"));
        match &run(&[&user])[0].kind {
            Kind::Prompt { words, .. } => assert_eq!(*words, 7 + 7),
            k => panic!("{k:?}"),
        }
        assert_eq!(unmarked(&format!("{typed}\n<clipboard-paste>\n{pasted}\n</clipboard-paste>\nand more\n{note}")).split_whitespace().count(), 16);
    }
}
