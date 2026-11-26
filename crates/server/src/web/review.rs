//! Focus time is shown exactly as the read API computes it for a program's filter.
//! Staff only, and every view is audited.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use chrono::{DateTime, Days, NaiveDate};
use chrono_tz::Tz;
use futures_util::{stream, StreamExt, TryStreamExt};
use kairos_core::{total_secs, union, Interval};
use serde::Deserialize;
use serde_json::{json, Value};

use super::admin::{search, SearchQ, UserRow};
use super::pages::{ago, kind, os_name, tool_name, Row};
use super::{render, Base, Ctx};
use crate::accounts::{self, may_set_trust, TRUST, TRUST_REASON_MAX};
use crate::agent_work;
use crate::aliases::{self, canon_commits, canon_projects};
use crate::api::read::{carve, counts, parse_time};
use crate::api::stats::{by, load_agent_runs, load_pieces, AgentRun, Piece, Span};
use crate::audit;
use crate::auth::{Session, SignedIn, User, USER_COLUMNS};
use crate::credit::{day_start, local_day, PRIORITY};
use crate::github;
use crate::signals::{self, git_date_off, Signal};
use crate::util::{clean, err, human, now, refused_or_internal, short, urlenc, Res};
use crate::St;

const DAY: f64 = 86_400.0;
pub const MAX_DAYS: u64 = 92;
const DEFAULT_DAYS: u64 = 30;
const MAX_PROJECTS: usize = 50;
const ROWS_PER_DAY: usize = 300;
const COMMITS_SHOWN: i64 = 200;
const FILES_SHOWN: usize = 50;
const GIT_DATE_HINT: f64 = 3600.0;
const NOTES_SHOWN: i64 = 100;
const HISTORY_SHOWN: i64 = 50;
pub const NOTE_MAX: usize = 2000;
const HOME_TOP: i64 = 50;
const HOME_DAYS: u64 = 7;
// Signals for the home list load a few people at a time (two connections each) so one page view
// can't take the pool.
const HOME_PARALLEL: usize = 3;
const GITHUB_REPOS: usize = 10;
const GITHUB_COMMITS: usize = 500;
// Repos are checked a few at a time so one page view can't hold many GitHub requests open.
const GITHUB_PARALLEL: usize = 4;
const GITHUB_REPO_TIMEOUT: Duration = Duration::from_secs(40);
const SHAS_KNOWN: i64 = 20_000;
// Drawing only: gaps shorter than this inside one class are closed, the strip can't show them anyway.
const DRAW_JOIN: f64 = 60.0;
// Smallest drawn width, in percent of a day, so a one minute span still shows.
const MIN_WIDTH: f64 = 0.12;

fn staff(s: &Session) -> Res<()> {
    if s.user.is_staff() {
        Ok(())
    } else {
        Err(err(StatusCode::NOT_FOUND, "not found"))
    }
}

#[derive(Deserialize, Default)]
pub struct ReviewQ {
    pub projects: Option<String>,
    pub filter_by_project: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub projects: Vec<String>,
    pub start: f64,
    pub end: f64,
    pub first: NaiveDate,
    pub last: NaiveDate,
    pub program: bool,
    /// More than `MAX_DAYS` were asked for, so only the most recent ones are shown.
    pub cut: bool,
}

fn blank(o: &Option<String>) -> Option<&str> {
    o.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn minus_days(d: NaiveDate, n: u64) -> NaiveDate {
    d.checked_sub_days(Days::new(n)).unwrap_or(d)
}

fn next_day(d: NaiveDate) -> NaiveDate {
    d.checked_add_days(Days::new(1)).unwrap_or(d)
}

impl Filter {
    pub fn parse(q: &ReviewQ, tz: Tz, now: f64) -> Result<Filter, String> {
        let mut projects: Vec<String> = Vec::new();
        for p in blank(&q.projects).or(blank(&q.filter_by_project)).unwrap_or("").split(',') {
            let p = clean(p, 200);
            if !p.is_empty() && !projects.contains(&p) && projects.len() < MAX_PROJECTS {
                projects.push(p);
            }
        }
        let longest = MAX_DAYS as f64 * DAY;
        if blank(&q.start_date).is_some() || blank(&q.end_date).is_some() {
            let time = |v: &Option<String>, name: &str| blank(v).map(|s| parse_time(s).ok_or(format!("Invalid {name}"))).transpose();
            // Defaults and flooring as `/stats` does them; a missing start would be ten years back.
            let end = time(&q.end_date, "end_date")?.map_or(((now / DAY).floor() + 1.0) * DAY - 1.0, f64::floor);
            let start = time(&q.start_date, "start_date")?.map_or(f64::NEG_INFINITY, f64::floor);
            if start >= end {
                return Err("start_date must be before end_date".into());
            }
            let cut = end - start > longest;
            let start = if cut { end - longest } else { start };
            return Ok(Filter { projects, start, end, first: local_day(tz, start), last: local_day(tz, end - 0.001), program: true, cut });
        }
        let date = |v: &Option<String>, name: &str| {
            blank(v).map(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| format!("{name} is not a date like 2026-12-01"))).transpose()
        };
        let mut last = date(&q.to, "to")?.unwrap_or_else(|| local_day(tz, now));
        let mut first = date(&q.from, "from")?.unwrap_or_else(|| minus_days(last, DEFAULT_DAYS - 1));
        if first > last {
            std::mem::swap(&mut first, &mut last);
        }
        let cut = (last - first).num_days() >= MAX_DAYS as i64;
        if cut {
            first = minus_days(last, MAX_DAYS - 1);
        }
        Ok(Filter { projects, start: day_start(tz, first), end: day_start(tz, next_day(last)), first, last, program: false, cut })
    }

    fn only(&self) -> Option<&[String]> {
        (!self.projects.is_empty()).then_some(self.projects.as_slice())
    }

    fn query(&self, tz: Tz) -> String {
        let mut q = if self.program {
            format!("start_date={}&end_date={}", urlenc(&stamp(tz, self.start)), urlenc(&stamp(tz, self.end)))
        } else {
            format!("from={}&to={}", self.first, self.last)
        };
        if !self.projects.is_empty() {
            q.push_str(&format!("&projects={}", urlenc(&self.projects.join(","))));
        }
        q
    }

    fn api_path(&self, username: &str, tz: Tz) -> String {
        let mut p = format!("/api/v1/users/{username}/stats?start_date={}&end_date={}", urlenc(&stamp(tz, self.start)), urlenc(&stamp(tz, self.end)));
        if !self.projects.is_empty() {
            p.push_str(&format!("&filter_by_project={}", urlenc(&self.projects.join(","))));
        }
        p
    }
}

/// RFC 3339 in the person's timezone, which the read API parses back to the same instant.
fn stamp(tz: Tz, t: f64) -> String {
    DateTime::from_timestamp(t.floor() as i64, 0).map_or_else(String::new, |d| d.with_timezone(&tz).to_rfc3339())
}

fn at(tz: Tz, t: f64, fmt: &str) -> String {
    DateTime::from_timestamp(t.floor() as i64, 0).map_or_else(String::new, |d| d.with_timezone(&tz).format(fmt).to_string())
}

struct Person {
    user: User,
    slack_id: Option<String>,
    trust: String,
    created: String,
}

