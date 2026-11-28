//! The destination and its key are read from the saved WakaTime settings for every request, and go nowhere else.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use kairos_core::wakatime::{self, Upstream, RELAY_EXT};
use reqwest::blocking::Client;
use reqwest::header::{HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER, USER_AGENT};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};

use crate::diag::diag;

const MAX_REQUESTS: usize = 10_000;
const MAX_BYTES: u64 = 16 << 20;

#[derive(Clone, Copy)]
struct Timing {
    idle: Duration,
    first_backoff: Duration,
    max_backoff: Duration,
    timeout: Duration,
}

const TIMING: Timing = Timing {
    idle: Duration::from_secs(60),
    first_backoff: Duration::from_secs(5),
    max_backoff: Duration::from_secs(10 * 60),
    timeout: Duration::from_secs(30),
};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Entry {
    pub bulk: bool,
    pub user_agent: Option<String>,
    pub machine_name: Option<String>,
    pub timezone: Option<String>,
    #[serde(skip)]
    pub body: Vec<u8>,
}

impl Entry {
    fn encode(&self) -> Vec<u8> {
        let mut out = serde_json::to_vec(self).unwrap_or_default();
        out.push(b'\n');
        out.extend_from_slice(&self.body);
        out
    }

    fn decode(bytes: &[u8]) -> Option<Entry> {
        let end = bytes.iter().position(|&b| b == b'\n')?;
        let mut entry: Entry = serde_json::from_slice(&bytes[..end]).ok()?;
        entry.body = bytes[end + 1..].to_vec();
        Some(entry)
    }
}

struct Item {
    seq: u64,
    size: u64,
}

struct Queue {
    next: u64,
    items: VecDeque<Item>,
    bytes: u64,
}

/// One file per request, named by a counter so they sort in arrival order, written whole and renamed into place.
struct Spool {
    dir: PathBuf,
    max_requests: usize,
    max_bytes: u64,
    queue: Mutex<Queue>,
}

impl Spool {
    fn open(dir: PathBuf, max_requests: usize, max_bytes: u64) -> Spool {
        let _ = fs::create_dir_all(&dir);
        let mut items = Vec::new();
        for file in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = file.path();
            let seq = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u64>().ok());
            match (seq, path.extension().is_some_and(|e| e == RELAY_EXT), file.metadata()) {
                (Some(seq), true, Ok(meta)) => items.push(Item { seq, size: meta.len() }),
                _ => {
                    let _ = fs::remove_file(&path);
                }
            }
        }
        items.sort_by_key(|i| i.seq);
        let queue = Queue { next: items.last().map_or(0, |i| i.seq + 1), bytes: items.iter().map(|i| i.size).sum(), items: items.into() };
        let spool = Spool { dir, max_requests, max_bytes, queue: Mutex::new(queue) };
        spool.trim(&mut spool.lock());
        spool
    }

    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn path(&self, seq: u64) -> PathBuf {
        self.dir.join(format!("{seq:020}.{RELAY_EXT}"))
    }

    fn len(&self) -> usize {
        self.lock().items.len()
    }

    fn push(&self, entry: &Entry) -> io::Result<usize> {
        let bytes = entry.encode();
        let mut queue = self.lock();
        let path = self.path(queue.next);
        let tmp = path.with_extension("tmp");
        fs::create_dir_all(&self.dir)?;
        fs::write(&tmp, &bytes).and_then(|()| fs::rename(&tmp, &path)).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
        let item = Item { seq: queue.next, size: bytes.len() as u64 };
        queue.next += 1;
        queue.bytes += item.size;
        queue.items.push_back(item);
        Ok(self.trim(&mut queue))
    }

    fn trim(&self, queue: &mut Queue) -> usize {
        let mut dropped = 0;
        while queue.items.len() > self.max_requests || queue.bytes > self.max_bytes {
            let Some(old) = queue.items.pop_front() else { break };
            queue.bytes -= old.size;
            let _ = fs::remove_file(self.path(old.seq));
            dropped += 1;
        }
        dropped
    }

    fn front(&self) -> Option<(u64, Entry)> {
        loop {
            let seq = self.lock().items.front()?.seq;
            match fs::read(self.path(seq)).ok().and_then(|b| Entry::decode(&b)) {
                Some(entry) => return Some((seq, entry)),
                None => {
                    diag!("kairosd: dropped a waiting editor heartbeat request it could not read");
                    self.done(seq);
                }
            }
        }
    }

    fn done(&self, seq: u64) {
        let mut queue = self.lock();
        if let Some(at) = queue.items.iter().position(|i| i.seq == seq) {
            if let Some(item) = queue.items.remove(at) {
                queue.bytes -= item.size;
            }
        }
        let _ = fs::remove_file(self.path(seq));
    }

    fn clear(&self) {
        let mut queue = self.lock();
        for item in queue.items.drain(..) {
            let _ = fs::remove_file(self.path(item.seq));
        }
        queue.bytes = 0;
    }
}

