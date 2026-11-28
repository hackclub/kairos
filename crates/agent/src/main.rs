#![cfg_attr(windows, windows_subsystem = "windows")]

mod api;
mod browser;
mod classify;
mod diag;
mod exclusions;
mod focus;
mod git;
#[cfg(unix)]
mod ingest;
mod instance;
mod log;
mod logs;
mod platform;
mod ports;
mod machine;
mod project_ids;
mod recorder;
mod relay;
mod sessions;
mod shadow;
mod status;
mod tokens;
mod tray;
mod update;
mod upload;
mod wakatime;
mod watch;

use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use kairos_core::account;
use kairos_core::event::Event;
use kairos_core::paths;

use crate::browser::BrowserState;
use crate::diag::diag;
use crate::exclusions::Excluded;
use crate::platform::{Backends, Sinks};
use crate::recorder::Recorder;
use crate::sessions::Sessions;
use crate::shadow::Shadow;

pub enum Msg {
    Event(Event),
    Hook(Event),
    PresenceReady,
    /// Sent right after `PresenceReady` by backends that report `Input`, so a recorder never writes made-up input bits for one that doesn't.
    InputReady,
    /// The unix second of an input, sent at most about once a second.
    Input(f64),
    PresenceLost,
    Resumed,
    Idled,
    Pause(f64),
    Resume,
    Backends(Backends),
}

pub fn now() -> f64 {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_secs_f64() * 1000.0).round() / 1000.0
}

fn run(replace: Option<u32>) -> Result<()> {
    let dir = paths::data_dir();
    let private = paths::make_private(&dir);
    diag::init(&dir);
    if let Err(e) = private {
        diag!("kairosd: warning: could not make {} private to you: {e}", dir.display());
    }
    let wait = match replace {
        Some(pid) => {
            diag!("kairosd: taking over from {pid}");
            Duration::from_secs(30)
        }
        None => Duration::from_secs(2),
    };
    let Some(_instance) = instance::acquire_within(&dir, wait)? else {
        diag!("kairosd: already running, exiting");
        return Ok(());
    };
    // Started from a terminal, kairosd would keep that terminal's folder (often a project) as its own, and Windows
    // won't delete or rename a folder a running process is in. The updater passes it on, so it lasted until a reboot.
    if dir.is_absolute() {
        let _ = std::env::set_current_dir(&dir);
    }
    update::begin(&dir);
    let status = status::shared();
    if let Some(acct) = account::load(&dir) {
        status::set(&status, |s| {
            s.username = Some(acct.username);
            s.server = Some(acct.server);
        });
    }
    let browser = Arc::new(BrowserState::default());
    let excluded = Arc::new(Excluded::new(dir.clone()));
    let (tx, rx) = mpsc::channel::<Msg>();
    #[cfg(unix)]
    {
        ingest::listen(paths::socket_path(), tx.clone())?;
        ingest::drain_spool(&paths::spool_path(), &tx);
    }
    let hooks = std::env::var_os("KAIROS_HOOKS").is_some();

    let (wtx, wrx) = mpsc::channel();
    let (gtx, grx) = mpsc::channel();
    git::spawn(grx, gtx.clone(), tx.clone(), dir.clone(), excluded.clone());
    watch::spawn(wrx, wtx.clone(), tx.clone(), dir.clone(), gtx, excluded.clone());
    let sessions = Arc::new(Sessions::load(dir.join("sessions.json")));
    let shadow = Arc::new(Shadow::new()?);
    let bound = Arc::new(AtomicBool::new(false));
    logs::spawn(tx.clone(), wtx.clone(), dir.join("sources.json"), excluded.clone(), sessions.clone(), shadow.clone(), status.clone());
    upload::spawn(dir.clone(), status.clone(), tx.clone());
    machine::spawn(tx.clone());
    tokens::spawn(dir.clone(), excluded.clone());
    update::spawn(dir.clone(), status.clone());
    let relay = Arc::new(relay::spawn(dir.clone()));
    wakatime::keep(dir.clone(), bound.clone());
    shadow::spawn(shadow.clone(), dir.clone(), bound.clone(), tx.clone());
    let (ftx, frx) = mpsc::channel();
    focus::spawn(frx, tx.clone(), wtx, browser.clone(), excluded.clone());
    let backends = platform::spawn(Sinks { foreground: ftx, events: tx.clone() });
    let waka = api::Wakatime { shadow, sessions, bound };
    api::spawn(api::Ctx::new(paths::API_PORT, dir.clone(), status.clone(), browser, excluded, tx.clone(), relay, waka), paths::API_PORT);
    diag!("kairosd: started, log dir {}", dir.display());

    let recorder = Recorder::open(&dir, status.clone(), hooks)?;
    thread::Builder::new().name("recorder".into()).spawn(move || {
        let code = match recorder.run(backends, rx) {
            Ok(()) => 0,
            Err(e) => {
                diag!("kairosd: {e:#}");
                1
            }
        };
        std::process::exit(code)
    })?;
    // The tray owns the main thread (macOS and Windows need it there) and is not needed for
    // tracking: if it can't start, the recorder carries on and this thread just waits.
    if let Err(e) = tray::run(status, tx, dir) {
        diag!("kairosd: running without a tray: {e:#}");
    }
    loop {
        thread::park();
    }
}

fn replaces() -> Option<u32> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == "--replace").and_then(|i| args.get(i + 1)?.parse().ok())
}

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("kairosd {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if std::env::args().nth(1).as_deref() == Some("--watch-update") {
        update::watch();
        return ExitCode::SUCCESS;
    }
    match run(replaces()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            diag!("kairosd: {e:#}");
            ExitCode::from(1)
        }
    }
}
