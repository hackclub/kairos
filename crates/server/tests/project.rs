mod common;

use axum::http::{header, Method, StatusCode};
use common::{upload, user_id, Browser, Chain, Resp, TestApp};
use kairos_core::event::{Event, Kind};
use kairos_server::util::now;
use serde_json::Value;

const SHA_A: &str = "abc1234def5678abc1234def5678abc1234def56";
const SHA_B: &str = "bbbbbbb1111111111111111111111111111111aa";
const SHA_C: &str = "ccccccc2222222222222222222222222222222bb";

fn strict(r: &Resp) {
    let c = r.headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap().to_string();
    let script = c.split(';').find(|d| d.trim().starts_with("script-src")).unwrap();
    assert!(script.contains("'nonce-") && !script.contains("unsafe-inline"), "{c}");
    let nonce = c.split("'nonce-").nth(1).unwrap().split('\'').next().unwrap();
    for tag in r.body.split("<script").skip(1) {
        assert!(tag.split('>').next().unwrap().contains(&format!("nonce=\"{nonce}\"")), "script without the nonce");
    }
    assert!(!r.body.contains("<style") && !r.body.contains(" onclick="));
}

fn commit(sha: &str, files: u32, added: u32, removed: u32) -> Kind {
    Kind::Commit { sha: sha.into(), files, added, removed, authored: None, committed: None, via: None, changes: vec![], replaces: None, not_before: None }
}

async fn project_user(app: &TestApp, b: &mut Browser<'_>) -> (String, i64) {
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let token = b.pair("ada-laptop").await;
    let t = now();
    let mut chain = Chain::default();
    chain.prompt(t - 3900.0, Some("ada/site"), "claude-code");
    chain.reply(t - 3800.0, Some("ada/site"), "claude-code");
    chain.editing("ada/site", t - 3600.0, t - 2400.0);
    chain.push(t - 2300.0, Some("ada/site"), commit(SHA_A, 3, 12, 4));
    chain.push(t - 2200.0, Some("ada/site"), commit(SHA_B, 1, 1, 0));
    assert_eq!(upload(app, &token, chain.take()).await.status, StatusCode::OK);
    (token, user_id(app, "ada").await)
}

#[tokio::test]
async fn a_project_page_shows_the_numbers_commits_and_links() {
    let app = TestApp::new().await;
    let mut ada = app.browser();
    project_user(&app, &mut ada).await;
    let mut visitor = app.browser();

    let page = visitor.get("/@ada/p/ada/site").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    for text in [
        "ada/site",
        "Focus time on this project",
        "This week",
        "Agents ran",
        "The past year",
        "class=\"h0\"",
        "Kind of work",
        "AI tools",
        "Claude Code",
        "Recent commits",
        "abc1234",
        "bbbbbbb",
        "+12",
        "3 files",
        "1 file",
        "https://github.com/ada/site/commit/abc1234def5678abc1234def5678abc1234def56",
        "href=\"https://github.com/ada/site\"",
    ] {
        assert!(page.body.contains(text), "missing {text:?}");
    }
    assert!(!page.body.contains("Only you can see"));
    assert!(!page.body.contains("Hide this project"));
    assert!(page.body.contains("<meta property=\"og:title\" content=\"ada/site by Ada · Kairos\">"));
    assert!(page.body.contains(&format!("<link rel=\"canonical\" href=\"{}/@ada/p/ada/site\">", app.url)));
    assert!(page.body.contains("<meta name=\"description\" content=\"Ada has spent"));
    strict(&page);

    let encoded = visitor.get("/@ada/p/ada%2Fsite").await;
    assert_eq!(encoded.status, StatusCode::OK);
    assert!(encoded.body.contains("Recent commits"));

    assert!(visitor.get("/@ada").await.body.contains("href=\"/@ada/p/ada/site\""), "the profile links the project");
    assert!(ada.get("/me").await.body.contains("href=\"/@ada/p/ada/site\""), "the dashboard links the project");
    app.finish().await;
}

