//! The user row is locked for the day rebuild, so concurrent uploads from two devices of one user
//! serialize there and each sees the other's committed spans.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use chrono::{DateTime, Days, NaiveDate, TimeZone};
use chrono_tz::Tz;
use kairos_core::credit::{agent_turns, Rule, PAUSE_MAX_SECS, REVISION, TURN_MAX_SECS};
use kairos_core::event::Event;
use kairos_core::union;
use serde_json::Value;
use tokio_postgres::Transaction;

pub const RULE: Rule = Rule::CURRENT;
// Spans that end more than SETTLE before a batch's first event are final, so a new event may only change credit that
// reaches back at most this far: the rule's typing lookback, answer window and edit evidence must each be at most
// SETTLE.
const SETTLE: f64 = 3600.0;
// Credit at or after the cut may depend on events up to this far before it: a reply can be read until read_expiry
// after it finished, and bridging time leans on credit up to BRIDGE_MAX before it (and on input up to two BRIDGE_MAX
// before it), so read_expiry plus BRIDGE_MAX must be at most CONTEXT (a reply from before the loaded events has
// expired by then).
// `credit::tests::the_rule_stays_inside_what_the_server_loads` fails when the defaults outgrow either.
const CONTEXT: f64 = 2.5 * 3600.0;
// How far before the loaded events a turn's start and end are still read, to know which turns run at the edge.
const AGENT_CONTEXT: f64 = 24.0 * 3600.0;
/// Stored spans are split to at most this length so range queries can bound start_ts.
pub const MAX_SPAN: f64 = 6.0 * 3600.0;

pub const PRIORITY: &[&str] = &["typing", "editing", "terminal", "testing", "reading", "bridge", "trip"];

pub fn local_day(tz: Tz, t: f64) -> NaiveDate {
    DateTime::from_timestamp(t.floor() as i64, 0).map_or(NaiveDate::MIN, |d| d.with_timezone(&tz).date_naive())
}

/// Today in every zone chrono-tz knows, for a query to look up by the person's zone name, and today in UTC for any other.
/// Postgres doesn't know every name browsers report (prod's has no Asia/Calcutta), so a query can't work it out itself.
pub fn todays(t: f64) -> (Vec<&'static str>, Vec<NaiveDate>, NaiveDate) {
    let zones = chrono_tz::TZ_VARIANTS.iter();
    (zones.clone().map(|z| z.name()).collect(), zones.map(|&z| local_day(z, t)).collect(), local_day(chrono_tz::UTC, t))
}

/// Local midnight, or the first valid local time after it on DST-gap days.
pub fn day_start(tz: Tz, d: NaiveDate) -> f64 {
    for h in 0..4 {
        if let Some(t) = d.and_hms_opt(h, 0, 0).and_then(|n| tz.from_local_datetime(&n).earliest()) {
            return t.timestamp() as f64;
        }
    }
    d.and_hms_opt(0, 0, 0).map_or(0.0, |n| n.and_utc().timestamp() as f64)
}

fn next_day(d: NaiveDate) -> NaiveDate {
    d.checked_add_days(Days::new(1)).unwrap_or(d)
}

#[derive(Debug, Clone)]
pub(crate) struct Piece {
    pub(crate) start: f64,
    pub(crate) end: f64,
    pub(crate) project: Option<String>,
    pub(crate) class: String,
}

fn split(start: f64, end: f64) -> impl Iterator<Item = (f64, f64)> {
    let n = ((end - start) / MAX_SPAN).ceil().max(1.0) as usize;
    let step = (end - start) / n as f64;
    (0..n).map(move |i| (start + step * i as f64, if i + 1 == n { end } else { start + step * (i + 1) as f64 }))
}

