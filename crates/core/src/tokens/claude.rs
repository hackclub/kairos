use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{keep, tail_since, unix, Counts, Reader, Use};
use crate::sources::claude::{log_folder_name, skip, started_name};
use crate::sources::Tail;

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    ty: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    uuid: Option<String>,
    #[serde(rename = "requestId")]
    request: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

pub struct Claude {
    roots: Vec<PathBuf>,
    tail: Option<Tail>,
}

impl Claude {
    pub fn new(roots: Vec<PathBuf>) -> Claude {
        Claude { roots, tail: None }
    }
}

// Claude Code files a session under the folder it was started in, while a log's cwd follows the agent's `cd`.
fn started_in(log: &Path, cwd: PathBuf) -> PathBuf {
    let started = started_name(log);
    let at = cwd.ancestors().find(|a| started.as_deref().is_some_and(|s| log_folder_name(a) == s)).map(Path::to_path_buf);
    at.unwrap_or(cwd)
}

fn reply(log: &Path, line: &str, since: f64) -> Option<Use> {
    // Most lines have no usage; the ones that do are cheap to find before parsing.
    if !line.contains("\"usage\"") {
        return None;
    }
    let e: Line = serde_json::from_str(line).ok()?;
    if e.ty.as_deref() != Some("assistant") {
        return None;
    }
    let m = e.message?;
    let usage = m.usage?;
    // One reply is written once per content block, and a resumed session's file has the old lines again.
    let key = match (m.id.as_deref(), e.request.as_deref().or(e.uuid.as_deref())) {
        (Some(id), Some(req)) => format!("claude:{id}:{req}"),
        _ => format!("claude:{}", e.uuid.as_deref()?),
    };
    let cwd = e.cwd.map(|c| started_in(log, PathBuf::from(c)));
    let n = Counts {
        input: usage.input_tokens.unwrap_or(0),
        output: usage.output_tokens.unwrap_or(0),
        cache_read: usage.cache_read_input_tokens.unwrap_or(0),
        cache_write: usage.cache_creation_input_tokens.unwrap_or(0),
    };
    keep(Use { ts: unix(e.timestamp.as_deref())?, harness: "claude-code", model: m.model?, cwd, key, n }, since)
}

