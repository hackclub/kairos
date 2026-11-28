use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

use crate::event::{App, Class, Event, Kind};
use crate::{total_secs, union, Interval, MIN_PROMPT_WORDS, PASTE_TYPED_WORDS_CAP, READ_WPM, TYPE_WPM};

const THINK_SECS: f64 = 120.0;
const LOOKBACK_SECS: f64 = 600.0;
const ANSWER_THINK_SECS: f64 = 30.0;
const ANSWER_MAX_SECS: f64 = 600.0;
const ANSWER_WINDOW_SECS: f64 = 1800.0;
const READ_EXPIRY_SECS: f64 = 7200.0;
const EDIT_EVIDENCE_SECS: f64 = 120.0;
// Harnesses whose logs make an agent_edit for every file the agent changes with its edit tools (what its shell commands
// write goes unlisted).
pub const EDIT_TRACED: [&str; 7] = ["claude-code", "codex", "opencode", "gemini-cli", "qwen-code", "copilot-cli", "antigravity"];
// Agents that live in the editor and save their changes through it, like the person does. kilo also names the Kilo
// CLI, whose turns this only makes stricter.
const IN_EDITOR: [&str; 5] = ["cursor", "copilot", "cline", "roo-code", "kilo"];
const PRESENT_SECS: f64 = 60.0;
// Docs and web agent tabs carry no project; they borrow one from project activity this recent.
const INFER_SECS: f64 = 15.0 * 60.0;
const EPS: f64 = 1e-6;
/// credit_v7 and agent_runtime end a turn whose reply_done never came this long after its last prompt or agent_start.
pub const TURN_MAX_SECS: f64 = 4.0 * 3600.0;
/// credit_v7 ignores an editor heartbeat kairosd got more than this long after (or before) the time it is for: wakatime-cli
/// sends within a couple of minutes, so a bigger gap is a queue replayed after kairosd was down.
pub const HEARTBEAT_LAG_SECS: f64 = 600.0;
/// credit_v7 ends a pause at most this long after it began (kairosd pauses until tomorrow at the latest).
pub const PAUSE_MAX_SECS: f64 = 25.0 * 3600.0;
/// credit_v8 puts an editor window in front in the project of the nearest editor heartbeat at most this far from it, of
/// those sent while that window was in front.
pub const EDITOR_PROJECT_SECS: f64 = 120.0;
/// credit_v8 bridges a gap of at most this long between two moments credited to the same project.
pub const BRIDGE_MAX_SECS: f64 = 180.0;
/// A bridged gap longer than this is a long one, which inspect marks for reviewers.
pub const BRIDGE_LONG_SECS: f64 = 120.0;
/// In a bridged gap, time away from the project's windows counts as a trip when the person came back within this long.
pub const TRIP_MAX_SECS: f64 = 30.0;
/// credit_v8 bridges a gap only when the person was active in that project's windows for at least this long in the
/// BRIDGE_MAX_SECS before it, and again in the BRIDGE_MAX_SECS after it.
pub const BRIDGE_ANCHOR_SECS: f64 = 5.0;
/// While an agent turn runs in the project, a bridged gap counts only up to this long after the person's last input.
pub const BRIDGE_WAIT_SECS: f64 = 30.0;
/// credit_v9 takes a prompt as typed in the agent app its log names when that app's window was in front with input at
/// most this long before it.
pub const TYPED_HERE_SECS: f64 = 5.0;
/// credit_v10 reads one reply for at most this long: a 4,100-word reply at READ_WPM plus THINK_SECS.
pub const READ_MAX_SECS: f64 = 1200.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CreditClass {
    Typing,
    Reading,
    Editing,
    Terminal,
    Testing,
    WebAgent,
    Docs,
    Bridge,
    Trip,
}

impl CreditClass {
    pub fn as_str(self) -> &'static str {
        match self {
            CreditClass::Typing => "typing",
            CreditClass::Reading => "reading",
            CreditClass::Editing => "editing",
            CreditClass::Terminal => "terminal",
            CreditClass::Testing => "testing",
            CreditClass::WebAgent => "web_agent",
            CreditClass::Docs => "docs",
            CreditClass::Bridge => "bridge",
            CreditClass::Trip => "trip",
        }
    }
}

/// Bump it whenever the live rule (`Rule::CURRENT`), `agent_turns` or the server's span and day building would store something different (a test
/// pins the output): a server rebuilds every user stored under an older revision when it starts.
pub const REVISION: i32 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    V1,
    V3,
    V4,
    V5,
    V6,
    V7,
    V8,
    V9,
    V10,
}

impl Rule {
    pub const CURRENT: Rule = Rule::V10;