/// Runs inside the upload transaction, which holds the device row lock.
pub async fn recompute_device(tx: &Transaction<'_>, device_id: i64, user_id: i64, from: f64, to: f64) -> Result<()> {
    let tz = lock_user(tx, user_id).await?;
    let cut = from - SETTLE;
    // An agent turn that runs at the cut is rewritten from its start (below), and where it ends depends on every event
    // up to TURN_MAX after it.
    let ctx = cut - CONTEXT.max(TURN_MAX_SECS);
    let rows = tx
        .query(
            "(SELECT seq, body FROM events WHERE device_id = $1 AND ts < $2 AND kind = 'focus' ORDER BY ts DESC, seq DESC LIMIT 1)
             UNION ALL (SELECT seq, body FROM events WHERE device_id = $1 AND ts >= $4 AND ts < $2 AND kind IN ('pause', 'resume'))
             UNION ALL (SELECT seq, body FROM events WHERE device_id = $1 AND ts >= $3 AND ts < $2 AND kind IN ('agent_start', 'prompt', 'reply_done'))
             UNION ALL (SELECT seq, body FROM events WHERE device_id = $1 AND ts >= $2)",
            &[&device_id, &ctx, &(ctx - AGENT_CONTEXT), &(ctx - PAUSE_MAX_SECS)],
        )
        .await?;
    let mut rows: Vec<(i64, Value)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    rows.sort_by_key(|r| r.0);
    let events: Vec<Event> = rows.into_iter().filter_map(|(_, b)| serde_json::from_value(b).ok()).collect();

    let credits: Vec<Piece> = RULE
        .apply(&events)
        .into_iter()
        .filter(|c| c.end > cut)
        .flat_map(|c| {
            split(c.start.max(cut), c.end).map(move |(s, e)| Piece { start: s, end: e, project: c.project.clone(), class: c.class.as_str().into() })
        })
        .filter(|p| p.end > p.start)
        .collect();

    let turns: Vec<(Option<String>, (f64, f64))> = agent_turns(&events).into_iter().map(|(iv, project)| (project.map(str::to_string), iv)).collect();
    // A turn's agent time grows until it ends, which can be up to TURN_MAX after it started, so one that runs at the
    // cut is stored again from its start.
    let agent_cut = turns.iter().filter(|(_, (s, e))| *s < cut && *e > cut).map(|(_, (s, _))| *s).fold(cut, f64::min);
    let mut runtime: HashMap<Option<String>, Vec<(f64, f64)>> = HashMap::new();
    for (project, iv) in turns {
        runtime.entry(project).or_default().push(iv);
    }
    let agents: Vec<Piece> = runtime
        .into_iter()
        .flat_map(|(project, ivs)| union(ivs).into_iter().map(move |iv| (project.clone(), iv)))
        .filter(|(_, (_, e))| *e > agent_cut)
        .flat_map(|(project, (s, e))| {
            split(s.max(agent_cut), e).map(move |(s, e)| Piece { start: s, end: e, project: project.clone(), class: "agent".into() })
        })
        .filter(|p| p.end > p.start)
        .collect();

    let mut hi = to.max(cut);
    for (table, cut) in [("spans", cut), ("agent_spans", agent_cut)] {
        let old: Option<f64> = tx
            .query_one(&format!("SELECT max(end_ts) FROM {table} WHERE device_id = $1 AND end_ts > $2"), &[&device_id, &cut])
            .await?
            .get(0);
        hi = hi.max(old.unwrap_or(cut));
        tx.execute(&format!("DELETE FROM {table} WHERE device_id = $1 AND start_ts >= $2"), &[&device_id, &cut]).await?;
        tx.execute(&format!("UPDATE {table} SET end_ts = $2 WHERE device_id = $1 AND end_ts > $2"), &[&device_id, &cut]).await?;
    }

    let starts: Vec<f64> = credits.iter().map(|p| p.start).collect();
    let ends: Vec<f64> = credits.iter().map(|p| p.end).collect();
    let projects: Vec<Option<&str>> = credits.iter().map(|p| p.project.as_deref()).collect();
    let classes: Vec<&str> = credits.iter().map(|p| p.class.as_str()).collect();
    tx.execute(
        "INSERT INTO spans (device_id, user_id, start_ts, end_ts, project, class, rule)
         SELECT $1, $2, s, e, p, c, $7 FROM UNNEST($3::float8[], $4::float8[], $5::text[], $6::text[]) AS t(s, e, p, c)",
        &[&device_id, &user_id, &starts, &ends, &projects, &classes, &RULE.name()],
    )
    .await?;
    let starts: Vec<f64> = agents.iter().map(|p| p.start).collect();
    let ends: Vec<f64> = agents.iter().map(|p| p.end).collect();
    let projects: Vec<Option<&str>> = agents.iter().map(|p| p.project.as_deref()).collect();
    tx.execute(
        "INSERT INTO agent_spans (device_id, user_id, start_ts, end_ts, project)
         SELECT $1, $2, s, e, p FROM UNNEST($3::float8[], $4::float8[], $5::text[]) AS t(s, e, p)",
        &[&device_id, &user_id, &starts, &ends, &projects],
    )
    .await?;

    hi = credits.iter().chain(&agents).map(|p| p.end).fold(hi, f64::max);
    rebuild_days(tx, user_id, tz, local_day(tz, agent_cut), local_day(tz, hi)).await
}

