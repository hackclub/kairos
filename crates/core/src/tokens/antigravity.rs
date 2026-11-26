use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{keep, tail_since, Counts, Reader, Use};
use crate::sources::antigravity::{list, selected_model, ts, workspace};
use crate::sources::Tail;

#[derive(Default)]
struct Conv {
    model: Option<String>,
    cwd: Option<PathBuf>,
    looked: bool,
}

pub struct Antigravity {
    roots: Vec<PathBuf>,
    tail: Option<Tail>,
    convs: HashMap<PathBuf, Conv>,
}

impl Antigravity {
    pub fn new(roots: Vec<PathBuf>) -> Antigravity {
        Antigravity { roots, tail: None, convs: HashMap::new() }
    }
}

// <root>/brain/<conversation>/.system_generated/logs/transcript.jsonl
fn whose(log: &Path) -> Option<(&Path, String)> {
    let mut up = log.ancestors();
    let id = up.nth(3)?.file_name()?.to_string_lossy().into_owned();
    Some((up.nth(1)?, id))
}

// The model picker names it "Gemini 3.8 Flash (High)", the steps "gemini-3.8-flash-high".
fn slug(name: &str) -> String {
    name.to_lowercase().replace(['(', ')'], "").split_whitespace().collect::<Vec<_>>().join("-")
}

fn step(conv: &mut Conv, log: &Path, e: &Value, since: f64) -> Option<Use> {
    if let Some(m) = e.get("content").and_then(Value::as_str).filter(|c| c.contains("`Model Selection`")).and_then(selected_model) {
        conv.model = Some(slug(&m));
    }
    if let Some(m) = e.get("model").and_then(Value::as_str).filter(|m| !m.is_empty()) {
        conv.model = Some(m.to_string());
    }
    let tokens = |k: &str| e.get(k).and_then(Value::as_u64);
    if tokens("input_tokens").is_none() && tokens("output_tokens").is_none() {
        return None;
    }
    let (root, id) = whose(log)?;
    let n = Counts {
        input: tokens("input_tokens").unwrap_or(0),
        output: tokens("output_tokens").unwrap_or(0),
        cache_read: tokens("cache_read_tokens").unwrap_or(0),
        cache_write: 0,
    };
    if conv.cwd.is_none() && !conv.looked {
        conv.looked = true;
        conv.cwd = workspace(root, &id);
    }
    let key = format!("antigravity:{}:{id}:{}", root.display(), e.get("step_index").and_then(Value::as_i64)?);
    keep(Use { ts: ts(e)?, harness: "antigravity", model: conv.model.clone()?, cwd: conv.cwd.clone(), key, n }, since)
}

impl Reader for Antigravity {
    fn read(&mut self, since: f64) -> Vec<Use> {
        let roots = &self.roots;
        let tail = self.tail.get_or_insert_with(|| tail_since(Tail::with(roots.clone(), list), since));
        let convs = &mut self.convs;
        convs.values_mut().filter(|c| c.cwd.is_none()).for_each(|c| c.looked = false);
        let mut out = Vec::new();
        tail.poll(|path, line| {
            if !(line.contains("\"input_tokens\"") || line.contains("\"output_tokens\"") || line.contains("`Model Selection`")) {
                return;
            }
            let Ok(e) = serde_json::from_str::<Value>(line) else { return };
            out.extend(step(convs.entry(path.to_path_buf()).or_default(), path, &e, since));
        });
        self.convs.retain(|p, _| p.exists());
        out
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::testing::{append, dir, write};
    use super::*;

    const SINCE: f64 = 1_700_000_000.0;

    fn step(idx: u32, at: &str, model: Option<&str>, tokens: (u64, u64, u64)) -> String {
        let model = model.map(|m| format!(r#""model":"{m}","#)).unwrap_or_default();
        format!(
            r#"{{"type":"PLANNER_RESPONSE","source":"MODEL","status":"DONE","step_index":{idx},"created_at":"{at}",{model}"input_tokens":{},"output_tokens":{},"cache_read_tokens":{}}}"#,
            tokens.0, tokens.1, tokens.2
        ) + "\n"
    }

    fn log(root: &Path, id: &str) -> PathBuf {
        root.join("brain").join(id).join(".system_generated/logs/transcript.jsonl")
    }

    #[test]
    fn steps_become_uses_in_the_conversations_folder() {
        let root = dir("agy-steps");
        let t = log(&root, "c1");
        let selection = r#"{"type":"USER_INPUT","source":"USER_EXPLICIT","step_index":0,"created_at":"2026-11-24T09:32:40Z","content":"The user changed setting `Model Selection` from None to Gemini 3.8 Flash (High). Do not mention it."}"#;
        write(&t, &format!("{selection}\n{}{}{}", step(1, "2026-11-24T09:32:45Z", None, (3112, 238, 10496)), step(3, "2026-11-24T09:33:45Z", Some("gemini-3.8-flash-high"), (100, 5, 0)), step(5, "2026-11-24T09:34:45Z", None, (0, 0, 0))));
        write(&root.join("cache/last_conversations.json"), r#"{"file:///w/my%20p":"c1"}"#);
        let got = Antigravity::new(vec![root.clone()]).read(SINCE);
        assert_eq!(got.len(), 2, "a step with no counts is not a use");
        assert_eq!(got[0].n, Counts { input: 3112, output: 238, cache_read: 10496, cache_write: 0 });
        assert_eq!((got[0].harness, got[0].model.as_str(), got[0].ts), ("antigravity", "gemini-3.8-flash-high", 1_795_512_765.0));
        assert_eq!(got[0].cwd.as_deref(), Some(Path::new("/w/my p")));
        assert_ne!(got[0].key, got[1].key);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn steps_before_any_model_is_known_are_skipped_and_the_same_step_in_two_roots_stays_apart() {
        let (a, b) = (dir("agy-a"), dir("agy-b"));
        for root in [&a, &b] {
            write(&log(root, "c1"), &step(1, "2026-11-24T09:32:45Z", None, (1, 1, 0)));
        }
        write(&log(&a, "c2"), &(step(1, "2026-11-24T09:32:45Z", None, (1, 1, 0)) + &step(3, "2026-11-24T09:32:50Z", Some("m"), (2, 2, 0))));
        let got = Antigravity::new(vec![a.clone(), b.clone()]).read(SINCE);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].model.as_str(), got[0].cwd.clone()), ("m", None));
        let _ = fs::remove_dir_all(&a);
        let _ = fs::remove_dir_all(&b);
    }

    #[test]
    fn a_second_read_returns_only_new_steps_and_keeps_the_model() {
        let root = dir("agy-incremental");
        let t = log(&root, "c1");
        write(&t, &step(1, "2026-11-24T09:32:45Z", Some("m"), (1, 1, 0)));
        let mut r = Antigravity::new(vec![root.clone()]);
        assert_eq!(r.read(SINCE).len(), 1);
        assert!(r.read(SINCE).is_empty());
        append(&t, &step(3, "2026-11-24T09:35:45Z", None, (2, 2, 0)));
        let got = r.read(SINCE);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].model, "m");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn model_names_from_the_picker_match_the_steps() {
        assert_eq!(slug("Gemini 3.8 Flash (High)"), "gemini-3.8-flash-high");
        assert_eq!(slug("Gemini 3.1 Pro"), "gemini-3.1-pro");
    }
}
