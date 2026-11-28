mod propgen;

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;

use kairos_core::credit::{agent_runtime, Credit, CreditClass, Rule, V6Params, HEARTBEAT_LAG_SECS, PAUSE_MAX_SECS, TURN_MAX_SECS};
use kairos_core::event::{Class, Event, Kind};
use propgen::{describe, distinct_ts, measure, meet, minus, norm, run_cases, Iv, Profile, Rng};

const READ_WPM: f64 = 230.0;
const MIN_PROMPT_WORDS: usize = 3;
const PASTE_CAP: usize = 60;
const PRESENT_SECS: f64 = 60.0;
const EDIT_TRACED: [&str; 7] = ["claude-code", "codex", "opencode", "gemini-cli", "qwen-code", "copilot-cli", "antigravity"];
const IN_EDITOR: [&str; 5] = ["cursor", "copilot", "cline", "roo-code", "kilo"];
const TOL: f64 = 1e-3;
const EDITOR_PROJECT_SECS: f64 = 120.0;
const BRIDGE_MAX_SECS: f64 = 180.0;
const TRIP_MAX_SECS: f64 = 30.0;
const BRIDGE_ANCHOR_SECS: f64 = 5.0;
const BRIDGE_WAIT_SECS: f64 = 30.0;

const FULL: Profile = Profile {
    events: 150,
    gaps_of_days: true,
    crashed_turns: true,
    long_turns: true,
    projectless_replies: true,
    queued_heartbeats: true,
    long_pauses: true,
    hostile: false,
    wild: false,
};
const HOSTILE: Profile = Profile { hostile: true, ..FULL };
const WILD: Profile = Profile { hostile: true, wild: true, ..FULL };
const RULES: [Rule; 3] = [Rule::V6, Rule::V7, Rule::V8];

fn run(ev: &[Event], rule: Rule) -> Result<Vec<Credit>, String> {
    let owned = ev.to_vec();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rule.apply(&owned)));
        let _ = tx.send(r.map_err(|p| p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()));
    });
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(c)) => Ok(c),
        Ok(Err(m)) => Err(format!("{} panicked: {m}", rule.name())),
        Err(_) => Err(format!("timeout: {} did not finish in 10 s", rule.name())),
    }
}

// v6 lets a missing project match any; v7 only on the `open` side (a prompt's, answer's or reply's project against the
// window's, or the editor time's project against a turn's).
fn same_project(open: Option<&str>, exact: Option<&str>, v7: bool) -> bool {
    open.is_none() || open == exact || (!v7 && exact.is_none())
}

fn is_agent(c: Class) -> bool {
    matches!(c, Class::AgentTerminal | Class::AgentApp | Class::WebAgent | Class::Editor)
}

fn is_work(c: Class) -> bool {
    is_agent(c) || matches!(c, Class::Terminal | Class::Testing | Class::Docs)
}

struct Turn<'a> {
    from: f64,
    until: f64,
    project: Option<&'a str>,
    harness: Option<&'a str>,
}

struct Item<'a> {
    lo: f64,
    hi: f64,
    budget: f64,
    project: Option<&'a str>,
}

fn close_reply<'a>(out: &mut Vec<Item<'a>>, (from, words, project): (f64, usize, Option<&'a str>), until: f64, params: &V6Params) {
    out.push(Item { lo: from, hi: until.min(from + params.read_expiry_secs), budget: 60.0 * words as f64 / READ_WPM + params.think_secs, project });
}

struct Model<'a> {
    v7: bool,
    editor_saves: bool,
    ev: Vec<&'a Event>,
    presence: Vec<Iv>,
    typing_ok: Vec<Iv>,
    pause: Vec<Iv>,
    spans: Vec<(f64, f64, Class, Option<&'a str>)>,
    turns: Vec<Turn<'a>>,
}

