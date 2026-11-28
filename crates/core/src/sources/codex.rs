use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use walkdir::WalkDir;

use super::{string_words, uri_path, work, words, Found, Source, Tail};
use crate::event::{App, Ask, Kind};
use crate::parse::timestamp;
use crate::{prompt_shape, prompt_stats};

const IDE_REQUEST: &str = "## My request for Codex:";
const INJECTED: [&str; 8] = [
    "<environment_context>",
    "<user_instructions>",
    "<instructions>",
    "<turn_aborted>",
    "<subagent_notification>",
    "<recommended_plugins>",
    "# AGENTS.md instructions",
    "Another language model started to solve this problem",
];
const PATCH_MARKERS: [&str; 4] = ["*** Update File: ", "*** Add File: ", "*** Delete File: ", "*** Move to: "];

pub fn default_roots() -> Vec<PathBuf> {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".codex")));
    home.map(|h| h.join("sessions")).into_iter().collect()
}

#[derive(Default)]
struct Turn {
    known: bool,
    skip: bool,
    items: bool,
    session: String,
    cwd: Option<PathBuf>,
    model: Option<String>,
    version: Option<String>,
    // The Codex app's own sessions, and whether the turn running was started from its composer (it also starts turns
    // for automations, reviews and reconnects, which say so).
    desktop: bool,
    composer: bool,
    armed: bool,
    open: bool,
    last_ts: f64,
    reply_words: usize,
    tools: usize,
    files: BTreeSet<PathBuf>,
    done: VecDeque<String>,
    asks: HashMap<String, (f64, usize)>,
}

// Patches and shell commands whose results the rollout logged, once each even if two event
// generations report the same call.
const DONE_KEPT: usize = 256;

impl Turn {
    fn fresh(&mut self, id: Option<&str>) -> bool {
        let Some(id) = id.filter(|i| !i.is_empty()) else { return true };
        if self.done.iter().any(|d| d == id) {
            return false;
        }
        if self.done.len() >= DONE_KEPT {
            self.done.pop_front();
        }
        self.done.push_back(id.to_string());
        true
    }

    fn meta(&mut self, p: &Value) {
        let source = p.get("source");
        self.known = true;
        self.skip = p.get("thread_source").and_then(Value::as_str).is_some_and(|s| s != "user")
            || source.is_some_and(|s| s.is_object() || s.as_str() == Some("exec"));
        if let Some(id) = p.get("id").or_else(|| p.get("session_id")).and_then(Value::as_str) {
            self.session = id.to_string();
        }
        if let Some(c) = p.get("cwd").and_then(Value::as_str).and_then(folder) {
            self.cwd = Some(c);
        }
        if let Some(v) = p.get("cli_version").and_then(Value::as_str) {
            self.version = Some(v.to_string());
        }
        self.desktop = p.get("originator").and_then(Value::as_str) == Some("Codex Desktop");
    }

    // Tail skips history on first run, so a session's metadata line may never reach feed().
    fn head(&mut self, path: &Path) {
        self.known = true;
        self.session = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let mut line = String::new();
        let Ok(f) = File::open(path) else { return };
        if BufReader::new(f).read_line(&mut line).is_err() {
            return;
        }
        if let Ok(e) = serde_json::from_str::<Value>(&line) {
            if e.get("type").and_then(Value::as_str) == Some("session_meta") {
                if let Some(p) = e.get("payload") {
                    self.meta(p);
                }
            }
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
                session: self.session.clone(),
                reply_words: self.reply_words,
                tools: self.tools,
                files_changed: self.files.len(),
                model: self.model.clone(),
            },
        });
    }

    fn prompt(&mut self, ts: f64, text: &str, image: bool, out: &mut Vec<Found>) {
        let text = text.split_once(IDE_REQUEST).map_or(text, |(_, r)| r).trim();
        if INJECTED.iter().any(|p| text.starts_with(p)) || (text.is_empty() && !image) {
            return;
        }
        let last = self.last_ts;
        self.close(last.min(ts), out);
        let (words, pasted) = prompt_stats(text);
        self.armed = true;
        self.open = true;
        self.last_ts = ts;
        self.reply_words = 0;
        self.tools = 0;
        self.files.clear();
        out.push(Found {
            ts,
            cwd: self.cwd.clone(),
            kind: Kind::Prompt {
                words,
                pasted,
                model: None,
                session: self.session.clone(),
                shape: Some(prompt_shape(text)),
                version: self.version.clone(),
                via: (self.desktop && self.composer).then_some(App::CodexApp),
                permissions: None,
            },
        });
    }

    fn add_file(&mut self, path: &str) {
        self.files.insert(self.cwd.clone().unwrap_or_default().join(path));
    }
}

