
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::account::{self, write_private, DEFAULT_SERVER};
use crate::paths::API_PORT;

const STATE: &str = "wakatime.json";
const LOCK: &str = "wakatime.lock";
const SECTION: &str = "settings";

pub fn api_url() -> String {
    format!("http://127.0.0.1:{API_PORT}/wakatime/v1")
}

/// wakatime-cli's own default, used where the config has no `api_url`
/// (`BaseURL` in wakatime-cli's `pkg/api/api.go`).
pub const DEFAULT_API_URL: &str = "https://api.wakatime.com/api/v1";

pub fn relay_dir(data: &Path) -> PathBuf {
    data.join("relay")
}

pub const RELAY_EXT: &str = "req";

pub fn relay_waiting(data: &Path) -> usize {
    let Ok(dir) = fs::read_dir(relay_dir(data)) else {
        return 0;
    };
    dir.flatten().filter(|e| e.path().extension().is_some_and(|x| x == RELAY_EXT)).count()
}

#[derive(Clone, PartialEq)]
pub struct Upstream {
    /// The API's base, without a trailing slash or a heartbeats endpoint.
    pub url: String,
    pub key: String,
}

impl std::fmt::Debug for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upstream").field("url", &self.url).field("key", &"<hidden>").finish()
    }
}

impl Upstream {
    /// No path, no credentials.
    pub fn host(&self) -> &str {
        authority(&self.url).unwrap_or_default()
    }
}

/// `api_url` as wakatime-cli reads it (`normalizeURL` in its `pkg/params/params.go`): without a
/// trailing slash or the heartbeats endpoint, which plugins sometimes put there.
fn normalize(url: &str) -> &str {
    let mut url = url;
    for suffix in ["/", ".bulk", "/users/current/heartbeats", "/heartbeats", "/heartbeat"] {
        url = url.strip_suffix(suffix).unwrap_or(url);
    }
    url
}

fn authority(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Some(rest[..end].rsplit_once('@').map_or(&rest[..end], |(_, host)| host))
}

fn is_kairos(url: &str) -> bool {
    let Some(host) = authority(url) else {
        return false;
    };
    ["127.0.0.1", "localhost", "[::1]", "0.0.0.0"].iter().any(|name| host.eq_ignore_ascii_case(&format!("{name}:{API_PORT}")))
}

/// The Kairos server takes heartbeats straight from an editor key too, so passing them on to it would store each one twice.
fn is_kairos_server(data: &Path, url: &str) -> bool {
    let Some(host) = authority(url) else {
        return false;
    };
    let servers = [Some(DEFAULT_SERVER.to_string()), account::load(data).map(|a| a.server)];
    servers.iter().flatten().any(|s| authority(s).is_some_and(|h| h.eq_ignore_ascii_case(host)))
}

/// `None` when Kairos has not taken a config over, WakaTime had no api key, or the saved url is Kairos itself (this
/// device or the server).
pub fn upstream(data: &Path) -> Option<Upstream> {
    let s = state(data)?;
    let key = s.api_key?;
    let url = normalize(s.api_url.as_deref().unwrap_or(DEFAULT_API_URL)).to_string();
    (authority(&url).is_some() && !is_kairos(&url) && !is_kairos_server(data, &url)).then_some(Upstream { url, key })
}

/// Where Hackatime's setup points wakatime-cli.
pub const HACKATIME_API_URL: &str = "https://hackatime.hackclub.com/api/hackatime/v1";
const HACKATIME_HOST: &str = "hackatime.hackclub.com";

/// Whether heartbeats reach a Hackatime account: through the relay when Kairos holds the config at `path`, else straight from it.
pub fn on_hackatime(data: &Path, path: &Path) -> bool {
    let hackatime = |url: &str| authority(url).is_some_and(|host| host.eq_ignore_ascii_case(HACKATIME_HOST));
    if state(data).is_some_and(|s| s.config == path) {
        return upstream(data).is_some_and(|u| hackatime(&u.url));
    }
    let text = read(path).ok().flatten().unwrap_or_default();
    get(&text, "api_key").is_some() && get(&text, "api_url").is_some_and(|u| hackatime(&u))
}

/// Sends heartbeats to the Hackatime account `key` belongs to: the relay passes them on with it when Kairos holds the config,
/// else the config at `path` gets it.
pub fn connect_hackatime(data: &Path, path: &Path, key: &str) -> Result<()> {
    let _lock = lock(data)?;
    if let Some(mut s) = state(data).filter(|s| s.config == path) {
        s.api_url = Some(HACKATIME_API_URL.to_string());
        s.api_key = Some(key.to_string());
        return save_state(data, &s);
    }
    let text = read(path)?.unwrap_or_default();
    write(path, &set(&set(&text, "api_url", Some(HACKATIME_API_URL)), "api_key", Some(key)))
}

/// What the person's own config does with agents' logs: `Some(false)` when it turns AI sync off, none without a config.
pub fn ai_sync(path: &Path) -> Option<bool> {
    let text = read(path).ok()??;
    let off = get(&text, "sync_ai_disabled").is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "t" | "yes" | "on"));
    Some(!off)
}