    pub fn name(self) -> &'static str {
        match self {
            Rule::V1 => "credit_v1",
            Rule::V3 => "credit_v3",
            Rule::V4 => "credit_v4",
            Rule::V5 => "credit_v5",
            Rule::V6 => "credit_v6",
            Rule::V7 => "credit_v7",
            Rule::V8 => "credit_v8",
            Rule::V9 => "credit_v9",
            Rule::V10 => "credit_v10",
        }
    }

    pub fn parse(s: &str) -> Option<Rule> {
        match s.strip_prefix("credit_").unwrap_or(s) {
            "v1" => Some(Rule::V1),
            "v3" => Some(Rule::V3),
            "v4" => Some(Rule::V4),
            "v5" => Some(Rule::V5),
            "v6" => Some(Rule::V6),
            "v7" => Some(Rule::V7),
            "v8" => Some(Rule::V8),
            "v9" => Some(Rule::V9),
            "v10" => Some(Rule::V10),
            _ => None,
        }
    }

    pub fn apply(self, events: &[Event]) -> Vec<Credit> {
        match self {
            Rule::V1 => credit_v1(events),
            Rule::V3 => credit_v3(events),
            Rule::V4 => credit_v4(events),
            Rule::V5 => credit_v5(events),
            Rule::V6 => credit_v6(events),
            Rule::V7 => credit_v7(events),
            Rule::V8 => credit_v8(events),
            Rule::V9 => credit_v9(events),
            Rule::V10 => credit_v10(events),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Credit {
    pub start: f64,
    pub end: f64,
    pub project: Option<String>,
    pub class: CreditClass,
}

pub fn credit_v1(events: &[Event]) -> Vec<Credit> {
    let mut ev: Vec<&Event> = events.iter().collect();
    ev.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    let windows = present_windows(&ev);
    resolve(vec![presence_credits(&focuses(&ev), &windows, class_v1), reading_credits(&ev)])
}

pub fn credit_v3(events: &[Event]) -> Vec<Credit> {
    let mut ev: Vec<&Event> = events.iter().collect();
    ev.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    let windows = present_windows(&ev);
    let presence = presence_credits(&focuses(&ev), &windows, class_v3);
    let of = |class: CreditClass| -> Vec<Credit> { presence.iter().filter(|c| c.class == class).cloned().collect() };
    let reading = reading_credits_v3(&ev, &windows).into_iter().filter(|c| c.project.is_some()).collect();
    resolve(vec![
        typing_credits(&ev, true),
        of(CreditClass::Editing),
        of(CreditClass::Terminal),
        of(CreditClass::Testing),
        reading,
        of(CreditClass::WebAgent),
    ])
}

/// credit_v3 plus docs time, a project borrowed by docs and web agent tabs from recent project
/// activity, credit with no project kept (shown as Other), and nothing credited while paused.
pub fn credit_v4(events: &[Event]) -> Vec<Credit> {
    let mut ev: Vec<&Event> = events.iter().collect();
    ev.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    let windows = present_windows(&ev);
    let presence = presence_credits(&focuses(&ev), &windows, class_v4);
    let anchors: Vec<(f64, &str)> =
        ev.iter().filter(|e| e.kind.is_activity()).filter_map(|e| Some((e.ts, e.project.as_deref()?))).collect();
    let of = |class: CreditClass| -> Vec<Credit> { presence.iter().filter(|c| c.class == class).cloned().collect() };
    let resolved = resolve(vec![
        typing_credits(&ev, false),
        of(CreditClass::Editing),
        of(CreditClass::Terminal),
        of(CreditClass::Testing),
        borrow_projects(of(CreditClass::Docs), &anchors),
        reading_credits_v3(&ev, &windows),
        borrow_projects(of(CreditClass::WebAgent), &anchors),
    ]);
    without(resolved, &pause_windows(&ev, false))
}

/// Only what can be shown: typing and reading from tool logs, editor and terminal presence, and
/// testing that ties to a project (a local port or an app started from its folder). No docs, no
/// web agent tabs, no project borrowed from earlier activity. Credit with no project is kept
/// (shown as Other), and nothing is credited while paused.
pub fn credit_v5(events: &[Event]) -> Vec<Credit> {
    let mut ev: Vec<&Event> = events.iter().collect();
    ev.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    let windows = present_windows(&ev);
    let presence = presence_credits(&focuses(&ev), &windows, class_v5);
    let of = |class: CreditClass| -> Vec<Credit> { presence.iter().filter(|c| c.class == class).cloned().collect() };
    let resolved = resolve(vec![
        typing_credits(&ev, false),
        of(CreditClass::Editing),
        of(CreditClass::Terminal),
        of(CreditClass::Testing),
        reading_credits_v3(&ev, &windows),
    ]);
    without(resolved, &pause_windows(&ev, false))
}

/// The numbers credit_v6 and credit_v7 are scored with.
#[derive(Debug, Clone, PartialEq)]
pub struct V6Params {
    pub think_secs: f64,
    pub lookback_secs: f64,
    pub answer_think_secs: f64,
    pub answer_max_secs: f64,
    /// An answer only takes the seconds this long before it was sent, however early the question was asked.
    pub answer_window_secs: f64,
    /// A reply can be read this long after it finished, if the session's next prompt hasn't closed it already.
    pub read_expiry_secs: f64,
    pub type_wpm: f64,
    pub edit_evidence_secs: f64,
    /// credit_v7 only.
    pub turn_max_secs: f64,
}

impl Default for V6Params {
    fn default() -> V6Params {
        V6Params {
            think_secs: THINK_SECS,
            lookback_secs: LOOKBACK_SECS,
            answer_think_secs: ANSWER_THINK_SECS,
            answer_max_secs: ANSWER_MAX_SECS,
            answer_window_secs: ANSWER_WINDOW_SECS,
            read_expiry_secs: READ_EXPIRY_SECS,
            type_wpm: TYPE_WPM,
            edit_evidence_secs: EDIT_EVIDENCE_SECS,
            turn_max_secs: TURN_MAX_SECS,
        }
    }
}

/// credit_v5 with the time pinned to what was in front of the person: reading only runs while a work window is in
/// front (and picks up again when they return, before the submit), typing only counts in agent windows (and only
/// seconds with input where the log has it), and answering the agent's question, plan or permission prompt earns
/// the wait. Nothing is guessed from a window's title. While an agent turn runs in a project, editor time there counts
/// only near evidence the person edited something themselves.
pub fn credit_v6(events: &[Event]) -> Vec<Credit> {
    credit_v6_with(events, &V6Params::default())
}

pub fn credit_v6_with(events: &[Event], params: &V6Params) -> Vec<Credit> {
    credit_with(events, params, false, false, false, false)
}

/// credit_v6 made causal, so credit at a time depends only on events shortly after it and a bounded time before: a turn
/// whose reply_done never came ends TURN_MAX after its last prompt or agent_start, a project learned partway applies
/// from then on, a turn or a window with no project counts only with work that has none either, heartbeats sent long
/// after the time they are for are ignored, and a pause lasts at most PAUSE_MAX.
pub fn credit_v7(events: &[Event]) -> Vec<Credit> {
    credit_v7_with(events, &V6Params::default())
}

pub fn credit_v7_with(events: &[Event], params: &V6Params) -> Vec<Credit> {
    credit_with(events, params, true, false, false, false)
}

/// credit_v7 with what proves the person edited by hand while an agent runs taken from their editor: a file it saved (the
/// WakaTime plugin's write heartbeat). A file changing on disk proves nothing, since agents write through their shells too,
/// and it no longer depends on every agent's log naming every file it changed. An editor window in front takes its project
/// from the editor's own heartbeats near it, and a short gap between work on one project, with the person active in its
/// windows on both sides, counts: as bridging time where that project's windows stayed in front, and as trips where the
/// person looked away and came straight back. While an agent works in the project, only the moments right after the
/// person's own input count.
pub fn credit_v8(events: &[Event]) -> Vec<Credit> {
    credit_v8_with(events, &V6Params::default())
}

pub fn credit_v8_with(events: &[Event], params: &V6Params) -> Vec<Credit> {
    credit_with(events, params, true, true, false, false)
}

/// credit_v8 where an agent app window that shows many projects fits the prompts typed in it: one whose log says the person
/// typed it in that app, while that app's window was in front with their input in the TYPED_HERE_SECS before it. The
/// window then fits that prompt's typing, its replies' reading and the answers in its session, whatever project it shows.
pub fn credit_v9(events: &[Event]) -> Vec<Credit> {
    credit_v9_with(events, &V6Params::default())
}

pub fn credit_v9_with(events: &[Event], params: &V6Params) -> Vec<Credit> {
    credit_with(events, params, true, true, true, false)
}

/// credit_v9 where watching the agent work earns nothing in a terminal or test window either: while a turn runs in the
/// project, those seconds count only up to BRIDGE_WAIT_SECS after the person's last input, as bridging already does. And
/// one reply is read for at most READ_MAX_SECS.
pub fn credit_v10(events: &[Event]) -> Vec<Credit> {
    credit_v10_with(events, &V6Params::default())
}

pub fn credit_v10_with(events: &[Event], params: &V6Params) -> Vec<Credit> {
    credit_with(events, params, true, true, true, true)
}

fn credit_with(events: &[Event], params: &V6Params, v7: bool, v8: bool, v9: bool, v10: bool) -> Vec<Credit> {
    let mut ev: Vec<&Event> = events.iter().collect();
    ev.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    let mut focus = focuses(&ev);
    if v8 {
        focus = refocus(&focus, &editor_marks(ev.iter().map(|e| (e.ts, e.project.as_deref(), &e.kind))));
    }
    let presents = presents(&ev);
    let presence = union(presents.iter().map(|p| (p.0, p.1)).collect());
    let spans = focus_spans(&focus, &if v9 { apps(&ev) } else { Vec::new() });
    let input = typing_base(&presents, &presence);
    let typed = if v9 { typed_in_apps(&ev, &spans, &input) } else { Typed::new() };
    let turns = running_turns(&ev, params, v7);
    let evidence = if v8 { editor_evidence(&ev, &turns, params) } else { edit_evidence(&ev, &turns, params, v7) };
    let v5 = presence_credits(&focus, &presence, class_v5);
    let of = |class: CreditClass| -> Vec<Credit> { v5.iter().filter(|c| c.class == class).cloned().collect() };
    let resolved = resolve(vec![
        typing_credits_v6(&ev, &spans, &input, &typed, params, v7),
        answered_credits(&ev, &spans, &presence, &typed, params, v7),
        editing_credits_v6(of(CreditClass::Editing), &turns, &evidence, params.edit_evidence_secs, v7),
        waited(of(CreditClass::Terminal), &turns, &input, v10),
        waited(of(CreditClass::Testing), &turns, &input, v10),
        reading_credits_v6(&ev, &spans, &presence, &typed, params, v7, v10),
    ]);
    let pauses = pause_windows(&ev, v7);
    let credited = without(resolved, &pauses);
    if v8 {
        bridged(credited, &spans, &pauses, &input, &turns, &evidence, params.edit_evidence_secs)
    } else {
        credited
    }
}

/// credit_v10: the seconds of `list` while a turn runs in their project and the person's last input is more than
/// BRIDGE_WAIT_SECS back are taken out.
fn waited(list: Vec<Credit>, turns: &[Running], input: &[Interval], v10: bool) -> Vec<Credit> {
    if !v10 {
        return list;
    }
    let recent = union(input.iter().map(|&(s, e)| (s, e + BRIDGE_WAIT_SECS)).collect());
    let mut blocked: HashMap<Option<String>, Vec<Interval>> = HashMap::new();
    let mut out = Vec::with_capacity(list.len());
    for c in list {
        let holes = blocked.entry(c.project.clone()).or_insert_with(|| {
            let running = union(turns.iter().filter(|t| t.project == c.project.as_deref()).map(|t| (t.from, t.until)).collect());
            minus(&running, &recent)
        });
        out.extend(without(vec![c], holes));
    }
    out
}

fn apps(ev: &[&Event]) -> Vec<(f64, Option<App>)> {
    ev.iter()
        .filter_map(|e| match &e.kind {
            Kind::Focus { app, .. } => Some((e.ts, app.filter(|a| *a != App::Unknown))),
            _ => None,
        })
        .collect()
}

// The agent app each prompt typed in one was typed in, by session and time (credit_v9).
type Typed<'a> = HashMap<(&'a str, u64), App>;

fn typed_in_apps<'a>(ev: &[&'a Event], spans: &[Span], input: &[Interval]) -> Typed<'a> {
    let mut out = Typed::new();
    for e in ev {
        let Kind::Prompt { session, via: Some(app), .. } = &e.kind else { continue };
        let lo = e.ts - TYPED_HERE_SECS;
        let first = spans.partition_point(|s| s.end <= lo);
        let here = spans[first..]
            .iter()
            .take_while(|s| s.start < e.ts)
            .any(|s| s.class == Class::AgentApp && s.app == Some(*app) && !within(input, s.start.max(lo), s.end.min(e.ts)).is_empty());
        if here {
            out.insert((session.as_str(), e.ts.to_bits()), *app);
        }
    }
    out
}

