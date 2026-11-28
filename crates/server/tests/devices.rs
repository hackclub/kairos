mod common;

use axum::http::{header, Method, StatusCode};
use common::{day_seconds, query_of, upload, user_id, Chain, TestApp};
use kairos_core::account;
use kairos_core::pairing::Pairing;
use kairos_core::pkce;
use kairos_core::event::{Class, Kind};
use serde_json::json;

#[tokio::test]
async fn pairing_approve_poll_once() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;

    let start = app.api(Method::POST, "/api/v1/device/start", Some(json!({ "name": "ada-laptop", "os": "linux" })), None).await.json();
    let code = start["user_code"].as_str().unwrap().to_string();
    let poll = json!({ "poll_token": start["poll_token"] });
    assert!(start["verify_url"].as_str().unwrap().ends_with(&format!("/device?code={code}")));

    let pending = app.api(Method::POST, "/api/v1/device/poll", Some(poll.clone()), None).await;
    assert_eq!(pending.status, StatusCode::OK);
    assert_eq!(pending.json()["status"], "pending");

    let mut signed_out = app.browser();
    let page = signed_out.get(&format!("/device?code={code}")).await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    assert!(page.location().starts_with("/auth/login?next="));

    let page = b.get(&format!("/device?code={code}")).await;
    assert!(page.body.contains("ada-laptop"));
    let bad = b.post("/device/approve", &[("code", &code), ("csrf", "00")]).await;
    assert_eq!(bad.status, StatusCode::FORBIDDEN);
    let ok = b.post("/device/approve", &[("code", &code), ("csrf", &page.csrf())]).await;
    assert!(ok.body.contains("Computer connected"), "{}", ok.body);

    let approved = app.api(Method::POST, "/api/v1/device/poll", Some(poll.clone()), None).await.json();
    assert_eq!(approved["status"], "approved");
    assert_eq!(approved["username"], "ada");
    let token = approved["token"].as_str().unwrap().to_string();
    let again = app.api(Method::POST, "/api/v1/device/poll", Some(poll), None).await;
    assert_eq!(again.status, StatusCode::GONE);

    let me = app.api(Method::GET, "/api/v1/device/me", None, Some(&token)).await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.body);
    assert_eq!(me.json()["username"], "ada");
    let c = app.st.pool.get().await.unwrap();
    let r = c.query_one("SELECT name, os, client_version FROM devices", &[]).await.unwrap();
    assert_eq!((r.get::<_, String>(0), r.get::<_, String>(1)), ("ada-laptop".into(), "linux".into()));
    assert_eq!(r.get::<_, Option<String>>(2).as_deref(), Some("9.9.9"));
    assert_eq!(c.query_one("SELECT count(*) FROM device_codes", &[]).await.unwrap().get::<_, i64>(0), 0);
    app.finish().await;
}

const STATE: &str = "s7Hq2xYk9PzLmN4c";

fn connect_path(port: &str, state: &str, challenge: &str) -> String {
    format!("/device/connect?port={port}&state={state}&challenge={challenge}")
}

