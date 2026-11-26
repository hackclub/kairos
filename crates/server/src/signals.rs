//! Signals inform a person and never change credit or trust.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Days, NaiveDate};
use chrono_tz::Tz;
use kairos_core::{total_secs, union, Interval};

use crate::aliases::canon_commits;
use crate::api::stats::{load_pieces, Span};
use crate::credit::{day_start, local_day};
use crate::util::{human, Res};
use crate::St;

pub const LONG_DAY: f64 = 12.0 * 3600.0;
pub const UNBROKEN_INPUT: f64 = 4.0 * 3600.0;
pub const OVERLAP_PER_DAY: f64 = 10.0 * 60.0;
pub const LATE_UPLOAD: f64 = 48.0 * 3600.0;
pub const NO_OUTPUT: f64 = 5.0 * 3600.0;
pub const DATED_BEHIND: f64 = 3600.0;
pub const DATED_AHEAD: f64 = 600.0;
/// `present` fires once a minute while there is input; each one stands for the minute before it.
pub const PRESENT_EVERY: f64 = 60.0;
const PRESENT_GAP: f64 = 90.0;
// Less than a minute from a blind device or a late upload is a daemon start, not a pattern.
const MIN_SECS: f64 = 60.0;
const MAX_EVIDENCE: usize = 12;
const BACKENDS_LOOKBACK: f64 = 92.0 * 86_400.0;
const MAX_ROWS: i64 = 50_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Notice,
    Info,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Notice => "notice",
            Level::Info => "info",
        }
    }
}

pub struct Evidence {
    pub day: Option<NaiveDate>,
    pub text: String,
}

pub struct Signal {
    pub id: &'static str,
    pub level: Level,
    pub title: &'static str,
    pub explanation: &'static str,
    pub evidence: Vec<Evidence>,
    pub more: usize,
}

#[derive(Debug, Clone)]
pub struct Presence {
    pub device_id: i64,
    pub start: f64,
    pub end: f64,
}

#[derive(Debug, Clone)]
pub struct Backends {
    pub device_id: i64,
    pub ts: f64,
    pub focus: String,
    pub idle: String,
}

impl Backends {
    fn blind(&self) -> bool {
        [&self.focus, &self.idle].iter().any(|b| b.is_empty() || b.as_str() == "none")
    }
}

#[derive(Debug, Clone)]
pub struct LateBatch {
    pub device_id: i64,
    pub received: f64,
    pub first: f64,
    pub last: f64,
}

#[derive(Debug, Clone)]
pub struct DatedCommit {
    pub project: String,
    pub sha: String,
    pub not_before: f64,
    pub seen: f64,
    pub dated: i64,
}

#[derive(Default)]
pub struct Facts {
    pub presence: Vec<Presence>,
    pub backends: Vec<Backends>,
    pub late: Vec<LateBatch>,
    pub commits: HashMap<String, i64>,
    pub saves: HashMap<String, i64>,
    pub dated: Vec<DatedCommit>,
}

pub struct Inputs<'a> {
    pub tz: Tz,
    pub devices: &'a HashMap<i64, String>,
    pub spans: &'a [&'a Span],
    pub facts: &'a Facts,
}

impl Inputs<'_> {
    fn device(&self, id: i64) -> String {
        self.devices.get(&id).cloned().unwrap_or_else(|| format!("device {id}"))
    }
}

pub fn detect(i: &Inputs) -> Vec<Signal> {
    let mut out: Vec<Signal> =
        [long_day(i), unbroken_input(i), overlap(i), no_output(i), commit_dates(i), blind_device(i), late_upload(i)].into_iter().flatten().collect();
    out.sort_by_key(|s| s.level);
    out
}

pub fn input_seen(presence: &[Presence], (start, end): (f64, f64)) -> f64 {
    total_secs(&union(presence.iter().map(|p| (p.start.max(start), p.end.min(end))).filter(|(s, e)| e > s).collect()))
}