fn typed_app(typed: &Typed, e: &Event) -> Option<App> {
    let Kind::Prompt { session, .. } = &e.kind else { return None };
    typed.get(&(session.as_str(), e.ts.to_bits())).copied()
}

fn focuses<'a>(ev: &[&'a Event]) -> Vec<(f64, Class, Option<&'a str>)> {
    ev.iter()
        .filter_map(|e| match &e.kind {
            Kind::Focus { class, .. } => Some((e.ts, *class, e.project.as_deref())),
            _ => None,
        })
        .collect()
}

/// Where editor heartbeats put the person: (time, project) for each the WakaTime plugin sent within HEARTBEAT_LAG_SECS
/// of the time it is for, leaving out new lines it put down to AI (an agent writing in a window, maybe one behind).
pub fn editor_marks<'a>(events: impl IntoIterator<Item = (f64, Option<&'a str>, &'a Kind)>) -> Vec<(f64, &'a str)> {
    let mut out: Vec<(f64, &str)> = events
        .into_iter()
        .filter_map(|(ts, project, kind)| {
            let Kind::Heartbeat { at, ai_lines, human_lines, .. } = kind else { return None };
            let typed = human_lines.is_some_and(|n| n > 0);
            let ok = (ts - at).abs() <= HEARTBEAT_LAG_SECS && (typed || !ai_lines.is_some_and(|n| n > 0));
            ok.then_some((*at, project?))
        })
        .collect();
    out.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(b.1)));
    out
}

/// Focus changes with each editor window in front split by the projects `marks` (sorted, from `editor_marks`) name near
/// it: on Linux and macOS an editor's window doesn't say which folder it shows, so its focus event can name another of the
/// editor's folders. A moment takes the project of the nearest mark at most EDITOR_PROJECT_SECS away of those made while
/// that window was in front (a heartbeat from a window behind, like an agent's change landing in an open file, says
/// nothing about the one in front), or keeps its own.
pub fn refocus<P: Clone + PartialEq>(focus: &[(f64, Class, Option<P>)], marks: &[(f64, P)]) -> Vec<(f64, Class, Option<P>)> {
    let mut out = Vec::with_capacity(focus.len());
    for (i, (start, class, own)) in focus.iter().enumerate() {
        let (start, end) = (*start, focus.get(i + 1).map_or(f64::INFINITY, |f| f.0));
        if *class != Class::Editor {
            out.push((start, *class, own.clone()));
            continue;
        }
        let first = marks.partition_point(|m| m.0 < start);
        let near = &marks[first..marks.partition_point(|m| m.0 < end).max(first)];
        let mut pieces: Vec<(f64, Option<P>)> = Vec::new();
        let mut put = |t: f64, p: Option<P>| {
            if pieces.last().map_or(true, |l| l.1 != p) {
                pieces.push((t, p));
            }
        };
        let mut t = start;
        for (j, (at, p)) in near.iter().enumerate() {
            let lo = (at - EDITOR_PROJECT_SECS).max(j.checked_sub(1).map_or(f64::MIN, |k| (near[k].0 + at) / 2.0)).max(start);
            let hi = (at + EDITOR_PROJECT_SECS).min(near.get(j + 1).map_or(f64::INFINITY, |n| (at + n.0) / 2.0)).min(end);
            if hi > lo {
                if lo > t {
                    put(t, own.clone());
                }
                put(lo, Some(p.clone()));
                t = hi;
            }
        }
        if t < end {
            put(t, own.clone());
        }
        if pieces.is_empty() {
            pieces.push((start, own.clone()));
        }
        out.extend(pieces.into_iter().map(|(t, p)| (t, Class::Editor, p)));
    }
    out
}

