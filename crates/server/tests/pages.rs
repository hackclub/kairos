mod common;

use std::fs;
use std::path::PathBuf;

use axum::http::{Method, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use chrono::{Days, Utc};
use common::{upload, user_id, Browser, Chain, Resp, TestApp};
use kairos_core::event::{Class, FileChange, Kind, Via};
use kairos_server::accounts;
use kairos_server::util::now;
use serde_json::{json, Value};

// 2026-06-01T00:00:00Z. Ada lives in Asia/Kolkata, so her June 2 starts at 18:30 UTC on June 1.
const D: f64 = 1_780_272_000.0;
const DAY: f64 = 86_400.0;
const SHA_A: &str = "abc1234def5678abc1234def5678abc1234def56";
const SHA_B: &str = "bbbb2220000000000000000000000000000000aa";
const SHA_UNSEEN: &str = "eeee5550000000000000000000000000000000bb";
const CHROME: &str = "https://chromewebstore.google.com/detail/kairos/abcdef";
const FIREFOX: &str = "https://addons.mozilla.org/firefox/addon/kairos/";
const TOKEN: &str = "ghp_testtoken";

/// With KAIROS_SNAPSHOT_DIR set, keeps each body as `<name>.html` so two builds can be compared page by page.
struct Shots {
    dir: Option<PathBuf>,
}

impl Shots {
    fn new() -> Shots {
        let dir = std::env::var_os("KAIROS_SNAPSHOT_DIR").map(PathBuf::from);
        if let Some(d) = &dir {
            fs::create_dir_all(d).unwrap();
        }
        Shots { dir }
    }

    fn page(&self, name: &str, status: StatusCode, r: &Resp) {
        assert_eq!(r.status, status, "{name}: {}", r.body);
        assert!(r.body.contains("<title>"), "{name} is not a page: {}", r.body);
        assert!(!r.body.contains('\u{2014}'), "{name} has an em dash");
        if let Some(d) = &self.dir {
            fs::write(d.join(format!("{name}.html")), &r.body).unwrap();
        }
    }

    fn ok(&self, name: &str, r: &Resp) {
        self.page(name, StatusCode::OK, r);
    }
}

fn local(day: f64, h: f64, m: f64) -> f64 {
    D + 18.5 * 3600.0 + day * DAY + h * 3600.0 + m * 60.0
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

fn files(paths: &[(&str, u32, u32)]) -> Vec<FileChange> {
    paths.iter().map(|&(p, a, r)| FileChange { path: p.into(), added: a, removed: r }).collect()
}

fn commit(sha: &str, via: Option<Via>, committed: Option<i64>, changes: Vec<FileChange>) -> Kind {
    let (added, removed) = changes.iter().fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
    Kind::Commit { sha: sha.into(), files: changes.len() as u32, added, removed, authored: None, committed, via, changes, replaces: None, not_before: None }
}

fn edit(c: &mut Chain, ts: f64, harness: &str, path: &str, added: u32, removed: u32) {
    c.push_from(ts, Some("ada/site"), harness, Kind::AgentEdit { session: "s1".into(), path: path.into(), added, removed });
}

fn agent_run(c: &mut Chain, start: f64, end: f64, project: &str, harness: &str) {
    c.prompt(start, Some(project), harness);
    c.push_from(start + 5.0, Some(project), harness, Kind::AgentStart { session: "s1".into() });
    c.push_from(end, Some(project), harness, Kind::ReplyDone { session: "s1".into(), reply_words: 40, tools: 3, files_changed: 1, model: None });
}

fn beat(c: &mut Chain, ts: f64, at: f64, path: &str) {
    let kind = Kind::Heartbeat {
        at,
        editor: Some("vscode".into()),
        language: Some("Rust".into()),
        category: Some("coding".into()),
        path: Some(path.into()),
        write: false,
        ai_lines: None,
        human_lines: None,
    };
    c.push(ts, Some("ada/site"), kind);
}

async fn june(app: &TestApp, ada: &mut Browser<'_>) {
    let laptop = ada.pair("ada-laptop").await;
    let mut c = Chain::default();
    c.push(local(0.0, 9.0, 0.0), None, Kind::Backends { focus: "x11".into(), idle: "xprintidle".into() });
    agent_run(&mut c, local(0.0, 9.0, 30.0), local(0.0, 9.0, 50.0), "ada/site", "claude-code");
    c.editing("ada/site", local(0.0, 10.0, 0.0), local(0.0, 11.0, 0.0));
    edit(&mut c, local(0.0, 10.0, 30.0), "claude-code", "src/main.rs", 12, 3);
    edit(&mut c, local(0.0, 10.0, 40.0), "codex", "src/lib.rs", 4, 0);
    let hour = 3600;
    let at = local(0.0, 11.0, 5.0);
    c.push(at, Some("ada/site"), commit(SHA_A, Some(Via::Commit), Some(at as i64), files(&[("src/main.rs", 30, 1), ("Cargo.lock", 10, 1)])));
    c.push(at + 60.0, Some("ada/site"), commit(SHA_B, Some(Via::Commit), Some(at as i64 - 2 * hour), files(&[("src/lib.rs", 10, 1)])));
    c.push(at + 120.0, Some("ada/site"), commit(&"c".repeat(40), Some(Via::Amend), Some(at as i64 + 120), files(&[("src/main.rs", 5, 5)])));
    c.push(at + 180.0, Some("ada/site"), commit(&"d".repeat(40), Some(Via::Arrived), Some(at as i64 - 5 * 86_400), files(&[])));
    c.push(at + 240.0, Some("ada/site"), commit(&"e".repeat(40), Some(Via::CherryPick), Some(at as i64 + 240), files(&[("a.rs", 2, 0)])));
    c.push(at + 300.0, Some("ada/site"), commit(&"f".repeat(40), Some(Via::Revert), Some(at as i64 + 300), files(&[("b.rs", 0, 2)])));
    c.push(at + 360.0, Some("ada/site"), commit(&"1".repeat(40), Some(Via::Rebase), Some(at as i64 + 360 - 2 * hour), files(&[])));
    let many: Vec<(String, u32, u32)> = (0..55).map(|i| (format!("src/mod{i}.rs"), 3, 1)).collect();
    let many: Vec<(&str, u32, u32)> = many.iter().map(|(p, a, r)| (p.as_str(), *a, *r)).collect();
    let mut big = commit(&"2".repeat(40), Some(Via::Commit), None, files(&many));
    if let Kind::Commit { files, .. } = &mut big {
        *files = 75;
    }
    c.push(at + 420.0, Some("ada/site"), big);
    let wide: Vec<(String, u32, u32)> = (0..51).map(|i| (format!("src/part{i}.rs"), 1, 0)).collect();
    let wide: Vec<(&str, u32, u32)> = wide.iter().map(|(p, a, r)| (p.as_str(), *a, *r)).collect();
    c.push(at + 450.0, Some("ada/site"), commit(&"4".repeat(40), Some(Via::Commit), None, files(&wide)));
    c.push(at + 480.0, Some("ada/gone"), commit(&"5".repeat(40), Some(Via::Commit), None, files(&[("lib.rs", 4, 0)])));
    c.push(at + 490.0, Some("ada/site"), Kind::AgentEdit { session: "s9".into(), path: "src/z.rs".into(), added: 5, removed: 0 });
    c.push(at + 495.0, Some("ada/site"), commit(&"8".repeat(40), Some(Via::Commit), None, files(&[("src/z.rs", 5, 0)])));
    c.push_from(at + 500.0, Some("ada/site"), "claude-code", Kind::AgentCommit { session: "s1".into(), sha: None });
    c.push(at + 505.0, Some("ada/site"), commit(&"3".repeat(40), Some(Via::Commit), None, files(&[("src/main.rs", 2, 2)])));
    c.editing("ada/other", local(0.0, 14.0, 0.0), local(0.0, 14.0, 30.0));
    agent_run(&mut c, local(0.0, 14.0, 40.0), local(0.0, 14.0, 55.0), "ada/other", "codex");
    c.push(local(0.0, 16.0, 0.0), None, Kind::Focus { class: Class::Editor, app: None });
    for i in 1..=10 {
        c.push(local(0.0, 16.0, i as f64), None, Kind::Present { input: None, keys: None, injected: None });
    }
    c.editing("ada/site", local(1.0, 6.0, 0.0), local(1.0, 19.5, 0.0));
    c.push_from(local(2.0, 12.0, 0.0), Some("ada/site"), "claude-code", Kind::AgentStart { session: "s2".into() });
    c.push_from(local(2.0, 12.0, 20.0), Some("ada/site"), "claude-code", Kind::ReplyDone { session: "s2".into(), reply_words: 40, tools: 3, files_changed: 1, model: None });
    for i in 0..5 {
        beat(&mut c, local(1.0, 19.5, 1.0 + i as f64), local(1.0, 12.0, i as f64), "src/main.rs");
    }
    assert_eq!(upload(app, &laptop, c.take()).await.status, StatusCode::OK);

    let desktop = ada.pair("ada-desktop").await;
    let mut c = Chain::default();
    c.push(local(1.0, 8.0, 0.0), None, Kind::Backends { focus: "none".into(), idle: "none".into() });
    c.editing("ada/site", local(1.0, 9.0, 0.0), local(1.0, 9.0, 20.0));
    assert_eq!(upload(app, &desktop, c.take()).await.status, StatusCode::OK);
}

/// Everything is placed from midnight UTC so two runs on the same day draw the same pages.
async fn recent(app: &TestApp, ada: &mut Browser<'_>) {
    let laptop = ada.pair("ada-third").await;
    let t = now();
    let base = (t / DAY).floor() * DAY;
    let day = |ago: f64, h: f64| base - ago * DAY + h * 3600.0;
    let mut c = Chain::default();
    c.editing("ai-tracking", day(5.0, 10.0), day(5.0, 11.0));
    c.push(day(4.0, 9.0), Some("ObayM/kairos"), Kind::ProjectAlias { from: "ai-tracking".into() });
    for (ago, mins) in [(4.0, 90.0), (3.0, 45.0), (2.0, 120.0), (1.0, 60.0)] {
        c.editing("ObayM/kairos", day(ago, 10.0), day(ago, 10.0) + mins * 60.0);
        agent_run(&mut c, day(ago, 12.5), day(ago, 12.5) + 900.0, "ObayM/kairos", if ago > 2.0 { "claude-code" } else { "codex" });
        let seen = day(ago, 13.0);
        c.push(seen, Some("ObayM/kairos"), commit(&format!("{:x}{}", ago as u32 + 1, "a".repeat(39)), Some(Via::Commit), Some(seen as i64), files(&[("src/main.rs", 8, 2)])));
        beat(&mut c, day(ago, 14.0), day(ago, 10.0) + 600.0, "src/main.rs");
        beat(&mut c, day(ago, 14.0) + 1.0, day(ago, 10.0) + 660.0, "src/lib.rs");
        match ago as u32 {
            3 => c.editing("ada/other", day(ago, 16.0), day(ago, 16.25)),
            2 => {
                c.editing("ada/site", day(ago, 15.0), day(ago, 15.5));
                c.editing("ada/old", day(ago, 16.0), day(ago, 16.5));
            }
            1 => {
                c.editing("scratch", day(ago, 17.0), day(ago, 17.5));
                c.push(day(ago, 17.6), Some("scratch"), commit(&"6".repeat(40), Some(Via::Commit), Some(day(ago, 17.6) as i64), files(&[("a.txt", 3, 0), ("b.txt", 1, 1)])));
                c.push(day(ago, 17.7), Some("scratch"), commit(&"7".repeat(40), Some(Via::Commit), Some(day(ago, 17.7) as i64), files(&[("a.txt", 1, 0)])));
                c.push(day(ago, 18.0), None, Kind::Focus { class: Class::Editor, app: None });
                for i in 1..=10 {
                    c.push(day(ago, 18.0) + 60.0 * i as f64, None, Kind::Present { input: None, keys: None, injected: None });
                }
            }
            _ => {}
        }
    }
    if t - base > 1600.0 {
        c.prompt(base + 20.0, Some("ada/site"), "claude-code");
        c.prompt(base + 30.0, Some("ada/site"), "cursor");
        c.editing("ada/site", base + 60.0, base + 1500.0);
    }
    assert_eq!(upload(app, &laptop, c.take()).await.status, StatusCode::OK);
}

async fn others(app: &TestApp) {
    let c = app.st.pool.get().await.unwrap();
    let today = Utc::now().date_naive();
    for (name, display, public, trust, mins) in [
        ("bea", Some("Bea Hacker"), true, "trusted", 95.0),
        ("dan", None, true, "unscored", 40.0),
        ("cy", Some("Cy Private"), false, "unscored", 120.0),
        ("mal", Some("Mal Banned"), true, "banned", 300.0),
    ] {
        let id: i64 = c
            .query_one(
                "INSERT INTO users (hca_sub, username, display_name, is_public, trust, timezone) VALUES ($1, $1, $2, $3, $4, 'UTC') RETURNING id",
                &[&name, &display, &public, &trust],
            )
            .await
            .unwrap()
            .get(0);
        for ago in 0..4u64 {
            let d = today.checked_sub_days(Days::new(ago)).unwrap();
            for (class, secs) in [("typing", mins * 20.0), ("editing", mins * 40.0), ("agent", mins * 30.0)] {
                c.execute("INSERT INTO user_days (user_id, day, project, class, seconds) VALUES ($1, $2, 'o/p', $3, $4)", &[&id, &d, &class, &secs]).await.unwrap();
            }
        }
    }
}

async fn bulk(app: &TestApp) {
    let mut b = person(app, "Bulk", "user").await;
    app.exec("UPDATE users SET timezone = 'Asia/Kolkata' WHERE username = 'bulk'").await;
    let token = b.pair("bulk-laptop").await;
    let mut c = Chain::default();
    for i in 0..700 {
        let ts = local(0.0, 8.0, 0.0) + i as f64;
        c.push(ts, Some(&format!("bulk/r{}", i % 12)), commit(&format!("{i:040x}"), Some(Via::Commit), Some(ts as i64), files(&[("a.rs", 1, 0)])));
    }
    for i in 0..320 {
        let start = local(1.0, 1.0, 0.0) + 120.0 * i as f64;
        c.editing(if i % 2 == 0 { "bulk/r0" } else { "bulk/r1" }, start, start + 60.0);
    }
    c.editing("bulk/quiet", local(1.0, 14.0, 0.0), local(1.0, 19.5, 0.0));
    assert_eq!(upload(app, &token, c.take()).await.status, StatusCode::OK);
    for day in 2..16 {
        let day = day as f64;
        c.editing("bulk/r0", local(day, 10.0, 0.0), local(day, 10.0, 10.0));
        assert_eq!(upload(app, &token, c.take()).await.status, StatusCode::OK);
    }
}

async fn fake_github() -> String {
    fn gh_commit(oid: &str, added: i64, removed: i64, who: &str) -> Value {
        json!({ "oid": oid, "additions": added, "deletions": removed, "changedFilesIfAvailable": 1, "committedDate": "2026-06-02T08:00:00Z",
                "author": { "name": who, "user": { "login": who } } })
    }
    async fn graphql(Json(body): Json<Value>) -> Json<Value> {
        let v = &body["variables"];
        if v["name"] == "gone" {
            return Json(json!({ "data": { "repository": null }, "errors": [{ "type": "NOT_FOUND", "message": "nope" }] }));
        }
        let mut repo = serde_json::Map::new();
        if v["name"] == "r1" && !v["since"].is_null() {
            let page: usize = v["cursor"].as_str().and_then(|c| c.strip_prefix('p')).and_then(|n| n.parse().ok()).unwrap_or(0);
            let nodes: Vec<Value> = (0..100).map(|i| gh_commit(&format!("{:040x}", 0x9000 + page * 100 + i), 3, 1, "grace")).collect();
            repo.insert("defaultBranchRef".into(), json!({ "name": "main", "target": { "history": { "pageInfo": { "hasNextPage": true, "endCursor": format!("p{}", page + 1) }, "nodes": nodes } } }));
        }
        if v["name"] == "site" && !v["since"].is_null() {
            let mut anonymous = gh_commit(&"9".repeat(40), 1, 1, "x");
            anonymous["author"] = Value::Null;
            let nodes = json!([gh_commit(SHA_A, 40, 2, "ada"), gh_commit(SHA_UNSEEN, 7, 3, "grace"), anonymous]);
            repo.insert("defaultBranchRef".into(), json!({ "name": "main", "target": { "history": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": nodes } } }));
        }
        for i in 0.. {
            let Some(sha) = v.get(format!("s{i}")).and_then(Value::as_str) else { break };
            let found = match sha {
                SHA_A => gh_commit(SHA_A, 40, 2, "ada"),
                SHA_B => gh_commit(SHA_B, 12, 1, "ada"),
                _ => Value::Null,
            };
            repo.insert(format!("c{i}"), found);
        }
        Json(json!({ "data": { "repository": repo } }))
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}/graphql", listener.local_addr().unwrap().port());
    let app = Router::new().route("/graphql", post(graphql));
    tokio::spawn(async move { axum::serve(listener, app).await });
    url
}

#[tokio::test]
async fn an_empty_server_renders_its_pages() {
    let app = TestApp::new().await;
    let shots = Shots::new();
    let mut visitor = app.browser();

    shots.ok("landing", &visitor.get("/").await);
    shots.ok("leaderboards-empty", &visitor.get("/leaderboards").await);
    shots.ok("extension-soon", &visitor.get("/extension").await);
    shots.page("not-found-signed-out", StatusCode::NOT_FOUND, &visitor.get("/@nobody").await);
    let login = visitor.get("/auth/login?next=/me").await;
    shots.ok("mock-authorize", &visitor.get(&login.location()).await);

    let mut newbie = app.browser();
    newbie.sign_in("Newbie", "newbie@example.com").await;
    shots.ok("me-new", &newbie.get("/me").await);
    shots.ok("settings-new", &newbie.get("/settings").await);
    app.exec("UPDATE users SET email = NULL, slack_id = NULL, is_public = false, show_ai_tools = false WHERE username = 'newbie'").await;
    app.exec("INSERT INTO projects (user_id, id, first_seen, last_seen) SELECT id, 'solo', 1, 2 FROM users WHERE username = 'newbie'").await;
    shots.ok("settings-one-project", &newbie.get("/settings").await);
    shots.ok("profile-new", &newbie.get("/@newbie").await);
    shots.ok("leaderboards-empty-signed-in", &newbie.get("/leaderboards").await);
    shots.ok("device-expired", &newbie.get("/device?code=NOPE").await);
    shots.page("not-found-signed-in", StatusCode::NOT_FOUND, &newbie.get("/@nobody").await);
    let csrf = newbie.csrf().await;
    shots.page("forbidden", StatusCode::FORBIDDEN, &newbie.post("/settings/key", &[("csrf", "wrong")]).await);
    shots.page("settings-error", StatusCode::BAD_REQUEST, &newbie.post("/settings/profile", &[("csrf", &csrf), ("username", "x"), ("timezone", "UTC")]).await);

    let mut rev = person(&app, "Rev", "reviewer").await;
    shots.ok("review-home-empty", &rev.get("/review").await);
    shots.ok("review-home-no-match", &rev.get("/review?q=zzz").await);
    shots.ok("review-new-user", &rev.get("/review/@newbie").await);
    shots.ok("github-unconfigured", &rev.get("/review/@newbie/github").await);
    shots.page("bad-request", StatusCode::BAD_REQUEST, &rev.get("/review/@newbie?from=yesterday").await);

    let mut boss = person(&app, "Boss", "admin").await;
    shots.ok("admin-empty", &boss.get("/admin").await);
    app.finish().await;
}

#[tokio::test]
async fn the_extension_page_with_one_store() {
    let shots = Shots::new();
    let app = TestApp::with(&[("KAIROS_FIREFOX_EXTENSION_URL", FIREFOX)]).await;
    shots.ok("extension-firefox-only", &app.browser().get("/extension").await);
    app.finish().await;
    let app = TestApp::with(&[("KAIROS_CHROME_EXTENSION_URL", CHROME)]).await;
    shots.ok("extension-chrome-only", &app.browser().get("/extension").await);
    app.finish().await;
}

#[tokio::test]
async fn a_live_server_has_no_test_banner() {
    let app = TestApp::with(&[("KAIROS_AUTH", "hca"), ("HCA_CLIENT_ID", "kairos-live")]).await;
    let shots = Shots::new();
    shots.ok("landing-live", &app.browser().get("/").await);
    shots.ok("leaderboards-live", &app.browser().get("/leaderboards").await);
    app.finish().await;
}

#[tokio::test]
async fn a_busy_server_renders_its_pages() {
    let github = fake_github().await;
    let app = TestApp::with(&[
        ("KAIROS_CHROME_EXTENSION_URL", CHROME),
        ("KAIROS_FIREFOX_EXTENSION_URL", FIREFOX),
        ("KAIROS_GITHUB_API", &github),
        ("KAIROS_GITHUB_TOKEN", TOKEN),
    ])
    .await;
    let shots = Shots::new();
    let mut ada = app.browser();
    ada.sign_in("Ada Lovelace", "ada@example.com").await;
    app.exec("UPDATE users SET timezone = 'Asia/Kolkata' WHERE username = 'ada'").await;
    june(&app, &mut ada).await;
    recent(&app, &mut ada).await;
    others(&app).await;
    bulk(&app).await;
    app.exec("UPDATE projects SET hidden = true WHERE id = 'ada/other'").await;
    app.exec("UPDATE devices SET last_seen_at = now() - interval '3 days' WHERE name = 'ada-desktop'").await;
    ada.pair("ada-old").await;
    app.exec("UPDATE devices SET client_version = NULL WHERE name = 'ada-old'").await;
    let uid = user_id(&app, "ada").await;
    let old: i64 = app.st.pool.get().await.unwrap().query_one("SELECT id FROM devices WHERE user_id = $1 AND name = 'ada-old'", &[&uid]).await.unwrap().get(0);
    let mut visitor = app.browser();

    shots.ok("me", &ada.get("/me").await);
    let one_day = ada.get("/me?from=2026-06-02&to=2026-06-02").await;
    shots.ok("me-one-day", &one_day);
    assert!(one_day.body.contains("Tue, Jun 2"), "{}", one_day.body);
    assert!(one_day.body.contains("/@ada/p/ada/site") && !one_day.body.contains("/@ada/p/ada/old"), "{}", one_day.body);
    let range = ada.get("/me?from=2026-06-03&to=2026-06-02").await;
    shots.ok("me-range", &range);
    assert!(range.body.contains("Jun 2") && range.body.contains("\u{2013} Jun 3"), "{}", range.body);
    let bad = ada.get("/me?from=nope&to=2026-06-02").await;
    shots.ok("me-bad-dates", &bad);
    assert!(bad.body.contains(r#"aria-current="true">Last 30 days"#), "{}", bad.body);
    app.exec("UPDATE users SET timezone_set = true WHERE username = 'ada'").await;
    shots.ok("me-timezone-set", &ada.get("/me").await);
    shots.ok("extension", &ada.get("/extension").await);
    shots.ok("leaderboards-today", &visitor.get("/leaderboards").await);
    shots.ok("leaderboards-week", &visitor.get("/leaderboards?period=week").await);
    shots.ok("leaderboards-signed-in", &ada.get("/leaderboards").await);

    shots.ok("profile", &visitor.get("/@ada").await);
    shots.ok("profile-own", &ada.get("/@ada").await);
    app.exec("UPDATE users SET show_ai_tools = false WHERE username = 'dan'").await;
    shots.ok("profile-plain", &visitor.get("/@dan").await);
    shots.ok("project", &visitor.get("/@ada/p/ObayM/kairos").await);
    shots.ok("project-own", &ada.get("/@ada/p/ObayM/kairos").await);
    shots.ok("project-no-repo", &ada.get("/@ada/p/scratch").await);
    shots.ok("project-hidden", &ada.get("/@ada/p/ada/other").await);
    shots.page("project-hidden-visitor", StatusCode::NOT_FOUND, &visitor.get("/@ada/p/ada/other").await);
    app.exec("UPDATE users SET show_ai_tools = false WHERE username = 'ada'").await;
    shots.ok("profile-no-tools", &visitor.get("/@ada").await);
    shots.ok("project-no-tools", &visitor.get("/@ada/p/ObayM/kairos").await);
    app.exec("UPDATE users SET is_public = false WHERE username = 'ada'").await;
    shots.ok("project-own-private", &ada.get("/@ada/p/ObayM/kairos").await);
    shots.page("profile-private", StatusCode::NOT_FOUND, &visitor.get("/@ada").await);
    shots.ok("extension-private-profile", &ada.get("/extension").await);
    app.exec("UPDATE users SET is_public = true, show_ai_tools = true WHERE username = 'ada'").await;

    let started = app.api(Method::POST, "/api/v1/device/start", Some(json!({ "name": "ada-new", "os": "macos" })), None).await.json();
    let code = started["user_code"].as_str().unwrap().to_string();
    let pending = ada.get(&format!("/device?code={code}")).await;
    shots.ok("device-pending", &pending);
    shots.ok("device-approved", &ada.post("/device/approve", &[("code", &code), ("csrf", &pending.csrf())]).await);

    let csrf = ada.csrf().await;
    shots.ok("settings-saved", &ada.get("/settings?done=profile").await);
    shots.ok("settings-merged", &ada.post("/settings/projects/merge", &[("csrf", &csrf), ("from", "ada/old"), ("into", "ada/site")]).await);
    shots.page("settings-merge-refused", StatusCode::BAD_REQUEST, &ada.post("/settings/projects/merge", &[("csrf", &csrf), ("from", "ada/site"), ("into", "ada/site")]).await);
    shots.ok("settings-new-key", &ada.post("/settings/key", &[("csrf", &csrf)]).await);
    assert_eq!(ada.post("/settings/devices/revoke", &[("csrf", &csrf), ("device_id", &old.to_string())]).await.status, StatusCode::SEE_OTHER);
    shots.ok("settings", &ada.get("/settings").await);

    let mut rev = person(&app, "Rev", "reviewer").await;
    let window = "from=2026-06-02&to=2026-06-04";
    shots.ok("review-home", &rev.get("/review").await);
    shots.ok("review-home-search", &rev.get("/review?q=b").await);
    shots.ok("review-home-search-dan", &rev.get("/review?q=dan").await);
    shots.ok("review", &rev.get(&format!("/review/@ada?{window}")).await);
    shots.ok("review-filtered", &rev.get(&format!("/review/@ada?{window}&projects=ada/site")).await);
    shots.ok("review-program-window", &rev.get("/review/@ada?start_date=2026-06-02&end_date=2026-06-04").await);
    shots.ok("review-too-long", &rev.get("/review/@ada?from=2025-01-01&to=2026-06-04").await);
    shots.ok("review-recent", &rev.get("/review/@ada").await);
    shots.ok("review-other-user", &rev.get("/review/@dan").await);
    shots.ok("review-self", &rev.get("/review/@rev").await);
    shots.ok("review-banned", &rev.get("/review/@mal").await);
    shots.ok("review-private", &rev.get("/review/@cy").await);
    shots.ok("review-bulk", &rev.get("/review/@bulk?from=2026-06-02&to=2026-06-15").await);
    shots.ok("github", &rev.get(&format!("/review/@ada/github?{window}")).await);
    shots.ok("github-filtered", &rev.get(&format!("/review/@ada/github?{window}&projects=ada/gone")).await);
    shots.ok("github-no-repos", &rev.get("/review/@dan/github").await);
    shots.ok("github-bulk", &rev.get("/review/@bulk/github?from=2026-06-02&to=2026-06-02").await);
    let rev_csrf = rev.csrf().await;
    let back = "from=2026-06-02&to=2026-06-04";
    rev.post("/review/@ada/trust", &[("csrf", &rev_csrf), ("trust", "trusted"), ("reason", "checked the spans"), ("back", back)]).await;
    rev.post("/review/@ada/notes", &[("csrf", &rev_csrf), ("body", "Looked at the long day.\nLooks fine."), ("back", back)]).await;
    shots.ok("review-with-notes", &rev.get(&format!("/review/@ada?{window}")).await);

    let mut boss = person(&app, "Boss", "admin").await;
    let boss_csrf = boss.csrf().await;
    boss.post("/admin/programs", &[("csrf", &boss_csrf), ("slug", "stardance"), ("name", "Stardance")]).await;
    let key = boss.post("/admin/programs/key", &[("csrf", &boss_csrf), ("slug", "stardance"), ("name", "the server")]).await;
    shots.ok("admin-new-key", &key);
    let second = boss.post("/admin/programs/key", &[("csrf", &boss_csrf), ("slug", "stardance"), ("name", "")]).await;
    let prefix = second.body.split("listed in Admin as <code>").nth(1).and_then(|s| s.split('<').next()).expect("the key page names its prefix").to_string();
    boss.post("/admin/programs/revoke", &[("csrf", &boss_csrf), ("prefix", &prefix)]).await;
    boss.post("/admin/programs", &[("csrf", &boss_csrf), ("slug", "high-seas"), ("name", "High Seas")]).await;
    shots.ok("admin", &boss.get("/admin").await);
    shots.ok("admin-search", &boss.get("/admin?q=b").await);
    shots.ok("admin-search-dan", &boss.get("/admin?q=dan").await);
    shots.ok("admin-no-match", &boss.get("/admin?q=zzz").await);
    app.finish().await;
}
