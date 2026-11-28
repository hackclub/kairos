mod common;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use common::{upload, user_id, Browser, Chain, TestApp};
use kairos_core::event::Kind;
use kairos_server::accounts;
use kairos_server::util::hash;
use serde_json::{json, Value};

const TABLES: &[&str] = &["web_sessions", "api_keys", "devices", "spans", "agent_spans", "user_days", "projects", "commits", "heartbeats"];

async fn editor_key(b: &mut Browser<'_>) -> String {
    let csrf = b.csrf().await;
    let page = b.post("/settings/key", &[("csrf", &csrf)]).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let at = page.body.find("api_key = ").expect("key shown once") + "api_key = ".len();
    page.body[at..].split('<').next().unwrap().trim().to_string()
}

async fn heartbeat(app: &TestApp, key: &str, time: f64) -> StatusCode {
    post_heartbeat(app, key, json!({ "time": time, "entity": "/home/ada/acme/main.rs", "project": "acme", "language": "Rust" })).await
}

async fn post_heartbeat(app: &TestApp, key: &str, body: Value) -> StatusCode {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/hackatime/v1/users/current/heartbeats")
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .header(header::USER_AGENT, "wakatime/v1.102.1 (linux) go1.22.5 vscode/1.95.0 vscode-wakatime/24.6.2")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    app.send(req).await.status
}

async fn fill(app: &TestApp, b: &mut Browser<'_>) -> (String, String) {
    let token = b.pair("laptop").await;
    let mut chain = Chain::default();
    let t0 = 1_773_000_000.0;
    chain.editing("acme/app", t0, t0 + 600.0);
    chain.push(t0 + 610.0, Some("acme/app"), Kind::Commit { sha: "abc1234".into(), files: 2, added: 10, removed: 1, authored: None, committed: None, via: None, changes: vec![], replaces: None, not_before: None });
    chain.push(t0 + 620.0, Some("acme/app"), Kind::AgentStart { session: "s1".into() });
    chain.push(t0 + 900.0, Some("acme/app"), Kind::ReplyDone {
        session: "s1".into(),
        reply_words: 40,
        tools: 3,
        files_changed: 1,
        model: None,
    });
    let res = upload(app, &token, chain.take()).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let key = editor_key(b).await;
    assert_eq!(heartbeat(app, &key, t0 + 30.0).await, StatusCode::CREATED);
    (token, key)
}

#[tokio::test]
async fn editor_keys_are_uuids_and_rotate() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let first = editor_key(&mut b).await;
    assert_eq!(first.len(), 36);
    assert_eq!(first.matches('-').count(), 4);
    assert_eq!(heartbeat(&app, &first, 1_773_000_000.0).await, StatusCode::CREATED);
    let second = editor_key(&mut b).await;
    assert_ne!(first, second);
    assert_eq!(heartbeat(&app, &first, 1_773_000_060.0).await, StatusCode::UNAUTHORIZED);
    assert_eq!(heartbeat(&app, &second, 1_773_000_060.0).await, StatusCode::CREATED);
    let settings = b.get("/settings").await;
    assert!(!settings.body.contains(&second), "keys are shown once");
    app.finish().await;
}

#[tokio::test]
async fn heartbeats_made_from_agent_logs_are_answered_and_not_kept() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let key = editor_key(&mut b).await;
    let agent = json!({ "time": 1_773_000_000.0, "entity": "Claude 0505d47f", "type": "app", "category": "ai coding", "project": "acme", "ai_session": "0505d47f" });
    assert_eq!(post_heartbeat(&app, &key, agent).await, StatusCode::CREATED);
    let beside = json!({ "time": 1_773_000_030.0, "entity": "/home/ada/acme/lib.rs", "type": "file", "category": "ai coding", "project": "acme" });
    assert_eq!(post_heartbeat(&app, &key, beside).await, StatusCode::CREATED);
    assert_eq!(heartbeat(&app, &key, 1_773_000_060.0).await, StatusCode::CREATED);
    let uid = user_id(&app, "ada").await;
    assert_eq!(app.count("SELECT count(*) FROM heartbeats WHERE user_id = $1", uid).await, 2, "the editor's own heartbeats stay, whatever their category");
    app.finish().await;
}

#[tokio::test]
async fn export_has_everything_and_no_secrets() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let (token, key) = fill(&app, &mut b).await;
    let session = b.jar.get("kairos_session").unwrap().clone();

    let res = b.get("/settings/export").await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.headers.get(header::CONTENT_DISPOSITION).unwrap().to_str().unwrap().contains("kairos-ada.json"));
    let v: Value = res.json();
    assert_eq!(v["format"], "kairos-export");
    assert_eq!(v["profile"]["username"], "ada");
    assert_eq!(v["profile"]["email"], "ada@example.com");
    for (section, min) in [("devices", 1), ("device_networks", 1), ("projects", 1), ("commits", 1), ("days", 1), ("spans", 1), ("agent_runtime", 1), ("heartbeats", 1), ("events", 14)] {
        let n = v[section].as_array().unwrap_or_else(|| panic!("{section} missing")).len();
        assert!(n >= min, "{section}: {n}");
    }
    assert_eq!(v["events"][0]["event"]["kind"], "focus");
    for secret in [token.as_str(), key.as_str(), session.as_str(), &hash(&token), &hash(&key), &hash(&session), "token_hash", "key_hash"] {
        assert!(!res.body.contains(secret), "export leaks {secret}");
    }

    let mut stranger = app.browser();
    let anon = stranger.get("/settings/export").await;
    assert_eq!(anon.status, StatusCode::SEE_OTHER);
    app.finish().await;
}

