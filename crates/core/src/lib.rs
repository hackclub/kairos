pub mod account;
pub mod brand;
pub mod credit;
pub mod event;
pub mod exclude;
#[cfg(target_os = "linux")]
pub mod gnome_extension;
#[cfg(feature = "client")]
pub mod loopback;
mod parse;
#[cfg(feature = "client")]
pub mod pairing;
pub mod paths;
pub mod pkce;
pub mod project;
pub mod shape;
pub mod sources;
pub mod tokens;
pub mod update;
pub mod upload;
pub mod wakatime;
#[cfg(feature = "client")]
pub mod wakatime_cli;

use std::collections::{BTreeMap, HashMap};

use chrono::{Duration, Local, NaiveDate, TimeZone};
use serde::Serialize;

pub use parse::{
    find_session_files, last_turn, parse_file, parse_session, prompt_stats, LastTurn, Session, Turn,
};
pub use shape::{prompt_shape, Shape};

pub(crate) const READ_WPM: f64 = 230.0;
pub(crate) const TYPE_WPM: f64 = 35.0;
pub(crate) const PASTE_TYPED_WORDS_CAP: usize = 60;
pub(crate) const MIN_PROMPT_WORDS: usize = 3;
const TOP_PROJECTS: usize = 10;

#[derive(Debug, Clone)]
pub struct Options {
    pub think_secs: f64,
    pub days: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options { think_secs: 120.0, days: 14 }
    }
}

#[derive(Debug, Serialize)]
pub struct Totals {
    pub human_hours: f64,
    pub agent_hours: f64,
    pub agent_to_human_ratio: f64,
    pub prompts: usize,
    pub sessions: usize,
    pub interrupts: usize,
}

#[derive(Debug, Serialize)]
pub struct DayRow {
    pub date: String,
    pub human_hours: f64,
    pub agent_hours: f64,
    pub prompts: usize,
}

#[derive(Debug, Serialize)]
pub struct ProjectRow {
    pub project: String,
    pub human_hours: f64,
}

#[derive(Debug, Serialize)]
pub struct ModelRow {
    pub model: String,
    pub turns: usize,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub totals: Totals,
    pub days: Vec<DayRow>,
    pub projects: Vec<ProjectRow>,
    pub models: Vec<ModelRow>,
}

pub type Interval = (f64, f64);

pub fn human_credits(turns: &[Turn], think_secs: f64) -> Vec<Interval> {
    let mut out = Vec::new();
    for w in turns.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        let gap = b.start - a.end;
        if gap <= 0.0 || b.words < MIN_PROMPT_WORDS {
            continue;
        }
        let typed = if b.paste { b.words.min(PASTE_TYPED_WORDS_CAP) } else { b.words };
        let allowed =
            60.0 * (a.reply_words as f64 / READ_WPM + typed as f64 / TYPE_WPM) + think_secs;
        let credit = gap.min(allowed);
        if credit > 0.0 {
            out.push((a.end, a.end + credit));
        }
    }
    out
}

pub fn union(mut iv: Vec<Interval>) -> Vec<Interval> {
    iv.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<Interval> = Vec::new();
    for (s, e) in iv {
        match out.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

pub fn total_secs(iv: &[Interval]) -> f64 {
    iv.iter().map(|(s, e)| e - s).sum()
}

fn local_date(t: f64) -> NaiveDate {
    Local
        .timestamp_opt(t as i64, 0)
        .earliest()
        .map(|d| d.date_naive())
        .unwrap_or_default()
}

fn add_by_day(map: &mut BTreeMap<NaiveDate, f64>, (start, end): Interval) {
    let mut s = start;
    while s < end {
        let d = local_date(s);
        let next = d
            .succ_opt()
            .and_then(|n| n.and_hms_opt(0, 0, 0))
            .and_then(|n| Local.from_local_datetime(&n).earliest())
            .map(|n| n.timestamp() as f64)
            .filter(|&n| n > s)
            .unwrap_or(s + 3600.0);
        let e = end.min(next);
        *map.entry(d).or_default() += e - s;
        s = e;
    }
}

fn hours(secs: f64) -> f64 {
    (secs / 3600.0 * 100.0).round() / 100.0
}

pub fn analyze(sessions: &[Session], opts: &Options, today: NaiveDate) -> Report {
    let mut human = Vec::new();
    let mut agent = Vec::new();
    let mut by_project: HashMap<&str, Vec<Interval>> = HashMap::new();
    let mut models: HashMap<&str, usize> = HashMap::new();
    let mut prompts_by_day: BTreeMap<NaiveDate, usize> = BTreeMap::new();
    let (mut prompts, mut interrupts, mut session_count) = (0, 0, 0);

    for s in sessions {
        interrupts += s.interrupts;
        if s.turns.is_empty() {
            continue;
        }
        session_count += 1;
        prompts += s.turns.len();
        let credits = human_credits(&s.turns, opts.think_secs);
        by_project.entry(&s.project).or_default().extend(&credits);
        human.extend(credits);
        for t in &s.turns {
            agent.push((t.start, t.end));
            if let Some(m) = &t.model {
                *models.entry(m).or_default() += 1;
            }
            *prompts_by_day.entry(local_date(t.start)).or_default() += 1;
        }
    }

    let human = union(human);
    let agent = union(agent);
    let (human_secs, agent_secs) = (total_secs(&human), total_secs(&agent));

    let mut human_days = BTreeMap::new();
    let mut agent_days = BTreeMap::new();
    human.iter().for_each(|&iv| add_by_day(&mut human_days, iv));
    agent.iter().for_each(|&iv| add_by_day(&mut agent_days, iv));
    let days = (0..opts.days as i64)
        .rev()
        .map(|back| today - Duration::days(back))
        .map(|d| DayRow {
            date: d.to_string(),
            human_hours: hours(human_days.get(&d).copied().unwrap_or(0.0)),
            agent_hours: hours(agent_days.get(&d).copied().unwrap_or(0.0)),
            prompts: prompts_by_day.get(&d).copied().unwrap_or(0),
        })
        .collect();

    let mut projects: Vec<(String, f64)> = by_project
        .into_iter()
        .map(|(p, iv)| (p.to_string(), total_secs(&union(iv))))
        .filter(|(_, s)| *s > 0.0)
        .collect();
    projects.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    projects.truncate(TOP_PROJECTS);

    let mut models: Vec<ModelRow> = models
        .into_iter()
        .map(|(m, turns)| ModelRow { model: m.to_string(), turns })
        .collect();
    models.sort_by(|a, b| b.turns.cmp(&a.turns).then_with(|| a.model.cmp(&b.model)));

    Report {
        totals: Totals {
            human_hours: hours(human_secs),
            agent_hours: hours(agent_secs),
            agent_to_human_ratio: if human_secs > 0.0 {
                (agent_secs / human_secs * 100.0).round() / 100.0
            } else {
                0.0
            },
            prompts,
            sessions: session_count,
            interrupts,
        },
        days,
        projects: projects
            .into_iter()
            .map(|(project, s)| ProjectRow { project, human_hours: hours(s) })
            .collect(),
        models,
    }
}
