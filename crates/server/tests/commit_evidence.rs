mod common;

use axum::http::StatusCode;
use common::{upload, user_id, Browser, Chain, TestApp};
use kairos_core::event::{Class, FileChange, Kind, Via, GENESIS, MAX_CHANGES, MAX_PATH_LEN};
use kairos_server::accounts;
use serde_json::{json, Value};

// 2026-06-01T00:00:00Z, a Monday.
const D: f64 = 1_780_272_000.0;
const H: f64 = 3600.0;
const DAY: f64 = 86_400.0;

fn sha(c: char) -> String {
    c.to_string().repeat(40)
}

fn canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                canonical(&m[k], out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(x, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// Sealed the way the daemon seals its own, so values it would never write (wrong types, made-up `via`)
/// still pass the hash check.
fn raw_commit(events: &mut Vec<Value>, ts: f64, sha: &str, extra: Value) {
    let mut body = json!({ "v": 1, "ts": ts, "project": "ada/site", "kind": "commit", "sha": sha, "files": 3, "added": 10, "removed": 2 });
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    raw_event(events, body);
}

fn raw_event(events: &mut Vec<Value>, mut body: Value) {
    let prev = events.last().and_then(|e| e["hash"].as_str()).unwrap_or(GENESIS).to_string();
    body["prev"] = json!(prev);
    let mut text = String::new();
    canonical(&body, &mut text);
    let mut h = blake3::Hasher::new();
    h.update(prev.as_bytes());
    h.update(text.as_bytes());
    body["hash"] = json!(h.finalize().to_hex().to_string());
    events.push(body);
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

type Stored = (String, Option<i64>, Option<i64>, Option<i64>, Option<String>, Option<Value>, bool);

async fn stored(app: &TestApp, uid: i64) -> Vec<Stored> {
    let c = app.st.pool.get().await.unwrap();
    c.query(
        "SELECT sha, device_id, authored_at, committed_at, via, changes, received_at IS NOT NULL FROM commits WHERE user_id = $1 ORDER BY sha",
        &[&uid],
    )
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5), r.get(6)))
    .collect()
}

async fn device_id(app: &TestApp, uid: i64) -> i64 {
    let c = app.st.pool.get().await.unwrap();
    c.query_one("SELECT id FROM devices WHERE user_id = $1 ORDER BY id LIMIT 1", &[&uid]).await.unwrap().get(0)
}

fn row<'a>(rows: &'a [Stored], c: char) -> &'a Stored {
    rows.iter().find(|r| r.0 == sha(c)).unwrap_or_else(|| panic!("no commit {c}"))
}

fn start(chain: &mut Chain) -> Vec<Value> {
    chain.push(D + H, None, Kind::Focus { class: Class::Editor, app: None });
    chain.take()
}

