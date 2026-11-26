mod common;

use std::collections::BTreeMap;

use axum::http::{Method, StatusCode};
use common::{day_seconds, upload, user_id, Browser, Chain, TestApp};
use kairos_core::event::{Kind, Via};
use kairos_server::util::now;

const SHA_A: &str = "abc1234def5678abc1234def5678abc1234def56";
const SHA_B: &str = "bbbbbbb1111111111111111111111111111111aa";

fn commit(sha: &str, via: Option<Via>) -> Kind {
    Kind::Commit { sha: sha.into(), files: 2, added: 5, removed: 1, authored: None, committed: None, via, changes: vec![], replaces: None, not_before: None }
}

fn alias(from: &str) -> Kind {
    Kind::ProjectAlias { from: from.into() }
}

async fn by_project(app: &TestApp, uid: i64) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    for (_, project, class, secs) in day_seconds(app, uid).await {
        if class != "agent" {
            *out.entry(project).or_insert(0.0) += secs;
        }
    }
    out
}

async fn ada(app: &TestApp, b: &mut Browser<'_>) -> (String, i64) {
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let token = b.pair("ada-laptop").await;
    (token, user_id(app, "ada").await)
}

async fn total(app: &TestApp, path: &str) -> i64 {
    let r = app.api(Method::GET, path, None, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    r.json()["total_seconds"].as_i64().unwrap()
}

#[tokio::test]
async fn a_device_alias_moves_the_rollups_to_the_new_id() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    let (token, uid) = ada(&app, &mut b).await;
    let t = now();
    let mut chain = Chain::default();
    chain.editing("ai-tracking", t - 7200.0, t - 6000.0);
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);
    let before = by_project(&app, uid).await;
    assert_eq!(before.keys().collect::<Vec<_>>(), ["ai-tracking"]);

    chain.push(t - 5900.0, Some("ObayM/kairos"), alias("ai-tracking"));
    chain.editing("ObayM/kairos", t - 5800.0, t - 5200.0);
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);
    let after = by_project(&app, uid).await;
    assert_eq!(after.keys().collect::<Vec<_>>(), ["ObayM/kairos"]);
    assert!(after["ObayM/kairos"] > before["ai-tracking"]);

    assert!(app.count("SELECT count(*) FROM spans WHERE user_id = $1 AND project = 'ai-tracking'", uid).await > 0, "spans keep the id they were recorded with");
    assert!(app.count("SELECT count(*) FROM events e JOIN devices d ON d.id = e.device_id WHERE d.user_id = $1 AND e.project = 'ai-tracking'", uid).await > 0);
    assert_eq!(app.count("SELECT count(*) FROM project_links WHERE user_id = $1 AND source = 'device'", uid).await, 1);
    app.finish().await;
}

#[tokio::test]
async fn a_later_rename_moves_everything_again() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    let (token, uid) = ada(&app, &mut b).await;
    let t = now();
    let mut chain = Chain::default();
    chain.editing("ai-tracking", t - 9000.0, t - 8400.0);
    chain.push(t - 8300.0, Some("ObayM/aitrack"), alias("ai-tracking"));
    chain.editing("ObayM/aitrack", t - 8200.0, t - 7600.0);
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);
    assert_eq!(by_project(&app, uid).await.keys().collect::<Vec<_>>(), ["ObayM/aitrack"]);

    chain.push(t - 7500.0, Some("ObayM/kairos"), alias("ObayM/aitrack"));
    chain.editing("ObayM/kairos", t - 7400.0, t - 6800.0);
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);
    let all = by_project(&app, uid).await;
    assert_eq!(all.keys().collect::<Vec<_>>(), ["ObayM/kairos"]);
    let aliases: Vec<(String, String)> = {
        let c = app.st.pool.get().await.unwrap();
        c.query("SELECT alias, canonical FROM project_aliases WHERE user_id = $1 ORDER BY alias", &[&uid])
            .await
            .unwrap()
            .iter()
            .map(|r| (r.get(0), r.get(1)))
            .collect()
    };
    assert_eq!(
        aliases,
        [("ObayM/aitrack".to_string(), "ObayM/kairos".to_string()), ("ai-tracking".to_string(), "ObayM/kairos".to_string())]
    );
    app.finish().await;
}