/// credit_v8's bridging time and trips, added to what is credited. A gap of at most BRIDGE_MAX_SECS between two moments
/// credited to the same project, with no pause in it and the person active in that project's windows for
/// BRIDGE_ANCHOR_SECS both in the BRIDGE_MAX_SECS before it and in those after it, is bridged: a second one of the
/// project's windows stayed in front is bridging time (thinking), and a stretch away from them of at most TRIP_MAX_SECS
/// is a trip (a quick look elsewhere). While an agent turn runs in the project only seconds at most BRIDGE_WAIT_SECS after
/// the person's last input count (watching it work is the agent's time), and never an editor window the editing gate
/// refused. Only seconds nothing else has count, bridging time before trips and an earlier trip before a later one.
fn bridged(
    credited: Vec<Credit>,
    spans: &[Span],
    pauses: &[Interval],
    input: &[Interval],
    turns: &[Running],
    evidence: &[(f64, Option<&str>)],
    reach: f64,
) -> Vec<Credit> {
    let mut by_project: BTreeMap<&str, Vec<Interval>> = BTreeMap::new();
    for c in &credited {
        if let Some(p) = c.project.as_deref() {
            by_project.entry(p).or_default().push((c.start, c.end));
        }
    }
    let recent = union(input.iter().map(|&(s, e)| (s, e + BRIDGE_WAIT_SECS)).collect());
    let (mut bridges, mut trips) = (Vec::new(), Vec::new());
    for (project, list) in by_project {
        let windows = |class: fn(Class) -> bool| union(spans.iter().filter(|s| class(s.class) && s.project == Some(project)).map(|s| (s.start, s.end)).collect());
        let home = windows(is_work);
        let active = meet(&home, input);
        let running = union(turns.iter().filter(|t| t.project == Some(project)).map(|t| (t.from, t.until)).collect());
        let vouched = union(evidence.iter().filter(|x| x.1 == Some(project)).map(|x| (x.0 - reach, x.0 + reach)).collect());
        let gated = minus(&meet(&windows(|c| c == Class::Editor), &running), &vouched);
        let blocked = union([minus(&running, &recent), gated].concat());
        let anchored = |lo: f64, hi: f64| total_secs(&within(&active, lo, hi)) >= BRIDGE_ANCHOR_SECS;
        for w in union(list).windows(2) {
            let (lo, hi) = (w[0].1, w[1].0);
            if hi - lo > BRIDGE_MAX_SECS || !within(pauses, lo, hi).is_empty() || !anchored(lo - BRIDGE_MAX_SECS, lo) || !anchored(hi, hi + BRIDGE_MAX_SECS) {
                continue;
            }
            let stay = within(&home, lo, hi);
            let away: Vec<Interval> = outside(&stay, lo, hi).into_iter().filter(|a| a.1 - a.0 <= TRIP_MAX_SECS).collect();
            let credit = |(start, end): Interval, class| Credit { start, end, project: Some(project.to_string()), class };
            trips.extend(minus(&away, &blocked).into_iter().map(|a| credit(a, CreditClass::Trip)));
            bridges.extend(minus(&stay, &blocked).into_iter().map(|h| credit(h, CreditClass::Bridge)));
        }
    }
    resolve(vec![credited, bridges, trips])
}

/// A sorted, disjoint list less another.
fn minus(list: &[Interval], holes: &[Interval]) -> Vec<Interval> {
    list.iter().flat_map(|&(lo, hi)| outside(&within(holes, lo, hi), lo, hi)).collect()
}

/// [lo, hi) less a sorted, disjoint list inside it.
fn outside(list: &[Interval], lo: f64, hi: f64) -> Vec<Interval> {
    let mut out = Vec::new();
    let mut cur = lo;
    for &(s, e) in list {
        if s > cur {
            out.push((cur, s));
        }
        cur = cur.max(e);
    }
    if hi > cur {
        out.push((cur, hi));
    }
    out
}

struct Span<'a> {
    start: f64,
    end: f64,
    class: Class,
    project: Option<&'a str>,
    app: Option<App>,
}

fn focus_spans<'a>(focus: &[(f64, Class, Option<&'a str>)], apps: &[(f64, Option<App>)]) -> Vec<Span<'a>> {
    focus
        .iter()
        .enumerate()
        .map(|(i, &(start, class, project))| {
            let app = apps.partition_point(|a| a.0 <= start).checked_sub(1).and_then(|j| apps[j].1);
            Span { start, end: focus.get(i + 1).map_or(f64::MAX, |f| f.0), class, project, app }
        })
        .collect()
}

fn is_agent(class: Class) -> bool {
    matches!(class, Class::AgentTerminal | Class::AgentApp | Class::WebAgent | Class::Editor)
}

fn is_work(class: Class) -> bool {
    is_agent(class) || matches!(class, Class::Terminal | Class::Testing | Class::Docs)
}

fn same_project(a: Option<&str>, b: Option<&str>) -> bool {
    a.is_none() || b.is_none() || a == b
}

// In v7 only `open` with no project fits any project; `exact` with none fits only none.
fn project_fits(open: Option<&str>, exact: Option<&str>, v7: bool) -> bool {
    if v7 {
        open.is_none() || open == exact
    } else {
        same_project(open, exact)
    }
}

// The window shows the work's project, or (credit_v9) is the agent app it was typed in.
fn fits(s: &Span, project: Option<&str>, app: Option<App>, v7: bool) -> bool {
    project_fits(project, s.project, v7) || (app.is_some() && s.app == app)
}

/// The part of a sorted, disjoint list inside [lo, hi).
fn within(list: &[Interval], lo: f64, hi: f64) -> Vec<Interval> {
    let first = list.partition_point(|w| w.1 <= lo);
    list[first..].iter().take_while(|w| w.0 < hi).map(|w| (w.0.max(lo), w.1.min(hi))).collect()
}

