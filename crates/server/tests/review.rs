mod common;

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use common::{upload, user_id, Browser, Chain, Resp, TestApp};
use kairos_core::event::{Class, Kind};
use kairos_server::{accounts, programs};
use serde_json::{json, Value};

// 2026-06-01T00:00:00Z. Ada lives in Asia/Kolkata, so her June 2 starts at 18:30 UTC on June 1.
const D: f64 = 1_780_272_000.0;
const SHA: &str = "abc1234def5678abc1234def5678abc1234def56";

fn local(day: f64, h: f64, m: f64) -> f64 {
    D + 18.5 * 3600.0 + day * 86_400.0 + h * 3600.0 + m * 60.0
}

async fn person<'a>(app: &'a TestApp, name: &str, role: &str) -> Browser<'a> {
    let mut b = app.browser();
    let username = name.to_lowercase();
    b.sign_in(name, &format!("{username}@example.com")).await;
    if role != "user" {
        accounts::grant(&app.st.pool, None, &username, role, "test").await.unwrap();
    }
    b
}

async fn seed(app: &TestApp) -> Browser<'_> {
    let mut ada = person(app, "Ada", "user").await;
    app.exec("UPDATE users SET timezone = 'Asia/Kolkata' WHERE username = 'ada'").await;

    let laptop = ada.pair("ada-laptop").await;
    let mut c = Chain::default();
    c.push(local(0.0, 9.0, 0.0), None, Kind::Backends { focus: "x11".into(), idle: "xprintidle".into() });
    c.editing("ada/site", local(0.0, 10.0, 0.0), local(0.0, 11.0, 0.0));
    c.push(local(0.0, 11.0, 5.0), Some("ada/site"), Kind::Commit { sha: SHA.into(), files: 3, added: 40, removed: 2, authored: None, committed: None, via: None, changes: vec![], replaces: None, not_before: None });
    c.editing("ada/other", local(0.0, 14.0, 0.0), local(0.0, 14.0, 30.0));
    c.push(local(0.0, 16.0, 0.0), None, Kind::Focus { class: Class::Editor, app: None });
    for i in 1..=10 {
        c.push(local(0.0, 16.0, i as f64), None, Kind::Present { input: None, keys: None, injected: None });
    }
    c.editing("ada/site", local(1.0, 23.0, 30.0), local(2.0, 0.0, 30.0));
    assert_eq!(upload(app, &laptop, c.take()).await.status, StatusCode::OK);

    let desktop = ada.pair("ada-desktop").await;
    let mut c = Chain::default();
    c.push(local(1.0, 8.0, 0.0), None, Kind::Backends { focus: "none".into(), idle: "none".into() });
    c.editing("ada/site", local(1.0, 9.0, 0.0), local(1.0, 9.0, 20.0));
    assert_eq!(upload(app, &desktop, c.take()).await.status, StatusCode::OK);

    app.exec("UPDATE projects SET hidden = true WHERE id = 'ada/other'").await;
    ada
}

async fn program_key(app: &TestApp) -> String {
    programs::create(&app.st.pool, None, "stardance", "Stardance", "test").await.unwrap();
    programs::new_key(&app.st.pool, None, "stardance", None, "test").await.unwrap().0
}

fn ok(r: &Resp) -> &str {
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    &r.body
}

fn official(r: &Resp) -> i64 {
    let before = ok(r).split(" seconds</b>").next().unwrap();
    before.rsplit("<b>").next().unwrap().parse().unwrap()
}

fn api_path(r: &Resp) -> String {
    ok(r).split("<div class=\"cmd\"><code>").nth(1).unwrap().split("</code>").next().unwrap().replace("&#38;", "&")
}

async fn total_seconds(app: &TestApp, path: &str, key: &str) -> Value {
    let r = app.api(Method::GET, path, None, Some(key)).await;
    assert_eq!(r.status, StatusCode::OK, "{path}: {}", r.body);
    r.json()["data"]["total_seconds"].clone()
}

async fn audit_rows(app: &TestApp, action: &str) -> Vec<(Option<i64>, Option<i64>, Value)> {
    let c = app.st.pool.get().await.unwrap();
    c.query("SELECT actor_id, target_user_id, detail FROM audit_log WHERE action = $1 ORDER BY id", &[&action])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect()
}