async fn person(st: &St, username: &str) -> Res<Option<Person>> {
    if username.len() > 64 {
        return Ok(None);
    }
    let c = st.pool.get().await?;
    let r = c
        .query_opt(
            &format!("SELECT {USER_COLUMNS}, u.slack_id, u.trust, to_char(u.created_at, 'FMMon FMDD, YYYY') AS created FROM users u WHERE u.username = lower($1)"),
            &[&username],
        )
        .await?;
    Ok(r.map(|r| Person { user: User::from_row(&r), slack_id: r.get("slack_id"), trust: r.get("trust"), created: r.get("created") }))
}

pub struct Seg {
    pub left: String,
    pub width: String,
    pub color: &'static str,
    pub title: String,
}

pub struct SpanRow {
    pub start: String,
    pub end: String,
    pub length: String,
    pub class: &'static str,
    pub color: &'static str,
    pub project: String,
    pub rule: String,
    pub device: String,
    pub counted: bool,
}

pub struct AgentRow {
    pub start: String,
    pub end: String,
    pub length: String,
    pub project: String,
    pub device: String,
}

pub struct Tick {
    pub left: String,
    pub title: String,
}

pub struct Day {
    pub id: String,
    pub label: String,
    pub total: String,
    pub seconds: f64,
    pub focus: Vec<Seg>,
    pub muted: Vec<Seg>,
    pub outside: Vec<Seg>,
    pub agent: Vec<Seg>,
    pub agent_muted: Vec<Seg>,
    pub ticks: Vec<Tick>,
    pub rows: Vec<SpanRow>,
    pub more: usize,
    pub agents: Vec<AgentRow>,
}

pub struct CommitRow {
    pub ts: f64,
    pub when: String,
    pub project: String,
    pub short: String,
    pub url: Option<String>,
    pub files: i32,
    pub added: i32,
    pub removed: i32,
    pub via: Option<&'static str>,
    pub git_date: Option<String>,
    pub changes: Vec<FileRow>,
    pub more_files: i32,
    pub agent: Option<(i64, i64, String)>,
    pub made_by: Option<String>,
}

pub struct FileRow {
    pub path: String,
    pub added: i64,
    pub removed: i64,
    pub agent: Option<(i64, i64)>,
}

fn via_label(via: Option<&str>) -> Option<&'static str> {
    match via? {
        "amend" => Some("amended"),
        "rebase" => Some("rebased"),
        "cherry_pick" => Some("cherry-picked"),
        "revert" => Some("reverted"),
        "arrived" => Some("made elsewhere"),
        _ => None,
    }
}

fn file_rows(changes: Option<Value>) -> Vec<FileRow> {
    let Some(Value::Array(list)) = changes else { return Vec::new() };
    list.iter()
        .filter_map(|c| {
            let (path, added, removed) = (c.get("path")?.as_str()?, c.get("added")?.as_i64()?, c.get("removed")?.as_i64()?);
            Some(FileRow { path: path.to_string(), added, removed, agent: None })
        })
        .collect()
}

fn tool_list(ids: &[String]) -> String {
    ids.iter().map(|t| tool_name(t)).collect::<Vec<_>>().join(", ")
}

pub struct DeviceInfo {
    pub name: String,
    pub os: String,
    pub version: Option<String>,
    pub focus: String,
    pub idle: String,
    pub focus_blind: bool,
    pub idle_blind: bool,
    pub first: String,
    pub last: String,
    pub events: i64,
    pub seen: String,
    pub revoked: Option<String>,
}

pub struct NoteRow {
    pub author: String,
    pub at: String,
    pub body: String,
}

pub struct TrustRow {
    pub at: String,
    pub actor: String,
    pub from: String,
    pub to: String,
    pub reason: String,
}

#[derive(Template)]
#[template(path = "review.html")]
struct Page {
    base: Base,
    name: String,
    username: String,
    slack_id: Option<String>,
    role: String,
    trust: String,
    created: String,
    timezone: String,
    public: bool,
    projects_value: String,
    known_projects: Vec<String>,
    from: String,
    to: String,
    range: String,
    program: bool,
    cut: bool,
    max_days: u64,
    api_path: String,
    back: String,
    official: String,
    official_secs: i64,
    agent: String,
    input: String,
    heartbeats: Option<(String, i64)>,
    not_counted: String,
    filtered: bool,
    signals: Vec<Signal>,
    days: Vec<Day>,
    by_project: Vec<Row>,
    by_class: Vec<Row>,
    commits: Vec<CommitRow>,
    commit_count: i64,
    commit_lines: i64,
    commits_more: i64,
    per_hour: Option<(String, String)>,
    agent_share: Option<(String, String)>,
    devices: Vec<DeviceInfo>,
    trust_options: Vec<&'static str>,
    trust_locked: Option<&'static str>,
    trust_history: Vec<TrustRow>,
    reason_max: usize,
    notes: Vec<NoteRow>,
    note_max: usize,
}

struct Extras {
    commits: Vec<CommitRow>,
    agent_lines: (i64, i64, usize),
    commit_count: i64,
    commit_lines: i64,
    heartbeats: i64,
    heartbeat_secs: f64,
    notes: Vec<NoteRow>,
    trust_history: Vec<TrustRow>,
    known_projects: Vec<String>,
}

