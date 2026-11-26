//! Two sessions of one harness at once, in two terminal windows and two projects. The person switches between them,
//! looks away briefly and for long, and Kairos has to follow: each session's events in its own project, the window in
//! front as the person had it, and credit only for the project whose window they were at.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use portable_pty::CommandBuilder;
use serde_json::{json, Value};

use crate::credit::{self, Shown, LOOKS};
use crate::harness::Harness;
use crate::model::Model;
use crate::pane::{self, Pane, Window};
use crate::person::Person;
use crate::scenario::{self, Scenario, Turn};
use crate::term::{Term, ENTER};
use crate::{check_session, command, daemon, events, kairosd_version, now, place, ready, repo, report, wait_focus, Args, Hands};

const SECOND: &str = "proj2";

/// The windows on the screen and which one the person has in front.
struct Desk {
    person: Person,
    main: Window,
    pane: Window,
    /// The second window runs its agent.
    up: bool,
    shown: Vec<(f64, Option<&'static str>)>,
}

impl Desk {
    /// Clicks on the lab's own window (`true`) or the second one.
    fn switch(&mut self, main: bool) -> Result<()> {
        self.person.press();
        // The window is in front before the raise returns.
        self.shown.push((now(), if main { Some("proj") } else { self.up.then_some(SECOND) }));
        pane::raise(if main { &self.main } else { &self.pane })
    }

    fn spans(&self, end: f64) -> Vec<Shown> {
        self.shown.iter().enumerate().map(|(i, &(t, p))| (t, self.shown.get(i + 1).map_or(end, |n| n.0), p)).collect()
    }
}

/// Types the turn's prompt and sends it, then takes the hands off.
fn send(term: &mut Term, model: &Model, i: usize, turn: &Turn, person: &Person) -> Result<()> {
    person.busy(true);
    model.arm(i);
    term.type_text(turn.prompt)?;
    sleep(Duration::from_millis(400));
    person.busy(false);
    person.press();
    term.send(ENTER)
}

/// Waits for the turn's answer, pressing what the harness asks for meanwhile.
fn answer(h: &Harness, term: &mut Term, hands: &mut Hands, model: &Model, i: usize) -> Result<()> {
    let until = Instant::now() + Duration::from_secs(150);
    while !model.said(i) {
        if term.exited() {
            bail!("{} quit during turn {}", h.id, i + 1);
        }
        if Instant::now() > until {
            bail!("turn {} never finished. Screen:\n{}", i + 1, term.screen());
        }
        hands.look(term, h.during)?;
        sleep(Duration::from_millis(200));
    }
    Ok(())
}

fn secs(n: u64) {
    sleep(Duration::from_secs(n));
}

struct Second<'a> {
    pane: &'a mut Pane,
    cmd: CommandBuilder,
    raw: &'a Path,
    term: &'a mut Option<Term>,
    model: &'a Model,
    scenario: &'a Scenario,
}

