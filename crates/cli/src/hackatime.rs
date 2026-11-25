//! Official hours come from Hackatime, so `kairos install` sets it up for people who haven't, the way Hackatime's own setup
//! does: their API key in the WakaTime config, wakatime-cli, and the editor plugin (offered next by `editors`).

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use kairos_core::loopback::{self, Loopback, Visit};
use kairos_core::wakatime::{self, HACKATIME_API_URL};
use kairos_core::{paths, pkce, wakatime_cli};
use serde_json::Value;

const SETUP: &str = "https://hackatime.hackclub.com/setup";
const TRIES: usize = 3;
const HACKATIME: &str = "https://hackatime.hackclub.com";
/// Hackatime is the optional step, and a closed tab never comes back, so this wait is the short one.
const WAIT: Duration = Duration::from_secs(300);
const SKIP: &str = "To skip it, click Cancel there or press Ctrl+C here. Kairos works either way.";
/// Kairos's app on Hackatime. It has no secret: the code comes back to this computer and only this process can use it.
const CLIENT_ID: &str = "__wEuIJz4ypHc2GlwtLJwuKHovRtIH3q4l89P-QXx5U";

pub fn command(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        anyhow::bail!("usage: kairos hackatime");
    }
    let config = wakatime::config_path().context("cannot find your home folder")?;
    if wakatime::on_hackatime(&paths::data_dir(), &config) {
        println!("Hackatime is set up: WakaTime plugins on this computer send your time to Hackatime.");
        return Ok(());
    }
    let data = paths::data_dir();
    if let Some(o) = Oauth::new().filter(|_| crate::account::browser_here()) {
        let acct = kairos_core::account::load(&data);
        let t = crate::account::Trip {
            server: acct.as_ref().map(|a| a.server.as_str()),
            connect: false,
            hackatime: Some((&o, &config)),
            data: &data,
            open: &crate::account::open_browser,
            wait: crate::account::WAIT,
        };
        match crate::account::trip(&t).map(|d| d.hackatime) {
            Ok(Some(Ok(()))) => {
                set_up();
                crate::editors::add_missing();
                return Ok(());
            }
            Ok(Some(Err(e))) | Err(e) => println!("Hackatime wasn't set up in the browser ({e:#}), so here's the other way."),
            Ok(None) => {}
        }
    }
    if offer() {
        crate::editors::add_missing();
    }
    Ok(())
}

/// Sets up Hackatime when WakaTime plugins on this computer don't send to it yet. True when it did.
pub fn offer() -> bool {
    let data = paths::data_dir();
    let Some(config) = wakatime::config_path() else {
        return false;
    };
    if wakatime::on_hackatime(&data, &config) {
        return false;
    }
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        println!("Hackatime isn't set up on this computer yet. Run kairos hackatime in a terminal and Kairos sets it up for you.");
        return false;
    }
    println!("\nYour official hours come from Hackatime, and it isn't set up on this computer yet. Kairos can set it up for you.");
    println!("Sign in at {SETUP} (opening it now) and copy your API key from there.");
    crate::account::open_browser(SETUP);
    let Some(key) = ask_key() else {
        println!("Skipped. Run kairos hackatime whenever you want to set it up.");
        return false;
    };
    if let Err(e) = wakatime::connect_hackatime(&data, &config, &key) {
        println!("Could not save your Hackatime key ({e:#}).");
        return false;
    }
    set_up();
    true
}

/// Tells the person and adds wakatime-cli once their key is in place.
pub fn set_up() {
    println!("Hackatime is set up.");
    if let Err(e) = wakatime_cli::install(&paths::data_dir()) {
        println!("Could not add wakatime-cli ({e:#}). Hackatime's editor plugin gets it when your editor starts.");
    }
}

/// Signing in to Hackatime in the browser, which hands the person's API key to this computer: no key to copy and paste.
pub struct Oauth {
    base: String,
    client_id: String,
}

impl Oauth {
    /// None while Kairos has no app on Hackatime.
    pub fn new() -> Option<Oauth> {
        let client_id = std::env::var("KAIROS_HACKATIME_CLIENT_ID").unwrap_or_else(|_| CLIENT_ID.to_string());
        let base = std::env::var("KAIROS_HACKATIME_URL").unwrap_or_else(|_| HACKATIME.to_string());
        Oauth::with(&base, &client_id)
    }

    pub fn with(base: &str, client_id: &str) -> Option<Oauth> {
        (!client_id.is_empty()).then(|| Oauth { base: base.trim_end_matches('/').to_string(), client_id: client_id.to_string() })
    }

    fn authorize_url(&self, redirect: &str, state: &str, verifier: &str) -> Result<String> {
        let mut u = reqwest::Url::parse(&format!("{}/oauth/authorize", self.base))?;
        u.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", redirect)
            .append_pair("response_type", "code")
            .append_pair("scope", "profile")
            .append_pair("state", state)
            .append_pair("code_challenge", &pkce::challenge(verifier))
            .append_pair("code_challenge_method", "S256");
        Ok(u.into())
    }

