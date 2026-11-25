//! What Hackatime would get from agents' logs if the person's wakatime-cli had AI sync on: kairosd runs that wakatime-cli
//! with it on, in a folder of its own, at the moments it would run (an agent finishing a turn, an editor plugin sending).
//! Running it live is what makes this exact, since what it makes of a log depends on when it reads it. Its heartbeats come
//! back to kairosd's API (`/shadow`), are recorded, and are never sent anywhere.

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use kairos_core::event::{Event, Kind};
use kairos_core::paths::API_PORT;
use kairos_core::wakatime::{self, Points};
use kairos_core::wakatime_cli;
use serde::Serialize;

use crate::diag::diag;
use crate::{now, Msg};

/// Nudges come in bursts (a bulk send right after a turn ends), and one run covers them all.
const SETTLE: Duration = Duration::from_secs(2);
const TIMEOUT: Duration = Duration::from_secs(300);
const VERSION_TIMEOUT: Duration = Duration::from_secs(20);
const SETUP_EVERY: Duration = Duration::from_secs(600);
/// A setup that stays the same is still recorded this often, so any stretch of time has one to go by.
const SETUP_AGAIN: Duration = Duration::from_secs(6 * 3600);
/// Getting wakatime-cli again after a failed try, doubling each time up to `INSTALL_RETRY_MAX`.
const INSTALL_RETRY: Duration = Duration::from_secs(600);
const INSTALL_RETRY_MAX: Duration = Duration::from_secs(6 * 3600);

pub fn url() -> String {
    format!("http://127.0.0.1:{API_PORT}/shadow/v1")
}

/// A run to make: with `CLAUDE_CONFIG_DIR` set to this folder, or as kairosd itself runs.
type Run = Option<PathBuf>;

pub struct Shadow {
    key: String,
    pending: Mutex<BTreeSet<Run>>,
    wake: Condvar,
}

impl Shadow {
    pub fn new() -> Result<Shadow> {
        Ok(Shadow::with_key(wakatime::new_key()?))
    }

    pub fn with_key(key: String) -> Shadow {
        Shadow { key, pending: Mutex::new(BTreeSet::new()), wake: Condvar::new() }
    }

    /// The key the shadow's wakatime-cli sends with, new each time kairosd starts.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// `claude`: the config folder of the Claude Code that just finished a turn, when wakatime-cli has to be told of it.
    pub fn nudge(&self, claude: Option<PathBuf>) {
        if let Ok(mut p) = self.pending.lock() {
            p.insert(claude);
            self.wake.notify_one();
        }
    }

    /// Waits up to `max` for a nudge, then for more of them to settle. The runs they ask for, none if there was no nudge.
    fn wait(&self, max: Duration) -> BTreeSet<Run> {
        let Ok(p) = self.pending.lock() else {
            return BTreeSet::new();
        };
        let Ok((p, _)) = self.wake.wait_timeout_while(p, max, |p| p.is_empty()) else {
            return BTreeSet::new();
        };
        if p.is_empty() {
            return BTreeSet::new();
        }
        drop(p);
        thread::sleep(SETTLE);
        self.pending.lock().map(|mut p| std::mem::take(&mut *p)).unwrap_or_default()
    }

    #[cfg(test)]
    pub fn nudged(&self) -> bool {
        self.pending.lock().is_ok_and(|p| !p.is_empty())
    }
}

#[derive(Serialize)]
struct LastRun<'a> {
    at: f64,
    secs: f64,
    cli: &'a Path,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub fn spawn(shadow: Arc<Shadow>, data: PathBuf, bound: Arc<AtomicBool>, tx: Sender<Msg>) {
    let _ = thread::Builder::new().name("hackatime-ai".into()).spawn(move || {
        // Long enough for the API to be listening, so the first setup recorded says where heartbeats really go.
        thread::sleep(Duration::from_secs(5));
        let dir = wakatime::shadow_dir(&data);
        let mut versions = Versions::default();
        let mut setup = None;
        let mut failed = None;
        let (mut install_at, mut install_failures) = (Instant::now(), 0);
        loop {
            // In this thread, so a newer wakatime-cli never goes in while the shadow is running the old one.
            if Instant::now() >= install_at {
                match wakatime_cli::install(&data) {
                    Ok(placed) => {
                        install_failures = 0;
                        if let Some(cli) = placed {
                            diag!("kairosd: put wakatime-cli {} at {}", wakatime_cli::VERSION, cli.display());
                        }
                    }
                    Err(e) => {
                        install_failures += 1;
                        install_at = Instant::now() + install_retry(install_failures);
                        diag!("kairosd: could not get wakatime-cli: {e:#}");
                    }
                }
            }
            let cli = wakatime_cli::find();
            let bound_now = bound.load(Ordering::Relaxed);
            let seen = Kind::WakatimeSetup {
                relay: bound_now && relaying(&data),
                cli: cli.as_deref().and_then(|c| versions.of(c)),
                shadow: bound_now && cli.is_some() && failed.is_none(),
                ai_sync: wakatime::config_path().and_then(|p| wakatime::ai_sync(&p)),
            };
            if setup.as_ref().is_none_or(|(s, at): &(Kind, Instant)| *s != seen || at.elapsed() >= SETUP_AGAIN) {
                let _ = tx.send(Msg::Event(Event::new(now(), None, seen.clone())));
                setup = Some((seen, Instant::now()));
            }
            let runs = shadow.wait(SETUP_EVERY);
            if runs.is_empty() || !bound.load(Ordering::Relaxed) {
                continue;
            }
            let Some(cli) = wakatime_cli::find() else {
                continue;
            };
            let started = Instant::now();
            let result = runs.iter().try_for_each(|claude| run(&dir, &cli, &url(), shadow.key(), claude.as_deref()));
            let error = result.err().map(|e| format!("{e:#}"));
            if error != failed {
                match &error {
                    Some(e) => diag!("kairosd: could not run wakatime-cli for agents' logs: {e}"),
                    None if failed.is_some() => diag!("kairosd: wakatime-cli runs for agents' logs again"),
                    None => {}
                }
                failed = error.clone();
            }
            let last = LastRun { at: now(), secs: started.elapsed().as_secs_f64(), cli: &cli, error };
            if let Ok(text) = serde_json::to_string(&last) {
                let _ = fs::write(dir.join(wakatime::SHADOW_LAST_RUN), text);
            }
        }
    });
}