#[tokio::test]
async fn ingest_stores_the_device_dates_via_and_changes() {
    let app = TestApp::new().await;
    let mut ada = person(&app, "Ada", "user").await;
    let token = ada.pair("ada-laptop").await;
    let uid = user_id(&app, "ada").await;
    let mut events = start(&mut Chain::default());
    let t = D + 2.0 * H;
    let future = kairos_server::util::now() + 3.0 * DAY;

    raw_commit(
        &mut events,
        t,
        &sha('a'),
        json!({
            "authored": D as i64, "committed": D as i64 + 60, "via": "amend", "not_before": t - 600.0,
            "changes": [{ "path": "src/main.rs", "added": 10, "removed": 2 }, { "path": "README.md", "added": 0, "removed": 0 }],
        }),
    );
    let long = "p".repeat(MAX_PATH_LEN);
    raw_commit(
        &mut events,
        t + 1.0,
        &sha('b'),
        json!({
            "authored": future as i64, "committed": 1.5, "via": "bogus", "not_before": t + 2.0,
            "changes": [
                { "path": "a\u{7}b\u{1b}.rs\n", "added": 4_294_967_295u64, "removed": 1 },
                { "path": "", "added": 1, "removed": 1 },
                { "path": "\u{7}", "added": 1, "removed": 1 },
                { "path": "p".repeat(MAX_PATH_LEN + 1), "added": 1, "removed": 1 },
                { "path": 5, "added": 1, "removed": 1 },
                { "path": "negative.rs", "added": -1, "removed": 0 },
                { "path": "fractional.rs", "added": 1.5, "removed": 0 },
                { "path": "no-counts.rs" },
                "not an object",
                { "path": long.clone(), "added": 7, "removed": 8 },
                { "path": "ok.rs", "added": 1, "removed": 2 },
            ],
        }),
    );
    raw_commit(&mut events, t + 2.0, &sha('c'), json!({ "authored": "yesterday", "committed": 0, "via": 7, "changes": "everything", "not_before": "earlier" }));
    let many: Vec<Value> = (0..MAX_CHANGES + 5).map(|i| json!({ "path": format!("f{i}.rs"), "added": i, "removed": 0 })).collect();
    raw_commit(&mut events, t + 3.0, &sha('d'), json!({ "via": "unknown", "committed": D as i64, "changes": many }));
    raw_commit(&mut events, t + 4.0, &sha('e'), json!({ "changes": [{ "path": "", "added": 1, "removed": 1 }] }));
    raw_commit(&mut events, t + 5.0, &sha('f'), json!({ "authored": -5, "committed": future as i64 - 2 * DAY as i64, "via": "cherry_pick", "changes": [] }));
    let r = upload(&app, &token, events).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);

    let rows = stored(&app, uid).await;
    assert_eq!(rows.len(), 6);
    let dev = device_id(&app, uid).await;
    assert!(rows.iter().all(|r| r.1 == Some(dev) && r.6), "every commit keeps its device and when it arrived");

    let a = row(&rows, 'a');
    assert_eq!((a.2, a.3, a.4.as_deref()), (Some(D as i64), Some(D as i64 + 60), Some("amend")));
    assert_eq!(a.5, Some(json!([{ "path": "src/main.rs", "added": 10, "removed": 2 }, { "path": "README.md", "added": 0, "removed": 0 }])));

    let b = row(&rows, 'b');
    assert_eq!((b.2, b.3, b.4.as_deref()), (None, None, None), "a future date, a fraction and an unknown via are dropped");
    assert_eq!(
        b.5,
        Some(json!([
            { "path": "ab.rs", "added": i32::MAX, "removed": 1 },
            { "path": long, "added": 7, "removed": 8 },
            { "path": "ok.rs", "added": 1, "removed": 2 },
        ]))
    );

    let c = row(&rows, 'c');
    assert_eq!((c.2, c.3, c.4.clone(), c.5.clone()), (None, None, None, None), "wrong types are dropped");

    let d = row(&rows, 'd');
    assert_eq!((d.3, d.4.as_deref()), (Some(D as i64), Some("unknown")));
    let kept = d.5.as_ref().unwrap().as_array().unwrap();
    assert_eq!(kept.len(), MAX_CHANGES);
    assert_eq!(kept[MAX_CHANGES - 1]["path"], format!("f{}.rs", MAX_CHANGES - 1));

    assert_eq!(row(&rows, 'e').5, None, "no valid entry means no changes");
    let f = row(&rows, 'f');
    assert_eq!((f.2, f.3, f.4.as_deref(), f.5.clone()), (None, Some(future as i64 - 2 * DAY as i64), Some("cherry_pick"), None));

    let db = app.st.pool.get().await.unwrap();
    let spans: Vec<(String, f64)> = db
        .query("SELECT sha, not_before FROM commits WHERE user_id = $1 AND not_before IS NOT NULL", &[&uid])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(spans, [(sha('a'), t - 600.0)], "a time after the device saw the commit, or not a number, is dropped");
    app.finish().await;
}

