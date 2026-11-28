use std::collections::BTreeMap;
use std::fs;

use kairos_core::credit::{agent_runtime, credit_v6_with, credit_v7_with, credit_v8_with, credit_v9_with, credit_v10_with, total_credit_secs, unattributed_ai_secs, CreditClass, Rule, V6Params};
use kairos_core::event::{load_events, log_files, verify_chain, Event, GENESIS};
use kairos_core::{paths, total_secs};
use anyhow::{bail, Context, Result};
use chrono::{Local, NaiveDate};
use serde::Serialize;

struct Opts {
    date: Option<NaiveDate>,
    json: bool,
    spans: bool,
    rule: Rule,
    tune: V6Params,
}

fn tune(params: &mut V6Params, pair: &str) -> Result<()> {
    let (key, value) = pair.split_once('=').context("--tune needs key=value")?;
    let value: f64 = value.parse().ok().filter(|v: &f64| v.is_finite() && *v >= 0.0).context("--tune values must be numbers, 0 or more")?;
    match key {
        "think_secs" => params.think_secs = value,
        "lookback_secs" => params.lookback_secs = value,
        "answer_think_secs" => params.answer_think_secs = value,
        "answer_max_secs" => params.answer_max_secs = value,
        "answer_window_secs" => params.answer_window_secs = value,
        "read_expiry_secs" => params.read_expiry_secs = value,
        "edit_evidence_secs" => params.edit_evidence_secs = value,
        "turn_max_secs" => params.turn_max_secs = value,
        "type_wpm" if value > 0.0 => params.type_wpm = value,
        "type_wpm" => bail!("type_wpm must be more than 0"),
        _ => bail!("unknown --tune key {key:?} (think_secs, lookback_secs, answer_think_secs, answer_max_secs, answer_window_secs, read_expiry_secs, type_wpm, edit_evidence_secs, turn_max_secs)"),
    }
    Ok(())
}

fn parse(args: &[String], allow_json: bool) -> Result<Opts> {
    let mut o = Opts { date: None, json: false, spans: false, rule: Rule::CURRENT, tune: V6Params::default() };
    let mut tuned = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (a.as_str(), None),
        };
        match flag {
            "--json" if allow_json => o.json = true,
            "--spans" if allow_json => o.spans = true,
            "--rule" if allow_json => {
                let v = inline.or_else(|| it.next().cloned()).context("--rule needs a value")?;
                o.rule = Rule::parse(&v).context("--rule must be v1, v3, v4, v5, v6 or v7")?;
            }
            "--tune" if allow_json => {
                let v = inline.or_else(|| it.next().cloned()).context("--tune needs key=value")?;
                tune(&mut o.tune, &v)?;
                tuned = true;
            }
            "--date" => {
                let v = inline.or_else(|| it.next().cloned()).context("--date needs a value")?;
                o.date = Some(NaiveDate::parse_from_str(&v, "%Y-%m-%d").context("--date must be YYYY-MM-DD")?);
            }
            other => bail!("unknown argument {other:?}\nusage: kairos report [--date YYYY-MM-DD] [--rule v1|v3|v4|v5|v6|v7|v8|v9] [--json [--spans [--tune KEY=VALUE]...]] | kairos verify [--date YYYY-MM-DD]"),
        }
    }
    if tuned && !(o.spans && matches!(o.rule, Rule::V6 | Rule::V7 | Rule::V8 | Rule::V9 | Rule::V10)) {
        bail!("--tune needs --spans and the v6, v7, v8 or v9 rule");
    }
    Ok(o)
}

#[derive(Serialize, Default, Clone, Copy)]
struct ClassSecs {
    typing: f64,
    reading_thinking: f64,
    editing: f64,
    terminal: f64,
    testing: f64,
    docs: f64,
    web_agent: f64,
    bridging: f64,
    trips: f64,
}

impl ClassSecs {
    fn add(&mut self, class: CreditClass, secs: f64) {
        match class {
            CreditClass::Typing => self.typing += secs,
            CreditClass::Reading => self.reading_thinking += secs,
            CreditClass::Editing => self.editing += secs,
            CreditClass::Terminal => self.terminal += secs,
            CreditClass::Testing => self.testing += secs,
            CreditClass::WebAgent => self.web_agent += secs,
            CreditClass::Docs => self.docs += secs,
            CreditClass::Bridge => self.bridging += secs,
            CreditClass::Trip => self.trips += secs,
        }
    }
}