pub async fn lock_user(tx: &Transaction<'_>, user_id: i64) -> Result<Tz> {
    let tz: String = tx.query_one("SELECT timezone FROM users WHERE id = $1 FOR NO KEY UPDATE", &[&user_id]).await?.get(0);
    Ok(tz.parse().unwrap_or(chrono_tz::UTC))
}

pub async fn rebuild_user(pool: &deadpool_postgres::Pool, user_id: i64) -> Result<()> {
    let mut c = pool.get().await?;
    let tx = c.transaction().await?;
    let tz = lock_user(&tx, user_id).await?;
    let devices = tx
        .query(
            "SELECT d.id, min(e.ts), max(e.ts) FROM devices d JOIN events e ON e.device_id = d.id
             WHERE d.user_id = $1 GROUP BY d.id",
            &[&user_id],
        )
        .await?;
    for r in &devices {
        recompute_device(&tx, r.get(0), user_id, r.get(1), r.get(2)).await?;
    }
    rebuild_all(&tx, user_id, tz).await?;
    tx.execute("UPDATE users SET credit_revision = $2 WHERE id = $1", &[&user_id, &REVISION]).await?;
    tx.commit().await?;
    Ok(())
}

/// Each user is tried once per call, so one that fails doesn't stop the rest.
pub async fn rebuild_stale(pool: &deadpool_postgres::Pool) -> Result<usize> {
    let mut after = 0i64;
    let mut done = 0;
    loop {
        let next = pool
            .get()
            .await?
            .query_opt("SELECT id FROM users WHERE credit_revision < $1 AND id > $2 ORDER BY id LIMIT 1", &[&REVISION, &after])
            .await?;
        let Some(row) = next else { return Ok(done) };
        after = row.get(0);
        match rebuild_user(pool, after).await {
            Ok(()) => done += 1,
            Err(e) => tracing::warn!(user_id = after, error = format!("{e:#}"), "credit rebuild failed"),
        }
    }
}

pub async fn rebuild_all(tx: &Transaction<'_>, user_id: i64, tz: Tz) -> Result<()> {
    tx.execute("DELETE FROM user_days WHERE user_id = $1", &[&user_id]).await?;
    tx.execute("DELETE FROM user_tool_days WHERE user_id = $1", &[&user_id]).await?;
    let r = tx
        .query_one(
            "SELECT min(lo), max(hi) FROM (
                SELECT min(start_ts) lo, max(end_ts) hi FROM spans WHERE user_id = $1
                UNION ALL SELECT min(start_ts), max(end_ts) FROM agent_spans WHERE user_id = $1
                UNION ALL SELECT min(e.ts), max(e.ts) FROM events e JOIN devices d ON d.id = e.device_id
                    WHERE d.user_id = $1 AND e.kind = 'prompt') x",
            &[&user_id],
        )
        .await?;
    let (lo, hi): (Option<f64>, Option<f64>) = (r.get(0), r.get(1));
    if let (Some(lo), Some(hi)) = (lo, hi) {
        rebuild_days(tx, user_id, tz, local_day(tz, lo), local_day(tz, hi)).await?;
    }
    Ok(())
}