/// The config for kairosd's own run of wakatime-cli: the person's, so excludes, project maps, hidden names and the rest shape the same
/// heartbeats, but sending only to `url` with `key`, AI sync on and no rate limit. Everything that could send them anywhere else goes.
pub fn shadow_config(user: &str, url: &str, key: &str) -> String {
    let mut text = user.to_string();
    for section in ["project_api_key", "api_urls"] {
        text = drop_section(&text, section);
    }
    for k in ["apikey", "api_key_vault_cmd", "proxy", "no_ssl_verify", "ssl_certs_file", "offline", "log_file"] {
        text = set(&text, k, None);
    }
    for (k, v) in [("api_url", url), ("api_key", key), ("sync_ai_disabled", "false"), ("heartbeat_rate_limit_seconds", "0")] {
        text = set(&text, k, Some(v));
    }
    text
}

/// [`shadow_config`] of the config at `path`, which may not exist.
pub fn shadow_config_of(path: &Path, url: &str, key: &str) -> Result<String> {
    Ok(shadow_config(&read(path)?.unwrap_or_default(), url, key))
}

/// Where kairosd runs the person's wakatime-cli with AI sync on (the agent's `shadow`), and its note on the last run.
pub fn shadow_dir(data: &Path) -> PathBuf {
    data.join("hackatime-ai")
}

pub const SHADOW_LAST_RUN: &str = "last-run.json";
pub const SHADOW_CONFIG: &str = ".wakatime.cfg";
pub const SHADOW_INTERNAL: &str = "wakatime-internal.cfg";

/// Readies `dir` as the home (`WAKATIME_HOME`) of a wakatime-cli run that reads agents' logs and sends to `url`: the
/// [`shadow_config_of`] the person's config, refreshed every time, and on the first run a note that the logs were read up
/// to `now`, so it starts there instead of from the first day wakatime-cli knows of. `tmp` is for its lock, which it keeps
/// in the temp folder.
pub fn prepare_shadow(dir: &Path, config: Option<&Path>, url: &str, key: &str, now: chrono::DateTime<chrono::Utc>) -> Result<()> {
    crate::paths::make_private(dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::create_dir_all(dir.join("tmp")).with_context(|| format!("creating {}", dir.join("tmp").display()))?;
    let text = match config {
        Some(path) => shadow_config_of(path, url, key)?,
        None => shadow_config("", url, key),
    };
    let cfg = dir.join(SHADOW_CONFIG);
    if read(&cfg)?.as_deref() != Some(text.as_str()) {
        write(&cfg, &text)?;
    }
    let internal = dir.join(SHADOW_INTERNAL);
    if read(&internal)?.is_none() {
        let at = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write(&internal, &format!("[internal]\nai_logs_last_parsed_at = {at}\n"))?;
    }
    Ok(())
}

/// The editor a WakaTime user agent names: the plugin's own name (`vscode-wakatime/24.6.2` is
/// `vscode`), else the first product after wakatime-cli's own.
pub fn editor(user_agent: &str) -> Option<String> {
    let names = || user_agent.split_whitespace().filter_map(|t| t.split_once('/').map(|(n, _)| n));
    names()
        .find_map(|n| n.strip_suffix("-wakatime").map(str::to_lowercase))
        .or_else(|| names().find(|n| !n.eq_ignore_ascii_case("wakatime")).map(str::to_lowercase))
}

/// The agent a heartbeat made from its log names last in its user agent (`... opus/5-5 claude-code/2.1.319` is `claude-code`).
pub fn agent(user_agent: &str) -> Option<String> {
    let name = user_agent.split_whitespace().filter_map(|t| t.split_once('/').map(|(n, _)| n)).last()?;
    (!name.eq_ignore_ascii_case("wakatime") && !name.is_empty()).then(|| name.chars().take(40).collect::<String>().to_lowercase())
}

/// Heartbeats wakatime-cli builds from AI agents' own logs: one per prompt, reply, tool call or
/// edited file, each with the agent's `ai_session` (backfilled from the whole history on a fresh
/// install). Kept apart from an editor's (`AiHeartbeat` events), which have no session, even when
/// wakatime-cli files them under `ai coding` because an agent was busy in the same project.
pub fn from_agent(heartbeat: &serde_json::Value) -> bool {
    heartbeat.get("ai_session").is_some_and(|s| !s.is_null())
}

/// The config wakatime-cli reads: `$WAKATIME_HOME/.wakatime.cfg`, else `~/.wakatime.cfg` unless only
/// the one under the XDG config folder exists.
pub fn config_path() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("WAKATIME_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(home).join(".wakatime.cfg"));
    }
    let home = dirs::home_dir()?;
    let legacy = home.join(".wakatime.cfg");
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
        .join("wakatime")
        .join("wakatime.cfg");
    Some(if !legacy.exists() && xdg.exists() { xdg } else { legacy })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub key: String,
    pub config: PathBuf,
    pub existed: bool,
    pub api_url: Option<String>,
    pub api_key: Option<String>,
    /// When kairosd last pointed the config back at Kairos after something else changed it (unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repointed: Option<f64>,
}

pub fn state(data: &Path) -> Option<State> {
    serde_json::from_str(&fs::read_to_string(data.join(STATE)).ok()?).ok()
}

fn save_state(data: &Path, s: &State) -> Result<()> {
    fs::create_dir_all(data).with_context(|| format!("creating {}", data.display()))?;
    let target = data.join(STATE);
    let tmp = target.with_extension("json.tmp");
    write_private(&tmp, serde_json::to_string_pretty(s)?.as_bytes()).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &target).with_context(|| format!("writing {}", target.display()))
}