async fn extras(st: &St, user_id: i64, tz: Tz, f: &Filter) -> Res<Extras> {
    let c = st.pool.get().await?;
    let only: Option<Vec<String>> = f.only().map(<[String]>::to_vec);
    let (commits, totals, beats, notes, history, known) = tokio::try_join!(
        async {
            c.query(
                concat!(
                    "SELECT c.ts, c.project, c.sha, c.files, c.added, c.removed, p.repo_url, c.via, c.committed_at,
                            c.changes, c.authored_at, c.device_id, c.not_before FROM ",
                    canon_commits!(),
                    " c LEFT JOIN projects p ON p.user_id = c.user_id AND p.id = c.project
                     WHERE c.ts >= $2 AND c.ts < $3 AND ($4::text[] IS NULL OR c.project = ANY($4))
                     ORDER BY c.ts DESC, c.sha LIMIT $5"
                ),
                &[&user_id, &f.start, &f.end, &only, &COMMITS_SHOWN],
            )
            .await
        },
        async {
            c.query_one(
                concat!(
                    "SELECT count(*), COALESCE(sum(added::int8 + removed::int8), 0)::int8 FROM ",
                    canon_commits!(),
                    " c WHERE ts >= $2 AND ts < $3 AND ($4::text[] IS NULL OR project = ANY($4))"
                ),
                &[&user_id, &f.start, &f.end, &only],
            )
            .await
        },
        async {
            c.query_one(
                "SELECT count(*), COALESCE(sum(LEAST(gap, 120)) FILTER (WHERE gap > 0), 0)::float8 FROM (
                     SELECT b.time - lag(b.time) OVER (ORDER BY b.time) AS gap FROM heartbeats b
                     LEFT JOIN project_aliases a ON a.user_id = b.user_id AND a.alias = b.project
                     WHERE b.user_id = $1 AND b.time >= $2 AND b.time < $3 AND ($4::text[] IS NULL OR COALESCE(a.canonical, b.project) = ANY($4))) h",
                &[&user_id, &f.start, &f.end, &only],
            )
            .await
        },
        async {
            c.query(
                "SELECT COALESCE(u.username, 'deleted'), to_char(n.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI'), n.body
                 FROM review_notes n LEFT JOIN users u ON u.id = n.author_id WHERE n.user_id = $1 ORDER BY n.id DESC LIMIT $2",
                &[&user_id, &NOTES_SHOWN],
            )
            .await
        },
        async {
            c.query(
                "SELECT to_char(a.at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI'), COALESCE(u.username, CASE WHEN a.actor_id IS NULL THEN 'system' ELSE 'deleted' END),
                        COALESCE(a.detail->>'from', ''), COALESCE(a.detail->>'to', ''), COALESCE(a.detail->>'reason', '')
                 FROM audit_log a LEFT JOIN users u ON u.id = a.actor_id
                 WHERE a.target_user_id = $1 AND a.action = 'trust.set' ORDER BY a.id DESC LIMIT $2",
                &[&user_id, &HISTORY_SHOWN],
            )
            .await
        },
        async { c.query(concat!("SELECT id FROM ", canon_projects!(), " p ORDER BY last_seen DESC LIMIT 200"), &[&user_id]).await },
    )?;
    let mut rows = Vec::with_capacity(commits.len());
    let mut seen = Vec::with_capacity(commits.len());
    for r in &commits {
        let (ts, sha, repo): (f64, String, Option<String>) = (r.get(0), r.get(2), r.get(6));
        let github = repo.filter(|u| u.starts_with("https://github.com/"));
        let via: Option<String> = r.get(7);
        let dated = git_date_off(r.get(12), ts, via.as_deref(), r.get(10), r.get(8), GIT_DATE_HINT, GIT_DATE_HINT);
        rows.push(CommitRow {
            ts,
            when: at(tz, ts, "%a %b %-d %H:%M"),
            project: r.get(1),
            short: sha.chars().take(7).collect(),
            url: github.map(|g| format!("{g}/commit/{sha}")),
            files: r.get(3),
            added: r.get(4),
            removed: r.get(5),
            via: via_label(via.as_deref()),
            git_date: dated.map(|t| at(tz, t as f64, "%b %-d, %Y %H:%M")),
            changes: file_rows(r.get(9)),
            more_files: 0,
            agent: None,
            made_by: None,
        });
        seen.push((sha, via, r.get::<_, Option<i64>>(11), r.get::<_, Option<i64>>(8)));
    }
    let refs: Vec<agent_work::Commit> = rows
        .iter()
        .zip(&seen)
        .map(|(row, (sha, via, device, committed))| agent_work::Commit {
            device: *device,
            project: &row.project,
            sha,
            ts: row.ts,
            committed: *committed,
            via: via.as_deref(),
            files: row.changes.iter().map(|f| (f.path.as_str(), f.added, f.removed)).collect(),
        })
        .collect();
    let work = agent_work::load(&c, user_id, &refs).await?;
    drop(refs);
    let mut agent_lines = (0, 0, 0);
    for (row, w) in rows.iter_mut().zip(work) {
        if w.compared {
            agent_lines.0 += w.added;
            agent_lines.1 += row.changes.iter().map(|f| f.added.max(0)).sum::<i64>();
            agent_lines.2 += 1;
        }
        if w.added + w.removed > 0 {
            row.agent = Some((w.added, w.removed, tool_list(&w.tools)));
        }
        row.made_by = w.made_by.map(|t| if t.is_empty() { "an agent".to_string() } else { tool_name(&t) });
        row.changes.truncate(FILES_SHOWN);
        for f in &mut row.changes {
            f.agent = w.files.get(&f.path).copied();
        }
        row.more_files = (row.files - row.changes.len() as i32).max(0);
    }
    Ok(Extras {
        commits: rows,
        agent_lines,
        commit_count: totals.get(0),
        commit_lines: totals.get(1),
        heartbeats: beats.get(0),
        heartbeat_secs: beats.get(1),
        notes: notes.iter().map(|r| NoteRow { author: r.get(0), at: r.get(1), body: r.get(2) }).collect(),
        trust_history: history.iter().map(|r| TrustRow { at: r.get(0), actor: r.get(1), from: r.get(2), to: r.get(3), reason: r.get(4) }).collect(),
        known_projects: known.iter().map(|r| r.get(0)).collect(),
    })
}

async fn load_devices(st: &St, user_id: i64, tz: Tz) -> Res<Vec<(i64, DeviceInfo)>> {
    let c = st.pool.get().await?;
    let rows = c
        .query(
            "SELECT id, name, os, client_version, COALESCE(backends->>'focus', ''), COALESCE(backends->>'idle', ''),
                    first_event_ts, last_event_ts, event_count, extract(epoch FROM now() - last_seen_at)::float8,
                    to_char(revoked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD')
             FROM devices WHERE user_id = $1 ORDER BY revoked_at IS NOT NULL, id",
            &[&user_id],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let (focus, idle): (String, String) = (r.get(4), r.get(5));
            let when = |t: Option<f64>| t.map_or_else(|| "none".to_string(), |t| at(tz, t, "%b %-d, %Y %H:%M"));
            let info = DeviceInfo {
                name: r.get(1),
                os: os_name(&r.get::<_, String>(2)),
                version: r.get(3),
                focus_blind: focus.is_empty() || focus == "none",
                idle_blind: idle.is_empty() || idle == "none",
                focus: if focus.is_empty() { "not reported".into() } else { focus },
                idle: if idle.is_empty() { "not reported".into() } else { idle },
                first: when(r.get(6)),
                last: when(r.get(7)),
                events: r.get(8),
                seen: r.get::<_, Option<f64>>(9).map_or_else(|| "never".to_string(), ago),
                revoked: r.get(10),
            };
            (r.get(0), info)
        })
        .collect())
}

fn minus(a: &[Interval], b: &[Interval]) -> Vec<Interval> {
    let mut out = Vec::new();
    let mut j = 0;
    for &(s, e) in a {
        let mut cur = s;
        while j < b.len() && b[j].1 <= cur {
            j += 1;
        }
        let mut k = j;
        while k < b.len() && b[k].0 < e {
            if b[k].0 > cur {
                out.push((cur, b[k].0));
            }
            cur = cur.max(b[k].1);
            k += 1;
        }
        if cur < e {
            out.push((cur, e));
        }
    }
    out
}

fn layer<'a>(spans: &[&'a Span]) -> Vec<(f64, f64, &'a str)> {
    let mut classes: Vec<&'a str> = PRIORITY.to_vec();
    for s in spans {
        if !classes.contains(&s.class.as_str()) {
            classes.push(s.class.as_str());
        }
    }
    let mut covered: Vec<Interval> = Vec::new();
    let mut out = Vec::new();
    for class in classes {
        let own = union(spans.iter().filter(|s| s.class == class).map(|s| (s.start, s.end)).collect());
        out.extend(minus(&own, &covered).into_iter().map(|(s, e)| (s, e, class)));
        covered = union(covered.into_iter().chain(own).collect());
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

fn split(tz: Tz, s: f64, e: f64) -> Vec<(NaiveDate, f64, f64)> {
    let mut out = Vec::new();
    let mut t = s;
    while t < e {
        let d = local_day(tz, t);
        let next = day_start(tz, next_day(d));
        let cut = if next > t { e.min(next) } else { e };
        out.push((d, t, cut));
        t = cut;
    }
    out
}

fn joined<'a>(pieces: impl IntoIterator<Item = (f64, f64, &'a str)>) -> Vec<(f64, f64, &'a str)> {
    let mut out: Vec<(f64, f64, &str)> = Vec::new();
    for p in pieces {
        match out.last_mut() {
            Some(l) if l.2 == p.2 && p.0 - l.1 < DRAW_JOIN => l.1 = l.1.max(p.1),
            _ => out.push(p),
        }
    }
    out
}

