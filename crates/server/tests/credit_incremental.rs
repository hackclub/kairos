mod common;
#[path = "../../core/tests/propgen/mod.rs"]
mod propgen;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{upload, user_id, TestApp};
use kairos_core::credit::{agent_turns, Rule};
use kairos_core::event::{Class, Event, Kind, GENESIS, VERSION};
use kairos_core::union;
use kairos_server::credit::rebuild_user;
use propgen::{generate, measure, minus, norm, run_cases_from, Iv, Profile, Rng, BASE};
use serde_json::Value;

const ZONES: [&str; 4] = ["UTC", "Asia/Kolkata", "America/Los_Angeles", "Pacific/Auckland"];
const TOL: f64 = 1e-3;

static USERS: AtomicUsize = AtomicUsize::new(0);

type Row = (f64, f64, Option<String>, String);

const REALISTIC: Profile = Profile {
    events: 140,
    gaps_of_days: false,
    crashed_turns: false,
    long_turns: false,
    projectless_replies: false,
    queued_heartbeats: false,
    long_pauses: false,
    hostile: false,
    wild: false,
};

fn sealed(es: &[Event], prev: &mut String) -> Vec<Value> {
    es.iter()
        .map(|e| {
            let mut e = e.clone();
            e.v = VERSION;
            e.seal(prev);
            *prev = e.hash.clone();
            serde_json::to_value(&e).unwrap()
        })
        .collect()
}

// Where batches start depends on the events only, so shrinking an event list keeps the cuts it still has.
fn starts_batch(e: &Event, div: u64) -> bool {
    let text = format!("{}{:?}", e.ts.to_bits(), e.kind);
    text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)) % div == 0
}

fn coalesce(mut rows: Vec<Row>) -> Vec<Row> {
    rows.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<Row> = Vec::new();
    for r in rows {
        match out.last_mut() {
            Some(l) if l.2 == r.2 && l.3 == r.3 && r.0 - l.1 < 1e-6 => l.1 = l.1.max(r.1),
            _ => out.push(r),
        }
    }
    out
}

fn diff(what: &str, a_name: &str, a: &[Row], b_name: &str, b: &[Row]) -> Option<String> {
    let keys = |rows: &[Row]| rows.iter().map(|r| (r.2.clone(), r.3.clone())).collect::<std::collections::BTreeSet<_>>();
    let mut all = keys(a);
    all.extend(keys(b));
    let pick = |rows: &[Row], k: &(Option<String>, String)| norm(rows.iter().filter(|r| (&r.2, &r.3) == (&k.0, &k.1)).map(|r| (r.0, r.1)).collect());
    let at = |t: f64| format!("+{:.3}", t - BASE);
    for k in all {
        let (ia, ib) = (pick(a, &k), pick(b, &k));
        let (only_a, only_b) = (minus(&ia, &ib), minus(&ib, &ia));
        if measure(&only_a) + measure(&only_b) > TOL {
            let show = |v: &[Iv]| v.iter().take(3).map(|iv| format!("[{}, {}]", at(iv.0), at(iv.1))).collect::<Vec<_>>().join(" ");
            return Some(format!(
                "{what} {:?}/{}: only in {a_name}: {:.3} s {}; only in {b_name}: {:.3} s {}",
                k.0,
                k.1,
                measure(&only_a),
                show(&only_a),
                measure(&only_b),
                show(&only_b)
            ));
        }
    }
    None
}

async fn stored(app: &TestApp, user: i64, table: &str) -> BTreeMap<i64, Vec<Row>> {
    let class = if table == "spans" { "class" } else { "'agent'" };
    let c = app.st.pool.get().await.unwrap();
    let rows = c
        .query(&format!("SELECT device_id, start_ts, end_ts, project, {class} FROM {table} WHERE user_id = $1"), &[&user])
        .await
        .unwrap();
    let mut out: BTreeMap<i64, Vec<Row>> = BTreeMap::new();
    for r in rows {
        out.entry(r.get(0)).or_default().push((r.get(1), r.get(2), r.get(3), r.get(4)));
    }
    out.into_iter().map(|(k, v)| (k, coalesce(v))).collect()
}

