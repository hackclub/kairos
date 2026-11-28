//! Sends the sealed log as kairosd recorded it. Nothing on the way should add to it or change it.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use kairos_core::account::{self, Account};
use kairos_core::upload::{self, State};
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde_json::Value;

use crate::diag::diag;
use crate::status::{self, Shared};
use crate::now;

const FIRST: Duration = Duration::from_secs(5);
const EVERY: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);
const REFUSED_EVERY: Duration = Duration::from_secs(3600);
const BATCH: usize = 5000;
const REJECTED: &str = "the server rejected the token, run kairos login again";

#[derive(Debug)]
enum Fail {
    Retry(String),
    /// The server won't take this batch as it is. Tried again now and then: a clock that ran ahead catches up, a
    /// proxy that answered for the server goes away.
    Refused(String),
    Rejected,
}

enum Reply {
    Accepted,
    Mismatch(Option<String>),
}

fn post(client: &Client, acct: &Account, body: String) -> Result<Reply, Fail> {
    let res = client
        .post(format!("{}/api/v1/events", acct.server.trim_end_matches('/')))
        .bearer_auth(&acct.token)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .map_err(|e| Fail::Retry(e.to_string()))?;
    let status = res.status();
    match status {
        StatusCode::OK => Ok(Reply::Accepted),
        StatusCode::CONFLICT => {
            let v: Value = res.json().map_err(|e| Fail::Retry(format!("unreadable 409 body: {e}")))?;
            Ok(Reply::Mismatch(v["last_hash"].as_str().map(str::to_string)))
        }
        StatusCode::UNAUTHORIZED => Err(Fail::Rejected),
        s if s.is_server_error() || s == StatusCode::TOO_MANY_REQUESTS || s == StatusCode::REQUEST_TIMEOUT => {
            Err(Fail::Retry(format!("server answered {s}")))
        }
        s => Err(Fail::Refused(format!("server answered {s}: {}", res.text().unwrap_or_default().chars().take(300).collect::<String>()))),
    }
}

fn sync(dir: &Path, acct: &Account, client: &Client, max: usize) -> Result<usize, Fail> {
    let mut state = State::load(dir);
    let mut sent = 0;
    let mut stuck = false;
    let save = |s: &State| s.save(dir).map_err(|e| Fail::Retry(format!("saving upload.json: {e}")));
    loop {
        upload::flush(dir);
        let Some(batch) = upload::next_batch(dir, &state, max) else {
            return Ok(sent);
        };
        match post(client, acct, batch.body())? {
            Reply::Accepted => {
                sent += batch.lines.len();
                state = batch.end;
                stuck = false;
                save(&state)?;
            }
            Reply::Mismatch(server_last) => {
                let resume = match &server_last {
                    None => Some(State::default()),
                    Some(h) => upload::locate_next(dir, h).or_else(|| upload::locate(dir, h)),
                };
                match resume {
                    Some(at) if !stuck => {
                        stuck = true;
                        state = at;
                        save(&state)?;
                    }
                    // The log doesn't continue from what the server has (a tail lost in a power cut after it was
                    // uploaded, files out of order): the recorder chains its next event from the server's last one,
                    // and what can't be linked to it is left behind.
                    _ => {
                        if let Some(h) = &server_last {
                            upload::request_reanchor(dir, h);
                        }
                        return Err(Fail::Retry("the local log does not continue from the server's last event, picking up from it".into()));
                    }
                }
            }
        }
    }
}