async fn load(tx: &Transaction<'_>, table: &str, user_id: i64, lo: f64, hi: f64) -> Result<Vec<(i64, Piece)>> {
    let class = if table == "spans" { "s.class" } else { "'agent'" };
    let rows = tx
        .query(
            &format!(
                "SELECT s.device_id, s.start_ts, s.end_ts, COALESCE(a.canonical, s.project), {class} FROM {table} s
                 LEFT JOIN project_aliases a ON a.user_id = s.user_id AND a.alias = s.project
                 WHERE s.user_id = $1 AND s.start_ts >= $2 AND s.start_ts < $3 AND s.end_ts > $4"
            ),
            &[&user_id, &(lo - MAX_SPAN), &hi, &lo],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let p = Piece { start: r.get::<_, f64>(1).max(lo), end: r.get::<_, f64>(2).min(hi), project: r.get(3), class: r.get(4) };
            (r.get(0), p)
        })
        .filter(|(_, p)| p.end > p.start)
        .collect())
}

/// Earlier groups win, then lower device ids, then projects in name order. Where stored spans start
/// depends on how uploads cut them up, so it must not decide who gets a second.
pub(crate) fn carve(mut pieces: Vec<(i64, Piece)>, rank: impl Fn(&Piece) -> usize) -> Vec<(i64, Piece)> {
    pieces.sort_by(|a, b| {
        rank(&a.1).cmp(&rank(&b.1)).then(a.0.cmp(&b.0)).then(a.1.project.cmp(&b.1.project)).then(a.1.start.total_cmp(&b.1.start))
    });
    let mut covered: Vec<(f64, f64)> = Vec::new();
    let mut out = Vec::new();
    for (device, p) in pieces {
        let mut cur = p.start;
        let first = covered.partition_point(|iv| iv.1 <= p.start);
        let mut gaps = Vec::new();
        for &(s, e) in &covered[first..] {
            if s >= p.end {
                break;
            }
            if s > cur {
                gaps.push((cur, s));
            }
            cur = cur.max(e);
        }
        if cur < p.end {
            gaps.push((cur, p.end));
        }
        for (s, e) in gaps {
            let at = covered.partition_point(|iv| iv.0 < s);
            covered.insert(at, (s, e));
            out.push((device, Piece { start: s, end: e, ..p.clone() }));
        }
    }
    out
}

fn add_by_day(out: &mut BTreeMap<(NaiveDate, String, String), f64>, tz: Tz, p: &Piece) {
    let mut t = p.start;
    while t < p.end {
        let day = local_day(tz, t);
        let end = p.end.min(day_start(tz, next_day(day)));
        let end = if end <= t { p.end } else { end };
        *out.entry((day, p.project.clone().unwrap_or_default(), p.class.clone())).or_default() += end - t;
        t = end;
    }
}

pub(crate) fn credit_rank(p: &Piece) -> usize {
    PRIORITY.iter().position(|c| *c == p.class).unwrap_or(PRIORITY.len())
}

pub async fn rebuild_days(tx: &Transaction<'_>, user_id: i64, tz: Tz, first: NaiveDate, last: NaiveDate) -> Result<()> {
    let lo = day_start(tz, first);
    let hi = day_start(tz, next_day(last));
    let credit = carve(load(tx, "spans", user_id, lo, hi).await?, credit_rank);
    let agent = carve(load(tx, "agent_spans", user_id, lo, hi).await?, |_| 0);
    let mut days = BTreeMap::new();
    for (_, p) in credit.iter().chain(&agent) {
        add_by_day(&mut days, tz, p);
    }
    tx.execute("DELETE FROM user_days WHERE user_id = $1 AND day >= $2 AND day <= $3", &[&user_id, &first, &last]).await?;
    let mut ds = Vec::new();
    let mut projects = Vec::new();
    let mut classes = Vec::new();
    let mut secs = Vec::new();
    for ((d, p, c), s) in days {
        if d >= first && d <= last && s > 0.0 {
            ds.push(d);
            projects.push(p);
            classes.push(c);
            secs.push(s);
        }
    }
    tx.execute(
        "INSERT INTO user_days (user_id, day, project, class, seconds)
         SELECT $1, d, p, c, s FROM UNNEST($2::date[], $3::text[], $4::text[], $5::float8[]) AS t(d, p, c, s)",
        &[&user_id, &ds, &projects, &classes, &secs],
    )
    .await?;
    rebuild_tools(tx, user_id, tz, first, last).await
}

