use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use kairos_core::credit::{Credit, Rule, BRIDGE_WAIT_SECS, TRIP_MAX_SECS};
use kairos_core::event::{Event, Kind};

use crate::person::Person;

type Span = (f64, f64);

fn union(mut list: Vec<Span>) -> Vec<Span> {
    list.retain(|s| s.1 > s.0);
    list.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<Span> = Vec::new();
    for s in list {
        match out.last_mut() {
            Some(last) if s.0 <= last.1 => last.1 = last.1.max(s.1),
            _ => out.push(s),
        }
    }
    out
}

fn meet(a: &[Span], b: &[Span]) -> Vec<Span> {
    union(a.iter().flat_map(|x| b.iter().map(move |y| (x.0.max(y.0), x.1.min(y.1)))).collect())
}

fn secs(list: &[Span]) -> f64 {
    list.iter().map(|s| s.1 - s.0).sum::<f64>() + 0.0
}

/// What the server's credit rule makes of the run, against what the person did: every stretch they spent reading and
/// typing gets credit, in the project, and nothing comes long after they last touched anything while the agent worked.
/// Only time kairosd could see counts as owed: it writes what it saw once a minute, so the minute before it was
/// stopped is lost.
fn read(kd: &Path, problems: &mut Vec<String>) -> Result<Vec<Event>> {
    let mut files: Vec<PathBuf> = fs::read_dir(kd)?.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    files.retain(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("events-") && n.ends_with(".jsonl")));
    files.sort();
    let mut events = Vec::new();
    for f in files {
        for line in fs::read_to_string(f)?.lines() {
            match serde_json::from_str::<Event>(line) {
                Ok(e) => events.push(e),
                Err(e) => problems.push(format!("the credit rule can't read an event ({e}): {}", line.chars().take(120).collect::<String>())),
            }
        }
    }
    events.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    Ok(events)
}

/// Every piece of credit the rule gives a kairosd folder's events, for looking into a run by hand.
pub fn show(kd: &Path) -> Result<()> {
    let mut problems = Vec::new();
    let events = read(kd, &mut problems)?;
    let first = events.first().map_or(0.0, |e| e.ts);
    for c in Rule::CURRENT.apply(&events) {
        println!("+{:7.1} s  {:5.1} s  {:8}  {:?}", c.start - first, c.end - c.start, c.class.as_str(), c.project);
    }
    for p in problems {
        eprintln!("{p}");
    }
    Ok(())
}

/// Credit long after the person last touched anything.
fn hands_off(credited: &[Span], person: &Person, problems: &mut Vec<String>) {
    let allowed = union(person.inputs().iter().map(|&t| (t - 1.0, t + BRIDGE_WAIT_SECS + 2.0)).collect());
    let over = secs(credited) - secs(&meet(credited, &allowed));
    if over > 1.0 {
        problems.push(format!("{over:.0} s of credit came while nobody had touched anything for over {BRIDGE_WAIT_SECS:.0} s"));
    }
}

/// The time kairosd saw: each minute it wrote down, since it started or last found nobody there.
fn seen(events: &[Event]) -> Vec<Span> {
    let mut seen = Vec::new();
    let mut since = f64::MIN;
    for e in events {
        match e.kind {
            Kind::Start { .. } | Kind::Idle {} => since = e.ts,
            Kind::Present { .. } => seen.push(((e.ts - 60.0).max(since), e.ts)),
            _ => {}
        }
    }
    union(seen)
}

/// kairosd looks at the window in front every few seconds, so it sees a switch this late at most.
pub const LOOKS: f64 = 5.0;