enum Outcome {
    Sent,
    Refused(String),
    Later { why: String, after: Option<Duration> },
}

fn text_header(value: &Option<String>) -> Option<HeaderValue> {
    value.as_deref().and_then(|v| HeaderValue::from_str(v).ok())
}

fn send(client: &Client, to: &Upstream, entry: &Entry) -> Outcome {
    let Ok(mut auth) = HeaderValue::from_str(&format!("Basic {}", BASE64.encode(&to.key))) else {
        return Outcome::Refused("the saved api key can't be used".into());
    };
    auth.set_sensitive(true);
    let url = format!("{}/users/current/heartbeats{}", to.url, if entry.bulk { ".bulk" } else { "" });
    let mut request = client
        .post(url)
        .header(AUTHORIZATION, auth)
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json")
        .body(entry.body.clone());
    for (name, value) in [
        (USER_AGENT, text_header(&entry.user_agent)),
        (HeaderName::from_static("x-machine-name"), text_header(&entry.machine_name)),
        (HeaderName::from_static("timezone"), text_header(&entry.timezone)),
    ] {
        if let Some(value) = value {
            request = request.header(name, value);
        }
    }
    match request.send() {
        Ok(res) if res.status().is_success() => Outcome::Sent,
        Ok(res) => {
            let status = res.status();
            let after = res.headers().get(RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse().ok()).map(Duration::from_secs);
            if status.is_server_error() || status == 429 || status == 408 {
                Outcome::Later { why: format!("answered {status}"), after }
            } else {
                Outcome::Refused(format!("answered {status}"))
            }
        }
        Err(e) if e.is_builder() => Outcome::Refused("the saved api url can't be used".into()),
        Err(e) => Outcome::Later { why: e.without_url().to_string(), after: None },
    }
}

#[derive(Default)]
struct Told {
    failing: bool,
    refused: Option<String>,
}

struct Worker {
    data: PathBuf,
    spool: Arc<Spool>,
    client: Client,
    timing: Timing,
    failures: u32,
    told: Told,
}

enum Next {
    Idle,
    After(Duration),
}

impl Worker {
    fn backoff(&mut self, after: Option<Duration>) -> Duration {
        let t = self.timing;
        let doubled = t.first_backoff.saturating_mul(2u32.saturating_pow(self.failures.min(16)));
        self.failures = self.failures.saturating_add(1);
        after.unwrap_or(doubled).clamp(t.first_backoff, t.max_backoff)
    }

    fn drain(&mut self) -> Next {
        loop {
            let Some(to) = wakatime::upstream(&self.data) else {
                if self.spool.len() > 0 {
                    diag!("kairosd: editor heartbeats have nowhere to go on to any more, dropping the ones waiting");
                    self.spool.clear();
                }
                return Next::Idle;
            };
            let Some((seq, entry)) = self.spool.front() else {
                return Next::Idle;
            };
            match send(&self.client, &to, &entry) {
                Outcome::Sent => {
                    if self.told.failing || self.told.refused.is_some() {
                        diag!("kairosd: editor heartbeats are reaching {} again", to.host());
                    }
                    (self.failures, self.told) = (0, Told::default());
                    self.spool.done(seq);
                }
                Outcome::Refused(why) => {
                    if self.told.refused.as_ref() != Some(&why) {
                        diag!("kairosd: {} {why} to editor heartbeats, dropping the ones it turns down", to.host());
                        self.told.refused = Some(why);
                    }
                    self.spool.done(seq);
                }
                Outcome::Later { why, after } => {
                    if !self.told.failing {
                        diag!("kairosd: editor heartbeats are not reaching {} ({why}), trying again later", to.host());
                        self.told.failing = true;
                    }
                    return Next::After(self.backoff(after));
                }
            }
        }
    }

    fn run(mut self, wake: Receiver<()>) {
        loop {
            match self.drain() {
                Next::After(wait) => {
                    // A push doesn't cut the backoff short, but dropping the relay still ends the thread.
                    let until = Instant::now() + wait;
                    loop {
                        let left = until.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            break;
                        }
                        if let Err(mpsc::RecvTimeoutError::Disconnected) = wake.recv_timeout(left) {
                            return;
                        }
                    }
                }
                Next::Idle => {
                    if let Err(mpsc::RecvTimeoutError::Disconnected) = wake.recv_timeout(self.timing.idle) {
                        return;
                    }
                }
            }
        }
    }
}