fn meet(a: &[Interval], b: &[Interval]) -> Vec<Interval> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() && j < b.len() {
        let (s, e) = (a[i].0.max(b[j].0), a[i].1.min(b[j].1));
        if e > s {
            out.push((s, e));
        }
        if a[i].1 < b[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// Time inside [lo, hi) that is in `base` while an agent window that `fits` was in front.
fn in_front(spans: &[Span], base: &[Interval], project: Option<&str>, app: Option<App>, lo: f64, hi: f64, v7: bool) -> Vec<Interval> {
    let first = spans.partition_point(|s| s.end <= lo);
    let front: Vec<Interval> = spans[first..]
        .iter()
        .take_while(|s| s.start < hi)
        .filter(|s| is_agent(s.class) && fits(s, project, app, v7))
        .map(|s| (s.start.max(lo), s.end.min(hi)))
        .collect();
    meet(&front, &within(base, lo, hi))
}

/// The latest `budget` seconds of a sorted, disjoint list.
fn latest(list: &[Interval], budget: f64) -> Vec<Interval> {
    let mut left = budget;
    let mut out = Vec::new();
    for &(s, e) in list.iter().rev() {
        if left <= EPS {
            break;
        }
        let take = (e - s).min(left);
        out.push((e - take, e));
        left -= take;
    }
    out
}

fn typing_credits_v6(ev: &[&Event], spans: &[Span], base: &[Interval], typed: &Typed, params: &V6Params, v7: bool) -> Vec<Credit> {
    let mut last: HashMap<&str, f64> = HashMap::new();
    let mut out = Vec::new();
    for e in ev {
        let Kind::Prompt { session, words, pasted, .. } = &e.kind else { continue };
        let previous = last.insert(session, e.ts);
        if *words < MIN_PROMPT_WORDS {
            continue;
        }
        let count = if *pasted { (*words).min(PASTE_TYPED_WORDS_CAP) } else { *words };
        let lo = (e.ts - params.lookback_secs).max(previous.unwrap_or(f64::MIN));
        let seconds = in_front(spans, base, e.project.as_deref(), typed_app(typed, e), lo, e.ts, v7);
        for (start, end) in latest(&seconds, 60.0 * count as f64 / params.type_wpm) {
            out.push(Credit { start, end, project: e.project.clone(), class: CreditClass::Typing });
        }
    }
    out
}

fn answered_credits(ev: &[&Event], spans: &[Span], presence: &[Interval], typed: &Typed, params: &V6Params, v7: bool) -> Vec<Credit> {
    let mut app_of: HashMap<&str, Option<App>> = HashMap::new();
    let mut out = Vec::new();
    for e in ev {
        if let Kind::Prompt { session, .. } = &e.kind {
            app_of.insert(session, typed_app(typed, e));
        }
        let Kind::Answered { session, asked, shown_words, answer_words, .. } = &e.kind else { continue };
        if !(*asked < e.ts) {
            continue;
        }
        let budget = 60.0 * (*shown_words as f64 / READ_WPM + *answer_words as f64 / params.type_wpm) + params.answer_think_secs;
        let app = app_of.get(session.as_str()).copied().flatten();
        let seconds = in_front(spans, presence, e.project.as_deref(), app, asked.max(e.ts - params.answer_window_secs), e.ts, v7);
        for (start, end) in latest(&seconds, budget.min(params.answer_max_secs)) {
            out.push(Credit { start, end, project: e.project.clone(), class: CreditClass::Reading });
        }
    }
    out
}

struct Reply<'a> {
    session: &'a str,
    from: f64,
    until: f64,
    budget: f64,
    project: Option<&'a str>,
    app: Option<App>,
}

fn replies<'a>(ev: &[&'a Event], typed: &Typed, params: &V6Params, v7: bool, v10: bool) -> Vec<Reply<'a>> {
    let cap = if v10 { READ_MAX_SECS } else { f64::INFINITY };
    let mut pending: HashMap<&'a str, (f64, usize, Option<&'a str>, Option<App>)> = HashMap::new();
    let mut app_of: HashMap<&'a str, Option<App>> = HashMap::new();
    let mut out = Vec::new();
    let mut close = |session: &'a str, (from, reply_words, project, app): (f64, usize, Option<&'a str>, Option<App>), until: f64| {
        let until = until.min(from + params.read_expiry_secs);
        out.push(Reply { session, from, until, budget: (60.0 * reply_words as f64 / READ_WPM + params.think_secs).min(cap), project, app });
    };
    for e in ev {
        match &e.kind {
            Kind::ReplyDone { session, reply_words, .. } => {
                let app = app_of.get(session.as_str()).copied().flatten();
                if let Some(old) = pending.insert(session, (e.ts, *reply_words, e.project.as_deref(), app)) {
                    close(session, old, e.ts);
                }
            }
            Kind::Prompt { session, .. } => {
                app_of.insert(session, typed_app(typed, e));
                if let Some((from, words, project, app)) = pending.remove(session.as_str()) {
                    let project = if v7 { project } else { project.or(e.project.as_deref()) };
                    close(session, (from, words, project, app), e.ts);
                }
            }
            _ => {}
        }
    }
    for (session, reply) in pending {
        close(session, reply, f64::MAX);
    }
    out.sort_by(|a, b| a.from.total_cmp(&b.from).then(a.session.cmp(b.session)));
    out
}

// Each second goes to one reply. A reply's budget is spent only by seconds a work window showing its project was in
// front, so time elsewhere pauses it. Where several want a second, the one whose project the agent window in front
// shows comes first, then the oldest.
fn reading_credits_v6(ev: &[&Event], spans: &[Span], presence: &[Interval], typed: &Typed, params: &V6Params, v7: bool, v10: bool) -> Vec<Credit> {
    let replies = replies(ev, typed, params, v7, v10);
    let mut left: Vec<f64> = replies.iter().map(|r| r.budget).collect();
    let mut next = 0;
    let mut live: Vec<usize> = Vec::new();
    let mut out = Vec::new();
    for span in spans.iter().filter(|s| is_work(s.class)) {
        for (start, end) in within(presence, span.start, span.end) {
            let mut x = start;
            while x < end {
                while next < replies.len() && replies[next].from <= x {
                    live.push(next);
                    next += 1;
                }
                live.retain(|&i| replies[i].until > x && left[i] > EPS);
                let upcoming = replies.get(next).map_or(f64::MAX, |r| r.from);
                let stop = live.iter().map(|&i| replies[i].until).fold(end.min(upcoming), f64::min);
                let mut at = x;
                while at < stop {
                    let strong = |i: usize| is_agent(span.class) && span.project.is_some() && replies[i].project == span.project;
                    let typed_here = |i: usize| replies[i].app.is_some() && replies[i].app == span.app;
                    let Some(i) = live
                        .iter()
                        .copied()
                        .filter(|&i| left[i] > EPS && fits(span, replies[i].project, replies[i].app, v7))
                        .min_by_key(|&i| (!typed_here(i), !strong(i), i))
                    else {
                        break;
                    };
                    let take = left[i].min(stop - at);
                    out.push(Credit { start: at, end: at + take, project: replies[i].project.map(str::to_string), class: CreditClass::Reading });
                    left[i] -= take;
                    at += take;
                }
                x = stop;
            }
        }
    }
    out
}