fn model(events: &[Event], rule: Rule) -> Model<'_> {
    let (v7, editor_saves) = (rule != Rule::V6, rule == Rule::V8);
    let mut ev: Vec<&Event> = events.iter().collect();
    ev.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    let idles: Vec<f64> = ev.iter().filter(|e| matches!(e.kind, Kind::Idle {})).map(|e| e.ts).collect();
    let (mut windows, mut input) = (Vec::new(), Vec::new());
    for p in &ev {
        let Kind::Present { input: bits, .. } = &p.kind else { continue };
        let last_idle = idles.iter().copied().filter(|&t| t <= p.ts).fold(f64::MIN, f64::max);
        let lo = (p.ts - PRESENT_SECS).max(last_idle);
        if p.ts <= lo {
            continue;
        }
        windows.push((lo, p.ts));
        match bits.as_deref().and_then(|h| u64::from_str_radix(h, 16).ok()) {
            Some(bits) => input.extend((0..60).filter(|i| bits >> i & 1 == 1).map(|i| (p.ts - PRESENT_SECS + i as f64, p.ts - PRESENT_SECS + i as f64 + 1.0))),
            None => input.push((lo, p.ts)),
        }
    }
    let presence = norm(windows);
    let typing_ok = meet(&presence, &norm(input));

    let mut pause = Vec::new();
    for (i, e) in ev.iter().enumerate() {
        if let Kind::Pause { until } = e.kind {
            let resumed = ev[i + 1..].iter().find(|r| matches!(r.kind, Kind::Resume {})).map_or(f64::INFINITY, |r| r.ts);
            let end = if v7 { until.min(resumed).min(e.ts + PAUSE_MAX_SECS) } else { until.min(resumed) };
            if end > e.ts {
                pause.push((e.ts, end));
            }
        }
    }

    let focus: Vec<(f64, Class, Option<&str>)> = ev
        .iter()
        .filter_map(|e| match &e.kind {
            Kind::Focus { class, .. } => Some((e.ts, *class, e.project.as_deref())),
            _ => None,
        })
        .collect();
    let spans = focus.iter().enumerate().map(|(i, &(s, c, p))| (s, focus.get(i + 1).map_or(f64::INFINITY, |f| f.0), c, p)).collect();
    let spans = if editor_saves { refocused(spans, &ev) } else { spans };

    let turns = if v7 { turns_v7(&ev) } else { turns_v6(&ev) };
    Model { v7, editor_saves, ev, presence, typing_ok, pause: norm(pause), spans, turns }
}

// v8 puts each moment of an editor window in the project of the nearest editor heartbeat within EDITOR_PROJECT_SECS
// (made while that window was in front, sent on time, not AI lines only, with a project), or leaves it. Between cut points the set of marks in reach and the
// nearest of them can't change, so one moment inside each piece decides it. Marks tied at one time give the moments
// before it to the first project and those after it to the last, as the rule's split at the midpoint does.
fn refocused<'a>(spans: Vec<(f64, f64, Class, Option<&'a str>)>, ev: &[&'a Event]) -> Vec<(f64, f64, Class, Option<&'a str>)> {
    let mut marks: Vec<(f64, &str)> = ev
        .iter()
        .filter_map(|e| match &e.kind {
            Kind::Heartbeat { at, ai_lines, human_lines, .. }
                if (e.ts - at).abs() <= HEARTBEAT_LAG_SECS && (human_lines.unwrap_or(0) > 0 || ai_lines.unwrap_or(0) <= 0) =>
            {
                Some((*at, e.project.as_deref()?))
            }
            _ => None,
        })
        .collect();
    marks.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(b.1)));
    let mut out = Vec::new();
    for (s, e, class, own) in spans {
        if class != Class::Editor {
            out.push((s, e, class, own));
            continue;
        }
        let sent: Vec<(f64, &str)> = marks.iter().copied().filter(|m| m.0 >= s && m.0 < e).collect();
        let mut cuts = vec![s, e];
        for (i, m) in sent.iter().enumerate() {
            cuts.extend([m.0 - EDITOR_PROJECT_SECS, m.0 + EDITOR_PROJECT_SECS]);
            cuts.extend(sent[i + 1..].iter().map(|n| (m.0 + n.0) / 2.0));
        }
        cuts.retain(|t| *t >= s && *t <= e);
        cuts.sort_by(f64::total_cmp);
        cuts.dedup();
        for w in cuts.windows(2) {
            let t = if w[1].is_finite() { (w[0] + w[1]) / 2.0 } else { w[0] + 1.0 };
            let best = sent.iter().map(|m| (m.0 - t).abs()).filter(|d| *d <= EDITOR_PROJECT_SECS).fold(f64::INFINITY, f64::min);
            let tied: Vec<&(f64, &str)> = sent.iter().filter(|m| (m.0 - t).abs() == best).collect();
            let pick = match tied.first() {
                None => own,
                Some(m) if m.0 > t => Some(m.1),
                Some(_) => tied.last().map(|m| m.1),
            };
            out.push((w[0], w[1], class, pick));
        }
    }
    out
}

fn turns_v6<'a>(ev: &[&'a Event]) -> Vec<Turn<'a>> {
    let mut open: HashMap<&str, Turn> = HashMap::new();
    let mut turns = Vec::new();
    for e in ev {
        match &e.kind {
            Kind::Prompt { session, .. } | Kind::AgentStart { session } => {
                let t = open.entry(session).or_insert(Turn { from: e.ts, until: f64::INFINITY, project: None, harness: None });
                t.project = t.project.or(e.project.as_deref());
                t.harness = t.harness.or(e.harness.as_deref());
            }
            Kind::ReplyDone { session, .. } => {
                if let Some(t) = open.remove(session.as_str()) {
                    turns.push(Turn { until: e.ts, ..t });
                }
            }
            _ => {}
        }
    }
    turns.extend(open.into_values());
    turns.retain(|t| t.until > t.from);
    turns
}