#[tokio::test]
async fn renaming_back_makes_the_old_id_canonical_again() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    let (token, uid) = ada(&app, &mut b).await;
    let t = now();
    let mut chain = Chain::default();
    chain.editing("a/one", t - 9000.0, t - 8400.0);
    chain.push(t - 8300.0, Some("a/two"), alias("a/one"));
    chain.editing("a/two", t - 8200.0, t - 7600.0);
    chain.push(t - 7500.0, Some("a/one"), alias("a/two"));
    chain.editing("a/one", t - 7400.0, t - 6800.0);
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);
    assert_eq!(by_project(&app, uid).await.keys().collect::<Vec<_>>(), ["a/one"]);
    assert_eq!(app.count("SELECT count(*) FROM project_aliases WHERE user_id = $1 AND alias = 'a/two' AND canonical = 'a/one'", uid).await, 1);
    assert_eq!(app.count("SELECT count(*) FROM project_aliases WHERE user_id = $1 AND alias = 'a/one'", uid).await, 0);
    app.finish().await;
}

#[tokio::test]
async fn the_read_api_follows_aliases_both_ways() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    let (token, uid) = ada(&app, &mut b).await;
    let t = now();
    let mut chain = Chain::default();
    chain.editing("ai-tracking", t - 7200.0, t - 6000.0);
    chain.push(t - 5900.0, Some("ObayM/kairos"), alias("ai-tracking"));
    chain.editing("ObayM/kairos", t - 5800.0, t - 5200.0);
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);

    let all = total(&app, "/api/v1/users/ada/stats?total_seconds=true").await;
    assert!(all > 0);
    assert_eq!(total(&app, "/api/v1/users/ada/stats?total_seconds=true&filter_by_project=ObayM%2Fkairos").await, all);
    assert_eq!(total(&app, "/api/v1/users/ada/stats?total_seconds=true&filter_by_project=ai-tracking").await, all);

    let list = app.api(Method::GET, "/api/v1/users/ada/projects", None, None).await.json();
    assert_eq!(list["projects"], serde_json::json!(["ObayM/kairos"]));
    let one = app.api(Method::GET, "/api/v1/users/ada/project/ai-tracking", None, None).await;
    assert_eq!(one.status, StatusCode::OK, "{}", one.body);
    assert_eq!(one.json()["name"], "ObayM/kairos");
    assert_eq!(one.json()["total_seconds"].as_i64().unwrap(), all);
    let details = app.api(Method::GET, "/api/v1/users/ada/projects/details?projects=ai-tracking,ObayM/kairos", None, None).await.json();
    assert_eq!(details["projects"].as_array().unwrap().len(), 1);
    let key = kairos_server::accounts::new_editor_key(&app.st, uid).await.unwrap();
    let spans = app.api(Method::GET, "/api/v1/users/ada/heartbeats/spans?project=ai-tracking", None, Some(&key)).await.json();
    assert!(!spans["spans"].as_array().unwrap().is_empty());
    app.finish().await;
}

#[tokio::test]
async fn a_commit_seen_under_both_ids_counts_once() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    let (token, _) = ada(&app, &mut b).await;
    let t = now();
    let mut chain = Chain::default();
    chain.editing("ai-tracking", t - 7200.0, t - 6600.0);
    chain.push(t - 6500.0, Some("ai-tracking"), commit(SHA_A, Some(Via::Commit)));
    chain.push(t - 5900.0, Some("ObayM/kairos"), alias("ai-tracking"));
    chain.push(t - 5800.0, Some("ObayM/kairos"), commit(SHA_A, Some(Via::Arrived)));
    chain.push(t - 5700.0, Some("ObayM/kairos"), commit(SHA_B, Some(Via::Commit)));
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);

    let page = app.browser().get("/@ada/p/ObayM/kairos").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("<span>Commits</span><b>2</b>"), "{}", page.body);
    assert_eq!(page.body.matches("<code>abc1234</code>").count(), 1);
    app.finish().await;
}

#[tokio::test]
async fn a_project_page_for_an_alias_redirects_to_the_canonical_one() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    let (token, _) = ada(&app, &mut b).await;
    let t = now();
    let mut chain = Chain::default();
    chain.editing("ai-tracking", t - 7200.0, t - 6600.0);
    chain.push(t - 6500.0, Some("ObayM/kairos"), alias("ai-tracking"));
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);

    let moved = app.browser().get("/@ada/p/ai-tracking").await;
    assert_eq!((moved.status, moved.location().as_str()), (StatusCode::SEE_OTHER, "/@ada/p/ObayM/kairos"));
    let page = app.browser().get("/@ada/p/ObayM/kairos").await;
    assert_eq!(page.status, StatusCode::OK);
    let me = b.get("/me").await;
    assert!(me.body.contains("href=\"/@ada/p/ObayM/kairos\"") && !me.body.contains("/@ada/p/ai-tracking"), "{}", me.body);
    app.finish().await;
}