// Devices send what they like, so the id is checked and each day and project keeps only its busiest tools.
const TOOLS_PER_DAY: usize = 20;

fn tool_id(s: &str) -> bool {
    let ok = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.');
    (1..=32).contains(&s.len()) && s.bytes().all(ok) && !s.starts_with(['-', '_', '.'])
}

async fn rebuild_tools(tx: &Transaction<'_>, user_id: i64, tz: Tz, first: NaiveDate, last: NaiveDate) -> Result<()> {
    let (lo, hi) = (day_start(tz, first), day_start(tz, next_day(last)));
    let rows = tx
        .query(
            "SELECT e.ts, e.body->>'harness', COALESCE(a.canonical, e.project, '') FROM events e JOIN devices d ON d.id = e.device_id
             LEFT JOIN project_aliases a ON a.user_id = d.user_id AND a.alias = e.project
             WHERE d.user_id = $1 AND e.kind = 'prompt' AND e.ts >= $2 AND e.ts < $3 AND e.body->>'harness' IS NOT NULL",
            &[&user_id, &lo, &hi],
        )
        .await?;
    let mut counts: BTreeMap<(NaiveDate, String, String), i32> = BTreeMap::new();
    for r in &rows {
        let (ts, harness, project): (f64, String, String) = (r.get(0), r.get(1), r.get(2));
        let day = local_day(tz, ts);
        if tool_id(&harness) && day >= first && day <= last {
            *counts.entry((day, project, harness)).or_default() += 1;
        }
    }
    let mut list: Vec<(NaiveDate, String, String, i32)> = counts.into_iter().map(|((d, p, h), n)| (d, p, h, n)).collect();
    list.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)).then(b.3.cmp(&a.3)).then(a.2.cmp(&b.2)));
    let mut kept: Vec<(NaiveDate, String, String, i32)> = Vec::new();
    for item in list {
        let same = kept.iter().rev().take_while(|k| (&k.0, &k.1) == (&item.0, &item.1)).count();
        if same < TOOLS_PER_DAY {
            kept.push(item);
        }
    }
    let days: Vec<NaiveDate> = kept.iter().map(|k| k.0).collect();
    let projects: Vec<&str> = kept.iter().map(|k| k.1.as_str()).collect();
    let harnesses: Vec<&str> = kept.iter().map(|k| k.2.as_str()).collect();
    let prompts: Vec<i32> = kept.iter().map(|k| k.3).collect();
    tx.execute("DELETE FROM user_tool_days WHERE user_id = $1 AND day >= $2 AND day <= $3", &[&user_id, &first, &last]).await?;
    tx.execute(
        "INSERT INTO user_tool_days (user_id, day, project, harness, prompts)
         SELECT $1, d, p, h, n FROM UNNEST($2::date[], $3::text[], $4::text[], $5::int[]) AS t(d, p, h, n)",
        &[&user_id, &days, &projects, &harnesses, &prompts],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: f64, e: f64, class: &str) -> Piece {
        Piece { start: s, end: e, project: None, class: class.into() }
    }

    #[test]
    fn carving_keeps_each_second_once() {
        let rank = |p: &Piece| PRIORITY.iter().position(|c| *c == p.class).unwrap_or(9);
        let out = carve(vec![(2, p(0.0, 100.0, "reading")), (1, p(50.0, 150.0, "editing"))], rank);
        let total: f64 = out.iter().map(|(_, p)| p.end - p.start).sum();
        assert_eq!(total, 150.0);
        assert!(out.iter().any(|(_, p)| p.class == "editing" && p.start == 50.0 && p.end == 150.0));
    }

    #[test]
    fn a_tie_between_devices_does_not_depend_on_where_spans_were_cut() {
        let rank = |p: &Piece| PRIORITY.iter().position(|c| *c == p.class).unwrap_or(9);
        let on = |s: f64, e: f64, project: &str| Piece { start: s, end: e, project: Some(project.into()), class: "editing".into() };
        let whole = vec![(1, on(100.0, 400.0, "b")), (2, on(0.0, 300.0, "a"))];
        let cut = vec![(1, on(100.0, 250.0, "b")), (1, on(250.0, 400.0, "b")), (2, on(0.0, 120.0, "a")), (2, on(120.0, 300.0, "a"))];
        let per_project = |out: Vec<(i64, Piece)>| {
            let mut m = BTreeMap::new();
            for (_, p) in out {
                *m.entry(p.project.unwrap()).or_insert(0.0) += p.end - p.start;
            }
            m
        };
        let a = per_project(carve(whole, rank));
        assert_eq!(a, per_project(carve(cut, rank)));
        assert_eq!((a["a"], a["b"]), (100.0, 300.0), "the lower device keeps the shared seconds");
    }

    #[test]
    fn days_split_at_local_midnight() {
        let tz: Tz = "Asia/Kolkata".parse().unwrap();
        let d = NaiveDate::from_ymd_opt(2026, 3, 10).unwrap();
        let mid = day_start(tz, d);
        assert_eq!(mid, 1773081000.0);
        let mut out = BTreeMap::new();
        add_by_day(&mut out, tz, &p(mid - 600.0, mid + 300.0, "editing"));
        let v: Vec<f64> = out.values().copied().collect();
        assert_eq!(v, vec![600.0, 300.0]);
    }

    #[test]
    fn tool_ids() {
        assert!(tool_id("claude-code") && tool_id("roo-code") && tool_id("qwen-code"));
        assert!(!tool_id("") && !tool_id("Claude") && !tool_id("a b") && !tool_id("-x") && !tool_id(&"a".repeat(33)));
    }

    #[test]
    fn the_rule_stays_inside_what_the_server_loads() {
        use kairos_core::credit::{BRIDGE_MAX_SECS, EDITOR_PROJECT_SECS, HEARTBEAT_LAG_SECS};
        use kairos_core::event::INPUT_WINDOW_SECS;
        let v6 = kairos_core::credit::V6Params::default();
        let read = v6.read_expiry_secs + BRIDGE_MAX_SECS;
        assert!(read <= CONTEXT, "a reply can be read {} s after it finished and bridged {BRIDGE_MAX_SECS} s on, the server loads {CONTEXT} s", v6.read_expiry_secs);
        assert!(2.0 * BRIDGE_MAX_SECS <= CONTEXT, "a gap's anchor reads input {BRIDGE_MAX_SECS} s before it, the server loads {CONTEXT} s");
        // Whatever a new event changes, a bridge can carry up to BRIDGE_MAX further back.
        for (name, secs) in [
            ("lookback_secs", v6.lookback_secs),
            ("answer_window_secs", v6.answer_window_secs),
            ("edit_evidence_secs", v6.edit_evidence_secs),
            ("a heartbeat's lag and edit_evidence_secs", HEARTBEAT_LAG_SECS + v6.edit_evidence_secs),
            ("a heartbeat's lag and EDITOR_PROJECT_SECS", HEARTBEAT_LAG_SECS + EDITOR_PROJECT_SECS),
            // A present's input seconds run up to INPUT_WINDOW before it, and a gap's anchor reads BRIDGE_MAX after it.
            ("a gap's anchor", INPUT_WINDOW_SECS + BRIDGE_MAX_SECS),
        ] {
            assert!(secs + BRIDGE_MAX_SECS <= SETTLE, "{name} {secs} and bridging reach back further than the settle window {SETTLE}");
        }
        assert_eq!(v6.turn_max_secs, TURN_MAX_SECS, "the server cuts agent turns with TURN_MAX_SECS");
        // A turn running after the agent cut started up to two TURN_MAX before the cut, and took its project from a start
        // up to one more TURN_MAX before that; the server reads them from AGENT_CONTEXT before its context, which is
        // itself at least TURN_MAX before the cut.
        assert!(2.0 * TURN_MAX_SECS <= AGENT_CONTEXT, "turns can run {TURN_MAX_SECS} s, the server reads {AGENT_CONTEXT} s of them");
    }

    #[test]
    fn long_spans_are_split() {
        let parts: Vec<_> = split(0.0, 13.0 * 3600.0).collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts.last().unwrap().1, 13.0 * 3600.0);
    }
}
