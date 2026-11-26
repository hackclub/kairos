//! A model API that plays the scenario instead of thinking: each request of the armed turn gets the next scripted
//! tool call, then the closing text. Tool call ids carry the turn and step, so a retried request gets the same answer.
//! Two sessions at once each play their own scenario, told apart by the project folder their requests name.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::scenario::{Act, Scenario};

struct Lane {
    scenario: Arc<Scenario>,
    project: PathBuf,
    turn: Option<usize>,
    said: Vec<bool>,
    /// The turn's last request is being held open, for the person to press Esc.
    stalled: Vec<bool>,
}

struct State {
    lanes: Vec<Lane>,
    requests: Vec<Value>,
    problems: Vec<String>,
}

/// One session's scenario on the model server.
#[derive(Clone)]
pub struct Model {
    pub port: u16,
    state: Arc<Mutex<State>>,
    lane: usize,
}

enum Plan {
    Side,
    Act(String, Act),
    Say(String),
    /// No answer comes until the person gives up on it.
    Stall,
}

impl Model {
    pub fn start(scenario: Arc<Scenario>, project: PathBuf) -> Result<Model> {
        let server = Server::http("127.0.0.1:0").map_err(|e| anyhow!("{e}"))?;
        let port = server.server_addr().to_ip().ok_or_else(|| anyhow!("the model server has no port"))?.port();
        let state = Arc::new(Mutex::new(State { lanes: Vec::new(), requests: Vec::new(), problems: Vec::new() }));
        let shared = state.clone();
        thread::spawn(move || {
            for req in server.incoming_requests() {
                let state = shared.clone();
                thread::spawn(move || handle(&state, req));
            }
        });
        Model { port, state, lane: 0 }.add(scenario, project)
    }

    /// A second session on the same server, working in another project.
    pub fn add(&self, scenario: Arc<Scenario>, project: PathBuf) -> Result<Model> {
        let (said, stalled) = (vec![false; scenario.turns.len()], vec![false; scenario.turns.len()]);
        let mut s = self.state.lock().unwrap();
        s.lanes.push(Lane { scenario, project, turn: None, said, stalled });
        Ok(Model { port: self.port, state: self.state.clone(), lane: s.lanes.len() - 1 })
    }

    pub fn arm(&self, turn: usize) {
        self.state.lock().unwrap().lanes[self.lane].turn = Some(turn);
    }

    pub fn said(&self, turn: usize) -> bool {
        self.state.lock().unwrap().lanes[self.lane].said[turn]
    }

    pub fn stalled(&self, turn: usize) -> bool {
        self.state.lock().unwrap().lanes[self.lane].stalled[turn]
    }

    pub fn problems(&self) -> Vec<String> {
        self.state.lock().unwrap().problems.clone()
    }

    pub fn requests(&self) -> Vec<Value> {
        self.state.lock().unwrap().requests.clone()
    }
}

fn pos(id: &str) -> Option<(usize, usize)> {
    let (turn, step) = id.rsplit_once("lab_")?.1.split_once('_')?;
    Some((turn.parse().ok()?, step.parse().ok()?))
}