fn signal(id: &'static str, level: Level, title: &'static str, explanation: &'static str, mut evidence: Vec<Evidence>) -> Option<Signal> {
    if evidence.is_empty() {
        return None;
    }
    let more = evidence.len().saturating_sub(MAX_EVIDENCE);
    evidence.truncate(MAX_EVIDENCE);
    Some(Signal { id, level, title, explanation, evidence, more })
}

fn clock(tz: Tz, t: f64) -> String {
    DateTime::from_timestamp(t.floor() as i64, 0).map_or_else(String::new, |d| d.with_timezone(&tz).format("%H:%M").to_string())
}

fn span_text(tz: Tz, s: f64, e: f64) -> String {
    if local_day(tz, s) == local_day(tz, e - 1e-6) {
        format!("{} to {}", clock(tz, s), clock(tz, e))
    } else {
        format!("{} to {} {}", clock(tz, s), local_day(tz, e - 1e-6).format("%b %-d"), clock(tz, e))
    }
}

fn by_day(tz: Tz, iv: impl IntoIterator<Item = Interval>) -> BTreeMap<NaiveDate, Vec<Interval>> {
    let mut days: BTreeMap<NaiveDate, Vec<Interval>> = BTreeMap::new();
    for (s, e) in iv {
        let mut t = s;
        while t < e {
            let day = local_day(tz, t);
            let next = day.checked_add_days(Days::new(1)).map_or(e, |d| day_start(tz, d));
            let cut = if next > t { e.min(next) } else { e };
            days.entry(day).or_default().push((t, cut));
            t = cut;
        }
    }
    days.into_iter().map(|(d, iv)| (d, union(iv))).collect()
}

fn clip(iv: &[Interval], s: f64, e: f64) -> Vec<Interval> {
    iv.iter().map(|c| (c.0.max(s), c.1.min(e))).filter(|c| c.1 > c.0).collect()
}

fn device_credit(spans: &[&Span]) -> BTreeMap<i64, Vec<Interval>> {
    let mut per: BTreeMap<i64, Vec<Interval>> = BTreeMap::new();
    for s in spans {
        per.entry(s.device_id).or_default().push((s.start, s.end));
    }
    per.into_iter().map(|(d, iv)| (d, union(iv))).collect()
}

fn long_day(i: &Inputs) -> Option<Signal> {
    let days = by_day(i.tz, i.spans.iter().map(|s| (s.start, s.end)));
    let evidence = days
        .into_iter()
        .map(|(d, iv)| (d, total_secs(&iv)))
        .filter(|(_, secs)| *secs > LONG_DAY)
        .map(|(d, secs)| Evidence { day: Some(d), text: format!("{} of focus time", human(secs)) })
        .collect();
    signal("long_day", Level::Notice, "Days over 12 hours", "More than 12 hours of focus time in one day.", evidence)
}

fn unbroken_input(i: &Inputs) -> Option<Signal> {
    let mut runs: Vec<&Presence> = i.facts.presence.iter().filter(|p| p.end - p.start >= UNBROKEN_INPUT).collect();
    runs.sort_by(|a, b| a.start.total_cmp(&b.start));
    let evidence = runs
        .into_iter()
        .map(|p| Evidence {
            day: Some(local_day(i.tz, p.start)),
            text: format!("{} on {}, {} with input every minute", span_text(i.tz, p.start, p.end), i.device(p.device_id), human(p.end - p.start)),
        })
        .collect();
    signal(
        "unbroken_input",
        Level::Notice,
        "Input without a break for 4 hours or more",
        "A device saw keyboard or mouse input in every minute for 4 hours or more. Most people pause for a minute now and then, so a long unbroken run is worth a look.",
        evidence,
    )
}

fn overlaps(spans: &[&Span]) -> Vec<(f64, f64, BTreeSet<i64>)> {
    let mut edges: Vec<(f64, bool, i64)> = Vec::new();
    for (d, iv) in device_credit(spans) {
        for (s, e) in iv {
            edges.push((s, true, d));
            edges.push((e, false, d));
        }
    }
    // Ends sort before starts at the same instant, so touching runs never count as overlap.
    edges.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut active: BTreeSet<i64> = BTreeSet::new();
    let mut out = Vec::new();
    let mut prev = f64::NEG_INFINITY;
    for (t, start, d) in edges {
        if active.len() >= 2 && t > prev {
            out.push((prev, t, active.clone()));
        }
        if start {
            active.insert(d);
        } else {
            active.remove(&d);
        }
        prev = t;
    }
    out
}