fn session_of(e: &Event) -> Option<(&str, bool)> {
    match &e.kind {
        Kind::Prompt { session, .. } | Kind::AgentStart { session } => Some((session, true)),
        Kind::ReplyDone { session, .. } => Some((session, false)),
        _ => None,
    }
}

// Each prompt or agent_start runs until its session's next one or reply_done, at most TURN_MAX. Without a project
// (harness) of its own it takes the one the latest earlier start since the session's last reply_done named, if that
// start is less than TURN_MAX before it.
fn turns_v7<'a>(ev: &[&'a Event]) -> Vec<Turn<'a>> {
    let mut turns = Vec::new();
    for (i, e) in ev.iter().enumerate() {
        let Some((session, true)) = session_of(e) else { continue };
        let next = ev[i + 1..].iter().find(|x| session_of(x).is_some_and(|x| x.0 == session)).map_or(f64::INFINITY, |x| x.ts);
        let before: Vec<&Event> = ev[..i].iter().rev().copied().filter(|x| session_of(x).is_some_and(|x| x.0 == session)).take_while(|x| session_of(x).unwrap().1).collect();
        let named = |own: Option<&'a str>, of: fn(&'a Event) -> Option<&'a str>| {
            own.or_else(|| before.iter().find_map(|&x| Some((x.ts, of(x)?))).filter(|(ts, _)| e.ts < ts + TURN_MAX_SECS).map(|(_, p)| p))
        };
        turns.push(Turn {
            from: e.ts,
            until: next.min(e.ts + TURN_MAX_SECS),
            project: named(e.project.as_deref(), |x| x.project.as_deref()),
            harness: named(e.harness.as_deref(), |x| x.harness.as_deref()),
        });
    }
    turns.retain(|t| t.until > t.from);
    turns
}