#[tokio::test]
async fn an_oversized_event_is_refused_before_anything_is_stored() {
    let app = TestApp::new().await;
    let mut ada = person(&app, "Ada", "user").await;
    let token = ada.pair("ada-laptop").await;
    let uid = user_id(&app, "ada").await;
    let mut events = start(&mut Chain::default());
    raw_commit(&mut events, D + 2.0 * H, &sha('a'), json!({}));
    raw_commit(&mut events, D + 2.0 * H + 1.0, &sha('b'), json!({ "pad": "x".repeat(300 * 1024) }));

    let r = upload(&app, &token, events.clone()).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!((r.json()["error"].as_str(), r.json()["index"].as_u64()), (Some("too_big"), Some(2)));
    assert_eq!(app.count("SELECT count(*) FROM events e JOIN devices d ON d.id = e.device_id WHERE d.user_id = $1", uid).await, 0);
    assert_eq!(app.count("SELECT count(*) FROM commits WHERE user_id = $1", uid).await, 0);

    events.truncate(2);
    assert_eq!(upload(&app, &token, events).await.status, StatusCode::OK, "the device can still upload the same chain without it");
    assert_eq!(app.count("SELECT count(*) FROM commits WHERE user_id = $1", uid).await, 1);
    app.finish().await;
}

#[tokio::test]
async fn the_review_page_tags_how_a_commit_arrived_and_lists_its_files() {
    let app = TestApp::new().await;
    let mut ada = person(&app, "Ada", "user").await;
    app.exec("UPDATE users SET timezone = 'Asia/Kolkata' WHERE username = 'ada'").await;
    let token = ada.pair("ada-laptop").await;
    let mut events = start(&mut Chain::default());
    let t = D + 10.0 * H;
    let file = |p: &str| json!({ "path": p, "added": 3, "removed": 1 });

    let mut many = vec![file("src/<script>alert(1)</script>.js")];
    many.extend((0..59).map(|i| file(&format!("src/f{i}.rs"))));
    raw_commit(&mut events, t, &sha('a'), json!({ "via": "amend", "committed": D as i64 + 3 * DAY as i64 + 13 * 3600 + 420, "files": 70, "changes": many, "not_before": t }));
    raw_commit(&mut events, t + 60.0, &sha('b'), json!({ "via": "commit", "committed": t as i64 + 1800, "not_before": t + 60.0 }));
    raw_commit(&mut events, t + 120.0, &sha('c'), json!({ "via": "rebase", "committed": t as i64 + 120 - 3601, "not_before": t + 120.0 }));
    raw_commit(&mut events, t + 180.0, &sha('d'), json!({ "via": "arrived", "committed": t as i64 - 5 * DAY as i64, "not_before": t + 180.0 }));
    raw_commit(&mut events, t + 240.0, &sha('e'), json!({ "via": "cherry_pick", "committed": t as i64 + 240, "files": 2, "changes": [file("a.rs"), file("b.rs")], "not_before": t + 240.0 }));
    raw_commit(&mut events, t + 300.0, &sha('f'), json!({ "via": "revert", "committed": t as i64 + 300 + 3601, "not_before": t + 300.0 }));
    raw_commit(&mut events, t + 360.0, &sha('1'), json!({ "via": "unknown", "committed": t as i64 - 5 * DAY as i64, "not_before": t + 360.0 }));
    raw_commit(&mut events, t + 420.0, &sha('2'), json!({}));
    assert_eq!(upload(&app, &token, events).await.status, StatusCode::OK);

    let mut rev = person(&app, "Rev", "reviewer").await;
    let page = rev.get("/review/@ada?from=2026-06-01&to=2026-06-01").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let body = &page.body;
    let section = body.split("<section id=\"commits\">").nth(1).unwrap().split("</section>").next().unwrap();

    for text in ["<span class=\"tag\">amended</span>", "<span class=\"tag\">rebased</span>", "<span class=\"tag\">cherry-picked</span>", "<span class=\"tag\">reverted</span>", "<span class=\"tag\">made elsewhere</span>"] {
        assert_eq!(section.matches(text).count(), 1, "{text}");
    }
    assert!(!section.contains(">commit</span>") && !section.contains(">unknown</span>"));
    assert_eq!(section.matches("<span class=\"tag\">").count(), 10, "five via tags and five git dates");
    assert!(section.contains("<span class=\"tag\">git date Jun 4, 2026 18:37</span>"), "in the person's timezone");
    assert!(section.contains("<span class=\"tag\">git date Jun 1, 2026 14:31</span>"));
    assert_eq!(section.matches("git date ").count(), 5, "an hour is the line, and a missing date shows nothing");
    assert!(section.contains("Mon Jun 1 15:30 · ada/site"), "the time stays when the device saw the commit");

    assert_eq!(section.matches("<details class=\"files\">").count(), 2, "only commits with a file list have one");
    assert_eq!(section.matches("<span class=\"path\">").count(), 52);
    assert!(section.contains("<span class=\"path\">src/&#60;script&#62;alert(1)&#60;/script&#62;.js</span>"), "{section}");
    assert!(!body.contains("<script>alert"));
    assert!(section.contains("<p class=\"muted small\">20 more files</p>"), "70 files, 50 listed");
    assert_eq!(section.matches("more files").count(), 1);
    app.finish().await;
}

