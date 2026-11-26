use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{keep, tail_since, unix, Counts, Reader, Use};
use crate::sources::codex::folder;
use crate::sources::Tail;

// What a rollout said so far about where and with which model it runs.
#[derive(Default)]
struct Run {
    model: Option<String>,
    cwd: Option<PathBuf>,
}

pub struct Codex {
    roots: Vec<PathBuf>,
    tail: Option<Tail>,
    runs: HashMap<PathBuf, Run>,
}

impl Codex {
    pub fn new(roots: Vec<PathBuf>) -> Codex {
        Codex { roots, tail: None, runs: HashMap::new() }
    }
}

fn cwd_of(p: &Value) -> Option<PathBuf> {
    p.get("cwd").and_then(Value::as_str).and_then(folder)
}

// A file that was not read from its start has no session_meta line to learn the folder from.
fn first_line(path: &Path) -> Run {
    let mut line = String::new();
    let Ok(f) = File::open(path) else { return Run::default() };
    if BufReader::new(f).read_line(&mut line).is_err() {
        return Run::default();
    }
    let meta: Option<Value> = serde_json::from_str(&line).ok();
    let cwd = meta.filter(|e| e.get("type").and_then(Value::as_str) == Some("session_meta")).and_then(|e| e.get("payload").and_then(cwd_of));
    Run { model: None, cwd }
}

