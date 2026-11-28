mod common;

use std::sync::atomic::{AtomicU32, Ordering};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use common::{Chain, TestApp};
use serde_json::json;

type Row = (Option<i64>, Option<String>, Option<String>, Option<String>, String, Option<f64>);

async fn rows(app: &TestApp) -> Vec<Row> {
    let c = app.st.pool.get().await.unwrap();
    c.query("SELECT asn, provider, country, ip_hash, fingerprint, clock_skew FROM device_networks ORDER BY since", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5)))
        .collect()
}

async fn send(app: &TestApp, token: &str, chain: &mut Chain, from: &str, agent: &str, clock: Option<f64>) -> StatusCode {
    static STEP: AtomicU32 = AtomicU32::new(0);
    let at = kairos_server::util::now().floor() - 3600.0 + 60.0 * f64::from(STEP.fetch_add(1, Ordering::Relaxed));
    chain.editing("acme/site", at, at + 30.0);
    let mut b = Request::post("/api/v1/events")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::USER_AGENT, agent)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-forwarded-for", from);
    if let Some(t) = clock {
        b = b.header("kairos-clock", t.to_string());
    }
    app.send(b.body(Body::from(json!({ "events": chain.take() }).to_string())).unwrap()).await.status
}

#[tokio::test]
async fn an_upload_records_where_it_came_from_once_per_change() {
    let table = std::env::temp_dir().join(format!("ip2asn-{}.tsv", std::process::id()));
    std::fs::write(&table, "41.32.0.0\t41.47.255.255\t8452\tEG\tTE-AS\n5.9.0.0\t5.9.255.255\t24940\tDE\tHETZNER-AS\n").unwrap();
    let app = TestApp::with(&[
        ("KAIROS_IP2ASN", table.to_str().unwrap()),
        ("KAIROS_SERVER_KEY", "a test key that is long enough to use"),
        ("KAIROS_TRUSTED_PROXY", "10.0.0.0/8"),
    ])
    .await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let token = b.pair("laptop").await;
    let mut chain = Chain::default();

    assert_eq!(send(&app, &token, &mut chain, "41.33.1.2", "kairosd/0.1.27", None).await, StatusCode::OK);
    assert_eq!(send(&app, &token, &mut chain, "41.33.1.2", "kairosd/0.1.28", None).await, StatusCode::OK);
    let first = rows(&app).await;
    assert_eq!(first.len(), 1, "the same network and an update to the client are one row: {first:?}");
    let (asn, provider, country, ip_hash, _, skew) = &first[0];
    assert_eq!((*asn, provider.as_deref(), country.as_deref(), *skew), (Some(8452), Some("TE-AS"), Some("EG"), None));
    let ip_hash = ip_hash.clone().unwrap();
    assert_eq!(ip_hash.len(), 16);

    send(&app, &token, &mut chain, "5.9.1.1", "kairosd/0.1.28", None).await;
    send(&app, &token, &mut chain, "41.33.1.2", "python-requests/2.32.3", None).await;
    let clock = kairos_server::util::now() - 2.0 * 86_400.0;
    send(&app, &token, &mut chain, "41.33.1.2", "python-requests/2.32.3", Some(clock)).await;
    send(&app, &token, &mut chain, "41.33.1.2", "python-requests/2.32.3", Some(clock + 30.0)).await;
    let all = rows(&app).await;
    assert_eq!(all.len(), 4, "{all:?}");
    assert_eq!((all[1].0, all[1].2.as_deref()), (Some(24940), Some("DE")));
    assert_ne!(all[1].3.as_deref(), Some(ip_hash.as_str()));
    assert_eq!(all[2].3.as_deref(), Some(ip_hash.as_str()), "back home is the same hash");
    assert_ne!(all[2].4, all[0].4, "another program sending makes another fingerprint");
    assert!(all[3].5.is_some_and(|s| (s - 2.0 * 86_400.0).abs() < 60.0), "a clock two days behind: {:?}", all[3].5);

    let c = app.st.pool.get().await.unwrap();
    let stored: i64 = c
        .query_one("SELECT count(*) FROM device_networks WHERE provider LIKE '%41.33%' OR ip_hash LIKE '%41.33%' OR fingerprint LIKE '%41.33%'", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(stored, 0, "the address itself is never stored");
    drop(c);
    std::fs::remove_file(&table).ok();
    app.finish().await;
}

#[tokio::test]
async fn without_a_table_or_key_uploads_still_work() {
    let app = TestApp::new().await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let token = b.pair("laptop").await;
    let mut chain = Chain::default();
    assert_eq!(send(&app, &token, &mut chain, "41.33.1.2", "kairosd/0.1.27", None).await, StatusCode::OK);
    let r = rows(&app).await;
    assert_eq!(r.len(), 1);
    assert_eq!((r[0].0, r[0].1.as_deref(), r[0].2.as_deref(), r[0].3.as_deref()), (None, None, None, None));
    app.finish().await;
}

#[tokio::test]
async fn an_upload_hands_back_a_receipt_and_one_sealed_into_the_log_is_checked() {
    let app = TestApp::with(&[("KAIROS_SERVER_KEY", "a test key that is long enough to use")]).await;
    let mut b = app.browser();
    b.sign_in("Ada Lovelace", "ada@example.com").await;
    let token = b.pair("laptop").await;
    let other = b.pair("desktop").await;
    let mut chain = Chain::default();
    let post = |token: &str, events: Vec<serde_json::Value>| {
        Request::post("/api/v1/events")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({ "events": events }).to_string()))
            .unwrap()
    };

    let before = kairos_server::util::now();
    let res = app.send(post(&token, { chain.editing("acme/site", before - 600.0, before - 540.0); chain.take() })).await;
    assert_eq!(res.status, StatusCode::OK);
    let receipt = res.headers.get("kairos-receipt").unwrap().to_str().unwrap().to_string();
    let (at, mac) = receipt.split_once('.').map(|(a, m)| (a.to_string(), m.to_string())).unwrap();
    assert!((at.parse::<f64>().unwrap() - before).abs() < 5.0 && mac.len() == 64, "{receipt}");

    let forged = format!("{}.{mac}", at.parse::<i64>().unwrap() - 86_400);
    chain.push(before - 30.0, None, kairos_core::event::Kind::Receipt { token: receipt.clone() });
    chain.push(before - 20.0, None, kairos_core::event::Kind::Receipt { token: forged });
    assert_eq!(app.send(post(&token, chain.take())).await.status, StatusCode::OK);
    let mut desk = Chain::default();
    desk.push(before - 10.0, None, kairos_core::event::Kind::Receipt { token: receipt });
    assert_eq!(app.send(post(&other, desk.take())).await.status, StatusCode::OK);

    let c = app.st.pool.get().await.unwrap();
    let got: Vec<(String, i64, Option<bool>)> = c
        .query("SELECT d.name, r.server_ts, r.valid FROM receipts r JOIN devices d ON d.id = r.device_id ORDER BY r.ts", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    let at: i64 = at.parse().unwrap();
    assert_eq!(
        got,
        [("laptop".to_string(), at, Some(true)), ("laptop".to_string(), at - 86_400, Some(false)), ("desktop".to_string(), at, Some(false))],
        "a moved time and another device's receipt don't check out"
    );
    app.finish().await;
}