#[tokio::test]
async fn the_commit_dates_signal_needs_a_local_commit_dated_outside_when_it_was_made() {
    let app = TestApp::new().await;
    let mut ada = person(&app, "Ada", "user").await;
    let laptop = ada.pair("ada-laptop").await;
    let mut c = Chain::default();
    c.editing("ada/site", D + 9.0 * H, D + 10.0 * H);
    let mut events = c.take();
    let t = D + 10.0 * H;
    let at = |mins: f64| t + mins * 60.0;
    let dated = |mins: f64, via: &str, off: i64| json!({ "via": via, "committed": at(mins) as i64 + off, "not_before": at(mins) - 30.0 });

    raw_commit(&mut events, at(5.0), &sha('a'), dated(5.0, "commit", -2 * 3600));
    raw_commit(&mut events, at(6.0), &sha('b'), dated(6.0, "amend", 660));
    raw_commit(&mut events, at(7.0), &sha('c'), dated(7.0, "commit", -1800));
    raw_commit(&mut events, at(7.5), &sha('1'), dated(7.5, "commit", 540));
    raw_commit(&mut events, at(8.0), &sha('d'), dated(8.0, "arrived", -5 * 86_400));
    raw_commit(&mut events, at(9.0), &sha('e'), dated(9.0, "rebase", -5 * 86_400));
    raw_commit(&mut events, at(10.0), &sha('2'), json!({ "committed": at(10.0) as i64 - 5 * 86_400, "not_before": at(9.5) }));
    raw_commit(&mut events, at(11.0), &sha('4'), json!({ "via": "commit", "committed": at(11.0) as i64, "authored": at(11.0) as i64 - 3 * 3600, "not_before": at(10.5) }));
    raw_commit(&mut events, at(12.0), &sha('5'), json!({ "via": "amend", "committed": at(12.0) as i64, "authored": at(12.0) as i64 - 3 * 3600, "not_before": at(11.5) }));
    // Found when kairosd came back: made hours before it was seen, but after the device last looked.
    raw_commit(&mut events, at(45.0), &sha('f'), json!({ "via": "commit", "committed": at(45.0) as i64 - 5 * 3600, "not_before": at(45.0) - 7.0 * H }));
    raw_commit(&mut events, at(50.0), &sha('6'), json!({ "via": "commit", "committed": at(50.0) as i64 - 4 * 3600, "not_before": at(50.0) - 2.0 * H }));
    raw_commit(&mut events, at(55.0), &sha('7'), json!({ "via": "commit", "committed": at(55.0) as i64 - 5 * 86_400 }));
    raw_commit(&mut events, at(58.0), &sha('8'), json!({ "via": "commit", "committed": D as i64 - 5 * 3600, "not_before": D - 2.0 * H }));
    assert_eq!(upload(&app, &laptop, events).await.status, StatusCode::OK);

    let mut rev = person(&app, "Rev", "reviewer").await;
    let page = rev.get("/review/@ada?from=2026-06-01&to=2026-06-01").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let signal = page.body.split("id=\"sig-commit_dates\"").nth(1).expect("the signal is shown").split("</ul>").next().unwrap();
    assert!(signal.contains("notice"), "{signal}");
    assert!(signal.contains("aaaaaaa in ada/site, not there at 10:04, seen 10:05, git date Jun 1, 2026 08:05"), "{signal}");
    assert!(signal.contains("bbbbbbb in ada/site, not there at 10:05, seen 10:06, git date Jun 1, 2026 10:17"), "{signal}");
    assert!(signal.contains("href=\"#d-2026-06-01\""));
    assert!(signal.contains("4444444 in ada/site, not there at 10:10, seen 10:11, git date Jun 1, 2026 07:11"), "--date moves only the author date: {signal}");
    assert!(signal.contains("6666666 in ada/site, not there at 08:50, seen 10:50, git date Jun 1, 2026 06:50"), "{signal}");
    assert!(signal.contains("8888888 in ada/site, not there at May 31 22:00, seen 10:58, git date May 31, 2026 19:00"), "{signal}");
    assert_eq!(signal.matches("<li>").count(), 5, "{signal}");
    for quiet in ["ccccccc", "1111111", "ddddddd", "eeeeeee", "2222222", "5555555", "fffffff", "7777777"] {
        assert!(!signal.contains(quiet), "{quiet} should not count: {signal}");
    }

    let section = page.body.split("<section id=\"commits\">").nth(1).unwrap().split("</section>").next().unwrap();
    assert!(item(section, '6').contains("git date "), "{section}");
    assert!(!item(section, 'f').contains("git date "), "made in its span, however late it was seen: {section}");
    assert!(!item(section, '7').contains("git date "), "no span, nothing to compare: {section}");
    app.finish().await;
}