/// The script of the run. Gives back when the second window's agent started.
#[allow(clippy::too_many_arguments)]
fn drive(h: &Harness, term: &mut Term, model: &Model, a: &Scenario, second: Second, desk: &mut Desk, kd: &Path) -> Result<f64> {
    let person = desk.person.clone();
    let mut hands = Hands::new(&person);
    ready(h, term, &mut hands)?;

    // A turn, then a quick look at the other window while reading the reply.
    send(term, model, 0, &a.turns[0], &person)?;
    answer(h, term, &mut hands, model, 0)?;
    person.busy(true);
    secs(6);
    desk.switch(false)?;
    secs(10);
    desk.switch(true)?;
    secs(6);

    // Away for long, busy in the other window, while the agent works.
    send(term, model, 1, &a.turns[1], &person)?;
    secs(2);
    person.busy(true);
    desk.switch(false)?;
    secs(40);
    desk.switch(true)?;
    person.busy(false);
    answer(h, term, &mut hands, model, 1)?;
    person.busy(true);
    secs(6);

    // A second agent in the other window, in another project.
    desk.switch(false)?;
    secs(3);
    person.busy(false);
    let started = now();
    *second.term = Some(second.pane.start(&second.cmd, second.raw)?);
    let tb = second.term.as_mut().expect("just started");
    let mut hands_b = Hands::new(&person);
    ready(h, tb, &mut hands_b)?;
    let seen = wait_focus(kd, SECOND, started)?;
    desk.up = true;
    desk.shown.push((seen.unwrap_or_else(now), Some(SECOND)));
    send(tb, second.model, 0, &second.scenario.turns[0], &person)?;
    answer(h, tb, &mut hands_b, second.model, 0)?;
    person.busy(true);
    secs(6);

    // A prompt in the first window, and while its agent works one in the second.
    desk.switch(true)?;
    secs(2);
    send(term, model, 2, &a.turns[2], &person)?;
    secs(2);
    desk.switch(false)?;
    send(tb, second.model, 1, &second.scenario.turns[1], &person)?;
    answer(h, tb, &mut hands_b, second.model, 1)?;
    person.busy(true);
    secs(6);
    desk.switch(true)?;
    person.busy(false);
    answer(h, term, &mut hands, model, 2)?;
    person.busy(true);
    secs(6);

    send(term, model, 3, &a.turns[3], &person)?;
    answer(h, term, &mut hands, model, 3)?;
    person.busy(true);
    Ok(started)
}

/// The window Kairos saw in front against the one the person had: it has LOOKS seconds to see each switch.
fn front(all: &[Value], shown: &[Shown]) -> Vec<String> {
    let focus: Vec<(f64, (Value, Value))> =
        all.iter().filter(|e| e["kind"] == "focus").map(|e| (e["ts"].as_f64().unwrap_or(0.0), (e["class"].clone(), e["project"].clone()))).collect();
    let first = shown.first().map_or(0.0, |s| s.0);
    let mut problems = Vec::new();
    for &(from, to, project) in shown {
        let want = match project {
            Some(p) => (json!("agent_terminal"), json!(p)),
            None => (json!("other"), Value::Null),
        };
        let had = || project.map_or("a window that is no project's".to_string(), |p| format!("agent_terminal in {p}"));
        match focus.iter().rev().find(|f| f.0 <= from + LOOKS) {
            Some((_, seen)) if *seen == want => {}
            Some((_, seen)) => problems.push(format!("{LOOKS} s after +{:.0} s the window in front was {} in project {}, the person had {}", from - first, seen.0, seen.1, had())),
            None => problems.push(format!("Kairos reported no window by +{:.0} s", from - first + LOOKS)),
        }
        // The next switch can show a moment before the lab writes it down.
        for (ts, seen) in focus.iter().filter(|f| f.0 > from + LOOKS && f.0 < to - 1.0) {
            if *seen != want {
                problems.push(format!("at +{:.0} s the window in front became {} in project {}, the person had {}", ts - first, seen.0, seen.1, had()));
            }
        }
    }
    problems
}

