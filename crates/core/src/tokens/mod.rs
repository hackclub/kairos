//! Token counts the harnesses log for each reply, read from the stores Kairos already reads for focus time.
//! Only counts, the model's name and the folder come out of here, never what was said.

mod antigravity;
mod claude;
mod codex;
mod opencode;

use std::path::PathBuf;

use crate::sources::{self, Tail};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Counts {
    pub fn is_zero(&self) -> bool {
        *self == Counts::default()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Use {
    pub ts: f64,
    pub harness: &'static str,
    pub model: String,
    pub cwd: Option<PathBuf>,
    /// Names one model reply across files, so the copies a harness makes of it are one reply.
    pub key: String,
    pub n: Counts,
}

pub trait Reader: Send {
    /// The first call returns everything at or after `since`, later ones what was written since the call before.
    fn read(&mut self, since: f64) -> Vec<Use>;
}

pub fn all() -> Vec<Box<dyn Reader>> {
    vec![
        Box::new(claude::Claude::new(sources::claude::default_roots())),
        Box::new(codex::Codex::new(sources::codex::default_roots())),
        Box::new(opencode::OpenCode::new(sources::opencode::default_db())),
        Box::new(antigravity::Antigravity::new(sources::antigravity::default_roots())),
    ]
}

// A Tail that starts at the beginning of every file written since `since`, which a fresh one would skip.
pub(crate) fn tail_since(tail: Tail, since: f64) -> Tail {
    let mut tail = tail;
    tail.restore(&serde_json::json!({ "since": since }));
    tail
}

// Replies with no model, no counts or from before `since` are not worth sending.
pub(crate) fn keep(u: Use, since: f64) -> Option<Use> {
    (u.ts >= since && !u.n.is_zero() && !u.model.is_empty() && u.model != "<synthetic>").then_some(u)
}

pub(crate) fn unix(ts: Option<&str>) -> Option<f64> {
    let d = chrono::DateTime::parse_from_rfc3339(ts?).ok()?;
    Some(d.timestamp_millis() as f64 / 1000.0)
}

#[cfg(test)]
pub(crate) mod testing {
    use std::fs;
    use std::path::{Path, PathBuf};

    pub fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kairos-tokens-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    pub fn append(path: &Path, text: &str) {
        use std::io::Write;
        fs::OpenOptions::new().append(true).open(path).unwrap().write_all(text.as_bytes()).unwrap();
    }
}

/// Reads this computer's own logs, read only, and prints counts per harness and model as JSON, nothing else:
/// `cargo test -p kairos-core --release --lib real_logs -- --ignored --nocapture`
#[cfg(test)]
#[test]
#[ignore]
fn real_logs() {
    use std::collections::{BTreeMap, HashMap};

    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
    let since = now - 30.0 * 86400.0;
    let mut out: BTreeMap<&str, BTreeMap<String, [u64; 4]>> = BTreeMap::new();
    let mut replies: BTreeMap<&str, usize> = BTreeMap::new();
    for mut reader in all() {
        // A file is read at most 64 MB at a time, so a big backlog takes a few reads. OpenCode reads its newest
        // rows again each time, so this stops once a read brings nothing new.
        let mut seen: HashMap<String, Use> = HashMap::new();
        loop {
            let mut changed = false;
            for u in reader.read(since) {
                changed |= seen.get(&u.key) != Some(&u);
                seen.insert(u.key.clone(), u);
            }
            if !changed {
                break;
            }
        }
        for u in seen.values() {
            replies.entry(u.harness).and_modify(|n| *n += 1).or_insert(1);
            let m = out.entry(u.harness).or_default().entry(u.model.clone()).or_default();
            for (i, v) in [u.n.input, u.n.output, u.n.cache_read, u.n.cache_write].into_iter().enumerate() {
                m[i] += v;
            }
        }
    }
    println!("REAL_LOGS {}", serde_json::json!({ "replies": replies, "totals": out }));
}