#[tokio::test]
async fn an_event_with_a_nul_is_refused_with_its_index() {
    let app = TestApp::new().await;
    let mut ada = person(&app, "Ada", "user").await;
    let token = ada.pair("ada-laptop").await;
    let uid = user_id(&app, "ada").await;
    let mut events = start(&mut Chain::default());
    raw_commit(&mut events, D + 2.0 * H, &sha('a'), json!({ "changes": [{ "path": "a\u{0}b", "added": 1, "removed": 0 }] }));
    let r = upload(&app, &token, events).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!((r.json()["error"].as_str(), r.json()["index"].as_u64()), (Some("bad_text"), Some(1)));
    assert_eq!(app.count("SELECT count(*) FROM events e JOIN devices d ON d.id = e.device_id WHERE d.user_id = $1", uid).await, 0);
    app.finish().await;
}

#[tokio::test]
async fn the_device_a_commit_was_made_on_replaces_a_pulled_sighting() {
    let app = TestApp::new().await;
    let mut ada = person(&app, "Ada", "user").await;
    let desktop = ada.pair("ada-desktop").await;
    let laptop = ada.pair("ada-laptop").await;
    let uid = user_id(&app, "ada").await;
    let made = D + 2.0 * H;

    let mut pulled = start(&mut Chain::default());
    raw_commit(&mut pulled, made + 600.0, &sha('a'), json!({ "via": "arrived", "committed": made as i64 - 5 }));
    raw_commit(&mut pulled, made + 601.0, &sha('b'), json!({ "via": "commit", "committed": made as i64 }));
    assert_eq!(upload(&app, &desktop, pulled).await.status, StatusCode::OK);

    let mut own = start(&mut Chain::default());
    raw_commit(&mut own, made, &sha('a'), json!({ "via": "commit", "committed": made as i64 - 5, "changes": [{ "path": "x.rs", "added": 1, "removed": 0 }] }));
    raw_commit(&mut own, made + 1.0, &sha('b'), json!({ "via": "arrived", "committed": made as i64 }));
    assert_eq!(upload(&app, &laptop, own).await.status, StatusCode::OK);

    let c = app.st.pool.get().await.unwrap();
    let laptop_id: i64 = c.query_one("SELECT id FROM devices WHERE user_id = $1 AND name = 'ada-laptop'", &[&uid]).await.unwrap().get(0);
    let rows = c.query("SELECT sha, ts, via, device_id, changes IS NOT NULL FROM commits WHERE user_id = $1 ORDER BY sha", &[&uid]).await.unwrap();
    let a = &rows[0];
    assert_eq!((a.get::<_, f64>(1), a.get::<_, &str>(2), a.get::<_, i64>(3), a.get::<_, bool>(4)), (made, "commit", laptop_id, true));
    let b = &rows[1];
    assert_eq!((b.get::<_, f64>(1), b.get::<_, &str>(2)), (made + 601.0, "commit"), "a local sighting is never replaced by a pulled one");
    app.finish().await;
}