fn patch_paths(v: &Value, found: &mut Vec<String>) {
    match v {
        Value::String(s) if s.contains("*** Begin Patch") => {
            for line in s.lines() {
                let line = line.trim();
                if let Some(p) = PATCH_MARKERS.iter().find_map(|m| line.strip_prefix(m)) {
                    found.push(p.trim().to_string());
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| patch_paths(x, found)),
        Value::Object(m) => m.values().for_each(|x| patch_paths(x, found)),
        _ => {}
    }
}

// `add` and `delete` carry the whole file, `update` a unified diff (and `move_path` when it also renamed the file).
fn patch(t: &mut Turn, ts: f64, id: Option<&str>, changes: Option<&Value>, out: &mut Vec<Found>) {
    let Some(changes) = changes.and_then(Value::as_object) else { return };
    if !t.fresh(id) {
        return;
    }
    for (path, c) in changes {
        let text = |k: &str| c.get(k).and_then(Value::as_str);
        let counts = match c.get("type").and_then(Value::as_str) {
            Some("add") => (text("content").map_or(0, work::line_count), 0),
            Some("delete") => (0, text("content").map_or(0, work::line_count)),
            Some("update") => text("unified_diff").map_or((0, 0), work::patch_counts),
            _ => continue,
        };
        let path = text("move_path").filter(|m| !m.is_empty()).unwrap_or(path);
        work::edit(ts, t.cwd.as_deref(), &t.session, path, counts, out);
    }
}

fn command(t: &mut Turn, ts: f64, id: Option<&str>, v: &Value, out: &mut Vec<Found>) {
    let argv: Vec<&str> = match v.get("command") {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        Some(Value::String(s)) => vec![s.as_str()],
        _ => return,
    };
    let output = ["aggregated_output", "stdout"].iter().find_map(|k| v.get(*k).and_then(Value::as_str)).unwrap_or("");
    if !argv.iter().any(|a| a.contains("commit")) || !t.fresh(id) {
        return;
    }
    let cwd = v.get("cwd").and_then(Value::as_str).and_then(folder).or_else(|| t.cwd.clone());
    work::commits(ts, cwd.as_deref(), &t.session, &work::shell_join(argv), output, out);
}

// Codex 0.167 names a command's folder with a file:// URI.
pub(crate) fn folder(s: &str) -> Option<PathBuf> {
    if s.starts_with("file://") {
        uri_path(s)
    } else {
        Some(PathBuf::from(s))
    }
}

// Subagent and `codex exec` rollouts count too.
fn agent_work(t: &mut Turn, ts: f64, ty: &str, p: &Value, out: &mut Vec<Found>) {
    if ty != "event_msg" {
        return;
    }
    match p.get("type").and_then(Value::as_str) {
        Some("item_completed") => {
            let Some(item) = p.get("item") else { return };
            let id = item.get("id").and_then(Value::as_str);
            match item.get("type").and_then(Value::as_str) {
                Some("FileChange") if item.get("status").and_then(Value::as_str) == Some("completed") => {
                    patch(t, ts, id, item.get("changes"), out)
                }
                Some("CommandExecution") => command(t, ts, id, item, out),
                _ => {}
            }
        }
        Some("patch_apply_end") if p.get("success").and_then(Value::as_bool) == Some(true) => {
            patch(t, ts, p.get("call_id").and_then(Value::as_str), p.get("changes"), out)
        }
        Some("exec_command_end") => command(t, ts, p.get("call_id").and_then(Value::as_str), p, out),
        _ => {}
    }
}

fn feed(t: &mut Turn, e: &Value, out: &mut Vec<Found>) {
    let Some(ts) = timestamp(e) else { return };
    let Some(p) = e.get("payload") else { return };
    let ty = e.get("type").and_then(Value::as_str).unwrap_or("");
    let pt = p.get("type").and_then(Value::as_str).unwrap_or("");
    if ty == "session_meta" {
        t.meta(p);
        return;
    }
    agent_work(t, ts, ty, p, out);
    if t.skip {
        return;
    }
    match (ty, pt) {
        ("turn_context", _) => {
            if let Some(c) = p.get("cwd").and_then(Value::as_str).and_then(folder) {
                t.cwd = Some(c);
            }
            if let Some(m) = p.get("model").and_then(Value::as_str) {
                t.model = Some(m.to_string());
            }
        }
        ("event_msg", "task_started") => {
            let last = t.last_ts;
            t.close(last, out);
            let trigger = p.pointer("/turn_attribution/turn_trigger").and_then(Value::as_str);
            t.composer = trigger.map_or(true, |k| matches!(k, "composer" | "composer_queue" | "edit_user_message"));
        }
        ("event_msg", "task_complete" | "turn_aborted") => t.close(ts, out),
        ("event_msg", "user_message") if !t.items => {
            t.prompt(ts, p.get("message").and_then(Value::as_str).unwrap_or(""), false, out);
        }
        ("event_msg", "item_completed") => {
            let Some(item) = p.get("item") else { return };
            match item.get("type").and_then(Value::as_str) {
                Some("UserMessage") => {
                    t.items = true;
                    let content: Vec<&Value> =
                        item.get("content").and_then(Value::as_array).into_iter().flatten().collect();
                    let text: Vec<&str> = content
                        .iter()
                        .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|c| c.get("text").and_then(Value::as_str))
                        .collect();
                    let image = content.iter().any(|c| c.get("type").and_then(Value::as_str) == Some("image"));
                    t.prompt(ts, &text.join("\n"), image, out);
                }
                Some("FileChange") if t.open => {
                    t.last_ts = ts;
                    for path in item.get("changes").and_then(Value::as_object).into_iter().flat_map(|m| m.keys()) {
                        t.add_file(path);
                    }
                }
                _ => {}
            }
        }
        ("response_item", "message") if t.open => {
            if p.get("role").and_then(Value::as_str) == Some("assistant") {
                t.last_ts = ts;
                for c in p.get("content").and_then(Value::as_array).into_iter().flatten() {
                    if c.get("type").and_then(Value::as_str) == Some("output_text") {
                        t.reply_words += c.get("text").and_then(Value::as_str).map_or(0, words);
                    }
                }
            }
        }
        ("response_item", "function_call" | "custom_tool_call" | "local_shell_call") if t.open => {
            t.last_ts = ts;
            if t.armed {
                t.armed = false;
                out.push(Found { ts, cwd: t.cwd.clone(), kind: Kind::AgentStart { session: t.session.clone() } });
            }
            t.tools += 1;
            if let (Some("request_user_input"), Some(id)) = (p.get("name").and_then(Value::as_str), p.get("call_id").and_then(Value::as_str)) {
                let shown = p
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|a| serde_json::from_str::<Value>(a).ok())
                    .and_then(|a| a.get("questions").map(|q| string_words(q, usize::MAX)))
                    .unwrap_or(0);
                work::remember(&mut t.asks, id.to_string(), (ts, shown));
            }
            // exec wraps patches in escaped JS strings; its FileChange items already name the files
            if p.get("name").and_then(Value::as_str) != Some("exec") {
                let args: Option<Value> =
                    p.get("arguments").and_then(Value::as_str).and_then(|a| serde_json::from_str(a).ok());
                let mut found = Vec::new();
                for v in [p.get("input"), p.get("action"), args.as_ref()].into_iter().flatten() {
                    patch_paths(v, &mut found);
                }
                for f in found {
                    t.add_file(&f);
                }
            }
        }
        ("response_item", "function_call_output") => {
            let asked = p.get("call_id").and_then(Value::as_str).and_then(|id| t.asks.remove(id));
            if let Some((asked, shown_words)) = asked {
                let answer_words = p.get("output").and_then(Value::as_str).map_or(0, words);
                let kind = Kind::Answered { session: t.session.clone(), asked, why: Ask::Question, shown_words, answer_words };
                out.push(Found { ts, cwd: t.cwd.clone(), kind });
            }
        }
        _ => {}
    }
}