struct Frame {
    tz: Tz,
    d0: f64,
    len: f64,
}

impl Frame {
    fn new(tz: Tz, d: NaiveDate) -> Frame {
        let d0 = day_start(tz, d);
        Frame { tz, d0, len: (day_start(tz, next_day(d)) - d0).max(1.0) }
    }

    fn end(&self) -> f64 {
        self.d0 + self.len
    }

    fn pct(&self, t: f64) -> f64 {
        ((t - self.d0) / self.len * 100.0).clamp(0.0, 100.0)
    }

    fn seg(&self, s: f64, e: f64, color: &'static str, title: String) -> Seg {
        let left = self.pct(s);
        let width = (self.pct(e) - left).max(MIN_WIDTH).min(100.0 - left);
        Seg { left: format!("{left:.3}"), width: format!("{width:.3}"), color, title }
    }

    fn clock(&self, t: f64, fmt: &str, midnight: &str) -> String {
        if t >= self.end() {
            midnight.to_string()
        } else {
            at(self.tz, t, fmt)
        }
    }

    fn range(&self, s: f64, e: f64) -> String {
        format!("{} to {}", at(self.tz, s, "%H:%M"), self.clock(e, "%H:%M", "24:00"))
    }
}

struct Timeline<'a> {
    tz: Tz,
    filter: &'a Filter,
    pieces: &'a [Piece],
    counted: &'a [&'a Span],
    agents: &'a [AgentRun],
    commits: &'a [CommitRow],
    names: &'a HashMap<i64, String>,
}

type DayMap = BTreeMap<NaiveDate, (Frame, Day)>;

impl Timeline<'_> {
    fn device(&self, id: i64) -> String {
        self.names.get(&id).cloned().unwrap_or_else(|| format!("device {id}"))
    }

    fn counts(&self, project: Option<&str>) -> bool {
        counts(project, self.filter.only(), &HashSet::new())
    }

    fn days(&self) -> Vec<Day> {
        let mut days = self.frames();
        self.focus(&mut days);
        self.agents(&mut days);
        self.ticks(&mut days);
        self.tables(&mut days);
        days.into_values()
            .rev()
            .map(|(_, mut day)| {
                day.total = short(day.seconds);
                day
            })
            .collect()
    }

    fn frames(&self) -> DayMap {
        let f = self.filter;
        let mut days = DayMap::new();
        let mut d = f.first;
        while d <= f.last {
            let frame = Frame::new(self.tz, d);
            let mut outside = Vec::new();
            if f.start > frame.d0 {
                outside.push(frame.seg(frame.d0, f.start.min(frame.end()), "", "Outside this window".into()));
            }
            if f.end < frame.end() {
                outside.push(frame.seg(f.end.max(frame.d0), frame.end(), "", "Outside this window".into()));
            }
            let day = Day {
                id: format!("d-{d}"),
                label: d.format("%a %b %-d").to_string(),
                total: String::new(),
                seconds: 0.0,
                focus: Vec::new(),
                muted: Vec::new(),
                outside,
                agent: Vec::new(),
                agent_muted: Vec::new(),
                ticks: Vec::new(),
                rows: Vec::new(),
                more: 0,
                agents: Vec::new(),
            };
            days.insert(d, (frame, day));
            d = next_day(d);
        }
        days
    }

    fn focus(&self, days: &mut DayMap) {
        let mut drawn: BTreeMap<NaiveDate, Vec<(f64, f64, &str)>> = BTreeMap::new();
        for (s, e, class) in layer(self.counted) {
            for (d, s, e) in split(self.tz, s, e) {
                if let Some((_, day)) = days.get_mut(&d) {
                    day.seconds += e - s;
                    drawn.entry(d).or_default().push((s, e, class));
                }
            }
        }
        for (d, pieces) in drawn {
            if let Some((frame, day)) = days.get_mut(&d) {
                for (s, e, class) in joined(pieces) {
                    let (label, color) = kind(class);
                    day.focus.push(frame.seg(s, e, color, format!("{}, {label}, {}", frame.range(s, e), human(e - s))));
                }
            }
        }

        let all = union(self.pieces.iter().map(|p| (p.span.start, p.span.end)).collect());
        let counted = union(self.counted.iter().map(|s| (s.start, s.end)).collect());
        let why = if self.filter.projects.is_empty() { "Other (no project), not counted" } else { "outside the filter, not counted" };
        for (s, e) in minus(&all, &counted) {
            for (d, s, e) in split(self.tz, s, e) {
                if let Some((frame, day)) = days.get_mut(&d) {
                    day.muted.push(frame.seg(s, e, "", format!("{}, {why}", frame.range(s, e))));
                }
            }
        }
    }

    fn agents(&self, days: &mut DayMap) {
        let on = union(self.agents.iter().filter(|a| self.counts(a.project.as_deref())).map(|a| (a.start, a.end)).collect());
        let off = minus(&union(self.agents.iter().map(|a| (a.start, a.end)).collect()), &on);
        for (iv, muted) in [(on, false), (off, true)] {
            for (s, e) in iv {
                for (d, s, e) in split(self.tz, s, e) {
                    if let Some((frame, day)) = days.get_mut(&d) {
                        let note = if muted { ", outside the filter" } else { "" };
                        let seg = frame.seg(s, e, "", format!("{}, agents ran {}{note}", frame.range(s, e), human(e - s)));
                        if muted {
                            day.agent_muted.push(seg);
                        } else {
                            day.agent.push(seg);
                        }
                    }
                }
            }
        }
    }

    fn ticks(&self, days: &mut DayMap) {
        for c in self.commits {
            if let Some((frame, day)) = days.get_mut(&local_day(self.tz, c.ts)) {
                let files = if c.files == 1 { "file" } else { "files" };
                day.ticks.push(Tick {
                    left: format!("{:.3}", frame.pct(c.ts)),
                    title: format!("{} in {} at {}: {} {files}, +{} -{}", c.short, c.project, at(self.tz, c.ts, "%H:%M"), c.files, c.added, c.removed),
                });
            }
        }
    }

    fn tables(&self, days: &mut DayMap) {
        for p in self.pieces {
            let counted = self.counts(p.span.project.as_deref());
            for (d, s, e) in split(self.tz, p.span.start, p.span.end) {
                if let Some((frame, day)) = days.get_mut(&d) {
                    if day.rows.len() >= ROWS_PER_DAY {
                        day.more += 1;
                        continue;
                    }
                    let (class, color) = kind(&p.span.class);
                    day.rows.push(SpanRow {
                        start: at(self.tz, s, "%H:%M:%S"),
                        end: frame.clock(e, "%H:%M:%S", "24:00:00"),
                        length: human(e - s),
                        class,
                        color,
                        project: p.span.project.clone().unwrap_or_else(|| "Other".into()),
                        rule: p.rule.clone(),
                        device: self.device(p.span.device_id),
                        counted,
                    });
                }
            }
        }
        for a in self.agents {
            for (d, s, e) in split(self.tz, a.start, a.end) {
                if let Some((frame, day)) = days.get_mut(&d) {
                    day.agents.push(AgentRow {
                        start: at(self.tz, s, "%H:%M:%S"),
                        end: frame.clock(e, "%H:%M:%S", "24:00:00"),
                        length: human(e - s),
                        project: a.project.clone().unwrap_or_else(|| "Other".into()),
                        device: self.device(a.device_id),
                    });
                }
            }
        }
    }
}

