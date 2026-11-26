use std::collections::HashMap;
use std::sync::OnceLock;

use askama::Template;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Form, Json};
use serde::Deserialize;
use serde_json::json;

use super::pages::{ago, os_name};
use super::{Base, Ctx};
use crate::accounts::{self, check_username};
use crate::aliases::{self, canon_projects};
use crate::auth::{clear_session, Session, SignedIn};
use crate::{audit, credit, live, tokens};
use crate::util::{clean, err, now, Res};
use crate::St;

pub fn timezones() -> &'static [&'static str] {
    static TZ: OnceLock<Vec<&'static str>> = OnceLock::new();
    TZ.get_or_init(|| {
        let mut v: Vec<&'static str> = chrono_tz::TZ_VARIANTS.iter().map(|t| t.name()).collect();
        v.sort_unstable();
        v
    })
}

pub struct Profile {
    pub username: String,
    pub display_name: String,
    pub is_public: bool,
    pub show_ai_tools: bool,
    pub timezone: String,
    pub email: Option<String>,
    pub slack_id: Option<String>,
    pub verification_status: Option<String>,
}

pub struct ProjectRow {
    pub id: String,
    pub hidden: bool,
    pub repo_url: Option<String>,
    pub merged: Vec<String>,
}

pub struct DeviceRow {
    pub id: i64,
    pub name: String,
    pub os: String,
    pub version: Option<String>,
    pub last_seen: String,
    pub revoked: bool,
}

pub struct KeyInfo {
    pub prefix: String,
    pub created: String,
    pub last_used: String,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    base: Base,
    profile: Profile,
    timezones: &'static [&'static str],
    projects: Vec<ProjectRow>,
    merge_targets: Vec<String>,
    devices: Vec<DeviceRow>,
    key: Option<KeyInfo>,
    new_key: Option<String>,
    notice: Option<String>,
    error: Option<String>,
    self_delete: bool,
    tokens: tokens::Settings,
    token_data: bool,
}

const NOTICES: &[(&str, &str)] = &[
    ("profile", "Profile saved."),
    ("project", "Project visibility saved."),
    ("device", "Device updated."),
    ("revoked", "Device disconnected. It can't upload anymore."),
    ("tokens_on", "Token stats are on. Your devices start sending them within a few minutes."),
    ("tokens_saved", "Token stats saved."),
    ("tokens_off", "Token stats are off."),
    ("tokens_deleted", "Token stats are off, and the ones Kairos had are deleted."),
];

#[derive(Deserialize)]
pub struct NoticeQ {
    done: Option<String>,
}

async fn show(st: &St, ctx: &Ctx, s: &Session, status: StatusCode, notice: Option<String>, error: Option<String>, new_key: Option<String>) -> Res<Response> {
    let c = st.pool.get().await?;
    let u = c
        .query_one(
            "SELECT username, display_name, is_public, show_ai_tools, timezone, email, slack_id, verification_status FROM users WHERE id = $1",
            &[&s.user.id],
        )
        .await?;
    let profile = Profile {
        username: u.get(0),
        display_name: u.get::<_, Option<String>>(1).unwrap_or_default(),
        is_public: u.get(2),
        show_ai_tools: u.get(3),
        timezone: u.get(4),
        email: u.get(5),
        slack_id: u.get(6),
        verification_status: u.get(7),
    };
    let mut merged: HashMap<String, Vec<String>> = HashMap::new();
    for (alias, canonical) in aliases::map(&c, s.user.id).await? {
        merged.entry(canonical).or_default().push(alias);
    }
    let projects: Vec<ProjectRow> = c
        .query(&format!("SELECT id, hidden, repo_url FROM {} p ORDER BY last_seen DESC LIMIT 200", canon_projects!()), &[&s.user.id])
        .await?
        .iter()
        .map(|r| {
            let id: String = r.get(0);
            let mut merged = merged.remove(&id).unwrap_or_default();
            merged.sort();
            ProjectRow { id, hidden: r.get(1), repo_url: r.get(2), merged }
        })
        .collect();
    let merge_targets = projects.iter().map(|p| p.id.clone()).collect();
    let devices = c
        .query(
            "SELECT id, name, os, client_version, extract(epoch FROM now() - last_seen_at)::float8, revoked_at IS NOT NULL
             FROM devices WHERE user_id = $1 ORDER BY revoked_at IS NOT NULL, id",
            &[&s.user.id],
        )
        .await?
        .iter()
        .map(|r| DeviceRow {
            id: r.get(0),
            name: r.get(1),
            os: os_name(&r.get::<_, String>(2)),
            version: r.get(3),
            last_seen: r.get::<_, Option<f64>>(4).map_or_else(|| "not seen yet".to_string(), |a| format!("seen {}", ago(a))),
            revoked: r.get(5),
        })
        .collect();
    let key = c
        .query_opt(
            "SELECT prefix, to_char(created_at, 'YYYY-MM-DD'), COALESCE(to_char(last_used_at, 'YYYY-MM-DD HH24:MI'), 'never')
             FROM api_keys WHERE user_id = $1 AND kind = 'editor' AND revoked_at IS NULL",
            &[&s.user.id],
        )
        .await?
        .map(|r| KeyInfo { prefix: r.get(0), created: r.get(1), last_used: r.get(2) });
    let token_settings = tokens::settings(&c, s.user.id).await?;
    let token_data = tokens::any(&c, s.user.id, None).await?;
    let page = SettingsPage {
        base: Base::new(st, ctx, "Settings · Kairos"),
        profile,
        timezones: timezones(),
        projects,
        merge_targets,
        devices,
        key,
        new_key,
        notice,
        error,
        self_delete: st.config.self_delete,
        tokens: token_settings,
        token_data,
    };
    Ok((status, axum::response::Html(page.render()?)).into_response())
}

