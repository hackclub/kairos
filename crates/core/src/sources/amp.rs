use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{snapshot, uri_path, work, words, Found, Source, Tail};
use crate::event::Kind;
use crate::{exclude, paths, prompt_shape, prompt_stats};

const PLUGIN: &str = include_str!("amp_plugin.ts");
const PLUGIN_FILE: &str = "kairos.ts";
const PLUGIN_CHECK_SECS: f64 = 60.0;
const KEEP_DAYS: i64 = 14;

// Amp keeps its threads on its servers, so nothing on disk says when a prompt was sent. Kairos adds a plugin to Amp
// that hands each moment to `kairos amp-event`, which writes the facts here, a file a day.
pub fn default_dir() -> PathBuf {
    paths::data_dir().join("amp")
}

fn config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .map(|c| c.join("amp"))
}

// The installed `kairos` next to this program, which is what the plugin runs. None for a build run from anywhere else.
pub fn installed_cli() -> Option<PathBuf> {
    let me = std::env::current_exe().ok()?;
    let dir = me.parent()?;
    let cli = dir.join(if cfg!(windows) { "kairos.exe" } else { "kairos" });
    (Some(dir) == paths::install_dir().as_deref() && cli.is_file()).then_some(cli)
}

fn plugin_text(cli: &Path) -> String {
    let quoted = serde_json::to_string(&cli.to_string_lossy()).unwrap_or_default();
    PLUGIN.replace("'__KAIROS__'", &quoted)
}

// Only where Amp has been run, and a running Amp loads it the next time it starts.
fn add_plugin(config: &Path, cli: &Path) {
    if !config.is_dir() {
        return;
    }
    let path = config.join("plugins").join(PLUGIN_FILE);
    let text = plugin_text(cli);
    if fs::read_to_string(&path).is_ok_and(|old| old == text) {
        return;
    }
    if fs::create_dir_all(config.join("plugins")).is_ok() {
        let _ = fs::write(path, text);
    }
}

pub fn remove_plugin() {
    if let Some(config) = config_dir() {
        let _ = fs::remove_file(config.join("plugins").join(PLUGIN_FILE));
    }
}

#[derive(Serialize, Deserialize)]
struct Line {
    ts: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    event: Kind,
}

fn tool(ts: f64, session: &str, root: Option<&Path>, v: &Value, out: &mut Vec<Found>) {
    let input = |k: &str| v.get("input").and_then(|i| i.get(k)).and_then(Value::as_str);
    let output = |k: &str| v.get("output").and_then(|o| o.get(k));
    match v.get("tool").and_then(Value::as_str) {
        Some("apply_patch") => {
            for f in output("files").and_then(Value::as_array).into_iter().flatten() {
                let Some(path) = f.get("uri").and_then(Value::as_str).and_then(uri_path) else { continue };
                let n = |k: &str| f.get(k).and_then(Value::as_u64).map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX));
                work::edit(ts, root, session, &path.to_string_lossy(), (n("additions"), n("deletions")), out);
            }
        }
        Some("edit_file") => {
            if let (Some(path), Some(diff)) = (input("path"), output("diff").and_then(Value::as_str)) {
                work::edit(ts, root, session, path, work::patch_counts(diff), out);
            }
        }
        Some("shell_command") => {
            let failed = output("exitCode").and_then(Value::as_i64).is_some_and(|c| c != 0);
            if let (Some(command), false) = (input("command"), failed) {
                let printed = output("output").and_then(Value::as_str).unwrap_or("");
                let dir = input("workdir").filter(|d| !d.is_empty()).map(Path::new).or(root);
                work::commits(ts, dir, session, command, printed, out);
            }
        }
        _ => {}
    }
}