fn overlap(i: &Inputs) -> Option<Signal> {
    let mut days: BTreeMap<NaiveDate, (f64, f64, BTreeSet<i64>)> = BTreeMap::new();
    for (s, e, devs) in overlaps(i.spans) {
        for (day, iv) in by_day(i.tz, [(s, e)]) {
            let entry = days.entry(day).or_insert((0.0, f64::INFINITY, BTreeSet::new()));
            entry.0 += total_secs(&iv);
            entry.1 = entry.1.min(iv[0].0);
            entry.2.extend(devs.iter().copied());
        }
    }
    let evidence = days
        .into_iter()
        .filter(|(_, (secs, _, _))| *secs > OVERLAP_PER_DAY)
        .map(|(day, (secs, first, devs))| {
            let names: Vec<String> = devs.into_iter().map(|d| i.device(d)).collect();
            Evidence { day: Some(day), text: format!("{} on {} at once, from {}", human(secs), names.join(" and "), clock(i.tz, first)) }
        })
        .collect();
    signal(
        "overlap",
        Level::Notice,
        "Focus on two devices at once",
        "Focus time was credited on two devices at the same moment for more than 10 minutes in a day. Totals count those seconds once.",
        evidence,
    )
}

fn blind_device(i: &Inputs) -> Option<Signal> {
    let mut evidence = Vec::new();
    for (device, credit) in device_credit(i.spans) {
        let mut reports: Vec<&Backends> = i.facts.backends.iter().filter(|b| b.device_id == device).collect();
        reports.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        // Before its first report a device's backends are unknown, which can't be checked either.
        let mut blind: Vec<(Interval, String)> = Vec::new();
        let first = reports.first().map_or(f64::INFINITY, |b| b.ts);
        blind.push(((f64::NEG_INFINITY, first), "no backends reported".to_string()));
        for (k, b) in reports.iter().enumerate() {
            if b.blind() {
                let until = reports.get(k + 1).map_or(f64::INFINITY, |n| n.ts);
                blind.push(((b.ts, until), format!("focus {}, idle {}", or_none(&b.focus), or_none(&b.idle))));
            }
        }
        let mut secs = 0.0;
        let mut first_day = None;
        let mut why: Vec<String> = Vec::new();
        for ((s, e), reason) in blind {
            let part = clip(&credit, s, e);
            let t = total_secs(&part);
            if t > 0.0 {
                secs += t;
                first_day = first_day.or(part.first().map(|p| local_day(i.tz, p.0)));
                if !why.contains(&reason) {
                    why.push(reason);
                }
            }
        }
        if secs >= MIN_SECS {
            evidence.push(Evidence { day: first_day, text: format!("{}: {} of focus time ({})", i.device(device), human(secs), why.join("; ")) });
        }
    }
    signal(
        "blind_device",
        Level::Info,
        "Time from devices that can't see focus or input",
        "Some focus time came from a device that reported no focus or idle backend, so what was on screen or whether someone was at the keyboard could not be checked.",
        evidence,
    )
}

fn or_none(s: &str) -> &str {
    if s.is_empty() {
        "none"
    } else {
        s
    }
}

