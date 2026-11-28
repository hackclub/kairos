use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::IgnoredAny;
use serde_json::{json, Map, Value};
use walkdir::WalkDir;

use super::Health;

const MAX_READ: u64 = 64 * 1024 * 1024;

// Before the first poll (and with no restored state) existing content is skipped, so installing the app never imports old history.
// Every store read this way holds one JSON value per line.
pub struct Tail {
    roots: Vec<PathBuf>,
    list: Box<dyn Fn(&Path, &mut Vec<PathBuf>) + Send>,
    offsets: HashMap<PathBuf, u64>,
    since: Option<f64>,
    health: Health,
}

fn mtime(p: &Path) -> f64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64())
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

impl Tail {
    pub fn new(roots: Vec<PathBuf>, ext: &'static str, skip: fn(&Path) -> bool) -> Tail {
        Tail::with(roots, move |root, files| {
            for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
                let p = entry.path();
                if entry.file_type().is_file() && p.extension().and_then(|e| e.to_str()) == Some(ext) && !skip(p) {
                    files.push(p.to_path_buf());
                }
            }
        })
    }

    // For stores where walking every root each poll would be too slow.
    pub fn with(roots: Vec<PathBuf>, list: impl Fn(&Path, &mut Vec<PathBuf>) + Send + 'static) -> Tail {
        Tail { roots, list: Box::new(list), offsets: HashMap::new(), since: None, health: Health::default() }
    }

    pub fn poll(&mut self, mut each: impl FnMut(&Path, &str)) {
        self.poll_lines(|path, line, _| each(path, line));
    }

    /// Also says whether the read began at the file's start: a new file, or one rewritten or put back since the last
    /// poll, which can hold lines already read before the rewrite or under another name.
    pub fn poll_lines(&mut self, mut each: impl FnMut(&Path, &str, bool)) {
        let started = now();
        let mut files = Vec::new();
        for root in &self.roots {
            (self.list)(root, &mut files);
        }
        for path in files {
            let Ok(len) = std::fs::metadata(&path).map(|m| m.len()) else { continue };
            let off = match (self.offsets.get(&path), self.since) {
                (Some(&o), _) => o,
                (None, None) => {
                    self.offsets.insert(path, len);
                    continue;
                }
                (None, Some(since)) if mtime(&path) < since - 5.0 => {
                    self.offsets.insert(path, len);
                    continue;
                }
                (None, Some(_)) => 0,
            };
            let off = if len < off { 0 } else { off };
            if len == off {
                self.offsets.insert(path, off);
                continue;
            }
            let Ok(mut f) = File::open(&path) else { continue };
            if f.seek(SeekFrom::Start(off)).is_err() {
                continue;
            }
            let mut buf = Vec::new();
            if f.take((len - off).min(MAX_READ)).read_to_end(&mut buf).is_err() {
                continue;
            }
            let Some(end) = buf.iter().rposition(|&b| b == b'\n') else {
                self.offsets.insert(path, off);
                continue;
            };
            for line in String::from_utf8_lossy(&buf[..end]).lines() {
                if !line.trim().is_empty() {
                    self.health.read += 1;
                    if serde_json::from_str::<IgnoredAny>(line).is_err() {
                        self.health.unreadable += 1;
                    }
                    each(&path, line, off == 0);
                }
            }
            self.offsets.insert(path, off + end as u64 + 1);
        }
        self.offsets.retain(|p, _| p.exists());
        self.since = Some(started);
    }

    pub fn health(&mut self) -> Health {
        std::mem::take(&mut self.health)
    }

    pub fn state(&self) -> Value {
        let files: Map<String, Value> =
            self.offsets.iter().map(|(p, o)| (p.to_string_lossy().into_owned(), json!(o))).collect();
        json!({ "since": self.since, "files": files })
    }

    pub fn restore(&mut self, state: &Value) {
        if let Some(files) = state.get("files").and_then(Value::as_object) {
            for (p, o) in files {
                if let Some(o) = o.as_u64() {
                    self.offsets.insert(PathBuf::from(p), o);
                }
            }
        }
        if let Some(s) = state.get("since").and_then(Value::as_f64) {
            self.since = Some(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn health_counts_the_lines_read_and_the_ones_that_are_not_json() {
        let dir = std::env::temp_dir().join(format!("kairos-tail-health-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s.jsonl");
        std::fs::write(&file, "{\"old\":1}\n").unwrap();
        let mut tail = Tail::new(vec![dir.clone()], "jsonl", |_| false);
        tail.poll(|_, _| {});
        assert_eq!(tail.health(), Health { read: 0, unreadable: 0 }, "what was there before isn't read");
        let mut f = std::fs::OpenOptions::new().append(true).open(&file).unwrap();
        f.write_all(b"{\"a\":1}\n{\"b\":\nnot json\n\n{\"c\":[1,2]}\n{\"half\":").unwrap();
        let mut lines = 0;
        tail.poll(|_, _| lines += 1);
        assert_eq!((lines, tail.health()), (4, Health { read: 4, unreadable: 2 }), "a line still being written waits");
        assert_eq!(tail.health(), Health::default(), "taken once");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