async fn days(app: &TestApp, user: i64, with_agents: bool) -> Vec<Row> {
    let c = app.st.pool.get().await.unwrap();
    let sql = format!("SELECT day::text, project, class, seconds FROM user_days WHERE user_id = $1 {}", if with_agents { "" } else { "AND class <> 'agent'" });
    let rows = c.query(&sql, &[&user]).await.unwrap();
    let mut keyed: BTreeMap<(String, String, String), f64> = BTreeMap::new();
    for r in rows {
        *keyed.entry((r.get(0), r.get(1), r.get(2))).or_default() += r.get::<_, f64>(3);
    }
    keyed.into_iter().enumerate().map(|(i, ((d, p, c), s))| (i as f64, i as f64 + s, Some(format!("{d} {p}")), c)).collect()
}

fn reference(events: &[Event]) -> (Vec<Row>, Vec<Row>) {
    let credits = coalesce(Rule::CURRENT.apply(events).into_iter().map(|c| (c.start, c.end, c.project, c.class.as_str().to_string())).collect());
    let mut by_project: BTreeMap<Option<String>, Vec<(f64, f64)>> = BTreeMap::new();
    for (iv, p) in agent_turns(events) {
        by_project.entry(p.map(str::to_string)).or_default().push(iv);
    }
    let agents = by_project
        .into_iter()
        .flat_map(|(p, ivs)| union(ivs).into_iter().map(move |iv| (iv.0, iv.1, p.clone(), "agent".to_string())))
        .collect();
    (credits, coalesce(agents))
}

fn stored_vs_reference(with_agents: bool, stage: &str, spans: &BTreeMap<i64, Vec<Row>>, agents: &BTreeMap<i64, Vec<Row>>, devices: &[(i64, Vec<Event>)]) -> Result<(), String> {
    for (n, (id, events)) in devices.iter().enumerate() {
        let (credits, runtime) = reference(events);
        let empty = Vec::new();
        if let Some(d) = diff(&format!("device {n} spans"), stage, spans.get(id).unwrap_or(&empty), "the rule over all events", &credits) {
            return Err(d);
        }
        if !with_agents {
            continue;
        }
        if let Some(d) = diff(&format!("device {n} agent_spans"), stage, agents.get(id).unwrap_or(&empty), "agent_turns over all events", &runtime) {
            return Err(d);
        }
    }
    Ok(())
}

/// A user in `zone` with `wanted` paired devices: the user id, the devices' tokens and their ids.
async fn account(app: &TestApp, zone: &str, wanted: usize) -> (i64, Vec<String>, Vec<i64>) {
    let n = USERS.fetch_add(1, Ordering::SeqCst);
    let name = format!("Case{n}");
    let mut b = app.browser();
    b.sign_in(&name, &format!("case{n}@example.com")).await;
    let user = user_id(app, &name.to_lowercase()).await;
    app.exec(&format!("UPDATE users SET timezone = '{zone}' WHERE id = {user}")).await;
    let mut tokens = Vec::new();
    for k in 0..wanted {
        tokens.push(b.pair(&format!("box{k}")).await);
    }
    let c = app.st.pool.get().await.unwrap();
    let ids = c.query("SELECT id FROM devices WHERE user_id = $1 ORDER BY id", &[&user]).await.unwrap().iter().map(|r| r.get(0)).collect();
    (user, tokens, ids)
}

async fn case(app: &TestApp, events: &[Event], seed: u64, two: bool, with_agents: bool) -> Result<(), String> {
    let mut rng = Rng::new(seed);
    let wanted = if two { 2 } else { 1 };
    let zone = rng.pick(&ZONES);
    let (user, tokens, ids) = account(app, zone, wanted).await;
    let devices: Vec<(i64, Vec<Event>)> = (0..wanted)
        .map(|k| (ids[k], events.iter().filter(|e| (e.v as usize).saturating_sub(1).min(wanted - 1) == k).cloned().collect()))
        .collect();

    let mut queue: Vec<(usize, Vec<Event>)> = Vec::new();
    for (k, (_, es)) in devices.iter().enumerate() {
        if es.is_empty() {
            continue;
        }
        let div = rng.pick(&[4u64, 8, 16, 32, 64, 1 << 20]);
        let mut batch: Vec<Event> = Vec::new();
        for e in es {
            if !batch.is_empty() && starts_batch(e, div) {
                queue.push((k, std::mem::take(&mut batch)));
            }
            batch.push(e.clone());
        }
        queue.push((k, batch));
    }
    let mut tags: Vec<usize> = queue.iter().map(|q| q.0).collect();
    if two {
        rng.shuffle(&mut tags);
    }
    let mut per: Vec<VecDeque<Vec<Event>>> = (0..wanted).map(|_| VecDeque::new()).collect();
    for (k, es) in queue {
        per[k].push_back(es);
    }
    let mut prevs = vec![GENESIS.to_string(); wanted];
    for k in tags {
        let es = per[k].pop_front().unwrap();
        let up = upload(app, &tokens[k], sealed(&es, &mut prevs[k])).await;
        if up.status != axum::http::StatusCode::OK {
            return Err(format!("upload of {} events for device {k} refused: {} {}", es.len(), up.status, up.body));
        }
    }
    matches_full_rebuild(app, user, &devices, with_agents).await
}