#[tokio::test]
async fn connect_hands_a_one_time_code_to_the_computer_that_asked() {
    let app = TestApp::new().await;
    let verifier = pkce::verifier().unwrap();
    let path = connect_path("50123", STATE, &pkce::challenge(&verifier));

    let mut signed_out = app.browser();
    let page = signed_out.get(&path).await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    assert_eq!(query_of(&format!("http://x{}", page.location()))["next"], path, "signing in comes back here");
    assert_eq!(app.count("SELECT count(*) FROM device_grants WHERE user_id <> $1", 0).await, 0);

    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let code_from = |r: &common::Resp| {
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        assert_eq!(r.headers.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()), Some("no-store"));
        let links: Vec<String> = r.body.split('"').filter(|s| s.contains("127.0.0.1:")).map(|s| s.replace("&#38;", "&").replace("&amp;", "&")).collect();
        let [refresh, to] = <[String; 2]>::try_from(links).expect("a refresh and a link to the computer");
        assert_eq!(refresh, format!("0;url={to}"));
        assert!(to.starts_with("http://127.0.0.1:50123/callback?"), "{to}");
        let q = query_of(&to);
        assert_eq!(q["state"], STATE);
        q["code"].clone()
    };
    let exchange = |code: &str, verifier: &str| {
        json!({ "code": code, "verifier": verifier, "name": "ada-laptop", "os": "macos" })
    };

    let code = code_from(&b.get(&path).await);
    let wrong = app.api(Method::POST, "/api/v1/device/exchange", Some(exchange(&code, &pkce::verifier().unwrap())), None).await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST, "{}", wrong.body);
    let spent = app.api(Method::POST, "/api/v1/device/exchange", Some(exchange(&code, &verifier)), None).await;
    assert_eq!(spent.status, StatusCode::BAD_REQUEST, "a wrong verifier uses the code up");

    let code = code_from(&b.get(&path).await);
    let ok = app.api(Method::POST, "/api/v1/device/exchange", Some(exchange(&code, &verifier)), None).await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    assert_eq!(ok.json()["username"], "ada");
    let token = ok.json()["token"].as_str().unwrap().to_string();
    let again = app.api(Method::POST, "/api/v1/device/exchange", Some(exchange(&code, &verifier)), None).await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST, "each code works once");

    let me = app.api(Method::GET, "/api/v1/device/me", None, Some(&token)).await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.body);
    let c = app.st.pool.get().await.unwrap();
    let r = c.query_one("SELECT name, os, client_version FROM devices", &[]).await.unwrap();
    assert_eq!((r.get::<_, String>(0), r.get::<_, String>(1)), ("ada-laptop".into(), "macos".into()));
    assert_eq!(r.get::<_, Option<String>>(2).as_deref(), Some("9.9.9"));

    let code = code_from(&b.get(&path).await);
    app.exec("UPDATE device_grants SET expires_at = now() - interval '1 second'").await;
    let late = app.api(Method::POST, "/api/v1/device/exchange", Some(exchange(&code, &verifier)), None).await;
    assert_eq!(late.status, StatusCode::BAD_REQUEST);
    assert_eq!(app.count("SELECT count(*) FROM devices WHERE user_id <> $1", 0).await, 1);
    app.finish().await;
}

#[tokio::test]
async fn connect_refuses_links_it_cannot_send_back_safely() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let challenge = pkce::challenge(&pkce::verifier().unwrap());
    let bad = [
        connect_path("80", STATE, &challenge),
        connect_path("70000", STATE, &challenge),
        connect_path("50123@evil.example", STATE, &challenge),
        connect_path("50123", "short", &challenge),
        connect_path("50123", "s7Hq2xYk9PzLmN4c%26x%3Dy", &challenge),
        connect_path("50123", STATE, "plain"),
        connect_path("50123", STATE, &format!("{}=", &challenge[..42])),
        "/device/connect?port=50123&state=s7Hq2xYk9PzLmN4c".to_string(),
        "/device/connect".to_string(),
    ];
    for path in bad {
        let r = b.get(&path).await;
        assert_eq!(r.status, StatusCode::OK, "{path}: {}", r.location());
        assert!(r.body.contains("That link doesn"), "{path}");
    }
    assert_eq!(app.count("SELECT count(*) FROM device_grants WHERE user_id <> $1", 0).await, 0);
    let garbage = app.api(Method::POST, "/api/v1/device/exchange", Some(json!({ "code": "kg_x", "verifier": "short", "name": "x", "os": "linux" })), None).await;
    assert_eq!(garbage.status, StatusCode::BAD_REQUEST);
    app.finish().await;
}

#[tokio::test]
async fn the_dashboard_says_the_computer_is_connected() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    assert!(!b.get("/me").await.body.contains("This computer is connected"));
    let page = b.get("/me?connected=1").await.body;
    assert!(page.contains("This computer is connected. Kairos runs in the background"), "{page}");
    assert!(b.get("/me?connected=1&hackatime=1").await.body.contains("and Hackatime is set up on it too"));
    assert!(b.get("/me?hackatime=1").await.body.contains("Hackatime is set up on this computer."));
    app.finish().await;
}

