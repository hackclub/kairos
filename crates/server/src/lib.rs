pub mod accounts;
pub mod agent_work;
pub mod aliases;
pub mod api;
pub mod assets;
pub mod audit;
pub mod auth;
pub mod cli;
pub mod config;
pub mod credit;
pub mod db;
pub mod downloads;
pub mod github;
pub mod jobs;
pub mod leaderboard;
pub mod limits;
pub mod live;
pub mod networks;
pub mod prices;
pub mod programs;
pub mod security;
pub mod signals;
pub mod streak;
pub mod tokens;
pub mod util;
pub mod wakatime;
pub mod web;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::extract::DefaultBodyLimit;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{get, post};
use axum::Router;
use deadpool_postgres::Pool;

use crate::config::{AuthMode, Config};
use crate::limits::Bucket;

pub struct App {
    pub config: Config,
    pub pool: Pool,
    pub http: reqwest::Client,
    pub oidc: auth::oidc::Cache,
    pub mock: Option<auth::mock::Provider>,
    pub limits: limits::Limiter,
    pub live: live::Hub,
    pub boards: web::leaderboards::Cache,
    pub networks: networks::Table,
    pub ip_key: Option<[u8; 32]>,
}

pub type St = Arc<App>;

impl App {
    pub fn new(config: Config, pool: Pool) -> Result<St> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("kairos-server/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let mock = match config.auth {
            AuthMode::Mock => Some(auth::mock::Provider::new()?),
            AuthMode::Hca => None,
        };
        let networks = match &config.ip2asn {
            None => networks::Table::default(),
            Some(path) => networks::Table::load(path).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "no provider table, uploads get no provider or country");
                networks::Table::default()
            }),
        };
        if config.ip2asn.is_some() {
            tracing::info!(ranges = networks.len(), "provider table loaded");
        }
        let ip_key = config.ip_key.as_deref().map(networks::ip_key);
        Ok(Arc::new(App {
            config,
            pool,
            http,
            oidc: auth::oidc::Cache::default(),
            mock,
            limits: limits::Limiter::default(),
            live: live::Hub::default(),
            boards: web::leaderboards::Cache::default(),
            networks,
            ip_key,
        }))
    }
}

const KB: usize = 1024;

pub fn router(st: St) -> Router {
    let by_ip = |bucket| from_fn_with_state((st.clone(), bucket), limits::by_ip);

    let auth_routes = Router::new()
        .route("/auth/login", get(auth::login))
        .route("/auth/callback", get(auth::callback))
        .route_layer(by_ip(Bucket::Auth));

    let mut web = Router::new()
        .route("/", get(web::pages::landing))
        .route("/me", get(web::pages::me))
        .route("/me/live", get(live::stream))
        .route("/me/timezone", post(web::settings::browser_timezone))
        .route("/leaderboards", get(web::leaderboards::page))
        .route("/extension", get(web::extension::page))
        .route("/@{username}", get(web::pages::profile))
        .route("/@{username}/p/{*project}", get(web::project::page))
        .route("/device", get(web::device::page))
        .route("/device/approve", post(web::device::approve))
        .route("/device/connect", get(web::device::connect))
        .route("/settings", get(web::settings::page))
        .route("/settings/profile", post(web::settings::profile))
        .route("/settings/projects", post(web::settings::project))
        .route("/settings/projects/merge", post(web::settings::merge))
        .route("/settings/projects/unmerge", post(web::settings::unmerge))
        .route("/settings/key", post(web::settings::key))
        .route("/settings/devices/rename", post(web::settings::rename_device))
        .route("/settings/devices/revoke", post(web::settings::revoke_device))
        .route("/settings/export", get(web::export::export))
        .route("/settings/delete", post(web::settings::delete))
        .route("/settings/tokens", post(web::settings::tokens))
        .route("/admin", get(web::admin::page))
        .route("/admin/role", post(web::admin::role))
        .route("/admin/programs", post(web::admin::create_program))
        .route("/admin/programs/key", post(web::admin::create_key))
        .route("/admin/programs/revoke", post(web::admin::revoke_key))
        .route("/review", get(web::review::home))
        .route("/review/@{username}", get(web::review::person_page))
        .route("/review/@{username}/github", get(web::review::github_page))
        .route("/review/@{username}/trust", post(web::review::set_trust))
        // A 2000 character note can be four times that in UTF-8, and three times that again
        // percent-encoded.
        .route("/review/@{username}/notes", post(web::review::add_note).layer(DefaultBodyLimit::max(32 * KB)))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/logout-all", post(auth::logout_all))
        .merge(auth_routes);
    if st.mock.is_some() {
        web = web.merge(
            Router::new()
                .route("/mock-auth/oauth/authorize", get(auth::mock::authorize).post(auth::mock::approve))
                .route_layer(by_ip(Bucket::Auth)),
        );
    }
    let web = web
        .route_layer(by_ip(Bucket::Web))
        .layer(DefaultBodyLimit::max(16 * KB))
        .layer(from_fn_with_state(st.clone(), web::layer))
        .layer(from_fn_with_state(st.clone(), security::same_origin));

    let mut api = Router::new()
        .route("/api/v1/device/start", post(api::device::start).layer(DefaultBodyLimit::max(4 * KB)))
        .route("/api/v1/device/poll", post(api::device::poll).layer(DefaultBodyLimit::max(4 * KB)))
        .route("/api/v1/device/exchange", post(api::device::exchange).layer(DefaultBodyLimit::max(4 * KB)))
        .route("/api/v1/device/me", get(api::device::me))
        .route("/api/v1/device/logout", post(api::device::logout))
        .route("/api/v1/events", post(api::events::upload).layer(DefaultBodyLimit::max(8 * KB * KB)))
        .route("/api/v1/tokens", get(tokens::state).post(tokens::upload).layer(DefaultBodyLimit::max(KB * KB)))
        .route("/api/v1/users/{username}/spans", get(api::stats::spans))
        .route("/api/hackatime/v1/users/current/heartbeats", post(wakatime::one))
        .route("/api/hackatime/v1/users/current/heartbeats.bulk", post(wakatime::bulk))
        .route("/api/v1/users/current/heartbeats", post(wakatime::one))
        .route("/api/v1/users/current/heartbeats.bulk", post(wakatime::bulk))
        .layer(DefaultBodyLimit::max(2 * KB * KB))
        .merge(api::read::router());
    if st.mock.is_some() {
        api = api.merge(
            Router::new()
                .route("/mock-auth/.well-known/openid-configuration", get(auth::mock::discovery))
                .route("/mock-auth/oauth/token", post(auth::mock::token))
                .route("/mock-auth/oauth/userinfo", get(auth::mock::userinfo))
                .route("/mock-auth/oauth/discovery/keys", get(auth::mock::jwks))
                .layer(DefaultBodyLimit::max(16 * KB)),
        );
    }

    let public = Router::new()
        .route("/static/{*path}", get(assets::serve))
        .route("/og.png", get(web::pages::og))
        .route("/favicon.svg", get(web::pages::favicon))
        .route("/install.ps1", get(downloads::install_ps1))
        .route("/install.sh", get(downloads::install_sh))
        .route("/download/latest/{file}", get(downloads::latest))
        .route("/download/{version}/{target}/{file}", get(downloads::file))
        .route("/healthz", get(|| async { "ok" }));

    Router::new()
        .merge(web)
        .merge(api)
        .merge(public)
        .layer(from_fn(security::headers))
        .layer(from_fn(security::log))
        .with_state(st)
}
