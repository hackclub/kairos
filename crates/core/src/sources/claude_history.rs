use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use serde_json::Value;

use super::Found;
use crate::event::Kind;
use crate::prompt_stats;
use crate::shape::{prompt_shape, Marked, Shape};

// Claude Code keeps every prompt in its config folder's history.jsonl as it was typed, with each paste swapped for a
// placeholder and stored apart. Matching a transcript's prompt to it tells which part was pasted.
const BACKLOG: u64 = 1024 * 1024;
const KEEP: usize = 2000;
const MATCH_SECS: f64 = 5.0;
const PLACEHOLDERS: [&str; 2] = ["[Pasted text #", "[Image #"];

struct Entry {
    session: String,
    ts: f64,
    typed: Shape,
    typed_words: usize,
    pastes: u32,
}

pub(super) struct History {
    files: Vec<PathBuf>,
    offsets: HashMap<PathBuf, u64>,
    entries: VecDeque<Entry>,
}

impl History {
    pub fn new(roots: &[PathBuf]) -> History {
        let files = roots
            .iter()
            .filter(|r| r.file_name().is_some_and(|n| n == "projects"))
            .filter_map(|r| r.parent())
            .map(|d| d.join("history.jsonl"))
            .collect();
        History { files, offsets: HashMap::new(), entries: VecDeque::new() }
    }

    pub fn refresh(&mut self) {
        for path in &self.files {
            let Ok(len) = std::fs::metadata(path).map(|m| m.len()) else { continue };
            let (start, partial) = match self.offsets.get(path) {
                Some(&o) if o <= len => (o, false),
                _ => (len.saturating_sub(BACKLOG), len > BACKLOG),
            };
            if start == len {
                self.offsets.insert(path.clone(), len);
                continue;
            }
            let Ok(mut f) = File::open(path) else { continue };
            let mut buf = Vec::new();
            if f.seek(SeekFrom::Start(start)).is_err() || f.take(len - start).read_to_end(&mut buf).is_err() {
                continue;
            }
            let Some(end) = buf.iter().rposition(|&b| b == b'\n') else {
                self.offsets.insert(path.clone(), start);
                continue;
            };
            let text = String::from_utf8_lossy(&buf[..end]);
            let mut lines = text.lines();
            if partial {
                lines.next();
            }
            self.entries.extend(lines.filter_map(entry));
            self.offsets.insert(path.clone(), start + end as u64 + 1);
        }
        while self.entries.len() > KEEP {
            self.entries.pop_front();
        }
    }

    pub fn attach(&self, out: &mut [Found]) {
        for f in out {
            if let Kind::Prompt { session, words, shape, .. } = &mut f.kind {
                if let Some(s) = self.shape_of(session, f.ts, *words) {
                    *shape = Some(s);
                }
            }
        }
    }

    fn shape_of(&self, session: &str, ts: f64, words: usize) -> Option<Shape> {
        let e = self
            .entries
            .iter()
            .filter(|e| e.session == session && (e.ts - ts).abs() <= MATCH_SECS)
            .min_by(|a, b| (a.ts - ts).abs().total_cmp(&(b.ts - ts).abs()))?;
        let pasted = if e.pastes == 0 { 0 } else { words.saturating_sub(e.typed_words) };
        let words = u32::try_from(pasted).unwrap_or(u32::MAX);
        Some(Shape { marked: Some(Marked { pastes: e.pastes, words }), ..e.typed.clone() })
    }
}

fn entry(line: &str) -> Option<Entry> {
    let v: Value = serde_json::from_str(line).ok()?;
    let session = v.get("sessionId")?.as_str()?.to_string();
    let ts = v.get("timestamp")?.as_f64()? / 1000.0;
    let typed = without_placeholders(v.get("display")?.as_str()?);
    let pastes = v
        .get("pastedContents")
        .and_then(Value::as_object)
        .map_or(0, |m| m.values().filter(|p| p.get("type").and_then(Value::as_str) == Some("text")).count());
    Some(Entry {
        session,
        ts,
        typed: prompt_shape(&typed),
        typed_words: prompt_stats(&typed).0,
        pastes: u32::try_from(pastes).unwrap_or(u32::MAX),
    })
}

fn without_placeholders(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('[') {
        let at = &rest[i..];
        match at.find(']').filter(|_| PLACEHOLDERS.iter().any(|p| at.starts_with(p))) {
            Some(j) => {
                out.push_str(&rest[..i]);
                rest = &at[j + 1..];
            }
            None => {
                out.push_str(&rest[..=i]);
                rest = &rest[i + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(session: &str, ms: u64, display: &str, pastes: usize) -> String {
        let pasted: serde_json::Map<String, Value> = (1..=pastes)
            .map(|i| (i.to_string(), serde_json::json!({"id": i, "type": "text", "contentHash": "abc"})))
            .collect();
        serde_json::json!({"display": display, "pastedContents": pasted, "timestamp": ms, "project": "/w", "sessionId": session})
            .to_string()
    }

    fn prompt(session: &str, ts: f64, words: usize) -> Found {
        Found {
            ts,
            cwd: None,
            kind: Kind::Prompt { words, pasted: false, model: None, session: session.into(), shape: None, version: None, via: None, permissions: None },
        }
    }

    fn marked(f: &Found) -> Option<(u32, u32, u32)> {
        match &f.kind {
            Kind::Prompt { shape: Some(s), .. } => s.marked.as_ref().map(|m| (m.pastes, m.words, s.chars)),
            _ => None,
        }
    }

    #[test]
    fn placeholders_are_cut_and_everything_else_kept() {
        assert_eq!(without_placeholders("see [Pasted text #1 +40 lines] and [Image #2] ok"), "see  and  ok");
        assert_eq!(
            without_placeholders("arr[0] and [link](x) [Pasted text #3"),
            "arr[0] and [link](x) [Pasted text #3"
        );
    }

    #[test]
    fn a_prompt_gets_the_typed_part_and_what_was_pasted() {
        let dir = std::env::temp_dir().join(format!("kairos-claude-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("projects")).unwrap();
        let file = dir.join("history.jsonl");
        std::fs::write(&file, format!("{}\n", line("s1", 1_000_000, "fix this [Pasted text #1 +40 lines]", 1)))
            .unwrap();
        let mut h = History::new(&[dir.join("projects"), dir.join("local-agent-mode-sessions")]);
        h.refresh();

        let mut out = vec![prompt("s1", 1000.2, 300), prompt("s2", 1000.0, 5), prompt("s1", 1010.0, 5)];
        h.attach(&mut out);
        assert_eq!(marked(&out[0]), Some((1, 298, 8)));
        assert_eq!(marked(&out[1]), None);
        assert_eq!(marked(&out[2]), None);

        let mut text = std::fs::read_to_string(&file).unwrap();
        text.push_str(&line("s1", 1_010_100, "just typed this", 0));
        std::fs::write(&file, &text).unwrap();
        h.refresh();
        h.attach(&mut out);
        assert_eq!(marked(&out[2]), None, "a line without its newline isn't read yet");
        std::fs::write(&file, format!("{text}\n")).unwrap();
        h.refresh();
        h.attach(&mut out);
        assert_eq!(marked(&out[2]), Some((0, 0, 15)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