#[tokio::test]
async fn expired_codes_cannot_be_approved_or_polled() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let start = app.api(Method::POST, "/api/v1/device/start", Some(json!({ "name": "old", "os": "linux" })), None).await.json();
    let code = start["user_code"].as_str().unwrap().to_string();
    app.exec("UPDATE device_codes SET expires_at = now() - interval '1 second'").await;

    let page = b.get(&format!("/device?code={code}")).await;
    assert!(page.body.contains("That code doesn't work anymore"), "{}", page.body);
    let csrf = b.csrf().await;
    let res = b.post("/device/approve", &[("code", &code), ("csrf", &csrf)]).await;
    assert!(!res.body.contains("Computer connected"));
    let poll = app.api(Method::POST, "/api/v1/device/poll", Some(json!({ "poll_token": start["poll_token"] })), None).await;
    assert_eq!(poll.status, StatusCode::GONE);
    let unknown = app.api(Method::POST, "/api/v1/device/poll", Some(json!({ "poll_token": "kp_nope" })), None).await;
    assert_eq!(unknown.status, StatusCode::GONE);
    assert_eq!(app.count("SELECT count(*) FROM devices WHERE user_id <> $1", 0).await, 0);
    app.finish().await;
}

#[tokio::test]
async fn revoked_devices_get_401() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let token = b.pair("laptop").await;
    let mut chain = Chain::default();
    chain.editing("acme/app", 1_773_000_000.0, 1_773_000_300.0);
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);

    let c = app.st.pool.get().await.unwrap();
    let id: i64 = c.query_one("SELECT id FROM devices", &[]).await.unwrap().get(0);
    let csrf = b.csrf().await;
    let res = b.post("/settings/devices/revoke", &[("csrf", &csrf), ("device_id", &id.to_string())]).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);

    chain.editing("acme/app", 1_773_000_400.0, 1_773_000_500.0);
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(app.api(Method::GET, "/api/v1/device/me", None, Some(&token)).await.status, StatusCode::UNAUTHORIZED);

    let mut bea = app.browser();
    bea.sign_in("Bea Hacker", "bea@example.com").await;
    let other = bea.pair("bea-laptop").await;
    let bea_dev: i64 = c.query_one("SELECT id FROM devices WHERE name = 'bea-laptop'", &[]).await.unwrap().get(0);
    let csrf = b.csrf().await;
    b.post("/settings/devices/revoke", &[("csrf", &csrf), ("device_id", &bea_dev.to_string())]).await;
    assert_eq!(app.api(Method::GET, "/api/v1/device/me", None, Some(&other)).await.status, StatusCode::OK);
    app.finish().await;
}

async fn me(app: &TestApp, token: &str) -> common::Resp {
    app.api(Method::GET, "/api/v1/device/me", None, Some(token)).await
}

/// Signs a computer in with a code, as `kairosd` does. `last` is the last event it uploaded.
async fn pair_again(b: &mut common::Browser<'_>, app: &TestApp, last: Option<&str>) -> (String, bool) {
    let s = app.api(Method::POST, "/api/v1/device/start", Some(json!({ "name": "renamed-since", "os": "linux" })), None).await.json();
    let code = s["user_code"].as_str().unwrap().to_string();
    let page = b.get(&format!("/device?code={code}")).await;
    b.post("/device/approve", &[("code", &code), ("csrf", &page.csrf())]).await;
    let p = app.api(Method::POST, "/api/v1/device/poll", Some(json!({ "poll_token": s["poll_token"], "last_hash": last })), None).await.json();
    assert_eq!(p["status"], "approved", "{p}");
    (p["token"].as_str().unwrap().to_string(), p["resumed"].as_bool().unwrap())
}