impl Model<'_> {
    fn spans_of(&self, class: fn(Class) -> bool, project: Option<&str>) -> Vec<Iv> {
        norm(self.spans.iter().filter(|s| class(s.2) && same_project(project, s.3, self.v7)).map(|s| (s.0, s.1)).collect())
    }

    fn exact_spans(&self, class: Class, project: Option<&str>) -> Vec<Iv> {
        norm(self.spans.iter().filter(|s| s.2 == class && s.3 == project).map(|s| (s.0, s.1)).collect())
    }

    fn typing_items(&self, params: &V6Params) -> Vec<Item<'_>> {
        let mut last: HashMap<&str, f64> = HashMap::new();
        let mut out = Vec::new();
        for e in &self.ev {
            let Kind::Prompt { session, words, pasted, .. } = &e.kind else { continue };
            let previous = last.insert(session, e.ts);
            if *words < MIN_PROMPT_WORDS {
                continue;
            }
            let typed = if *pasted { (*words).min(PASTE_CAP) } else { *words };
            out.push(Item {
                lo: (e.ts - params.lookback_secs).max(previous.unwrap_or(f64::MIN)),
                hi: e.ts,
                budget: 60.0 * typed as f64 / params.type_wpm,
                project: e.project.as_deref(),
            });
        }
        out
    }

    fn reply_items(&self, params: &V6Params) -> Vec<Item<'_>> {
        let mut pending: HashMap<&str, (f64, usize, Option<&str>)> = HashMap::new();
        let mut out = Vec::new();
        for e in &self.ev {
            match &e.kind {
                Kind::ReplyDone { session, reply_words, .. } => {
                    if let Some(old) = pending.insert(session, (e.ts, *reply_words, e.project.as_deref())) {
                        close_reply(&mut out, old, e.ts, params);
                    }
                }
                Kind::Prompt { session, .. } => {
                    if let Some((from, words, project)) = pending.remove(session.as_str()) {
                        let project = if self.v7 { project } else { project.or(e.project.as_deref()) };
                        close_reply(&mut out, (from, words, project), e.ts, params);
                    }
                }
                _ => {}
            }
        }
        for (_, reply) in pending {
            close_reply(&mut out, reply, f64::INFINITY, params);
        }
        out
    }

    fn answer_items(&self, params: &V6Params) -> Vec<Item<'_>> {
        self.ev
            .iter()
            .filter_map(|e| match &e.kind {
                Kind::Answered { asked, shown_words, answer_words, .. } if *asked < e.ts => Some(Item {
                    lo: asked.max(e.ts - params.answer_window_secs),
                    hi: e.ts,
                    budget: (60.0 * (*shown_words as f64 / READ_WPM + *answer_words as f64 / params.type_wpm) + params.answer_think_secs).min(params.answer_max_secs),
                    project: e.project.as_deref(),
                }),
                _ => None,
            })
            .collect()
    }

    fn evidence(&self, params: &V6Params) -> Vec<(f64, Option<&str>)> {
        let agent_edits: Vec<(f64, Option<&str>, &str)> = self
            .ev
            .iter()
            .filter_map(|e| match &e.kind {
                Kind::AgentEdit { path, .. } => Some((e.ts, e.project.as_deref(), path.as_str())),
                _ => None,
            })
            .collect();
        if self.editor_saves {
            return self
                .ev
                .iter()
                .filter_map(|e| {
                    let Kind::Heartbeat { at, write: true, path, ai_lines, human_lines, .. } = &e.kind else { return None };
                    let project = e.project.as_deref();
                    let typed = human_lines.is_some_and(|n| n > 0);
                    let by_ai = ai_lines.is_some_and(|n| n > 0) && !typed;
                    let agent = path.as_ref().is_some_and(|p| {
                        agent_edits.iter().any(|a| (a.0 - at).abs() <= params.edit_evidence_secs && a.1 == project && a.2 == p)
                    });
                    let in_editor = self.turns.iter().any(|t| {
                        t.from <= *at && t.until > *at && same_project(project, t.project, true) && t.harness.is_some_and(|h| IN_EDITOR.contains(&h))
                    });
                    let fresh = (e.ts - at).abs() <= HEARTBEAT_LAG_SECS;
                    (fresh && !by_ai && !agent && (typed || !in_editor)).then_some((*at, project))
                })
                .collect();
        }
        let mut out = Vec::new();
        for e in &self.ev {
            match &e.kind {
                Kind::Heartbeat { at, write: true, human_lines: Some(lines), .. } if *lines > 0 && (!self.v7 || (e.ts - at).abs() <= HEARTBEAT_LAG_SECS) => {
                    out.push((*at, e.project.as_deref()))
                }
                Kind::FileSave { paths, .. } => {
                    let traced = self
                        .turns
                        .iter()
                        .filter(|t| t.from <= e.ts && t.until > e.ts && same_project(e.project.as_deref(), t.project, self.v7))
                        .all(|t| t.harness.is_some_and(|h| EDIT_TRACED.contains(&h)));
                    let near = |path: &String| {
                        agent_edits.iter().any(|a| {
                            a.0 >= e.ts - params.edit_evidence_secs && a.0 <= e.ts + params.edit_evidence_secs && a.1 == e.project.as_deref() && a.2 == path
                        })
                    };
                    if traced && paths.iter().any(|p| !near(p)) {
                        out.push((e.ts, e.project.as_deref()));
                    }
                }
                _ => {}
            }
        }
        out
    }
}

fn bridged(c: &Credit) -> bool {
    matches!(c.class, CreditClass::Bridge | CreditClass::Trip)
}

fn by_project(credits: &[Credit], class: CreditClass) -> HashMap<Option<String>, Vec<Iv>> {
    let mut m: HashMap<Option<String>, Vec<Iv>> = HashMap::new();
    for c in credits.iter().filter(|c| c.class == class) {
        m.entry(c.project.clone()).or_default().push((c.start, c.end));
    }
    m.into_iter().map(|(k, v)| (k, norm(v))).collect()
}

fn inside(what: &str, project: &Option<String>, got: &[Iv], allowed: &[Iv]) -> Result<(), String> {
    let extra = minus(got, allowed);
    if measure(&extra) > TOL {
        return Err(format!("{what} credit for project {project:?} falls outside where it may stand: {:?} (total {:.3} s outside)", extra[0], measure(&extra)));
    }
    Ok(())
}

fn clusters(mut items: Vec<&Item>) -> Vec<(Iv, f64)> {
    items.sort_by(|a, b| a.lo.total_cmp(&b.lo));
    let mut out: Vec<(Iv, f64)> = Vec::new();
    for i in items {
        match out.last_mut() {
            Some(l) if i.lo < l.0 .1 => {
                l.0 .1 = l.0 .1.max(i.hi);
                l.1 += i.budget;
            }
            _ => out.push(((i.lo, i.hi), i.budget)),
        }
    }
    out
}

