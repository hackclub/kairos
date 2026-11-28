
use std::fmt;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use chrono::Local;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::account::{self, Account, DEFAULT_SERVER};
use crate::upload::State;
use crate::{loopback, pkce, upload};

#[derive(Debug)]
pub struct Expired;

impl fmt::Display for Expired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the code expired, run kairos login again")
    }
}

impl std::error::Error for Expired {}

#[derive(Deserialize)]
struct Start {
    user_code: String,
    poll_token: String,
    verify_url: String,
    #[serde(default = "default_interval")]
    interval: u64,
    #[serde(default = "default_expires")]
    expires_in: u64,
}

fn default_interval() -> u64 {
    3
}

fn default_expires() -> u64 {
    900
}

#[derive(Debug, PartialEq)]
pub enum Poll {
    Pending,
    Approved(Account),
}

pub struct Pairing {
    client: Client,
    server: String,
    start: Start,
    interval: Duration,
}

pub fn default_server() -> String {
    std::env::var("KAIROS_SERVER").unwrap_or_else(|_| DEFAULT_SERVER.to_string())
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| fs::read_to_string("/etc/hostname").ok())
        .or_else(|| {
            let out = Command::new("hostname").output().ok()?;
            String::from_utf8(out.stdout).ok()
        })
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

impl Pairing {
    pub fn begin(server: &str) -> Result<Pairing> {
        Pairing::begin_with(Client::builder().timeout(Duration::from_secs(30)).build()?, server)
    }

    pub fn begin_with(client: Client, server: &str) -> Result<Pairing> {
        let server = server.trim_end_matches('/').to_string();
        let start: Start = client
            .post(format!("{server}/api/v1/device/start"))
            .json(&json!({
                "name": hostname(),
                "os": std::env::consts::OS,
                "utc_offset_min": Local::now().offset().local_minus_utc() / 60,
            }))
            .send()
            .with_context(|| format!("contacting {server}"))?
            .error_for_status()?
            .json()?;
        let interval = Duration::from_secs(start.interval.max(1));
        Ok(Pairing { client, server, start, interval })
    }

    pub fn user_code(&self) -> &str {
        &self.start.user_code
    }

    pub fn verify_url(&self) -> &str {
        &self.start.verify_url
    }

    /// One poll. Network hiccups and unreadable replies count as still pending.
    pub fn poll(&self) -> Result<Poll> {
        Ok(self.ask(None)?.map_or(Poll::Pending, |(acct, _)| Poll::Approved(acct)))
    }

    /// The account once the code is approved, and whether the server gave this computer its device back.
    fn ask(&self, last_hash: Option<&str>) -> Result<Option<(Account, bool)>> {
        let Ok(res) = self
            .client
            .post(format!("{}/api/v1/device/poll", self.server))
            .json(&json!({ "poll_token": self.start.poll_token, "last_hash": last_hash }))
            .send()
        else {
            return Ok(None);
        };
        if res.status() == StatusCode::GONE {
            return Err(Expired.into());
        }
        let Ok(v) = res.json::<Value>() else { return Ok(None) };
        if v["status"] != "approved" {
            return Ok(None);
        }
        let (Some(token), Some(username)) = (v["token"].as_str(), v["username"].as_str()) else {
            bail!("the server approved the code but sent no token");
        };
        let acct = Account { server: self.server.clone(), token: token.to_string(), username: username.to_string() };
        Ok(Some((acct, v["resumed"] == true)))
    }

    pub fn wait(&self, dir: &Path) -> Result<Account> {
        let deadline = Instant::now() + Duration::from_secs(self.start.expires_in);
        let last = State::load(dir).last_hash;
        loop {
            thread::sleep(self.interval);
            if Instant::now() > deadline {
                return Err(Expired.into());
            }
            if let Some((acct, resumed)) = self.ask(last.as_deref())? {
                keep(dir, &acct, resumed)?;
                return Ok(acct);
            }
        }
    }
}