#[tokio::test]
async fn a_hidden_alias_hides_the_canonical_project() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    let (token, _) = ada(&app, &mut b).await;
    let t = now();
    let mut chain = Chain::default();
    chain.editing("secret", t - 7200.0, t - 6600.0);
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);
    let page = b.get("/settings").await;
    let hide = b.post("/settings/projects", &[("csrf", &page.csrf()), ("project", "secret"), ("hidden", "1")]).await;
    assert_eq!(hide.status, StatusCode::SEE_OTHER);

    chain.push(t - 6500.0, Some("ObayM/public-name"), alias("secret"));
    chain.editing("ObayM/public-name", t - 6400.0, t - 5800.0);
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);

    let mut visitor = app.browser();
    assert_eq!(visitor.get("/@ada/p/ObayM/public-name").await.status, StatusCode::NOT_FOUND);
    assert_eq!(visitor.get("/@ada/p/secret").await.status, StatusCode::NOT_FOUND);
    let list = app.api(Method::GET, "/api/v1/users/ada/projects", None, None).await.json();
    assert_eq!(list["projects"], serde_json::json!([]));
    let profile = visitor.get("/@ada").await;
    assert!(!profile.body.contains("public-name") && profile.body.contains("Private projects"), "{}", profile.body);
    assert_eq!(b.get("/@ada/p/ObayM/public-name").await.status, StatusCode::OK);

    let page = b.get("/settings").await;
    b.post("/settings/projects", &[("csrf", &page.csrf()), ("project", "ObayM/public-name"), ("hidden", "0")]).await;
    assert_eq!(visitor.get("/@ada/p/ObayM/public-name").await.status, StatusCode::OK);
    app.finish().await;
}

async fn two_projects(app: &TestApp, token: &str, chain: &mut Chain) {
    let t = now();
    chain.editing("ai-tracking", t - 9000.0, t - 8400.0);
    chain.editing("ObayM/kairos", t - 7200.0, t - 6600.0);
    assert_eq!(upload(app, token, chain.take()).await.status, StatusCode::OK);
}

#[tokio::test]
async fn merging_and_unmerging_in_settings_moves_the_rollups_and_is_audited() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    let (token, uid) = ada(&app, &mut b).await;
    let mut chain = Chain::default();
    two_projects(&app, &token, &mut chain).await;
    let before = by_project(&app, uid).await;
    assert_eq!(before.len(), 2);

    let page = b.get("/settings").await;
    assert!(page.body.contains("action=\"/settings/projects/merge\"") && !page.body.contains("Unmerge"), "{}", page.body);
    let csrf = page.csrf();

    let denied = b.post("/settings/projects/merge", &[("csrf", "nope"), ("from", "ai-tracking"), ("into", "ObayM/kairos")]).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(by_project(&app, uid).await, before);

    let merged = b.post("/settings/projects/merge", &[("csrf", &csrf), ("from", "ai-tracking"), ("into", "ObayM/kairos")]).await;
    assert_eq!(merged.status, StatusCode::OK, "{}", merged.body);
    assert!(merged.body.contains("Merged. Time from ai-tracking now counts toward ObayM/kairos."), "{}", merged.body);
    assert!(merged.body.contains("action=\"/settings/projects/unmerge\"") && merged.body.contains("Unmerge"));
    let after = by_project(&app, uid).await;
    assert_eq!(after.keys().collect::<Vec<_>>(), ["ObayM/kairos"]);
    assert!((after["ObayM/kairos"] - (before["ai-tracking"] + before["ObayM/kairos"])).abs() < 1e-6);
    let api = total(&app, "/api/v1/users/ada/stats?total_seconds=true&filter_by_project=ObayM%2Fkairos").await;
    assert!((api as f64 - after["ObayM/kairos"]).abs() <= 1.0, "{api} vs {}", after["ObayM/kairos"]);
    assert_eq!(app.count("SELECT count(*) FROM audit_log WHERE action = 'project.merge' AND actor_id = $1 AND target_user_id = $1 AND detail->>'from' = 'ai-tracking' AND detail->>'into' = 'ObayM/kairos'", uid).await, 1);

    let unmerged = b.post("/settings/projects/unmerge", &[("csrf", &csrf), ("alias", "ai-tracking")]).await;
    assert_eq!(unmerged.status, StatusCode::OK, "{}", unmerged.body);
    assert!(unmerged.body.contains("Unmerged. ai-tracking is its own project again."), "{}", unmerged.body);
    assert_eq!(by_project(&app, uid).await, before);
    assert_eq!(app.count("SELECT count(*) FROM audit_log WHERE action = 'project.unmerge' AND actor_id = $1 AND detail->>'from' = 'ai-tracking'", uid).await, 1);
    assert_eq!(app.count("SELECT count(*) FROM project_aliases WHERE user_id = $1", uid).await, 0);
    assert_eq!(app.count("SELECT count(*) FROM project_links WHERE user_id = $1 AND source = 'split'", uid).await, 1);

    let again = b.post("/settings/projects/merge", &[("csrf", &csrf), ("from", "ai-tracking"), ("into", "ObayM/kairos")]).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(by_project(&app, uid).await.keys().collect::<Vec<_>>(), ["ObayM/kairos"]);
    app.finish().await;
}