fn late_upload(i: &Inputs) -> Option<Signal> {
    let credit = device_credit(i.spans);
    let mut batches: Vec<&LateBatch> = i.facts.late.iter().collect();
    batches.sort_by(|a, b| a.device_id.cmp(&b.device_id).then(a.first.total_cmp(&b.first)));
    let mut stretches: Vec<(i64, f64, f64, f64, f64)> = Vec::new();
    for b in batches {
        let (s, e) = (b.first - PRESENT_EVERY, b.last);
        let (shortest, longest) = (b.received - b.last, b.received - b.first);
        match stretches.last_mut() {
            Some(l) if l.0 == b.device_id && s <= l.2 => {
                l.2 = l.2.max(e);
                l.3 = l.3.min(shortest);
                l.4 = l.4.max(longest);
            }
            _ => stretches.push((b.device_id, s, e, shortest, longest)),
        }
    }
    let mut evidence: Vec<(f64, Evidence)> = Vec::new();
    for (device, s, e, shortest, longest) in stretches {
        let parts = clip(credit.get(&device).map_or(&[][..], Vec::as_slice), s, e);
        let secs = total_secs(&parts);
        if let (true, Some(first), Some(last)) = (secs >= MIN_SECS, parts.first(), parts.last()) {
            let delay = later(shortest, longest);
            let text = format!("{} on {}: {} of focus time, uploaded {delay} later", span_text(i.tz, first.0, last.1), i.device(device), human(secs));
            evidence.push((first.0, Evidence { day: Some(local_day(i.tz, first.0)), text }));
        }
    }
    evidence.sort_by(|a, b| a.0.total_cmp(&b.0));
    signal(
        "late_upload",
        Level::Info,
        "Uploaded more than 48 hours later",
        "The events behind this time reached the server more than 48 hours after they happened. A computer that was offline does this; so would events written after the fact.",
        evidence.into_iter().map(|e| e.1).collect(),
    )
}

fn later(shortest: f64, longest: f64) -> String {
    let unit = |secs: f64| {
        if secs >= 2.0 * 86_400.0 {
            ((secs / 86_400.0).floor() as i64, "days")
        } else {
            ((secs / 3600.0).floor() as i64, "hours")
        }
    };
    match (unit(shortest), unit(longest)) {
        (a, b) if a == b => format!("{} {}", a.0, a.1),
        ((a, u), (b, v)) if u == v => format!("{a} to {b} {v}"),
        ((a, u), (b, v)) => format!("{a} {u} to {b} {v}"),
    }
}

fn no_output(i: &Inputs) -> Option<Signal> {
    let mut per: BTreeMap<&str, Vec<Interval>> = BTreeMap::new();
    for s in i.spans {
        if let Some(p) = s.project.as_deref() {
            per.entry(p).or_default().push((s.start, s.end));
        }
    }
    let evidence = per
        .into_iter()
        .map(|(p, iv)| (p, total_secs(&union(iv))))
        .filter(|(p, secs)| {
            *secs >= NO_OUTPUT && i.facts.commits.get(*p).copied().unwrap_or(0) == 0 && i.facts.saves.get(*p).copied().unwrap_or(0) == 0
        })
        .map(|(p, secs)| Evidence { day: None, text: format!("{p}: {} of focus time, no commits, no saved files", human(secs)) })
        .collect();
    signal(
        "no_output",
        Level::Notice,
        "Long project time with no commits or saves",
        "A project has 5 hours or more of focus time in these dates but no commits and no saved files.",
        evidence,
    )
}

/// A git date off the span the device shows the commit was made in, from `not_before` to `seen`. Without `not_before`
/// (pulled, an older kairosd, a reflog it lost track of) nothing shows when it was made, so no date is off.
/// The committer date counts, and for a plain commit or revert the author date too (`--date` sets only that one).
/// An amend or cherry-pick keeps the original author date, so only its committer date counts.
pub fn git_date_off(not_before: Option<f64>, seen: f64, via: Option<&str>, authored: Option<i64>, committed: Option<i64>, behind: f64, ahead: f64) -> Option<i64> {
    let from = not_before?;
    let off = |t: i64| (t as f64) < from - behind || (t as f64) > seen + ahead;
    committed.filter(|t| off(*t)).or_else(|| authored.filter(|t| matches!(via, Some("commit" | "revert")) && off(*t)))
}