#[tokio::test]
async fn a_computer_that_signs_in_again_gets_its_device_back() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let ada = user_id(&app, "ada").await;
    let devices = "SELECT count(*) FROM devices WHERE user_id = $1";
    let last_of = |events: &[serde_json::Value]| events.last().unwrap()["hash"].as_str().unwrap().to_string();

    let first = b.pair("laptop").await;
    let id = me(&app, &first).await.json()["device_id"].clone();
    let mut chain = Chain::default();
    chain.editing("acme/app", 1_773_000_000.0, 1_773_000_300.0);
    let events = chain.take();
    let (sent, last) = (events.len(), last_of(&events));
    assert_eq!(upload(&app, &first, events).await.status, StatusCode::OK);

    let out = app.api(Method::POST, "/api/v1/device/logout", None, Some(&first)).await;
    assert_eq!(out.status, StatusCode::OK, "{}", out.body);
    assert_eq!(me(&app, &first).await.status, StatusCode::UNAUTHORIZED, "logging out ends the token");
    assert_eq!(app.api(Method::POST, "/api/v1/device/logout", None, Some(&first)).await.status, StatusCode::UNAUTHORIZED);

    let (second, resumed) = pair_again(&mut b, &app, Some(&last)).await;
    assert!(resumed);
    assert_eq!(app.count(devices, ada).await, 1);
    let m = me(&app, &second).await.json();
    assert_eq!((&m["device_id"], m["events"].as_i64(), m["last_hash"].as_str()), (&id, Some(sent as i64), Some(last.as_str())));
    assert_eq!(me(&app, &first).await.status, StatusCode::UNAUTHORIZED);
    let c = app.st.pool.get().await.unwrap();
    let name: String = c.query_one("SELECT name FROM devices WHERE user_id = $1", &[&ada]).await.unwrap().get(0);
    assert_eq!(name, "laptop", "the name it had, which its owner may have chosen");

    chain.editing("acme/app", 1_773_000_400.0, 1_773_000_500.0);
    let events = chain.take();
    let last = last_of(&events);
    assert_eq!(upload(&app, &second, events).await.status, StatusCode::OK, "the log goes on from where it stopped");

    let verifier = pkce::verifier().unwrap();
    let page = b.get(&connect_path("50123", STATE, &pkce::challenge(&verifier))).await;
    let to = page.body.split('"').find(|s| s.starts_with("http://127.0.0.1:")).unwrap().replace("&#38;", "&").replace("&amp;", "&");
    let exchange = json!({ "code": query_of(&to)["code"], "verifier": verifier, "name": "laptop", "os": "linux", "last_hash": last });
    let third = app.api(Method::POST, "/api/v1/device/exchange", Some(exchange), None).await.json();
    assert_eq!(third["resumed"], true, "{third}");
    assert_eq!(me(&app, &second).await.status, StatusCode::UNAUTHORIZED, "one token at a time");
    let third = third["token"].as_str().unwrap().to_string();
    assert_eq!(me(&app, &third).await.json()["device_id"], id);

    for unknown in [None, Some("0".repeat(64)), Some("not a hash".to_string()), Some(String::new())] {
        let (_, resumed) = pair_again(&mut b, &app, unknown.as_deref()).await;
        assert!(!resumed, "{unknown:?}");
    }
    assert_eq!(app.count(devices, ada).await, 5);

    let mut bea = app.browser();
    bea.sign_in("Bea Hacker", "bea@example.com").await;
    let (hers, resumed) = pair_again(&mut bea, &app, Some(&last)).await;
    assert!(!resumed, "another account never gets this one's device");
    assert_eq!(me(&app, &hers).await.json()["events"], 0);
    assert_eq!(me(&app, &third).await.status, StatusCode::OK);
    app.finish().await;
}

#[tokio::test]
async fn ingest_rejects_broken_chains_and_bad_times() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let token = b.pair("laptop").await;

    let mut chain = Chain::default();
    chain.editing("acme/app", 1_773_000_000.0, 1_773_000_300.0);
    let first = upload(&app, &token, chain.take()).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(first.json()["accepted"], 6);
    let head = first.json()["last_hash"].clone();

    let mut fork = Chain::default();
    fork.editing("acme/app", 1_773_000_400.0, 1_773_000_460.0);
    let res = upload(&app, &token, fork.take()).await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    assert_eq!(res.json()["error"], "chain_mismatch");
    assert_eq!(res.json()["last_hash"], head);

    chain.editing("acme/app", 1_773_000_400.0, 1_773_000_520.0);
    let mut tampered = chain.events.clone();
    tampered[1]["ts"] = json!(1_773_000_401.0);
    let res = upload(&app, &token, tampered).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(res.json()["error"], "bad_hash");
    assert_eq!(res.json()["index"], 1);

    let mut future = Chain::default();
    let later = kairos_server::util::now() + 86_400.0;
    future.push(later, Some("acme/app"), Kind::Focus { class: Class::Editor, app: None });
    let res = upload(&app, &token, future.take()).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(res.json()["error"], "bad_ts");

    let ok = upload(&app, &token, chain.take()).await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    assert_eq!(ok.json()["accepted"], 3);
    let uid = user_id(&app, "ada").await;
    assert_eq!(app.count("SELECT count(*) FROM events e JOIN devices d ON d.id = e.device_id WHERE d.user_id = $1", uid).await, 9);
    assert_eq!(app.count("SELECT max(seq) FROM events e JOIN devices d ON d.id = e.device_id WHERE d.user_id = $1", uid).await, 9);
    assert_eq!(app.count("SELECT count(*) FROM projects WHERE user_id = $1 AND repo_url = 'https://github.com/acme/app'", uid).await, 1);
    let res = upload(&app, &token, Vec::new()).await;
    assert_eq!(res.json()["accepted"], 0);
    app.finish().await;
}