#[derive(Serialize)]
struct ProjectSecs {
    project: String,
    active_secs: f64,
    by_class: ClassSecs,
}

#[derive(Serialize)]
struct Summary {
    date: String,
    rule: &'static str,
    active_secs: f64,
    by_class: ClassSecs,
    by_project: Vec<ProjectSecs>,
    unattributed_ai_secs: Option<f64>,
    agent_runtime_secs: f64,
    event_counts: BTreeMap<String, usize>,
}

fn summarize(date: String, events: &[Event], rule: Rule) -> Summary {
    let credits = rule.apply(events);
    let mut by_class = ClassSecs::default();
    let mut projects: BTreeMap<String, ClassSecs> = BTreeMap::new();
    for c in &credits {
        let secs = c.end - c.start;
        by_class.add(c.class, secs);
        projects.entry(c.project.clone().unwrap_or_else(|| "Other".into())).or_default().add(c.class, secs);
    }
    let mut by_project: Vec<ProjectSecs> = projects
        .into_iter()
        .map(|(project, by_class)| ProjectSecs {
            project,
            active_secs: by_class.typing
                + by_class.reading_thinking
                + by_class.editing
                + by_class.terminal
                + by_class.testing
                + by_class.docs
                + by_class.web_agent
                + by_class.bridging
                + by_class.trips,
            by_class,
        })
        .collect();
    by_project.sort_by(|a, b| b.active_secs.total_cmp(&a.active_secs).then_with(|| a.project.cmp(&b.project)));
    let mut event_counts = BTreeMap::new();
    for e in events {
        *event_counts.entry(e.kind.name().to_string()).or_default() += 1;
    }
    Summary {
        date,
        rule: rule.name(),
        active_secs: total_credit_secs(&credits),
        by_class,
        by_project,
        unattributed_ai_secs: (rule == Rule::V3).then(|| unattributed_ai_secs(events)),
        agent_runtime_secs: total_secs(&agent_runtime(events)).abs(),
        event_counts,
    }
}

fn dur(secs: f64) -> String {
    let s = secs.round() as u64;
    format!("{}h {:02}m {:02}s", s / 3600, s % 3600 / 60, s % 60)
}

fn hours(secs: f64) -> String {
    format!("{:.2}", secs / 3600.0)
}

fn print_spans(o: &Opts) -> Result<()> {
    let files = match o.date {
        Some(d) => vec![paths::log_path(&d.to_string())],
        None => log_files(&paths::data_dir()),
    };
    let mut events = Vec::new();
    for f in files.iter().filter(|f| f.exists()) {
        events.extend(load_events(f).with_context(|| format!("reading {}", f.display()))?);
    }
    events.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    let credits = match o.rule {
        Rule::V6 => credit_v6_with(&events, &o.tune),
        Rule::V7 => credit_v7_with(&events, &o.tune),
        Rule::V8 => credit_v8_with(&events, &o.tune),
        Rule::V9 => credit_v9_with(&events, &o.tune),
        Rule::V10 => credit_v10_with(&events, &o.tune),
        rule => rule.apply(&events),
    };
    println!("{}", serde_json::to_string(&credits)?);
    Ok(())
}

