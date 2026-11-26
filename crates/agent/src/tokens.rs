use std::collections::HashMap;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use kairos_core::account::{self, Account};
use kairos_core::exclude::Exclusions;
use kairos_core::tokens::{self, Counts, Reader, Use};
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::diag::diag;
use crate::exclusions::Excluded;
use crate::logs::{cached_project, Lookups};
use crate::now;

const FIRST: Duration = Duration::from_secs(30);
const EVERY: Duration = Duration::from_secs(5 * 60);
const DAY: f64 = 86400.0;
// Counts are read back this far when token stats are turned on. The days a bucket can still change in are kept a day
// longer, so a reply written twice is not counted again as it ages out.
const WINDOW: f64 = 30.0 * DAY;
const KEEP: f64 = 31.0 * DAY;
const BATCH: usize = 2000;
const MODEL_LEN: usize = 100;
const FILE: &str = "tokens.json";

#[derive(Debug)]
enum Fail {
    Retry(String),
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    hour: i64,
    harness: String,
    model: String,
    project: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Row {
    hour: i64,
    harness: String,
    model: String,
    project: Option<String>,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

impl Row {
    fn new(k: &Key, n: Counts) -> Row {
        Row {
            hour: k.hour,
            harness: k.harness.clone(),
            model: k.model.clone(),
            project: k.project.clone(),
            input: n.input,
            output: n.output,
            cache_read: n.cache_read,
            cache_write: n.cache_write,
        }
    }

    fn key(&self) -> Key {
        Key { hour: self.hour, harness: self.harness.clone(), model: self.model.clone(), project: self.project.clone() }
    }

    fn counts(&self) -> Counts {
        Counts { input: self.input, output: self.output, cache_read: self.cache_read, cache_write: self.cache_write }
    }
}

#[derive(Serialize, Deserialize)]
struct Saved {
    // Who the server that took these is, so another account on this computer sends everything again.
    account: String,
    hours: Vec<Row>,
}

fn plus(a: Counts, b: Counts) -> Counts {
    Counts { input: a.input + b.input, output: a.output + b.output, cache_read: a.cache_read + b.cache_read, cache_write: a.cache_write + b.cache_write }
}

fn minus(a: Counts, b: Counts) -> Counts {
    Counts {
        input: a.input.saturating_sub(b.input),
        output: a.output.saturating_sub(b.output),
        cache_read: a.cache_read.saturating_sub(b.cache_read),
        cache_write: a.cache_write.saturating_sub(b.cache_write),
    }
}

fn larger(a: Counts, b: Counts) -> Counts {
    Counts { input: a.input.max(b.input), output: a.output.max(b.output), cache_read: a.cache_read.max(b.cache_read), cache_write: a.cache_write.max(b.cache_write) }
}

fn grew(now: Counts, sent: Counts) -> bool {
    now.input > sent.input || now.output > sent.output || now.cache_read > sent.cache_read || now.cache_write > sent.cache_write
}

// Where a reply was counted, so a second line for it takes the first's place.
struct Seen {
    ts: f64,
    key: Rc<Key>,
    n: Counts,
}

#[derive(Default)]
struct Totals {
    sums: HashMap<Rc<Key>, Counts>,
    seen: HashMap<u64, Seen>,
}

impl Totals {
    // A reply seen again (a copy in a resumed session, a row rewritten) counts once, with the newest counts, in the
    // hour of its earliest line.
    fn add(&mut self, id: u64, ts: f64, key: Key, n: Counts) {
        let mut bucket = None;
        let mut at = ts;
        if let Some(old) = self.seen.remove(&id) {
            if let Some(s) = self.sums.get_mut(&old.key) {
                *s = minus(*s, old.n);
            }
            if old.ts <= ts {
                (bucket, at) = (Some(old.key), old.ts);
            }
        }
        let key = match bucket {
            Some(k) => k,
            None => match self.sums.get_key_value(&key) {
                Some((k, _)) => k.clone(),
                None => Rc::new(key),
            },
        };
        let sum = self.sums.entry(key.clone()).or_default();
        *sum = plus(*sum, n);
        self.seen.insert(id, Seen { ts: at, key, n });
    }

    fn prune(&mut self, before: f64) {
        self.seen.retain(|_, s| s.ts >= before);
        self.sums.retain(|k, _| (k.hour + 3600) as f64 >= before);
    }

    // The buckets that hold more than the server was told.
    fn changed(&self, sent: &HashMap<Key, Counts>) -> Vec<Row> {
        let mut rows: Vec<Row> = self
            .sums
            .iter()
            .filter(|(k, n)| !n.is_zero() && sent.get(&***k).map_or(true, |s| grew(**n, *s)))
            .map(|(k, n)| Row::new(k, *n))
            .collect();
        rows.sort_by(|a, b| (a.hour, &a.harness, &a.model, &a.project).cmp(&(b.hour, &b.harness, &b.model, &b.project)));
        rows
    }
}

fn hash(harness: &str, key: &str) -> u64 {
    let mut h = DefaultHasher::new();
    (harness, key).hash(&mut h);
    h.finish()
}

fn who(acct: &Account) -> String {
    format!("{} {}", acct.server.trim_end_matches('/'), acct.username)
}

type Make = Box<dyn Fn() -> Vec<Box<dyn Reader>> + Send>;

struct Tokens {
    dir: PathBuf,
    excluded: Arc<Excluded>,
    make: Make,
    // Only there while the person has token stats on.
    readers: Option<Vec<Box<dyn Reader>>>,
    totals: Totals,
    sent: HashMap<Key, Counts>,
    sent_for: Option<String>,
    projects: Lookups,
}

impl Tokens {
    fn new(dir: PathBuf, excluded: Arc<Excluded>, make: Make) -> Tokens {
        let saved: Option<Saved> = fs::read_to_string(dir.join(FILE)).ok().and_then(|t| serde_json::from_str(&t).ok());
        let (sent_for, sent) = match saved {
            Some(s) => (Some(s.account), s.hours.iter().map(|r| (r.key(), r.counts())).collect()),
            None => (None, HashMap::new()),
        };
        Tokens { dir, excluded, make, readers: None, totals: Totals::default(), sent, sent_for, projects: Lookups::new() }
    }

    fn save(&mut self, now: f64) {
        self.sent.retain(|k, _| k.hour as f64 >= now - KEEP);
        let mut hours: Vec<Row> = self.sent.iter().map(|(k, n)| Row::new(k, *n)).collect();
        hours.sort_by(|a, b| (a.hour, &a.harness, &a.model, &a.project).cmp(&(b.hour, &b.harness, &b.model, &b.project)));
        let saved = Saved { account: self.sent_for.clone().unwrap_or_default(), hours };
        let path = self.dir.join(FILE);
        let tmp = path.with_extension("json.tmp");
        let written = serde_json::to_string(&saved).map_err(std::io::Error::from).and_then(|text| fs::write(&tmp, text)).and_then(|_| fs::rename(&tmp, &path));
        if let Err(e) = written {
            diag!("kairosd: could not save {FILE}: {e}");
        }
    }

    // Turned off: nothing stays, so turning it on again starts from the logs.
    fn wipe(&mut self) {
        self.readers = None;
        self.totals = Totals::default();
        self.sent.clear();
        self.sent_for = None;
        let _ = fs::remove_file(self.dir.join(FILE));
    }

    // Some(None) is a reply with no project, None one that belongs to a folder that is excluded.
    fn project(&mut self, ex: &Exclusions, cwd: Option<&Path>) -> Option<Option<String>> {
        let Some(cwd) = cwd else { return Some(None) };
        if ex.covers(cwd) {
            return None;
        }
        let project = cached_project(&mut self.projects, &cwd.to_path_buf());
        if project.as_ref().is_some_and(|p| ex.covers(&p.root)) {
            return None;
        }
        Some(project.map(|p| p.id))
    }

    fn collect(&mut self, now: f64) {
        let since = now - WINDOW;
        let mut uses: Vec<Use> = Vec::new();
        for r in self.readers.iter_mut().flatten() {
            uses.extend(r.read(since));
        }
        // Read after the readers, so a folder excluded while they ran is on the list.
        let ex = self.excluded.current();
        for u in uses.into_iter().filter(|u| u.ts >= since) {
            let Some(project) = self.project(&ex, u.cwd.as_deref()) else { continue };
            let key = Key {
                hour: (u.ts as i64).div_euclid(3600) * 3600,
                harness: u.harness.to_string(),
                model: u.model.chars().take(MODEL_LEN).collect(),
                project,
            };
            self.totals.add(hash(u.harness, &u.key), u.ts, key, u.n);
        }
        self.totals.prune(now - KEEP);
    }

    // Whether the server wants token stats: what it answers to the buckets that grew, or to a question when none did.
    fn sync(&mut self, acct: &Account, client: &Client, now: f64) -> Result<bool, Fail> {
        let url = format!("{}/api/v1/tokens", acct.server.trim_end_matches('/'));
        let me = who(acct);
        if self.sent_for.as_ref() != Some(&me) {
            self.sent.clear();
            self.sent_for = Some(me);
        }
        let rows = self.totals.changed(&self.sent);
        if rows.is_empty() {
            return answer(client.get(&url).bearer_auth(&acct.token));
        }
        let mut on = true;
        for batch in rows.chunks(BATCH) {
            let body = json!({ "hours": batch }).to_string();
            on = answer(client.post(&url).bearer_auth(&acct.token).header("Content-Type", "application/json").body(body))?;
            for r in batch {
                let sent = self.sent.entry(r.key()).or_default();
                *sent = larger(*sent, r.counts());
            }
            self.save(now);
            if !on {
                break;
            }
        }
        Ok(on)
    }

    fn tick(&mut self, acct: &Account, client: &Client, now: f64) -> Result<(), Fail> {
        if self.readers.is_some() {
            self.collect(now);
        }
        let mut on = self.sync(acct, client, now)?;
        if on && self.readers.is_none() {
            self.readers = Some((self.make)());
            self.collect(now);
            on = self.sync(acct, client, now)?;
        }
        if !on {
            self.wipe();
        }
        Ok(())
    }
}

fn answer(req: RequestBuilder) -> Result<bool, Fail> {
    let res = req.send().map_err(|e| Fail::Retry(e.to_string()))?;
    match res.status() {
        StatusCode::OK => {
            let v: Value = res.json().map_err(|e| Fail::Retry(format!("unreadable answer: {e}")))?;
            v["on"].as_bool().ok_or_else(|| Fail::Retry("the answer has no \"on\"".into()))
        }
        StatusCode::UNAUTHORIZED => Err(Fail::Rejected),
        s => Err(Fail::Retry(format!("server answered {s}"))),
    }
}

/// Counts the harnesses logged for each reply, sent by the hour to the server while the person has token stats on.
pub fn spawn(dir: PathBuf, excluded: Arc<Excluded>) {
    thread::spawn(move || {
        let client = match Client::builder().timeout(Duration::from_secs(30)).user_agent(concat!("kairosd/", env!("CARGO_PKG_VERSION"))).build() {
            Ok(c) => c,
            Err(e) => return diag!("kairosd: token stats disabled: {e}"),
        };
        let mut state = Tokens::new(dir.clone(), excluded, Box::new(tokens::all));
        let mut halted: Option<Account> = None;
        let mut told: Option<String> = None;
        let mut wait = FIRST;
        loop {
            thread::sleep(wait);
            wait = EVERY;
            let Some(acct) = account::load(&dir) else { continue };
            if halted.as_ref() == Some(&acct) {
                continue;
            }
            match state.tick(&acct, &client, now()) {
                Ok(()) => told = None,
                Err(Fail::Retry(e)) => {
                    if told.as_ref() != Some(&e) {
                        diag!("kairosd: token stats failed: {e}, trying again in {}s", EVERY.as_secs());
                    }
                    told = Some(e);
                }
                Err(Fail::Rejected) => {
                    diag!("kairosd: token stats stopped: the server rejected the token");
                    halted = Some(acct);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use kairos_core::exclude;

    use super::*;

    const NOW: f64 = 1_795_512_000.0;
    const HOUR: i64 = 1_795_510_800;

    #[derive(Default)]
    struct Srv {
        on: bool,
        gets: usize,
        posts: Vec<Value>,
        force: Option<u16>,
    }

    fn read_request(s: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let head_end = loop {
            let n = s.read(&mut chunk).ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let (mut len, mut auth) = (0, String::new());
        for line in head.lines().skip(1) {
            if let Some((k, v)) = line.split_once(':') {
                match k.to_ascii_lowercase().as_str() {
                    "content-length" => len = v.trim().parse().unwrap_or(0),
                    "authorization" => auth = v.trim().to_string(),
                    _ => {}
                }
            }
        }
        while buf.len() < head_end + len {
            let n = s.read(&mut chunk).ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let request_line = head.lines().next()?.to_string();
        Some((request_line, auth, buf[head_end..head_end + len].to_vec()))
    }

    fn reply(s: &mut TcpStream, code: u16, body: &Value) {
        let body = body.to_string();
        let _ = write!(s, "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    }

    fn stub(srv: Arc<Mutex<Srv>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let Some((line, auth, body)) = read_request(&mut s) else { continue };
                let mut srv = srv.lock().unwrap();
                if auth != "Bearer tok" {
                    reply(&mut s, 401, &json!({ "error": "unauthorized" }));
                    continue;
                }
                if let Some(code) = srv.force {
                    reply(&mut s, code, &json!({}));
                    continue;
                }
                match line.split(' ').take(2).collect::<Vec<_>>()[..] {
                    ["GET", "/api/v1/tokens"] => srv.gets += 1,
                    ["POST", "/api/v1/tokens"] => {
                        let v: Value = serde_json::from_slice(&body).unwrap();
                        assert!(v["hours"].as_array().unwrap().len() <= 2000);
                        srv.posts.push(v);
                    }
                    _ => {
                        reply(&mut s, 404, &json!({}));
                        continue;
                    }
                }
                let on = srv.on;
                reply(&mut s, 200, &json!({ "on": on }));
            }
        });
        url
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kairos-agent-tokens-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn account(url: &str) -> Account {
        Account { server: url.to_string(), token: "tok".into(), username: "u".into() }
    }

    fn folder(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join("work").join(name);
        fs::create_dir_all(p.join(".git")).unwrap();
        p
    }

    fn reply_at(ts: f64, harness: &'static str, model: &str, cwd: Option<&Path>, key: &str, n: [u64; 4]) -> Use {
        let n = Counts { input: n[0], output: n[1], cache_read: n[2], cache_write: n[3] };
        Use { ts, harness, model: model.to_string(), cwd: cwd.map(Path::to_path_buf), key: key.to_string(), n }
    }

    struct Stub(VecDeque<Vec<Use>>);

    impl Reader for Stub {
        fn read(&mut self, _: f64) -> Vec<Use> {
            self.0.pop_front().unwrap_or_default()
        }
    }

    // Readers that give each batch in turn, built again every time the agent builds them.
    fn readers(batches: Vec<Vec<Use>>) -> (Make, Arc<AtomicUsize>) {
        let built = Arc::new(AtomicUsize::new(0));
        let count = built.clone();
        let make: Make = Box::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            vec![Box::new(Stub(batches.clone().into())) as Box<dyn Reader>]
        });
        (make, built)
    }

    fn state(dir: &Path, make: Make) -> Tokens {
        Tokens::new(dir.to_path_buf(), Arc::new(Excluded::new(dir.to_path_buf())), make)
    }

    type Posted = HashMap<(i64, String, String, Option<String>), [u64; 4]>;

    fn posted(v: &Value) -> Posted {
        v["hours"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                let n = |k: &str| r[k].as_u64().unwrap();
                ((r["hour"].as_i64().unwrap(), r["harness"].as_str().unwrap().into(), r["model"].as_str().unwrap().into(), r["project"].as_str().map(str::to_string)), [n("input"), n("output"), n("cache_read"), n("cache_write")])
            })
            .collect()
    }

    fn saved(dir: &Path) -> Saved {
        serde_json::from_str(&fs::read_to_string(dir.join(FILE)).unwrap()).unwrap()
    }

    fn key(hour: i64, model: &str) -> Key {
        Key { hour, harness: "h".into(), model: model.into(), project: None }
    }

    #[test]
    fn a_reply_seen_again_takes_the_place_of_the_first_in_the_hour_of_the_earliest() {
        let mut t = Totals::default();
        let n = |input| Counts { input, output: 1, cache_read: 0, cache_write: 0 };
        t.add(1, 10_000.0, key(7200, "m"), n(5));
        t.add(2, 10_100.0, key(7200, "m"), n(7));
        assert_eq!(t.changed(&HashMap::new())[0].input, 12);
        t.add(1, 10_050.0, key(7200, "m"), n(9));
        assert_eq!(t.changed(&HashMap::new())[0].input, 16, "the newest counts, once");
        t.add(1, 3600.0, key(3600, "m"), n(9));
        let rows = t.changed(&HashMap::new());
        assert_eq!(rows.iter().map(|r| (r.hour, r.input)).collect::<Vec<_>>(), [(3600, 9), (7200, 7)], "the earlier line's hour");
        t.add(1, 10_000.0, key(7200, "m"), n(9));
        assert_eq!(t.changed(&HashMap::new()).iter().map(|r| (r.hour, r.input)).collect::<Vec<_>>(), [(3600, 9), (7200, 7)]);
        t.prune(8000.0);
        assert_eq!(t.changed(&HashMap::new()).len(), 1);
        assert_eq!(t.seen.len(), 1);
    }

    #[test]
    fn replies_are_bucketed_by_hour_harness_model_and_project_and_sent_as_totals() {
        let dir = tmp("bucket");
        let alpha = folder(&dir, "alpha");
        let long_model = "x".repeat(150);
        let uses = vec![
            reply_at(NOW - 100.0, "claude-code", "opus", Some(&alpha), "k1", [1, 2, 3, 4]),
            reply_at(NOW - 50.0, "claude-code", "opus", Some(&alpha), "k2", [10, 20, 30, 40]),
            reply_at(NOW - 10.0, "claude-code", "opus", Some(&alpha), "k1", [5, 6, 7, 8]),
            reply_at(NOW - 5.0 * 3600.0, "claude-code", "opus", Some(&alpha), "k3", [1, 1, 1, 1]),
            reply_at(NOW - 31.0 * 86400.0, "claude-code", "opus", Some(&alpha), "old", [99, 99, 99, 99]),
            reply_at(NOW - 20.0, "codex", "gpt", None, "k4", [2, 0, 0, 0]),
            reply_at(NOW - 20.0, "codex", &long_model, Some(&alpha), "k5", [3, 0, 0, 0]),
        ];
        let srv = Arc::new(Mutex::new(Srv { on: true, ..Srv::default() }));
        let acct = account(&stub(srv.clone()));
        let (make, built) = readers(vec![uses]);
        let mut t = state(&dir, make);
        let client = Client::new();

        t.tick(&acct, &client, NOW).unwrap();
        let s = srv.lock().unwrap();
        assert_eq!((s.gets, s.posts.len(), built.load(Ordering::SeqCst)), (1, 1, 1));
        let got = posted(&s.posts[0]);
        let a = Some("alpha".to_string());
        assert_eq!(got.len(), 4);
        assert_eq!(got[&(HOUR, "claude-code".into(), "opus".into(), a.clone())], [15, 26, 37, 48]);
        assert_eq!(got[&(HOUR - 5 * 3600, "claude-code".into(), "opus".into(), a.clone())], [1, 1, 1, 1]);
        assert_eq!(got[&(HOUR, "codex".into(), "gpt".into(), None)], [2, 0, 0, 0]);
        assert_eq!(got[&(HOUR, "codex".into(), "x".repeat(100), a)], [3, 0, 0, 0]);
        let file = saved(&dir);
        assert_eq!((file.account.as_str(), file.hours.len()), (who(&acct).as_str(), 4));
        drop(s);

        t.tick(&acct, &client, NOW + 300.0).unwrap();
        let s = srv.lock().unwrap();
        assert_eq!((s.gets, s.posts.len()), (2, 1), "nothing grew, so only the question");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn replies_from_excluded_folders_are_dropped() {
        let dir = tmp("excluded");
        let (alpha, secret) = (folder(&dir, "alpha"), folder(&dir, "secret"));
        exclude::add(&dir, &secret).unwrap();
        let uses = vec![
            reply_at(NOW - 10.0, "claude-code", "opus", Some(&alpha), "k1", [1, 1, 1, 1]),
            reply_at(NOW - 10.0, "claude-code", "opus", Some(&secret), "k2", [100, 100, 100, 100]),
            reply_at(NOW - 10.0, "claude-code", "opus", Some(&secret.join("deep/er")), "k3", [100, 100, 100, 100]),
        ];
        let srv = Arc::new(Mutex::new(Srv { on: true, ..Srv::default() }));
        let acct = account(&stub(srv.clone()));
        let mut t = state(&dir, readers(vec![uses]).0);
        t.tick(&acct, &Client::new(), NOW).unwrap();
        let s = srv.lock().unwrap();
        assert_eq!(posted(&s.posts[0]).into_values().collect::<Vec<_>>(), [[1, 1, 1, 1]]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn turning_it_off_wipes_everything_and_turning_it_on_again_starts_from_the_logs() {
        let dir = tmp("off");
        let alpha = folder(&dir, "alpha");
        let first = vec![reply_at(NOW - 10.0, "claude-code", "opus", Some(&alpha), "k1", [1, 1, 1, 1])];
        let later = vec![reply_at(NOW + 60.0, "claude-code", "opus", Some(&alpha), "k2", [2, 2, 2, 2])];
        let srv = Arc::new(Mutex::new(Srv { on: true, ..Srv::default() }));
        let acct = account(&stub(srv.clone()));
        let (make, built) = readers(vec![first, later]);
        let mut t = state(&dir, make);
        let client = Client::new();

        t.tick(&acct, &client, NOW).unwrap();
        assert!(dir.join(FILE).exists() && t.readers.is_some());
        srv.lock().unwrap().on = false;
        // The server turned it off while a new reply came in: the buckets are sent, it says no.
        t.tick(&acct, &client, NOW + 300.0).unwrap();
        assert!(!dir.join(FILE).exists(), "the file is gone");
        assert!(t.readers.is_none() && t.totals.sums.is_empty() && t.totals.seen.is_empty() && t.sent.is_empty());
        let before = srv.lock().unwrap().posts.len();
        t.tick(&acct, &client, NOW + 600.0).unwrap();
        assert_eq!((srv.lock().unwrap().posts.len(), built.load(Ordering::SeqCst)), (before, 1), "off reads nothing");

        srv.lock().unwrap().on = true;
        t.tick(&acct, &client, NOW + 900.0).unwrap();
        let s = srv.lock().unwrap();
        assert_eq!(built.load(Ordering::SeqCst), 2);
        let got = posted(s.posts.last().unwrap());
        assert_eq!(got[&(HOUR, "claude-code".into(), "opus".into(), Some("alpha".into()))], [1, 1, 1, 1], "backfilled from scratch");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn more_than_2000_buckets_go_in_several_requests_and_each_is_saved() {
        let dir = tmp("batches");
        let uses: Vec<Use> = (0..4500).map(|i| reply_at(NOW - 10.0, "codex", &format!("m{i:04}"), None, &format!("k{i}"), [i + 1, 0, 0, 0])).collect();
        let srv = Arc::new(Mutex::new(Srv { on: true, ..Srv::default() }));
        let acct = account(&stub(srv.clone()));
        let mut t = state(&dir, readers(vec![uses]).0);
        t.tick(&acct, &Client::new(), NOW).unwrap();
        let s = srv.lock().unwrap();
        assert_eq!(s.posts.iter().map(|p| p["hours"].as_array().unwrap().len()).collect::<Vec<_>>(), [2000, 2000, 500]);
        let all: Posted = s.posts.iter().flat_map(posted).collect();
        assert_eq!(all.len(), 4500);
        assert_eq!(all[&(HOUR, "codex".into(), "m0499".into(), None)], [500, 0, 0, 0]);
        assert_eq!(saved(&dir).hours.len(), 4500);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn after_a_restart_only_the_buckets_that_grew_are_sent() {
        let dir = tmp("restart");
        let alpha = folder(&dir, "alpha");
        let uses = |a: u64, b: u64| {
            vec![
                reply_at(NOW - 10.0, "claude-code", "opus", Some(&alpha), "k1", [a, 0, 0, 0]),
                reply_at(NOW - 10.0, "claude-code", "sonnet", Some(&alpha), "k2", [b, 0, 0, 0]),
                reply_at(NOW - 10.0, "codex", "gpt", Some(&alpha), "k3", [7, 0, 0, 0]),
            ]
        };
        let srv = Arc::new(Mutex::new(Srv { on: true, ..Srv::default() }));
        let acct = account(&stub(srv.clone()));
        let client = Client::new();
        state(&dir, readers(vec![uses(5, 6)]).0).tick(&acct, &client, NOW).unwrap();
        assert_eq!(srv.lock().unwrap().posts.len(), 1);

        // Restarted with the same logs: nothing to send.
        state(&dir, readers(vec![uses(5, 6)]).0).tick(&acct, &client, NOW + 600.0).unwrap();
        assert_eq!(srv.lock().unwrap().posts.len(), 1);

        // One bucket grew, one is smaller than what the server has (a log was cleaned up).
        state(&dir, readers(vec![uses(9, 2)]).0).tick(&acct, &client, NOW + 1200.0).unwrap();
        let s = srv.lock().unwrap();
        assert_eq!(s.posts.len(), 2);
        let got = posted(&s.posts[1]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[&(HOUR, "claude-code".into(), "opus".into(), Some("alpha".into()))], [9, 0, 0, 0]);
        // The file keeps the larger one, as the server does.
        let file = saved(&dir);
        let sonnet = file.hours.iter().find(|r| r.model == "sonnet").unwrap();
        assert_eq!(sonnet.input, 6);
        drop(s);

        // Another account on this computer has none of it.
        let mut other = account(&srv_url(&srv));
        other.username = "someone-else".into();
        state(&dir, readers(vec![uses(9, 2)]).0).tick(&other, &client, NOW + 1800.0).unwrap();
        assert_eq!(posted(srv.lock().unwrap().posts.last().unwrap()).len(), 3);
        let _ = fs::remove_dir_all(&dir);
    }

    fn srv_url(srv: &Arc<Mutex<Srv>>) -> String {
        stub(srv.clone())
    }

    #[test]
    fn a_bad_token_halts_and_an_old_or_dead_server_is_retried() {
        let dir = tmp("errors");
        let srv = Arc::new(Mutex::new(Srv { on: true, ..Srv::default() }));
        let url = stub(srv.clone());
        let mut bad = account(&url);
        bad.token = "nope".into();
        let mut t = state(&dir, readers(vec![]).0);
        assert!(matches!(t.tick(&bad, &Client::new(), NOW), Err(Fail::Rejected)));

        srv.lock().unwrap().force = Some(404);
        assert!(matches!(t.tick(&account(&url), &Client::new(), NOW), Err(Fail::Retry(e)) if e.contains("404")));

        let dead = TcpListener::bind("127.0.0.1:0").unwrap();
        let gone = format!("http://{}", dead.local_addr().unwrap());
        drop(dead);
        assert!(matches!(t.tick(&account(&gone), &Client::new(), NOW), Err(Fail::Retry(_))));
        assert!(t.readers.is_none(), "nothing is read until the server says yes");
        let _ = fs::remove_dir_all(&dir);
    }
}