fn budget_ok(what: &str, items: &[Item], got: &HashMap<Option<String>, Vec<Iv>>) -> Result<(), String> {
    for (project, iv) in got {
        let mine: Vec<&Item> = items.iter().filter(|i| i.project == project.as_deref()).collect();
        for ((lo, hi), budget) in clusters(mine) {
            let used = measure(&meet(iv, &[(lo, hi)]));
            if used > budget + TOL {
                return Err(format!("{what} for project {project:?} used {used:.3} s in [{lo}, {hi}] but its items allow {budget:.3} s"));
            }
        }
    }
    Ok(())
}

fn props(name: &str, default_cases: usize, profiles: &[Profile], check: impl Fn(&[Event], &[Credit], Rule) -> Result<(), String>) {
    for rule in RULES {
        for p in profiles {
            let r = run_cases(&format!("{name} ({})", rule.name()), default_cases, 400, *p, |ev, _| {
                let credits = run(ev, rule)?;
                check(ev, &credits, rule)
            });
            if let Err(e) = r {
                panic!("{e}");
            }
        }
    }
}

#[test]
fn the_rule_never_panics_or_hangs() {
    for (rule, p) in RULES.into_iter().flat_map(|r| [FULL, HOSTILE, WILD].map(|p| (r, p))) {
        let r = run_cases(&format!("never_panics ({})", rule.name()), 400, 400, p, |ev, _| {
            for c in run(ev, rule)? {
                if !(c.start.is_finite() && c.end.is_finite() && c.end > c.start) {
                    return Err(format!("bad credit {c:?}"));
                }
            }
            Ok(())
        });
        if let Err(e) = r {
            panic!("{e}");
        }
    }
}

#[test]
fn credit_and_agent_runtime_are_finite_and_forward() {
    props("finite", 400, &[FULL, HOSTILE], |ev, credits, _| {
        for c in credits {
            if !(c.start.is_finite() && c.end.is_finite() && c.end > c.start) {
                return Err(format!("bad credit {c:?}"));
            }
        }
        let runtime = agent_runtime(ev);
        for w in runtime.windows(2) {
            if w[1].0 < w[0].1 {
                return Err(format!("agent_runtime overlaps: {:?}", w));
            }
        }
        Ok(())
    });
}

#[test]
fn nothing_is_credited_outside_presence_or_while_paused() {
    props("presence_and_pause", 600, &[FULL, HOSTILE], |ev, credits, rule| {
        let m = model(ev, rule);
        let all = norm(credits.iter().map(|c| (c.start, c.end)).collect());
        let held = norm(credits.iter().filter(|c| !bridged(c)).map(|c| (c.start, c.end)).collect());
        let outside = minus(&held, &m.presence);
        if measure(&outside) > TOL {
            return Err(format!("credit other than bridging time and trips outside presence: {:?}", outside[0]));
        }
        let paused = meet(&all, &m.pause);
        if measure(&paused) > TOL {
            return Err(format!("credit while paused: {:?} (pause windows {:?})", paused[0], m.pause));
        }
        Ok(())
    });
}

#[test]
fn each_second_is_credited_once() {
    props("no_overlap", 600, &[FULL, HOSTILE], |_, credits, _| {
        let mut sorted: Vec<&Credit> = credits.iter().collect();
        sorted.sort_by(|a, b| a.start.total_cmp(&b.start));
        for w in sorted.windows(2) {
            if w[1].start < w[0].end - 1e-6 {
                return Err(format!("overlap: {:?} and {:?}", w[0], w[1]));
            }
        }
        Ok(())
    });
}

#[test]
fn budgets_hold() {
    let params = V6Params::default();
    props("budgets", 600, &[FULL, HOSTILE], |ev, credits, rule| {
        let m = model(ev, rule);
        budget_ok("typing", &m.typing_items(&params), &by_project(credits, CreditClass::Typing))?;
        let mut reading = m.reply_items(&params);
        reading.extend(m.answer_items(&params));
        budget_ok("reading", &reading, &by_project(credits, CreditClass::Reading))
    });
}