fn commit_dates(i: &Inputs) -> Option<Signal> {
    let mut dated: Vec<&DatedCommit> = i.facts.dated.iter().collect();
    dated.sort_by(|a, b| a.seen.total_cmp(&b.seen).then_with(|| a.sha.cmp(&b.sha)));
    let evidence = dated
        .into_iter()
        .map(|c| {
            let git = DateTime::from_timestamp(c.dated, 0).map_or_else(String::new, |d| d.with_timezone(&i.tz).format("%b %-d, %Y %H:%M").to_string());
            let short: String = c.sha.chars().take(7).collect();
            let day = local_day(i.tz, c.seen);
            let from = match local_day(i.tz, c.not_before) {
                d if d == day => clock(i.tz, c.not_before),
                d => format!("{} {}", d.format("%b %-d"), clock(i.tz, c.not_before)),
            };
            let text = format!("{short} in {}, not there at {from}, seen {}, git date {git}", c.project, clock(i.tz, c.seen));
            Evidence { day: Some(day), text }
        })
        .collect();
    signal(
        "commit_dates",
        Level::Notice,
        "Commit dates that don't match when they were made",
        "A commit made on a device has a git date (committer, or author for a plain commit) more than an hour before the device last looked and it wasn't there yet, or more than 10 minutes after the device saw it. Whoever makes a commit can set its date, so the review goes by what the device saw.",
        evidence,
    )
}

const PRESENCE_RUNS: &str = "
    SELECT device_id, min(ts), max(ts) FROM (
        SELECT device_id, ts, kind, sum(brk) OVER (PARTITION BY device_id ORDER BY ts, seq) AS run FROM (
            SELECT device_id, seq, ts, kind,
                   CASE WHEN kind = 'present' AND lag(kind) OVER w = 'present' AND ts - lag(ts) OVER w <= $4 THEN 0 ELSE 1 END AS brk
            FROM events WHERE device_id = ANY($1) AND kind IN ('present', 'idle') AND ts >= $2 AND ts < $3
            WINDOW w AS (PARTITION BY device_id ORDER BY ts, seq)
        ) a
    ) b WHERE kind = 'present' GROUP BY device_id, run ORDER BY 2 LIMIT $5";

const BACKENDS: &str = "
    SELECT device_id, ts, COALESCE(body->>'focus', ''), COALESCE(body->>'idle', '') FROM events
    WHERE device_id = ANY($1) AND kind = 'backends' AND ts >= $2 AND ts < $3
    UNION ALL
    SELECT d.id, b.ts, b.focus, b.idle FROM unnest($1::int8[]) AS d(id) CROSS JOIN LATERAL (
        SELECT ts, COALESCE(body->>'focus', '') AS focus, COALESCE(body->>'idle', '') AS idle FROM events
        WHERE device_id = d.id AND kind = 'backends' AND ts < $2 AND ts >= $4 ORDER BY ts DESC LIMIT 1) b
    LIMIT $5";

// Rows of one upload share received_at (the transaction time), so this is one row per late upload.
const LATE: &str = "
    SELECT device_id, extract(epoch FROM received_at)::float8, min(ts), max(ts) FROM events
    WHERE device_id = ANY($1) AND ts >= $2 AND ts < $3 AND extract(epoch FROM received_at) - ts > $4
    GROUP BY device_id, received_at ORDER BY 3 LIMIT $5";

const SAVES: &str = "
    SELECT COALESCE(a.canonical, e.project), count(*) FROM events e
    LEFT JOIN project_aliases a ON a.user_id = $4 AND a.alias = e.project
    WHERE e.device_id = ANY($1) AND e.kind = 'file_save' AND e.ts >= $2 AND e.ts < $3 AND e.project IS NOT NULL GROUP BY 1";

const COMMITS: &str = concat!("SELECT project, count(*) FROM ", canon_commits!(), " c WHERE ts >= $2 AND ts < $3 GROUP BY project");