#[tokio::test]
async fn fractional_timestamps_keep_their_hashes() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let token = b.pair("laptop").await;
    let mut chain = Chain::default();
    let mut ts = 1_773_000_000.123_456_7;
    for i in 0..2000 {
        ts += 0.731_234_567_891_3 + (i % 7) as f64 * 0.000_123_456_789;
        chain.push(ts, None, Kind::Present { input: None, keys: None, injected: None });
    }
    let res = upload(&app, &token, chain.take()).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    app.finish().await;
}

#[tokio::test]
async fn a_present_with_input_bits_is_stored_as_sent() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let token = b.pair("laptop").await;
    let mut chain = Chain::default();
    chain.push(1_773_000_060.0, None, Kind::Present { input: Some("800000000000001".into()), keys: None, injected: None });
    chain.push(1_773_000_120.0, None, Kind::Present { input: None, keys: None, injected: None });
    let res = upload(&app, &token, chain.take()).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let c = app.st.pool.get().await.unwrap();
    let rows = c.query("SELECT body FROM events WHERE kind = 'present' ORDER BY seq", &[]).await.unwrap();
    let bodies: Vec<serde_json::Value> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(bodies[0]["input"], "800000000000001");
    assert!(bodies[1].get("input").is_none());
    drop(c);
    app.finish().await;
}

#[tokio::test]
async fn overlapping_devices_count_once() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let laptop = b.pair("laptop").await;
    let desktop = b.pair("desktop").await;
    let t0 = 1_773_000_000.0;

    let mut a = Chain::default();
    a.editing("acme/app", t0, t0 + 600.0);
    assert_eq!(upload(&app, &laptop, a.take()).await.status, StatusCode::OK);
    let mut d = Chain::default();
    d.editing("acme/app", t0 + 300.0, t0 + 900.0);
    assert_eq!(upload(&app, &desktop, d.take()).await.status, StatusCode::OK);

    let uid = user_id(&app, "ada").await;
    let days = day_seconds(&app, uid).await;
    let total: f64 = days.iter().map(|d| d.3).sum();
    assert!((total - 900.0).abs() < 1e-6, "{days:?}");
    let spans: f64 = {
        let c = app.st.pool.get().await.unwrap();
        c.query_one("SELECT sum(end_ts - start_ts) FROM spans WHERE user_id = $1", &[&uid]).await.unwrap().get(0)
    };
    assert!((spans - 1200.0).abs() < 1e-6, "per-device spans keep their own time: {spans}");

    assert_eq!(app.api(Method::GET, "/api/v1/users/ada/stats", None, None).await.status, StatusCode::OK);
    let csrf = b.csrf().await;
    b.post("/settings/profile", &[("csrf", &csrf), ("username", "ada"), ("timezone", "UTC")]).await;
    let stats = app.api(Method::GET, "/api/v1/users/ada/stats", None, None).await;
    assert_eq!(stats.status, StatusCode::FORBIDDEN, "private profiles stay private");
    assert_eq!(stats.json()["error"], "user has disabled public stats");
    assert_eq!(app.browser().get("/@ada").await.status, StatusCode::NOT_FOUND);
    app.finish().await;
}