/// The session a request is from: the one whose project folder it names (the longest, since one folder's name can
/// start another's), else the only one, else the only one waiting on an answer.
fn route(state: &Mutex<State>, raw: &str) -> Option<usize> {
    let s = state.lock().unwrap();
    let raw = if cfg!(windows) { raw.to_lowercase() } else { raw.to_string() };
    let named = s.lanes.iter().enumerate().filter_map(|(i, l)| {
        let p = l.project.to_string_lossy();
        let p = if cfg!(windows) { p.to_lowercase() } else { p.into_owned() };
        let forms = [p.clone(), p.replace('\\', "/"), p.replace('\\', "\\\\")];
        forms.iter().any(|f| raw.contains(f.as_str())).then_some((p.len(), i))
    });
    if let Some((_, i)) = named.max() {
        return Some(i);
    }
    if s.lanes.len() == 1 {
        return Some(0);
    }
    let waiting: Vec<usize> = (0..s.lanes.len()).filter(|&i| s.lanes[i].turn.is_some_and(|t| !s.lanes[i].said[t])).collect();
    match waiting.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

fn project(state: &Mutex<State>, lane: Option<usize>) -> PathBuf {
    state.lock().unwrap().lanes[lane.unwrap_or(0)].project.clone()
}

fn plan(state: &Mutex<State>, lane: Option<usize>, main: bool, answered: &[String]) -> Plan {
    let mut s = state.lock().unwrap();
    let Some(lane) = lane else { return Plan::Side };
    let Some(turn) = s.lanes[lane].turn.filter(|_| main) else { return Plan::Side };
    let step = answered.iter().filter_map(|id| pos(id)).filter(|p| p.0 == turn).map(|p| p.1 + 1).max().unwrap_or(0);
    let scenario = s.lanes[lane].scenario.clone();
    match scenario.turns[turn].acts.get(step) {
        Some(act) => Plan::Act(format!("lab_{turn}_{step}"), *act),
        None if scenario.turns[turn].interrupt => {
            s.lanes[lane].stalled[turn] = true;
            Plan::Stall
        }
        None => {
            drop(s);
            thread::sleep(Duration::from_secs(scenario.turns[turn].wait));
            state.lock().unwrap().lanes[lane].said[turn] = true;
            Plan::Say(scenario.turns[turn].say.to_string())
        }
    }
}

fn names(tools: &[Value]) -> Vec<String> {
    tools.iter().map(|t| t["name"].as_str().or(t["function"]["name"].as_str()).or(t["type"].as_str()).unwrap_or("?").to_string()).collect()
}

fn sse(events: &[(&str, Value)]) -> String {
    events.iter().map(|(name, data)| format!("event: {name}\ndata: {data}\n\n")).collect()
}

fn anthropic(state: &Mutex<State>, lane: Option<usize>, body: &Value) -> (&'static str, String, String) {
    let tools = body["tools"].as_array().cloned().unwrap_or_default();
    let offered = names(&tools);
    let main = ["Write", "Edit", "Bash"].iter().all(|n| offered.iter().any(|o| o == n));
    let answered: Vec<String> = body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(|b| b["type"] == "tool_result")
        .filter_map(|b| b["tool_use_id"].as_str().map(str::to_string))
        .collect();
    let project = project(state, lane);
    let file = |p: &str| project.join(p).to_string_lossy().into_owned();
    let (note, block) = match plan(state, lane, main, &answered) {
        Plan::Side => ("side".to_string(), json!({ "type": "text", "text": "ok" })),
        Plan::Stall => ("stall".to_string(), json!({ "type": "text", "text": "ok" })),
        Plan::Say(text) => ("say".to_string(), json!({ "type": "text", "text": text })),
        Plan::Act(id, act) => {
            let (name, input) = match act {
                Act::Write { path, content } => ("Write", json!({ "file_path": file(path), "content": content })),
                Act::Edit { path, old, new } => ("Edit", json!({ "file_path": file(path), "old_string": old, "new_string": new })),
                Act::Shell { cmd } | Act::Asked { cmd } => ("Bash", json!({ "command": cmd, "description": "Run the scripted command" })),
            };
            (id.clone(), json!({ "type": "tool_use", "id": format!("toolu_{id}"), "name": name, "input": input }))
        }
    };
    let text = block["type"] == "text";
    let stop = if text { "end_turn" } else { "tool_use" };
    let message = json!({
        "id": format!("msg_{note}"), "type": "message", "role": "assistant", "model": body["model"], "content": [], "stop_reason": null,
        "stop_sequence": null, "usage": { "input_tokens": 10, "output_tokens": 1, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0 }
    });
    if body["stream"] != true {
        let mut full = message;
        full["content"] = json!([block]);
        full["stop_reason"] = json!(stop);
        return ("application/json", full.to_string(), note);
    }
    let (start, delta) = if text {
        (json!({ "type": "text", "text": "" }), json!({ "type": "text_delta", "text": block["text"] }))
    } else {
        let empty = json!({ "type": "tool_use", "id": block["id"], "name": block["name"], "input": {} });
        (empty, json!({ "type": "input_json_delta", "partial_json": block["input"].to_string() }))
    };
    let out = sse(&[
        ("message_start", json!({ "type": "message_start", "message": message })),
        ("content_block_start", json!({ "type": "content_block_start", "index": 0, "content_block": start })),
        ("content_block_delta", json!({ "type": "content_block_delta", "index": 0, "delta": delta })),
        ("content_block_stop", json!({ "type": "content_block_stop", "index": 0 })),
        ("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": stop, "stop_sequence": null }, "usage": { "output_tokens": 20 } })),
        ("message_stop", json!({ "type": "message_stop" })),
    ]);
    ("text/event-stream", out, note)
}