// Commits made on a device (not pulled or rebased). `git_date_off` has the last word on which date counts.
const DATED: &str = concat!("
    SELECT c.project, c.sha, c.ts, c.via, c.authored_at, c.committed_at, c.not_before FROM ", canon_commits!(), " c
    WHERE c.ts >= $2 AND c.ts < $3 AND c.via IN ('commit', 'amend', 'cherry_pick', 'revert')
      AND c.device_id IS NOT NULL AND c.not_before IS NOT NULL
      AND (c.committed_at < c.not_before - $4 OR c.committed_at > c.ts + $5
           OR (c.via IN ('commit', 'revert') AND (c.authored_at < c.not_before - $4 OR c.authored_at > c.ts + $5)))
    ORDER BY c.ts, c.sha LIMIT $6");

pub async fn load(st: &St, user_id: i64, devices: &[i64], (start, end): (f64, f64)) -> Res<Facts> {
    let c = st.pool.get().await?;
    let (presence, backends, late, saves, commits, dated) = tokio::try_join!(
        async { c.query(PRESENCE_RUNS, &[&devices, &start, &(end + PRESENT_EVERY), &PRESENT_GAP, &MAX_ROWS]).await },
        async { c.query(BACKENDS, &[&devices, &start, &end, &(start - BACKENDS_LOOKBACK), &MAX_ROWS]).await },
        async { c.query(LATE, &[&devices, &start, &end, &LATE_UPLOAD, &MAX_ROWS]).await },
        async { c.query(SAVES, &[&devices, &start, &end, &user_id]).await },
        async { c.query(COMMITS, &[&user_id, &start, &end]).await },
        async {
            c.query(DATED, &[&user_id, &start, &end, &DATED_BEHIND, &DATED_AHEAD, &MAX_ROWS]).await
        },
    )?;
    Ok(Facts {
        presence: presence
            .iter()
            .map(|r| Presence { device_id: r.get(0), start: r.get::<_, f64>(1) - PRESENT_EVERY, end: r.get(2) })
            .collect(),
        backends: backends.iter().map(|r| Backends { device_id: r.get(0), ts: r.get(1), focus: r.get(2), idle: r.get(3) }).collect(),
        late: late.iter().map(|r| LateBatch { device_id: r.get(0), received: r.get(1), first: r.get(2), last: r.get(3) }).collect(),
        saves: saves.iter().map(|r| (r.get(0), r.get(1))).collect(),
        commits: commits.iter().map(|r| (r.get(0), r.get(1))).collect(),
        dated: dated
            .iter()
            .filter_map(|r| {
                let (seen, not_before): (f64, f64) = (r.get(2), r.get(6));
                let dated = git_date_off(Some(not_before), seen, r.get(3), r.get(4), r.get(5), DATED_BEHIND, DATED_AHEAD)?;
                Some(DatedCommit { project: r.get(0), sha: r.get(1), not_before, seen, dated })
            })
            .collect(),
    })
}

pub async fn for_user(st: &St, user_id: i64, tz: Tz, window: (f64, f64)) -> Res<Vec<Signal>> {
    let devices: HashMap<i64, String> = {
        let c = st.pool.get().await?;
        c.query("SELECT id, name FROM devices WHERE user_id = $1", &[&user_id]).await?.iter().map(|r| (r.get(0), r.get(1))).collect()
    };
    let ids: Vec<i64> = devices.keys().copied().collect();
    let (pieces, facts) = tokio::try_join!(load_pieces(st, user_id, window.0, window.1), load(st, user_id, &ids, window))?;
    let spans: Vec<&Span> = pieces.iter().map(|p| &p.span).collect();
    Ok(detect(&Inputs { tz, devices: &devices, spans: &spans, facts: &facts }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-06-01T00:00:00Z, a Monday.
    const D: f64 = 1_780_272_000.0;
    const H: f64 = 3600.0;

    fn span(device_id: i64, start: f64, end: f64, project: Option<&str>) -> Span {
        Span { start: D + start, end: D + end, project: project.map(String::from), class: "editing".into(), device_id }
    }

    fn devices() -> HashMap<i64, String> {
        HashMap::from([(1, "laptop".to_string()), (2, "desktop".to_string())])
    }

    fn run(spans: &[Span], facts: &Facts) -> Vec<Signal> {
        let devices = devices();
        let refs: Vec<&Span> = spans.iter().collect();
        detect(&Inputs { tz: chrono_tz::UTC, devices: &devices, spans: &refs, facts })
    }

    fn ids(signals: &[Signal]) -> Vec<&'static str> {
        signals.iter().map(|s| s.id).collect()
    }

    fn seen(device_id: i64, focus: &str, idle: &str, ts: f64) -> Backends {
        Backends { device_id, ts: D + ts, focus: focus.into(), idle: idle.into() }
    }

    /// Backends that can see everything, reported before any credit, so only the signal under test fires.
    fn sighted() -> Facts {
        Facts { backends: vec![seen(1, "x11", "x11-screensaver", -H), seen(2, "x11", "x11-screensaver", -H)], ..Facts::default() }
    }

    fn one<'a>(signals: &'a [Signal], id: &str) -> &'a Signal {
        signals.iter().find(|s| s.id == id).unwrap_or_else(|| panic!("no {id} in {:?}", ids(signals)))
    }

    #[test]
    fn a_long_day_is_more_than_twelve_hours_on_one_local_day() {
        let facts = sighted();
        let long = [span(1, 6.0 * H, 10.0 * H, Some("a/b")), span(1, 10.0 * H, 18.5 * H, None)];
        let s = run(&long, &facts);
        assert_eq!(ids(&s), ["long_day"]);
        let e = &one(&s, "long_day").evidence;
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].day, NaiveDate::from_ymd_opt(2026, 6, 1));
        assert_eq!(e[0].text, "12 hr 30 min of focus time");

        let split = [span(1, 18.0 * H, 31.0 * H, None)];
        assert!(run(&split, &facts).is_empty(), "13 hours across midnight are two ordinary days");
        assert!(run(&[span(1, 0.0, 12.0 * H, None)], &facts).is_empty(), "exactly 12 hours is fine");
    }

    #[test]
    fn unbroken_input_needs_four_hours_without_a_gap() {
        let mut facts = sighted();
        facts.presence = vec![Presence { device_id: 1, start: D + H, end: D + 5.0 * H }];
        let s = run(&[], &facts);
        assert_eq!(ids(&s), ["unbroken_input"]);
        assert_eq!(s[0].evidence[0].text, "01:00 to 05:00 on laptop, 4 hr with input every minute");

        facts.presence = vec![
            Presence { device_id: 1, start: D + H, end: D + 3.0 * H },
            Presence { device_id: 1, start: D + 3.0 * H + 120.0, end: D + 5.0 * H },
        ];
        assert!(run(&[], &facts).is_empty(), "a break splits the run");
    }

    #[test]
    fn overlap_counts_simultaneous_credit_on_two_devices() {
        let facts = sighted();
        let both = [span(1, H, 2.0 * H, Some("a/b")), span(2, 1.5 * H, 3.0 * H, None)];
        let s = run(&both, &facts);
        assert_eq!(ids(&s), ["overlap"]);
        assert_eq!(s[0].evidence[0].text, "30 min on laptop and desktop at once, from 01:30");

        let short = [span(1, H, 2.0 * H, Some("a/b")), span(2, 2.0 * H - 300.0, 3.0 * H, None)];
        assert!(run(&short, &facts).is_empty(), "five minutes is under the threshold");
        let touching = [span(1, H, 2.0 * H, Some("a/b")), span(2, 2.0 * H, 3.0 * H, None)];
        assert!(run(&touching, &facts).is_empty(), "back to back is not at once");
        let same_device = [span(1, H, 2.0 * H, Some("a/b")), span(1, 1.5 * H, 3.0 * H, None)];
        assert!(run(&same_device, &facts).is_empty(), "one device can't overlap itself");
    }

    #[test]
    fn blind_devices_are_those_without_a_focus_or_idle_backend() {
        let work = [span(1, H, 2.0 * H, Some("a/b")), span(2, H, 1.1 * H, Some("a/b"))];
        let mut facts = Facts { backends: vec![seen(1, "none", "x11-screensaver", 0.0), seen(2, "x11", "x11-screensaver", 0.0)], ..Facts::default() };
        let s = run(&work, &facts);
        let b = one(&s, "blind_device");
        assert_eq!((b.level, b.evidence.len()), (Level::Info, 1));
        assert_eq!(b.evidence[0].text, "laptop: 1 hr of focus time (focus none, idle x11-screensaver)");

        facts.backends[0] = seen(1, "x11", "x11-screensaver", 1.5 * H);
        let s = run(&work, &facts);
        assert_eq!(one(&s, "blind_device").evidence[0].text, "laptop: 30 min of focus time (no backends reported)");

        facts.backends[0] = seen(1, "x11", "x11-screensaver", 0.0);
        assert!(!ids(&run(&work, &facts)).contains(&"blind_device"));
    }

    #[test]
    fn late_uploads_cover_the_credit_their_events_made() {
        let work = [span(1, H, 3.0 * H, Some("a/b"))];
        let mut facts = sighted();
        facts.late = vec![
            LateBatch { device_id: 1, received: D + 4.0 * 86_400.0, first: D + H + 60.0, last: D + 2.0 * H },
            LateBatch { device_id: 1, received: D + 4.0 * 86_400.0 + 60.0, first: D + 2.0 * H, last: D + 2.5 * H },
        ];
        let s = run(&work, &facts);
        let l = one(&s, "late_upload");
        assert_eq!(l.level, Level::Info);
        assert_eq!(l.evidence.len(), 1, "touching batches are one stretch");
        assert_eq!(l.evidence[0].text, "01:00 to 02:30 on laptop: 1 hr 30 min of focus time, uploaded 3 days later");

        facts.late = vec![LateBatch { device_id: 1, received: D + 13.0 * 86_400.0, first: D - 6.0 * 86_400.0, last: D + 2.0 * H }];
        assert!(one(&run(&work, &facts), "late_upload").evidence[0].text.ends_with("uploaded 12 to 19 days later"));
        assert_eq!(later(30.0 * H, 70.0 * H), "30 hours to 2 days");

        facts.late = vec![LateBatch { device_id: 2, received: D + 4.0 * 86_400.0, first: D + H, last: D + 2.0 * H }];
        assert!(run(&work, &facts).is_empty(), "late events from a device with no credit then made no focus time");
    }

    #[test]
    fn no_output_is_long_project_time_without_commits_or_saves() {
        let work = [span(1, 0.0, 3.0 * H, Some("a/b")), span(2, 2.0 * H, 5.5 * H, Some("a/b")), span(1, 6.0 * H, 12.0 * H, None)];
        let mut facts = sighted();
        let s = run(&work, &facts);
        let n = one(&s, "no_output");
        assert_eq!(n.evidence.len(), 1, "Other is not a project");
        assert_eq!(n.evidence[0].text, "a/b: 5 hr 30 min of focus time, no commits, no saved files");

        facts.saves.insert("a/b".into(), 3);
        assert!(!ids(&run(&work, &facts)).contains(&"no_output"));
        facts.saves.clear();
        facts.commits.insert("a/b".into(), 1);
        assert!(!ids(&run(&work, &facts)).contains(&"no_output"));
        facts.commits.clear();
        assert!(!ids(&run(&work[..1], &facts)).contains(&"no_output"), "3 hours is under the threshold");
    }

    #[test]
    fn notices_come_before_info_and_evidence_is_capped() {
        let mut facts = Facts::default();
        let days: Vec<Span> = (0..20).map(|d| span(1, d as f64 * 24.0 * H, d as f64 * 24.0 * H + 13.0 * H, Some("a/b"))).collect();
        facts.saves.insert("a/b".into(), 1);
        let s = run(&days, &facts);
        assert_eq!(ids(&s), ["long_day", "blind_device"]);
        assert_eq!((s[0].evidence.len(), s[0].more), (MAX_EVIDENCE, 20 - MAX_EVIDENCE));
    }

    #[test]
    fn input_seen_counts_each_minute_once() {
        let p = [
            Presence { device_id: 1, start: D, end: D + 600.0 },
            Presence { device_id: 2, start: D + 300.0, end: D + 900.0 },
        ];
        assert_eq!(input_seen(&p, (D, D + 86_400.0)), 900.0);
        assert_eq!(input_seen(&p, (D + 600.0, D + 700.0)), 100.0);
    }
}