pub async fn page(State(st): State<St>, ctx: Ctx, SignedIn(s): SignedIn, Query(q): Query<NoticeQ>) -> Res<Response> {
    let notice = q.done.and_then(|d| NOTICES.iter().find(|n| n.0 == d).map(|n| n.1.to_string()));
    show(&st, &ctx, &s, StatusCode::OK, notice, None, None).await
}

fn done(what: &str) -> Response {
    Redirect::to(&format!("/settings?done={what}")).into_response()
}

#[derive(Deserialize)]
pub struct ProfileForm {
    csrf: String,
    username: String,
    #[serde(default)]
    display_name: String,
    is_public: Option<String>,
    show_ai_tools: Option<String>,
    timezone: String,
}

pub async fn profile(State(st): State<St>, ctx: Ctx, SignedIn(s): SignedIn, Form(f): Form<ProfileForm>) -> Res<Response> {
    s.check_csrf(&f.csrf)?;
    let bad = |msg: String| show(&st, &ctx, &s, StatusCode::BAD_REQUEST, None, Some(msg), None);
    let username = f.username.trim().to_lowercase();
    if let Err(msg) = check_username(&username) {
        return bad(msg.into()).await;
    }
    let Ok(tz) = f.timezone.parse::<chrono_tz::Tz>() else {
        return bad("Pick a timezone from the list.".into()).await;
    };
    let display = Some(clean(&f.display_name, 64)).filter(|d| !d.is_empty());
    let mut c = st.pool.get().await?;
    let tx = c.transaction().await?;
    let old_tz = credit::lock_user(&tx, s.user.id).await?;
    let taken = tx.query_opt("SELECT 1 FROM users WHERE username = $1 AND id <> $2", &[&username, &s.user.id]).await?.is_some();
    if taken {
        drop(tx);
        drop(c);
        return bad("That username is taken.".into()).await;
    }
    tx.execute(
        "UPDATE users SET username = $2, display_name = $3, is_public = $4, show_ai_tools = $5, timezone = $6, timezone_set = TRUE
         WHERE id = $1",
        &[&s.user.id, &username, &display, &f.is_public.is_some(), &f.show_ai_tools.is_some(), &tz.name()],
    )
    .await?;
    if old_tz != tz {
        credit::rebuild_all(&tx, s.user.id, tz).await?;
    }
    tx.commit().await?;
    live::publish(&st, s.user.id);
    Ok(done("profile"))
}

#[derive(Deserialize)]
pub struct TokensForm {
    csrf: String,
    on: String,
    public: Option<String>,
    delete: Option<String>,
}

