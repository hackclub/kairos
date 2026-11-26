mod common;

use axum::http::{Method, StatusCode};
use common::{user_id, Chain, TestApp};
use serde_json::{json, Value};

fn hour_ago(n: i64) -> i64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    now - now % 3600 - n * 3600
}

fn row(hour: i64, model: &str, project: Option<&str>, n: (u64, u64, u64, u64)) -> Value {
    json!({ "hour": hour, "harness": "claude-code", "model": model, "project": project,
            "input": n.0, "output": n.1, "cache_read": n.2, "cache_write": n.3 })
}

async fn send(app: &TestApp, token: &str, hours: Vec<Value>) -> (StatusCode, Value) {
    let r = app.api(Method::POST, "/api/v1/tokens", Some(json!({ "hours": hours })), Some(token)).await;
    let v = serde_json::from_str(&r.body).unwrap_or(Value::Null);
    (r.status, v)
}

async fn stored(app: &TestApp, uid: i64) -> Vec<(String, String, i64, i64, i64, i64)> {
    let c = app.st.pool.get().await.unwrap();
    c.query("SELECT model, project, input, output, cache_read, cache_write FROM token_hours WHERE user_id = $1 ORDER BY hour, model", &[&uid])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5)))
        .collect()
}

#[tokio::test]
async fn devices_send_token_hours_only_while_the_person_has_them_on() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let uid = user_id(&app, "ada").await;
    let laptop = b.pair("laptop").await;
    let h = hour_ago(2);

    assert_eq!(app.api(Method::GET, "/api/v1/tokens", None, None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(app.api(Method::GET, "/api/v1/tokens", None, Some(&laptop)).await.json(), json!({ "on": false }));
    assert_eq!(send(&app, &laptop, vec![row(h, "m", None, (1, 1, 1, 1))]).await, (StatusCode::OK, json!({ "on": false })));
    assert!(stored(&app, uid).await.is_empty());

    let csrf = b.csrf().await;
    let on = b.post("/settings/tokens", &[("csrf", &csrf), ("on", "1")]).await;
    assert_eq!((on.status, on.location()), (StatusCode::SEE_OTHER, "/settings?done=tokens_on#tokens".to_string()));
    assert_eq!(app.api(Method::GET, "/api/v1/tokens", None, Some(&laptop)).await.json(), json!({ "on": true }));

    let first = vec![
        row(h, "claude-opus-5-5", Some("ada/site"), (10, 20, 300, 40)),
        row(h, "claude-opus-5-5", Some("ada/site"), (11, 5, 5, 5)),
        row(h + 1, "m", None, (1, 1, 1, 1)),
        row(hour_ago(24 * 41), "m", None, (1, 1, 1, 1)),
        row(h, "bad\u{0}name", Some("ada/\nsite"), (7, 0, 0, 0)),
    ];
    assert_eq!(send(&app, &laptop, first).await, (StatusCode::OK, json!({ "on": true })));
    assert_eq!(
        stored(&app, uid).await,
        vec![
            ("badname".into(), "ada/site".into(), 7, 0, 0, 0),
            ("claude-opus-5-5".into(), "ada/site".into(), 11, 20, 300, 40),
        ]
    );
    // A resend keeps the larger of each count, so an older, smaller total never wins.
    send(&app, &laptop, vec![row(h, "claude-opus-5-5", Some("ada/site"), (9, 25, 100, 40))]).await;
    assert_eq!(stored(&app, uid).await[1], ("claude-opus-5-5".into(), "ada/site".into(), 11, 25, 300, 40));

    let too_many: Vec<Value> = (0..2001).map(|i| row(h, &format!("m{i}"), None, (1, 0, 0, 0))).collect();
    assert_eq!(send(&app, &laptop, too_many).await.0, StatusCode::PAYLOAD_TOO_LARGE);

    let off = b.post("/settings/tokens", &[("csrf", &csrf), ("on", "0")]).await;
    assert_eq!(off.location(), "/settings?done=tokens_off#tokens");
    assert_eq!(app.api(Method::GET, "/api/v1/tokens", None, Some(&laptop)).await.json(), json!({ "on": false }));
    assert_eq!(stored(&app, uid).await.len(), 2, "turning off keeps what is there unless asked");
    assert!(b.get("/settings").await.body.contains("Delete the token stats Kairos has"));
    let gone = b.post("/settings/tokens", &[("csrf", &csrf), ("on", "0"), ("delete", "1")]).await;
    assert_eq!(gone.location(), "/settings?done=tokens_deleted#tokens");
    assert!(stored(&app, uid).await.is_empty());
    app.finish().await;
}

#[tokio::test]
async fn token_stats_show_to_their_owner_and_on_public_pages_only_when_shared() {
    let app = TestApp::new().await;
    let mut ada = app.browser();
    ada.sign_in("Ada Lovelace", "ada@example.com").await;
    let laptop = ada.pair("laptop").await;
    let mut visitor = app.browser();

    let me = ada.get("/me").await;
    assert!(me.body.contains("/settings#tokens") && !me.body.contains(r#"id="tokens""#), "{}", me.body);

    let mut c = Chain::default();
    let t = hour_ago(1) as f64;
    c.editing("ada/site", t, t + 1800.0);
    assert_eq!(common::upload(&app, &laptop, c.take()).await.status, StatusCode::OK);

    let csrf = ada.csrf().await;
    ada.post("/settings/tokens", &[("csrf", &csrf), ("on", "1")]).await;
    let empty = ada.get("/me").await;
    assert!(empty.body.contains("No token stats yet"), "{}", empty.body);

    app.exec(
        "INSERT INTO model_prices (model, input, output, cache_read, cache_write)
         VALUES ('claude-opus-5-5', 0.000005, 0.000025, 0.0000005, 0.00000625)",
    )
    .await;
    let h = hour_ago(1);
    send(
        &app,
        &laptop,
        vec![
            row(h, "claude-opus-5-5-20261101", Some("ada/site"), (1_000_000, 100_000, 10_000_000, 0)),
            row(h, "big-pickle", Some("ada/site"), (500_000, 0, 0, 0)),
            row(h, "claude-opus-5-5", Some("ada/other"), (0, 0, 0, 1_000_000)),
        ],
    )
    .await;

    // 1M input $5, 100K output $2.50, 10M cache reads $5 = $12.50 in ada/site, 1M cache writes $6.25 in ada/other.
    let me = ada.get("/me").await;
    assert!(me.body.contains(r#"id="tokens""#) && me.body.contains("$18.75") && me.body.contains("12.6M tokens"), "{}", me.body);
    assert!(me.body.contains("big-pickle") && me.body.contains("no price") && me.body.contains("Models with no known price"), "{}", me.body);
    assert!(me.body.contains(r#"id="tokdays""#) && me.body.contains(r#"id="tokboxes""#));

    let project = ada.get("/@ada/p/ada/site").await;
    assert!(project.body.contains("$12.50") && project.body.contains("11.6M") && project.body.contains("API cost per focus hour"), "{}", project.body);
    let focus = app.st.pool.get().await.unwrap().query_one("SELECT sum(seconds)::float8 FROM user_days WHERE project = 'ada/site' AND class <> 'agent'", &[]).await.unwrap().get::<_, f64>(0);
    assert!(focus > 1500.0, "{focus}");
    let per_hour = format!("${:.2}", 12.5 / (focus / 3600.0));
    assert!(project.body.contains(&per_hour), "{per_hour}: {}", project.body);
    assert!(project.body.contains(r#"data-path="/@ada/p/ada/site""#), "{}", project.body);
    let long_ago = ada.get("/@ada/p/ada/site?from=2026-01-01&to=2026-01-02").await;
    assert!(long_ago.body.contains("No tokens on these days"), "{}", long_ago.body);

    assert!(ada.get("/@ada").await.body.contains(r#"id="tokens""#), "the owner sees their own");
    assert!(!visitor.get("/@ada").await.body.contains(r#"id="tokens""#));
    assert!(!visitor.get("/@ada/p/ada/site").await.body.contains(r#"id="tokens""#));

    ada.post("/settings/tokens", &[("csrf", &csrf), ("on", "1"), ("public", "on")]).await;
    let profile = visitor.get("/@ada").await;
    assert!(profile.body.contains(r#"id="tokens""#) && profile.body.contains("$18.75"), "{}", profile.body);
    assert!(!profile.body.contains(r#"id="dates""#), "profiles get no date picker");
    assert!(visitor.get("/@ada/p/ada/site").await.body.contains("$12.50"));

    ada.post("/settings/tokens", &[("csrf", &csrf), ("on", "0")]).await;
    assert!(!visitor.get("/@ada").await.body.contains(r#"id="tokens""#), "turning off takes them off public pages");
    assert!(ada.get("/me").await.body.contains("$18.75"), "what is kept still shows to its owner");
    app.finish().await;
}
