use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use chrono::Local;
use kairos_core::pairing::{self, Expired, Pairing};

use crate::diag::diag;
use crate::status::{self, Shared, Signin};
use crate::{now, Msg};
use menu::{is_paused, menu_model, tooltip, Action, Item};

#[cfg(any(windows, target_os = "macos"))]
mod desktop;
mod icon;
#[cfg(target_os = "linux")]
mod linux;
mod menu;
mod open;
pub(crate) mod pause;

#[cfg(any(windows, target_os = "macos"))]
use desktop as native;
#[cfg(target_os = "linux")]
use linux as native;

const REFRESH: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub items: Vec<Item>,
    pub tooltip: String,
    pub paused: bool,
}

enum Flow {
    Idle,
    Starting,
    Waiting(String),
}

pub struct Controller {
    status: Shared,
    tx: Sender<Msg>,
    dir: PathBuf,
    flow: Mutex<Flow>,
}

pub fn run(status: Shared, tx: Sender<Msg>, dir: PathBuf) -> anyhow::Result<()> {
    native::run(Arc::new(Controller { status, tx, dir, flow: Mutex::new(Flow::Idle) }))
}

impl Controller {
    pub fn view(&self) -> View {
        let s = self.status.read().map(|g| g.clone()).unwrap_or_default();
        let now = now();
        View { items: menu_model(&s, now), tooltip: tooltip(&s), paused: is_paused(&s, now) }
    }

    pub fn act(self: &Arc<Self>, action: Action) {
        match action {
            Action::OpenDashboard => open::url(&format!("{}/me", self.server())),
            Action::SignIn => self.sign_in(),
            Action::Pause(p) => match p.until(&Local::now()) {
                Some(until) => self.send(Msg::Pause(until)),
                None => diag!("kairosd: could not work out when to end the pause"),
            },
            Action::Resume => self.send(Msg::Resume),
        }
    }

    fn send(&self, msg: Msg) {
        if self.tx.send(msg).is_err() {
            diag!("kairosd: the recorder is gone, dropping a tray command");
        }
    }

    fn server(&self) -> String {
        let server = self.status.read().ok().and_then(|s| s.server.clone()).unwrap_or_else(pairing::default_server);
        server.trim_end_matches('/').to_string()
    }

    fn flow(&self) -> std::sync::MutexGuard<'_, Flow> {
        self.flow.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn sign_in(self: &Arc<Self>) {
        {
            let mut flow = self.flow();
            match &*flow {
                Flow::Starting => return,
                Flow::Waiting(url) => return open::url(url),
                Flow::Idle => *flow = Flow::Starting,
            }
        }
        status::set(&self.status, |s| s.signin = Signin::Idle);
        let me = self.clone();
        if let Err(e) = thread::Builder::new().name("signin".into()).spawn(move || me.pair()) {
            diag!("kairosd: could not start sign-in: {e}");
            self.end_sign_in(Signin::Failed { reason: "try again".into() });
        }
    }

    fn pair(&self) {
        let pairing = match Pairing::begin(&self.server()) {
            Ok(p) => p,
            Err(e) => {
                diag!("kairosd: sign-in could not start: {e:#}");
                return self.end_sign_in(Signin::Failed { reason: "can't reach the server".into() });
            }
        };
        *self.flow() = Flow::Waiting(pairing.verify_url().to_string());
        status::set(&self.status, |s| s.signin = Signin::Waiting { code: pairing.user_code().to_string() });
        open::url(pairing.verify_url());
        match pairing.wait(&self.dir) {
            Ok(acct) => {
                diag!("kairosd: signed in");
                status::set(&self.status, |s| {
                    s.username = Some(acct.username);
                    s.server = Some(acct.server);
                    s.token_rejected = false;
                    s.upload_error = None;
                });
                self.end_sign_in(Signin::Idle);
            }
            Err(e) => {
                diag!("kairosd: sign-in failed: {e:#}");
                let reason = if e.downcast_ref::<Expired>().is_some() { "code expired" } else { "try again" };
                self.end_sign_in(Signin::Failed { reason: reason.into() });
            }
        }
    }

    fn end_sign_in(&self, signin: Signin) {
        status::set(&self.status, |s| s.signin = signin);
        *self.flow() = Flow::Idle;
    }
}