/// Not truncated: staff need the whole list.
fn breakdown(list: Vec<(String, f64)>, color: impl Fn(&str) -> Option<(String, &'static str)>, href: impl Fn(&str) -> Option<String>) -> Vec<Row> {
    let max = list.iter().map(|r| r.1).fold(0.0, f64::max);
    list.into_iter()
        .filter(|r| r.1 > 0.0)
        .map(|(n, v)| {
            let (label, c) = color(&n).map_or((n.clone(), None), |(l, c)| (l, Some(c)));
            Row { href: href(&n), label, color: c, pct: format!("{:.1}", if max > 0.0 { v / max * 100.0 } else { 0.0 }), time: human(v) }
        })
        .collect()
}

fn agent_share((agent, added, commits): (i64, i64, usize)) -> Option<(String, String)> {
    let of = format!("of lines added in {commits} {} from their devices.", if commits == 1 { "commit" } else { "commits" });
    (added > 0).then(|| (format!("{:.0}%", agent as f64 * 100.0 / added as f64), of))
}

fn per_hour(commits: i64, lines: i64, secs: f64) -> Option<(String, String)> {
    let hours = secs / 3600.0;
    (secs >= 60.0).then(|| (format!("{:.1}", commits as f64 / hours), format!("{:.0}", lines as f64 / hours)))
}

pub async fn person_page(
    State(st): State<St>,
    ctx: Ctx,
    SignedIn(s): SignedIn,
    Path(username): Path<String>,
    Query(q): Query<ReviewQ>,
) -> Res<Response> {
    staff(&s)?;
    let p = person(&st, &username).await?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such user"))?;
    let (uid, tz) = (p.user.id, p.user.tz());
    let mut f = Filter::parse(&q, tz, now()).map_err(|e| err(StatusCode::BAD_REQUEST, &e))?;
    f.projects = aliases::canonical_list(&st.pool.get().await?, uid, &f.projects).await?;
    {
        let mut c = st.pool.get().await?;
        let tx = c.transaction().await?;
        let detail = json!({ "page": "user", "from": f.first.to_string(), "to": f.last.to_string(), "start": f.start, "end": f.end, "projects": f.projects });
        audit::log(&tx, Some(s.user.id), "review.view", Some(uid), detail).await?;
        tx.commit().await?;
    }

    let devices = load_devices(&st, uid, tz).await?;
    let names: HashMap<i64, String> = devices.iter().map(|(id, d)| (*id, d.name.clone())).collect();
    let ids: Vec<i64> = devices.iter().map(|(id, _)| *id).collect();
    let (pieces, agents, facts, extra) = tokio::try_join!(
        load_pieces(&st, uid, f.start, f.end),
        load_agent_runs(&st, uid, f.start, f.end),
        signals::load(&st, uid, &ids, (f.start, f.end)),
        extras(&st, uid, tz, &f),
    )?;

    let empty = HashSet::new();
    let all: Vec<&Span> = pieces.iter().map(|p| &p.span).collect();
    let counted: Vec<&Span> = all.iter().copied().filter(|s| counts(s.project.as_deref(), f.only(), &empty)).collect();
    let official = total_secs(&union(counted.iter().map(|s| (s.start, s.end)).collect()));
    let everything = total_secs(&union(all.iter().map(|s| (s.start, s.end)).collect()));
    let agent = total_secs(&union(
        agents.iter().filter(|a| counts(a.project.as_deref(), f.only(), &empty)).map(|a| (a.start, a.end)).collect(),
    ));
    let signals = signals::detect(&signals::Inputs { tz, devices: &names, spans: &all, facts: &facts });

    let query = f.query(tz);
    let mut classes = carve(&counted);
    classes.sort_by(|a, b| b.1.total_cmp(&a.1));
    let by_class = breakdown(
        classes,
        |c| {
            let (l, col) = kind(c);
            Some((l.to_string(), col))
        },
        |_| None,
    );
    let by_project = breakdown(by(&counted, |s| s.project.clone().unwrap_or_default()), |_| None, |name| {
        let only = Filter { projects: vec![name.to_string()], ..f.clone() };
        Some(format!("/review/@{}?{}", p.user.username, only.query(tz)))
    });

    let days = Timeline { tz, filter: &f, pieces: &pieces, counted: &counted, agents: &agents, commits: &extra.commits, names: &names }.days();
    let (trust_options, trust_locked) = if s.user.id == uid {
        (Vec::new(), Some("You can't change your own trust. Ask another reviewer."))
    } else {
        let options: Vec<&'static str> = TRUST.iter().copied().filter(|t| *t != p.trust && may_set_trust(&s.user.role, &p.trust, t)).collect();
        let locked = options.is_empty().then_some("Only admins can lift a ban.");
        (options, locked)
    };
    let range = if f.first == f.last { f.first.format("%b %-d, %Y").to_string() } else { format!("{} to {}", f.first.format("%b %-d"), f.last.format("%b %-d, %Y")) };

    render(&Page {
        base: Base::new(&st, &ctx, &format!("Review @{} · Kairos", p.user.username)),
        name: p.user.display_name.clone().unwrap_or_else(|| p.user.username.clone()),
        username: p.user.username.clone(),
        slack_id: p.slack_id,
        role: p.user.role.clone(),
        trust: p.trust,
        created: p.created,
        timezone: p.user.timezone.clone(),
        public: p.user.is_public,
        projects_value: f.projects.join(","),
        known_projects: extra.known_projects,
        from: f.first.to_string(),
        to: f.last.to_string(),
        range,
        program: f.program,
        cut: f.cut,
        max_days: MAX_DAYS,
        api_path: f.api_path(&p.user.username, tz),
        back: query,
        official: short(official),
        official_secs: official.round() as i64,
        agent: short(agent),
        input: short(signals::input_seen(&facts.presence, (f.start, f.end))),
        heartbeats: (extra.heartbeats > 0).then(|| (short(extra.heartbeat_secs), extra.heartbeats)),
        not_counted: short(everything - official),
        filtered: !f.projects.is_empty(),
        signals,
        days,
        by_project,
        by_class,
        commits_more: extra.commit_count - extra.commits.len() as i64,
        commits: extra.commits,
        commit_count: extra.commit_count,
        commit_lines: extra.commit_lines,
        per_hour: per_hour(extra.commit_count, extra.commit_lines, official),
        agent_share: agent_share(extra.agent_lines),
        devices: devices.into_iter().map(|(_, d)| d).collect(),
        trust_options,
        trust_locked,
        trust_history: extra.trust_history,
        reason_max: TRUST_REASON_MAX,
        notes: extra.notes,
        note_max: NOTE_MAX,
    })
}

pub struct CheckRow {
    pub short: String,
    pub url: String,
    pub when: String,
    pub project: String,
    pub added: i32,
    pub removed: i32,
    pub label: &'static str,
    pub missing: bool,
    pub github: Option<(i64, i64)>,
}

pub struct UnseenRow {
    pub short: String,
    pub url: String,
    pub date: String,
    pub who: Option<String>,
    pub added: i64,
    pub removed: i64,
}

#[derive(Default)]
pub struct RepoCheck {
    pub name: String,
    pub url: String,
    pub error: Option<String>,
    pub matched: usize,
    pub differ: usize,
    pub missing: usize,
    pub rows: Vec<CheckRow>,
    pub unseen: Vec<UnseenRow>,
    pub history_more: bool,
}

#[derive(Template)]
#[template(path = "review_github.html")]
struct GithubPage {
    base: Base,
    name: String,
    username: String,
    range: String,
    timezone: String,
    filtered: bool,
    projects_value: String,
    back: String,
    configured: bool,
    repos: Vec<RepoCheck>,
    repos_capped: bool,
    commits_capped: bool,
    max_repos: usize,
    max_commits: usize,
    max_history: usize,
}

struct Mine {
    ts: f64,
    project: String,
    sha: String,
    added: i32,
    removed: i32,
}

struct Target {
    repo: github::Repo,
    mine: Vec<Mine>,
    known: Vec<String>,
}

struct Targets {
    list: Vec<Target>,
    repos_capped: bool,
    commits_capped: bool,
}

async fn github_targets(st: &St, user_id: i64, f: &Filter) -> Res<Targets> {
    let c = st.pool.get().await?;
    let only: Option<Vec<String>> = f.only().map(<[String]>::to_vec);
    let projects = c
        .query(
            concat!(
                "SELECT id, repo_url FROM ",
                canon_projects!(),
                " p WHERE last_seen >= $2 AND first_seen < $3 AND ($4::text[] IS NULL OR id = ANY($4))
                   AND starts_with(repo_url, 'https://github.com/')
                 ORDER BY last_seen DESC, id LIMIT 200"
            ),
            &[&user_id, &f.start, &f.end, &only],
        )
        .await?;
    let mut list: Vec<Target> = Vec::new();
    let mut of: HashMap<String, usize> = HashMap::new();
    let mut repos_capped = false;
    for r in &projects {
        let (id, url): (String, String) = (r.get(0), r.get(1));
        let Some(repo) = github::parse_repo(&url) else { continue };
        let i = match list.iter().position(|t| t.repo == repo) {
            Some(i) => i,
            None if list.len() >= GITHUB_REPOS => {
                repos_capped = true;
                continue;
            }
            None => {
                list.push(Target { repo, mine: Vec::new(), known: Vec::new() });
                list.len() - 1
            }
        };
        of.insert(id, i);
    }
    if list.is_empty() {
        return Ok(Targets { list, repos_capped, commits_capped: false });
    }
    let ids: Vec<String> = of.keys().cloned().collect();
    let limit = GITHUB_COMMITS as i64 + 1;
    let (commits, known) = tokio::try_join!(
        async {
            c.query(
                concat!(
                    "SELECT ts, project, sha, added, removed FROM ",
                    canon_commits!(),
                    " c WHERE ts >= $2 AND ts < $3 AND project = ANY($4) ORDER BY ts DESC, sha LIMIT $5"
                ),
                &[&user_id, &f.start, &f.end, &ids, &limit],
            )
            .await
        },
        async { c.query(concat!("SELECT project, sha FROM ", canon_commits!(), " c WHERE project = ANY($2) LIMIT $3"), &[&user_id, &ids, &SHAS_KNOWN]).await },
    )?;
    let commits_capped = commits.len() > GITHUB_COMMITS;
    for r in commits.iter().take(GITHUB_COMMITS) {
        let project: String = r.get(1);
        list[of[&project]].mine.push(Mine { ts: r.get(0), project, sha: r.get(2), added: r.get(3), removed: r.get(4) });
    }
    for r in &known {
        list[of[&r.get::<_, String>(0)]].known.push(r.get(1));
    }
    Ok(Targets { list, repos_capped, commits_capped })
}

async fn check_repo(st: &St, token: &str, t: Target, tz: Tz, window: &github::Window) -> RepoCheck {
    let mut shas: Vec<String> = Vec::new();
    for m in &t.mine {
        if !shas.contains(&m.sha) {
            shas.push(m.sha.clone());
        }
    }
    let mut out = RepoCheck { name: format!("{}/{}", t.repo.owner, t.repo.name), url: t.repo.url(), ..RepoCheck::default() };
    let api = github::Api { http: &st.http, url: &st.config.github_api, token };
    let fetched = match tokio::time::timeout(GITHUB_REPO_TIMEOUT, github::fetch(&api, &t.repo, &shas, window)).await {
        Ok(Ok(f)) => f,
        Ok(Err(e)) => return RepoCheck { error: Some(e), ..out },
        Err(_) => return RepoCheck { error: Some("GitHub timed out".into()), ..out },
    };
    let by_sha: HashMap<&str, Option<&github::Commit>> = shas.iter().map(String::as_str).zip(fetched.found.iter().map(Option::as_ref)).collect();
    for m in &t.mine {
        let found = by_sha.get(m.sha.as_str()).copied().flatten();
        let (label, github) = match github::verdict(m.added.into(), m.removed.into(), found) {
            github::Verdict::Match => {
                out.matched += 1;
                ("on GitHub, sizes match", None)
            }
            github::Verdict::Differ { added, removed } => {
                out.differ += 1;
                ("on GitHub, sizes differ", Some((added, removed)))
            }
            github::Verdict::Missing => {
                out.missing += 1;
                ("not on GitHub", None)
            }
        };
        out.rows.push(CheckRow {
            short: m.sha.chars().take(7).collect(),
            url: format!("{}/commit/{}", out.url, m.sha),
            when: at(tz, m.ts, "%a %b %-d %H:%M"),
            project: m.project.clone(),
            added: m.added,
            removed: m.removed,
            label,
            missing: found.is_none(),
            github,
        });
    }
    out.unseen = github::unseen(&fetched.history, &t.known)
        .into_iter()
        .map(|c| UnseenRow {
            short: c.oid.chars().take(7).collect(),
            url: format!("{}/commit/{}", out.url, c.oid),
            date: c.date.map_or_else(|| "unknown".into(), |d| at(tz, d as f64, "%b %-d, %Y %H:%M")),
            who: c.author.clone(),
            added: c.added,
            removed: c.removed,
        })
        .collect();
    out.history_more = fetched.more_history;
    out
}

pub async fn github_page(
    State(st): State<St>,
    ctx: Ctx,
    SignedIn(s): SignedIn,
    Path(username): Path<String>,
    Query(q): Query<ReviewQ>,
) -> Res<Response> {
    staff(&s)?;
    let p = person(&st, &username).await?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such user"))?;
    let (uid, tz) = (p.user.id, p.user.tz());
    let mut f = Filter::parse(&q, tz, now()).map_err(|e| err(StatusCode::BAD_REQUEST, &e))?;
    f.projects = aliases::canonical_list(&st.pool.get().await?, uid, &f.projects).await?;
    {
        let mut c = st.pool.get().await?;
        let tx = c.transaction().await?;
        let detail = json!({ "page": "github", "from": f.first.to_string(), "to": f.last.to_string(), "start": f.start, "end": f.end, "projects": f.projects });
        audit::log(&tx, Some(s.user.id), "review.view", Some(uid), detail).await?;
        tx.commit().await?;
    }
    let token = st.config.github_token.as_deref();
    let (mut repos, mut repos_capped, mut commits_capped) = (Vec::new(), false, false);
    if let Some(token) = token {
        let targets = github_targets(&st, uid, &f).await?;
        (repos_capped, commits_capped) = (targets.repos_capped, targets.commits_capped);
        let window = github::Window::new(f.start, f.end);
        repos = stream::iter(targets.list.into_iter().map(|t| check_repo(&st, token, t, tz, &window))).buffered(GITHUB_PARALLEL).collect().await;
    }
    let range = if f.first == f.last { f.first.format("%b %-d, %Y").to_string() } else { format!("{} to {}", f.first.format("%b %-d"), f.last.format("%b %-d, %Y")) };
    render(&GithubPage {
        base: Base::new(&st, &ctx, &format!("GitHub check @{} · Kairos", p.user.username)),
        name: p.user.display_name.clone().unwrap_or_else(|| p.user.username.clone()),
        username: p.user.username.clone(),
        range,
        timezone: p.user.timezone.clone(),
        filtered: !f.projects.is_empty(),
        projects_value: f.projects.join(","),
        back: f.query(tz),
        configured: token.is_some(),
        repos,
        repos_capped,
        commits_capped,
        max_repos: GITHUB_REPOS,
        max_commits: GITHUB_COMMITS,
        max_history: github::MAX_HISTORY,
    })
}

/// Only the query string comes from the form, and only characters `Filter::query` writes are kept.
fn back(username: &str, qs: &str, anchor: &str) -> String {
    let ok = qs.len() <= 2048 && qs.bytes().all(|b| b.is_ascii_alphanumeric() || b"%&=._~-".contains(&b));
    if ok && !qs.is_empty() {
        format!("/review/@{username}?{qs}#{anchor}")
    } else {
        format!("/review/@{username}#{anchor}")
    }
}

#[derive(Deserialize)]
pub struct TrustForm {
    csrf: String,
    trust: String,
    reason: String,
    #[serde(default)]
    back: String,
}

pub async fn set_trust(State(st): State<St>, SignedIn(s): SignedIn, Path(username): Path<String>, Form(f): Form<TrustForm>) -> Res<Response> {
    staff(&s)?;
    s.check_csrf(&f.csrf)?;
    let p = person(&st, &username).await?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such user"))?;
    if !may_set_trust(&s.user.role, &p.trust, &f.trust) {
        return Err(err(StatusCode::FORBIDDEN, "only admins can ban someone or lift a ban"));
    }
    accounts::set_trust(&st.pool, &s.user, p.user.id, &f.trust, &f.reason).await.map_err(refused_or_internal)?;
    Ok(Redirect::to(&back(&p.user.username, &f.back, "trust")).into_response())
}

#[derive(Deserialize)]
pub struct NoteForm {
    csrf: String,
    body: String,
    #[serde(default)]
    back: String,
}

fn note_body(raw: &str) -> Result<String, &'static str> {
    let body: String = raw.replace("\r\n", "\n").chars().filter(|c| *c == '\n' || !c.is_control()).collect();
    let body = body.trim();
    if body.is_empty() {
        return Err("write something first");
    }
    if body.chars().count() > NOTE_MAX {
        return Err("notes are at most 2000 characters");
    }
    Ok(body.to_string())
}