/// What the uploads stored equals the rule over all events, and a full rebuild stores the same.
async fn matches_full_rebuild(app: &TestApp, user: i64, devices: &[(i64, Vec<Event>)], with_agents: bool) -> Result<(), String> {
    let (spans, agents, live_days) = (stored(app, user, "spans").await, stored(app, user, "agent_spans").await, days(app, user, with_agents).await);
    stored_vs_reference(with_agents, "the incremental recompute", &spans, &agents, devices)?;

    rebuild_user(&app.st.pool, user).await.map_err(|e| format!("rebuild failed: {e:#}"))?;
    let (rspans, ragents, rdays) = (stored(app, user, "spans").await, stored(app, user, "agent_spans").await, days(app, user, with_agents).await);
    stored_vs_reference(with_agents, "the full rebuild", &rspans, &ragents, devices)?;
    if let Some(d) = diff("user_days", "the incremental recompute", &live_days, "the full rebuild", &rdays) {
        return Err(d);
    }

    for (table, rows) in [("spans", &rspans), ("agent_spans", &ragents)].into_iter().filter(|t| with_agents || t.0 == "spans") {
        let all: Vec<Iv> = rows.values().flatten().map(|r| (r.0, r.1)).collect();
        let union = measure(&norm(all));
        let c = app.st.pool.get().await.unwrap();
        let sum: f64 = c
            .query_one(
                &format!(
                    "SELECT COALESCE(sum(seconds), 0) FROM user_days WHERE user_id = $1 AND class {} 'agent'",
                    if table == "spans" { "<>" } else { "=" }
                ),
                &[&user],
            )
            .await
            .unwrap()
            .get(0);
        if (sum - union).abs() > TOL * 10.0 {
            return Err(format!("user_days hold {sum:.3} s for {table} but their union is {union:.3} s"));
        }
    }
    Ok(())
}

// Sign-in and pairing hit per-IP limits (300 page loads), so each app serves a few cases only.
const CASES_PER_APP: usize = 8;

fn run(name: &str, default_cases: usize, p: Profile, two: bool, agents: bool) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let app = RefCell::new(rt.block_on(TestApp::new()));
    let used = Cell::new(0);
    let make = |rng: &mut Rng, p: Profile| {
        let mut all = generate(rng, p);
        if two {
            let mut other = generate(rng, p);
            other.iter_mut().for_each(|e| e.v = 2);
            all.extend(other);
        }
        all
    };
    let result = run_cases_from(name, default_cases, 120, p, make, |events, seed| {
        if used.get() == CASES_PER_APP {
            let old = app.replace(rt.block_on(TestApp::new()));
            rt.block_on(old.finish());
            used.set(0);
        }
        used.set(used.get() + 1);
        rt.block_on(case(&app.borrow(), events, seed, two, agents))
    });
    rt.block_on(app.into_inner().finish());
    result
}

fn check(name: &str, default_cases: usize, p: Profile, two: bool, agents: bool) {
    if let Err(e) = run(name, default_cases, p, two, agents) {
        panic!("{e}");
    }
}

const HOSTILE: Profile = Profile { hostile: true, ..REALISTIC };

#[test]
fn incremental_equals_full_on_one_device() {
    check("one_device", 30, REALISTIC, false, true);
}

#[test]
fn incremental_equals_full_on_two_devices_of_one_user() {
    check("two_devices", 20, REALISTIC, true, true);
}

#[test]
fn incremental_equals_full_on_two_devices_with_duplicate_out_of_order_and_orphan_events() {
    check("two_devices_hostile", 20, HOSTILE, true, false);
}