pub struct Codex {
    tail: Tail,
    turns: HashMap<PathBuf, Turn>,
    // The newest line read from each thread. Codex writes a thread's lines out again (unarchiving moves the rollout back
    // with a new time, migrating an old one rewrites it, a compressed one is put back), and those must not be reported twice.
    read: HashMap<String, f64>,
    archived: Vec<PathBuf>,
}

// An archived thread can be opened again months later.
const READ_KEPT: f64 = 365.0 * 86400.0;

impl Codex {
    pub fn new(roots: Vec<PathBuf>) -> Codex {
        let archived = roots.iter().filter_map(|r| Some(r.parent()?.join("archived_sessions"))).collect();
        Codex { tail: Tail::new(roots, "jsonl", |_| false), turns: HashMap::new(), read: HashMap::new(), archived }
    }
}

// rollout-<time>-<thread>.jsonl, or rollout-<time>-<thread>_<rollout>.jsonl for a reverted thread.
fn thread_of(path: &Path) -> Option<&str> {
    let ids = path.file_stem()?.to_str()?.strip_prefix("rollout-")?.get(20..)?;
    Some(ids.split_once('_').map_or(ids, |(thread, _)| thread))
}

impl Source for Codex {
    fn health(&mut self) -> super::Health {
        self.tail.health()
    }