/// kairosd re-points the config while `kairos install` or `uninstall` may be taking it or giving it back, so each of
/// them holds this for the whole read, save and write.
fn lock(data: &Path) -> Result<File> {
    fs::create_dir_all(data).with_context(|| format!("creating {}", data.display()))?;
    let path = data.join(LOCK);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.lock().with_context(|| format!("locking {}", path.display()))?;
    Ok(file)
}

fn read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Replaces the config the way an editor would see it change: all at once, keeping its permissions.
fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("cfg.kairos-tmp");
    write_private(&tmp, text.as_bytes()).with_context(|| format!("writing {}", tmp.display()))?;
    if let Ok(meta) = fs::metadata(path) {
        let _ = fs::set_permissions(&tmp, meta.permissions());
    }
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// wakatime-cli refuses api keys that are not UUIDs.
pub fn new_key() -> Result<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("os random: {e}"))?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Points {
    Kairos,
    Elsewhere(Option<String>),
    NoConfig,
}

pub fn points(path: &Path) -> Result<Points> {
    Ok(match read(path)? {
        None => Points::NoConfig,
        Some(t) if get(&t, "api_url").as_deref() == Some(api_url().as_str()) => Points::Kairos,
        Some(t) => Points::Elsewhere(get(&t, "api_url")),
    })
}

pub fn take(data: &Path, path: &Path) -> Result<()> {
    let _lock = lock(data)?;
    unmark_parked(data)?;
    take_locked(data, path)
}

fn take_locked(data: &Path, path: &Path) -> Result<()> {
    let text = read(path)?;
    let current = text.as_deref().unwrap_or("");
    let saved = state(data).filter(|s| s.config == path);
    if let Some(other) = state(data).filter(|s| s.config != path) {
        release_locked(data).with_context(|| format!("putting back {}", other.config.display()))?;
    }
    let s = match saved {
        Some(s) if get(current, "api_url").as_deref() == Some(api_url().as_str()) => s,
        saved => State {
            key: match saved {
                Some(s) => s.key,
                None => new_key()?,
            },
            config: path.to_path_buf(),
            existed: text.is_some(),
            api_url: get(current, "api_url"),
            api_key: get(current, "api_key"),
            repointed: None,
        },
    };
    save_state(data, &s)?;
    let text = set(&set(current, "api_url", Some(&api_url())), "api_key", Some(&s.key));
    write(path, &text)
}

/// Puts Kairos back in a config it holds that something else changed: Hackatime's setup and its editor sign-in write
/// their own `api_url` and `api_key`, and a plugin's api key command writes just the key. A new key becomes what heartbeats
/// are passed on with, along with the url written next to it. Without one, where they went before stays: a saved key is
/// never sent to a url it wasn't written with. `None` when nothing changed, Kairos holds no config, or it was emptied.
pub fn repoint(data: &Path, now: f64) -> Result<Option<State>> {
    if state(data).is_none() {
        return Ok(None);
    }
    let _lock = lock(data)?;
    let Some(mut s) = state(data) else {
        return Ok(None);
    };
    let Some(text) = read(&s.config)? else {
        return Ok(None);
    };
    let (url, key) = (get(&text, "api_url"), get(&text, "api_key"));
    let ours = url.as_deref().is_some_and(is_kairos);
    if (ours && key.as_deref() == Some(s.key.as_str())) || (url.is_none() && key.is_none()) {
        return Ok(None);
    }
    if let Some(key) = key.filter(|k| *k != s.key) {
        if !ours {
            s.api_url = url;
        }
        s.api_key = Some(key);
    }
    s.repointed = Some(now);
    save_state(data, &s)?;
    write(&s.config, &set(&set(&text, "api_url", Some(&api_url())), "api_key", Some(&s.key)))?;
    Ok(Some(s))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Released {
    Restored(PathBuf),
    Untouched(PathBuf),
    NotTaken,
}

pub fn release(data: &Path) -> Result<Released> {
    if state(data).is_none() && !parked(data) {
        return Ok(Released::NotTaken);
    }
    let _lock = lock(data)?;
    unmark_parked(data)?;
    release_locked(data)
}

const PARKED: &str = "wakatime-parked.json";

#[derive(Serialize, Deserialize)]
struct Parked {
    config: PathBuf,
}

/// Whether kairosd gave the config back because another program has Kairos's port.
pub fn parked(data: &Path) -> bool {
    data.join(PARKED).exists()
}

fn unmark_parked(data: &Path) -> Result<()> {
    match fs::remove_file(data.join(PARKED)) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e).with_context(|| format!("removing {}", data.join(PARKED).display())),
        _ => Ok(()),
    }
}

/// Gives the config Kairos holds back while another program has Kairos's port (another account's kairosd on the same
/// computer), so editor heartbeats go where they went before instead of to a program that turns them away. [`unpark`]
/// takes it again once the port is kairosd's. `None` when Kairos holds no config.
pub fn park(data: &Path) -> Result<Option<Released>> {
    if state(data).is_none() {
        return Ok(None);
    }
    let _lock = lock(data)?;
    let Some(s) = state(data) else {
        return Ok(None);
    };
    let text = serde_json::to_string(&Parked { config: s.config.clone() })?;
    write_private(&data.join(PARKED), text.as_bytes()).with_context(|| format!("writing {}", data.join(PARKED).display()))?;
    release_locked(data).map(Some)
}