fn edit(chain: &mut Chain, ts: f64, project: &str, harness: &str, path: &str, added: u32, removed: u32) {
    chain.push_from(ts, Some(project), harness, Kind::AgentEdit { session: "s1".into(), path: path.into(), added, removed });
}

fn commit(chain: &mut Chain, ts: f64, c: char, via: Via, changes: &[(&str, u32, u32)]) {
    let changes: Vec<FileChange> = changes.iter().map(|&(p, a, r)| FileChange { path: p.into(), added: a, removed: r }).collect();
    let (added, removed) = changes.iter().fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
    let kind = Kind::Commit { sha: sha(c), files: changes.len() as u32, added, removed, authored: None, committed: Some(ts as i64), via: Some(via), changes, replaces: None, not_before: None };
    chain.push(ts, Some("ada/site"), kind);
}

fn item<'a>(section: &'a str, c: char) -> &'a str {
    let short: String = sha(c).chars().take(7).collect();
    let at = section.find(&format!("<code>{short}</code>")).unwrap_or_else(|| panic!("no commit {c}"));
    let end = section[at + 1..].find("<code>").map_or(section.len(), |e| at + 1 + e);
    &section[at..end]
}

#[tokio::test]
async fn the_review_page_shows_what_agent_edits_wrote_into_each_commit() {
    let app = TestApp::new().await;
    let mut ada = person(&app, "Ada", "user").await;
    let laptop = ada.pair("ada-laptop").await;
    let desktop = ada.pair("ada-desktop").await;
    let t = D + 10.0 * H;

    let mut chain = Chain::default();
    chain.push(t - 900.0, Some("ada/site"), Kind::Focus { class: Class::Editor, app: None });
    edit(&mut chain, t - 600.0, "ada/site", "claude-code", "src/main.rs", 12, 3);
    edit(&mut chain, t - 500.0, "ada/site", "codex", "src/lib.rs", 4, 0);
    edit(&mut chain, t - 450.0, "ada/other", "codex", "src/main.rs", 50, 50);
    commit(&mut chain, t, 'a', Via::Commit, &[("src/main.rs", 10, 3), ("src/lib.rs", 4, 1), ("Cargo.lock", 40, 2)]);
    edit(&mut chain, t + 100.0, "ada/site", "claude-code", "src/main.rs", 5, 5);
    chain.push_from(t + 195.0, Some("ada/site"), "claude-code", Kind::AgentCommit { session: "s1".into(), sha: None });
    commit(&mut chain, t + 200.0, 'b', Via::Commit, &[("src/main.rs", 5, 5)]);
    edit(&mut chain, t + 220.0, "ada/site", "claude-code", "src/b.rs", 2, 0);
    commit(&mut chain, t + 240.0, 'c', Via::Amend, &[("src/main.rs", 5, 5), ("src/b.rs", 2, 0)]);
    edit(&mut chain, t + 280.0, "ada/site", "codex", "src/x.rs", 9, 0);
    chain.push_from(t + 290.0, Some("ada/site"), "codex", Kind::AgentCommit { session: "s2".into(), sha: Some(sha('d')[..9].to_string()) });
    commit(&mut chain, t + 300.0, 'd', Via::Arrived, &[("src/x.rs", 9, 0)]);
    assert_eq!(upload(&app, &laptop, chain.take()).await.status, StatusCode::OK);

    let mut other = Chain::default();
    edit(&mut other, t - 300.0, "ada/site", "claude-code", "src/lib.rs", 0, 1);
    assert_eq!(upload(&app, &desktop, other.take()).await.status, StatusCode::OK);

    let mut rev = person(&app, "Rev", "reviewer").await;
    let page = rev.get("/review/@ada?from=2026-06-01&to=2026-06-01").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let section = page.body.split("<section id=\"commits\">").nth(1).unwrap().split("</section>").next().unwrap();

    let a = item(section, 'a');
    assert!(a.contains("<span class=\"ag\">agent edits +14 -3, Claude Code, Codex</span>"), "capped per file, other projects and devices left out: {a}");
    assert!(a.contains("<span class=\"ag\">agent +10 -3</span> <b class=\"add\">+10</b>"), "{a}");
    assert!(a.contains("<span class=\"ag\">agent +4 -0</span> <b class=\"add\">+4</b>"), "{a}");
    assert_eq!(a.matches("<span class=\"ag\">").count(), 3, "Cargo.lock came from no agent edit");
    assert!(!a.contains("made by"));

    let b = item(section, 'b');
    assert!(b.contains("<span class=\"ag\">agent edits +5 -5, Claude Code</span>"), "only edits since the previous commit: {b}");
    assert!(b.contains("<span class=\"tag made\">made by Claude Code</span>"), "the quiet agent commit is the one seen 5 s later: {b}");

    let c = item(section, 'c');
    assert!(c.contains("<span class=\"ag\">agent edits +7 -5, Claude Code</span>"), "an amend counts from before the commit it amended: {c}");
    assert!(!c.contains("made by"));

    let d = item(section, 'd');
    assert!(!d.contains("class=\"ag\""), "a pulled commit isn't compared: {d}");
    assert!(d.contains("made by Codex"), "an agent's own sha still marks it: {d}");

    assert!(section.contains("<span>From agent edits</span>") && section.contains("<b>39%</b>"), "26 of 66: {section}");
    assert!(section.contains("<p>of lines added in 3 commits from their devices.</p>"), "{section}");
    app.finish().await;
}