#[test]
fn credit_stands_only_where_a_window_was_in_front() {
    let params = V6Params::default();
    props("windows", 600, &[FULL, HOSTILE], |ev, credits, rule| {
        let m = model(ev, rule);
        for c in credits {
            if matches!(c.class, CreditClass::Docs | CreditClass::WebAgent) {
                return Err(format!("the rule produced {:?}", c.class));
            }
        }

        for (project, got) in by_project(credits, CreditClass::Typing) {
            let p = project.as_deref();
            let prompts = norm(m.typing_items(&params).iter().filter(|i| i.project == p).map(|i| (i.lo, i.hi)).collect());
            let allowed = meet(&meet(&m.spans_of(is_agent, p), &m.typing_ok), &prompts);
            inside("typing", &project, &got, &allowed)?;
        }

        let replies = m.reply_items(&params);
        let answers = m.answer_items(&params);
        for (project, got) in by_project(credits, CreditClass::Reading) {
            let p = project.as_deref();
            let read = norm(replies.iter().filter(|i| i.project == p).map(|i| (i.lo, i.hi)).collect());
            let answered = norm(answers.iter().filter(|i| i.project == p).map(|i| (i.lo, i.hi)).collect());
            let from_replies = meet(&meet(&m.spans_of(is_work, p), &m.presence), &read);
            let from_answers = meet(&meet(&m.spans_of(is_agent, p), &m.presence), &answered);
            let allowed = norm([from_replies, from_answers].concat());
            inside("reading", &project, &got, &allowed)?;
        }

        for (class, focus) in [(CreditClass::Editing, Class::Editor), (CreditClass::Terminal, Class::Terminal), (CreditClass::Testing, Class::Testing)] {
            for (project, got) in by_project(credits, class) {
                let allowed = meet(&m.exact_spans(focus, project.as_deref()), &m.presence);
                inside(class.as_str(), &project, &got, &allowed)?;
            }
        }

        let reach = params.edit_evidence_secs;
        let evidence = m.evidence(&params);
        for (project, got) in by_project(credits, CreditClass::Editing) {
            let p = project.as_deref();
            let running = norm(m.turns.iter().filter(|t| same_project(p, t.project, m.v7)).map(|t| (t.from, t.until)).collect());
            let vouched = norm(evidence.iter().filter(|x| p.is_none() || x.1 == p).map(|x| (x.0 - reach, x.0 + reach)).collect());
            let ungated = minus(&meet(&got, &running), &vouched);
            if measure(&ungated) > TOL {
                return Err(format!("editing credit for {project:?} during an agent turn with no edit evidence near it: {:?}", ungated[0]));
            }
        }
        Ok(())
    });
}

// A gap bridges when the person had BRIDGE_ANCHOR_SECS of input in the project's windows within BRIDGE_MAX_SECS on each
// side. Where an anchor is within TOL of that, rounding decides, so such a gap may bridge but needn't.
#[test]
fn bridging_time_and_trips_fill_short_gaps_in_a_projects_work_and_nothing_else() {
    let params = V6Params::default();
    props("bridges", 600, &[FULL, HOSTILE], |ev, credits, rule| {
        if rule != Rule::V8 {
            return credits.iter().find(|c| bridged(c)).map_or(Ok(()), |c| Err(format!("{} bridged {c:?}", rule.name())));
        }
        let m = model(ev, rule);
        let all = norm(credits.iter().map(|c| (c.start, c.end)).collect());
        let recent = norm(m.typing_ok.iter().map(|&(s, e)| (s, e + BRIDGE_WAIT_SECS)).collect());
        let evidence = m.evidence(&params);
        let reach = params.edit_evidence_secs;
        let mut projects: Vec<&str> = credits.iter().filter_map(|c| c.project.as_deref()).collect();
        projects.sort();
        projects.dedup();
        let of = |class: CreditClass, p: &str| norm(credits.iter().filter(|c| c.class == class && c.project.as_deref() == Some(p)).map(|c| (c.start, c.end)).collect());
        for p in projects {
            let work = norm(credits.iter().filter(|c| !bridged(c) && c.project.as_deref() == Some(p)).map(|c| (c.start, c.end)).collect());
            let home = norm(m.spans.iter().filter(|s| is_work(s.2) && s.3 == Some(p)).map(|s| (s.0, s.1)).collect());
            let active = meet(&home, &m.typing_ok);
            let anchor = |lo: f64, hi: f64| measure(&meet(&active, &[(lo, hi)]));
            let running = norm(m.turns.iter().filter(|t| t.project == Some(p)).map(|t| (t.from, t.until)).collect());
            let vouched = norm(evidence.iter().filter(|x| x.1 == Some(p)).map(|x| (x.0 - reach, x.0 + reach)).collect());
            let gated = minus(&meet(&m.exact_spans(Class::Editor, Some(p)), &running), &vouched);
            let blocked = norm([minus(&running, &recent), gated].concat());
            let (mut sure, mut may) = (Vec::new(), Vec::new());
            for g in work.windows(2).map(|w| (w[0].1, w[1].0)) {
                if g.1 - g.0 > BRIDGE_MAX_SECS || measure(&meet(&[g], &m.pause)) != 0.0 {
                    continue;
                }
                let least = anchor(g.0 - BRIDGE_MAX_SECS, g.0).min(anchor(g.1, g.1 + BRIDGE_MAX_SECS));
                if least >= BRIDGE_ANCHOR_SECS + TOL {
                    sure.push(g);
                }
                if least >= BRIDGE_ANCHOR_SECS - TOL {
                    may.push(g);
                }
            }
            let fill = |gaps: &[Iv]| {
                let stays = minus(&meet(&norm(gaps.to_vec()), &home), &blocked);
                let away = gaps.iter().flat_map(|g| minus(&[*g], &home).into_iter().filter(|a| a.1 - a.0 <= TRIP_MAX_SECS)).collect();
                (stays, minus(&norm(away), &blocked))
            };
            let name = Some(p.to_string());
            let (stays, trips) = fill(&may);
            inside("bridging", &name, &of(CreditClass::Bridge, p), &stays)?;
            inside("trip", &name, &of(CreditClass::Trip, p), &trips)?;
            let (stays, trips) = fill(&sure);
            for (what, owed) in [("bridging time", minus(&stays, &all)), ("a trip", minus(&trips, &all))] {
                if measure(&owed) > TOL {
                    return Err(format!("a second that is {what} for {p} earned nothing: {:?}", owed[0]));
                }
            }
        }
        Ok(())
    });
}