#[test]
fn incremental_equals_full_with_duplicate_out_of_order_and_orphan_events() {
    check("hostile", 30, HOSTILE, false, false);
}

#[test]
fn incremental_equals_full_with_long_turns_that_learn_their_project_late() {
    check("long_turns", 40, Profile { long_turns: true, ..REALISTIC }, false, false);
}

#[test]
fn incremental_equals_full_with_replies_that_have_no_project() {
    check("projectless_replies", 40, Profile { projectless_replies: true, ..REALISTIC }, false, true);
}

#[test]
fn incremental_agent_spans_equal_full_with_turns_longer_than_the_settle_window() {
    check("long_turn_agent_spans", 40, Profile { long_turns: true, ..REALISTIC }, false, true);
}

#[test]
fn incremental_equals_full_with_queued_heartbeats() {
    check("queued_heartbeats", 40, Profile { queued_heartbeats: true, ..REALISTIC }, false, false);
}

#[test]
fn incremental_equals_full_with_turns_that_never_finish_across_gaps_of_days() {
    check("crashed_turns", 40, Profile { crashed_turns: true, gaps_of_days: true, ..REALISTIC }, false, false);
}

#[test]
fn incremental_equals_full_with_long_pauses() {
    check("long_pauses", 40, Profile { long_pauses: true, ..REALISTIC }, false, false);
}

// Uploaded as they happen: a three-hour agent turn whose project's only events in between are hours apart, then a
// 12-hour pause followed by two short ones, with an upload six hours in.
#[test]
fn incremental_equals_full_for_a_long_turn_and_a_long_pause_uploaded_as_they_happen() {
    let h = 3600.0;
    let t0 = BASE + h;
    let at = |ts: f64, project: Option<&str>, kind: Kind| Event::new(ts, project.map(str::to_string), kind);
    let presents = |from: f64| (1..=10).map(move |i| Event::new(from + 60.0 * f64::from(i), None, Kind::Present { input: None }));
    let p = Some("ada/site");
    let mut batches = vec![
        vec![
            at(t0, p, Kind::Focus { class: Class::AgentTerminal, app: None }),
            at(t0 + 10.0, p, Kind::Prompt { words: 12, pasted: false, model: None, session: "s1".into(), shape: None, version: None, via: None, permissions: None }),
            at(t0 + 20.0, p, Kind::AgentStart { session: "s1".into() }),
        ],
        vec![at(t0 + 2.0 * h, p, Kind::Focus { class: Class::Editor, app: None })],
        vec![at(t0 + 3.0 * h, p, Kind::ReplyDone { session: "s1".into(), reply_words: 40, tools: 3, files_changed: 1, model: None })],
        vec![
            at(t0 + 4.0 * h, None, Kind::Pause { until: t0 + 16.0 * h }),
            at(t0 + 4.0 * h + 60.0, None, Kind::Pause { until: t0 + 4.0 * h + 360.0 }),
            at(t0 + 4.0 * h + 120.0, None, Kind::Pause { until: t0 + 4.0 * h + 420.0 }),
        ],
        vec![at(t0 + 10.0 * h, p, Kind::Focus { class: Class::Terminal, app: None })],
        vec![at(t0 + 17.0 * h, p, Kind::Focus { class: Class::Terminal, app: None })],
    ];
    for b in batches.iter_mut() {
        let from = b[0].ts;
        b.extend(presents(from));
    }
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let app = rt.block_on(TestApp::new());
    let result = rt.block_on(async {
        let (user, tokens, ids) = account(&app, "UTC", 1).await;
        let mut prev = GENESIS.to_string();
        for b in &batches {
            let up = upload(&app, &tokens[0], sealed(b, &mut prev)).await;
            assert_eq!(up.status, axum::http::StatusCode::OK, "{}", up.body);
        }
        let all: Vec<Event> = batches.concat();
        let (credits, runtime) = reference(&all);
        assert_eq!(runtime, vec![(t0 + 20.0, t0 + 3.0 * h, p.map(str::to_string), "agent".to_string())]);
        assert!(credits.iter().all(|c| c.0 >= t0 + 17.0 * h || c.1 <= t0 + 4.0 * h), "the pause holds until its end: {credits:?}");
        assert!(credits.iter().any(|c| c.0 >= t0 + 17.0 * h));
        matches_full_rebuild(&app, user, &[(ids[0], all)], true).await
    });
    rt.block_on(app.finish());
    if let Err(e) = result {
        panic!("{e}");
    }
}