pub async fn add_note(State(st): State<St>, SignedIn(s): SignedIn, Path(username): Path<String>, Form(f): Form<NoteForm>) -> Res<Response> {
    staff(&s)?;
    s.check_csrf(&f.csrf)?;
    let p = person(&st, &username).await?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such user"))?;
    let body = note_body(&f.body).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    let mut c = st.pool.get().await?;
    let tx = c.transaction().await?;
    let id: i64 = tx
        .query_one("INSERT INTO review_notes (user_id, author_id, body) VALUES ($1, $2, $3) RETURNING id", &[&p.user.id, &s.user.id, &body])
        .await?
        .get(0);
    audit::log(&tx, Some(s.user.id), "review.note", Some(p.user.id), json!({ "note_id": id, "chars": body.chars().count() })).await?;
    tx.commit().await?;
    Ok(Redirect::to(&back(&p.user.username, &f.back, "notes")).into_response())
}

pub struct TopRow {
    pub username: String,
    pub name: Option<String>,
    pub href: String,
    pub focus: String,
    pub agent: String,
    pub pct: String,
    pub trust: String,
    pub public: bool,
    pub signals: usize,
}

#[derive(Template)]
#[template(path = "review_home.html")]
struct Home {
    base: Base,
    q: String,
    users: Vec<UserRow>,
    top: Vec<TopRow>,
    days: u64,
}