// What one plugin message says. A `create_file` call doesn't say whether the file was there before, so its lines
// can't be counted and it only shows in the turn's files_changed.
fn facts(v: &Value) -> Vec<Found> {
    let mut out = Vec::new();
    let text = |k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    let (Some(ts), Some(session)) = (v.get("ts").and_then(Value::as_f64), text("thread")) else { return out };
    let root = text("root").and_then(uri_path);
    let count = |k: &str| v.get(k).and_then(Value::as_u64).map_or(0, |n| usize::try_from(n).unwrap_or(usize::MAX));
    match text("ev") {
        Some("prompt") => {
            let prompt = text("text").unwrap_or("");
            let (words, pasted) = prompt_stats(prompt);
            out.push(Found {
                ts,
                cwd: root,
                kind: Kind::Prompt {
                    words,
                    pasted,
                    model: None,
                    session: session.to_string(),
                    shape: Some(prompt_shape(prompt)),
                    version: text("version").map(str::to_string),
                    via: None,
                    permissions: None,
                },
            });
        }
        Some("start") => out.push(Found { ts, cwd: root, kind: Kind::AgentStart { session: session.to_string() } }),
        Some("tool") => tool(ts, session, root.as_deref(), v, &mut out),
        Some("end") => {
            let reply = v.get("reply").and_then(Value::as_array);
            out.push(Found {
                ts,
                cwd: root,
                kind: Kind::ReplyDone {
                    session: session.to_string(),
                    reply_words: reply.into_iter().flatten().filter_map(Value::as_str).map(words).sum(),
                    tools: count("tools"),
                    files_changed: count("files"),
                    model: text("model").map(str::to_string),
                },
            });
        }
        _ => {}
    }
    out
}

fn day(ts: f64) -> Option<chrono::NaiveDate> {
    chrono::DateTime::from_timestamp(ts as i64, 0).map(|t| t.date_naive())
}

fn forget_old(dir: &Path, today: chrono::NaiveDate) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name();
        let date = name.to_str().and_then(|n| n.strip_suffix(".jsonl")).and_then(|d| d.parse::<chrono::NaiveDate>().ok());
        if date.is_some_and(|d| (today - d).num_days() > KEEP_DAYS) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn record_in(dir: &Path, message: &str, excluded: impl Fn(&Path) -> bool) -> Option<()> {
    let found = facts(&serde_json::from_str(message).ok()?);
    let today = day(found.first()?.ts)?;
    let mut lines = String::new();
    for f in found {
        if f.cwd.as_deref().is_some_and(&excluded) {
            continue;
        }
        let line = Line { ts: f.ts, cwd: f.cwd.map(|c| c.to_string_lossy().into_owned()), event: f.kind };
        lines.push_str(&serde_json::to_string(&line).ok()?);
        lines.push('\n');
    }
    if lines.is_empty() {
        return None;
    }
    fs::create_dir_all(dir).ok()?;
    let path = dir.join(format!("{today}.jsonl"));
    if !path.exists() {
        forget_old(dir, today);
    }
    OpenOptions::new().create(true).append(true).open(path).ok()?.write_all(lines.as_bytes()).ok()
}

/// One message from the Amp plugin, as `kairos amp-event` gets it on stdin. Its text is counted and dropped here.
pub fn record(message: &str) {
    let data = paths::data_dir();
    let ex = exclude::load(&data).unwrap_or_default();
    let _ = record_in(&default_dir(), message, |p| ex.covers(p));
}

pub struct Amp {
    tail: Tail,
    plugin: Option<(PathBuf, PathBuf)>,
    checked: f64,
}

impl Amp {
    pub fn new(dir: PathBuf) -> Amp {
        Amp { tail: Tail::new(vec![dir], "jsonl", |_| false), plugin: None, checked: 0.0 }
    }

    /// Also keeps the plugin in Amp's config folder, pointed at `cli`.
    pub fn with_plugin(dir: PathBuf, cli: Option<PathBuf>) -> Amp {
        Amp { plugin: config_dir().zip(cli), ..Amp::new(dir) }
    }
}

impl Source for Amp {
    fn health(&mut self) -> super::Health {
        self.tail.health()
    }