/// Saves the account and leaves the upload cursor where its device starts. The server gives a computer its device back
/// when the account has one that ends on the last event the computer uploaded, and then uploads go on from there. A new
/// device gets the whole log when none of it was uploaded, and only what comes from now on when it was: those events
/// are on another device already, maybe another account's. The cursor is written first, so kairosd never uploads for
/// the new account from the old place. The token this computer had before is given up only once the new one is saved,
/// so signing in twice doesn't leave a second device connected, and a sign-in that fails leaves the old one working.
fn keep(dir: &Path, acct: &Account, resumed: bool) -> Result<()> {
    let old = account::load(dir).filter(|old| old.token != acct.token);
    if !resumed {
        match State::load(dir).last_hash {
            Some(_) => upload::end(dir).save(dir).context("saving upload.json")?,
            None => {
                let _ = fs::remove_file(upload::state_path(dir));
            }
        }
    }
    account::save(dir, acct)?;
    if let Some(old) = old {
        give_up(&old);
    }
    Ok(())
}

/// `kairos logout`: tells the server this computer gave its token up, so its device stops counting as connected.
pub fn give_up(acct: &Account) {
    let Ok(client) = Client::builder().timeout(Duration::from_secs(10)).user_agent(concat!("kairos/", env!("CARGO_PKG_VERSION"))).build() else {
        return;
    };
    let _ = client.post(format!("{}/api/v1/device/logout", acct.server.trim_end_matches('/'))).bearer_auth(&acct.token).send();
}

/// Signing in through the browser, which comes back to this computer on 127.0.0.1 (see `loopback`) with a code that only
/// this process can use. No code to compare and nothing to click on Kairos's side.
pub struct Connect {
    client: Client,
    server: String,
    verifier: String,
    state: String,
}

impl Connect {
    pub fn new(server: &str) -> Result<Connect> {
        let client = Client::builder().timeout(Duration::from_secs(30)).user_agent(concat!("kairos/", env!("CARGO_PKG_VERSION"))).build()?;
        Connect::with(client, server)
    }