struct Running<'a> {
    from: f64,
    until: f64,
    project: Option<&'a str>,
    harness: Option<&'a str>,
}

// A turn runs from a session's first prompt or agent_start after its last reply_done to the next reply_done, or to
// the end of the log. Some sources emit no agent_start (gemini) and the rest only on the first tool call, so the
// prompt is what starts those.
fn running_turns<'a>(ev: &[&'a Event], params: &V6Params, v7: bool) -> Vec<Running<'a>> {
    if v7 {
        return turns_v7(ev, prompt_or_start, params.turn_max_secs);
    }
    let mut open: HashMap<&str, Running> = HashMap::new();
    let mut out = Vec::new();
    for e in ev {
        match &e.kind {
            Kind::Prompt { session, .. } | Kind::AgentStart { session } => {
                let t = open.entry(session).or_insert(Running { from: e.ts, until: f64::MAX, project: None, harness: None });
                t.project = t.project.or(e.project.as_deref());
                t.harness = t.harness.or(e.harness.as_deref());
            }
            Kind::ReplyDone { session, .. } => {
                if let Some(t) = open.remove(session.as_str()) {
                    out.push(Running { until: e.ts, ..t });
                }
            }
            _ => {}
        }
    }
    out.extend(open.into_values());
    out.retain(|t| t.until > t.from);
    out.sort_by(|a, b| a.from.total_cmp(&b.from));
    out
}

fn prompt_or_start(kind: &Kind) -> Option<&str> {
    match kind {
        Kind::Prompt { session, .. } | Kind::AgentStart { session } => Some(session),
        _ => None,
    }
}

fn agent_start(kind: &Kind) -> Option<&str> {
    match kind {
        Kind::AgentStart { session } => Some(session),
        _ => None,
    }
}

// v7's turns, in pieces: each start (prompt or agent_start, as `starts` says) runs until the session's next start, its
// reply_done or `turn_max` after it, whichever comes first. So whether a turn runs at a time depends only on the
// `turn_max` before it. A project or harness counts from the start that names it, and later starts without one keep
// it while that start is less than `turn_max` old.
fn turns_v7<'a>(ev: &[&'a Event], starts: fn(&Kind) -> Option<&str>, turn_max: f64) -> Vec<Running<'a>> {
    struct Open<'a> {
        from: f64,
        project: Option<(&'a str, f64)>,
        harness: Option<(&'a str, f64)>,
    }
    let close = |o: &Open<'a>, at: f64| Running { from: o.from, until: at.min(o.from + turn_max), project: o.project.map(|p| p.0), harness: o.harness.map(|h| h.0) };
    let mut open: HashMap<&str, Open> = HashMap::new();
    let mut out = Vec::new();
    for e in ev {
        if let Kind::ReplyDone { session, .. } = &e.kind {
            if let Some(o) = open.remove(session.as_str()) {
                out.push(close(&o, e.ts));
            }
        } else if let Some(session) = starts(&e.kind) {
            let last = open.remove(session);
            let named = |now: Option<&'a str>, before: Option<(&'a str, f64)>| now.map(|x| (x, e.ts)).or(before.filter(|b| e.ts < b.1 + turn_max));
            let next = Open {
                from: e.ts,
                project: named(e.project.as_deref(), last.as_ref().and_then(|o| o.project)),
                harness: named(e.harness.as_deref(), last.as_ref().and_then(|o| o.harness)),
            };
            out.extend(last.map(|o| close(&o, e.ts)));
            open.insert(session, next);
        }
    }
    out.extend(open.values().map(|o| close(o, f64::INFINITY)));
    out.retain(|t| t.until > t.from);
    out.sort_by(|a, b| a.from.total_cmp(&b.from));
    out
}