pub fn run(h: &'static Harness, args: &Args) -> Result<bool> {
    if !args.window {
        bail!("the two scenario switches windows, so it needs --window");
    }
    let root = std::path::absolute(&args.work)?;
    let (bin, version) = h.install(&args.version, &root.join("tools"))?;
    let p = place(args, h.id, &version, h.state)?;
    let second = p.run.join(SECOND);
    repo(&second)?;
    let away = std::env::temp_dir().join(format!("lab-away-{}", std::process::id()));
    fs::create_dir_all(&away)?;
    let (a, b) = scenario::two();
    let (a, b) = (Arc::new(a), Arc::new(b));
    let model = Model::start(a.clone(), p.project.clone())?;
    let model_b = model.add(b.clone(), second.clone())?;
    let url = format!("http://127.0.0.1:{}", model.port);
    eprintln!("lab: {} {version}, scenario two, run folder {}", h.id, p.run.display());

    let person = Person::new(true)?;
    let mut daemon = daemon(args, &p)?;
    sleep(Duration::from_secs(2));
    let launch = (h.prepare)(&p.home, &p.project, &url, &version)?;
    let launch_b = (h.prepare)(&p.home, &second, &url, &version)?;
    let main = pane::own(&format!("lab-main-{}", std::process::id()))?;
    let mut pane = Pane::open(&p.run, &away)?;
    // It opens in front, and the person goes back to their own window to start.
    sleep(Duration::from_secs(1));
    pane::raise(&main)?;
    let mut desk = Desk { person: person.clone(), main, pane: pane.window.clone(), up: false, shown: Vec::new() };
    let mut term = Term::spawn(command(&bin, &launch.args, &p.project, &p.home, &launch.env), &p.run.join("pty.raw"))?;
    let seen = wait_focus(&p.kd, "proj", 0.0)?;
    desk.shown.push((seen.unwrap_or_else(now), Some("proj")));
    let mut term_b = None;
    let raw_b = p.run.join("pty-second.raw");
    let cmd_b = command(&bin, &launch_b.args, &second, &p.home, &launch_b.env);
    let to_second = Second { pane: &mut pane, cmd: cmd_b, raw: &raw_b, term: &mut term_b, model: &model_b, scenario: &b };
    let drove = drive(h, &mut term, &model, &a, to_second, &mut desk, &p.kd);
    sleep(Duration::from_secs(8));
    if drove.is_ok() {
        // Still reading the last reply until kairosd writes down the minute.
        let (from, until) = (now(), Instant::now() + Duration::from_secs(65));
        while Instant::now() < until && !events(&p.kd)?.iter().any(|e| e["kind"] == "present" && e["ts"].as_f64().is_some_and(|t| t > from)) {
            sleep(Duration::from_secs(1));
        }
    }
    person.busy(false);
    let end = now();
    fs::write(p.run.join("screen.txt"), term.screen())?;
    if let Some(tb) = term_b.as_mut() {
        fs::write(p.run.join("screen-second.txt"), tb.screen())?;
        tb.kill();
    }
    drop(pane);
    term.kill();
    if !launch.after.is_empty() {
        let _ = std::process::Command::new(&bin).args(&launch.after).current_dir(&p.project).env("HOME", &p.home).env("USERPROFILE", &p.home).output();
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
    #[cfg(unix)]
    let _ = std::process::Command::new("pkill").arg("-f").arg(&p.run).status();
    let _ = fs::remove_dir_all(&away);
    let requests: String = model.requests().iter().map(|r| format!("{r}\n")).collect();
    fs::write(p.run.join("requests.jsonl"), requests)?;
    crate::keep(&p, h.stores)?;
    let started = drove?;

    let all = events(&p.kd)?;
    let mine: Vec<&Value> = all.iter().filter(|e| e["harness"] == h.id).collect();
    // Sessions apart: the second window's started with its agent.
    let mut first_seen: HashMap<String, f64> = HashMap::new();
    for e in &mine {
        let (s, t) = (e["session"].as_str().unwrap_or_default().to_string(), e["ts"].as_f64().unwrap_or(0.0));
        let at = first_seen.entry(s).or_insert(t);
        *at = at.min(t);
    }
    let (mine_b, mine_a): (Vec<&Value>, Vec<&Value>) =
        mine.iter().partition(|e| first_seen.get(e["session"].as_str().unwrap_or_default()).is_some_and(|&t| t >= started));
    let saves = |second: bool| -> Vec<&Value> { all.iter().filter(|e| e["kind"] == "file_save" && (e["project"] == SECOND) == second).collect() };
    let mut problems = model.problems();
    problems.extend(check_session(h, Some(&version), &a, &[], &mine_a, &saves(false), &p.project, "proj")?);
    let wrong_b = check_session(h, Some(&version), &b, &[], &mine_b, &saves(true), &second, SECOND)?;
    problems.extend(wrong_b.into_iter().map(|w| format!("second window, {w}")));
    let shown = desk.spans(end);
    problems.extend(front(&all, &shown));
    let (wrong, said) = credit::check_windows(&p.kd, &shown, &person)?;
    problems.extend(wrong);
    let mine: Vec<Value> = mine.into_iter().cloned().collect();
    let passed = report(h.id, &version, &a, &kairosd_version(&args.kairosd)?, &mine, &problems);
    println!("  {said}");
    Ok(passed)
}