pub async fn home(State(st): State<St>, ctx: Ctx, SignedIn(s): SignedIn, Query(sq): Query<SearchQ>) -> Res<Response> {
    staff(&s)?;
    let q = clean(sq.q.as_deref().unwrap_or(""), 64);
    let (users, rows) = {
        let mut c = st.pool.get().await?;
        let tx = c.transaction().await?;
        audit::log(&tx, Some(s.user.id), "review.view", None, json!({ "page": "home", "q": q })).await?;
        let users = search(&tx, &q).await?;
        let rows = tx
            .query(
                "SELECT u.id, u.username, u.display_name, u.trust, u.timezone, u.is_public,
                        sum(d.seconds) FILTER (WHERE d.class <> 'agent') AS focus,
                        COALESCE(sum(d.seconds) FILTER (WHERE d.class = 'agent'), 0)
                 FROM user_days d JOIN users u ON u.id = d.user_id
                 -- A zone name Postgres doesn't know would fail the whole query, so it falls back to UTC.
                 LEFT JOIN pg_timezone_names z ON z.name = u.timezone
                 WHERE d.day > (now() AT TIME ZONE coalesce(z.name, 'UTC'))::date - $1::int
                   AND d.day <= (now() AT TIME ZONE coalesce(z.name, 'UTC'))::date
                 GROUP BY u.id HAVING sum(d.seconds) FILTER (WHERE d.class <> 'agent') > 0
                 ORDER BY focus DESC, u.id LIMIT $2",
                &[&(HOME_DAYS as i32), &HOME_TOP],
            )
            .await?;
        tx.commit().await?;
        (users, rows)
    };
    let max = rows.first().map_or(0.0, |r| r.get::<_, f64>(6));
    let t = now();
    let top: Vec<TopRow> = stream::iter(rows.into_iter().map(|r| {
        let st = st.clone();
        async move {
            let (id, username, timezone): (i64, String, String) = (r.get(0), r.get(1), r.get(4));
            let tz: Tz = timezone.parse().unwrap_or(chrono_tz::UTC);
            let last = local_day(tz, t);
            let first = minus_days(last, HOME_DAYS - 1);
            let found = signals::for_user(&st, id, tz, (day_start(tz, first), day_start(tz, next_day(last)))).await?;
            let (focus, agent): (f64, f64) = (r.get(6), r.get(7));
            Ok::<_, crate::util::AppError>(TopRow {
                href: format!("/review/@{username}?from={first}&to={last}"),
                username,
                name: r.get(2),
                focus: short(focus),
                agent: short(agent),
                pct: format!("{:.1}", if max > 0.0 { focus / max * 100.0 } else { 0.0 }),
                trust: r.get(3),
                public: r.get(5),
                signals: found.len(),
            })
        }
    }))
    .buffered(HOME_PARALLEL)
    .try_collect()
    .await?;
    render(&Home { base: Base::new(&st, &ctx, "Review · Kairos"), q, users, top, days: HOME_DAYS })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 1_795_478_400.0;

    fn q(pairs: &[(&str, &str)]) -> ReviewQ {
        let mut q = ReviewQ::default();
        for (k, v) in pairs {
            let v = Some(v.to_string());
            match *k {
                "projects" => q.projects = v,
                "filter_by_project" => q.filter_by_project = v,
                "from" => q.from = v,
                "to" => q.to = v,
                "start_date" => q.start_date = v,
                "end_date" => q.end_date = v,
                _ => unreachable!(),
            }
        }
        q
    }

    fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn local_days_default_to_the_last_thirty() {
        let tz: Tz = "America/New_York".parse().unwrap();
        let f = Filter::parse(&ReviewQ::default(), tz, NOW).unwrap();
        assert_eq!((f.first, f.last, f.program, f.cut), (date("2026-10-25"), date("2026-11-23"), false, false));
        assert_eq!(f.start, day_start(tz, date("2026-10-25")));
        assert_eq!(f.end, day_start(tz, date("2026-11-24")));
        assert!(f.projects.is_empty());
    }

    #[test]
    fn long_ranges_keep_the_most_recent_days() {
        let f = Filter::parse(&q(&[("from", "2026-01-01"), ("to", "2026-11-20")]), chrono_tz::UTC, NOW).unwrap();
        assert!(f.cut);
        assert_eq!((f.last - f.first).num_days() + 1, MAX_DAYS as i64);
        assert_eq!(f.last, date("2026-11-20"));
        let swapped = Filter::parse(&q(&[("from", "2026-11-20"), ("to", "2026-11-01")]), chrono_tz::UTC, NOW).unwrap();
        assert_eq!((swapped.first, swapped.last), (date("2026-11-01"), date("2026-11-20")));
        assert!(Filter::parse(&q(&[("from", "yesterday")]), chrono_tz::UTC, NOW).is_err());
    }

    #[test]
    fn program_parameters_are_read_like_the_api() {
        let tz: Tz = "Asia/Kolkata".parse().unwrap();
        let f = Filter::parse(&q(&[("start_date", "2026-11-01"), ("end_date", "2026-11-03"), ("filter_by_project", "a/b, c/d,a/b,")]), tz, NOW).unwrap();
        assert!(f.program && !f.cut);
        assert_eq!((f.start, f.end), (1_793_491_200.0, 1_793_664_000.0), "UTC midnights, as /stats reads them");
        assert_eq!((f.first, f.last), (date("2026-11-01"), date("2026-11-03")), "05:30 local to 05:30 local");
        assert_eq!(f.projects, ["a/b", "c/d"]);
        assert_eq!(
            f.api_path("ada", tz),
            "/api/v1/users/ada/stats?start_date=2026-11-01T05%3A30%3A00%2B05%3A30&end_date=2026-11-03T05%3A30%3A00%2B05%3A30&filter_by_project=a%2Fb%2Cc%2Fd"
        );
        let only_end = Filter::parse(&q(&[("end_date", "2026-11-03")]), tz, NOW).unwrap();
        assert!(only_end.cut && only_end.end - only_end.start == MAX_DAYS as f64 * DAY);
        assert!(Filter::parse(&q(&[("start_date", "nope")]), tz, NOW).is_err());
        assert!(Filter::parse(&q(&[("start_date", "2026-11-03"), ("end_date", "2026-11-01")]), tz, NOW).is_err());
    }

    #[test]
    fn the_query_string_round_trips() {
        let tz: Tz = "America/New_York".parse().unwrap();
        for pairs in [vec![("from", "2026-11-01"), ("to", "2026-11-05"), ("projects", "a/b,c")], vec![("start_date", "2026-11-01T10:00:00Z"), ("end_date", "2026-11-02")]] {
            let f = Filter::parse(&q(&pairs), tz, NOW).unwrap();
            let uri: axum::http::Uri = format!("/review/@ada?{}", f.query(tz)).parse().unwrap();
            let again = Query::<ReviewQ>::try_from_uri(&uri).unwrap().0;
            assert_eq!(Filter::parse(&again, tz, NOW).unwrap(), f);
        }
    }

    #[test]
    fn back_links_keep_only_our_own_query_strings() {
        assert_eq!(back("ada", "from=2026-11-01&to=2026-11-02&projects=a%2Fb", "notes"), "/review/@ada?from=2026-11-01&to=2026-11-02&projects=a%2Fb#notes");
        assert_eq!(back("ada", "x=1#y", "trust"), "/review/@ada#trust");
        assert_eq!(back("ada", "a=//evil.example", "trust"), "/review/@ada#trust");
        assert_eq!(back("ada", "", "notes"), "/review/@ada#notes");
    }

    #[test]
    fn notes_keep_line_breaks_and_drop_other_controls() {
        assert_eq!(note_body("  one\r\ntwo\u{7}  ").unwrap(), "one\ntwo");
        assert!(note_body(" \n ").is_err());
        assert!(note_body(&"x".repeat(NOTE_MAX + 1)).is_err());
        assert!(note_body(&"é".repeat(NOTE_MAX)).is_ok());
    }

    fn span(start: f64, end: f64, class: &str, device_id: i64) -> Span {
        Span { start, end, project: Some("a/b".into()), class: class.into(), device_id }
    }

    #[test]
    fn layers_give_each_second_one_class_by_priority() {
        let a = span(0.0, 100.0, "reading", 1);
        let b = span(50.0, 150.0, "editing", 2);
        let c = span(120.0, 200.0, "reading", 1);
        let out = layer(&[&a, &b, &c]);
        assert_eq!(out, vec![(0.0, 50.0, "reading"), (50.0, 150.0, "editing"), (150.0, 200.0, "reading")]);
        assert_eq!(minus(&[(0.0, 10.0), (20.0, 30.0)], &[(5.0, 25.0)]), vec![(0.0, 5.0), (25.0, 30.0)]);
    }

    #[test]
    fn days_split_at_local_midnight() {
        let tz: Tz = "Asia/Kolkata".parse().unwrap();
        let mid = day_start(tz, date("2026-03-10"));
        let parts = split(tz, mid - 600.0, mid + 300.0);
        assert_eq!(parts, vec![(date("2026-03-09"), mid - 600.0, mid), (date("2026-03-10"), mid, mid + 300.0)]);
    }
}