/// Takes back a config [`park`] gave back. `None` when none was.
pub fn unpark(data: &Path) -> Result<Option<PathBuf>> {
    if !parked(data) {
        return Ok(None);
    }
    let _lock = lock(data)?;
    let Some(text) = read(&data.join(PARKED))? else {
        return Ok(None);
    };
    unmark_parked(data)?;
    let Ok(p) = serde_json::from_str::<Parked>(&text) else {
        return Ok(None);
    };
    if state(data).is_some() {
        return Ok(None);
    }
    take_locked(data, &p.config)?;
    Ok(Some(p.config))
}

fn release_locked(data: &Path) -> Result<Released> {
    let Some(s) = state(data) else {
        return Ok(Released::NotTaken);
    };
    let text = read(&s.config)?.unwrap_or_default();
    let released = if get(&text, "api_url").as_deref() != Some(api_url().as_str()) {
        Released::Untouched(s.config.clone())
    } else {
        let back = set(&set(&text, "api_url", s.api_url.as_deref()), "api_key", s.api_key.as_deref());
        if !s.existed && is_empty(&back) {
            fs::remove_file(&s.config).with_context(|| format!("removing {}", s.config.display()))?;
        } else {
            write(&s.config, &back)?;
        }
        Released::Restored(s.config.clone())
    };
    fs::remove_file(data.join(STATE)).with_context(|| format!("removing {}", data.join(STATE).display()))?;
    Ok(released)
}

fn section(line: &str) -> Option<&str> {
    line.trim().strip_prefix('[')?.strip_suffix(']').map(str::trim)
}

fn key_of(line: &str) -> Option<&str> {
    let t = line.trim_start();
    if t.starts_with([';', '#']) {
        return None;
    }
    t.split_once(['=', ':']).map(|(k, _)| k.trim())
}

/// The value of `key` in the `[settings]` section, as go-ini reads it.
fn get(text: &str, key: &str) -> Option<String> {
    let mut inside = false;
    for line in text.lines() {
        if let Some(name) = section(line) {
            inside = name.eq_ignore_ascii_case(SECTION);
        } else if inside && key_of(line) == Some(key) {
            let value = line.split_once(['=', ':'])?.1.trim();
            return (!value.is_empty()).then(|| value.to_string());
        }
    }
    None
}