#[test]
fn terminal_testing_and_idle_free_editor_time_is_never_dropped() {
    props("completeness", 600, &[FULL, HOSTILE], |ev, credits, rule| {
        let m = model(ev, rule);
        let all = norm(credits.iter().map(|c| (c.start, c.end)).collect());
        let busy = norm(m.turns.iter().map(|t| (t.from, t.until)).collect());
        let mut owed = Vec::new();
        for s in &m.spans {
            let own = match s.2 {
                Class::Terminal | Class::Testing => meet(&[(s.0, s.1)], &m.presence),
                Class::Editor => minus(&meet(&[(s.0, s.1)], &m.presence), &busy),
                _ => continue,
            };
            owed.extend(minus(&own, &m.pause));
        }
        let missing = minus(&norm(owed), &all);
        if measure(&missing) > TOL {
            return Err(format!("presence in a terminal, testing or turn-free editor window earned nothing: {:?}", missing[0]));
        }
        Ok(())
    });
}

fn shuffled_ties(rng: &mut Rng, ev: &[Event]) -> Vec<Event> {
    let mut groups: Vec<Vec<Event>> = Vec::new();
    for e in ev {
        match groups.last_mut() {
            Some(g) if g[0].ts == e.ts => g.push(e.clone()),
            _ => groups.push(vec![e.clone()]),
        }
    }
    let session = |e: &Event| -> Option<String> {
        match &e.kind {
            Kind::Prompt { session, .. } | Kind::AgentStart { session } | Kind::ReplyDone { session, .. } | Kind::Answered { session, .. } => Some(session.clone()),
            _ => None,
        }
    };
    let mut out = Vec::new();
    for g in groups {
        let mut queues: Vec<(Option<String>, Vec<Event>)> = Vec::new();
        for e in g {
            let key = session(&e);
            match queues.iter_mut().find(|q| q.0 == key && key.is_some()) {
                Some(q) => q.1.push(e),
                None => queues.push((key, vec![e])),
            }
        }
        // the shared, non-session events keep their relative order, so they sit in one queue
        let shared: Vec<Event> = queues.iter().filter(|q| q.0.is_none()).flat_map(|q| q.1.clone()).collect();
        queues.retain(|q| q.0.is_some());
        queues.push((None, shared));
        for q in &mut queues {
            q.1.reverse();
        }
        while queues.iter().any(|q| !q.1.is_empty()) {
            let live: Vec<usize> = (0..queues.len()).filter(|&i| !queues[i].1.is_empty()).collect();
            let at = live[rng.below(live.len())];
            out.push(queues[at].1.pop().unwrap());
        }
    }
    out
}

#[test]
fn the_same_events_give_the_same_credit() {
    for (rule, p) in RULES.into_iter().flat_map(|r| [FULL, HOSTILE].map(|p| (r, p))) {
        let run = |ev: &[Event]| run(ev, rule);
        let r = run_cases(&format!("determinism ({})", rule.name()), 400, 400, p, |ev, seed| {
            let a = run(ev)?;
            if a != run(ev)? {
                return Err("two runs over the same events differ".into());
            }
            let mut rng = Rng::new(seed);

            let mut unique = ev.to_vec();
            distinct_ts(&mut unique);
            let base = run(&unique)?;
            let mut shuffled = unique.clone();
            rng.shuffle(&mut shuffled);
            if base != run(&shuffled)? {
                return Err("with distinct timestamps, the order the events are given in changes the credit".into());
            }

            let mut sorted = ev.to_vec();
            sorted.sort_by(|a, b| a.ts.total_cmp(&b.ts));
            let tied = shuffled_ties(&mut rng, &sorted);
            let (x, y) = (run(&sorted)?, run(&tied)?);
            if x != y {
                return Err(format!("reordering events that share a timestamp (different sessions, same session order kept) changes the credit:\n{}", describe(&tied)));
            }
            Ok(())
        });
        if let Err(e) = r {
            panic!("{e}");
        }
    }
}