/// Turning token stats off also takes them off public pages, so turning them on again starts private.
pub async fn tokens(State(st): State<St>, SignedIn(s): SignedIn, Form(f): Form<TokensForm>) -> Res<Response> {
    s.check_csrf(&f.csrf)?;
    let mut c = st.pool.get().await?;
    let tx = c.transaction().await?;
    let what = if f.on == "1" {
        let was = tokens::settings(&tx, s.user.id).await?.on;
        tx.execute("UPDATE users SET tokens_on = TRUE, tokens_public = $2 WHERE id = $1", &[&s.user.id, &f.public.is_some()]).await?;
        if was { "tokens_saved" } else { "tokens_on" }
    } else {
        tx.execute("UPDATE users SET tokens_on = FALSE, tokens_public = FALSE WHERE id = $1", &[&s.user.id]).await?;
        if f.delete.is_some() {
            tx.execute("DELETE FROM token_hours WHERE user_id = $1", &[&s.user.id]).await?;
            "tokens_deleted"
        } else {
            "tokens_off"
        }
    };
    tx.commit().await?;
    Ok(Redirect::to(&format!("/settings?done={what}#tokens")).into_response())
}

#[derive(Deserialize)]
pub struct BrowserTimezone {
    csrf: String,
    timezone: String,
}

/// Taken once, for an account that never picked one. `changed` means its days moved: reload.
pub async fn browser_timezone(State(st): State<St>, SignedIn(s): SignedIn, Form(f): Form<BrowserTimezone>) -> Res<Json<serde_json::Value>> {
    s.check_csrf(&f.csrf)?;
    let tz = f.timezone.parse::<chrono_tz::Tz>().map_err(|_| err(StatusCode::BAD_REQUEST, "unknown timezone"))?;
    let mut c = st.pool.get().await?;
    let tx = c.transaction().await?;
    let old_tz = credit::lock_user(&tx, s.user.id).await?;
    let taken = tx
        .execute("UPDATE users SET timezone = $2, timezone_set = TRUE WHERE id = $1 AND NOT timezone_set", &[&s.user.id, &tz.name()])
        .await?
        == 1;
    let changed = taken && old_tz != tz;
    if changed {
        credit::rebuild_all(&tx, s.user.id, tz).await?;
    }
    tx.commit().await?;
    if changed {
        live::publish(&st, s.user.id);
    }
    Ok(Json(serde_json::json!({ "changed": changed })))
}

#[derive(Deserialize)]
pub struct ProjectForm {
    csrf: String,
    project: String,
    hidden: String,
    #[serde(default)]
    next: String,
}

pub async fn project(State(st): State<St>, SignedIn(s): SignedIn, Form(f): Form<ProjectForm>) -> Res<Response> {
    s.check_csrf(&f.csrf)?;
    let c = st.pool.get().await?;
    c.execute(
        "UPDATE projects SET hidden = $3 WHERE user_id = $1
            AND (id = $2 OR id IN (SELECT alias FROM project_aliases WHERE user_id = $1 AND canonical = $2))",
        &[&s.user.id, &f.project, &(f.hidden == "1")],
    )
    .await?;
    if f.next.is_empty() {
        return Ok(done("project"));
    }
    Ok(Redirect::to(&crate::auth::safe_next(Some(&f.next))).into_response())
}

#[derive(Deserialize)]
pub struct MergeForm {
    csrf: String,
    from: String,
    into: String,
}

pub async fn merge(State(st): State<St>, ctx: Ctx, SignedIn(s): SignedIn, Form(f): Form<MergeForm>) -> Res<Response> {
    s.check_csrf(&f.csrf)?;
    let bad = |msg: &str| show(&st, &ctx, &s, StatusCode::BAD_REQUEST, None, Some(msg.into()), None);
    let mut c = st.pool.get().await?;
    let tx = c.transaction().await?;
    let tz = credit::lock_user(&tx, s.user.id).await?;
    let map = aliases::map(&tx, s.user.id).await?;
    let known: Vec<String> = tx
        .query("SELECT id FROM projects WHERE user_id = $1 AND id = ANY($2)", &[&s.user.id, &vec![f.from.clone(), f.into.clone()]])
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    let mine = |id: &String| known.contains(id) && !map.contains_key(id);
    let problem = if f.from == f.into {
        Some("Pick a different project to merge into.")
    } else if !mine(&f.from) {
        Some("That project can't be merged.")
    } else if !mine(&f.into) {
        Some("Pick one of your other projects to merge into.")
    } else {
        None
    };
    if let Some(msg) = problem {
        drop(tx);
        drop(c);
        return bad(msg).await;
    }
    aliases::link(&tx, s.user.id, &[(f.from.clone(), f.into.clone(), now())], "merge").await?;
    aliases::refresh(&tx, s.user.id).await?;
    credit::rebuild_all(&tx, s.user.id, tz).await?;
    audit::log(&tx, Some(s.user.id), "project.merge", Some(s.user.id), json!({ "from": f.from, "into": f.into })).await?;
    tx.commit().await?;
    drop(c);
    live::publish(&st, s.user.id);
    let notice = format!("Merged. Time from {} now counts toward {}.", f.from, f.into);
    show(&st, &ctx, &s, StatusCode::OK, Some(notice), None, None).await
}