    fn name(&self) -> &'static str {
        "codex"
    }

    fn poll(&mut self) -> Vec<Found> {
        let mut out = Vec::new();
        let (turns, read) = (&mut self.turns, &mut self.read);
        // A rollout read from its start still sets up its turn from the lines already read, but reports only what's newer.
        let mut cuts: HashMap<PathBuf, Option<f64>> = HashMap::new();
        self.tail.poll_lines(|path, line, from_start| {
            let Ok(e) = serde_json::from_str::<Value>(line) else { return };
            let Some(ts) = timestamp(&e) else { return };
            let t = turns.entry(path.to_path_buf()).or_default();
            if !t.known {
                t.head(path);
            }
            let cut = if from_start { *cuts.entry(path.to_path_buf()).or_insert_with(|| read.get(&t.session).copied()) } else { None };
            let before = out.len();
            feed(t, &e, &mut out);
            if cut.is_some_and(|c| ts <= c) {
                out.truncate(before);
            }
            if !t.session.is_empty() {
                let r = read.entry(t.session.clone()).or_insert(ts);
                *r = r.max(ts);
            }
        });
        let newest = self.read.values().copied().fold(f64::MIN, f64::max);
        self.read.retain(|_, ts| *ts > newest - READ_KEPT);
        self.turns.retain(|p, _| p.exists());
        out.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        out
    }

    fn state(&self) -> Value {
        let mut state = self.tail.state();
        state["read"] = json!(self.read);
        state
    }

    fn restore(&mut self, state: &Value) {
        self.tail.restore(state);
        if let Some(read) = state.get("read").and_then(Value::as_object) {
            self.read.extend(read.iter().filter_map(|(k, v)| Some((k.clone(), v.as_f64()?))));
            return;
        }
        // Saved before threads were kept: what the rollouts held at the last poll was read, and so was every archived one.
        // A little before it, for lines Codex was still writing out then.
        let Some(since) = state.get("since").and_then(Value::as_f64) else { return };
        let saved = state.get("files").and_then(Value::as_object).into_iter().flat_map(|f| f.keys()).map(PathBuf::from);
        let archived = self.archived.iter().flat_map(|d| WalkDir::new(d).into_iter().filter_map(Result::ok).map(|e| e.into_path()));
        for path in saved.chain(archived) {
            if let Some(thread) = thread_of(&path) {
                self.read.insert(thread.to_string(), since - 10.0);
            }
        }
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

    const META: &str = r#"{"timestamp":"2026-11-19T09:59:59Z","type":"session_meta","payload":{"id":"s1","cwd":"/w/p","source":"cli","thread_source":"user","cli_version":"0.167.0"}}"#;
    const START: &str = r#"{"timestamp":"2026-11-19T10:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#;
    const CTX: &str = r#"{"timestamp":"2026-11-19T10:00:00Z","type":"turn_context","payload":{"cwd":"/w/p","model":"gpt-x"}}"#;
    const USER: &str = r#"{"timestamp":"2026-11-19T10:00:01Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"please fix the failing parser test now"}]}}}"#;
    const SAY: &str = r#"{"timestamp":"2026-11-19T10:00:03Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Looking at it."}]}}"#;
    const TOOL: &str = r#"{"timestamp":"2026-11-19T10:00:05Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"run()"}}"#;
    const EDIT: &str = r#"{"timestamp":"2026-11-19T10:00:06Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"FileChange","changes":{"/w/p/a.rs":{"type":"update"}}}}}"#;
    const FINAL: &str = r#"{"timestamp":"2026-11-19T10:01:00Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"Fixed it in a.rs."}]}}"#;
    const DONE: &str = r#"{"timestamp":"2026-11-19T10:01:01Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"t1"}}"#;
    const ABORT: &str = r#"{"timestamp":"2026-11-19T10:00:30Z","type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}"#;

    #[test]
    fn a_prompt_typed_in_the_codex_app_composer_says_so() {
        let via = |meta: &str, start: &str| {
            run(&[meta, start, CTX, USER]).into_iter().find_map(|f| match f.kind {
                Kind::Prompt { via, .. } => Some(via),
                _ => None,
            })
        };
        let app = META.replace(r#""source":"cli""#, r#""originator":"Codex Desktop","source":"vscode""#);
        let started = |trigger: &str| START.replace(r#""turn_id":"t1""#, &format!(r#""turn_id":"t1","turn_attribution":{{"turn_trigger":"{trigger}"}}"#));
        assert_eq!(via(&app, START), Some(Some(App::CodexApp)));
        assert_eq!(via(&app, &started("composer")), Some(Some(App::CodexApp)));
        assert_eq!(via(&app, &started("automation_heartbeat_x")), Some(None));
        assert_eq!(via(META, &started("composer")), Some(None), "the CLI");
        let vscode = META.replace(r#""source":"cli""#, r#""originator":"codex_vscode","source":"vscode""#);
        assert_eq!(via(&vscode, START), Some(None));
    }

    #[test]
    fn a_turn_becomes_prompt_start_done() {
        let out = run(&[META, START, CTX, USER, SAY, TOOL, EDIT, FINAL, DONE]);
        assert_eq!(names(&out), ["prompt", "agent_start", "reply_done"]);
        assert_eq!(out[0].cwd.as_deref(), Some(Path::new("/w/p")));
        match &out[0].kind {
            Kind::Prompt { words, pasted, model, session, shape, version, .. } => {
                assert_eq!((*words, *pasted, session.as_str(), version.as_deref()), (7, false, "s1", Some("0.167.0")));
                assert_eq!(shape.as_ref().map(|s| (s.lines, s.sentences)), Some((1, 1)));
                assert_eq!(*model, None);
            }
            k => panic!("{k:?}"),
        }
        match &out[2].kind {
            Kind::ReplyDone { reply_words, tools, files_changed, model, .. } => {
                assert_eq!((*reply_words, *tools, *files_changed), (7, 1, 1));
                assert_eq!(model.as_deref(), Some("gpt-x"));
            }
            k => panic!("{k:?}"),
        }
        assert_eq!(out[2].ts - out[0].ts, 60.0);
    }

    #[test]
    fn legacy_user_message_and_patch_inputs() {
        let user = r#"{"timestamp":"2026-11-19T10:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"add a retry loop to the fetcher"}}"#;
        let patch = r#"{"timestamp":"2026-11-19T10:00:02Z","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","input":"*** Begin Patch\n*** Update File: a.rs\n@@\n*** End Patch"}}"#;
        let shell = r#"{"timestamp":"2026-11-19T10:00:03Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"apply_patch\",\"*** Begin Patch\\n*** Add File: b.rs\\n+x\\n*** End Patch\"]}"}}"#;
        let out = run(&[META, START, CTX, user, patch, shell, EDIT, DONE]);
        assert_eq!(names(&out), ["prompt", "agent_start", "reply_done"]);
        match &out[2].kind {
            Kind::ReplyDone { tools, files_changed, .. } => assert_eq!((*tools, *files_changed), (2, 2)),
            k => panic!("{k:?}"),
        }
    }

    #[test]
    fn interrupted_turn_closes_on_abort_or_next_prompt() {
        let out = run(&[META, START, CTX, USER, TOOL, ABORT]);
        assert_eq!(names(&out), ["prompt", "agent_start", "reply_done"]);
        assert_eq!(out[2].ts, out[0].ts + 29.0);

        let next = USER.replace("10:00:01", "10:02:00");
        let out = run(&[META, START, CTX, USER, TOOL, &next]);
        assert_eq!(names(&out), ["prompt", "agent_start", "reply_done", "prompt"]);
        assert_eq!(out[2].ts, out[1].ts);
    }

    #[test]
    fn injected_context_and_subagents_are_not_prompts() {
        let item = |text: &str| {
            format!(
                r#"{{"timestamp":"2026-11-19T10:00:01Z","type":"event_msg","payload":{{"type":"item_completed","item":{{"type":"UserMessage","content":[{{"type":"text","text":{text:?}}}]}}}}}}"#
            )
        };
        let env = item("<environment_context>\n<cwd>/w/p</cwd>\n</environment_context>");
        let agents = item("# AGENTS.md instructions for /w/p\n\nbe nice");
        let out = run(&[META, START, CTX, &env, &agents, SAY, TOOL, DONE]);
        assert!(out.is_empty());

        let ide = item("# Context from my IDE setup:\n\n## Open tabs\n- a.rs\n\n## My request for Codex:\nrename the parser struct please");
        let out = run(&[META, START, CTX, &ide, DONE]);
        assert!(matches!(&out[0].kind, Kind::Prompt { words: 5, .. }));

        let child = META.replace(r#""thread_source":"user""#, r#""thread_source":"subagent""#);
        assert!(run(&[&child, START, CTX, USER, TOOL, DONE]).is_empty());
        let exec = META.replace(r#""source":"cli""#, r#""source":"exec""#);
        assert!(run(&[&exec, START, CTX, USER, TOOL, DONE]).is_empty());
        let guardian = META.replace(r#""thread_source":"user""#, r#""thread_source":"guardian_review""#);
        assert!(run(&[&guardian, START, CTX, USER, TOOL, DONE]).is_empty());
        let spawned = META
            .replace(r#","thread_source":"user""#, "")
            .replace(r#""source":"cli""#, r#""source":{"subagent":{"other":"guardian"}}"#);
        assert!(run(&[&spawned, START, CTX, USER, TOOL, DONE]).is_empty());
    }

    #[test]
    fn tail_skips_history_then_reads_appends() {
        let dir = std::env::temp_dir().join(format!("kairos-codex-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("2026/11/19")).unwrap();
        let f = dir.join("2026/11/19/rollout-1.jsonl");
        std::fs::write(&f, format!("{META}\n{START}\n{CTX}\n{USER}\n{TOOL}\n")).unwrap();
        let mut src = Codex::new(vec![dir.clone()]);
        assert!(src.poll().is_empty());
        let mut text = std::fs::read_to_string(&f).unwrap();
        text.push_str(&format!(
            "{DONE}\n{START}\n{CTX}\n{}\n{}\n",
            USER.replace("10:00:01", "10:05:00"),
            TOOL.replace("10:00:05", "10:05:05")
        ));
        std::fs::write(&f, &text).unwrap();
        let out = src.poll();
        assert_eq!(names(&out), ["prompt", "agent_start"]);
        assert!(matches!(&out[0].kind, Kind::Prompt { session, .. } if session == "s1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn rollouts(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kairos-codex-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sessions/2026/11/19")).unwrap();
        std::fs::create_dir_all(dir.join("archived_sessions")).unwrap();
        dir
    }

    const ROLLOUT: &str = "sessions/2026/11/19/rollout-2026-11-19T09-59-59-s1.jsonl";
    const ARCHIVED: &str = "archived_sessions/rollout-2026-11-19T09-59-59-s1.jsonl";

    fn first_turn() -> String {
        format!("{META}\n{START}\n{CTX}\n{USER}\n{SAY}\n{TOOL}\n{FINAL}\n{DONE}\n")
    }

    fn second_turn(minute: &str) -> String {
        [START, CTX, USER, TOOL].iter().map(|l| format!("{}\n", l.replacen("T10:00:", &format!("T10:{minute}:"), 1))).collect()
    }

    #[test]
    fn a_thread_codex_writes_out_again_is_reported_once() {
        let dir = rollouts("again");
        let f = dir.join(ROLLOUT);
        let mut src = Codex::new(vec![dir.join("sessions")]);
        assert!(src.poll().is_empty());
        std::fs::write(&f, first_turn()).unwrap();
        assert_eq!(names(&src.poll()), ["prompt", "agent_start", "reply_done"], "a new rollout, its lines older than the last poll");
        // Rewritten shorter where it is, like Codex migrating an old rollout.
        std::fs::write(&f, first_turn().replace(&format!("{SAY}\n"), "")).unwrap();
        assert!(src.poll().is_empty());
        // Compressed after a week, put back when the thread resumes.
        let zst = f.with_extension("jsonl.zst");
        std::fs::rename(&f, &zst).unwrap();
        assert!(src.poll().is_empty());
        std::fs::rename(&zst, &f).unwrap();
        std::fs::write(&f, first_turn() + &second_turn("05")).unwrap();
        let out = src.poll();
        assert_eq!(names(&out), ["prompt", "agent_start"]);
        assert_eq!(out[0].ts - ts_of(USER), 300.0);
        // Archived, then opened again: Codex moves it back with a new time.
        std::fs::rename(&f, dir.join(ARCHIVED)).unwrap();
        assert!(src.poll().is_empty());
        std::fs::rename(dir.join(ARCHIVED), &f).unwrap();
        assert!(src.poll().is_empty());
        std::fs::write(&f, first_turn() + &second_turn("05") + &second_turn("07")).unwrap();
        assert_eq!(names(&src.poll()), ["reply_done", "prompt", "agent_start"], "the 10:05 turn ends once, when the next one starts");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ts_of(line: &str) -> f64 {
        timestamp(&serde_json::from_str(line).unwrap()).unwrap()
    }

    #[test]
    fn rollouts_read_before_threads_were_kept_are_not_reported_again() {
        let dir = rollouts("update");
        let f = dir.join(ROLLOUT);
        std::fs::write(&f, first_turn()).unwrap();
        let len = std::fs::metadata(&f).unwrap().len();
        let since = ts_of(r#"{"timestamp":"2026-11-19T10:02:00Z"}"#);
        let mut src = Codex::new(vec![dir.join("sessions")]);
        src.restore(&json!({ "since": since, "files": { f.to_string_lossy().to_string(): len } }));
        let shorter = first_turn().replace(&format!("{SAY}\n"), "");
        std::fs::write(&f, &shorter).unwrap();
        assert!(src.poll().is_empty());
        std::fs::write(&f, shorter.clone() + &second_turn("05")).unwrap();
        assert_eq!(names(&src.poll()), ["prompt", "agent_start"]);
        // And what's kept survives a restart.
        let mut again = Codex::new(vec![dir.join("sessions")]);
        again.restore(&src.state());
        std::fs::write(&f, &shorter).unwrap();
        assert!(again.poll().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_thread_archived_before_threads_were_kept_is_not_reported_again() {
        let dir = rollouts("archived");
        std::fs::write(dir.join(ARCHIVED), first_turn()).unwrap();
        let since = ts_of(r#"{"timestamp":"2026-11-19T10:02:00Z"}"#);
        let mut src = Codex::new(vec![dir.join("sessions")]);
        src.restore(&json!({ "since": since, "files": {} }));
        let f = dir.join(ROLLOUT);
        std::fs::rename(dir.join(ARCHIVED), &f).unwrap();
        assert!(src.poll().is_empty());
        std::fs::write(&f, first_turn() + &second_turn("05")).unwrap();
        assert_eq!(names(&src.poll()), ["prompt", "agent_start"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn item(item: &str) -> String {
        format!(r#"{{"timestamp":"2026-11-19T10:00:07Z","type":"event_msg","payload":{{"type":"item_completed","item":{item}}}}}"#)
    }

    fn event(payload: &str) -> String {
        format!(r#"{{"timestamp":"2026-11-19T10:00:07Z","type":"event_msg","payload":{payload}}}"#)
    }

    fn edits(out: &[Found]) -> Vec<(String, u32, u32)> {
        let mut v: Vec<_> = out
            .iter()
            .filter_map(|f| match &f.kind {
                Kind::AgentEdit { path, added, removed, .. } => Some((path.clone(), *added, *removed)),
                _ => None,
            })
            .collect();
        v.sort();
        v
    }

    fn commits(out: &[Found]) -> Vec<(String, Option<String>)> {
        out.iter()
            .filter_map(|f| match &f.kind {
                Kind::AgentCommit { sha, .. } => Some((sha.clone().unwrap_or_default(), f.cwd.as_ref().map(|c| c.to_string_lossy().replace('\\', "/")))),
                _ => None,
            })
            .collect()
    }

    const CHANGES: &str = r#""changes":{"/w/p/a.rs":{"type":"update","unified_diff":"@@ -1,3 +1,3 @@\n keep\n-old\n+new\n keep\n@@ -9 +9,2 @@\n x\n+y\n","move_path":null},"/w/p/new.rs":{"type":"add","content":"a\nb\n"},"/w/p/gone.rs":{"type":"delete","content":"1\n2\n3\n"}}"#;

    #[test]
    fn applied_patches_count_lines_per_file() {
        let done = item(&format!(r#"{{"id":"fc1","type":"FileChange","status":"completed",{CHANGES},"stdout":"Success","stderr":""}}"#));
        let out = run(&[META, START, CTX, USER, SAY, TOOL, &done, FINAL, DONE]);
        assert_eq!(edits(&out), [("/w/p/a.rs".into(), 2, 1), ("/w/p/gone.rs".into(), 0, 3), ("/w/p/new.rs".into(), 2, 0)]);
        assert!(out.iter().filter(|f| f.kind.name() == "agent_edit").all(|f| f.cwd.as_deref() == Some(Path::new("/w/p"))));
        assert!(out.iter().any(|f| matches!(&f.kind, Kind::AgentEdit { session, .. } if session == "s1")));
        let rest: Vec<&str> = out.iter().filter(|f| f.kind.name() != "agent_edit").map(|f| f.kind.name()).collect();
        assert_eq!(rest, ["prompt", "agent_start", "reply_done"]);

        let moved = item(r#"{"id":"fc2","type":"FileChange","status":"completed","changes":{"/w/p/old.rs":{"type":"update","unified_diff":"@@ -1 +1 @@\n-a\n+b\n","move_path":"/w/p/renamed.rs"}}}"#);
        assert_eq!(edits(&run(&[META, START, CTX, USER, &moved])), [("/w/p/renamed.rs".into(), 1, 1)]);
    }

    #[test]
    fn rejected_and_failed_patches_make_nothing() {
        for status in ["failed", "declined"] {
            let bad = item(&format!(r#"{{"id":"fc1","type":"FileChange","status":"{status}",{CHANGES}}}"#));
            assert!(edits(&run(&[META, START, CTX, USER, &bad])).is_empty(), "{status}");
        }
        let legacy = |success: bool| event(&format!(r#"{{"type":"patch_apply_end","call_id":"c1","success":{success},"stdout":"","stderr":"",{CHANGES}}}"#));
        assert!(edits(&run(&[META, START, CTX, USER, &legacy(false)])).is_empty());
        assert_eq!(edits(&run(&[META, START, CTX, USER, &legacy(true)])).len(), 3);
    }

    #[test]
    fn one_patch_reported_twice_counts_once() {
        let a = item(&format!(r#"{{"id":"c1","type":"FileChange","status":"completed",{CHANGES}}}"#));
        let b = event(&format!(r#"{{"type":"patch_apply_end","call_id":"c1","success":true,{CHANGES}}}"#));
        assert_eq!(edits(&run(&[META, START, CTX, USER, &a, &b])).len(), 3);
    }

    #[test]
    fn a_commit_is_a_shell_command_whose_output_printed_a_sha() {
        let cmd = |id: &str, script: &str, out: &str, code: i32| {
            item(&format!(
                r#"{{"id":"{id}","type":"CommandExecution","command":["bash","-lc",{script:?}],"cwd":"/w/q","aggregated_output":{out:?},"exit_code":{code},"status":"completed"}}"#
            ))
        };
        let ok = cmd("k1", "git add -A && git commit -m 'add x'", "[main abc1234] add x\n 1 file changed\n", 0);
        let out = run(&[META, START, CTX, USER, &ok]);
        assert_eq!(commits(&out), [("abc1234".into(), Some("/w/q".into()))]);
        assert!(matches!(&out[1].kind, Kind::AgentCommit { session, .. } if session == "s1"));

        let status = cmd("k2", "git status --short", "[main abc1234] stray\n", 0);
        let empty = cmd("k3", "git commit -m x", "nothing to commit, working tree clean\n", 1);
        assert!(commits(&run(&[META, START, CTX, USER, &status, &empty])).is_empty());

        let legacy = event(r#"{"type":"exec_command_end","call_id":"k4","command":["git","commit","-m","two words"],"cwd":"/w/p","aggregated_output":"[main (root-commit) 0123abc] two words\n","exit_code":0}"#);
        assert_eq!(commits(&run(&[META, START, CTX, USER, &legacy])), [("0123abc".into(), Some("/w/p".into()))]);
        assert_eq!(commits(&run(&[META, START, CTX, USER, &ok, &ok])).len(), 1);

        let uri = ok.replace(r#""cwd":"/w/q""#, r#""cwd":"file:///w/my%20q""#).replace("k1", "k5");
        assert_eq!(commits(&run(&[META, START, CTX, USER, &uri])), [("abc1234".into(), Some("/w/my q".into()))]);
    }

    #[test]
    fn subagent_and_exec_rollouts_count_for_their_work_only() {
        let done = item(&format!(r#"{{"id":"fc1","type":"FileChange","status":"completed",{CHANGES}}}"#));
        let child = META.replace(r#""thread_source":"user""#, r#""thread_source":"subagent""#);
        let out = run(&[&child, START, CTX, USER, TOOL, &done, DONE]);
        assert_eq!(names(&out), ["agent_edit", "agent_edit", "agent_edit"]);
        let exec = META.replace(r#""source":"cli""#, r#""source":"exec""#);
        assert_eq!(edits(&run(&[&exec, START, CTX, USER, TOOL, &done, DONE])).len(), 3);
    }

    const ASK: &str = r#"{"timestamp":"2026-11-19T10:00:10Z","type":"response_item","payload":{"type":"function_call","name":"request_user_input","arguments":"{\"questions\":[{\"header\":\"Scope\",\"id\":\"scope\",\"question\":\"Which parser should I fix first?\",\"options\":[{\"label\":\"Lexer\",\"description\":\"Fix the lexer\"}]}]}","call_id":"c1"}}"#;
    const ANSWER: &str = r#"{"timestamp":"2026-11-19T10:00:40Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"Lexer first please"}}"#;

    #[test]
    fn request_user_input_answered_is_a_question() {
        let out = run(&[META, START, CTX, USER, ASK, ANSWER, FINAL, DONE]);
        let got: Vec<&Found> = out.iter().filter(|f| f.kind.name() == "answered").collect();
        assert_eq!(got.len(), 1);
        let asked = crate::parse::timestamp(&serde_json::from_str::<Value>(ASK).unwrap()).unwrap();
        assert_eq!(got[0].ts - asked, 30.0);
        assert_eq!(got[0].kind, Kind::Answered { session: "s1".into(), asked, why: Ask::Question, shown_words: 12, answer_words: 3 });
        assert_eq!(names(&out), ["prompt", "agent_start", "answered", "reply_done"]);

        let other_call = ANSWER.replace("c1", "c2");
        assert!(!names(&run(&[META, START, CTX, USER, ASK, &other_call, DONE])).contains(&"answered"));
        let shell = ASK.replace("request_user_input", "shell_command");
        assert!(!names(&run(&[META, START, CTX, USER, &shell, ANSWER, DONE])).contains(&"answered"));
    }

    #[test]
    fn exec_sessions_make_no_answers() {
        let exec = META.replace(r#""source":"cli""#, r#""source":"exec""#);
        assert!(!names(&run(&[&exec, START, CTX, USER, ASK, ANSWER, DONE])).contains(&"answered"));
    }
}