pub struct Relay {
    data: PathBuf,
    spool: Arc<Spool>,
    wake: SyncSender<()>,
    told_full: AtomicBool,
    told_write: AtomicBool,
}

pub fn spawn(data: PathBuf) -> Relay {
    start(data, TIMING, MAX_REQUESTS, MAX_BYTES)
}

fn start(data: PathBuf, timing: Timing, max_requests: usize, max_bytes: u64) -> Relay {
    let spool = Arc::new(Spool::open(wakatime::relay_dir(&data), max_requests, max_bytes));
    let (wake, woken) = mpsc::sync_channel(1);
    let client = Client::builder()
        .timeout(timing.timeout)
        .connect_timeout(timing.timeout.min(Duration::from_secs(10)))
        .redirect(Policy::none())
        .user_agent(concat!("kairosd/", env!("CARGO_PKG_VERSION")))
        .build();
    match client {
        Ok(client) => {
            let worker = Worker { data: data.clone(), spool: spool.clone(), client, timing, failures: 0, told: Told::default() };
            thread::Builder::new().name("relay".into()).spawn(move || worker.run(woken)).map_or_else(|e| diag!("kairosd: not passing editor heartbeats on: {e}"), drop);
        }
        Err(e) => diag!("kairosd: not passing editor heartbeats on: {e}"),
    }
    Relay { data, spool, wake, told_full: AtomicBool::new(false), told_write: AtomicBool::new(false) }
}