fn trust_form<'a>(csrf: &'a str, trust: &'a str, reason: &'a str) -> [(&'a str, &'a str); 4] {
    [("csrf", csrf), ("trust", trust), ("reason", reason), ("back", "from=2026-06-02&to=2026-06-04")]
}

async fn create_program(b: &mut Browser<'_>, csrf: &str, slug: &str) -> Resp {
    b.post("/admin/programs", &[("csrf", csrf), ("slug", slug), ("name", "Stardance")]).await
}

async fn trust_of(app: &TestApp, username: &str) -> String {
    let c = app.st.pool.get().await.unwrap();
    c.query_one("SELECT trust FROM users WHERE username = $1", &[&username]).await.unwrap().get(0)
}

#[tokio::test]
async fn review_pages_are_for_staff_only() {
    let app = TestApp::new().await;
    let mut ada = person(&app, "Ada", "user").await;
    let mut rev = person(&app, "Rev", "reviewer").await;
    let mut boss = person(&app, "Boss", "admin").await;
    let csrf = ada.csrf().await;

    for path in ["/review", "/review/@rev", "/review/@ada", "/review/@ada/github"] {
        assert_eq!(ada.get(path).await.status, StatusCode::NOT_FOUND, "{path}");
        assert!(!app.browser().get(path).await.status.is_success(), "{path}");
    }
    assert_eq!(ada.post("/review/@rev/trust", &[("csrf", &csrf), ("trust", "trusted"), ("reason", "x")]).await.status, StatusCode::NOT_FOUND);
    assert_eq!(ada.post("/review/@rev/notes", &[("csrf", &csrf), ("body", "x")]).await.status, StatusCode::NOT_FOUND);
    assert!(!ok(&ada.get("/me").await).contains("href=\"/review\""));

    for b in [&mut rev, &mut boss] {
        ok(&b.get("/review").await);
        let page = b.get("/review/@ada").await;
        assert!(ok(&page).contains("@ada"));
        for tag in page.body.split("<script").skip(1) {
            assert!(tag.split('>').next().unwrap().contains("nonce="), "script without a nonce");
        }
        assert!(!page.body.contains("<style"));
        assert!(ok(&b.get("/me").await).contains("href=\"/review\""));
        assert_eq!(b.get("/review/@nobody").await.status, StatusCode::NOT_FOUND);
        assert_eq!(b.get("/review/@ada?from=yesterday").await.status, StatusCode::BAD_REQUEST);
        let github = b.get("/review/@ada/github").await;
        assert!(ok(&github).contains("<code>KAIROS_GITHUB_TOKEN</code>"), "{}", github.body);
        assert_eq!(b.get("/review/@nobody/github").await.status, StatusCode::NOT_FOUND);
        assert_eq!(b.get("/review/@ada/github?from=yesterday").await.status, StatusCode::BAD_REQUEST);
    }
    assert!(!ok(&rev.get("/me").await).contains("href=\"/admin\""));
    app.finish().await;
}

#[tokio::test]
async fn the_official_number_is_what_the_read_api_returns() {
    let app = TestApp::new().await;
    seed(&app).await;
    let key = program_key(&app).await;
    let mut rev = person(&app, "Rev", "reviewer").await;

    let mut seen = Vec::new();
    for q in [
        "from=2026-06-02&to=2026-06-04",
        "from=2026-06-02&to=2026-06-04&projects=ada/site",
        "from=2026-06-03&to=2026-06-03&projects=ada/site,ada/other",
        "from=2026-06-02&to=2026-06-02&projects=ada/other",
        "start_date=2026-06-01&end_date=2026-06-04&filter_by_project=ada/site",
        "start_date=2026-06-02T05:00:00Z&end_date=2026-06-03",
    ] {
        let page = rev.get(&format!("/review/@ada?{q}")).await;
        let secs = official(&page);
        let path = api_path(&page);
        assert_eq!(total_seconds(&app, &path, &key).await, secs, "{q} via {path}");
        seen.push(secs);
    }
    let raw = total_seconds(&app, "/api/v1/users/ada/stats?start_date=2026-06-01&end_date=2026-06-04&filter_by_project=ada/site", &key).await;
    assert_eq!(raw, seen[4], "a program's own parameters give the program's number");

    assert!(seen[0] > seen[1] && seen[1] > seen[3], "{seen:?}");
    assert!(seen.iter().all(|s| *s > 0), "the hidden project counts for staff as it does for programs: {seen:?}");
    app.finish().await;
}

