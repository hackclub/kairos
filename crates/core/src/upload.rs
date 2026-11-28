use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::log_files;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub last_hash: Option<String>,
    pub file: Option<String>,
    pub line: usize,
}

/// A single event larger than this still goes alone.
pub const MAX_BODY: usize = 4 << 20;
const BODY_FRAME: usize = r#"{"events":[]}"#.len();

pub struct Batch {
    pub lines: Vec<String>,
    pub end: State,
}

impl Batch {
    pub fn body(&self) -> String {
        format!("{{\"events\":[{}]}}", self.lines.join(","))
    }
}

pub fn state_path(dir: &Path) -> PathBuf {
    dir.join("upload.json")
}

impl State {
    pub fn load(dir: &Path) -> State {
        fs::read_to_string(state_path(dir))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let path = state_path(dir);
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string(self)?)?;
        fs::rename(&tmp, &path)
    }
}

fn scan(dir: &Path, from: &State, mut f: impl FnMut(&str, &str, usize) -> bool) {
    for path in log_files(dir) {
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        let skip = match from.file.as_deref() {
            Some(done) if name.as_str() < done => continue,
            Some(done) if name == done => from.line,
            _ => 0,
        };
        let Ok(file) = File::open(&path) else { continue };
        let mut reader = BufReader::new(file);
        let mut buf = String::new();
        let mut n = 0;
        loop {
            buf.clear();
            if !matches!(reader.read_line(&mut buf), Ok(c) if c > 0) || !buf.ends_with('\n') {
                break;
            }
            n += 1;
            if n <= skip || buf.trim().is_empty() {
                continue;
            }
            if !f(buf.trim_end(), &name, n) {
                return;
            }
        }
    }
}

/// A line that doesn't parse is skipped, and a batch ends where the chain breaks: the server then answers
/// chain_mismatch at the break, which the uploader recovers from, where a break inside a batch is refused for good.
pub fn next_batch(dir: &Path, from: &State, max: usize) -> Option<Batch> {
    let mut lines = Vec::new();
    let mut end = from.clone();
    let mut size = BODY_FRAME;
    let mut last: Option<String> = None;
    scan(dir, from, |line, file, n| {
        let Some((prev, hash)) = links(line) else { return true };
        let grown = size + line.len() + usize::from(!lines.is_empty());
        if !lines.is_empty() && (grown > MAX_BODY || last.as_deref() != Some(prev.as_str())) {
            return false;
        }
        size = grown;
        lines.push(line.to_string());
        end = State { last_hash: Some(hash.clone()), file: Some(file.to_string()), line: n };
        last = Some(hash);
        lines.len() < max
    });
    (!lines.is_empty()).then_some(Batch { lines, end })
}

fn links(line: &str) -> Option<(String, String)> {
    let v: Value = serde_json::from_str(line).ok()?;
    Some((v.get("prev").and_then(Value::as_str).unwrap_or("").to_string(), v.get("hash")?.as_str()?.to_string()))
}

/// Just before the last event that follows `hash`.
pub fn locate_next(dir: &Path, hash: &str) -> Option<State> {
    let mut found = None;
    scan(dir, &State::default(), |line, file, n| {
        if line.contains(hash) && links(line).is_some_and(|(prev, _)| prev == hash) {
            found = Some(State { last_hash: Some(hash.to_string()), file: Some(file.to_string()), line: n - 1 });
        }
        true
    });
    found
}

pub fn reanchor_path(dir: &Path) -> PathBuf {
    dir.join("reanchor")
}

/// Asks the recorder to chain its next event from `hash`, the server's last one. For a log that no longer continues
/// from what the server has: a tail lost in a power cut after it was uploaded, or files written out of order.
pub fn request_reanchor(dir: &Path, hash: &str) {
    if !hash.is_empty() && hash.len() <= 128 && hash.bytes().all(|b| b.is_ascii_alphanumeric()) {
        let _ = fs::write(reanchor_path(dir), hash);
    }
}

pub fn take_reanchor(dir: &Path) -> Option<String> {
    let path = reanchor_path(dir);
    let hash = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(&path);
    let hash = hash.trim();
    (!hash.is_empty() && hash.len() <= 128 && hash.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| hash.to_string())
}

/// What the server is sent has to survive a power cut here, or the next start can't continue from it.
pub fn flush(dir: &Path) {
    for path in log_files(dir).iter().rev().take(2) {
        if let Ok(f) = OpenOptions::new().append(true).open(path) {
            let _ = f.sync_data();
        }
    }
}

pub fn locate(dir: &Path, hash: &str) -> Option<State> {
    let mut found = None;
    scan(dir, &State::default(), |line, file, n| {
        if line.contains(hash)
            && serde_json::from_str::<Value>(line).ok().is_some_and(|v| v["hash"].as_str() == Some(hash))
        {
            found = Some(State { last_hash: Some(hash.to_string()), file: Some(file.to_string()), line: n });
            return false;
        }
        true
    });
    found
}