    pub fn with(client: Client, server: &str) -> Result<Connect> {
        Ok(Connect { client, server: server.trim_end_matches('/').to_string(), verifier: pkce::verifier()?, state: loopback::random_state()? })
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    pub fn url(&self, port: u16) -> String {
        format!("{}/device/connect?port={port}&state={}&challenge={}", self.server, self.state, pkce::challenge(&self.verifier))
    }

    /// Trades the code the browser brought back for this device's token, and saves the account.
    pub fn finish(&self, code: &str, dir: &Path) -> Result<Account> {
        let res = self
            .client
            .post(format!("{}/api/v1/device/exchange", self.server))
            .json(&json!({
                "code": code,
                "verifier": self.verifier,
                "name": hostname(),
                "os": std::env::consts::OS,
                "last_hash": State::load(dir).last_hash,
            }))
            .send()
            .with_context(|| format!("contacting {}", self.server))?;
        let status = res.status();
        let v: Value = res.json().unwrap_or(Value::Null);
        if !status.is_success() {
            bail!("{}", v["error"].as_str().unwrap_or("the server turned the sign-in down"));
        }
        let (Some(token), Some(username)) = (v["token"].as_str(), v["username"].as_str()) else {
            bail!("the server signed you in but sent no token");
        };
        let acct = Account { server: self.server.clone(), token: token.to_string(), username: username.to_string() };
        keep(dir, &acct, v["resumed"] == true)?;
        Ok(acct)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::mpsc;

    use crate::event::{Event, Kind, GENESIS};

    fn fake_server(replies: Vec<(u16, String)>) -> String {
        fake_server_seen(replies).0
    }

    /// Also hands over each request it answered, head and body.
    fn fake_server_seen(replies: Vec<(u16, String)>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, seen) = mpsc::channel();
        thread::spawn(move || {
            for (code, body) in replies {
                let Ok((mut sock, _)) = listener.accept() else { return };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                let head_end = loop {
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                    match sock.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
                let len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                    .unwrap_or(0);
                let mut rest = vec![0u8; len.saturating_sub(buf.len() - head_end)];
                let _ = sock.read_exact(&mut rest);
                let _ = tx.send(String::from_utf8_lossy(&[&buf[..], &rest[..]].concat()).into_owned());
                let reply = format!(
                    "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes());
            }
        });
        (format!("http://{addr}"), seen)
    }

    fn start_reply() -> (u16, String) {
        (
            200,
            json!({ "user_code": "ABCD-EFGH", "poll_token": "pt", "verify_url": "http://x/device", "interval": 0, "expires_in": 60 })
                .to_string(),
        )
    }

    fn pairing(server: &str) -> Pairing {
        let client = Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap();
        let mut p = Pairing::begin_with(client, server).unwrap();
        p.interval = Duration::from_millis(5);
        p
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kairos-pairing-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn waits_through_pending_then_saves_the_account() {
        let approved = json!({ "status": "approved", "token": "kd_1", "username": "ada" }).to_string();
        let server = fake_server(vec![
            start_reply(),
            (200, json!({ "status": "pending" }).to_string()),
            (500, "oops".into()),
            (200, approved),
        ]);
        let dir = temp("ok");
        fs::create_dir_all(&dir).unwrap();
        fs::write(upload::state_path(&dir), "{}").unwrap();
        let p = pairing(&server);
        assert_eq!(p.user_code(), "ABCD-EFGH");
        assert_eq!(p.verify_url(), "http://x/device");
        let acct = p.wait(&dir).unwrap();
        assert_eq!(acct, Account { server: server.clone(), token: "kd_1".into(), username: "ada".into() });
        assert_eq!(account::load(&dir), Some(acct));
        assert!(!upload::state_path(&dir).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A log of five events of which three were uploaded.
    fn uploaded_some(name: &str) -> (PathBuf, State, State) {
        let dir = temp(name);
        fs::create_dir_all(&dir).unwrap();
        let (mut prev, mut text, mut hashes) = (GENESIS.to_string(), String::new(), Vec::new());
        for i in 0..5 {
            let mut e = Event::new(1000.0 + i as f64, None, Kind::Present { input: None, keys: None, injected: None });
            e.seal(&prev);
            prev = e.hash.clone();
            text.push_str(&serde_json::to_string(&e).unwrap());
            text.push('\n');
            hashes.push(e.hash);
        }
        let file = "events-2026-11-19.jsonl";
        fs::write(dir.join(file), text).unwrap();
        let at = |n: usize| State { last_hash: Some(hashes[n - 1].clone()), file: Some(file.into()), line: n };
        at(3).save(&dir).unwrap();
        (dir, at(3), at(5))
    }

    #[test]
    fn a_computer_given_its_device_back_uploads_on_from_where_it_stopped() {
        let (dir, cursor, _) = uploaded_some("resumed");
        let last = format!("\"last_hash\":\"{}\"", cursor.last_hash.as_deref().unwrap());
        let approved = json!({ "status": "approved", "token": "kd_1", "username": "ada", "resumed": true }).to_string();
        let (server, seen) = fake_server_seen(vec![start_reply(), (200, approved), (200, json!({ "token": "kd_2", "username": "ada", "resumed": true }).to_string())]);
        pairing(&server).wait(&dir).unwrap();
        assert_eq!(State::load(&dir), cursor);
        let client = Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap();
        Connect::with(client, &server).unwrap().finish("kg_1", &dir).unwrap();
        assert_eq!(State::load(&dir), cursor);
        let bodies: Vec<String> = seen.try_iter().collect();
        assert!(bodies[1].contains(&last) && bodies[2].contains(&last), "{bodies:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_device_starts_after_a_log_that_was_uploaded_elsewhere() {
        let (dir, _, end) = uploaded_some("elsewhere");
        let server = fake_server(vec![(200, json!({ "token": "kd_2", "username": "bea" }).to_string())]);
        let client = Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap();
        Connect::with(client, &server).unwrap().finish("kg_1", &dir).unwrap();
        assert_eq!(State::load(&dir), end);
        assert!(upload::next_batch(&dir, &end, 10).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn gone_means_expired() {
        let server = fake_server(vec![start_reply(), (410, "{}".into())]);
        let dir = temp("gone");
        let err = pairing(&server).wait(&dir).unwrap_err();
        assert!(err.downcast_ref::<Expired>().is_some());
        assert_eq!(account::load(&dir), None);
    }

    #[test]
    fn approval_without_a_token_is_an_error() {
        let server = fake_server(vec![start_reply(), (200, json!({ "status": "approved" }).to_string())]);
        let err = pairing(&server).wait(&temp("notoken")).unwrap_err();
        assert!(err.to_string().contains("sent no token"));
    }

    #[test]
    fn connect_trades_the_code_once_and_saves_the_account() {
        let refused = json!({ "error": "this sign-in expired or was already used, run kairos login again" }).to_string();
        let server = fake_server(vec![(200, json!({ "token": "kd_2", "username": "bea" }).to_string()), (400, refused), (200, "{}".into())]);
        let dir = temp("connect");
        fs::create_dir_all(&dir).unwrap();
        fs::write(upload::state_path(&dir), "{}").unwrap();
        let client = Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap();
        let c = Connect::with(client, &format!("{server}/")).unwrap();
        let url = c.url(50123);
        let prefix = format!("{server}/device/connect?port=50123&state={}&challenge=", c.state());
        assert!(url.starts_with(&prefix) && pkce::is_challenge(&url[prefix.len()..]), "{url}");
        assert_eq!(c.server(), server);

        let acct = c.finish("kg_1", &dir).unwrap();
        assert_eq!(acct, Account { server: server.clone(), token: "kd_2".into(), username: "bea".into() });
        assert_eq!(account::load(&dir), Some(acct.clone()));
        assert!(!upload::state_path(&dir).exists());
        assert!(c.finish("kg_1", &dir).unwrap_err().to_string().contains("already used"));
        assert!(c.finish("kg_1", &dir).unwrap_err().to_string().contains("sent no token"));
        assert_eq!(account::load(&dir), Some(acct), "a failed sign-in leaves the saved one alone");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn signing_in_again_gives_up_the_old_token_after_the_new_one_is_saved() {
        let refused = json!({ "error": "this sign-in expired or was already used, run kairos login again" }).to_string();
        let (server, seen) = fake_server_seen(vec![
            (400, refused),
            (200, json!({ "token": "kd_2", "username": "bea" }).to_string()),
            (200, json!({ "ok": true }).to_string()),
        ]);
        let dir = temp("again");
        fs::create_dir_all(&dir).unwrap();
        let old = Account { server: server.clone(), token: "kd_1".into(), username: "bea".into() };
        account::save(&dir, &old).unwrap();
        let client = Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap();
        let c = Connect::with(client, &server).unwrap();

        assert!(c.finish("kg_1", &dir).is_err());
        assert_eq!(account::load(&dir), Some(old));
        assert_eq!(seen.try_iter().count(), 1, "a failed sign-in gives nothing up");

        let acct = c.finish("kg_2", &dir).unwrap();
        assert_eq!(account::load(&dir), Some(acct));
        let requests: Vec<String> = seen.try_iter().collect();
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert!(requests[0].starts_with("POST /api/v1/device/exchange"), "{requests:?}");
        assert!(requests[1].starts_with("POST /api/v1/device/logout") && requests[1].contains("Bearer kd_1"), "{requests:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn starting_against_a_dead_server_fails() {
        let addr = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let client = Client::builder().no_proxy().timeout(Duration::from_secs(2)).build().unwrap();
        assert!(Pairing::begin_with(client, &format!("http://{addr}")).is_err());
    }
}