// When the person edited something themselves: an editor heartbeat that wrote lines by hand, or a saved file no
// agent_edit of that project touched nearby (only where every turn running then comes from a harness that makes them).
fn edit_evidence<'a>(ev: &[&'a Event], turns: &[Running], params: &V6Params, v7: bool) -> Vec<(f64, Option<&'a str>)> {
    let agent_edits: Vec<(f64, Option<&str>, &str)> = ev
        .iter()
        .filter_map(|e| match &e.kind {
            Kind::AgentEdit { path, .. } => Some((e.ts, e.project.as_deref(), path.as_str())),
            _ => None,
        })
        .collect();
    let mut out = Vec::new();
    for e in ev {
        match &e.kind {
            Kind::Heartbeat { at, write: true, human_lines: Some(lines), .. } if *lines > 0 && (!v7 || (e.ts - at).abs() <= HEARTBEAT_LAG_SECS) => {
                out.push((*at, e.project.as_deref()))
            }
            Kind::FileSave { paths, .. } => {
                let traced = turns
                    .iter()
                    .take_while(|t| t.from <= e.ts)
                    .filter(|t| t.until > e.ts && project_fits(e.project.as_deref(), t.project, v7))
                    .all(|t| t.harness.is_some_and(|h| EDIT_TRACED.contains(&h)));
                let first = agent_edits.partition_point(|a| a.0 < e.ts - params.edit_evidence_secs);
                let near: Vec<_> = agent_edits[first..]
                    .iter()
                    .take_while(|a| a.0 <= e.ts + params.edit_evidence_secs)
                    .filter(|a| a.1 == e.project.as_deref())
                    .collect();
                if traced && paths.iter().any(|p| !near.iter().any(|a| a.2 == p)) {
                    out.push((e.ts, e.project.as_deref()));
                }
            }
            _ => {}
        }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

// credit_v8's: a file the person's editor saved, unless an agent edited that file nearby (the editor saving its change), or
// the plugin put the new lines down to AI, or an agent that saves through the editor was running in that project and the
// plugin didn't see the person type new lines.
fn editor_evidence<'a>(ev: &[&'a Event], turns: &[Running], params: &V6Params) -> Vec<(f64, Option<&'a str>)> {
    let agent_edits: Vec<(f64, Option<&str>, &str)> = ev
        .iter()
        .filter_map(|e| match &e.kind {
            Kind::AgentEdit { path, .. } => Some((e.ts, e.project.as_deref(), path.as_str())),
            _ => None,
        })
        .collect();
    let mut out = Vec::new();
    for e in ev {
        let Kind::Heartbeat { at, write: true, path, ai_lines, human_lines, .. } = &e.kind else { continue };
        if (e.ts - at).abs() > HEARTBEAT_LAG_SECS {
            continue;
        }
        let project = e.project.as_deref();
        let typed = human_lines.is_some_and(|n| n > 0);
        if ai_lines.is_some_and(|n| n > 0) && !typed {
            continue;
        }
        let first = agent_edits.partition_point(|a| a.0 < at - params.edit_evidence_secs);
        let agent = path.as_deref().is_some_and(|p| {
            agent_edits[first..].iter().take_while(|a| a.0 <= at + params.edit_evidence_secs).any(|a| a.1 == project && a.2 == p)
        });
        let in_editor = turns
            .iter()
            .take_while(|t| t.from <= *at)
            .any(|t| t.until > *at && project_fits(project, t.project, true) && t.harness.is_some_and(|h| IN_EDITOR.contains(&h)));
        if !agent && (typed || !in_editor) {
            out.push((*at, project));
        }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

fn editing_credits_v6(edits: Vec<Credit>, turns: &[Running], evidence: &[(f64, Option<&str>)], reach: f64, v7: bool) -> Vec<Credit> {
    let busy = union(turns.iter().map(|t| (t.from, t.until)).collect());
    let mut out = Vec::new();
    for c in edits {
        if within(&busy, c.start, c.end).is_empty() {
            out.push(c);
            continue;
        }
        let project = c.project.as_deref();
        let running = union(
            turns
                .iter()
                .take_while(|t| t.from < c.end)
                .filter(|t| t.until > c.start && project_fits(project, t.project, v7))
                .map(|t| (t.from.max(c.start), t.until.min(c.end)))
                .collect(),
        );
        let first = evidence.partition_point(|x| x.0 < c.start - reach);
        let vouched = union(
            evidence[first..]
                .iter()
                .take_while(|x| x.0 <= c.end + reach)
                .filter(|x| project.is_none() || x.1 == project)
                .map(|x| (x.0 - reach, x.0 + reach))
                .collect(),
        );
        out.extend(without(vec![c.clone()], &running));
        out.extend(meet(&running, &vouched).into_iter().map(|(start, end)| Credit { start, end, ..c.clone() }));
    }
    out
}

/// Seconds a typing credit may stand on: presence, and where the log says so, only seconds with input.
fn typing_base(presents: &[(f64, f64, Option<u64>)], presence: &[Interval]) -> Vec<Interval> {
    let mut seconds = Vec::new();
    for &(lo, hi, bits) in presents {
        match bits {
            Some(bits) => {
                let start = hi - PRESENT_SECS;
                seconds.extend((0..PRESENT_SECS as u32).filter(|i| bits >> i & 1 == 1).map(|i| (start + f64::from(i), start + f64::from(i) + 1.0)));
            }
            None => seconds.push((lo, hi)),
        }
    }
    meet(presence, &union(seconds))
}

fn borrow_projects(mut credits: Vec<Credit>, anchors: &[(f64, &str)]) -> Vec<Credit> {
    for c in credits.iter_mut().filter(|c| c.project.is_none()) {
        let before = anchors.partition_point(|a| a.0 < c.end);
        if let Some(&(ts, project)) = before.checked_sub(1).map(|i| &anchors[i]) {
            if c.start - ts <= INFER_SECS {
                c.project = Some(project.to_string());
            }
        }
    }
    credits
}

fn pause_windows(ev: &[&Event], v7: bool) -> Vec<Interval> {
    let mut out = Vec::new();
    for (i, e) in ev.iter().enumerate() {
        if let Kind::Pause { until } = e.kind {
            let resumed = ev[i + 1..].iter().find(|r| matches!(r.kind, Kind::Resume {})).map_or(f64::MAX, |r| r.ts);
            let end = if v7 { until.min(resumed).min(e.ts + PAUSE_MAX_SECS) } else { until.min(resumed) };
            if end > e.ts {
                out.push((e.ts, end));
            }
        }
    }
    union(out)
}

fn without(credits: Vec<Credit>, holes: &[Interval]) -> Vec<Credit> {
    if holes.is_empty() {
        return credits;
    }
    let mut out = Vec::with_capacity(credits.len());
    for c in credits {
        let mut cur = c.start;
        let first = holes.partition_point(|h| h.1 <= c.start);
        for &(hs, he) in &holes[first..] {
            if hs >= c.end {
                break;
            }
            if hs > cur {
                out.push(Credit { start: cur, end: hs, ..c.clone() });
            }
            cur = cur.max(he);
        }
        if c.end - cur > EPS {
            out.push(Credit { start: cur, ..c });
        }
    }
    out
}

pub fn unattributed_ai_secs(events: &[Event]) -> f64 {
    let mut ev: Vec<&Event> = events.iter().collect();
    ev.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    let windows = present_windows(&ev);
    let iv = reading_credits_v3(&ev, &windows).into_iter().filter(|c| c.project.is_none()).map(|c| (c.start, c.end));
    total_secs(&union(iv.collect()))
}

pub fn agent_runtime(events: &[Event]) -> Vec<Interval> {
    union(agent_turns(events).into_iter().map(|t| t.0).collect())
}

/// The pieces of agent_runtime with their projects: credit_v7's turns, started by agent_start only, each in the
/// project its start names or keeps (a reply in another project doesn't move it). A piece that neither the session's
/// next agent_start nor its reply_done ends runs until the last event at most TURN_MAX after it, so a turn still
/// running counts as far as the log goes and a later recompute extends it.
pub fn agent_turns(events: &[Event]) -> Vec<(Interval, Option<&str>)> {
    let mut ev: Vec<&Event> = events.iter().collect();
    ev.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    // These are written by kairosd about itself and the computer, not by anything the agent or the person did.
    let about_the_device =
        |k: &Kind| matches!(k, Kind::Receipt { .. } | Kind::Slept { .. } | Kind::Screen { .. } | Kind::Machine { .. } | Kind::Uploads { .. } | Kind::Reader { .. });
    let times: Vec<f64> = ev.iter().filter(|e| !about_the_device(&e.kind)).map(|e| e.ts).collect();
    turns_v7(&ev, agent_start, TURN_MAX_SECS)
        .into_iter()
        .filter_map(|t| {
            let last = times[..times.partition_point(|&x| x <= t.until)].last().copied()?;
            (last > t.from).then_some(((t.from, last), t.project))
        })
        .collect()
}

fn reading_credits(ev: &[&Event]) -> Vec<Credit> {
    let mut pending: HashMap<&str, (f64, usize, Option<&str>)> = HashMap::new();
    let mut out = Vec::new();
    for e in ev {
        match &e.kind {
            Kind::ReplyDone { session, reply_words, .. } => {
                pending.insert(session, (e.ts, *reply_words, e.project.as_deref()));
            }
            Kind::Prompt { session, words, pasted, .. } => {
                let Some((t0, reply_words, project)) = pending.remove(session.as_str()) else {
                    continue;
                };
                let gap = e.ts - t0;
                if gap <= 0.0 || *words < MIN_PROMPT_WORDS {
                    continue;
                }
                let typed = if *pasted { (*words).min(PASTE_TYPED_WORDS_CAP) } else { *words };
                let allowed = 60.0 * (reply_words as f64 / READ_WPM + typed as f64 / TYPE_WPM) + THINK_SECS;
                out.push(Credit {
                    start: t0,
                    end: t0 + gap.min(allowed),
                    project: project.or(e.project.as_deref()).map(str::to_string),
                    class: CreditClass::Reading,
                });
            }
            _ => {}
        }
    }
    out
}

fn class_v1(class: Class) -> Option<CreditClass> {
    match class {
        Class::Editor => Some(CreditClass::Editing),
        Class::Testing => Some(CreditClass::Testing),
        _ => None,
    }
}

fn class_v4(class: Class) -> Option<CreditClass> {
    match class {
        Class::Docs => Some(CreditClass::Docs),
        other => class_v3(other),
    }
}

fn class_v5(class: Class) -> Option<CreditClass> {
    match class {
        Class::Editor => Some(CreditClass::Editing),
        Class::Terminal => Some(CreditClass::Terminal),
        Class::Testing => Some(CreditClass::Testing),
        _ => None,
    }
}

fn class_v3(class: Class) -> Option<CreditClass> {
    match class {
        Class::Editor => Some(CreditClass::Editing),
        Class::Terminal => Some(CreditClass::Terminal),
        Class::Testing => Some(CreditClass::Testing),
        Class::WebAgent => Some(CreditClass::WebAgent),
        _ => None,
    }
}

fn present_windows(ev: &[&Event]) -> Vec<Interval> {
    presents(ev).into_iter().map(|p| (p.0, p.1)).collect()
}

// Each present event's window (cut at the last idle) and its input bits, if the log has them.
fn presents(ev: &[&Event]) -> Vec<(f64, f64, Option<u64>)> {
    let idles: Vec<f64> = ev.iter().filter(|e| matches!(e.kind, Kind::Idle {})).map(|e| e.ts).collect();
    let mut out = Vec::new();
    for p in ev {
        let Kind::Present { input } = &p.kind else { continue };
        let last_idle = idles[..idles.partition_point(|&t| t <= p.ts)].last().copied();
        let lo = (p.ts - PRESENT_SECS).max(last_idle.unwrap_or(f64::MIN));
        if p.ts > lo {
            out.push((lo, p.ts, input.as_deref().and_then(|h| u64::from_str_radix(h, 16).ok())));
        }
    }
    out
}

fn presence_credits(focus: &[(f64, Class, Option<&str>)], windows: &[Interval], map: fn(Class) -> Option<CreditClass>) -> Vec<Credit> {
    let mut out = Vec::new();
    for &(lo, hi) in windows {
        let first = focus.partition_point(|f| f.0 <= lo).saturating_sub(1);
        for (i, &(fs, class, project)) in focus.iter().enumerate().skip(first) {
            if fs >= hi {
                break;
            }
            let Some(class) = map(class) else { continue };
            let fe = focus.get(i + 1).map_or(f64::MAX, |f| f.0);
            let (s, e) = (fs.max(lo), fe.min(hi));
            if e > s {
                out.push(Credit { start: s, end: e, project: project.map(str::to_string), class });
            }
        }
    }
    out
}

fn typing_credits(ev: &[&Event], project_only: bool) -> Vec<Credit> {
    ev.iter()
        .filter_map(|e| match &e.kind {
            Kind::Prompt { words, pasted, .. }
                if *words >= MIN_PROMPT_WORDS && (e.project.is_some() || !project_only) =>
            {
                let typed = if *pasted { (*words).min(PASTE_TYPED_WORDS_CAP) } else { *words };
                Some(Credit {
                    start: e.ts - 60.0 * typed as f64 / TYPE_WPM,
                    end: e.ts,
                    project: e.project.clone(),
                    class: CreditClass::Typing,
                })
            }
            _ => None,
        })
        .collect()
}

fn reading_credits_v3(ev: &[&Event], windows: &[Interval]) -> Vec<Credit> {
    let windows = union(windows.to_vec());
    let mut pending: HashMap<&str, (f64, usize, Option<&str>)> = HashMap::new();
    let mut out = Vec::new();
    let mut close = |t0: f64, reply_words: usize, project: Option<&str>, until: f64| {
        let end = t0 + (until - t0).min(60.0 * reply_words as f64 / READ_WPM + THINK_SECS);
        let first = windows.partition_point(|w| w.1 <= t0);
        for &(ws, we) in &windows[first..] {
            if ws >= end {
                break;
            }
            let (s, e) = (ws.max(t0), we.min(end));
            if e > s {
                out.push(Credit { start: s, end: e, project: project.map(str::to_string), class: CreditClass::Reading });
            }
        }
    };
    for e in ev {
        match &e.kind {
            Kind::ReplyDone { session, reply_words, .. } => {
                if let Some((t0, rw, project)) = pending.insert(session, (e.ts, *reply_words, e.project.as_deref())) {
                    close(t0, rw, project, e.ts);
                }
            }
            Kind::Prompt { session, .. } => {
                if let Some((t0, rw, project)) = pending.remove(session.as_str()) {
                    close(t0, rw, project.or(e.project.as_deref()), e.ts);
                }
            }
            _ => {}
        }
    }
    // Replies still unread can overlap; the oldest claims the shared time, never the map's order.
    let mut left: Vec<(&str, (f64, usize, Option<&str>))> = pending.into_iter().collect();
    left.sort_by(|a, b| a.1 .0.total_cmp(&b.1 .0).then(a.0.cmp(b.0)));
    for (_, (t0, rw, project)) in left {
        close(t0, rw, project, f64::MAX);
    }
    out
}

fn resolve(groups: Vec<Vec<Credit>>) -> Vec<Credit> {
    let mut covered: Vec<Interval> = Vec::new();
    let mut out: Vec<Credit> = Vec::new();
    for mut group in groups {
        group.sort_by(|a, b| a.start.total_cmp(&b.start));
        for c in group {
            let mut pieces = Vec::new();
            let mut cur = c.start;
            let first = covered.partition_point(|iv| iv.1 <= c.start);
            for &(s, e) in &covered[first..] {
                if s >= c.end {
                    break;
                }
                if s > cur {
                    pieces.push((cur, s));
                }
                cur = cur.max(e);
            }
            if cur < c.end {
                pieces.push((cur, c.end));
            }
            for (s, e) in pieces.into_iter().filter(|(s, e)| e - s > EPS) {
                let at = covered.partition_point(|iv| iv.0 < s);
                covered.insert(at, (s, e));
                out.push(Credit { start: s, end: e, project: c.project.clone(), class: c.class });
            }
        }
    }
    out.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut merged: Vec<Credit> = Vec::new();
    for c in out {
        match merged.last_mut() {
            Some(l) if l.project == c.project && l.class == c.class && c.start - l.end < EPS => {
                l.end = l.end.max(c.end)
            }
            _ => merged.push(c),
        }
    }
    merged
}

pub fn total_credit_secs(credits: &[Credit]) -> f64 {
    credits.iter().fold(0.0, |acc, c| acc + (c.end - c.start))
}
