use std::collections::BTreeMap;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Datelike, Days, NaiveDate};
use chrono_tz::Tz;
use kairos_core::union;

use super::pages::{ago, kind, picked, picker, project_path, rows, spoken, tool_name, DayBar, Picker, Row};
use super::{avatar, render, Base, Ctx};
use crate::aliases::{self, canon_commits, canon_projects};
use crate::api::events::repo_url;
use crate::api::stats::{languages, load_spans, user_by_name};
use crate::credit::{day_start, local_day};
use crate::live::{self, Bar};
use crate::tokens::{self, ProjectView};
use crate::util::{err, human, now, short, Res};
use crate::St;

const HEAT_DAYS: u64 = 364;
const COMMITS: i64 = 15;

pub struct Cell {
    pub level: u8,
    pub title: String,
}

pub struct Commit {
    pub short: String,
    pub url: Option<String>,
    pub when: String,
    pub files: i32,
    pub added: i32,
    pub removed: i32,
}

#[derive(Template)]
#[template(path = "project.html")]
struct Page {
    base: Base,
    username: String,
    name: String,
    avatar: String,
    project: String,
    repo: Option<String>,
    path: String,
    own: bool,
    hidden: bool,
    public: bool,
    total: String,
    week: String,
    agent: String,
    commit_count: i64,
    first: String,
    last: String,
    bars: Vec<Bar>,
    days: Vec<DayBar>,
    first_label: String,
    heat: Vec<Cell>,
    picker: Picker,
    classes: Vec<Row>,
    languages: Vec<Row>,
    tools: Option<Vec<Row>>,
    tokens: Option<ProjectView>,
    commits: Vec<Commit>,
}

#[derive(serde::Deserialize)]
pub struct PickQ {
    from: Option<String>,
    to: Option<String>,
}

fn date(tz: Tz, ts: f64) -> String {
    DateTime::from_timestamp(ts as i64, 0).map_or_else(String::new, |d| d.with_timezone(&tz).format("%b %-d, %Y").to_string())
}

fn level(v: f64, max: f64) -> u8 {
    if v <= 0.0 || max <= 0.0 {
        0
    } else {
        ((v / max * 4.0).ceil() as u8).clamp(1, 4)
    }
}

/// A year of days as columns of seven, starting on a Sunday, ending today.
fn heat(per_day: &BTreeMap<NaiveDate, (f64, f64)>, today: NaiveDate) -> Vec<Cell> {
    let from = today.checked_sub_days(Days::new(HEAT_DAYS)).unwrap_or(today);
    let from = from.checked_sub_days(Days::new(u64::from(from.weekday().num_days_from_sunday()))).unwrap_or(from);
    let max = per_day.range(from..=today).map(|(_, v)| v.0).fold(0.0, f64::max);
    let mut out = Vec::new();
    let mut d = from;
    while d <= today {
        let v = per_day.get(&d).map_or(0.0, |v| v.0);
        let title = if v > 0.0 { format!("{} · {}", d.format("%a %b %-d, %Y"), human(v)) } else { d.format("%a %b %-d, %Y").to_string() };
        out.push(Cell { level: level(v, max), title });
        match d.succ_opt() {
            Some(n) => d = n,
            None => break,
        }
    }
    out
}

