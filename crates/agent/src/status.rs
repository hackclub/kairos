use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use kairos_core::sources::Health;
use serde::Serialize;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Signin {
    #[default]
    Idle,
    Waiting { code: String },
    Failed { reason: String },
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Status {
    pub version: &'static str,
    pub focus: &'static str,
    pub idle: &'static str,
    pub today_secs: f64,
    pub paused_until: Option<f64>,
    pub username: Option<String>,
    pub server: Option<String>,
    pub last_upload: Option<f64>,
    pub upload_error: Option<String>,
    /// Tries that failed since the last upload that went through, and when the first of them was.
    pub upload_failures: u32,
    pub upload_failing_since: Option<f64>,
    /// What each harness reader went through since kairosd started.
    pub readers: BTreeMap<&'static str, Health>,
    pub token_rejected: bool,
    pub update: Option<String>,
    pub signin: Signin,
}

pub type Shared = Arc<RwLock<Status>>;

pub fn shared() -> Shared {
    Arc::new(RwLock::new(Status { version: env!("CARGO_PKG_VERSION"), ..Status::default() }))
}

pub fn set(s: &Shared, f: impl FnOnce(&mut Status)) {
    if let Ok(mut g) = s.write() {
        f(&mut g);
    }
}