fn field(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

fn reply(run: &Run, e: &Value, p: &Value, since: f64) -> Option<Use> {
    let info = p.get("info").filter(|i| i.is_object())?;
    let (total, last) = (info.get("total_token_usage")?.as_object()?, info.get("last_token_usage")?);
    // Totals only grow, so the same totals are the same reply: the event written twice, or a forked rollout's copied history.
    let totals: Vec<String> = total.values().map(|v| v.as_u64().unwrap_or(0).to_string()).collect();
    let (cached, written) = (field(last, "cached_input_tokens"), field(last, "cache_write_input_tokens"));
    let n = Counts {
        input: field(last, "input_tokens").saturating_sub(cached).saturating_sub(written),
        output: field(last, "output_tokens"),
        cache_read: cached,
        cache_write: written,
    };
    let (model, cwd) = (run.model.clone()?, run.cwd.clone());
    keep(Use { ts: unix(e.get("timestamp").and_then(Value::as_str))?, harness: "codex", model, cwd, key: format!("codex:{}", totals.join(",")), n }, since)
}

impl Reader for Codex {
    fn read(&mut self, since: f64) -> Vec<Use> {
        let roots = &self.roots;
        let tail = self.tail.get_or_insert_with(|| tail_since(Tail::new(roots.clone(), "jsonl", |_| false), since));
        let runs = &mut self.runs;
        let mut out = Vec::new();
        tail.poll(|path, line| {
            if !(line.contains("\"token_count\"") || line.contains("\"turn_context\"") || line.contains("\"session_meta\"")) {
                return;
            }
            let Ok(e) = serde_json::from_str::<Value>(line) else { return };
            let Some(p) = e.get("payload") else { return };
            let run = runs.entry(path.to_path_buf()).or_insert_with(|| first_line(path));
            match e.get("type").and_then(Value::as_str) {
                // A forked rollout has its parent's session_meta further down, which is not where this one runs.
                Some("session_meta") if run.cwd.is_none() => run.cwd = cwd_of(p),
                Some("turn_context") => {
                    run.cwd = cwd_of(p).or(run.cwd.take());
                    run.model = p.get("model").and_then(Value::as_str).map(str::to_string).or(run.model.take());
                }
                Some("event_msg") if p.get("type").and_then(Value::as_str) == Some("token_count") => out.extend(reply(run, &e, p, since)),
                _ => {}
            }
        });
        self.runs.retain(|p, _| p.exists());
        out
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::testing::{append, dir, write};
    use super::*;

    const SINCE: f64 = 1_700_000_000.0;
    const META: &str = r#"{"timestamp":"2026-11-19T09:59:59Z","type":"session_meta","payload":{"id":"s1","cwd":"file:///w/meta","cli_version":"0.167.0"}}"#;
    const CTX: &str = r#"{"timestamp":"2026-11-19T10:00:00Z","type":"turn_context","payload":{"cwd":"/w/p","model":"gpt-x"}}"#;

    fn count(ts: &str, total: (u64, u64, u64), last: (u64, u64, u64)) -> String {
        let usage = |(input, cached, output): (u64, u64, u64)| {
            format!(r#"{{"input_tokens":{input},"cached_input_tokens":{cached},"cache_write_input_tokens":0,"output_tokens":{output},"reasoning_output_tokens":0,"total_tokens":{}}}"#, input + output)
        };
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{},"last_token_usage":{},"model_context_window":258400}},"rate_limits":{{}}}}}}"#,
            usage(total),
            usage(last)
        )
    }

    fn lines(l: &[&str]) -> String {
        l.iter().map(|l| format!("{l}\n")).collect()
    }

    #[test]
    fn counts_come_from_the_last_call_and_the_same_totals_are_one_key() {
        let root = dir("codex-dupes");
        let first = count("2026-11-19T10:00:05Z", (16492, 10880, 81), (16492, 10880, 81));
        let second = count("2026-11-19T10:00:08Z", (33107, 27136, 142), (16615, 16256, 61));
        let null = r#"{"timestamp":"2026-11-19T10:00:09Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{}}}"#;
        write(&root.join("2026/11/19/rollout-a.jsonl"), &lines(&[META, CTX, &first, &second, &second, null]));
        let fork = lines(&[META, CTX, &first, &second]).replace("file:///w/meta", "/w/other");
        write(&root.join("2026/11/19/rollout-fork.jsonl"), &fork);
        let got = Codex::new(vec![root.clone()]).read(SINCE);
        assert_eq!(got.len(), 5);
        let keys: std::collections::BTreeSet<&str> = got.iter().map(|u| u.key.as_str()).collect();
        assert_eq!(keys.len(), 2);
        let n = |u: &Use| (u.n.input, u.n.output, u.n.cache_read, u.n.cache_write);
        let mut counts: Vec<_> = got.iter().map(n).collect();
        counts.sort();
        counts.dedup();
        assert_eq!(counts, [(359, 61, 16256, 0), (5612, 81, 10880, 0)]);
        assert!(got.iter().all(|u| u.harness == "codex" && u.model == "gpt-x" && u.cwd.as_deref() == Some(Path::new("/w/p"))));
        assert_eq!(got.iter().map(|u| u.ts).fold(f64::MAX, f64::min), 1_795_082_405.0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_cache_split_the_model_and_the_folder_follow_the_rollout() {
        let root = dir("codex-state");
        let file = root.join("2026/11/19/rollout-a.jsonl");
        let nocwd = CTX.replace(r#""cwd":"/w/p","#, "");
        let written = count("2026-11-19T10:00:05Z", (100, 60, 10), (100, 60, 10)).replace(r#""cache_write_input_tokens":0,"output_tokens":10"#, r#""cache_write_input_tokens":15,"output_tokens":10"#);
        let before_ctx = count("2026-11-19T09:59:59Z", (1, 0, 1), (1, 0, 1));
        write(&file, &lines(&[&before_ctx, META, &nocwd, &written]));
        let mut r = Codex::new(vec![root.clone()]);
        let got = r.read(SINCE);
        assert_eq!(got.len(), 1, "no model is known before the first turn_context");
        assert_eq!(got[0].n, Counts { input: 25, output: 10, cache_read: 60, cache_write: 15 });
        assert_eq!(got[0].cwd.as_deref(), Some(Path::new("/w/meta")), "the session's folder when no turn names one");

        let next = CTX.replace("gpt-x", "gpt-y");
        append(&file, &lines(&[&next, &count("2026-11-19T10:05:00Z", (300, 100, 30), (200, 40, 20))]));
        let got = r.read(SINCE);
        assert_eq!(got.len(), 1, "only what was written since");
        assert_eq!((got[0].model.as_str(), got[0].cwd.as_deref()), ("gpt-y", Some(Path::new("/w/p"))));
        assert!(r.read(SINCE).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rollout_first_read_in_the_middle_still_knows_its_folder() {
        let root = dir("codex-middle");
        let file = root.join("2026/11/19/rollout-a.jsonl");
        write(&file, &lines(&[META, CTX]));
        let long_ago = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        fs::File::options().write(true).open(&file).unwrap().set_modified(long_ago).unwrap();
        let mut r = Codex::new(vec![root.clone()]);
        assert!(r.read(SINCE).is_empty());
        let nocwd = CTX.replace(r#""cwd":"/w/p","#, "");
        append(&file, &lines(&[&nocwd, &count("2026-11-19T10:05:00Z", (300, 100, 30), (200, 40, 20))]));
        let got = r.read(SINCE);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].cwd.as_deref(), Some(Path::new("/w/meta")));
        let _ = fs::remove_dir_all(&root);
    }
}