fn patch_lines(sign: char, text: &str) -> String {
    text.lines().map(|l| format!("{sign}{l}\n")).collect()
}

/// Code mode (Codex with GPT-6 models): the only tool is `exec`, JavaScript that calls the real tools on `tools`.
fn code_mode(tools: &[Value]) -> bool {
    tools.iter().any(|t| t["type"] == "custom" && t["name"] == "exec")
}

fn exec_item(call_id: &str, js: String) -> Value {
    json!({ "type": "custom_tool_call", "name": "exec", "input": js, "call_id": call_id })
}

/// A shell call in whichever form this version offers, with the arguments its own schema names.
fn shell_item(tools: &[Value], call_id: &str, cmd: &str, project: &Path) -> Option<Value> {
    let dir = project.to_string_lossy();
    if code_mode(tools) {
        let args = json!({ "cmd": cmd, "workdir": dir });
        return Some(exec_item(call_id, format!("const r = await tools.exec_command({args});\ntext(r);\n")));
    }
    if tools.iter().any(|t| t["type"] == "local_shell") {
        let action = json!({ "type": "exec", "command": ["bash", "-lc", cmd], "working_directory": dir });
        return Some(json!({ "type": "local_shell_call", "call_id": call_id, "status": "completed", "action": action }));
    }
    let tool = ["exec_command", "shell_command", "shell"].iter().find_map(|n| tools.iter().find(|t| t["name"] == *n))?;
    let props = &tool["parameters"]["properties"];
    let mut args = json!({});
    if props["command"]["type"] == "array" {
        args["command"] = json!(["bash", "-lc", cmd]);
    } else if props.get("command").is_some() {
        args["command"] = json!(cmd);
    } else if props.get("cmd").is_some() {
        args["cmd"] = json!(cmd);
    } else {
        return None;
    }
    if props.get("workdir").is_some() {
        args["workdir"] = json!(dir);
    }
    Some(json!({ "type": "function_call", "name": tool["name"], "arguments": args.to_string(), "call_id": call_id }))
}

fn patch_item(tools: &[Value], call_id: &str, patch: &str, project: &Path) -> Option<Value> {
    if code_mode(tools) {
        return Some(exec_item(call_id, format!("const r = await tools.apply_patch({});\ntext(r);\n", json!(patch))));
    }
    match tools.iter().find(|t| t["name"] == "apply_patch") {
        Some(t) if t["type"] == "custom" => Some(json!({ "type": "custom_tool_call", "name": "apply_patch", "input": patch, "call_id": call_id })),
        Some(_) => Some(json!({ "type": "function_call", "name": "apply_patch", "arguments": json!({ "input": patch }).to_string(), "call_id": call_id })),
        None => shell_item(tools, call_id, &format!("apply_patch <<'EOF'\n{patch}EOF"), project),
    }
}

/// Every tool a request offers: the `tools` list, and since code mode also `additional_tools` in the input, grouped in
/// namespaces.
fn offered(body: &Value) -> Vec<Value> {
    fn flat(list: &[Value], out: &mut Vec<Value>) {
        for t in list {
            match t["tools"].as_array() {
                Some(inner) if t["type"] == "namespace" => flat(inner, out),
                _ => out.push(t.clone()),
            }
        }
    }
    let mut out = Vec::new();
    flat(body["tools"].as_array().map(Vec::as_slice).unwrap_or_default(), &mut out);
    for i in body["input"].as_array().into_iter().flatten().filter(|i| i["type"] == "additional_tools") {
        flat(i["tools"].as_array().map(Vec::as_slice).unwrap_or_default(), &mut out);
    }
    out
}