#[tokio::test]
async fn days_follow_the_user_timezone() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let token = b.pair("laptop").await;
    let midnight_ist = 1_773_081_000.0;
    let mut chain = Chain::default();
    chain.editing("acme/app", midnight_ist - 600.0, midnight_ist + 600.0);
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);

    let uid = user_id(&app, "ada").await;
    let utc = day_seconds(&app, uid).await;
    assert_eq!(utc.len(), 1, "{utc:?}");
    assert_eq!((utc[0].0.as_str(), utc[0].1.as_str(), utc[0].2.as_str()), ("2026-03-09", "acme/app", "editing"));
    assert!((utc[0].3 - 1200.0).abs() < 1e-6);

    let csrf = b.csrf().await;
    let res = b.post("/settings/profile", &[("csrf", &csrf), ("username", "ada"), ("timezone", "Asia/Kolkata")]).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    let ist = day_seconds(&app, uid).await;
    assert_eq!(ist.len(), 2, "{ist:?}");
    assert_eq!((ist[0].0.as_str(), ist[1].0.as_str()), ("2026-03-09", "2026-03-10"));
    assert!((ist[0].3 - 600.0).abs() < 1e-6 && (ist[1].3 - 600.0).abs() < 1e-6, "{ist:?}");

    let mut more = Chain::default();
    more.editing("acme/app", midnight_ist + 3600.0, midnight_ist + 3900.0);
    let token2 = b.pair("desktop").await;
    assert_eq!(upload(&app, &token2, more.take()).await.status, StatusCode::OK);
    let ist = day_seconds(&app, uid).await;
    assert!((ist[1].3 - 900.0).abs() < 1e-6, "{ist:?}");
    app.finish().await;
}

#[tokio::test]
async fn public_profile_hides_hidden_projects() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let token = b.pair("laptop").await;
    let now = kairos_server::util::now().floor() - 7200.0;
    let mut chain = Chain::default();
    chain.editing("acme/secret-thing", now, now + 600.0);
    chain.editing("acme/open", now + 600.0, now + 900.0);
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);

    let csrf = b.csrf().await;
    b.post("/settings/profile", &[("csrf", &csrf), ("username", "ada"), ("timezone", "UTC"), ("is_public", "on")]).await;
    b.post("/settings/projects", &[("csrf", &csrf), ("project", "acme/secret-thing"), ("hidden", "1")]).await;

    let mut visitor = app.browser();
    let page = visitor.get("/@ada").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("acme/open") && page.body.contains("Private projects"));
    assert!(!page.body.contains("secret-thing"));
    let stats = app.api(Method::GET, "/api/v1/users/ada/stats", None, None).await;
    assert_eq!(stats.status, StatusCode::OK, "{}", stats.body);
    assert!(!stats.body.contains("secret-thing"));
    let spans = app.api(Method::GET, "/api/v1/users/ada/spans", None, None).await;
    assert_eq!(spans.status, StatusCode::FORBIDDEN);
    app.finish().await;
}

#[tokio::test]
async fn hide_all_hides_every_project_and_new_ones_until_turned_off() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let token = b.pair("laptop").await;
    let now = kairos_server::util::now().floor() - 7200.0;
    let mut chain = Chain::default();
    chain.editing("acme/first", now, now + 300.0);
    chain.editing("acme/second", now + 300.0, now + 600.0);
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);
    let csrf = b.csrf().await;
    let save = |hide: bool| {
        let mut form = vec![("csrf", csrf.as_str()), ("username", "ada"), ("timezone", "UTC"), ("is_public", "on")];
        if hide {
            form.push(("hide_projects", "on"));
        }
        form
    };

    b.post("/settings/profile", &save(true)).await;
    let page = app.browser().get("/@ada").await.body;
    assert!(!page.contains("acme/first") && !page.contains("acme/second") && page.contains("Private projects"), "{page}");
    assert!(b.get("/me").await.body.contains(r#"<span class="own">acme</span><span class="rp">/first</span>"#));

    b.post("/settings/projects", &[("csrf", &csrf), ("project", "acme/first"), ("hidden", "0")]).await;
    b.post("/settings/profile", &save(true)).await;
    let page = app.browser().get("/@ada").await.body;
    assert!(page.contains("acme/first") && !page.contains("acme/second"), "a later save keeps what was shown by hand");

    chain.editing("acme/third", now + 600.0, now + 900.0);
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);
    assert!(!app.browser().get("/@ada").await.body.contains("acme/third"), "a new project starts hidden");

    b.post("/settings/profile", &save(false)).await;
    chain.editing("acme/fourth", now + 900.0, now + 1200.0);
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);
    let page = app.browser().get("/@ada").await.body;
    assert!(page.contains("acme/fourth") && !page.contains("acme/third"), "turning it off shows new projects and leaves the rest as they are");
    app.finish().await;
}