#[tokio::test]
async fn the_timeline_has_a_row_per_day_with_spans_commits_and_signals() {
    let app = TestApp::new().await;
    seed(&app).await;
    let mut rev = person(&app, "Rev", "reviewer").await;
    let rev_id = user_id(&app, "rev").await;
    let ada_id = user_id(&app, "ada").await;

    let page = rev.get("/review/@ada?from=2026-06-02&to=2026-06-04").await;
    let body = ok(&page);
    let at = |s: &str| body.find(s).unwrap_or_else(|| panic!("{s} missing"));
    assert!(at("id=\"d-2026-06-04\"") < at("id=\"d-2026-06-03\"") && at("id=\"d-2026-06-03\"") < at("id=\"d-2026-06-02\""), "newest first");
    for text in [
        "<details id=\"d-2026-06-02\">",
        "Wed Jun 3",
        "ada/other",
        "(not counted)",
        "<td>23:30:00</td>",
        "<td>24:00:00</td>",
        "<td>00:00:00</td>",
        "title=\"abc1234 in ada",
        "<code>abc1234</code>",
        "+40",
        "ada-laptop",
        "ada-desktop",
        "xprintidle",
        "<span class=\"flag\"",
        "id=\"sig-blind_device\"",
        "Time from devices that can",
        "id=\"sig-late_upload\"",
        "href=\"#d-2026-06-03\"",
        "Kind of work",
        "Asia/Kolkata",
    ] {
        assert!(body.contains(text), "{text} missing");
    }
    assert!(!body.contains("id=\"sig-overlap\"") && !body.contains("id=\"sig-long_day\""));

    let views = audit_rows(&app, "review.view").await;
    let (actor, target, detail) = views.last().unwrap();
    assert_eq!((*actor, *target), (Some(rev_id), Some(ada_id)));
    assert_eq!((detail["page"].as_str(), detail["from"].as_str(), detail["to"].as_str()), (Some("user"), Some("2026-06-02"), Some("2026-06-04")));

    let filtered = rev.get("/review/@ada?from=2026-06-02&to=2026-06-04&projects=ada/other").await;
    assert!(ok(&filtered).contains("outside the filter, not counted"));
    assert_eq!(audit_rows(&app, "review.view").await.last().unwrap().2["projects"], serde_json::json!(["ada/other"]));
    app.finish().await;
}

#[tokio::test]
async fn public_pages_never_mention_review_or_signals() {
    let app = TestApp::new().await;
    let mut ada = seed(&app).await;
    let mut visitor = app.browser();
    for path in ["/", "/me", "/@ada", "/@ada/p/ada/site", "/leaderboards", "/extension", "/settings"] {
        for (who, b) in [("ada", &mut ada), ("visitor", &mut visitor)] {
            let r = b.get(path).await;
            if r.status != StatusCode::OK {
                continue;
            }
            let lower = r.body.to_lowercase();
            for word in ["signal", "/review", "suspicious", "reviewer"] {
                assert!(!lower.contains(word), "{path} for {who} mentions {word}");
            }
        }
    }
    app.finish().await;
}