    /// The API key of the account that signed in. The access token is only used for this and is not kept.
    fn key(&self, code: &str, redirect: &str, verifier: &str) -> Result<String> {
        let http = client()?;
        let token: Value = http
            .post(format!("{}/oauth/token", self.base))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect),
                ("client_id", &self.client_id),
                ("code_verifier", verifier),
            ])
            .send()
            .context("contacting Hackatime")?
            .error_for_status()
            .context("Hackatime turned the sign-in down")?
            .json()?;
        let access = token["access_token"].as_str().context("Hackatime sent no access token")?;
        let keys: Value = http
            .get(format!("{}/api/v1/authenticated/api_keys", self.base))
            .bearer_auth(access)
            .send()
            .context("contacting Hackatime")?
            .error_for_status()
            .context("Hackatime didn't hand over the API key")?
            .json()?;
        match keys["token"].as_str() {
            Some(key) if is_key(key) => Ok(key.to_string()),
            _ => bail!("Hackatime sent a key wakatime-cli can't use"),
        }
    }
}

/// Hackatime's sign-in in the browser, saved for WakaTime plugins on this computer. `prev` is the browser still waiting on
/// a step before this one (else it gets opened). Hands back where the browser is now, for the caller to send it on.
pub fn via_browser(o: &Oauth, lb: &Loopback, prev: Option<Visit>, open: &dyn Fn(&str) -> bool, data: &Path, config: &Path, wait: Duration) -> (Result<()>, Option<Visit>) {
    let (state, verifier) = match (loopback::random_state(), pkce::verifier()) {
        (Ok(s), Ok(v)) => (s, v),
        (Err(e), _) | (_, Err(e)) => return (Err(e), prev),
    };
    let redirect = lb.url("/hackatime");
    let url = match o.authorize_url(&redirect, &state, &verifier) {
        Ok(u) => u,
        Err(e) => return (Err(e), prev),
    };
    match prev {
        Some(v) => {
            v.redirect(&url);
            println!("One more step in your browser: let Kairos set up Hackatime for you.\n{SKIP}");
        }
        None if open(&url) => println!(
            "Your browser should open so you can sign in to Hackatime and let Kairos set it up.\nIf it doesn't, open this link on this computer:\n  {url}\n{SKIP}"
        ),
        None => return (Err(anyhow::anyhow!("no browser could be opened")), None),
    }
    let v = match lb.wait("/hackatime", &state, wait.min(WAIT)) {
        Ok(v) => v,
        Err(e) => return (Err(e), None),
    };
    if v.get("error").is_some() {
        return (Err(anyhow::anyhow!("Kairos wasn't authorized on Hackatime")), Some(v));
    }
    let saved = v
        .get("code")
        .context("Hackatime came back without a code")
        .and_then(|code| o.key(code, &redirect, &verifier))
        .and_then(|key| wakatime::connect_hackatime(data, config, &key));
    (saved, Some(v))
}

/// A key Hackatime accepted, or `None` when the person skipped, gave up or Hackatime couldn't be reached.
fn ask_key() -> Option<String> {
    for _ in 0..TRIES {
        print!("Paste your Hackatime API key (or press Enter to skip): ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).ok()? == 0 {
            return None;
        }
        let key = line.trim().trim_matches(['"', '\'']).to_string();
        if key.is_empty() {
            return None;
        }
        if !is_key(&key) {
            println!("That isn't an API key. It looks like 0b5f1c2d-3e4f-4a5b-8c6d-7e8f9a0b1c2d, on {SETUP}.");
            continue;
        }
        match accepted(&key) {
            Ok(true) => return Some(key),
            Ok(false) => println!("Hackatime didn't accept that key. Copy it again from {SETUP}."),
            Err(e) => {
                println!("Could not reach Hackatime to check the key ({e:#}).");
                return None;
            }
        }
    }
    None
}

/// wakatime-cli only sends with UUID keys.
fn is_key(key: &str) -> bool {
    let parts: Vec<&str> = key.split('-').collect();
    parts.iter().map(|p| p.len()).eq([8, 4, 4, 4, 12]) && parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Asks Hackatime for today's total with the key, the way wakatime-cli's `--today` does.
fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("kairos/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building the http client")
}

fn accepted(key: &str) -> Result<bool> {
    let status = client()?
        .get(format!("{HACKATIME_API_URL}/users/current/statusbar/today"))
        .header("Authorization", format!("Basic {}", BASE64.encode(key)))
        .send()?
        .status();
    match status.as_u16() {
        200..=299 => Ok(true),
        401 | 403 => Ok(false),
        code => anyhow::bail!("it answered {code}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_uuids() {
        assert!(is_key("0b5f1c2d-3e4f-4a5b-8c6d-7e8f9a0b1c2d"));
        assert!(!is_key("0b5f1c2d-3e4f-4a5b-8c6d-7e8f9a0b1c2"));
        assert!(!is_key("0b5f1c2d3e4f4a5b8c6d7e8f9a0b1c2d"));
        assert!(!is_key("zb5f1c2d-3e4f-4a5b-8c6d-7e8f9a0b1c2d"));
        assert!(!is_key("key 0b5f1c2d-3e4f-4a5b-8c6d-7e8f9a0b1c2d"));
    }
}