#[test]
fn facts_about_projects_and_commits_do_not_change_credit() {
    use kairos_core::event::{Kind as K, Via};
    for (rule, p) in RULES.into_iter().flat_map(|r| [FULL, HOSTILE].map(|p| (r, p))) {
        let run = |ev: &[Event]| run(ev, rule);
        let r = run_cases(&format!("inert_kinds ({})", rule.name()), 400, 400, p, |ev, seed| {
            let mut rng = Rng::new(seed);
            let base = run(ev)?;
            let mut more = ev.to_vec();
            let (lo, hi) = (ev.iter().map(|e| e.ts).fold(f64::INFINITY, f64::min), ev.iter().map(|e| e.ts).fold(f64::NEG_INFINITY, f64::max));
            for _ in 0..rng.below(20) + 1 {
                let ts = rng.range(lo, hi);
                let kind = match rng.below(8) {
                    0 => K::Start { version: "1".into(), os: "linux".into() },
                    1 => K::Backends { focus: "a".into(), idle: "b".into() },
                    2 => K::Commit { sha: "abcdef1".into(), files: 1, added: 1, removed: 1, authored: None, committed: None, via: Some(Via::Commit), changes: vec![], replaces: None, not_before: None },
                    3 => K::Diff { files: 1, added: 1, removed: 1, changes: vec![] },
                    4 => K::AgentCommit { session: "s1".into(), sha: None },
                    5 => K::ProjectAlias { from: "x/y".into() },
                    6 => K::Unknown,
                    _ => K::Heartbeat { at: ts, editor: None, language: None, category: None, path: None, write: rng.chance(0.5), ai_lines: Some(3), human_lines: None },
                };
                let at = rng.below(more.len() + 1);
                more.insert(at, Event::new(ts, Some("ada/site".into()), kind));
            }
            if base != run(&more)? {
                return Err("adding commit, diff, rename, start, backends, unknown or non-human heartbeat events changed the credit".into());
            }
            Ok(())
        });
        if let Err(e) = r {
            panic!("{e}");
        }
    }
}

#[test]
fn the_generator_reaches_every_credit_path() {
    for rule in RULES {
        coverage(rule);
    }
}

fn coverage(rule: Rule) {
    let mut secs: HashMap<&str, f64> = HashMap::new();
    let (mut gated, mut answered, mut paused_cases, mut pauses_cut) = (0, 0, 0, 0);
    let r = run_cases(&format!("coverage ({})", rule.name()), 300, 0, FULL, |ev, _| {
        let credits = run(ev, rule)?;
        for c in &credits {
            *secs.entry(c.class.as_str()).or_default() += c.end - c.start;
        }
        let m = model(ev, rule);
        let turns = norm(m.turns.iter().map(|t| (t.from, t.until)).collect());
        let editing = norm(credits.iter().filter(|c| c.class == CreditClass::Editing).map(|c| (c.start, c.end)).collect());
        let editor: Vec<Iv> = m.spans.iter().filter(|s| s.2 == Class::Editor).map(|s| (s.0, s.1)).collect();
        if measure(&minus(&meet(&norm(editor), &meet(&turns, &m.presence)), &editing)) > 1.0 {
            gated += 1;
        }
        answered += ev.iter().filter(|e| matches!(e.kind, Kind::Answered { .. })).count();
        if !m.pause.is_empty() {
            paused_cases += 1;
            let all = norm(credits.iter().map(|c| (c.start, c.end)).collect());
            if measure(&meet(&m.pause, &m.presence)) > 0.0 && measure(&minus(&meet(&m.pause, &m.presence), &all)) > 0.0 {
                pauses_cut += 1;
            }
        }
        Ok(())
    });
    if let Err(e) = r {
        panic!("{e}");
    }
    eprintln!("coverage of {}: {secs:?} gated_cases={gated} answered={answered} paused_cases={paused_cases} pauses_cut={pauses_cut}", rule.name());
    for class in ["typing", "reading", "editing", "terminal", "testing"] {
        assert!(secs.get(class).copied().unwrap_or(0.0) > 0.0, "no {class} credit in any case");
    }
    if rule == Rule::V8 {
        for class in ["bridge", "trip"] {
            assert!(secs.get(class).copied().unwrap_or(0.0) > 0.0, "no {class} credit in any case");
        }
    }
    assert!(gated > 0, "no case had editor time gated by an agent turn");
}