pub fn report(args: &[String]) -> Result<()> {
    let o = parse(args, true)?;
    if o.spans {
        return print_spans(&o);
    }
    let date = o.date.unwrap_or_else(|| Local::now().date_naive()).to_string();
    let path = paths::log_path(&date);
    let events = if path.exists() {
        load_events(&path).with_context(|| format!("reading {}", path.display()))?
    } else {
        Vec::new()
    };
    let s = summarize(date, &events, o.rule);
    if o.json {
        println!("{}", serde_json::to_string_pretty(&s)?);
        return Ok(());
    }
    println!("Kairos report for {} (rule {})", s.date, s.rule);
    println!("Human active time  {}", dur(s.active_secs));
    println!("  typing           {}", dur(s.by_class.typing));
    println!("  reading/thinking {}", dur(s.by_class.reading_thinking));
    println!("  editing          {}", dur(s.by_class.editing));
    println!("  terminal         {}", dur(s.by_class.terminal));
    println!("  testing          {}", dur(s.by_class.testing));
    println!("  docs             {}", dur(s.by_class.docs));
    println!("  web agent        {}", dur(s.by_class.web_agent));
    println!("  bridging time    {}", dur(s.by_class.bridging));
    println!("  trips            {}", dur(s.by_class.trips));
    if let Some(u) = s.unattributed_ai_secs {
        println!("Unattributed AI chat {}  (not credited)", dur(u));
    }
    println!("Agent runtime      {}", dur(s.agent_runtime_secs));
    println!("\nBy project (hours)");
    println!(
        "  {:>6} {:>6} {:>7} {:>7} {:>8} {:>7} {:>5} {:>9} {:>8} {:>5}  project",
        "total", "typing", "reading", "editing", "terminal", "testing", "docs", "web_agent", "bridging", "trips"
    );
    for p in &s.by_project {
        let c = &p.by_class;
        println!(
            "  {:>6} {:>6} {:>7} {:>7} {:>8} {:>7} {:>5} {:>9} {:>8} {:>5}  {}",
            hours(p.active_secs),
            hours(c.typing),
            hours(c.reading_thinking),
            hours(c.editing),
            hours(c.terminal),
            hours(c.testing),
            hours(c.docs),
            hours(c.web_agent),
            hours(c.bridging),
            hours(c.trips),
            p.project
        );
    }
    println!("\nEvents");
    for (k, n) in &s.event_counts {
        println!("  {n:>6}  {k}");
    }
    Ok(())
}

pub fn verify(args: &[String]) -> Result<bool> {
    let o = parse(args, false)?;
    let dir = paths::data_dir();
    let files = match o.date {
        Some(d) => vec![paths::log_path(&d.to_string())],
        None => log_files(&dir),
    };
    if files.is_empty() {
        println!("no event logs in {}", dir.display());
        return Ok(true);
    }
    let mut anchor: Option<String> = (o.date.is_none()).then(|| GENESIS.to_string());
    let mut total = 0;
    for f in &files {
        let text = fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?;
        match verify_chain(text.lines(), anchor.as_deref()) {
            Ok(v) => {
                total += v.events;
                anchor = Some(v.last_hash);
            }
            Err(b) => {
                let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                println!("BROKEN at {name} line {}: {}", b.line, b.reason);
                return Ok(false);
            }
        }
    }
    println!("ok: {total} events in {} file(s), chain intact", files.len());
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn spans_flag_parses_where_json_is_allowed() {
        let o = parse(&args(&["--rule", "v4", "--json", "--spans", "--date=2026-11-20"]), true).unwrap();
        assert!(o.spans && o.json && o.rule == Rule::V4);
        assert_eq!(o.date, NaiveDate::from_ymd_opt(2026, 11, 20));
        assert!(!parse(&args(&[]), true).unwrap().spans);
        assert!(parse(&args(&["--spans"]), false).is_err());
    }

    #[test]
    fn tune_sets_v6_numbers_and_needs_spans_and_v6() {
        let o = parse(&args(&["--tune", "think_secs=90", "--json", "--spans", "--tune=type_wpm=50", "--rule", "v6"]), true).unwrap();
        assert_eq!((o.tune.think_secs, o.tune.type_wpm), (90.0, 50.0));
        assert_eq!(o.tune.lookback_secs, V6Params::default().lookback_secs);
        assert!(parse(&args(&["--tune", "nope=1", "--spans", "--rule", "v6"]), true).is_err());
        assert!(parse(&args(&["--tune", "think_secs=x", "--spans", "--rule", "v6"]), true).is_err());
        assert!(parse(&args(&["--tune", "type_wpm=0", "--spans", "--rule", "v6"]), true).is_err());
        assert!(parse(&args(&["--tune", "think_secs=90", "--rule", "v6"]), true).is_err());
        assert_eq!(parse(&args(&["--tune", "think_secs=90", "--spans"]), true).unwrap().tune.think_secs, 90.0, "v7 is the default rule");
        let o = parse(&args(&["--tune", "answer_window_secs=900", "--tune", "read_expiry_secs=3600", "--spans"]), true).unwrap();
        assert_eq!((o.tune.answer_window_secs, o.tune.read_expiry_secs), (900.0, 3600.0));
        assert!(parse(&args(&["--tune", "think_secs=90", "--spans", "--rule", "v5"]), true).is_err());
        assert!(parse(&args(&["--tune", "think_secs=90"]), false).is_err());
    }
}