/// Every other line stays as it was.
fn set(text: &str, key: &str, value: Option<&str>) -> String {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let (mut start, mut end) = (None, lines.len());
    for (i, line) in lines.iter().enumerate() {
        match (section(line), start) {
            (Some(name), None) if name.eq_ignore_ascii_case(SECTION) => start = Some(i),
            (Some(_), Some(_)) => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    let entry = value.map(|v| format!("{key} = {v}"));
    match start {
        Some(start) => {
            let mut placed = false;
            let mut i = start + 1;
            while i < end {
                if key_of(&lines[i]) == Some(key) {
                    match (&entry, placed) {
                        (Some(e), false) => {
                            lines[i] = e.clone();
                            placed = true;
                            i += 1;
                        }
                        _ => {
                            lines.remove(i);
                            end -= 1;
                        }
                    }
                } else {
                    i += 1;
                }
            }
            if let (Some(e), false) = (entry, placed) {
                let last = (start..end).rev().find(|&i| !lines[i].trim().is_empty()).unwrap_or(start);
                lines.insert(last + 1, e);
            }
        }
        None => {
            if let Some(e) = entry {
                if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                    lines.push(String::new());
                }
                lines.push(format!("[{SECTION}]"));
                lines.push(e);
            }
        }
    }
    let mut out = lines.join(nl);
    if !out.is_empty() {
        out.push_str(nl);
    }
    out
}

fn drop_section(text: &str, name: &str) -> String {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut inside = false;
    let mut out = String::new();
    for line in text.lines() {
        if let Some(s) = section(line) {
            inside = s.eq_ignore_ascii_case(name);
        }
        if !inside {
            out.push_str(line);
            out.push_str(nl);
        }
    }
    out
}

fn is_empty(text: &str) -> bool {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).all(|l| section(l).is_some_and(|s| s.eq_ignore_ascii_case(SECTION)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shadow_home_follows_the_persons_config_and_starts_reading_logs_now() {
        let dir = std::env::temp_dir().join(format!("kairos-shadow-home-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let person = dir.join("person.cfg");
        let home = dir.join("shadow");
        let first = chrono::DateTime::from_timestamp(1_795_000_000, 0).unwrap();
        prepare_shadow(&home, Some(&person), "http://127.0.0.1:1/shadow/v1", "k1", first).unwrap();
        let cfg = || fs::read_to_string(home.join(SHADOW_CONFIG)).unwrap();
        assert_eq!(get(&cfg(), "api_url").as_deref(), Some("http://127.0.0.1:1/shadow/v1"));
        assert_eq!(get(&cfg(), "sync_ai_disabled").as_deref(), Some("false"));
        assert!(home.join("tmp").is_dir());
        let internal = fs::read_to_string(home.join(SHADOW_INTERNAL)).unwrap();
        assert_eq!(internal, "[internal]\nai_logs_last_parsed_at = 2026-11-18T11:06:40Z\n");

        fs::write(&person, "[settings]\napi_key = real\nexclude_unknown_project = true\n").unwrap();
        fs::write(home.join(SHADOW_INTERNAL), "[internal]\nai_logs_last_parsed_at = moved on\n").unwrap();
        prepare_shadow(&home, Some(&person), "http://127.0.0.1:1/shadow/v1", "k2", first).unwrap();
        assert_eq!(get(&cfg(), "exclude_unknown_project").as_deref(), Some("true"));
        assert_eq!(get(&cfg(), "api_key").as_deref(), Some("k2"));
        assert!(!cfg().contains("real"));
        assert!(fs::read_to_string(home.join(SHADOW_INTERNAL)).unwrap().contains("moved on"), "its own progress is kept");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(home.join(SHADOW_CONFIG)).unwrap().permissions().mode() & 0o077, 0);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_parked_config_goes_back_to_where_it_sent_and_is_taken_again() {
        let dir = std::env::temp_dir().join(format!("kairos-park-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join(".wakatime.cfg");
        let before = "[settings]\napi_url = https://hackatime.hackclub.com/api/hackatime/v1\napi_key = theirs\n";
        fs::write(&cfg, before).unwrap();
        assert_eq!(park(&dir).unwrap(), None, "nothing to give back");
        assert!(!parked(&dir));

        take(&dir, &cfg).unwrap();
        let key = state(&dir).unwrap().key;
        assert_eq!(park(&dir).unwrap(), Some(Released::Restored(cfg.clone())));
        assert_eq!(fs::read_to_string(&cfg).unwrap(), before);
        assert!(parked(&dir) && state(&dir).is_none());

        assert_eq!(unpark(&dir).unwrap(), Some(cfg.clone()));
        assert!(!parked(&dir));
        assert_eq!(points(&cfg).unwrap(), Points::Kairos);
        let s = state(&dir).unwrap();
        assert_ne!(s.key, key, "a fresh key, like any take");
        assert_eq!((s.api_url.as_deref(), s.api_key.as_deref()), (Some(HACKATIME_API_URL), Some("theirs")));
        assert_eq!(unpark(&dir).unwrap(), None);

        park(&dir).unwrap();
        assert_eq!(release(&dir).unwrap(), Released::NotTaken, "turning it off while parked");
        assert!(!parked(&dir));
        assert_eq!(unpark(&dir).unwrap(), None, "and it stays off");
        assert_eq!(fs::read_to_string(&cfg).unwrap(), before);

        take(&dir, &cfg).unwrap();
        park(&dir).unwrap();
        take(&dir, &cfg).unwrap();
        assert!(!parked(&dir), "turning it on again clears it too");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn agents_from_user_agents() {
        assert_eq!(agent("wakatime/v2.28.0 (linux-7.2.4-arch1-2-unknown) go1.27.3 opus/5-5 claude-code/2.1.319").as_deref(), Some("claude-code"));
        assert_eq!(agent("wakatime/v2.28.0 (linux-7.2.4) go1.27.3 gemini/3.8-flash-low antigravity-cli/unknown").as_deref(), Some("antigravity-cli"));
        assert_eq!(agent("wakatime/v2.28.0 (linux-7.2.4) go1.27.3"), None);
        assert_eq!(agent(""), None);
    }

    #[test]
    fn editors_from_user_agents() {
        let ua = "wakatime/v1.102.1 (linux-6.8.0-x86_64) go1.22.5 vscode/1.95.0 vscode-wakatime/24.6.2";
        assert_eq!(editor(ua).as_deref(), Some("vscode"));
        assert_eq!(editor("wakatime/v1.90.0 (windows-10.0-amd64) go1.21.1 Zed/0.160.7 zed-wakatime/0.1.9").as_deref(), Some("zed"));
        assert_eq!(editor("wakatime/v1.90.0 (darwin-23.1.0-arm64) go1.21.1 neovim/0.10.1").as_deref(), Some("neovim"));
        assert_eq!(editor("curl"), None);
    }

    #[test]
    fn reads_settings_like_go_ini() {
        let cfg = "; a comment\n[settings]\napi_url = https://hackatime.hackclub.com/api/hackatime/v1\napi_key: abc\nhidefilenames = false\n[git]\napi_key = other\n";
        assert_eq!(get(cfg, "api_url").as_deref(), Some("https://hackatime.hackclub.com/api/hackatime/v1"));
        assert_eq!(get(cfg, "api_key").as_deref(), Some("abc"));
        assert_eq!(get(cfg, "debug"), None);
        assert_eq!(get("[git]\napi_key = x\n", "api_key"), None, "only [settings] counts");
        assert_eq!(get("[settings]\n# api_key = x\napi_key =\n", "api_key"), None, "comments and empty values are unset");
    }

    #[test]
    fn set_changes_only_the_key_it_sets() {
        let cfg = "; mine\n[settings]\ndebug = true\napi_key = old\n\n[projectmap]\nfoo = bar\n";
        let out = set(cfg, "api_key", Some("new"));
        assert_eq!(out, "; mine\n[settings]\ndebug = true\napi_key = new\n\n[projectmap]\nfoo = bar\n");
        let out = set(cfg, "api_url", Some("http://x"));
        assert_eq!(out, "; mine\n[settings]\ndebug = true\napi_key = old\napi_url = http://x\n\n[projectmap]\nfoo = bar\n");
        assert_eq!(set(&out, "api_url", None), cfg);
        assert_eq!(set("[settings]\napi_key = a\napi_key = b\n", "api_key", Some("c")), "[settings]\napi_key = c\n", "duplicates go");
        assert_eq!(set("[git]\nx = 1", "api_key", Some("k")), "[git]\nx = 1\n\n[settings]\napi_key = k\n");
        assert_eq!(set("", "api_key", Some("k")), "[settings]\napi_key = k\n");
        assert_eq!(set("[Settings]\r\napi_key = a\r\n", "api_key", Some("b")), "[Settings]\r\napi_key = b\r\n", "keeps CRLF");
    }

    struct Home(PathBuf);

    impl Home {
        fn new(tag: &str) -> Home {
            let dir = std::env::temp_dir().join(format!("kairos-wakatime-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Home(dir)
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn take_and_release_a_config() {
        let home = Home::new("cfg");
        let data = home.0.join("data");
        let cfg = home.0.join(".wakatime.cfg");

        let hackatime = "[settings]\napi_url = https://hackatime.hackclub.com/api/hackatime/v1\napi_key = 11111111-2222-4333-8444-555555555555\nexclude = secret\n";
        fs::write(&cfg, hackatime).unwrap();
        assert_eq!(points(&cfg).unwrap(), Points::Elsewhere(Some("https://hackatime.hackclub.com/api/hackatime/v1".into())));
        take(&data, &cfg).unwrap();
        let s = state(&data).unwrap();
        let text = fs::read_to_string(&cfg).unwrap();
        assert_eq!(get(&text, "api_url"), Some(api_url()));
        assert_eq!(get(&text, "api_key"), Some(s.key.clone()));
        assert_eq!(get(&text, "exclude").as_deref(), Some("secret"));
        assert!(s.existed);
        assert_eq!(s.api_key.as_deref(), Some("11111111-2222-4333-8444-555555555555"));
        assert_eq!(points(&cfg).unwrap(), Points::Kairos);

        take(&data, &cfg).unwrap();
        assert_eq!(state(&data).unwrap(), s, "taking it again keeps what was saved the first time");
        assert_eq!(release(&data).unwrap(), Released::Restored(cfg.clone()));
        assert_eq!(fs::read_to_string(&cfg).unwrap(), hackatime);
        assert_eq!(state(&data), None);
        assert_eq!(release(&data).unwrap(), Released::NotTaken);

        take(&data, &cfg).unwrap();
        fs::write(&cfg, hackatime).unwrap();
        assert_eq!(release(&data).unwrap(), Released::Untouched(cfg.clone()), "pointed elsewhere since");
        assert_eq!(fs::read_to_string(&cfg).unwrap(), hackatime);

        fs::remove_file(&cfg).unwrap();
        assert_eq!(points(&cfg).unwrap(), Points::NoConfig);
        take(&data, &cfg).unwrap();
        assert!(!state(&data).unwrap().existed);
        assert_eq!(release(&data).unwrap(), Released::Restored(cfg.clone()));
        assert!(!cfg.exists(), "a config Kairos made is removed again");

        let moved = home.0.join("elsewhere").join(".wakatime.cfg");
        fs::write(&cfg, hackatime).unwrap();
        take(&data, &cfg).unwrap();
        take(&data, &moved).unwrap();
        assert_eq!(fs::read_to_string(&cfg).unwrap(), hackatime, "the config it took before is given back first");
        assert_eq!(state(&data).unwrap().config, moved);
        assert_eq!(release(&data).unwrap(), Released::Restored(moved.clone()));
        assert!(!moved.exists());
    }

    #[test]
    fn a_config_changed_under_kairos_is_pointed_back_and_what_was_written_is_passed_on_to() {
        let home = Home::new("repoint");
        let data = home.0.join("data");
        let cfg = home.0.join(".wakatime.cfg");
        let hackatime = "https://hackatime.hackclub.com/api/hackatime/v1";
        assert_eq!(repoint(&data, 1.0).unwrap(), None, "nothing taken");
        fs::write(&cfg, format!("[settings]\napi_url = {hackatime}\napi_key = 11111111-2222-4333-8444-555555555555\n")).unwrap();
        take(&data, &cfg).unwrap();
        let ours = state(&data).unwrap().key;
        assert_eq!(repoint(&data, 1.0).unwrap(), None, "still pointing at Kairos");

        let setup = format!("[settings]\napi_url = {hackatime}\napi_key = 66666666-7777-4888-9999-000000000000\nheartbeat_rate_limit_seconds = 30\n");
        fs::write(&cfg, &setup).unwrap();
        let s = repoint(&data, 50.0).unwrap().unwrap();
        assert_eq!((s.key.as_str(), s.repointed), (ours.as_str(), Some(50.0)));
        let text = fs::read_to_string(&cfg).unwrap();
        assert_eq!(get(&text, "api_url"), Some(api_url()));
        assert_eq!(get(&text, "api_key").as_deref(), Some(ours.as_str()));
        assert_eq!(get(&text, "heartbeat_rate_limit_seconds").as_deref(), Some("30"));
        let up = upstream(&data).unwrap();
        assert_eq!((up.url.as_str(), up.key.as_str()), (hackatime, "66666666-7777-4888-9999-000000000000"));
        assert_eq!(repoint(&data, 60.0).unwrap(), None);

        fs::write(&cfg, set(&text, "api_key", Some("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"))).unwrap();
        assert_eq!(repoint(&data, 70.0).unwrap().unwrap().repointed, Some(70.0), "a new key alone");
        let up = upstream(&data).unwrap();
        assert_eq!((up.url.as_str(), up.key.as_str()), (hackatime, "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"));
        assert_eq!(fs::read_to_string(&cfg).unwrap(), text);

        let other = "https://waka.example.org/api/v1";
        fs::write(&cfg, set(&text, "api_url", Some(other))).unwrap();
        assert!(repoint(&data, 75.0).unwrap().is_some(), "a new url with Kairos's key");
        let up = upstream(&data).unwrap();
        assert_eq!((up.url.as_str(), up.key.as_str()), (hackatime, "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"), "the saved key stays with its url");
        assert_eq!(fs::read_to_string(&cfg).unwrap(), text);
        fs::write(&cfg, set(&text, "api_key", None)).unwrap();
        assert!(repoint(&data, 76.0).unwrap().is_some(), "the key taken out");
        assert_eq!(upstream(&data).unwrap().key, "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee");
        assert_eq!(get(&fs::read_to_string(&cfg).unwrap(), "api_key").as_deref(), Some(ours.as_str()));
        fs::write(&cfg, set(&set(&text, "api_url", Some(other)), "api_key", Some("bbbbbbbb-bbbb-4ccc-8ddd-eeeeeeeeeeee"))).unwrap();
        repoint(&data, 77.0).unwrap();
        let up = upstream(&data).unwrap();
        assert_eq!((up.url.as_str(), up.key.as_str()), (other, "bbbbbbbb-bbbb-4ccc-8ddd-eeeeeeeeeeee"), "a new key comes with its url");
        fs::write(&cfg, set(&set(&text, "api_url", Some(hackatime)), "api_key", Some("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"))).unwrap();
        repoint(&data, 78.0).unwrap();

        fs::write(&cfg, "[settings]\ndebug = true\n").unwrap();
        assert_eq!(repoint(&data, 80.0).unwrap(), None, "emptied by hand");
        fs::write(&cfg, &text).unwrap();
        assert_eq!(release(&data).unwrap(), Released::Restored(cfg.clone()));
        let back = fs::read_to_string(&cfg).unwrap();
        assert_eq!(get(&back, "api_key").as_deref(), Some("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"), "giving it back keeps the newest key");
        assert_eq!(get(&back, "api_url").as_deref(), Some(hackatime));
    }

    #[test]
    fn pointing_back_waits_for_whoever_holds_the_config() {
        let home = Home::new("lock");
        let data = home.0.join("data");
        let cfg = home.0.join(".wakatime.cfg");
        take(&data, &cfg).unwrap();
        fs::write(&cfg, "[settings]\napi_key = 66666666-7777-4888-9999-000000000000\n").unwrap();
        let held = lock(&data).unwrap();
        let d = data.clone();
        let waiting = std::thread::spawn(move || repoint(&d, 1.0));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!waiting.is_finished());
        drop(held);
        assert!(waiting.join().unwrap().unwrap().is_some());
    }

    #[test]
    fn heartbeats_go_on_to_where_the_config_pointed_before() {
        let home = Home::new("upstream");
        let data = home.0.join("data");
        let cfg = home.0.join(".wakatime.cfg");
        let key = "11111111-2222-4333-8444-555555555555";
        let upstream_after = |text: &str| {
            fs::write(&cfg, text).unwrap();
            take(&data, &cfg).unwrap();
            let up = upstream(&data);
            release(&data).unwrap();
            up
        };

        let up = upstream_after(&format!("[settings]\napi_url = https://hackatime.hackclub.com/api/hackatime/v1\napi_key = {key}\n")).unwrap();
        assert_eq!((up.url.as_str(), up.key.as_str(), up.host()), ("https://hackatime.hackclub.com/api/hackatime/v1", key, "hackatime.hackclub.com"));
        assert!(!format!("{up:?}").contains(key), "the key stays out of debug output");
        assert_eq!(upstream(&data), None, "nothing is passed on once the config is given back");

        let up = upstream_after(&format!("[settings]\napi_key = {key}\n")).unwrap();
        assert_eq!(up.url, DEFAULT_API_URL, "wakatime-cli's own default when no url was set");
        assert_eq!(upstream_after(&format!("[settings]\napi_url = https://h.example/v1/\napi_key = {key}\n")).unwrap().url, "https://h.example/v1");
        assert_eq!(upstream_after("[settings]\napi_url = https://h.example/v1\n"), None, "no key, no relay");
        assert_eq!(upstream_after(""), None);
        for me in [api_url(), format!("{}/", api_url()), format!("http://localhost:{API_PORT}/wakatime/v1/users/current/heartbeats.bulk")] {
            assert_eq!(upstream_after(&format!("[settings]\napi_url = {me}\napi_key = {key}\n")), None, "{me} is Kairos itself");
        }
        assert_eq!(upstream_after(&format!("[settings]\napi_url = ftp://h.example/v1\napi_key = {key}\n")), None);
    }

    #[test]
    fn api_urls_are_read_like_wakatime_cli() {
        for (given, wanted) in [
            ("https://h.example/api/v1", "https://h.example/api/v1"),
            ("https://h.example/api/v1/", "https://h.example/api/v1"),
            ("https://h.example/api/v1/users/current/heartbeats", "https://h.example/api/v1"),
            ("https://h.example/api/v1/users/current/heartbeats.bulk", "https://h.example/api/v1"),
            ("https://h.example/api/v1/heartbeat", "https://h.example/api/v1"),
        ] {
            assert_eq!(normalize(given), wanted, "{given}");
        }
    }

    #[test]
    fn hosts_come_without_credentials_or_paths() {
        let host = |url: &str| Upstream { url: url.into(), key: String::new() }.host().to_string();
        assert_eq!(host("https://user:secret@h.example:8443/api/v1"), "h.example:8443");
        assert_eq!(host("http://h.example"), "h.example");
        assert_eq!(host("not a url"), "");
    }

    #[test]
    fn waiting_heartbeats_are_counted_by_their_files() {
        let home = Home::new("waiting");
        assert_eq!(relay_waiting(&home.0), 0);
        fs::create_dir_all(relay_dir(&home.0)).unwrap();
        for name in ["1.req", "2.req", "3.req.tmp", "notes"] {
            fs::write(relay_dir(&home.0).join(name), "x").unwrap();
        }
        assert_eq!(relay_waiting(&home.0), 2);
    }

    #[test]
    fn keys_are_uuids() {
        let k = new_key().unwrap();
        let parts: Vec<&str> = k.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), [8, 4, 4, 4, 12]);
        assert!(parts[2].starts_with('4') && "89ab".contains(&parts[3][..1]));
        assert_ne!(k, new_key().unwrap());
    }

    #[test]
    fn the_shadow_config_keeps_what_shapes_heartbeats_and_drops_what_sends_them_elsewhere() {
        let user = "; mine\n[settings]\napi_url = http://127.0.0.1:24816/wakatime/v1\napi_key = ours\napikey = old\napi_key_vault_cmd = pass hackatime\n\
                    exclude = ^/secret/\nexclude_unknown_project = true\nhide_branch_names = true\nhostname = box\nsync_ai_disabled = true\n\
                    heartbeat_rate_limit_seconds = 30\nproxy = socks5://x:1\noffline = false\nlog_file = /tmp/w.log\n\n\
                    [projectmap]\n^/w/a = alpha\n\n[project_api_key]\n^/w/b = 22222222-2222-4222-8222-222222222222\n\n\
                    [api_urls]\n^/w/c = https://elsewhere.example/api|33333333-3333-4333-8333-333333333333\n\n[git]\nsubmodules_disabled = true\n";
        let out = shadow_config(user, "http://127.0.0.1:24816/shadow/v1", "44444444-4444-4444-8444-444444444444");
        for (k, v) in [
            ("api_url", Some("http://127.0.0.1:24816/shadow/v1")),
            ("api_key", Some("44444444-4444-4444-8444-444444444444")),
            ("sync_ai_disabled", Some("false")),
            ("heartbeat_rate_limit_seconds", Some("0")),
            ("exclude", Some("^/secret/")),
            ("exclude_unknown_project", Some("true")),
            ("hide_branch_names", Some("true")),
            ("hostname", Some("box")),
            ("apikey", None),
            ("api_key_vault_cmd", None),
            ("proxy", None),
            ("offline", None),
            ("log_file", None),
        ] {
            assert_eq!(get(&out, k).as_deref(), v, "{k}");
        }
        assert!(out.contains("[projectmap]\n^/w/a = alpha") && out.contains("[git]\nsubmodules_disabled = true") && out.starts_with("; mine\n"));
        assert!(!out.contains("project_api_key") && !out.contains("api_urls") && !out.contains("elsewhere") && !out.contains("2222"));

        let none = shadow_config("", "http://u", "k");
        assert_eq!((get(&none, "api_url").as_deref(), get(&none, "sync_ai_disabled").as_deref()), (Some("http://u"), Some("false")));
    }

    #[test]
    fn ai_sync_as_the_persons_config_has_it() {
        let home = Home::new("aisync");
        let cfg = home.0.join(".wakatime.cfg");
        assert_eq!(ai_sync(&cfg), None);
        for (text, on) in [("[settings]\napi_key = k\n", true), ("[settings]\nsync_ai_disabled = true\n", false), ("[settings]\nsync_ai_disabled = false\n", true), ("[settings]\nsync_ai_disabled = 1\n", false)] {
            fs::write(&cfg, text).unwrap();
            assert_eq!(ai_sync(&cfg), Some(on), "{text}");
        }
    }

    #[test]
    fn a_hackatime_key_goes_where_heartbeats_are_sent_from() {
        let home = Home::new("connect");
        let data = home.0.join("data");
        let cfg = home.0.join(".wakatime.cfg");
        let key = "11111111-2222-4333-8444-555555555555";

        fs::write(&cfg, "[settings]\nexclude = x\n").unwrap();
        take(&data, &cfg).unwrap();
        assert_eq!(upstream(&data).map(|u| u.url), None);
        assert!(!on_hackatime(&data, &cfg));
        connect_hackatime(&data, &cfg, key).unwrap();
        assert!(on_hackatime(&data, &cfg));
        let up = upstream(&data).unwrap();
        assert_eq!((up.url.as_str(), up.key.as_str()), (HACKATIME_API_URL, key), "the relay passes heartbeats on with it");
        let text = fs::read_to_string(&cfg).unwrap();
        assert_eq!(get(&text, "api_url"), Some(api_url()), "editors keep sending to Kairos");
        assert_eq!(release(&data).unwrap(), Released::Restored(cfg.clone()));
        let back = fs::read_to_string(&cfg).unwrap();
        assert_eq!((get(&back, "api_url").as_deref(), get(&back, "api_key").as_deref()), (Some(HACKATIME_API_URL), Some(key)), "and keep it after uninstall");

        fs::write(&cfg, "[settings]\nexclude = x\n").unwrap();
        assert!(!on_hackatime(&data, &cfg));
        connect_hackatime(&data, &cfg, key).unwrap();
        assert!(on_hackatime(&data, &cfg), "straight from the config when Kairos doesn't hold it");
        let direct = fs::read_to_string(&cfg).unwrap();
        assert_eq!((get(&direct, "api_url").as_deref(), get(&direct, "api_key").as_deref(), get(&direct, "exclude").as_deref()), (Some(HACKATIME_API_URL), Some(key), Some("x")));
    }
}