impl Relay {
    pub fn push(&self, entry: Entry) {
        if wakatime::upstream(&self.data).is_none() {
            return;
        }
        match self.spool.push(&entry) {
            Ok(dropped) => {
                if dropped > 0 && !self.told_full.swap(true, Ordering::Relaxed) {
                    diag!("kairosd: too many editor heartbeats are waiting to be passed on, dropping the oldest");
                }
                let _ = self.wake.try_send(());
            }
            Err(e) => {
                if !self.told_write.swap(true, Ordering::Relaxed) {
                    diag!("kairosd: could not keep an editor heartbeat request to pass on: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;

    use kairos_core::account::{self, Account, DEFAULT_SERVER};
    use tiny_http::{Response, Server};

    use super::*;

    const KEY: &str = "11111111-2222-4333-8444-555555555555";

    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Dir {
            let dir = std::env::temp_dir().join(format!("kairos-relay-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Dir(dir)
        }

        fn upstream(&self, api_url: Option<&str>, key: Option<&str>) {
            let mut cfg = String::from("[settings]\n");
            if let Some(url) = api_url {
                cfg += &format!("api_url = {url}\n");
            }
            if let Some(key) = key {
                cfg += &format!("api_key = {key}\n");
            }
            let path = self.0.join(".wakatime.cfg");
            fs::write(&path, cfg).unwrap();
            wakatime::take(&self.0, &path).unwrap();
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Debug)]
    struct Seen {
        url: String,
        authorization: Option<String>,
        user_agent: Option<String>,
        machine_name: Option<String>,
        timezone: Option<String>,
        body: Vec<u8>,
    }

    fn fake(answers: Vec<(u16, Option<&'static str>)>) -> (String, Receiver<Seen>) {
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/api/v1", server.server_addr().to_ip().unwrap().port());
        let (tx, rx) = channel();
        thread::spawn(move || {
            let mut answers = answers.into_iter();
            for mut request in server.incoming_requests() {
                let mut body = Vec::new();
                let _ = request.as_reader().read_to_end(&mut body);
                let header = |name: &str| request.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|h| h.value.to_string());
                let seen = Seen {
                    url: request.url().to_string(),
                    authorization: header("Authorization"),
                    user_agent: header("User-Agent"),
                    machine_name: header("X-Machine-Name"),
                    timezone: header("Timezone"),
                    body,
                };
                let (status, retry_after) = answers.next().unwrap_or((202, None));
                let mut response = Response::from_string("{}").with_status_code(status);
                if let Some(after) = retry_after {
                    response = response.with_header(tiny_http::Header::from_bytes("Retry-After", after).unwrap());
                }
                let _ = request.respond(response);
                let _ = tx.send(seen);
            }
        });
        (url, rx)
    }

    fn quick() -> Timing {
        Timing { idle: Duration::from_millis(20), first_backoff: Duration::from_millis(10), max_backoff: Duration::from_millis(40), timeout: Duration::from_secs(5) }
    }

    fn beat(n: u8) -> Entry {
        Entry {
            bulk: true,
            user_agent: Some("wakatime/v1.102.1 (linux) go1.22.5 vim/9.1 vim-wakatime/11.2.0".into()),
            machine_name: Some("laptop".into()),
            timezone: Some("Europe/Berlin".into()),
            body: format!(r#"[{{"entity":"/p/a{n}.rs","time":{n}.5}}]"#).into_bytes(),
        }
    }

    fn within<T>(rx: &Receiver<T>) -> T {
        rx.recv_timeout(Duration::from_secs(10)).expect("the destination was not asked in time")
    }

    fn waiting(dir: &Dir) -> usize {
        wakatime::relay_waiting(&dir.0)
    }

    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(10), "{what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn requests_are_passed_on_as_they_came_with_the_saved_key() {
        let dir = Dir::new("forward");
        let (url, seen) = fake(vec![]);
        dir.upstream(Some(&url), Some(KEY));
        let relay = start(dir.0.clone(), quick(), 100, 1 << 20);

        relay.push(beat(1));
        let bulk = within(&seen);
        assert_eq!(bulk.url, "/api/v1/users/current/heartbeats.bulk");
        assert_eq!(bulk.authorization, Some(format!("Basic {}", BASE64.encode(KEY))));
        assert_eq!(bulk.user_agent, beat(1).user_agent);
        assert_eq!((bulk.machine_name.as_deref(), bulk.timezone.as_deref()), (Some("laptop"), Some("Europe/Berlin")));
        assert_eq!(bulk.body, beat(1).body);

        relay.push(Entry { bulk: false, body: br#"{"entity":"/p/b.rs"}"#.to_vec(), ..beat(2) });
        let single = within(&seen);
        assert_eq!(single.url, "/api/v1/users/current/heartbeats");
        assert_eq!(single.body, br#"{"entity":"/p/b.rs"}"#);
        until("the spool empties", || waiting(&dir) == 0);
    }

    #[test]
    fn a_503_is_retried_until_it_goes_through() {
        let dir = Dir::new("retry");
        let (url, seen) = fake(vec![(503, None), (429, Some("0")), (202, None)]);
        dir.upstream(Some(&url), Some(KEY));
        let relay = start(dir.0.clone(), quick(), 100, 1 << 20);

        relay.push(beat(1));
        let bodies: Vec<_> = (0..3).map(|_| within(&seen).body).collect();
        assert_eq!(bodies, vec![beat(1).body; 3], "the same request each time");
        until("the spool empties", || waiting(&dir) == 0);
    }

    #[test]
    fn a_request_the_destination_turns_down_is_dropped_and_the_next_one_goes() {
        let dir = Dir::new("refused");
        let (url, seen) = fake(vec![(401, None)]);
        dir.upstream(Some(&url), Some(KEY));
        let relay = start(dir.0.clone(), quick(), 100, 1 << 20);

        relay.push(beat(1));
        relay.push(beat(2));
        assert_eq!(within(&seen).body, beat(1).body);
        assert_eq!(within(&seen).body, beat(2).body, "the first is not tried again");
        until("the spool empties", || waiting(&dir) == 0);
        assert!(seen.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn nothing_is_passed_on_without_a_saved_key_or_when_the_saved_url_is_kairos() {
        let (url, seen) = fake(vec![]);
        let server = url.trim_end_matches("/api/v1");
        for (tag, api_url, key, signed_in_to) in [
            ("nokey", url.clone(), None, None),
            ("self", wakatime::api_url(), Some(KEY), None),
            // An editor key sends straight to the Kairos server, which already gets each heartbeat from this device.
            ("server", url.clone(), Some(KEY), Some(server)),
            ("default", format!("{DEFAULT_SERVER}/api/hackatime/v1"), Some(KEY), None),
        ] {
            let dir = Dir::new(tag);
            if let Some(server) = signed_in_to {
                account::save(&dir.0, &Account { server: server.into(), token: "t".into(), username: "ada".into() }).unwrap();
            }
            dir.upstream(Some(&api_url), key);
            let relay = start(dir.0.clone(), quick(), 100, 1 << 20);
            relay.push(beat(1));
            thread::sleep(Duration::from_millis(100));
            assert_eq!(waiting(&dir), 0, "{tag}: not even kept");
        }
        assert!(seen.try_recv().is_err());
    }

    #[test]
    fn requests_wait_while_the_destination_is_down_and_survive_a_restart() {
        let dir = Dir::new("restart");
        // Never answers, and holding the port keeps another test's server from answering on it.
        let silent = Server::http("127.0.0.1:0").unwrap();
        let down = format!("http://127.0.0.1:{}/api/v1", silent.server_addr().to_ip().unwrap().port());
        dir.upstream(Some(&down), Some(KEY));
        {
            let relay = start(dir.0.clone(), quick(), 100, 1 << 20);
            relay.push(beat(1));
            relay.push(beat(2));
            thread::sleep(Duration::from_millis(100));
            assert_eq!(waiting(&dir), 2, "kept while nothing answers");
        }

        let (url, seen) = fake(vec![]);
        dir.upstream(Some(&url), Some(KEY));
        let _relay = start(dir.0.clone(), quick(), 100, 1 << 20);
        assert_eq!(within(&seen).body, beat(1).body);
        assert_eq!(within(&seen).body, beat(2).body, "in the order they came in");
        until("the spool empties", || waiting(&dir) == 0);
    }

    #[test]
    fn a_dropped_relay_stops_retrying() {
        let dir = Dir::new("dropped");
        let (busy, _) = fake(vec![(503, None); 1000]);
        dir.upstream(Some(&busy), Some(KEY));
        let relay = start(dir.0.clone(), quick(), 100, 1 << 20);
        relay.push(beat(1));
        thread::sleep(Duration::from_millis(100));
        drop(relay);

        let (url, seen) = fake(vec![]);
        dir.upstream(Some(&url), Some(KEY));
        assert!(seen.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn waiting_requests_are_dropped_once_wakatime_is_given_back() {
        let dir = Dir::new("released");
        let (url, seen) = fake(vec![(503, None), (503, None), (503, None), (503, None)]);
        dir.upstream(Some(&url), Some(KEY));
        let relay = start(dir.0.clone(), quick(), 100, 1 << 20);
        relay.push(beat(1));
        within(&seen);
        wakatime::release(&dir.0).unwrap();
        until("the spool is cleared", || waiting(&dir) == 0);
        relay.push(beat(2));
        assert_eq!(waiting(&dir), 0);
    }

    #[test]
    fn the_oldest_requests_go_first_when_there_are_too_many() {
        let dir = Dir::new("cap");
        let spool = Spool::open(dir.0.join("spool"), 3, 1 << 20);
        let dropped: Vec<usize> = (1..=5).map(|n| spool.push(&beat(n)).unwrap()).collect();
        assert_eq!(dropped, [0, 0, 0, 1, 1]);
        assert_eq!(spool.len(), 3);
        assert_eq!(spool.front().unwrap().1.body, beat(3).body);

        let size = beat(1).encode().len() as u64;
        let small = Spool::open(dir.0.join("bytes"), 100, size * 2 + 1);
        for n in 1..=4 {
            small.push(&beat(n)).unwrap();
        }
        assert_eq!(small.len(), 2);
        assert_eq!(small.front().unwrap().1.body, beat(3).body);
        let reopened = Spool::open(dir.0.join("bytes"), 100, size * 2 + 1);
        assert_eq!((reopened.len(), reopened.front().unwrap().1), (2, beat(3)), "what is kept survives a restart in order");
        let tighter = Spool::open(dir.0.join("bytes"), 1, 1 << 20);
        assert_eq!((tighter.len(), tighter.front().unwrap().1), (1, beat(4)), "a lower limit trims what an earlier run left");
    }

    #[test]
    fn entries_keep_their_body_byte_for_byte() {
        let odd = Entry { body: b"[\n{\"a\":\"\xff\"}\r\n]\n".to_vec(), user_agent: None, ..beat(1) };
        assert_eq!(Entry::decode(&odd.encode()), Some(odd));
        assert_eq!(Entry::decode(b"no header"), None);
    }

    #[test]
    fn spooled_files_leave_no_temp_files_and_never_hold_the_key() {
        let dir = Dir::new("files");
        let spool = Spool::open(wakatime::relay_dir(&dir.0), 10, 1 << 20);
        spool.push(&beat(1)).unwrap();
        let names: Vec<_> = fs::read_dir(wakatime::relay_dir(&dir.0)).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["00000000000000000000.req"]);
        let key_in_files = fs::read_dir(wakatime::relay_dir(&dir.0)).unwrap().flatten().any(|e| fs::read_to_string(e.path()).is_ok_and(|t| t.contains(KEY)));
        assert!(!key_in_files, "the key is read from the saved settings when sending, never spooled");
    }
}