impl Reader for Claude {
    fn read(&mut self, since: f64) -> Vec<Use> {
        let roots = &self.roots;
        let tail = self.tail.get_or_insert_with(|| tail_since(Tail::new(roots.clone(), "jsonl", skip), since));
        let mut out = Vec::new();
        tail.poll(|path, line| out.extend(reply(path, line, since)));
        out
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::testing::{append, dir, write};
    use super::*;

    const SINCE: f64 = 1_700_000_000.0;

    fn line(id: &str, req: &str, ts: &str, out: u64) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"u-{id}-{out}","timestamp":"{ts}","cwd":"/w/p/sub","requestId":"{req}","message":{{"id":"{id}","model":"claude-opus-5-5","content":[],"usage":{{"input_tokens":2,"cache_creation_input_tokens":372,"cache_read_input_tokens":106621,"output_tokens":{out},"service_tier":"standard"}}}}}}"#
        ) + "\n"
    }

    #[test]
    fn one_reply_written_per_block_and_copied_into_a_resumed_session_is_one_key() {
        let root = dir("claude-copies");
        let a = line("msg_1", "req_1", "2026-11-26T14:09:12.358Z", 258);
        write(&root.join("-w-p/s1.jsonl"), &format!("{a}{a}{}", line("msg_2", "req_2", "2026-11-26T14:10:00Z", 40)));
        write(&root.join("-w-p/s2.jsonl"), &a);
        write(&root.join("-w-p/s1/subagents/agent-x.jsonl"), &line("msg_3", "req_3", "2026-11-26T14:11:00Z", 5));
        let mut r = Claude::new(vec![root.clone()]);
        let mut got = r.read(SINCE);
        got.sort_by(|a, b| a.key.cmp(&b.key));
        let keys: Vec<&str> = got.iter().map(|u| u.key.as_str()).collect();
        assert_eq!(keys, ["claude:msg_1:req_1", "claude:msg_1:req_1", "claude:msg_1:req_1", "claude:msg_2:req_2", "claude:msg_3:req_3"]);
        assert_eq!(got[0].n, Counts { input: 2, output: 258, cache_read: 106621, cache_write: 372 });
        assert_eq!((got[0].harness, got[0].model.as_str()), ("claude-code", "claude-opus-5-5"));
        assert_eq!(got[0].ts, 1_795_702_152.358);
        assert_eq!(got[0].cwd.as_deref(), Some(Path::new("/w/p")), "the folder the log is filed under, not where the agent cd'd");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn lines_without_a_model_or_counts_and_other_types_are_skipped() {
        let root = dir("claude-skips");
        let ok = line("msg_1", "req_1", "2026-11-26T14:09:12Z", 7);
        let synthetic = ok.replace("claude-opus-5-5", "<synthetic>");
        let empty = line("msg_2", "req_2", "2026-11-26T14:09:13Z", 0)
            .replace(r#""input_tokens":2"#, r#""input_tokens":0"#)
            .replace(r#""cache_creation_input_tokens":372"#, r#""cache_creation_input_tokens":0"#)
            .replace(r#""cache_read_input_tokens":106621"#, r#""cache_read_input_tokens":0"#);
        let user = r#"{"type":"user","timestamp":"2026-11-26T14:09:14Z","message":{"role":"user","content":"look at \"usage\" please"}}"#;
        let no_model = ok.replace(r#""model":"claude-opus-5-5","#, "");
        let old = line("msg_9", "req_9", "2020-01-01T00:00:00Z", 9);
        write(&root.join("-w-p/s.jsonl"), &format!("{synthetic}{empty}{user}\n{no_model}{old}{ok}not json \"usage\"\n"));
        let got = Claude::new(vec![root.clone()]).read(SINCE);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].key, "claude:msg_1:req_1");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_reply_without_a_request_id_is_keyed_by_its_line() {
        let root = dir("claude-uuid");
        let l = line("msg_1", "x", "2026-11-26T14:09:12Z", 7).replace(r#""requestId":"x","#, "");
        write(&root.join("-w-p/s.jsonl"), &l);
        let got = Claude::new(vec![root.clone()]).read(SINCE);
        assert_eq!(got[0].key, "claude:msg_1:u-msg_1-7");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_second_read_returns_only_what_was_written_since() {
        let root = dir("claude-incremental");
        let file = root.join("-w-p/s.jsonl");
        write(&file, &line("msg_1", "req_1", "2026-11-26T14:09:12Z", 7));
        let mut r = Claude::new(vec![root.clone()]);
        assert_eq!(r.read(SINCE).len(), 1);
        assert!(r.read(SINCE).is_empty());
        append(&file, &line("msg_2", "req_2", "2026-11-26T14:10:12Z", 8));
        let got = r.read(SINCE);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].key, "claude:msg_2:req_2");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_untouched_since_the_start_is_not_read_but_one_written_later_is() {
        let root = dir("claude-mtime");
        let old = root.join("-w-p/old.jsonl");
        write(&old, &line("msg_1", "req_1", "2026-11-26T14:09:12Z", 7));
        let long_ago = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        fs::File::options().write(true).open(&old).unwrap().set_modified(long_ago).unwrap();
        let mut r = Claude::new(vec![root.clone()]);
        assert!(r.read(SINCE).is_empty());
        write(&root.join("-w-p/new.jsonl"), &line("msg_2", "req_2", "2026-11-26T14:10:12Z", 8));
        let got = r.read(SINCE);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].key, "claude:msg_2:req_2");
        let _ = fs::remove_dir_all(&root);
    }
}