pub async fn page(State(st): State<St>, ctx: Ctx, Path((username, project)): Path<(String, String)>, Query(q): Query<PickQ>) -> Res<Response> {
    let not_found = || err(StatusCode::NOT_FOUND, "no such project");
    let u = user_by_name(&st, &username).await?.ok_or_else(not_found)?;
    let own = ctx.session.as_ref().is_some_and(|s| s.user.id == u.id);
    if !u.is_public && !own {
        return Err(not_found());
    }
    let c = st.pool.get().await?;
    let map = aliases::map(&c, u.id).await?;
    let moved = aliases::canonical(&map, &project).to_string();
    let row = c
        .query_opt(&format!("SELECT repo_url, hidden, first_seen, last_seen FROM {} p WHERE p.id = $2", canon_projects!()), &[&u.id, &moved])
        .await?
        .ok_or_else(not_found)?;
    let (repo_col, hidden, first_seen, last_seen): (Option<String>, bool, f64, f64) = (row.get(0), row.get(1), row.get(2), row.get(3));
    if hidden && !own {
        return Err(not_found());
    }
    if moved != project {
        return Ok(Redirect::to(&project_path(&u.username, &moved)).into_response());
    }
    let repo = repo_col.or_else(|| repo_url(&project)).filter(|r| r.starts_with("https://"));

    let tz = u.tz();
    let today = local_day(tz, now());
    let (from, to) = picked(q.from.as_deref(), q.to.as_deref(), today);
    let mut per_day: BTreeMap<NaiveDate, (f64, f64)> = BTreeMap::new();
    let mut classes: BTreeMap<String, f64> = BTreeMap::new();
    for r in c
        .query(
            "SELECT day, class, sum(seconds)::float8 FROM user_days WHERE user_id = $1 AND project = $2 AND day <= $3 GROUP BY day, class",
            &[&u.id, &project, &today],
        )
        .await?
    {
        let (day, class, secs): (NaiveDate, String, f64) = (r.get(0), r.get(1), r.get(2));
        let e = per_day.entry(day).or_default();
        if class == "agent" {
            e.1 += secs;
        } else {
            e.0 += secs;
            if day >= from && day <= to {
                *classes.entry(class).or_default() += secs;
            }
        }
    }
    let all = c
        .query_one(
            "SELECT COALESCE(sum(seconds) FILTER (WHERE class <> 'agent'), 0)::float8, COALESCE(sum(seconds) FILTER (WHERE class = 'agent'), 0)::float8
             FROM user_days WHERE user_id = $1 AND project = $2",
            &[&u.id, &project],
        )
        .await?;
    let (total, agent): (f64, f64) = (all.get(0), all.get(1));
    drop(c);

    let week_days: Vec<(NaiveDate, f64, f64)> = (0..7)
        .rev()
        .filter_map(|i| today.checked_sub_days(Days::new(i)))
        .map(|d| {
            let v = per_day.get(&d).copied().unwrap_or_default();
            (d, v.0, v.1)
        })
        .collect();
    let week: f64 = week_days.iter().map(|d| d.1).sum();
    let dates: Vec<NaiveDate> = (0..30).rev().filter_map(|i| today.checked_sub_days(Days::new(i))).collect();
    let max = dates.iter().filter_map(|d| per_day.get(d)).map(|v| v.0).fold(0.0, f64::max);
    let days = dates
        .iter()
        .map(|d| {
            let v = per_day.get(d).map_or(0.0, |v| v.0);
            DayBar {
                title: format!("{} · {}", d.format("%a %b %-d"), human(v)),
                pct: format!("{:.1}", if max > 0.0 { v / max * 100.0 } else { 0.0 }),
                day: d.to_string(),
                on: (from..=to).contains(d),
            }
        })
        .collect();
    let path = project_path(&u.username, &project);
    let focus: BTreeMap<NaiveDate, f64> = per_day.iter().map(|(d, v)| (*d, v.0)).collect();
    let picker = picker(&path, from, to, today, &focus)?;
    let picked_focus: f64 = focus.range(from..=to).map(|d| d.1).sum();

    let lo = day_start(tz, from);
    let hi = match to.succ_opt() {
        Some(next) if to < today => day_start(tz, next),
        _ => now() + 60.0,
    };
    let spans = load_spans(&st, u.id, lo, hi).await?;
    let credited = union(spans.iter().filter(|s| s.project.as_deref() == Some(project.as_str())).map(|s| (s.start, s.end)).collect());
    let langs = languages(&st, u.id, &credited, lo, hi).await?;
    let c = st.pool.get().await?;

    let mut class_list: Vec<(String, f64)> = classes.into_iter().filter(|r| r.1 > 0.0).collect();
    class_list.sort_by(|a, b| b.1.total_cmp(&a.1));
    let classes = rows(
        class_list,
        |c| {
            let (l, col) = kind(c);
            Some((l.to_string(), col))
        },
        |_| None,
    );
    let languages = rows(langs, |_| None, |_| None);

    let tools = if u.show_ai_tools || own {
        let list: Vec<(String, f64)> = c
            .query(
                "SELECT harness, sum(prompts)::float8 FROM user_tool_days WHERE user_id = $1 AND project = $2 AND day >= $3 AND day <= $4
                 GROUP BY harness ORDER BY 2 DESC, 1 LIMIT 12",
                &[&u.id, &project, &from, &to],
            )
            .await?
            .iter()
            .map(|r| (r.get(0), r.get(1)))
            .collect();
        let top = list.first().map_or(0.0, |r| r.1);
        Some(
            list.into_iter()
                .map(|(id, n)| Row {
                    label: tool_name(&id),
                    href: None,
                    color: None,
                    pct: format!("{:.1}", if top > 0.0 { n / top * 100.0 } else { 0.0 }),
                    time: format!("{} {}", n as i64, if n as i64 == 1 { "prompt" } else { "prompts" }),
                })
                .collect(),
        )
    } else {
        None
    };

    let token_view = if own || tokens::settings(&c, u.id).await?.public {
        let mut ids = vec![project.clone()];
        ids.extend(map.iter().filter(|(_, canon)| **canon == project).map(|(alias, _)| alias.clone()));
        if tokens::any(&c, u.id, Some(&ids)).await? {
            Some(tokens::project_view(&c, u.id, tz, &ids, from, to, picked_focus).await?)
        } else {
            None
        }
    } else {
        None
    };

    let commit_count: i64 = c.query_one(&format!("SELECT count(*) FROM {} c WHERE c.project = $2", canon_commits!()), &[&u.id, &project]).await?.get(0);
    let github = repo.as_deref().filter(|r| r.starts_with("https://github.com/"));
    let t = now();
    let commits = c
        .query(
            &format!("SELECT sha, ts, files, added, removed FROM {} c WHERE c.project = $2 ORDER BY ts DESC, sha LIMIT $3", canon_commits!()),
            &[&u.id, &project, &COMMITS],
        )
        .await?
        .iter()
        .map(|r| {
            let (sha, ts): (String, f64) = (r.get(0), r.get(1));
            let ago_secs = t - ts;
            Commit {
                short: sha.chars().take(7).collect(),
                url: github.map(|g| format!("{g}/commit/{sha}")),
                when: if ago_secs < 7.0 * 86400.0 { ago(ago_secs) } else { date(tz, ts) },
                files: r.get(2),
                added: r.get(3),
                removed: r.get(4),
            }
        })
        .collect();
    drop(c);

    let name = u.display_name.clone().unwrap_or_else(|| u.username.clone());
    let description = format!("{name} has spent {} of focus time on {project} with Kairos, {} of it this week.", spoken(total), spoken(week));
    render(&Page {
        base: Base::new(&st, &ctx, &format!("{project} by {name} · Kairos")).share(&description, &path, "website"),
        avatar: avatar(&u.username),
        username: u.username.clone(),
        name,
        project,
        repo,
        path,
        own,
        hidden,
        public: u.is_public,
        total: short(total),
        week: short(week),
        agent: short(agent),
        commit_count,
        first: date(tz, first_seen),
        last: date(tz, last_seen),
        bars: live::bars(&week_days, true),
        days,
        first_label: dates.first().map(|d| d.format("%b %-d").to_string()).unwrap_or_default(),
        heat: heat(&per_day, today),
        picker,
        classes,
        languages,
        tools,
        tokens: token_view,
        commits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heat_levels_scale_to_the_busiest_day() {
        assert_eq!((level(0.0, 100.0), level(1.0, 100.0), level(50.0, 100.0), level(100.0, 100.0)), (0, 1, 2, 4));
    }

    #[test]
    fn the_heatmap_starts_on_a_sunday_and_ends_today() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        let cells = heat(&BTreeMap::new(), today);
        assert!(cells.len() >= 365 && cells.len() <= 371);
        assert!(cells.first().unwrap().title.starts_with("Sun"));
        assert!(cells.last().unwrap().title.starts_with("Fri Nov 20, 2026"));
    }
}