pub fn spawn(dir: PathBuf, status: Shared) {
    thread::spawn(move || {
        let client = match Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("kairosd/", env!("CARGO_PKG_VERSION")))
            .build()
        {
            Ok(c) => c,
            Err(e) => return diag!("kairosd: upload disabled: {e}"),
        };
        let mut wait = FIRST;
        let mut backoff = EVERY;
        let mut halted: Option<Account> = None;
        loop {
            thread::sleep(wait);
            wait = EVERY;
            let acct = account::load(&dir);
            status::set(&status, |s| {
                s.username = acct.as_ref().map(|a| a.username.clone());
                s.server = acct.as_ref().map(|a| a.server.clone());
            });
            let Some(acct) = acct else { continue };
            if halted.as_ref() == Some(&acct) {
                continue;
            }
            match sync(&dir, &acct, &client, BATCH) {
                Ok(n) => {
                    backoff = EVERY;
                    if n > 0 {
                        diag!("kairosd: uploaded {n} events");
                    }
                    status::set(&status, |s| {
                        s.last_upload = Some(now());
                        s.upload_error = None;
                        s.token_rejected = false;
                    });
                }
                Err(Fail::Retry(e)) => {
                    diag!("kairosd: upload failed: {e}, retrying in {}s", backoff.as_secs());
                    status::set(&status, |s| s.upload_error = Some(e));
                    wait = backoff;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
                Err(Fail::Refused(e)) => {
                    diag!("kairosd: upload refused: {e}, trying again in {}s", REFUSED_EVERY.as_secs());
                    status::set(&status, |s| s.upload_error = Some(e));
                    wait = REFUSED_EVERY;
                }
                Err(Fail::Rejected) => {
                    diag!("kairosd: upload stopped: the server rejected the token");
                    status::set(&status, |s| {
                        s.upload_error = Some(REJECTED.into());
                        s.token_rejected = true;
                    });
                    halted = Some(acct);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    use kairos_core::event::{Event, Kind, GENESIS};

    use super::*;

    #[derive(Default)]
    struct Srv {
        last: Option<String>,
        got: Vec<String>,
        posts: usize,
        force: Option<Option<String>>,
    }

    fn read_request(s: &mut TcpStream) -> Option<(String, Vec<u8>)> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
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
        for line in head.lines() {
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
        Some((auth, buf[head_end..head_end + len].to_vec()))
    }

    fn reply(s: &mut TcpStream, code: u16, body: &Value) {
        let body = body.to_string();
        let _ = write!(
            s,
            "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }

    fn stub(srv: Arc<Mutex<Srv>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let Some((auth, body)) = read_request(&mut s) else { continue };
                let mut srv = srv.lock().unwrap();
                if auth != "Bearer tok" {
                    reply(&mut s, 401, &serde_json::json!({ "error": "unauthorized" }));
                    continue;
                }
                srv.posts += 1;
                let v: Value = serde_json::from_slice(&body).unwrap();
                let events = v["events"].as_array().unwrap();
                assert!(events.len() <= 5000);
                let mismatch = match srv.force.take() {
                    Some(f) => Some(f),
                    None if srv.last.is_some() && events[0]["prev"].as_str() != srv.last.as_deref() => {
                        Some(srv.last.clone())
                    }
                    None => None,
                };
                if let Some(last) = mismatch {
                    reply(&mut s, 409, &serde_json::json!({ "error": "chain_mismatch", "last_hash": last }));
                    continue;
                }
                srv.got.extend(events.iter().map(|e| e["hash"].as_str().unwrap().to_string()));
                srv.last = srv.got.last().cloned();
                let last = srv.last.clone();
                reply(&mut s, 200, &serde_json::json!({ "accepted": events.len(), "last_hash": last }));
            }
        });
        url
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kairos-agent-upload-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn chain(n: usize) -> Vec<Event> {
        let mut prev = GENESIS.to_string();
        (0..n)
            .map(|i| {
                let mut e = Event::new(1000.0 + i as f64, None, Kind::Present { input: None });
                e.seal(&prev);
                prev = e.hash.clone();
                e
            })
            .collect()
    }

    fn write_log(dir: &Path, day: &str, events: &[Event]) {
        let text: String = events.iter().map(|e| serde_json::to_string(e).unwrap() + "\n").collect();
        fs::write(dir.join(format!("events-{day}.jsonl")), text).unwrap();
    }

    fn account(url: &str) -> Account {
        Account { server: url.to_string(), token: "tok".into(), username: "u".into() }
    }

    fn hashes(ev: &[Event]) -> Vec<String> {
        ev.iter().map(|e| e.hash.clone()).collect()
    }

    #[test]
    fn uploads_in_batches_then_only_new_events() {
        let dir = tmp("batches");
        let ev = chain(9);
        write_log(&dir, "2026-11-18", &ev[..4]);
        write_log(&dir, "2026-11-19", &ev[4..7]);
        let srv = Arc::new(Mutex::new(Srv::default()));
        let acct = account(&stub(srv.clone()));
        let client = Client::new();

        assert_eq!(sync(&dir, &acct, &client, 3).unwrap(), 7);
        assert_eq!(srv.lock().unwrap().got, hashes(&ev[..7]));
        assert_eq!(srv.lock().unwrap().posts, 3);
        let st = State::load(&dir);
        assert_eq!(st.last_hash.as_deref(), Some(ev[6].hash.as_str()));
        assert_eq!(upload::pending(&dir, &st), 0);

        assert_eq!(sync(&dir, &acct, &client, 3).unwrap(), 0);
        assert_eq!(srv.lock().unwrap().posts, 3);

        write_log(&dir, "2026-11-19", &ev[4..]);
        write_log(&dir, "2026-11-20", &[]);
        assert_eq!(sync(&dir, &acct, &client, 3).unwrap(), 2);
        assert_eq!(srv.lock().unwrap().got, hashes(&ev));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn chain_mismatch_resumes_after_servers_last_hash() {
        let dir = tmp("resume");
        let ev = chain(7);
        write_log(&dir, "2026-11-18", &ev[..3]);
        write_log(&dir, "2026-11-19", &ev[3..]);
        let srv = Arc::new(Mutex::new(Srv {
            last: Some(ev[3].hash.clone()),
            got: hashes(&ev[..4]),
            ..Srv::default()
        }));
        let acct = account(&stub(srv.clone()));

        assert_eq!(sync(&dir, &acct, &Client::new(), 100).unwrap(), 3);
        assert_eq!(srv.lock().unwrap().got, hashes(&ev));
        assert_eq!(srv.lock().unwrap().posts, 2);
        assert_eq!(State::load(&dir).last_hash.as_deref(), Some(ev[6].hash.as_str()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn null_server_hash_restarts_from_first_local_event() {
        let dir = tmp("null");
        let ev = chain(5);
        write_log(&dir, "2026-11-19", &ev);
        State { last_hash: Some(ev[2].hash.clone()), file: Some("events-2026-11-19.jsonl".into()), line: 3 }
            .save(&dir)
            .unwrap();
        let srv = Arc::new(Mutex::new(Srv { force: Some(None), ..Srv::default() }));
        let acct = account(&stub(srv.clone()));

        assert_eq!(sync(&dir, &acct, &Client::new(), 100).unwrap(), 5);
        assert_eq!(srv.lock().unwrap().got, hashes(&ev));
        let _ = fs::remove_dir_all(&dir);
    }

    fn record(dir: &Path, ts: f64) -> Event {
        crate::log::Log::open(&dir.to_path_buf()).unwrap().append(Event::new(ts, None, Kind::Present { input: None })).unwrap()
    }

    #[test]
    fn a_log_that_lost_what_the_server_has_picks_up_from_the_servers_last_event() {
        let dir = tmp("unknown");
        let ev = chain(3);
        write_log(&dir, "2026-11-19", &ev);
        let lost = "f".repeat(64);
        let srv = Arc::new(Mutex::new(Srv { last: Some(lost.clone()), ..Srv::default() }));
        let acct = account(&stub(srv.clone()));

        assert!(matches!(sync(&dir, &acct, &Client::new(), 100), Err(Fail::Retry(_))));
        assert!(srv.lock().unwrap().got.is_empty());
        let next = record(&dir, 2000.0);
        assert_eq!(next.prev, lost);
        assert_eq!(sync(&dir, &acct, &Client::new(), 100).unwrap(), 1);
        assert_eq!(srv.lock().unwrap().got, [next.hash.clone()]);
        let after = record(&dir, 2001.0);
        assert_eq!(sync(&dir, &acct, &Client::new(), 100).unwrap(), 1);
        assert_eq!(srv.lock().unwrap().got, [next.hash, after.hash]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_chain_broken_after_the_servers_last_event_picks_up_from_it() {
        let dir = tmp("stuck");
        let mut broken = chain(3);
        broken[1].seal(&"a".repeat(64));
        write_log(&dir, "2026-11-19", &broken);
        let srv = Arc::new(Mutex::new(Srv { last: Some(broken[0].hash.clone()), ..Srv::default() }));
        let acct = account(&stub(srv.clone()));

        assert!(matches!(sync(&dir, &acct, &Client::new(), 100), Err(Fail::Retry(_))));
        let next = record(&dir, 2000.0);
        assert_eq!(next.prev, broken[0].hash);
        assert_eq!(sync(&dir, &acct, &Client::new(), 100).unwrap(), 1);
        assert_eq!(srv.lock().unwrap().got, [next.hash]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_token_is_rejected_and_dead_server_retries() {
        let dir = tmp("auth");
        write_log(&dir, "2026-11-19", &chain(2));
        let srv = Arc::new(Mutex::new(Srv::default()));
        let mut acct = account(&stub(srv));
        acct.token = "nope".into();
        assert!(matches!(sync(&dir, &acct, &Client::new(), 100), Err(Fail::Rejected)));

        let dead = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", dead.local_addr().unwrap());
        drop(dead);
        assert!(matches!(sync(&dir, &account(&url), &Client::new(), 100), Err(Fail::Retry(_))));
        let _ = fs::remove_dir_all(&dir);
    }
}