/// `side`: a job the app runs on the side, like naming a thread, which offers the same tools.
fn responses(state: &Mutex<State>, lane: Option<usize>, body: &Value, side: bool) -> (&'static str, String, String) {
    let tools = offered(body);
    let input = body["input"].as_array().cloned().unwrap_or_default();
    let main = !side && !tools.is_empty() && !input.is_empty();
    let answered: Vec<String> = input
        .iter()
        .filter(|i| i["type"].as_str().is_some_and(|t| t.ends_with("_output")))
        .filter_map(|i| i["call_id"].as_str().map(str::to_string))
        .collect();
    let project = project(state, lane);
    let message = |text: &str| json!({ "type": "message", "role": "assistant", "id": "msg_lab", "content": [{ "type": "output_text", "text": text }] });
    let (note, item) = match plan(state, lane, main, &answered) {
        Plan::Side => ("side".to_string(), message("ok")),
        Plan::Stall => ("stall".to_string(), message("ok")),
        Plan::Say(text) => ("say".to_string(), message(&text)),
        Plan::Act(id, act) => {
            let call_id = format!("call_{id}");
            let item = match act {
                Act::Write { path, content } => {
                    let patch = format!("*** Begin Patch\n*** Add File: {path}\n{}*** End Patch\n", patch_lines('+', content));
                    patch_item(&tools, &call_id, &patch, &project)
                }
                Act::Edit { path, old, new } => {
                    let patch = format!("*** Begin Patch\n*** Update File: {path}\n@@\n{}{}*** End Patch\n", patch_lines('-', old), patch_lines('+', new));
                    patch_item(&tools, &call_id, &patch, &project)
                }
                Act::Shell { cmd } | Act::Asked { cmd } => shell_item(&tools, &call_id, cmd, &project),
            };
            match item {
                Some(item) => (id, item),
                None => {
                    let problem = format!("step {id}: this version offers no tool the lab knows for it: {:?}", names(&tools));
                    state.lock().unwrap().problems.push(problem);
                    (id, message("The lab has no tool for this step."))
                }
            }
        }
    };
    let usage = json!({
        "input_tokens": 10, "input_tokens_details": { "cached_tokens": 0 }, "output_tokens": 20,
        "output_tokens_details": { "reasoning_tokens": 0 }, "total_tokens": 30
    });
    let out = sse(&[
        ("response.created", json!({ "type": "response.created", "response": { "id": "resp_lab" } })),
        ("response.output_item.done", json!({ "type": "response.output_item.done", "output_index": 0, "item": item })),
        ("response.completed", json!({ "type": "response.completed", "response": { "id": "resp_lab", "usage": usage } })),
    ]);
    ("text/event-stream", out, note)
}