#[tokio::test]
async fn a_device_link_after_an_unmerge_applies_again() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    let (token, uid) = ada(&app, &mut b).await;
    let mut chain = Chain::default();
    two_projects(&app, &token, &mut chain).await;
    let csrf = b.csrf().await;
    b.post("/settings/projects/merge", &[("csrf", &csrf), ("from", "ai-tracking"), ("into", "ObayM/kairos")]).await;
    b.post("/settings/projects/unmerge", &[("csrf", &csrf), ("alias", "ai-tracking")]).await;
    assert_eq!(by_project(&app, uid).await.len(), 2);

    chain.push(now() + 30.0, Some("ObayM/kairos"), alias("ai-tracking"));
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);
    assert_eq!(by_project(&app, uid).await.keys().collect::<Vec<_>>(), ["ObayM/kairos"]);
    app.finish().await;
}

#[tokio::test]
async fn merge_requests_that_make_no_sense_are_refused() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    let (token, uid) = ada(&app, &mut b).await;
    let mut chain = Chain::default();
    two_projects(&app, &token, &mut chain).await;
    let before = by_project(&app, uid).await;
    let csrf = b.csrf().await;

    for (from, into) in [("ai-tracking", "ai-tracking"), ("ai-tracking", "nowhere/at-all"), ("nowhere/at-all", "ObayM/kairos"), ("Other", "ObayM/kairos")] {
        let r = b.post("/settings/projects/merge", &[("csrf", &csrf), ("from", from), ("into", into)]).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{from} into {into}: {}", r.body);
    }
    let not_merged = b.post("/settings/projects/unmerge", &[("csrf", &csrf), ("alias", "ai-tracking")]).await;
    assert_eq!(not_merged.status, StatusCode::BAD_REQUEST);

    b.post("/settings/projects/merge", &[("csrf", &csrf), ("from", "ai-tracking"), ("into", "ObayM/kairos")]).await;
    let into_alias = b.post("/settings/projects/merge", &[("csrf", &csrf), ("from", "ObayM/kairos"), ("into", "ai-tracking")]).await;
    assert_eq!(into_alias.status, StatusCode::BAD_REQUEST, "{}", into_alias.body);
    assert_eq!(by_project(&app, uid).await.keys().collect::<Vec<_>>(), ["ObayM/kairos"]);
    assert_ne!(by_project(&app, uid).await, before);
    assert_eq!(app.count("SELECT count(*) FROM audit_log WHERE action LIKE 'project.%' AND actor_id = $1", uid).await, 1);
    app.finish().await;
}

#[tokio::test]
async fn the_export_carries_the_links() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    let (token, _) = ada(&app, &mut b).await;
    let mut chain = Chain::default();
    two_projects(&app, &token, &mut chain).await;
    let csrf = b.csrf().await;
    b.post("/settings/projects/merge", &[("csrf", &csrf), ("from", "ai-tracking"), ("into", "ObayM/kairos")]).await;
    let export = b.get("/settings/export").await;
    let v = export.json();
    let links = v["project_links"].as_array().unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!((links[0]["from"].as_str(), links[0]["to"].as_str(), links[0]["source"].as_str()), (Some("ai-tracking"), Some("ObayM/kairos"), Some("merge")));
    app.finish().await;
}

#[tokio::test]
async fn a_device_stops_adding_moves_at_the_cap() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    let (token, uid) = ada(&app, &mut b).await;
    let cap = kairos_server::aliases::MAX_LINKS;
    app.exec(&format!(
        "INSERT INTO project_links (user_id, from_id, to_id, source, at)
         SELECT {uid}, 'old-' || i, 'new-' || i, 'device', 1 FROM generate_series(1, {}) i",
        cap - 1
    ))
    .await;
    let t = now();
    let mut chain = Chain::default();
    chain.push(t - 600.0, Some("b"), alias("a"));
    chain.push(t - 500.0, Some("d"), alias("c"));
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);
    assert_eq!(app.count("SELECT count(*) FROM project_links WHERE user_id = $1", uid).await, cap);
    assert_eq!(app.count("SELECT count(*) FROM project_links WHERE user_id = $1 AND from_id = 'a'", uid).await, 1);
    assert_eq!(app.count("SELECT count(*) FROM project_links WHERE user_id = $1 AND from_id = 'c'", uid).await, 0);
    app.finish().await;
}