#[tokio::test]
async fn trust_follows_roles_and_is_audited() {
    let app = TestApp::new().await;
    let mut ada = person(&app, "Ada", "user").await;
    let mut rev = person(&app, "Rev", "reviewer").await;
    let mut boss = person(&app, "Boss", "admin").await;
    let (rev_id, boss_id, ada_id) = (user_id(&app, "rev").await, user_id(&app, "boss").await, user_id(&app, "ada").await);

    let page = rev.get("/review/@ada").await;
    assert!(ok(&page).contains("<option>suspicious</option>") && !page.body.contains("<option>banned</option>"));
    let csrf = page.csrf();

    let r = rev.post("/review/@ada/trust", &trust_form(&csrf, "suspicious", "Two devices at once")).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(r.location(), "/review/@ada?from=2026-06-02&to=2026-06-04#trust");
    assert_eq!(trust_of(&app, "ada").await, "suspicious");

    for (token, level, reason, status) in [
        (csrf.as_str(), "trusted", "  ", StatusCode::BAD_REQUEST),
        ("00", "trusted", "fine", StatusCode::FORBIDDEN),
        (csrf.as_str(), "banned", "no", StatusCode::FORBIDDEN),
        (csrf.as_str(), "royal", "no", StatusCode::BAD_REQUEST),
    ] {
        let r = rev.post("/review/@ada/trust", &trust_form(token, level, reason)).await;
        assert_eq!(r.status, status, "{level} {reason:?}: {}", r.body);
    }
    assert_eq!(trust_of(&app, "ada").await, "suspicious");

    let boss_csrf = boss.get("/review/@ada").await.csrf();
    let r = boss.post("/review/@ada/trust", &trust_form(&boss_csrf, "banned", "Confirmed with the team")).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let locked = rev.get("/review/@ada").await;
    assert!(ok(&locked).contains("Only admins can lift a ban.") && !locked.body.contains("name=\"reason\""));
    assert_eq!(rev.post("/review/@ada/trust", &trust_form(&csrf, "trusted", "Looks fine")).await.status, StatusCode::FORBIDDEN);
    assert_eq!(boss.post("/review/@ada/trust", &trust_form(&boss_csrf, "trusted", "Appeal accepted")).await.status, StatusCode::SEE_OTHER);
    assert_eq!(trust_of(&app, "ada").await, "trusted");

    let own = rev.get("/review/@rev").await;
    assert!(ok(&own).contains("change your own trust") && !own.body.contains("name=\"reason\""));
    assert_eq!(rev.post("/review/@rev/trust", &trust_form(&csrf, "trusted", "me")).await.status, StatusCode::BAD_REQUEST);
    let ada_csrf = ada.csrf().await;
    assert_eq!(ada.post("/review/@ada/trust", &trust_form(&ada_csrf, "trusted", "me")).await.status, StatusCode::NOT_FOUND);

    let rows = audit_rows(&app, "trust.set").await;
    let got: Vec<(Option<i64>, Option<i64>, &str, &str, &str)> = rows
        .iter()
        .map(|(a, t, d)| (*a, *t, d["from"].as_str().unwrap(), d["to"].as_str().unwrap(), d["reason"].as_str().unwrap()))
        .collect();
    assert_eq!(
        got,
        [
            (Some(rev_id), Some(ada_id), "unscored", "suspicious", "Two devices at once"),
            (Some(boss_id), Some(ada_id), "suspicious", "banned", "Confirmed with the team"),
            (Some(boss_id), Some(ada_id), "banned", "trusted", "Appeal accepted"),
        ]
    );
    let history = rev.get("/review/@ada").await;
    let h = ok(&history);
    assert!(h.find("banned to trusted").unwrap() < h.find("unscored to suspicious").unwrap(), "newest first");
    app.finish().await;
}