fn install_retry(failures: u32) -> Duration {
    INSTALL_RETRY.saturating_mul(1 << failures.saturating_sub(1).min(6)).min(INSTALL_RETRY_MAX)
}

/// Whether editor heartbeats reach Kairos: it holds the config wakatime-cli reads and that still points here.
fn relaying(data: &Path) -> bool {
    let (Some(s), Some(config)) = (wakatime::state(data), wakatime::config_path()) else {
        return false;
    };
    s.config == config && matches!(wakatime::points(&config), Ok(Points::Kairos))
}

pub(crate) fn run(dir: &Path, cli: &Path, url: &str, key: &str, claude: Option<&Path>) -> Result<()> {
    wakatime::prepare_shadow(dir, wakatime::config_path().as_deref(), url, key, chrono::Utc::now())?;
    execute(command(cli, dir, claude), cli)
}

fn command(cli: &Path, dir: &Path, claude: Option<&Path>) -> Command {
    let tmp = dir.join("tmp");
    let mut cmd = Command::new(cli);
    cmd.arg("--sync-ai-activity")
        .env("WAKATIME_HOME", dir)
        .env("TMPDIR", &tmp)
        .env("TMP", &tmp)
        .env("TEMP", &tmp)
        .current_dir(dir);
    if let Some(claude) = claude {
        cmd.env("CLAUDE_CONFIG_DIR", claude);
    }
    cmd
}

fn execute(cmd: Command, cli: &Path) -> Result<()> {
    let mut child = start(cmd, Stdio::null()).context("starting it")?;
    match finish(&mut child, TIMEOUT)? {
        Some(0) => Ok(()),
        Some(code) => bail!("{} --sync-ai-activity exited with {code}", cli.display()),
        None => bail!("{} --sync-ai-activity was stopped after {}s", cli.display(), TIMEOUT.as_secs()),
    }
}

/// Starts `cmd` with no window, no input and nothing on stderr.
fn start(mut cmd: Command, stdout: Stdio) -> std::io::Result<Child> {
    cmd.stdin(Stdio::null()).stdout(stdout).stderr(Stdio::null());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    cmd.spawn()
}

/// Waits for `child`, stopping it after `max`. Its exit code, `None` if it was stopped.
fn finish(child: &mut Child, max: Duration) -> Result<Option<i32>> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status.code().unwrap_or(-1)));
        }
        if started.elapsed() >= max {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// `wakatime-cli --version` of each file, asked again only when the file changes.
#[derive(Default)]
struct Versions {
    known: Option<(PathBuf, Option<SystemTime>, Option<String>)>,
}

impl Versions {
    fn of(&mut self, cli: &Path) -> Option<String> {
        let modified = fs::metadata(cli).and_then(|m| m.modified()).ok();
        if let Some((path, at, version)) = &self.known {
            if path == cli && *at == modified {
                return version.clone();
            }
        }
        let version = version(cli);
        self.known = Some((cli.to_path_buf(), modified, version.clone()));
        version
    }
}

fn version(cli: &Path) -> Option<String> {
    let mut cmd = Command::new(cli);
    cmd.arg("--version");
    let mut child = start(cmd, Stdio::piped()).ok()?;
    if finish(&mut child, VERSION_TIMEOUT).ok()?? != 0 {
        return None;
    }
    let mut text = String::new();
    child.stdout.take()?.read_to_string(&mut text).ok()?;
    let v: String = text.split_whitespace().next()?.chars().filter(|c| !c.is_control()).take(40).collect();
    (!v.is_empty()).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_is_told_of_a_claude_folder_wakatime_cli_wouldnt_read() {
        let var = |cmd: &Command| cmd.get_envs().find(|(k, _)| *k == "CLAUDE_CONFIG_DIR").map(|(_, v)| v.map(PathBuf::from));
        let (cli, dir) = (Path::new("/w/wakatime-cli"), Path::new("/w/hackatime-ai"));
        assert_eq!(var(&command(cli, dir, None)), None, "kairosd's own environment as it is");
        assert_eq!(var(&command(cli, dir, Some(Path::new("/home/u/.claude-work")))), Some(Some("/home/u/.claude-work".into())));
    }

    #[test]
    fn getting_wakatime_cli_again_backs_off() {
        assert_eq!(install_retry(1), INSTALL_RETRY);
        assert_eq!(install_retry(2), INSTALL_RETRY * 2);
        assert_eq!(install_retry(6), INSTALL_RETRY * 32);
        assert_eq!(install_retry(7), INSTALL_RETRY_MAX);
        assert_eq!(install_retry(u32::MAX), INSTALL_RETRY_MAX);
    }

    #[test]
    fn nudges_ask_for_one_run_per_claude_folder() {
        let shadow = Shadow::with_key("k".into());
        let work = Some(PathBuf::from("/home/u/.claude-work"));
        for claude in [None, work.clone(), None, work.clone()] {
            shadow.nudge(claude);
        }
        assert_eq!(*shadow.pending.lock().unwrap(), BTreeSet::from([None, work]));
    }
}