#[tokio::test]
async fn deleting_an_account_removes_every_row() {
    let app = TestApp::with(SELF_DELETE).await;
    let mut bea = app.browser();
    bea.sign_in("Bea Hacker", "bea@example.com").await;
    fill(&app, &mut bea).await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let (token, key) = fill(&app, &mut b).await;
    let uid = user_id(&app, "ada").await;
    let bea_id = user_id(&app, "bea").await;
    let c = app.st.pool.get().await.unwrap();
    let device: i64 = c.query_one("SELECT id FROM devices WHERE user_id = $1", &[&uid]).await.unwrap().get(0);

    let csrf = b.csrf().await;
    let wrong = b.post("/settings/delete", &[("csrf", &csrf), ("confirm", "bea")]).await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST);
    let res = b.post("/settings/delete", &[("csrf", &csrf), ("confirm", "ada")]).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body.contains("Account deleted"));
    assert!(res.cookies().iter().any(|c| c.starts_with("kairos_session=;") && c.contains("Max-Age=0")));

    assert_eq!(app.count("SELECT count(*) FROM users WHERE id = $1", uid).await, 0);
    for t in TABLES {
        assert_eq!(app.count(&format!("SELECT count(*) FROM {t} WHERE user_id = $1"), uid).await, 0, "{t}");
        assert!(app.count(&format!("SELECT count(*) FROM {t} WHERE user_id = $1"), bea_id).await > 0, "{t} for bea");
    }
    let events: i64 = c.query_one("SELECT count(*) FROM events WHERE device_id = $1", &[&device]).await.unwrap().get(0);
    assert_eq!(events, 0);
    let audit = c
        .query_one("SELECT target_user_id, detail FROM audit_log WHERE action = 'account.delete'", &[])
        .await
        .unwrap();
    assert_eq!(audit.get::<_, Option<i64>>(0), None);
    assert_eq!(audit.get::<_, Value>(1)["user_id"], uid);

    assert_eq!(b.get("/me").await.status, StatusCode::SEE_OTHER);
    assert_eq!(upload(&app, &token, Vec::new()).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(heartbeat(&app, &key, 1_773_000_000.0).await, StatusCode::UNAUTHORIZED);
    app.finish().await;
}

const SELF_DELETE: &[(&str, &str)] = &[("KAIROS_SELF_DELETE", "1")];

#[tokio::test]
async fn settings_has_no_delete_unless_self_delete_is_on() {
    let app = TestApp::new().await;
    let mut ada = app.browser();
    ada.sign_in("Ada Lovelace", "ada@example.com").await;
    let page = ada.get("/settings").await;
    assert!(page.body.contains("Download my data") && !page.body.contains("Delete account"), "{}", page.body);
    let csrf = ada.csrf().await;
    assert_eq!(ada.post("/settings/delete", &[("csrf", &csrf), ("confirm", "ada")]).await.status, StatusCode::NOT_FOUND);
    let id = user_id(&app, "ada").await;
    assert_eq!(app.count("SELECT count(*) FROM users WHERE id = $1", id).await, 1);
    accounts::delete(&app.st.pool, id).await.unwrap();
    assert_eq!(app.count("SELECT count(*) FROM users WHERE id = $1", id).await, 0);
    app.finish().await;
}

