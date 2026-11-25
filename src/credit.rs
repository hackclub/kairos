use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use kairos_core::credit::{Rule, BRIDGE_WAIT_SECS};
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
pub fn check(kd: &Path, harness: &str, project: &str, person: &Person) -> Result<(Vec<String>, String)> {
    let mut problems = Vec::new();
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
    let credits = Rule::CURRENT.apply(&events);
    let credited = union(credits.iter().map(|c| (c.start, c.end)).collect());

    let mut classes: BTreeMap<&str, f64> = BTreeMap::new();
    let mut elsewhere: BTreeMap<String, f64> = BTreeMap::new();
    for c in &credits {
        *classes.entry(c.class.as_str()).or_default() += c.end - c.start;
        if c.project.as_deref() != Some(project) {
            *elsewhere.entry(format!("{} in project {:?}", c.class.as_str(), c.project)).or_default() += c.end - c.start;
        }
    }
    for (what, s) in elsewhere {
        problems.push(format!("{s:.0} s of credit was {what}, the person worked in {project}"));
    }

    let allowed = union(person.inputs().iter().map(|&t| (t - 1.0, t + BRIDGE_WAIT_SECS + 2.0)).collect());
    let over = secs(&credited) - secs(&meet(&credited, &allowed));
    if over > 1.0 {
        problems.push(format!("{over:.0} s of credit came while nobody had touched anything for over {BRIDGE_WAIT_SECS:.0} s"));
    }

    let mut seen = Vec::new();
    let mut since = f64::MIN;
    for e in &events {
        match e.kind {
            Kind::Start { .. } | Kind::Idle {} => since = e.ts,
            Kind::Present { .. } => seen.push(((e.ts - 60.0).max(since), e.ts)),
            _ => {}
        }
    }
    let owed = meet(&union(person.busy_times()), &union(seen));
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
