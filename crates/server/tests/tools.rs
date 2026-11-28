mod common;

use axum::http::StatusCode;
use common::{upload, user_id, Chain, TestApp};
use kairos_core::event::Kind;

const D: f64 = 1773100800.0;

async fn tools(app: &TestApp, user_id: i64) -> Vec<(String, String, i32)> {
    let c = app.st.pool.get().await.unwrap();
    c.query("SELECT day::text, harness, sum(prompts)::int FROM user_tool_days WHERE user_id = $1 GROUP BY 1, 2 ORDER BY 1, 2", &[&user_id])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect()
}

fn rows(list: &[(&str, &str, i32)]) -> Vec<(String, String, i32)> {
    list.iter().map(|(d, h, n)| (d.to_string(), h.to_string(), *n)).collect()
}

#[tokio::test]
async fn prompts_roll_up_per_tool_and_local_day_across_devices() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let uid = user_id(&app, "ada").await;
    let laptop = b.pair("laptop").await;
    let desktop = b.pair("desktop").await;

    let mut a = Chain::default();
    a.prompt(D + 3600.0, Some("ada/site"), "codex");
    a.prompt(D + 86340.0, Some("ada/site"), "claude-code");
    assert_eq!(upload(&app, &laptop, a.take()).await.status, StatusCode::OK);

    let mut d = Chain::default();
    d.prompt(D + 7200.0, Some("ada/site"), "claude-code");
    d.prompt(D + 7300.0, Some("ada/site"), "Not A Tool");
    d.prompt(D + 7400.0, None, "x".repeat(40).as_str());
    d.prompt(D + 86460.0, Some("ada/site"), "claude-code");
    assert_eq!(upload(&app, &desktop, d.take()).await.status, StatusCode::OK);

    assert_eq!(
        tools(&app, uid).await,
        rows(&[("2026-03-10", "claude-code", 2), ("2026-03-10", "codex", 1), ("2026-03-11", "claude-code", 1)])
    );

    let csrf = b.csrf().await;
    let form = [
        ("csrf", csrf.as_str()),
        ("username", "ada"),
        ("display_name", ""),
        ("is_public", "on"),
        ("show_ai_tools", "on"),
        ("timezone", "Asia/Kolkata"),
    ];
    assert_eq!(b.post("/settings/profile", &form).await.status, StatusCode::SEE_OTHER);
    assert_eq!(
        tools(&app, uid).await,
        rows(&[("2026-03-10", "claude-code", 1), ("2026-03-10", "codex", 1), ("2026-03-11", "claude-code", 2)])
    );

    let again = [("timezone", "UTC")];
    let back: Vec<(&str, &str)> = form.iter().map(|&(k, v)| (k, again.iter().find(|a| a.0 == k).map_or(v, |a| a.1))).collect();
    assert_eq!(b.post("/settings/profile", &back).await.status, StatusCode::SEE_OTHER);
    assert_eq!(
        tools(&app, uid).await,
        rows(&[("2026-03-10", "claude-code", 2), ("2026-03-10", "codex", 1), ("2026-03-11", "claude-code", 1)])
    );
    app.finish().await;
}

#[tokio::test]
async fn a_later_upload_updates_the_days_it_touches() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let uid = user_id(&app, "ada").await;
    let token = b.pair("laptop").await;

    let mut c = Chain::default();
    c.prompt(D + 100.0, None, "cursor");
    assert_eq!(upload(&app, &token, c.take()).await.status, StatusCode::OK);
    c.prompt(D + 200.0, None, "cursor");
    c.prompt(D + 300.0, None, "gemini-cli");
    assert_eq!(upload(&app, &token, c.take()).await.status, StatusCode::OK);
    assert_eq!(tools(&app, uid).await, rows(&[("2026-03-10", "cursor", 2), ("2026-03-10", "gemini-cli", 1)]));
    app.finish().await;
}