/// A chat completions tool call, by the tool and argument names this version offers (OpenCode 1.x says `bash` and
/// `filePath`, agy `run_command` and `Path`).
fn chat_call(tools: &[Value], act: Act, project: &Path) -> Option<(Value, Value)> {
    let find = |names: &[&str]| names.iter().find_map(|n| tools.iter().find(|t| t["name"] == *n));
    let key = |tool: &Value, names: &[&str]| names.iter().find(|n| tool["parameters"]["properties"].get(**n).is_some()).map(|n| n.to_string());
    let file = |path: &str| json!(project.join(path).to_string_lossy());
    const PATH: &[&str] = &["path", "filePath", "Path"];
    let (tool, wants): (&Value, Vec<(&[&str], Value)>) = match act {
        Act::Write { path, content } => (find(&["write", "write_file", "create"])?, vec![(PATH, file(path)), (&["content", "Content", "file_text"], json!(content))]),
        Act::Edit { path, old, new } => (
            find(&["edit", "edit_file"])?,
            vec![(PATH, file(path)), (&["oldString", "Target", "old_str"], json!(old)), (&["newString", "Replacement", "new_str"], json!(new))],
        ),
        Act::Shell { cmd } | Act::Asked { cmd } => (find(&["shell", "bash", "powershell", "run_command"])?, vec![(&["command", "CommandLine"], json!(cmd))]),
    };
    // Copilot CLI's `create` won't make a missing folder, where a model would first run mkdir.
    if let (Act::Write { path, .. }, true) = (act, tool["name"] == "create") {
        if let Some(dir) = project.join(path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
    }
    let mut args = json!({});
    for (names, value) in wants {
        args[key(tool, names).unwrap_or_else(|| names[0].to_string())] = value;
    }
    // agy turns its command away without a summary and a wait before it goes to the background.
    for (name, value) in [("description", json!("Run the scripted command")), ("toolSummary", json!("Run the scripted command")), ("WaitMsBeforeAsync", json!(30000))] {
        if key(tool, &[name]).is_some() {
            args[name] = value;
        }
    }
    Some((tool["name"].clone(), args))
}

fn chat(state: &Mutex<State>, lane: Option<usize>, body: &Value) -> (&'static str, String, String) {
    let tools: Vec<Value> = body["tools"].as_array().into_iter().flatten().map(|t| t["function"].clone()).collect();
    let answered: Vec<String> = body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["tool_call_id"].as_str().map(str::to_string))
        .collect();
    let project = project(state, lane);
    let text = |t: &str| (json!({ "role": "assistant", "content": t }), "stop");
    let (note, (delta, finish)) = match plan(state, lane, !tools.is_empty(), &answered) {
        Plan::Side => ("side".to_string(), text("ok")),
        Plan::Stall => ("stall".to_string(), text("ok")),
        Plan::Say(t) => ("say".to_string(), text(&t)),
        Plan::Act(id, act) => match chat_call(&tools, act, &project) {
            Some((name, args)) => {
                let call = json!({ "index": 0, "id": format!("call_{id}"), "type": "function", "function": { "name": name, "arguments": args.to_string() } });
                (id, (json!({ "role": "assistant", "content": null, "tool_calls": [call] }), "tool_calls"))
            }
            None => {
                let problem = format!("step {id}: this version offers no tool the lab knows for it: {:?}", names(&tools));
                state.lock().unwrap().problems.push(problem);
                (id, text("The lab has no tool for this step."))
            }
        },
    };
    let usage = json!({ "prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30 });
    if body["stream"] != true {
        let choices = json!([{ "index": 0, "message": delta, "finish_reason": finish }]);
        let full = json!({ "id": "chatcmpl-lab", "object": "chat.completion", "created": 0, "model": body["model"], "choices": choices, "usage": usage });
        return ("application/json", full.to_string(), note);
    }
    let chunk = |choices: Value| json!({ "id": "chatcmpl-lab", "object": "chat.completion.chunk", "created": 0, "model": body["model"], "choices": choices });
    let first = chunk(json!([{ "index": 0, "delta": delta, "finish_reason": null }]));
    let last = chunk(json!([{ "index": 0, "delta": {}, "finish_reason": finish }]));
    let mut tail = chunk(json!([]));
    tail["usage"] = usage;
    ("text/event-stream", format!("data: {first}\n\ndata: {last}\n\ndata: {tail}\n\ndata: [DONE]\n\n"), note)
}

fn handle(state: &Mutex<State>, mut req: Request) {
    let mut raw = String::new();
    let _ = req.as_reader().read_to_string(&mut raw);
    let body: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let url = req.url().to_string();
    // For looking at what a new harness sends: every request whole, one per line.
    if let Some(f) = std::env::var_os("LAB_DUMP") {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(f) {
            // The runs' artifacts can be downloaded, so no keys, not even the lab's own.
            let headers: Vec<String> = req
                .headers()
                .iter()
                .filter(|h| !["authorization", "cookie", "x-api-key"].iter().any(|k| h.field.equiv(*k)))
                .map(|h| format!("{}: {}", h.field, h.value))
                .collect();
            let _ = writeln!(f, "{}", json!({ "path": url, "headers": headers, "body": raw }));
        }
    }
    let path = url.split('?').next().unwrap_or_default();
    let lane = route(state, &raw);
    let (ctype, out, note) = if *req.method() != Method::Post {
        ("application/json", json!({ "object": "list", "data": [] }).to_string(), "get".to_string())
    } else if path.ends_with("/messages/count_tokens") {
        ("application/json", json!({ "input_tokens": 42 }).to_string(), "count".to_string())
    } else if path.ends_with("/messages") {
        anthropic(state, lane, &body)
    } else if path.ends_with("/responses") {
        // T3 Code names threads and writes commit messages with `codex exec`, which the lab never runs as the harness.
        let side = req.headers().iter().any(|h| h.field.equiv("User-Agent") && h.value.as_str().contains("codex_exec"));
        responses(state, lane, &body, side)
    } else if path.ends_with("/chat/completions") {
        chat(state, lane, &body)
    } else {
        ("application/json", "{}".to_string(), "unknown".to_string())
    };
    let tools = names(&offered(&body));
    let record = json!({ "method": req.method().as_str(), "path": url, "model": body["model"], "stream": body["stream"], "tools": tools, "lane": lane, "reply": note });
    state.lock().unwrap().requests.push(record);
    if note.starts_with("lab_") || note == "say" {
        thread::sleep(Duration::from_millis(1000));
    }
    if note == "stall" {
        // Longer than the lab waits for Esc, and the harness drops the request when it is pressed.
        thread::sleep(Duration::from_secs(120));
    }
    let header = Header::from_bytes("Content-Type", ctype).expect("a plain header");
    let _ = req.respond(Response::from_string(out).with_header(header));
}