// Two focus events share a timestamp before an upload's context: the one uploaded last is the one in front, whichever
// order the database hands equal timestamps back in. Rewriting the first one's row puts it last on disk.
#[test]
fn incremental_equals_full_when_the_focus_before_the_context_shares_its_timestamp() {
    let h = 3600.0;
    let t0 = BASE + h;
    let p = Some("ada/api".to_string());
    let focus = |class: Class, project: Option<String>| Event::new(t0, project, Kind::Focus { class, app: None });
    let presents = |from: f64| (1..=10).map(move |i| Event::new(from + 60.0 * f64::from(i), None, Kind::Present { input: None })).collect::<Vec<_>>();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let app = rt.block_on(TestApp::new());
    let result = rt.block_on(async {
        for pair in [[focus(Class::Terminal, p.clone()), focus(Class::Unknown, None)], [focus(Class::Unknown, None), focus(Class::Terminal, p.clone())]] {
            let terminal_last = pair[1].project.is_some();
            let batches = [[pair.to_vec(), presents(t0)].concat(), presents(t0 + 6.0 * h)];
            let (user, tokens, ids) = account(&app, "UTC", 1).await;
            let mut prev = GENESIS.to_string();
            for b in &batches {
                let up = upload(&app, &tokens[0], sealed(b, &mut prev)).await;
                assert_eq!(up.status, axum::http::StatusCode::OK, "{}", up.body);
                let d = ids[0];
                app.exec(&format!(
                    "WITH m AS (DELETE FROM events WHERE device_id = {d} AND seq = (SELECT min(seq) FROM events WHERE device_id = {d}) RETURNING *)
                     INSERT INTO events SELECT * FROM m"
                ))
                .await;
            }
            let all: Vec<Event> = batches.concat();
            assert_eq!(reference(&all).0.iter().any(|c| c.0 >= t0 + 6.0 * h), terminal_last);
            matches_full_rebuild(&app, user, &[(ids[0], all)], false).await?;
        }
        Ok::<(), String>(())
    });
    rt.block_on(app.finish());
    if let Err(e) = result {
        panic!("{e}");
    }
}

// The agent cd's into another repo mid-turn, so the reply names a different project than the start: the turn's agent
// time stays with the start's project and still ends at the reply.
#[test]
fn agent_time_stays_with_the_turns_start_when_its_reply_names_another_project() {
    let h = 3600.0;
    let t0 = BASE + h;
    let at = |ts: f64, project: Option<&str>, kind: Kind| Event::new(ts, project.map(str::to_string), kind);
    let done = |session: &str| Kind::ReplyDone { session: session.into(), reply_words: 40, tools: 3, files_changed: 1, model: None };
    let mut all = vec![
        at(t0, None, Kind::AgentStart { session: "s1".into() }),
        at(t0 + 600.0, Some("ada/api"), done("s1")),
        at(t0 + h, Some("ada/lib"), Kind::AgentStart { session: "s2".into() }),
        at(t0 + h + 600.0, Some("ada/api"), done("s2")),
    ];
    all.extend((1..300).map(|i| at(t0 + 60.0 * f64::from(i) + 1.0, None, Kind::Present { input: None })));
    all.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let app = rt.block_on(TestApp::new());
    let result = rt.block_on(async {
        let (user, tokens, ids) = account(&app, "UTC", 1).await;
        let mut prev = GENESIS.to_string();
        for b in all.chunk_by(|a, b| ((a.ts - t0) / h).floor() == ((b.ts - t0) / h).floor()) {
            let up = upload(&app, &tokens[0], sealed(b, &mut prev)).await;
            assert_eq!(up.status, axum::http::StatusCode::OK, "{}", up.body);
        }
        let agent = |s: f64, e: f64, p: Option<&str>| (s, e, p.map(str::to_string), "agent".to_string());
        assert_eq!(reference(&all).1, vec![agent(t0, t0 + 600.0, None), agent(t0 + h, t0 + h + 600.0, Some("ada/lib"))]);
        matches_full_rebuild(&app, user, &[(ids[0], all)], true).await
    });
    rt.block_on(app.finish());
    if let Err(e) = result {
        panic!("{e}");
    }
}