#[tokio::test]
async fn a_device_reaches_far_behind_what_it_already_sent_only_a_few_times() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let token = b.pair("laptop").await;
    let uid = user_id(&app, "ada").await;
    let events = "SELECT count(*) FROM events e JOIN devices d ON d.id = e.device_id WHERE d.user_id = $1";
    let now = kairos_server::util::now();
    let focus = || Kind::Focus { class: Class::Editor, app: None };

    let mut chain = Chain::default();
    chain.editing("acme/app", now - 600.0, now - 300.0);
    assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK);
    for _ in 0..5 {
        chain.push(now - 14.0 * 86_400.0, Some("acme/app"), focus());
        assert_eq!(upload(&app, &token, chain.take()).await.status, StatusCode::OK, "a turn found two weeks late is ordinary");
    }
    for i in 0..3 {
        chain.push(1.0 + f64::from(i), Some("acme/app"), focus());
        let res = upload(&app, &token, chain.take()).await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }
    let before = app.count(events, uid).await;
    chain.push(5.0, Some("acme/app"), focus());
    let res = upload(&app, &token, chain.events.clone()).await;
    assert_eq!(res.status, StatusCode::TOO_MANY_REQUESTS, "{}", res.body);
    assert_eq!(app.count(events, uid).await, before, "nothing of a refused upload is kept");

    let desktop = b.pair("desktop").await;
    let mut wide = Chain::default();
    wide.push(1.0, Some("acme/app"), focus());
    wide.editing("acme/app", now - 600.0, now - 300.0);
    assert_eq!(upload(&app, &desktop, wide.take()).await.status, StatusCode::OK, "each device has its own allowance");
    for i in 0..2 {
        wide.push(2.0 + f64::from(i), Some("acme/app"), focus());
        assert_eq!(upload(&app, &desktop, wide.take()).await.status, StatusCode::OK);
    }
    wide.push(9.0, Some("acme/app"), focus());
    assert_eq!(upload(&app, &desktop, wide.take()).await.status, StatusCode::TOO_MANY_REQUESTS, "a first upload that spans decades counted too");
    app.finish().await;
}

/// The real client signing in twice before it uploaded anything, so the server can't tell it's the same computer: the
/// second sign-in gives the first token up, and the account keeps one device connected instead of two.
#[tokio::test(flavor = "multi_thread")]
async fn signing_in_twice_before_any_upload_leaves_one_device_connected() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let ada = user_id(&app, "ada").await;
    let dir = std::env::temp_dir().join(format!("kairos-relogin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let mut tokens = Vec::new();
    for _ in 0..2 {
        let url = app.url.clone();
        let pairing = tokio::task::spawn_blocking(move || Pairing::begin(&url).unwrap()).await.unwrap();
        let code = pairing.user_code().to_string();
        let page = b.get(&format!("/device?code={code}")).await;
        assert!(b.post("/device/approve", &[("code", &code), ("csrf", &page.csrf())]).await.body.contains("Computer connected"));
        let d = dir.clone();
        tokens.push(tokio::task::spawn_blocking(move || pairing.wait(&d).unwrap().token).await.unwrap());
    }

    let connected = "SELECT count(*) FROM devices WHERE user_id = $1 AND revoked_at IS NULL";
    assert_eq!(app.count(connected, ada).await, 1);
    assert_eq!(account::load(&dir).map(|a| a.token).as_ref(), Some(&tokens[1]));
    assert_eq!(me(&app, &tokens[1]).await.status, StatusCode::OK);
    assert_eq!(me(&app, &tokens[0]).await.status, StatusCode::UNAUTHORIZED);
    let _ = std::fs::remove_dir_all(&dir);
    app.finish().await;
}
