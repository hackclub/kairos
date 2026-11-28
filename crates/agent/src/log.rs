use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use kairos_core::event::{log_files, Event, Kind, GENESIS};
use kairos_core::upload::take_reanchor;
use anyhow::{Context, Result};
use chrono::Local;
use serde_json::Value;

pub struct Log {
    dir: PathBuf,
    open: Option<(String, File)>,
    last_hash: String,
    last_focus: Option<Event>,
    // The newest log's day. Files are read back in name order, so nothing is written to an earlier one when the
    // clock goes back.
    newest: Option<String>,
}

fn day_of(path: &Path) -> Option<String> {
    Some(path.file_name()?.to_str()?.strip_prefix("events-")?.strip_suffix(".jsonl")?.to_string())
}

/// Cuts off a last line that was never finished (a power cut or a full disk mid-write), so the next event doesn't
/// land on the end of it.
fn drop_torn_tail(path: &Path) -> std::io::Result<()> {
    let bytes = fs::read(path)?;
    if bytes.last().is_none_or(|b| *b == b'\n') {
        return Ok(());
    }
    let keep = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    OpenOptions::new().write(true).open(path)?.set_len(keep as u64)
}

fn last_hash_in(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    String::from_utf8_lossy(&bytes).lines().rev().find_map(|l| {
        let v: Value = serde_json::from_str(l).ok()?;
        v.get("hash")?.as_str().map(str::to_string)
    })
}

impl Log {
    pub fn open(dir: &PathBuf) -> Result<Log> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let files = log_files(dir);
        if let Some(newest) = files.last() {
            drop_torn_tail(newest).with_context(|| format!("repairing {}", newest.display()))?;
        }
        let last_hash = files.iter().rev().find_map(|f| last_hash_in(f)).unwrap_or_else(|| GENESIS.to_string());
        let newest = files.last().and_then(|f| day_of(f));
        Ok(Log { dir: dir.clone(), open: None, last_hash, last_focus: None, newest })
    }

    pub fn append(&mut self, e: Event) -> Result<Event> {
        if let Some(hash) = take_reanchor(&self.dir) {
            self.last_hash = hash;
        }
        let today = Local::now().format("%Y-%m-%d").to_string();
        let day = self.newest.clone().filter(|n| *n > today).unwrap_or(today);
        if self.open.as_ref().map(|o| o.0.as_str()) != Some(day.as_str()) {
            let path = self.dir.join(format!("events-{day}.jsonl"));
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            let rolled = self.open.is_some();
            self.open = Some((day.clone(), file));
            self.newest = Some(day);
            if rolled {
                if let Some(mut f) = self.last_focus.clone() {
                    f.ts = e.ts;
                    self.write(f)?;
                }
            }
        }
        if matches!(e.kind, Kind::Focus { .. }) {
            self.last_focus = Some(e.clone());
        }
        self.write(e)
    }

    fn write(&mut self, mut e: Event) -> Result<Event> {
        e.seal(&self.last_hash);
        let mut line = serde_json::to_string(&e)?;
        line.push('\n');
        let (day, file) = self.open.as_mut().expect("log file open");
        if let Err(err) = file.write_all(line.as_bytes()) {
            // Part of the line may be out: it is cut off again and the file reopened, or the next event would land on
            // the end of it.
            let path = self.dir.join(format!("events-{day}.jsonl"));
            self.open = None;
            let _ = drop_torn_tail(&path);
            return Err(err.into());
        }
        self.last_hash = e.hash.clone();
        Ok(e)
    }
}

#[cfg(test)]
mod tests {
    use kairos_core::event::verify_chain;
    use kairos_core::upload::{next_batch, request_reanchor, State};

    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kairos-log-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn present(ts: f64) -> Event {
        Event::new(ts, None, Kind::Present { input: None, keys: None, injected: None })
    }

    fn sent(dir: &Path) -> Vec<String> {
        let batch = next_batch(dir, &State::default(), 1000).unwrap();
        verify_chain(batch.lines.iter().map(String::as_str), None).unwrap();
        batch.lines
    }

    #[test]
    fn a_torn_last_line_is_cut_and_the_chain_goes_on_from_the_last_whole_event() {
        let dir = tmp("torn");
        let mut log = Log::open(&dir).unwrap();
        let (a, b) = (log.append(present(1.0)).unwrap(), log.append(present(2.0)).unwrap());
        drop(log);
        let file = log_files(&dir).pop().unwrap();
        for torn in ["{\"v\":1,\"ts\":3.0,\"ki", "\0\0\0\0\0\0\0\0"] {
            let whole = fs::read(&file).unwrap();
            fs::write(&file, [whole.as_slice(), torn.as_bytes()].concat()).unwrap();
            let mut log = Log::open(&dir).unwrap();
            assert_eq!(fs::read(&file).unwrap(), whole);
            let c = log.append(present(3.0)).unwrap();
            assert_eq!(c.prev, b.hash);
            fs::write(&file, whole).unwrap();
        }
        let mut log = Log::open(&dir).unwrap();
        log.append(present(3.0)).unwrap();
        assert_eq!(sent(&dir).len(), 3);
        assert_eq!(a.prev, GENESIS);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_newest_file_does_not_restart_the_chain() {
        let dir = tmp("empty");
        let mut log = Log::open(&dir).unwrap();
        let a = log.append(present(1.0)).unwrap();
        drop(log);
        fs::write(dir.join("events-2999-01-01.jsonl"), "").unwrap();
        let b = Log::open(&dir).unwrap().append(present(2.0)).unwrap();
        assert_eq!(b.prev, a.hash);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_is_written_to_a_file_older_than_the_newest() {
        let dir = tmp("back");
        let mut ahead = present(1.0);
        ahead.seal(GENESIS);
        let future = dir.join("events-2999-01-01.jsonl");
        fs::write(&future, serde_json::to_string(&ahead).unwrap() + "\n").unwrap();
        let mut log = Log::open(&dir).unwrap();
        let next = log.append(present(2.0)).unwrap();
        assert_eq!(next.prev, ahead.hash);
        assert_eq!(log_files(&dir), [future]);
        assert_eq!(sent(&dir).len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_next_event_follows_the_hash_the_uploader_asks_for() {
        let dir = tmp("reanchor");
        let mut log = Log::open(&dir).unwrap();
        log.append(present(1.0)).unwrap();
        request_reanchor(&dir, "abc123");
        let b = log.append(present(2.0)).unwrap();
        let c = log.append(present(3.0)).unwrap();
        assert_eq!((b.prev.as_str(), c.prev.as_str()), ("abc123", b.hash.as_str()));
        let _ = fs::remove_dir_all(&dir);
    }
}