#[tokio::test]
async fn project_pages_follow_the_profile_and_the_hidden_flag() {
    let app = TestApp::new().await;
    let mut ada = app.browser();
    project_user(&app, &mut ada).await;
    let mut visitor = app.browser();

    for (public, hidden, visitor_sees, note) in [
        (true, false, true, ""),
        (true, true, false, "because the project is hidden"),
        (false, false, false, "because your profile is private"),
        (false, true, false, "because the project is hidden"),
    ] {
        app.exec(&format!("UPDATE users SET is_public = {public}")).await;
        app.exec(&format!("UPDATE projects SET hidden = {hidden}")).await;
        for path in ["/@ada/p/ada/site", "/@ada/p/ada%2Fsite"] {
            let seen = visitor.get(path).await;
            assert_eq!(seen.status == StatusCode::OK, visitor_sees, "visitor, public={public} hidden={hidden} {path}");
            if !visitor_sees {
                assert_eq!(seen.status, StatusCode::NOT_FOUND);
                assert!(seen.body.contains("Not found"), "{}", seen.body);
                assert!(!seen.body.contains("ada/site"));
            }
            let own = ada.get(path).await;
            assert_eq!(own.status, StatusCode::OK, "owner, public={public} hidden={hidden} {path}");
            assert_eq!(own.body.contains("Only you can see this page"), !note.is_empty(), "{path}");
            if !note.is_empty() {
                assert!(own.body.contains(note), "{}", own.body);
            }
        }
    }
    app.exec("UPDATE users SET is_public = true").await;
    app.exec("UPDATE projects SET hidden = false").await;
    for path in ["/@ada/p/nope/nothing", "/@nobody/p/ada/site", "/@ada/p/Other"] {
        assert_eq!(visitor.get(path).await.status, StatusCode::NOT_FOUND, "{path}");
    }
    app.finish().await;
}

#[tokio::test]
async fn owners_can_hide_and_show_a_project_from_its_page() {
    let app = TestApp::new().await;
    let mut ada = app.browser();
    project_user(&app, &mut ada).await;
    let mut visitor = app.browser();

    let page = ada.get("/@ada/p/ada/site").await;
    assert!(page.body.contains("Hide this project") && page.body.contains("This page is public"));
    let hide = ada
        .post("/settings/projects", &[("csrf", &page.csrf()), ("project", "ada/site"), ("hidden", "1"), ("next", "/@ada/p/ada/site")])
        .await;
    assert_eq!((hide.status, hide.location().as_str()), (StatusCode::SEE_OTHER, "/@ada/p/ada/site"));
    assert_eq!(visitor.get("/@ada/p/ada/site").await.status, StatusCode::NOT_FOUND);
    let page = ada.get("/@ada/p/ada/site").await;
    assert!(page.body.contains("Show this project"));
    let show = ada
        .post("/settings/projects", &[("csrf", &page.csrf()), ("project", "ada/site"), ("hidden", "0"), ("next", "/@ada/p/ada/site")])
        .await;
    assert_eq!(show.status, StatusCode::SEE_OTHER);
    assert_eq!(visitor.get("/@ada/p/ada/site").await.status, StatusCode::OK);

    let off_site = ada.post("/settings/projects", &[("csrf", &page.csrf()), ("project", "ada/site"), ("hidden", "0"), ("next", "//evil.example")]).await;
    assert_eq!(off_site.location(), "/me");
    let settings = ada.post("/settings/projects", &[("csrf", &page.csrf()), ("project", "ada/site"), ("hidden", "0")]).await;
    assert_eq!(settings.location(), "/settings?done=project");
    app.finish().await;
}