/// After the last event in the log.
pub fn end(dir: &Path) -> State {
    let mut end = State::default();
    scan(dir, &State::default(), |line, file, n| {
        if let Some((_, hash)) = links(line) {
            end = State { last_hash: Some(hash), file: Some(file.to_string()), line: n };
        }
        true
    });
    end
}

pub fn pending(dir: &Path, from: &State) -> usize {
    let mut count = 0;
    scan(dir, from, |_, _, _| {
        count += 1;
        true
    });
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, Kind, GENESIS};

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kairos-upload-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn chain(n: usize) -> Vec<Event> {
        let mut prev = GENESIS.to_string();
        (0..n)
            .map(|i| {
                let mut e = Event::new(1000.0 + i as f64, None, Kind::Present { input: None, keys: None, injected: None });
                e.seal(&prev);
                prev = e.hash.clone();
                e
            })
            .collect()
    }

    fn write_logs(dir: &Path, events: &[Event], split: &[(&str, usize)]) {
        let mut at = 0;
        for (name, count) in split {
            let text: String =
                events[at..at + count].iter().map(|e| serde_json::to_string(e).unwrap() + "\n").collect();
            fs::write(dir.join(format!("events-{name}.jsonl")), text).unwrap();
            at += count;
        }
    }

    fn hashes(b: &Batch) -> Vec<String> {
        b.lines.iter().map(|l| serde_json::from_str::<Event>(l).unwrap().hash).collect()
    }

    #[test]
    fn locate_across_files_and_batches() {
        let dir = tmp("locate");
        let ev = chain(7);
        write_logs(&dir, &ev, &[("2026-11-17", 3), ("2026-11-18", 0), ("2026-11-19", 4)]);

        let all = next_batch(&dir, &State::default(), 100).unwrap();
        assert_eq!(all.lines.len(), 7);
        assert_eq!(all.end.last_hash.as_deref(), Some(ev[6].hash.as_str()));
        assert_eq!(all.end.file.as_deref(), Some("events-2026-11-19.jsonl"));
        assert_eq!(all.end.line, 4);

        let first = next_batch(&dir, &State::default(), 2).unwrap();
        assert_eq!(hashes(&first), [ev[0].hash.clone(), ev[1].hash.clone()]);
        let second = next_batch(&dir, &first.end, 2).unwrap();
        assert_eq!(hashes(&second), [ev[2].hash.clone(), ev[3].hash.clone()]);
        let third = next_batch(&dir, &second.end, 100).unwrap();
        assert_eq!(hashes(&third), [ev[4].hash.clone(), ev[5].hash.clone(), ev[6].hash.clone()]);
        assert!(next_batch(&dir, &third.end, 100).is_none());

        let at = locate(&dir, &ev[2].hash).unwrap();
        assert_eq!(
            at,
            State { last_hash: Some(ev[2].hash.clone()), file: Some("events-2026-11-17.jsonl".into()), line: 3 }
        );
        let rest = next_batch(&dir, &at, 100).unwrap();
        assert_eq!(rest.lines.len(), 4);
        assert_eq!(pending(&dir, &at), 4);
        assert_eq!(pending(&dir, &State::default()), 7);
        assert_eq!(pending(&dir, &all.end), 0);
        assert!(locate(&dir, "deadbeef").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn batches_stop_before_the_body_passes_the_cap() {
        let dir = tmp("size");
        let pad = "x".repeat(1536 << 10);
        let line = |i: usize, pad: &str| {
            let prev = i.checked_sub(1).map_or("hx".to_string(), |p| format!("h{p}"));
            format!("{{\"prev\":\"{prev}\",\"hash\":\"h{i}\",\"pad\":\"{pad}\"}}\n")
        };
        let text: String = (0..5).map(|i| line(i, &pad)).collect();
        fs::write(dir.join("events-2026-11-19.jsonl"), text).unwrap();

        let first = next_batch(&dir, &State::default(), 100).unwrap();
        assert_eq!(first.lines.len(), 2);
        assert!(first.body().len() <= MAX_BODY);
        assert_eq!(first.end.line, 2);
        let second = next_batch(&dir, &first.end, 100).unwrap();
        assert_eq!(second.lines.len(), 2);
        let third = next_batch(&dir, &second.end, 100).unwrap();
        assert_eq!(third.lines.len(), 1);
        assert!(next_batch(&dir, &third.end, 100).is_none());

        // The frame and commas are counted: this body is exactly the cap.
        let fit = MAX_BODY - BODY_FRAME - 1 - line(0, "").trim_end().len() * 2;
        let (a, b) = (line(0, &"y".repeat(fit / 2)), line(1, &"y".repeat(fit - fit / 2)));
        fs::write(dir.join("events-2026-11-19.jsonl"), format!("{a}{b}")).unwrap();
        let both = next_batch(&dir, &State::default(), 100).unwrap();
        assert_eq!((both.lines.len(), both.body().len()), (2, MAX_BODY));
        fs::write(dir.join("events-2026-11-19.jsonl"), format!("{a}{}", line(1, &"y".repeat(fit - fit / 2 + 1)))).unwrap();
        assert_eq!(next_batch(&dir, &State::default(), 100).unwrap().lines.len(), 1);

        let huge = line(9, &"z".repeat(MAX_BODY + 10));
        fs::write(dir.join("events-2026-11-19.jsonl"), format!("{huge}{}", line(10, "a"))).unwrap();
        let alone = next_batch(&dir, &State::default(), 100).unwrap();
        assert_eq!(alone.lines.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_last_line_is_left_alone() {
        let dir = tmp("partial");
        let ev = chain(3);
        let mut text: String = ev[..2].iter().map(|e| serde_json::to_string(e).unwrap() + "\n").collect();
        text.push_str(&serde_json::to_string(&ev[2]).unwrap()[..20]);
        fs::write(dir.join("events-2026-11-19.jsonl"), &text).unwrap();
        let b = next_batch(&dir, &State::default(), 100).unwrap();
        assert_eq!(b.lines.len(), 2);
        text.truncate(text.len() - 20);
        text.push_str(&(serde_json::to_string(&ev[2]).unwrap() + "\n"));
        fs::write(dir.join("events-2026-11-19.jsonl"), &text).unwrap();
        let b2 = next_batch(&dir, &b.end, 100).unwrap();
        assert_eq!(hashes(&b2), [ev[2].hash.clone()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_line_that_does_not_parse_is_skipped() {
        let dir = tmp("garbage");
        let ev = chain(3);
        let line = |e: &Event| serde_json::to_string(e).unwrap() + "\n";
        let half = &line(&ev[1])[..30];
        fs::write(dir.join("events-2026-11-19.jsonl"), format!("{}{}\n\0\0\0\n{}{}", line(&ev[0]), half, line(&ev[1]), line(&ev[2]))).unwrap();
        let b = next_batch(&dir, &State::default(), 100).unwrap();
        assert_eq!(hashes(&b), [ev[0].hash.clone(), ev[1].hash.clone(), ev[2].hash.clone()]);
        assert!(next_batch(&dir, &b.end, 100).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_batch_ends_where_the_chain_breaks() {
        let dir = tmp("break");
        let (a, b) = (chain(3), chain(2));
        let all: Vec<Event> = a.iter().chain(&b).cloned().collect();
        write_logs(&dir, &all, &[("2026-11-19", 5)]);
        let first = next_batch(&dir, &State::default(), 100).unwrap();
        assert_eq!(first.lines.len(), 3);
        let second = next_batch(&dir, &first.end, 100).unwrap();
        assert_eq!(hashes(&second), [b[0].hash.clone(), b[1].hash.clone()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn locate_next_finds_the_last_event_that_follows_a_hash() {
        let dir = tmp("next");
        let ev = chain(4);
        let mut again = Event::new(2000.0, None, Kind::Present { input: None, keys: None, injected: None });
        again.seal(&ev[1].hash);
        let all: Vec<Event> = ev.iter().cloned().chain([again.clone()]).collect();
        write_logs(&dir, &all, &[("2026-11-18", 3), ("2026-11-19", 2)]);
        let at = locate_next(&dir, &ev[1].hash).unwrap();
        assert_eq!(at, State { last_hash: Some(ev[1].hash.clone()), file: Some("events-2026-11-19.jsonl".into()), line: 1 });
        assert_eq!(hashes(&next_batch(&dir, &at, 100).unwrap()), [again.hash.clone()]);
        let first = locate_next(&dir, GENESIS).unwrap();
        assert_eq!((first.file.as_deref(), first.line), (Some("events-2026-11-18.jsonl"), 0));
        assert!(locate_next(&dir, &again.hash).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reanchor_request_is_taken_once() {
        let dir = tmp("reanchor");
        assert!(take_reanchor(&dir).is_none());
        request_reanchor(&dir, "not a hash");
        assert!(take_reanchor(&dir).is_none());
        request_reanchor(&dir, "ab12");
        assert_eq!(take_reanchor(&dir).as_deref(), Some("ab12"));
        assert!(take_reanchor(&dir).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_roundtrip() {
        let dir = tmp("state");
        assert_eq!(State::load(&dir), State::default());
        let s = State { last_hash: Some("ab".into()), file: Some("events-x.jsonl".into()), line: 9 };
        s.save(&dir).unwrap();
        assert_eq!(State::load(&dir), s);
        let _ = fs::remove_dir_all(&dir);
    }
}