/// A stretch of the run with one window in front: from, to, and the project of the agent in it (none for a window
/// that is no project's).
pub type Shown = (f64, f64, Option<&'static str>);

/// The credit rule over a run with several windows: credit for a project only while its window was in front, or on a
/// quick look elsewhere between two stretches of it (a trip), and what the person read and typed in each window
/// credited to its project.
pub fn check_windows(kd: &Path, shown: &[Shown], person: &Person) -> Result<(Vec<String>, String)> {
    let mut problems = Vec::new();
    let events = read(kd, &mut problems)?;
    let credits = Rule::CURRENT.apply(&events);
    let first = shown.first().map_or(0.0, |s| s.0);
    let mut may: BTreeMap<&str, Vec<Span>> = BTreeMap::new();
    let mut owed: Vec<(&str, Span, String)> = Vec::new();
    for &(from, to, project) in shown {
        if let Some(p) = project {
            may.entry(p).or_default().push((from, to + LOOKS));
            owed.push((p, (from + LOOKS, to), format!("{p}'s window in front at +{:.0} s", from - first)));
        }
    }
    // A trip: the windows between two stretches of a project's, for at most TRIP_MAX in all. Kairos sees each switch
    // up to LOOKS late, so a trip near the limit may go either way.
    let projects: Vec<&str> = may.keys().copied().collect();
    for p in projects {
        let n = shown.len();
        let mut i = 0;
        loop {
            let Some(start) = (i..n).find(|&k| shown[k].2 == Some(p)) else { break };
            let Some(j) = (start..n).find(|&k| shown[k].2 != Some(p)) else { break };
            let Some(end) = (j..n).find(|&k| shown[k].2 == Some(p)) else { break };
            let (from, to) = (shown[j].0, shown[end].0);
            if to - from <= TRIP_MAX_SECS + LOOKS {
                may.entry(p).or_default().push((from, to + LOOKS));
            }
            if to - from <= TRIP_MAX_SECS - LOOKS {
                for &(a, b, _) in shown[j..end].iter().filter(|s| s.2.is_none()) {
                    owed.push((p, (a, b), format!("a {:.0} s look away from {p} at +{:.0} s", to - from, a - first)));
                }
            }
            i = end;
        }
    }
    let mut wrong: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    for c in &credits {
        let p = c.project.as_deref().unwrap_or("none");
        let allowed = union(may.get(p).cloned().unwrap_or_default());
        let span = [(c.start, c.end)];
        let out = secs(&span) - secs(&meet(&span, &allowed));
        if out > 0.5 {
            let w = wrong.entry(format!("{} in project {p}", c.class.as_str())).or_insert((0.0, c.start - first));
            w.0 += out;
        }
    }
    for (what, (s, at)) in wrong {
        if s > 1.0 {
            problems.push(format!("{s:.0} s of credit was {what} while another window was in front (from +{at:.0} s)"));
        }
    }
    hands_off(&union(credits.iter().map(|c| (c.start, c.end)).collect()), person, &mut problems);
    let busy = meet(&union(person.busy_times()), &seen(&events));
    for (p, span, what) in owed {
        let want = secs(&meet(&busy, &[span]));
        let got = secs(&meet(&union(credits.iter().filter(|c| c.project.as_deref() == Some(p)).map(|c| (c.start, c.end)).collect()), &[span]));
        if want > 0.0 && got < want - (0.1 * want).max(3.0) {
            problems.push(format!("{what}: {got:.0} s of credit for the {want:.0} s the person spent reading and typing"));
        }
    }
    Ok((problems, summary(&credits, person, &busy)))
}

fn summary(credits: &[Credit], person: &Person, owed: &[Span]) -> String {
    let mut by: BTreeMap<String, f64> = BTreeMap::new();
    for c in credits {
        *by.entry(format!("{} {}", c.project.as_deref().unwrap_or("none"), c.class.as_str())).or_default() += c.end - c.start;
    }
    let by: Vec<String> = by.iter().map(|(c, s)| format!("{c} {s:.0} s")).collect();
    format!(
        "credit {} ({:.0} s in all); the person read and typed {:.0} s, {:.0} s of it while kairosd could see",
        if by.is_empty() { "none".to_string() } else { by.join(", ") },
        secs(&union(credits.iter().map(|c| (c.start, c.end)).collect())),
        secs(&union(person.busy_times())),
        secs(owed),
    )
}

/// `closed`: while the harness was closed, when the window showed the lab itself, in the lab's own folder.
pub fn check(kd: &Path, harness: &str, project: &str, person: &Person, closed: &[Span]) -> Result<(Vec<String>, String)> {
    let mut problems = Vec::new();
    let events = read(kd, &mut problems)?;
    let credits = Rule::CURRENT.apply(&events);
    let credited = union(credits.iter().map(|c| (c.start, c.end)).collect());

    let mut classes: BTreeMap<&str, f64> = BTreeMap::new();
    let mut elsewhere: BTreeMap<String, f64> = BTreeMap::new();
    for c in &credits {
        *classes.entry(c.class.as_str()).or_default() += c.end - c.start;
        if c.project.as_deref() != Some(project) {
            let away: Vec<Span> = closed.iter().map(|&(a, b)| (a, b + LOOKS)).collect();
            let s = c.end - c.start - secs(&meet(&[(c.start, c.end)], &union(away)));
            if s > 0.0 {
                *elsewhere.entry(format!("{} in project {:?}", c.class.as_str(), c.project)).or_default() += s;
            }
        }
    }
    for (what, s) in elsewhere {
        problems.push(format!("{s:.0} s of credit was {what}, the person worked in {project}"));
    }

    hands_off(&credited, person, &mut problems);
    let owed = meet(&union(person.busy_times()), &seen(&events));
    let mine = |name: &str| -> Vec<&Event> { events.iter().filter(|e| e.harness.as_deref() == Some(harness) && e.kind.name() == name).collect() };
    let (prompts, replies) = (mine("prompt"), mine("reply_done"));
    // Each stretch is reading the last reply, then typing the next prompt.
    let mut stretches: Vec<(String, Span, Vec<&Event>)> = Vec::new();
    for (i, p) in prompts.iter().enumerate() {
        let reply = replies.iter().filter(|r| r.ts < p.ts).last();
        stretches.push((format!("turn {}", i + 1), (reply.map_or(f64::MIN, |r| r.ts), p.ts), reply.copied().into_iter().chain([*p]).collect()));
    }
    if let Some(last) = replies.last() {
        stretches.push(("after the last reply".to_string(), (last.ts, f64::MAX), vec![*last]));
    }
    for (what, s, by) in stretches {
        let want = secs(&meet(&owed, &[s]));
        let got = secs(&meet(&credited, &[s]));
        if want > 0.0 && got < want - (0.1 * want).max(3.0) {
            let mut why: Vec<String> = by
                .iter()
                .filter(|e| e.project.as_deref() != Some(project))
                .map(|e| format!("the {} is in project {:?}", e.kind.name(), e.project.as_deref().unwrap_or("none")))
                .collect();
            why.dedup();
            let why = if why.is_empty() { String::new() } else { format!(" ({})", why.join(", ")) };
            problems.push(format!("{what}: {got:.0} s of credit for the {want:.0} s the person spent reading and typing{why}"));
        }
    }

    let by_class: Vec<String> = classes.iter().map(|(c, s)| format!("{c} {s:.0} s")).collect();
    let summary = format!(
        "credit {} ({:.0} s in all); the person read and typed {:.0} s, {:.0} s of it while kairosd could see",
        if by_class.is_empty() { "none".to_string() } else { by_class.join(", ") },
        secs(&credited),
        secs(&union(person.busy_times())),
        secs(&owed),
    );
    Ok((problems, summary))
}