#[tokio::test]
async fn a_prompt_sent_again_from_a_copied_log_counts_once() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let uid = user_id(&app, "ada").await;
    let laptop = b.pair("laptop").await;
    let desktop = b.pair("desktop").await;
    let prompt = |words: usize, session: &str| Kind::Prompt { words, pasted: false, model: None, session: session.into(), shape: None, version: None, via: None, permissions: None };

    let mut a = Chain::default();
    a.push_from(D + 3600.0, Some("ada/site"), "claude-code", prompt(5, "s1"));
    a.push_from(D + 3700.0, Some("ada/site"), "claude-code", prompt(7, "s1"));
    a.push_from(D + 3700.0, Some("ada/site"), "claude-code", prompt(9, "s2"));
    assert_eq!(upload(&app, &laptop, a.take()).await.status, StatusCode::OK);
    // A forked session repeats the first prompt, later in the same chain.
    a.push_from(D + 3600.0, Some("ada/site"), "claude-code", prompt(5, "fork"));
    assert_eq!(upload(&app, &laptop, a.take()).await.status, StatusCode::OK);
    let mut d = Chain::default();
    d.push_from(D + 3700.0, None, "claude-code", prompt(7, "s1"));
    assert_eq!(upload(&app, &desktop, d.take()).await.status, StatusCode::OK);

    assert_eq!(tools(&app, uid).await, rows(&[("2026-03-10", "claude-code", 3)]));
    app.exec("DELETE FROM user_tool_days").await;
    kairos_server::credit::rebuild_user(&app.st.pool, uid).await.unwrap();
    assert_eq!(tools(&app, uid).await, rows(&[("2026-03-10", "claude-code", 3)]));
    app.finish().await;
}

#[tokio::test]
async fn rebuild_restores_spans_and_rollups_from_events() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let uid = user_id(&app, "ada").await;
    let laptop = b.pair("laptop").await;
    let mut a = Chain::default();
    a.prompt(D + 3600.0, Some("ada/site"), "codex");
    a.reply(D + 3700.0, Some("ada/site"), "codex");
    a.editing("ada/site", D + 4000.0, D + 4600.0);
    assert_eq!(upload(&app, &laptop, a.take()).await.status, StatusCode::OK);

    let before = tools(&app, uid).await;
    let days = "SELECT count(*) FROM user_days WHERE user_id = $1";
    let spans = "SELECT count(*) FROM spans WHERE user_id = $1";
    let (n_days, n_spans) = (app.count(days, uid).await, app.count(spans, uid).await);
    assert!(n_days > 0 && n_spans > 0 && !before.is_empty());

    app.exec("DELETE FROM user_tool_days; DELETE FROM user_days; DELETE FROM spans; DELETE FROM agent_spans").await;
    kairos_server::credit::rebuild_user(&app.st.pool, uid).await.unwrap();
    assert_eq!(tools(&app, uid).await, before);
    assert_eq!((app.count(days, uid).await, app.count(spans, uid).await), (n_days, n_spans));
    app.finish().await;
}

#[tokio::test]
async fn users_stored_under_an_older_credit_revision_are_rebuilt_once() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let uid = user_id(&app, "ada").await;
    let laptop = b.pair("laptop").await;
    let mut a = Chain::default();
    a.editing("ada/site", D + 4000.0, D + 4600.0);
    assert_eq!(upload(&app, &laptop, a.take()).await.status, StatusCode::OK);
    let mut other = app.browser();
    other.sign_in("Bea Hacker", "bea@example.com").await;

    let days = "SELECT count(*) FROM user_days WHERE user_id = $1";
    let n_days = app.count(days, uid).await;
    assert!(n_days > 0);
    app.exec("UPDATE users SET credit_revision = 0; DELETE FROM user_days").await;
    app.exec(&format!("UPDATE users SET credit_revision = {} WHERE username = 'bea'", kairos_core::credit::REVISION)).await;

    assert_eq!(kairos_server::credit::rebuild_stale(&app.st.pool).await.unwrap(), app.count("SELECT count(*) FROM users WHERE id <> $1 AND username <> 'bea'", 0).await as usize);
    assert_eq!(app.count(days, uid).await, n_days);
    assert_eq!(app.count("SELECT count(*) FROM users WHERE credit_revision < $1::bigint", kairos_core::credit::REVISION as i64).await, 0);
    assert_eq!(kairos_server::credit::rebuild_stale(&app.st.pool).await.unwrap(), 0);
    app.finish().await;
}