#[derive(Deserialize)]
pub struct UnmergeForm {
    csrf: String,
    alias: String,
}

pub async fn unmerge(State(st): State<St>, ctx: Ctx, SignedIn(s): SignedIn, Form(f): Form<UnmergeForm>) -> Res<Response> {
    s.check_csrf(&f.csrf)?;
    let mut c = st.pool.get().await?;
    let tx = c.transaction().await?;
    let tz = credit::lock_user(&tx, s.user.id).await?;
    let map = aliases::map(&tx, s.user.id).await?;
    let Some(into) = map.get(&f.alias).cloned() else {
        drop(tx);
        drop(c);
        return show(&st, &ctx, &s, StatusCode::BAD_REQUEST, None, Some("That project isn't merged into another one.".into()), None).await;
    };
    aliases::link(&tx, s.user.id, &[(f.alias.clone(), into.clone(), now())], "split").await?;
    aliases::refresh(&tx, s.user.id).await?;
    credit::rebuild_all(&tx, s.user.id, tz).await?;
    audit::log(&tx, Some(s.user.id), "project.unmerge", Some(s.user.id), json!({ "from": f.alias, "into": into })).await?;
    tx.commit().await?;
    drop(c);
    live::publish(&st, s.user.id);
    show(&st, &ctx, &s, StatusCode::OK, Some(format!("Unmerged. {} is its own project again.", f.alias)), None, None).await
}

#[derive(Deserialize)]
pub struct CsrfOnly {
    csrf: String,
}

pub async fn key(State(st): State<St>, ctx: Ctx, SignedIn(s): SignedIn, Form(f): Form<CsrfOnly>) -> Res<Response> {
    s.check_csrf(&f.csrf)?;
    let key = accounts::new_editor_key(&st, s.user.id).await?;
    show(&st, &ctx, &s, StatusCode::OK, None, None, Some(key)).await
}

#[derive(Deserialize)]
pub struct DeviceForm {
    csrf: String,
    device_id: i64,
    #[serde(default)]
    name: String,
}

pub async fn rename_device(State(st): State<St>, ctx: Ctx, SignedIn(s): SignedIn, Form(f): Form<DeviceForm>) -> Res<Response> {
    s.check_csrf(&f.csrf)?;
    let name = clean(&f.name, 64);
    if name.is_empty() {
        return show(&st, &ctx, &s, StatusCode::BAD_REQUEST, None, Some("Give the device a name.".into()), None).await;
    }
    let c = st.pool.get().await?;
    c.execute("UPDATE devices SET name = $3 WHERE id = $1 AND user_id = $2", &[&f.device_id, &s.user.id, &name]).await?;
    Ok(done("device"))
}

pub async fn revoke_device(State(st): State<St>, SignedIn(s): SignedIn, Form(f): Form<DeviceForm>) -> Res<Response> {
    s.check_csrf(&f.csrf)?;
    let c = st.pool.get().await?;
    c.execute(
        "UPDATE devices SET revoked_at = now() WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
        &[&f.device_id, &s.user.id],
    )
    .await?;
    Ok(done("revoked"))
}

#[derive(Deserialize)]
pub struct DeleteForm {
    csrf: String,
    confirm: String,
}

pub async fn delete(State(st): State<St>, ctx: Ctx, SignedIn(s): SignedIn, Form(f): Form<DeleteForm>) -> Res<Response> {
    s.check_csrf(&f.csrf)?;
    if !st.config.self_delete {
        return Err(err(StatusCode::NOT_FOUND, "not found"));
    }
    if f.confirm.trim() != s.user.username {
        let msg = "Type your username exactly to delete your account.".to_string();
        return show(&st, &ctx, &s, StatusCode::BAD_REQUEST, None, Some(msg), None).await;
    }
    accounts::delete(&st.pool, s.user.id).await?;
    let page = super::Message {
        base: Base::new(&st, &Ctx { nonce: ctx.nonce.clone(), session: None }, "Account deleted · Kairos"),
        heading: "Account deleted".into(),
        text: "Your account and everything Kairos stored about it are gone. Uninstall the app with kairos uninstall.".into(),
    };
    let mut res = super::render(&page)?;
    clear_session(&st, &mut res)?;
    Ok(res)
}