#[tokio::test]
async fn malformed_agent_events_count_nothing_and_break_nothing() {
    let app = TestApp::new().await;
    let mut ada = person(&app, "Ada", "user").await;
    let token = ada.pair("ada-laptop").await;
    let t = D + 10.0 * H;
    let mut events = start(&mut Chain::default());
    let agent_edit = |ts: f64, extra: Value| {
        let mut body = json!({ "v": 1, "ts": ts, "project": "ada/site", "kind": "agent_edit", "session": "s" });
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        body
    };
    for (i, bad) in [
        json!({ "path": "a.rs", "added": "12", "removed": 1 }),
        json!({ "path": 7, "added": 3, "removed": 0 }),
        json!({ "path": "a.rs", "added": -40, "removed": 1e30 }),
        json!({ "path": "a.rs", "added": 2.6, "removed": [1] }),
    ]
    .into_iter()
    .enumerate()
    {
        raw_event(&mut events, agent_edit(t - 100.0 + i as f64, bad));
    }
    raw_event(&mut events, json!({ "v": 1, "ts": t - 10.0, "project": "ada/site", "kind": "agent_commit", "session": "s", "sha": 5 }));
    raw_event(&mut events, json!({ "v": 1, "ts": t - 9.0, "project": "ada/site", "kind": "agent_commit", "session": "s", "sha": "%" }));
    raw_commit(&mut events, t, &sha('a'), json!({ "via": "commit", "files": 1, "added": 10, "removed": 9, "changes": [{ "path": "a.rs", "added": 10, "removed": 9 }] }));
    assert_eq!(upload(&app, &token, events).await.status, StatusCode::OK);

    let mut rev = person(&app, "Rev", "reviewer").await;
    let page = rev.get("/review/@ada?from=2026-06-01&to=2026-06-01").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let section = page.body.split("<section id=\"commits\">").nth(1).unwrap().split("</section>").next().unwrap();
    assert!(section.contains("<span class=\"ag\">agent edits +3 -9</span>"), "only numbers count, clamped to the commit: {section}");
    assert!(!section.contains("made by"), "a sha that isn't one marks nothing, and isn't a quiet commit either");
    app.finish().await;
}