    fn name(&self) -> &'static str {
        "amp"
    }

    fn poll(&mut self) -> Vec<Found> {
        let now = snapshot::now();
        if let Some((config, cli)) = self.plugin.as_ref().filter(|_| now - self.checked >= PLUGIN_CHECK_SECS) {
            self.checked = now;
            add_plugin(config, cli);
        }
        let mut out = Vec::new();
        self.tail.poll(|_, line| {
            if let Ok(l) = serde_json::from_str::<Line>(line) {
                out.push(Found { ts: l.ts, cwd: l.cwd.map(PathBuf::from), kind: l.event });
            }
        });
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

    const THREAD: &str = "T-01a20f60-c099-7319-8bd2-1e9395f51368";

    fn msg(body: &str) -> String {
        format!(r#"{{"ts":1795554335.236,"thread":"{THREAD}","root":"/w/probe",{body}}}"#)
    }

    fn summary(out: &[Found]) -> Vec<String> {
        out.iter()
            .map(|f| match &f.kind {
                Kind::AgentEdit { path, added, removed, .. } => format!("edit {path} +{added} -{removed}"),
                Kind::AgentCommit { sha, .. } => format!("commit {} in {}", sha.as_deref().unwrap_or("?"), f.cwd.as_deref().unwrap().display()),
                k => k.name().to_string(),
            })
            .collect()
    }

    fn of(body: &str) -> Vec<Found> {
        facts(&serde_json::from_str(&msg(body)).unwrap())
    }

    #[test]
    fn a_prompt_keeps_counts_and_never_text() {
        let out = of(r#""ev":"prompt","text":"fix the parser please\nit drops the last line","version":"0.0.1793857728-g655a2c""#);
        let [Found { ts, cwd, kind: Kind::Prompt { words, pasted, session, shape, version, model, .. } }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((*ts, cwd.as_deref()), (1795554335.236, Some(Path::new("/w/probe"))));
        assert_eq!((*words, *pasted, session.as_str(), model), (9, false, THREAD, &None));
        assert_eq!(shape.as_ref().unwrap().lines, 2);
        assert_eq!(version.as_deref(), Some("0.0.1793857728-g655a2c"));
        let dir = std::env::temp_dir().join(format!("kairos-amp-text-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        record_in(&dir, &msg(r#""ev":"prompt","text":"fix the parser please""#), |_| false).unwrap();
        let written = fs::read_to_string(dir.join("2026-11-24.jsonl")).unwrap();
        assert!(written.contains(r#""kind":"prompt""#) && !written.contains("parser"), "{written}");
        let _ = fs::remove_dir_all(&dir);
    }

    // The tool results below are what Amp 0.0.1793857728 gave a real plugin (low and medium modes).
    #[test]
    fn real_tool_results_become_edits_and_commits() {
        let patch = of(r#""ev":"tool","tool":"apply_patch","input":{"patchText":"*** Begin Patch\n*** Update File: notes.txt\n@@\n-one\n-two\n+one\n+TWO\n+two and a half\n three\n*** Add File: hello.txt\n+hi\n+bye\n*** End Patch"},"output":{"summary":"update: /w/probe/notes.txt (+2/-1)\nadd: /w/probe/hello.txt (+2/-0)","files":[{"uri":"file:///w/probe/notes.txt","type":"update","additions":2,"deletions":1,"diff":"Index: /w/probe/notes.txt\n@@ -1,3 +1,4 @@\n one\n-two\n+TWO\n+two and a half\n three\n"},{"uri":"file:///w/probe/hello.txt","type":"add","additions":2,"deletions":0,"diff":""}]}"#);
        assert_eq!(summary(&patch), ["edit /w/probe/notes.txt +2 -1", "edit /w/probe/hello.txt +2 -0"]);

        let edit = of(r#""ev":"tool","tool":"edit_file","input":{"path":"/w/probe/notes.txt","old_str":"three","new_str":"THREE"},"output":{"diff":"```diff\n--- /w/probe/notes.txt\toriginal\n+++ /w/probe/notes.txt\tmodified\n@@ -1,5 +1,5 @@\n one\n TWO\n two and a half\n-three\n+THREE\n four\n```","lineRange":[4,4]}"#);
        assert_eq!(summary(&edit), ["edit /w/probe/notes.txt +1 -1"]);

        let commit = of(r#""ev":"tool","tool":"shell_command","input":{"command":"git commit -m probe","workdir":"/w/probe/sub"},"output":{"output":"[master f7940c7] probe\n 3 files changed, 20 insertions(+), 1 deletion(-)\n","exitCode":0}"#);
        assert_eq!(summary(&commit), ["commit f7940c7 in /w/probe/sub"]);

        let failed = of(r#""ev":"tool","tool":"shell_command","input":{"command":"git commit -q -m probe","workdir":"/w/probe"},"output":{"output":"","exitCode":1}"#);
        assert!(failed.is_empty());
        let other = of(r#""ev":"tool","tool":"shell_command","input":{"command":"echo commit","workdir":"/w/probe"},"output":{"output":"commit\n","exitCode":0}"#);
        assert!(other.is_empty());
    }

    #[test]
    fn the_agent_starts_at_its_first_tool() {
        let out = of(r#""ev":"start""#);
        let [Found { ts, kind: Kind::AgentStart { session }, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((*ts, session.as_str()), (1795554335.236, THREAD));
    }

    #[test]
    fn the_end_of_a_turn() {
        let out = of(r#""ev":"end","reply":["I changed two files.","done"],"tools":4,"files":2,"model":"gpt-5.6-terra""#);
        let [Found { kind: Kind::ReplyDone { session, reply_words, tools, files_changed, model }, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((session.as_str(), *reply_words, *tools, *files_changed, model.as_deref()), (THREAD, 5, 4, 2, Some("gpt-5.6-terra")));
    }

    #[test]
    fn messages_that_say_nothing_are_dropped() {
        assert!(facts(&serde_json::json!({"ev": "prompt", "text": "hi", "thread": THREAD})).is_empty());
        assert!(facts(&serde_json::json!({"ev": "prompt", "text": "hi", "ts": 1.0})).is_empty());
        assert!(of(r#""ev":"session""#).is_empty());
        let dir = std::env::temp_dir().join(format!("kairos-amp-none-{}", std::process::id()));
        assert!(record_in(&dir, "not json", |_| false).is_none());
        assert!(!dir.exists());
    }

    #[test]
    fn recorded_lines_are_read_once_and_excluded_folders_never_written() {
        let dir = std::env::temp_dir().join(format!("kairos-amp-read-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("2026-01-01.jsonl"), "").unwrap();
        fs::write(dir.join("2026-11-20.jsonl"), "").unwrap();
        let mut amp = Amp::new(dir.clone());
        assert!(amp.poll().is_empty());

        record_in(&dir, &msg(r#""ev":"prompt","text":"hello there""#), |_| false).unwrap();
        record_in(&dir, &msg(r#""ev":"end","reply":["ok"],"tools":0,"files":0"#), |_| false).unwrap();
        assert!(record_in(&dir, &msg(r#""ev":"prompt","text":"secret""#), |p| p == Path::new("/w/probe")).is_none());
        assert!(!dir.join("2026-01-01.jsonl").exists() && dir.join("2026-11-20.jsonl").exists());

        let out = amp.poll();
        assert_eq!(summary(&out), ["prompt", "reply_done"]);
        assert_eq!(out[0].cwd.as_deref(), Some(Path::new("/w/probe")));
        assert!(amp.poll().is_empty());

        let mut again = Amp::new(dir.clone());
        again.restore(&amp.state());
        record_in(&dir, &msg(r#""ev":"prompt","text":"one more""#), |_| false).unwrap();
        assert_eq!(summary(&again.poll()), ["prompt"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_plugin_is_added_only_where_amp_is_and_points_at_kairos() {
        let config = std::env::temp_dir().join(format!("kairos-amp-config-{}", std::process::id()));
        let _ = fs::remove_dir_all(&config);
        let cli = Path::new(r"C:\Users\a b\AppData\Local\Programs\Kairos\kairos.exe");
        add_plugin(&config, cli);
        assert!(!config.exists());
        fs::create_dir_all(&config).unwrap();
        add_plugin(&config, cli);
        let text = fs::read_to_string(config.join("plugins").join(PLUGIN_FILE)).unwrap();
        assert!(text.contains(r#"const KAIROS = "C:\\Users\\a b\\AppData\\Local\\Programs\\Kairos\\kairos.exe""#), "{text}");
        assert!(!text.contains("__KAIROS__"));
        let _ = fs::remove_dir_all(&config);
    }
}