async fn delete_account(b: &mut Browser<'_>, username: &str) {
    let csrf = b.csrf().await;
    let res = b.post("/settings/delete", &[("csrf", &csrf), ("confirm", username)]).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

#[tokio::test]
async fn a_banned_or_reviewed_account_gets_its_record_back_after_deleting_and_signing_in_again() {
    let app = TestApp::with(SELF_DELETE).await;
    let mut bea = app.browser();
    bea.sign_in("Bea Hacker", "bea@example.com").await;
    let bea_id = user_id(&app, "bea").await;
    let mut ada = app.browser();
    ada.sign_in("Ada Lovelace", "ada@example.com").await;
    let old_id = user_id(&app, "ada").await;
    let c = app.st.pool.get().await.unwrap();
    c.execute("UPDATE users SET trust = 'banned' WHERE id = $1", &[&old_id]).await.unwrap();
    for (author, body) in [(Some(bea_id), "first look"), (None, "second look")] {
        c.execute("INSERT INTO review_notes (user_id, author_id, body) VALUES ($1, $2, $3)", &[&old_id, &author, &body]).await.unwrap();
    }
    delete_account(&mut ada, "ada").await;
    assert_eq!(app.count("SELECT count(*) FROM users WHERE id = $1", old_id).await, 0);
    assert_eq!(c.query_one("SELECT count(*) FROM departed", &[]).await.unwrap().get::<_, i64>(0), 1);

    let mut again = app.browser();
    again.sign_in("Ada Lovelace", "ada@example.com").await;
    let new_id = user_id(&app, "ada").await;
    assert_ne!(new_id, old_id);
    let trust: String = c.query_one("SELECT trust FROM users WHERE id = $1", &[&new_id]).await.unwrap().get(0);
    assert_eq!(trust, "banned");
    let notes: Vec<(Option<i64>, String)> = c
        .query("SELECT author_id, body FROM review_notes WHERE user_id = $1 ORDER BY id", &[&new_id])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(notes, [(Some(bea_id), "first look".to_string()), (None, "second look".to_string())]);
    assert_eq!(c.query_one("SELECT count(*) FROM departed", &[]).await.unwrap().get::<_, i64>(0), 0);
    let restored = c.query_one("SELECT detail FROM audit_log WHERE action = 'trust.restore' AND target_user_id = $1", &[&new_id]).await.unwrap();
    assert_eq!(restored.get::<_, Value>(0), json!({ "to": "banned", "notes": 2 }));

    app.browser().sign_in("Ada Lovelace", "ada@example.com").await;
    assert_eq!(app.count("SELECT count(*) FROM review_notes WHERE user_id = $1", new_id).await, 2, "signing in again adds nothing");
    app.finish().await;
}

#[tokio::test]
async fn deleting_an_ordinary_account_leaves_nothing_behind_to_restore() {
    let app = TestApp::with(SELF_DELETE).await;
    let mut ada = app.browser();
    ada.sign_in("Ada Lovelace", "ada@example.com").await;
    delete_account(&mut ada, "ada").await;
    let c = app.st.pool.get().await.unwrap();
    assert_eq!(c.query_one("SELECT count(*) FROM departed", &[]).await.unwrap().get::<_, i64>(0), 0);
    app.finish().await;
}

#[tokio::test]
async fn grant_sets_roles_and_audits() {
    let app = TestApp::new().await;
    let mut ada = app.browser();
    ada.sign_in("Ada Lovelace", "ada@example.com").await;
    let mut bea = app.browser();
    bea.sign_in("Bea Hacker", "bea@example.com").await;

    assert_eq!(ada.get("/admin").await.status, StatusCode::NOT_FOUND);
    assert!(accounts::grant(&app.st.pool, None, "ada", "root", "cli").await.is_err());
    assert!(accounts::grant(&app.st.pool, None, "nobody", "admin", "cli").await.is_err());
    assert_eq!(accounts::grant(&app.st.pool, None, "Ada", "admin", "cli").await.unwrap(), "user");

    let page = ada.get("/admin?q=bea").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("bea"));
    let csrf = page.csrf();
    let res = ada.post("/admin/role", &[("csrf", &csrf), ("username", "bea"), ("role", "reviewer")]).await;
    assert!(res.status.is_redirection() || res.status.is_success(), "{} {}", res.status, res.body);
    let own = ada.post("/admin/role", &[("csrf", &csrf), ("username", "ada"), ("role", "user")]).await;
    assert!(own.status.is_client_error(), "{}", own.status);
    assert_eq!(bea.get("/admin").await.status, StatusCode::NOT_FOUND);
    let bea_csrf = bea.csrf().await;
    let res = bea.post("/admin/role", &[("csrf", &bea_csrf), ("username", "bea"), ("role", "admin")]).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    let ada_id = user_id(&app, "ada").await;
    let bea_id = user_id(&app, "bea").await;
    let c = app.st.pool.get().await.unwrap();
    let roles: Vec<(String, String)> =
        c.query("SELECT username, role FROM users ORDER BY username", &[]).await.unwrap().iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(roles, vec![("ada".into(), "admin".into()), ("bea".into(), "reviewer".into())]);
    let rows = c
        .query("SELECT actor_id, action, target_user_id, detail FROM audit_log WHERE action = 'role.change' ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<_, Option<i64>>(0), None);
    assert_eq!(rows[0].get::<_, Option<i64>>(2), Some(ada_id));
    assert_eq!(rows[0].get::<_, Value>(3), json!({ "from": "user", "to": "admin", "via": "cli" }));
    assert_eq!(rows[1].get::<_, Option<i64>>(0), Some(ada_id));
    assert_eq!(rows[1].get::<_, Option<i64>>(2), Some(bea_id));
    assert_eq!(rows[1].get::<_, Value>(3)["to"], "reviewer");
    assert_eq!(app.count("SELECT count(*) FROM audit_log WHERE action = 'admin.view' AND actor_id = $1", ada_id).await, 1);
    app.finish().await;
}