#[tokio::test]
async fn notes_are_added_by_staff_and_listed_newest_first() {
    let app = TestApp::new().await;
    let mut ada = person(&app, "Ada", "user").await;
    let mut rev = person(&app, "Rev", "reviewer").await;
    let mut boss = person(&app, "Boss", "admin").await;
    let csrf = rev.get("/review/@ada").await.csrf();

    for body in ["first <b>note</b>", "second\r\nline two"] {
        let r = rev.post("/review/@ada/notes", &[("csrf", &csrf), ("body", body), ("back", "from=2026-06-02&to=2026-06-04")]).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
        assert_eq!(r.location(), "/review/@ada?from=2026-06-02&to=2026-06-04#notes");
    }
    let long = "x".repeat(2001);
    assert_eq!(rev.post("/review/@ada/notes", &[("csrf", &csrf), ("body", "  \n ")]).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(rev.post("/review/@ada/notes", &[("csrf", &csrf), ("body", &long)]).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(rev.post("/review/@ada/notes", &[("csrf", "00"), ("body", "forged")]).await.status, StatusCode::FORBIDDEN);
    let ada_csrf = ada.csrf().await;
    assert_eq!(ada.post("/review/@ada/notes", &[("csrf", &ada_csrf), ("body", "mine")]).await.status, StatusCode::NOT_FOUND);
    let boss_csrf = boss.get("/review/@ada").await.csrf();
    assert_eq!(boss.post("/review/@ada/notes", &[("csrf", &boss_csrf), ("body", "from an admin")]).await.status, StatusCode::SEE_OTHER);

    let page = rev.get("/review/@ada").await;
    let body = ok(&page);
    let at = |s: &str| body.find(s).unwrap_or_else(|| panic!("{s} missing"));
    assert!(at("from an admin") < at("second\nline two") && at("second\nline two") < at("first &#60;b&#62;note&#60;/b&#62;"));
    assert!(!body.contains("<b>note</b>") && !body.contains("forged") && !body.contains(">mine<"));
    assert!(body.contains("boss ·") && body.contains("rev ·"));

    let ada_id = user_id(&app, "ada").await;
    assert_eq!(app.count("SELECT count(*) FROM review_notes WHERE user_id = $1", ada_id).await, 3);
    let logged = audit_rows(&app, "review.note").await;
    assert_eq!(logged.len(), 3);
    assert!(logged.iter().all(|(_, t, d)| *t == Some(ada_id) && d["chars"].as_i64().unwrap() > 0 && d.get("body").is_none()));
    app.finish().await;
}

#[tokio::test]
async fn admins_manage_program_keys_in_the_browser() {
    let app = TestApp::new().await;
    person(&app, "Ada", "user").await;
    let mut rev = person(&app, "Rev", "reviewer").await;
    let mut boss = person(&app, "Boss", "admin").await;
    let boss_id = user_id(&app, "boss").await;
    let csrf = boss.get("/admin").await.csrf();
    let rev_csrf = rev.csrf().await;

    assert_eq!(create_program(&mut rev, &rev_csrf, "stardance").await.status, StatusCode::NOT_FOUND);
    assert_eq!(create_program(&mut boss, "00", "stardance").await.status, StatusCode::FORBIDDEN);
    let bad = create_program(&mut boss, &csrf, "Star Dance").await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert!(bad.body.contains("lowercase letters"), "{}", bad.body);
    let made = create_program(&mut boss, &csrf, "stardance").await;
    assert_eq!((made.status, made.location().as_str()), (StatusCode::SEE_OTHER, "/admin#programs"));
    assert!(create_program(&mut boss, &csrf, "stardance").await.body.contains("already exists"));

    let res = boss.post("/admin/programs/key", &[("csrf", &csrf), ("slug", "stardance"), ("name", "site")]).await;
    assert_eq!(res.headers[header::CACHE_CONTROL], "no-store");
    let key = ok(&res).split("<div class=\"cmd\"><code>").nth(1).unwrap().split("</code>").next().unwrap().to_string();
    assert!(key.starts_with("kp_") && key.len() > 40, "{key}");
    assert_eq!(rev.post("/admin/programs/key", &[("csrf", &rev_csrf), ("slug", "stardance")]).await.status, StatusCode::NOT_FOUND);
    assert_eq!(boss.post("/admin/programs/key", &[("csrf", &csrf), ("slug", "nope")]).await.status, StatusCode::BAD_REQUEST);

    let lookup = "/api/v1/users/lookup_email/ada@example.com";
    assert_eq!(app.api(Method::GET, lookup, None, Some(&key)).await.status, StatusCode::OK);
    let admin = boss.get("/admin").await;
    let prefix = &key[..11];
    assert!(ok(&admin).contains("Stardance") && admin.body.contains(prefix) && !admin.body.contains(&key));

    assert_eq!(rev.post("/admin/programs/revoke", &[("csrf", &rev_csrf), ("prefix", prefix)]).await.status, StatusCode::NOT_FOUND);
    let gone = boss.post("/admin/programs/revoke", &[("csrf", &csrf), ("prefix", prefix)]).await;
    assert_eq!((gone.status, gone.location().as_str()), (StatusCode::SEE_OTHER, "/admin#programs"));
    assert_eq!(app.api(Method::GET, lookup, None, Some(&key)).await.status, StatusCode::UNAUTHORIZED);
    assert!(ok(&boss.get("/admin").await).contains("revoked"));

    for action in ["program.create", "program.key_create", "program.key_revoke"] {
        let rows = audit_rows(&app, action).await;
        assert_eq!(rows.len(), 1, "{action}");
        assert_eq!((rows[0].0, rows[0].2["via"].as_str()), (Some(boss_id), Some("admin")), "{action}");
    }
    app.finish().await;
}

#[tokio::test]
async fn the_review_home_lists_the_weeks_busiest() {
    let app = TestApp::new().await;
    person(&app, "Ada", "user").await;
    person(&app, "Bea", "user").await;
    person(&app, "Cy", "user").await;
    let mut rev = person(&app, "Rev", "reviewer").await;
    let (ada_id, bea_id) = (user_id(&app, "ada").await, user_id(&app, "bea").await);
    app.exec(&format!(
        "INSERT INTO user_days (user_id, day, project, class, seconds) VALUES
           ({ada_id}, now()::date - 2, 'ada/site', 'editing', 3600), ({ada_id}, now()::date - 1, 'ada/site', 'agent', 1200),
           ({bea_id}, now()::date - 1, 'bea/app', 'typing', 1800), ({bea_id}, now()::date - 30, 'bea/app', 'typing', 99999);
         UPDATE users SET is_public = false, trust = 'banned' WHERE id = {ada_id};"
    ))
    .await;

    let page = rev.get("/review").await;
    let body = ok(&page);
    let at = |s: &str| body.find(s).unwrap_or_else(|| panic!("{s} missing"));
    assert!(at("href=\"/review/@ada?from=") < at("href=\"/review/@bea?from="), "most focus first");
    for text in ["1h", "20m", "30m", "(private)", "class=\"trust banned\""] {
        assert!(body.contains(text), "{text} missing");
    }
    assert!(!body.contains("/review/@cy") && !body.contains("27h"), "only the last week, only people with focus time");

    let found = rev.get("/review?q=be").await;
    assert!(ok(&found).contains("href=\"/review/@bea\"") && !found.body.contains("href=\"/review/@ada\""));
    let views = audit_rows(&app, "review.view").await;
    assert_eq!(views.last().unwrap().2["q"], "be");
    assert_eq!(views.last().unwrap().2["page"], "home");
    app.finish().await;
}

fn beat(at: f64, path: Option<&str>) -> Kind {
    Kind::Heartbeat {
        at,
        editor: Some("vscode".into()),
        language: Some("Rust".into()),
        category: Some("coding".into()),
        path: path.map(String::from),
        write: false,
        ai_lines: None,
        human_lines: None,
    }
}

#[tokio::test]
async fn editor_heartbeats_from_a_device_show_as_editor_time_and_credit_nothing() {
    let app = TestApp::new().await;
    let mut ada = person(&app, "Ada", "user").await;
    app.exec("UPDATE users SET timezone = 'Asia/Kolkata' WHERE username = 'ada'").await;
    let laptop = ada.pair("ada-laptop").await;
    let mut c = Chain::default();
    c.editing("ada/site", local(0.0, 10.0, 0.0), local(0.0, 10.0, 30.0));
    assert_eq!(upload(&app, &laptop, c.take()).await.status, StatusCode::OK);
    let uid = user_id(&app, "ada").await;
    let credited = app.count("SELECT COALESCE(sum(seconds), 0)::int8 FROM user_days WHERE user_id = $1", uid).await;

    // Five heartbeats a minute apart, one the plugin queued while kairosd was down, and two that can't be stored.
    // Hackatime counts every gap up to two minutes: four between the five and two for the gap before them.
    let t = local(0.0, 12.0, 0.0);
    for i in 0..5 {
        c.push(t + 120.0 + 60.0 * i as f64, Some("ada/site"), beat(t + 60.0 * i as f64, Some("src/main.rs")));
    }
    c.push(t + 3600.0, None, beat(t - 7200.0, None));
    c.push(t + 3601.0, Some("ada/site"), beat(f64::MAX, Some("src/main.rs")));
    c.push(t + 3602.0, Some("ada/site"), beat(-1.0, None));
    assert_eq!(upload(&app, &laptop, c.take()).await.status, StatusCode::OK);

    assert_eq!(app.count("SELECT count(*) FROM heartbeats WHERE user_id = $1", uid).await, 6);
    assert_eq!(app.count("SELECT count(*) FROM heartbeats WHERE user_id = $1 AND project = 'ada/site' AND editor = 'vscode'", uid).await, 5);
    assert_eq!(app.count("SELECT count(*) FROM events WHERE kind = 'heartbeat' AND device_id IN (SELECT id FROM devices WHERE user_id = $1)", uid).await, 8);
    let after = app.count("SELECT COALESCE(sum(seconds), 0)::int8 FROM user_days WHERE user_id = $1", uid).await;
    assert_eq!(after, credited, "heartbeats are never credited");

    let mut rev = person(&app, "Rev", "reviewer").await;
    let page = rev.get("/review/@ada?from=2026-06-02&to=2026-06-02").await;
    assert!(page.body.contains("Editor plugins sent 6 heartbeats, 6m counted the Hackatime way."), "{}", page.body);
    app.finish().await;
}

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

const TOKEN: &str = "ghp_testtoken";
const BEHIND: &str = "bbbb2220000000000000000000000000000000aa";
const UNSEEN: &str = "eeee5550000000000000000000000000000000bb";

fn gh_commit(oid: &str, added: i64, removed: i64, who: &str) -> Value {
    json!({ "oid": oid, "additions": added, "deletions": removed, "changedFilesIfAvailable": 1, "committedDate": "2026-06-02T08:00:00Z",
            "author": { "name": who, "user": { "login": who } } })
}

async fn fake_github() -> (String, Calls) {
    async fn graphql(State(calls): State<Calls>, headers: HeaderMap, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
        let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        calls.lock().unwrap().push((auth.clone(), body.clone()));
        if auth != format!("Bearer {TOKEN}") {
            return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "Bad credentials" })));
        }
        let v = &body["variables"];
        if v["name"] == "gone" {
            return (StatusCode::OK, Json(json!({ "data": { "repository": null }, "errors": [{ "type": "NOT_FOUND", "message": "nope" }] })));
        }
        let mut repo = serde_json::Map::new();
        if v["name"] == "site" && !v["since"].is_null() {
            let nodes = json!([gh_commit(SHA, 40, 2, "ada"), gh_commit(UNSEEN, 7, 3, "grace")]);
            repo.insert("defaultBranchRef".into(), json!({ "name": "main", "target": { "history": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": nodes } } }));
        }
        for i in 0.. {
            let Some(sha) = v.get(format!("s{i}")).and_then(Value::as_str) else { break };
            let found = match sha {
                SHA => gh_commit(SHA, 40, 2, "ada"),
                "bbbb222" => gh_commit(BEHIND, 12, 1, "ada"),
                _ => Value::Null,
            };
            repo.insert(format!("c{i}"), found);
        }
        (StatusCode::OK, Json(json!({ "data": { "repository": repo } })))
    }
    let calls = Calls::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}/graphql", listener.local_addr().unwrap().port());
    let app = Router::new().route("/graphql", post(graphql)).with_state(calls.clone());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (url, calls)
}

async fn more_commits(app: &TestApp, ada: &mut Browser<'_>) {
    let token = ada.pair("ada-third").await;
    let mut c = Chain::default();
    let commit = |sha: &str, files, added, removed| Kind::Commit { sha: sha.into(), files, added, removed, authored: None, committed: None, via: None, changes: vec![], replaces: None, not_before: None };
    c.push(local(0.0, 12.0, 0.0), Some("ada/site"), commit("bbbb222", 1, 10, 1));
    c.push(local(0.0, 12.5, 0.0), Some("ada/site"), commit("cccc333", 2, 5, 5));
    c.push(local(0.0, 13.0, 0.0), Some("ada/gone"), commit("dddd444", 1, 1, 1));
    assert_eq!(upload(app, &token, c.take()).await.status, StatusCode::OK);
}

#[tokio::test]
async fn the_github_check_compares_what_devices_saw_with_the_repo() {
    let (api, calls) = fake_github().await;
    let app = TestApp::with(&[("KAIROS_GITHUB_API", &api), ("KAIROS_GITHUB_TOKEN", TOKEN)]).await;
    let mut ada = seed(&app).await;
    more_commits(&app, &mut ada).await;
    let mut rev = person(&app, "Rev", "reviewer").await;

    let query = "from=2026-06-02&to=2026-06-04";
    let review = rev.get(&format!("/review/@ada?{query}")).await;
    assert!(ok(&review).contains("/review/@ada/github?from=2026-06-02&#38;to=2026-06-04\">Check these commits on GitHub"), "{}", review.body);

    let page = rev.get(&format!("/review/@ada/github?{query}")).await;
    let body = ok(&page);
    assert!(!body.contains(TOKEN) && !body.contains("KAIROS_GITHUB_TOKEN"));
    assert!(body.contains(">ada/site</a>") && body.contains(">ada/gone</a>"), "{body}");
    assert!(body.contains("1 on GitHub with matching sizes, 1 on GitHub with different sizes, 1 not on GitHub."), "{body}");
    let row = |short: &str| body.split("<li>").find(|l| l.contains(&format!("<code>{short}</code>"))).unwrap_or_else(|| panic!("no row for {short}: {body}"));
    assert!(row("abc1234").contains("on GitHub, sizes match") && row("abc1234").contains("href=\"https://github.com/ada/site/commit/abc1234def5678abc1234def5678abc1234def56\""));
    let differ = row("bbbb222");
    assert!(differ.contains("on GitHub, sizes differ") && differ.contains("+10") && differ.contains("GitHub +12 -1"), "{differ}");
    assert!(row("cccc333").contains("not on GitHub") && !row("cccc333").contains("GitHub +"));
    assert!(body.contains("Can't be read</span> not found, or private without access."), "{body}");
    assert!(body.contains("On GitHub but not seen by Kairos") && body.contains("Date git claims"));
    let unseen = row("eeee555");
    assert!(unseen.contains("grace") && unseen.contains("+7") && unseen.contains("-3") && unseen.contains("Jun 2, 2026 13:30"), "{unseen}");
    assert_eq!(body.matches("<code>abc1234</code>").count(), 1, "a commit a device saw isn't listed as unseen");

    let sent = calls.lock().unwrap();
    assert_eq!(sent.len(), 3, "one query per repo");
    for (auth, request) in sent.iter() {
        assert_eq!(auth, &format!("Bearer {TOKEN}"));
        let text = request["query"].as_str().unwrap();
        for leak in ["ada", "site", "abc1234", "bbbb222", "cccc333", "dddd444"] {
            assert!(!text.contains(leak), "{leak} is in the query text");
        }
    }
    let site = sent.iter().map(|(_, r)| r).find(|r| r["variables"]["name"] == "site").unwrap();
    assert_eq!(site["variables"]["owner"], "ada");
    assert_eq!(site["variables"]["since"], "2026-06-01T18:30:00Z");
    drop(sent);

    let audit = audit_rows(&app, "review.view").await;
    assert!(audit.iter().any(|(_, _, d)| d["page"] == "github"));
    app.finish().await;
}

#[tokio::test]
async fn a_rejected_github_token_makes_repos_unreadable_without_showing_it() {
    let (api, _) = fake_github().await;
    let app = TestApp::with(&[("KAIROS_GITHUB_API", &api), ("KAIROS_GITHUB_TOKEN", "ghp_wrongtoken")]).await;
    seed(&app).await;
    let mut rev = person(&app, "Rev", "reviewer").await;
    let page = rev.get("/review/@ada/github?from=2026-06-02&to=2026-06-04").await;
    let body = ok(&page);
    assert!(body.contains("Can't be read</span> GitHub rejected the token (401)."), "{body}");
    assert!(!body.contains("ghp_wrongtoken") && !body.contains("Bad credentials"));
    app.finish().await;
}