#[tokio::test]
async fn project_ids_with_odd_characters_get_pages_too() {
    let app = TestApp::new().await;
    let mut ada = app.browser();
    ada.sign_in("Ada Lovelace", "ada@example.com").await;
    let token = ada.pair("laptop").await;
    let t = now();
    let mut chain = Chain::default();
    chain.editing("gitlab.com/ada/my-app", t - 3600.0, t - 3000.0);
    chain.editing("my folder", t - 2900.0, t - 2400.0);
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);
    let me = ada.get("/me").await;
    assert!(
        me.body.contains("href=\"/@ada/p/gitlab.com/ada/my-app\"") && me.body.contains("href=\"/@ada/p/my%20folder\""),
        "{}",
        me.body
    );
    for path in ["/@ada/p/gitlab.com/ada/my-app", "/@ada/p/gitlab.com%2Fada%2Fmy-app", "/@ada/p/my%20folder"] {
        assert_eq!(app.browser().get(path).await.status, StatusCode::OK, "{path}");
    }
    let page = app.browser().get("/@ada/p/my%20folder").await;
    assert!(page.body.contains("<h1>my folder</h1>") && !page.body.contains("github.com"));
    let page = app.browser().get("/@ada/p/gitlab.com/ada/my-app").await;
    assert!(page.body.contains("href=\"https://gitlab.com/ada/my-app\""), "{}", page.body);
    assert!(!page.body.contains("/commit/"));
    app.finish().await;
}

type Stored = (String, String, i32, i32, i32, bool);

async fn stored(app: &TestApp, uid: i64) -> Vec<Stored> {
    let c = app.st.pool.get().await.unwrap();
    c.query("SELECT sha, project, files, added, removed, verified FROM commits WHERE user_id = $1 ORDER BY sha", &[&uid])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5)))
        .collect()
}

fn rechain(events: Vec<Value>, head: &str) -> Vec<Value> {
    let mut prev = head.to_string();
    events
        .into_iter()
        .map(|v| {
            let mut e: Event = serde_json::from_value(v).unwrap();
            e.seal(&prev);
            prev = e.hash.clone();
            serde_json::to_value(&e).unwrap()
        })
        .collect()
}

#[tokio::test]
async fn commits_are_stored_once_across_uploads() {
    let app = TestApp::new().await;
    let mut ada = app.browser();
    let (token, uid) = project_user(&app, &mut ada).await;
    let first = stored(&app, uid).await;
    assert_eq!(first.len(), 2);
    assert_eq!(first[0], (SHA_A.to_string(), "ada/site".to_string(), 3, 12, 4, false));

    let t = now();
    let mut chain = Chain::default();
    chain.push(t - 100.0, Some("ada/site"), commit(SHA_A, 9, 99, 99));
    chain.push(t - 90.0, Some("ada/site"), commit(SHA_C, 2, 5, 5));
    chain.push(t - 80.0, Some("ada/site"), commit(&SHA_B.to_uppercase(), 7, 7, 7));
    chain.push(t - 70.0, Some("ada/site"), commit("nothex!", 1, 1, 1));
    chain.push(t - 60.0, None, commit("dddddddd", 1, 1, 1));
    let head: Option<String> = {
        let c = app.st.pool.get().await.unwrap();
        c.query_one("SELECT head FROM devices WHERE user_id = $1", &[&uid]).await.unwrap().get(0)
    };
    let events = rechain(chain.take(), &head.unwrap());
    assert_eq!(upload(&app, &token, events).await.status, StatusCode::OK);

    let after = stored(&app, uid).await;
    assert_eq!(after.len(), 3, "{after:?}");
    assert_eq!(after[0], first[0], "the first copy of a commit is kept");
    assert!(after.iter().all(|r| !r.5 && r.1 == "ada/site"));
    assert!(after.iter().any(|r| r.0 == SHA_C));
    let me = app.api(Method::GET, "/api/v1/device/me", None, Some(&token)).await;
    assert_eq!(me.status, StatusCode::OK);
    app.finish().await;
}

#[tokio::test]
async fn a_project_without_data_still_has_a_page() {
    let app = TestApp::new().await;
    let mut ada = app.browser();
    ada.sign_in("Ada Lovelace", "ada@example.com").await;
    let uid = user_id(&app, "ada").await;
    app.exec(&format!("INSERT INTO projects (user_id, id, first_seen, last_seen) VALUES ({uid}, 'ada/empty', 1700000000, 1700000100)")).await;
    let page = app.browser().get("/@ada/p/ada/empty").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    for text in ["<div class=\"big\">0m</div>", "No commits yet", "Nothing here yet.", "The past year", "Nov 14, 2023"] {
        assert!(page.body.contains(text), "missing {text:?}");
    }
    strict(&page);
    app.finish().await;
}
