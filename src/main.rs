mod app;
mod credit;
mod harness;
mod model;
mod pane;
mod person;
mod scenario;
mod term;
mod two;

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use portable_pty::CommandBuilder;
use serde_json::{json, Value};

use harness::{Harness, Rule};
use model::Model;
use person::Person;
use scenario::Scenario;
use term::{Term, ENTER, ESC};

const USAGE: &str =
    "usage: lab run <harness or app> [version|latest|N.x] --kairosd PATH [--scenario basic|two|tmux|screen] [--work DIR] [--save DIR] [--real-home] [--window]";
/// Set on a developer's machine or in CI, they would point the harness and kairosd away from the run's own home.
const STRAY_ENV: &[&str] = &["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME", "CLAUDE_CONFIG_DIR", "CODEX_HOME"];

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("lab: {e:#}");
            ExitCode::from(2)
        }
    }
}

struct Args {
    harness: String,
    version: String,
    kairosd: PathBuf,
    work: PathBuf,
    save: Option<PathBuf>,
    real_home: bool,
    /// The lab runs in the terminal window in front, so Kairos has to see the agent there.
    window: bool,
    scenario: String,
}

fn args() -> Result<Args> {
    let mut words = Vec::new();
    let (mut kairosd, mut work, mut save, mut real_home, mut window) = (None, PathBuf::from("work"), None, false, false);
    let mut scenario = "basic".to_string();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--kairosd" => kairosd = it.next().map(PathBuf::from),
            "--work" => work = it.next().map(PathBuf::from).context(USAGE)?,
            "--save" => save = it.next().map(PathBuf::from),
            "--real-home" => real_home = true,
            "--window" => window = true,
            "--scenario" => scenario = it.next().context(USAGE)?,
            _ => words.push(a),
        }
    }
    if words.first().map(String::as_str) != Some("run") || words.len() < 2 {
        bail!(USAGE);
    }
    Ok(Args {
        harness: words[1].clone(),
        version: words.get(2).cloned().unwrap_or_else(|| "latest".to_string()),
        kairosd: std::path::absolute(kairosd.context(USAGE)?)?,
        work,
        save,
        real_home,
        window,
        scenario,
    })
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().context("git")?;
    if !out.status.success() {
        bail!("git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Presses keys the way a person does: a second after what asks for them came up, then looks again.
struct Hands {
    last: String,
    since: Instant,
    asked: Option<(usize, Instant)>,
    person: Person,
}

impl Hands {
    fn new(person: &Person) -> Hands {
        Hands { last: String::new(), since: Instant::now(), asked: None, person: person.clone() }
    }

    /// The screen and how long it has looked like this, after pressing the keys of the first rule that fits it. A rule
    /// has to fit for a second first (Claude Code's welcome screen never holds still), and again before a second press.
    fn look(&mut self, term: &mut Term, rules: &[Rule]) -> Result<(String, Duration)> {
        let screen = term.screen();
        if screen != self.last {
            (self.last, self.since) = (screen.clone(), Instant::now());
        }
        let fits = rules.iter().position(|(texts, _)| texts.iter().all(|t| screen.contains(t)));
        match (fits, self.asked) {
            (Some(i), Some((j, at))) if i == j => {
                if at.elapsed() > Duration::from_secs(1) {
                    self.person.press();
                    for k in rules[i].1 {
                        term.send(k)?;
                    }
                    (self.asked, self.since) = (Some((i, Instant::now())), Instant::now());
                }
            }
            (Some(i), _) => self.asked = Some((i, Instant::now())),
            (None, _) => self.asked = None,
        }
        Ok((screen, self.since.elapsed()))
    }
}

fn ready(h: &Harness, term: &mut Term, hands: &mut Hands) -> Result<()> {
    // Codex shows its prompt for a moment before the trust question, so the prompt has to be up and holding still.
    let until = Instant::now() + Duration::from_secs(90);
    loop {
        if term.exited() {
            bail!("{} quit before its prompt came up. Screen:\n{}", h.id, term.screen());
        }
        if Instant::now() > until {
            bail!("{} never showed its prompt ({:?}). Screen:\n{}", h.id, h.ready, term.screen());
        }
        let (screen, still) = hands.look(term, h.first_run)?;
        if h.ready.iter().any(|r| screen.contains(r)) && still > Duration::from_secs(2) {
            return Ok(());
        }
        sleep(Duration::from_millis(200));
    }
}

struct Stage<'a> {
    /// Stops kairosd and starts it again.
    restart: &'a mut dyn FnMut() -> Result<()>,
    /// Starts the harness again on its last session.
    reopen: &'a mut dyn FnMut() -> Result<Term>,
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// When the person did what the events have to show.
struct Done {
    /// When Esc was pressed, for each turn that was interrupted.
    esc: Vec<Option<f64>>,
    /// From closing the harness to its prompt being back, when no agent runs in the terminal.
    closed: Vec<(f64, f64)>,
}

fn drive(h: &Harness, term: &mut Term, model: &Model, scenario: &Scenario, person: &Person, stage: Stage) -> Result<Done> {
    let mut hands = Hands::new(person);
    let mut done = Done { esc: vec![None; scenario.turns.len()], closed: Vec::new() };
    ready(h, term, &mut hands)?;
    for (i, turn) in scenario.turns.iter().enumerate() {
        person.busy(true);
        if turn.restart {
            // Long enough for kairosd to have read and written the turn before.
            sleep(Duration::from_secs(6));
            (stage.restart)()?;
            sleep(Duration::from_secs(4));
        }
        if turn.reopen {
            person.busy(false);
            let closing = now();
            // Ctrl+C twice, the way out every one of them takes. A terminal whose program has gone refuses the second
            // on Mac and Windows.
            for _ in 0..2 {
                if term.exited() {
                    break;
                }
                let _ = term.send("\x03");
                sleep(Duration::from_millis(400));
            }
            let until = Instant::now() + Duration::from_secs(8);
            while !term.exited() && Instant::now() < until {
                sleep(Duration::from_millis(200));
            }
            term.kill();
            *term = (stage.reopen)()?;
            hands = Hands::new(person);
            ready(h, term, &mut hands)?;
            done.closed.push((closing, now()));
            person.busy(true);
        }
        model.arm(i);
        term.type_text(turn.prompt)?;
        if !turn.paste.is_empty() {
            sleep(Duration::from_millis(400));
            term.paste(turn.paste)?;
            sleep(Duration::from_secs(1));
        }
        sleep(Duration::from_millis(400));
        person.busy(false);
        person.press();
        term.send(ENTER)?;
        let until = Instant::now() + Duration::from_secs(90);
        while !(model.said(i) || (turn.interrupt && model.stalled(i))) {
            if term.exited() {
                bail!("{} quit during turn {}", h.id, i + 1);
            }
            if Instant::now() > until {
                bail!("turn {} never finished. Screen:\n{}", i + 1, term.screen());
            }
            hands.look(term, h.during)?;
            sleep(Duration::from_millis(200));
        }
        if turn.interrupt {
            // Watching it work for a bit, then giving up on it.
            sleep(Duration::from_secs(3));
            done.esc[i] = Some(now());
            person.press();
            term.send(ESC)?;
            // OpenCode wants a second press. Claude Code takes one, and a second would open its rewind list.
            let until = Instant::now() + Duration::from_secs(3);
            while Instant::now() < until {
                sleep(Duration::from_millis(300));
                if term.screen().contains("again to interrupt") {
                    person.press();
                    term.send(ESC)?;
                    break;
                }
            }
            // Back at the prompt once the screen holds still.
            let until = Instant::now() + Duration::from_secs(20);
            while hands.look(term, &[])?.1 < Duration::from_secs(2) && Instant::now() < until {
                sleep(Duration::from_millis(200));
            }
        }
        // Reading the reply.
        person.busy(true);
        sleep(Duration::from_secs(4));
    }
    Ok(done)
}

fn events(kd: &Path) -> Result<Vec<Value>> {
    let mut files: Vec<PathBuf> = fs::read_dir(kd)?.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    files.retain(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("events-") && n.ends_with(".jsonl")));
    files.sort();
    let mut out = Vec::new();
    for f in files {
        for line in fs::read_to_string(f)?.lines() {
            let mut e: Value = serde_json::from_str(line).context("an event line that is not JSON")?;
            if let Some(o) = e.as_object_mut() {
                o.remove("prev");
                o.remove("hash");
            }
            out.push(e);
        }
    }
    Ok(out)
}

fn differ(problems: &mut Vec<String>, turn: usize, what: &str, got: &Value, want: Value) {
    if *got != want {
        problems.push(format!("turn {turn}: {what} is {got}, the script says {want}"));
    }
}

/// Compares what Kairos recorded with what the script did, and the script with what is on disk.
/// A person watching the terminal the whole session: from the first prompt to the last reply the window in front is an
/// agent in a terminal, in the project.
/// While the harness is closed the terminal really is a plain terminal, and kairosd looks at the window every few seconds.
fn front(all: &[Value], harness: &str, class: &str, closed: &[(f64, f64)]) -> Vec<String> {
    const LOOKS: f64 = 5.0;
    let of = |kind: &str| -> Vec<f64> { all.iter().filter(|e| e["harness"] == harness && e["kind"] == kind).filter_map(|e| e["ts"].as_f64()).collect() };
    let (Some(&from), Some(&to)) = (of("prompt").first(), of("reply_done").last()) else {
        return Vec::new();
    };
    let want = (json!(class), json!("proj"));
    let mut problems = Vec::new();
    let mut before = None;
    let focus: Vec<(f64, (Value, Value))> =
        all.iter().filter(|e| e["kind"] == "focus").map(|e| (e["ts"].as_f64().unwrap_or(0.0), (e["class"].clone(), e["project"].clone()))).collect();
    // An app's window learns its project from the app's log, which kairosd reads every few seconds: in a new session
    // that is the first prompt.
    let from = if class == "agent_app" { from + LOOKS } else { from };
    for (ts, seen) in &focus {
        let away = closed.iter().any(|&(a, b)| (a..=b + LOOKS).contains(ts));
        if *ts <= from {
            before = Some(seen.clone());
        } else if *ts <= to && *seen != want && !away {
            problems.push(format!("{:.1} s after the first prompt the window in front became {} in project {}", ts - from, seen.0, seen.1));
        }
    }
    for &(_, back) in closed {
        match focus.iter().rev().find(|(ts, _)| *ts <= back + LOOKS) {
            Some((_, seen)) if *seen == want => {}
            seen => problems.push(format!("{LOOKS} s after the harness was opened again the window in front was {seen:?}, not {class} in proj")),
        }
    }
    match before {
        Some(seen) if seen == want => {}
        Some(seen) => problems.push(format!("at the first prompt the window in front was {} in project {}, not {class} in proj", seen.0, seen.1)),
        None => problems.push("Kairos reported no window before the first prompt".to_string()),
    }
    problems
}

/// `version` is the harness's, when the lab knows which one runs (an app brings its own).
fn check(h: &Harness, version: Option<&str>, scenario: &Scenario, esc: &[Option<f64>], all: &[Value], project: &Path) -> Result<Vec<String>> {
    let mine: Vec<&Value> = all.iter().filter(|e| e["harness"] == h.id).collect();
    let saves: Vec<&Value> = all.iter().filter(|e| e["kind"] == "file_save").collect();
    check_session(h, version, scenario, esc, &mine, &saves, project, "proj")
}

/// One session's events (`mine`) against its script, and the file saves in its project, named `name` by Kairos.
#[allow(clippy::too_many_arguments)]
fn check_session(
    h: &Harness,
    version: Option<&str>,
    scenario: &Scenario,
    esc: &[Option<f64>],
    mine: &[&Value],
    saves: &[&Value],
    project: &Path,
    name: &str,
) -> Result<Vec<String>> {
    let marks = version.is_some_and(|v| h.marks(v));
    let mut problems = Vec::new();
    let files = scenario.files();
    for save in saves {
        for path in save["paths"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if !files.contains_key(path) {
                problems.push(format!("file_save names {path}, a file the script never wrote"));
            }
        }
    }
    for (path, content) in files {
        // Some agents on Windows (Copilot CLI) write the lines with Windows line endings.
        if fs::read_to_string(project.join(&path)).unwrap_or_default().replace("\r\n", "\n") != content {
            problems.push(format!("{path} on disk is not what the script wrote, so {} did not follow it", h.id));
        }
    }
    let log = git(project, &["log", "--format=%H"])?;
    if log.lines().count() != 1 + scenario.commits() {
        problems.push(format!("the project has {} commits, the script makes {}", log.lines().count() - 1, scenario.commits()));
    }

    let mut turns: Vec<Vec<&Value>> = Vec::new();
    for e in mine.iter().copied() {
        if e["kind"] == "prompt" {
            turns.push(Vec::new());
        }
        match turns.last_mut() {
            Some(t) => t.push(e),
            None => problems.push(format!("a {} event came before any prompt", e["kind"])),
        }
    }
    if turns.len() != scenario.turns.len() {
        problems.push(format!("Kairos saw {} prompts, the lab typed {}", turns.len(), scenario.turns.len()));
    }
    for (i, (turn, got)) in scenario.turns.iter().zip(&turns).enumerate() {
        let (n, want) = (i + 1, turn.want());
        let of = |kind: &str| got.iter().filter(|e| e["kind"] == kind).copied().collect::<Vec<_>>();
        let prompt = got[0];
        differ(&mut problems, n, "prompt.words", &prompt["words"], json!(want.prompt_words));
        differ(&mut problems, n, "prompt.pasted", &prompt["pasted"], json!(want.pasted));
        // Claude Code keeps what was pasted apart in its prompt history.
        if marks && want.paste_words > 0 {
            differ(&mut problems, n, "prompt.shape.marked", &prompt["shape"]["marked"], json!({ "pastes": 1, "words": want.paste_words }));
        }
        if let Some(v) = version {
            differ(&mut problems, n, "prompt.version", &prompt["version"], json!(v));
        }
        for e in got {
            differ(&mut problems, n, &format!("{}.project", e["kind"].as_str().unwrap_or("?")), &e["project"], json!(name));
        }
        problems.dedup();
        differ(&mut problems, n, "agent_start count", &json!(of("agent_start").len()), json!(want.starts));
        let edits: Vec<Value> = of("agent_edit").iter().map(|e| json!([e["path"], e["added"], e["removed"]])).collect();
        // Pi logs a whole-file write without its lines, so Kairos can't tell a new file from one written over.
        let want_edits: Vec<_> = want.edits.iter().filter(|e| h.counts_writes || !want.writes.contains(e)).collect();
        differ(&mut problems, n, "agent edits [path, added, removed]", &json!(edits), json!(want_edits));
        let commits = of("agent_commit");
        differ(&mut problems, n, "agent_commit count", &json!(commits.len()), json!(want.commits));
        for c in commits {
            match c["sha"].as_str() {
                Some(sha) if log.lines().any(|l| l.starts_with(sha)) => {}
                sha => problems.push(format!("turn {n}: agent_commit sha {sha:?} is not a commit in the project")),
            }
        }
        let asks = of("answered");
        differ(&mut problems, n, "answered count", &json!(asks.len()), json!(if marks { want.asks } else { 0 }));
        for a in asks {
            differ(&mut problems, n, "answered.why", &a["why"], json!("permission"));
            let (asked, at) = (a["asked"].as_f64().unwrap_or(0.0), a["ts"].as_f64().unwrap_or(0.0));
            // The lab answers a second after the prompt comes up.
            if !(0.5..20.0).contains(&(at - asked)) {
                problems.push(format!("turn {n}: the answer came {:.1} s after the ask, the lab answered within a few seconds", at - asked));
            }
        }
        let done = of("reply_done");
        differ(&mut problems, n, "reply_done count", &json!(done.len()), json!(1));
        if let Some(d) = done.first() {
            differ(&mut problems, n, "reply_done.reply_words", &d["reply_words"], json!(want.reply_words));
            let reads = if h.reads_first { want.edits.len() - want.writes.len() } else { 0 };
            differ(&mut problems, n, "reply_done.tools", &d["tools"], json!(want.tools + reads));
            differ(&mut problems, n, "reply_done.files_changed", &d["files_changed"], json!(want.files));
            if let Some(pressed) = esc.get(i).copied().flatten() {
                let off = d["ts"].as_f64().unwrap_or(0.0) - pressed;
                // Otherwise the turn ends at its last step, which came 3 s before Esc, in whole seconds.
                let fits = if h.writes_esc { (-1.5..3.0).contains(&off) } else { (-5.0..0.5).contains(&off) };
                if !fits {
                    problems.push(format!("turn {n}: the interrupted reply ended {off:.1} s from when Esc was pressed"));
                }
            }
            if got.last().map(|e| &e["kind"]) != Some(&json!("reply_done")) {
                problems.push(format!("turn {n}: something came after reply_done"));
            }
        }
    }
    Ok(problems)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for e in fs::read_dir(from)? {
        let e = e?;
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &to.join(e.file_name()))?;
        } else {
            fs::copy(e.path(), to.join(e.file_name()))?;
        }
    }
    Ok(())
}

/// One run's folders: the home the harness and kairosd see, the project and kairosd's own folder.
struct Place {
    run: PathBuf,
    home: PathBuf,
    project: PathBuf,
    kd: PathBuf,
}

fn place(args: &Args, name: &str, version: &str, state: &[&str]) -> Result<Place> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let run = std::path::absolute(&args.work)?.join(format!("{name}-{version}-{stamp}"));
    // kairosd on Windows asks the system for the home folder, no variable moves it, so there the run uses the real one.
    let home = if args.real_home {
        if std::env::var_os("CI").is_none() {
            bail!("--real-home rewrites harness settings and sessions in this account's home, so it only runs in CI");
        }
        let home = PathBuf::from(std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).context("no home folder")?);
        // Every run starts as a first install, with nothing an earlier version left behind.
        for kept in state {
            let old = home.join(kept);
            if old.is_dir() {
                fs::remove_dir_all(&old)?;
            } else if old.is_file() {
                fs::remove_file(&old)?;
            }
        }
        home
    } else {
        run.join("home")
    };
    let (project, kd) = (run.join("proj"), run.join("kd"));
    for d in [&home, &kd] {
        fs::create_dir_all(d)?;
    }
    repo(&project)?;
    Ok(Place { run, home, project, kd })
}

/// A git repo with one commit, for the agent to work in.
fn repo(project: &Path) -> Result<()> {
    fs::create_dir_all(project)?;
    git(project, &["init", "-q", "-b", "main"])?;
    git(project, &["config", "user.name", "lab"])?;
    git(project, &["config", "user.email", "lab@example.invalid"])?;
    fs::write(project.join("README.md"), "# demo\n")?;
    git(project, &["add", "-A"])?;
    git(project, &["commit", "-q", "-m", "init"])?;
    Ok(())
}

fn daemon(args: &Args, p: &Place) -> Result<Child> {
    let log = |name: &str| OpenOptions::new().create(true).append(true).open(p.run.join(name));
    let mut daemon = Command::new(&args.kairosd);
    daemon.env("HOME", &p.home).env("USERPROFILE", &p.home).env("KAIROS_DIR", &p.kd);
    daemon.stdin(Stdio::null()).stdout(log("kairosd.out")?).stderr(log("kairosd.err")?);
    for k in STRAY_ENV {
        daemon.env_remove(k);
    }
    let mut child = daemon.spawn().context("starting kairosd")?;
    sleep(Duration::from_secs(1));
    if let Ok(Some(status)) = child.try_wait() {
        let err = fs::read_to_string(p.run.join("kairosd.err")).unwrap_or_default();
        bail!("kairosd quit right away ({status}): {}", err.lines().last().unwrap_or_default());
    }
    Ok(child)
}

/// Copies what the harness wrote next to the run, so a wrong event can be traced to the line it came from.
fn keep(p: &Place, stores: &[&str]) -> Result<()> {
    for store in stores {
        let (from, to) = (p.home.join(store), p.run.join("store").join(store));
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else if from.is_file() {
            fs::create_dir_all(to.parent().context("a store path with no folder")?)?;
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// A bug the lab caught, listed without failing the run so the nightly stays quiet about it.
struct Known {
    /// Its number in BUGS.md.
    bug: u32,
    /// Empty for every harness, or one harness, or one harness at one version ("copilot-cli 1.0.108-0").
    harness: &'static str,
    /// The first release with the fix, empty while it is open.
    fixed: &'static str,
    /// Text in the problem.
    text: &'static str,
}

const KNOWN: &[Known] = &[
    Known { bug: 23, harness: "", fixed: "0.1.20", text: "\"inner\"" },
    Known { bug: 29, harness: "antigravity", fixed: "0.1.20", text: "credit came while nobody had touched anything" },
    Known { bug: 24, harness: "opencode-desktop", fixed: "0.1.20", text: "in project null" },
    Known { bug: 24, harness: "opencode-desktop", fixed: "0.1.20", text: "s the person spent reading and typing" },
    Known { bug: 32, harness: "t3code-opencode", fixed: "0.1.21", text: "Kairos saw 6 prompts, the lab typed 5" },
    Known { bug: 34, harness: "antigravity-ide", fixed: "0.1.21", text: "\"testing\" in project" },
    Known { bug: 34, harness: "antigravity-ide", fixed: "0.1.21", text: "credit came while nobody had touched anything" },
    Known { bug: 45, harness: "copilot-cli", fixed: "0.1.24", text: "is 0, the script says" },
    Known { bug: 45, harness: "copilot-cli", fixed: "0.1.24", text: "something came after reply_done" },
    Known { bug: 50, harness: "vscode", fixed: "", text: "Kairos saw 0 prompts, the lab typed 5" },
    Known { bug: 51, harness: "copilot-cli 1.0.108-0", fixed: "", text: "is 0, the script says" },
    Known { bug: 51, harness: "copilot-cli 1.0.108-0", fixed: "", text: "something came after reply_done" },
    Known { bug: 51, harness: "copilot-cli 1.0.108-0", fixed: "", text: "the interrupted reply ended" },
    Known { bug: 51, harness: "copilot-cli 1.0.108-0", fixed: "", text: "credit came while nobody had touched anything" },
    Known { bug: 52, harness: "codex-app", fixed: "", text: "came with app null in front" },
    Known { bug: 53, harness: "qwen-code", fixed: "", text: "agent_commit count is 0" },
    Known { bug: 53, harness: "qwen-code", fixed: "", text: "reply_done.files_changed is" },
    Known { bug: 53, harness: "qwen-code", fixed: "", text: "prompt.words is" },
    Known { bug: 54, harness: "qwen-code", fixed: "", text: "window in front" },
    Known { bug: 54, harness: "qwen-code", fixed: "", text: "credit came while nobody had touched anything" },
    Known { bug: 54, harness: "qwen-code", fixed: "", text: "of credit was testing in project" },
    Known { bug: 55, harness: "gemini-cli", fixed: "", text: "Kairos saw" },
    Known { bug: 55, harness: "gemini-cli", fixed: "", text: "turn " },
];

fn kairosd_version(kairosd: &Path) -> Result<String> {
    let out = Command::new(kairosd).arg("--version").stdin(Stdio::null()).output().context("kairosd --version")?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().nth(1).map(str::to_string).context("kairosd --version gave no version")
}

fn older(a: &str, b: &str) -> bool {
    let parts = |v: &str| v.split('.').map(|p| p.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    parts(a) < parts(b)
}

/// True when nothing but known bugs went wrong.
fn report(name: &str, version: &str, scenario: &Scenario, kairosd: &str, mine: &[Value], problems: &[String]) -> bool {
    for e in mine {
        println!("{e}");
    }
    let known = |p: &String| {
        let at = format!("{name} {version}");
        KNOWN.iter().find(|k| (k.harness.is_empty() || k.harness == name || k.harness == at) && (k.fixed.is_empty() || older(kairosd, k.fixed)) && p.contains(k.text))
    };
    // A known extra or missing prompt moves every turn after it, so those turns can't be checked.
    let shifted = problems.iter().any(|p| p.starts_with("Kairos saw") && known(p).is_some());
    let real: Vec<&String> = problems.iter().filter(|p| !shifted && known(p).is_none()).collect();
    if real.is_empty() {
        println!("PASS {name} {version} {}: {} events match the script", scenario.name, mine.len());
    } else {
        println!("FAIL {name} {version} {}:", scenario.name);
        for p in &real {
            println!("  {p}");
        }
    }
    for p in problems {
        if shifted && known(p).is_none() {
            println!("  after a known extra or missing prompt: {p}");
            continue;
        }
        match known(p) {
            Some(k) if k.fixed.is_empty() => println!("  known open bug #{}: {p}", k.bug),
            Some(k) => println!("  known bug #{} in kairosd {kairosd}, fixed in {}: {p}", k.bug, k.fixed),
            None => {}
        }
    }
    real.is_empty()
}

/// Only the scripted model, for driving a harness by hand: each turn is armed once the one before has its answer.
fn serve(project: &str) -> Result<bool> {
    let scenario = Arc::new(scenario::basic());
    let model = Model::start(scenario.clone(), std::path::absolute(project)?)?;
    println!("{}", model.port);
    for i in 0..scenario.turns.len() {
        model.arm(i);
        while !model.said(i) && !model.stalled(i) {
            sleep(Duration::from_millis(200));
        }
        eprintln!("lab: turn {} answered", i + 1);
    }
    Ok(true)
}

/// The harness started the way a person starts it in a terminal.
fn command(bin: &Path, args: &[String], project: &Path, home: &Path, env: &[(String, String)]) -> CommandBuilder {
    // npm's launchers on Windows are batch files, which only cmd can start.
    let mut cmd = if bin.extension().is_some_and(|e| e == "cmd") {
        let mut cmd = CommandBuilder::new("cmd.exe");
        cmd.arg("/c");
        cmd.arg(bin);
        cmd
    } else {
        CommandBuilder::new(bin)
    };
    cmd.args(args);
    cmd.cwd(project);
    cmd.env("HOME", home);
    cmd.env("USERPROFILE", home);
    cmd.env("TERM", "xterm-256color");
    for k in STRAY_ENV {
        cmd.env_remove(k);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd
}

/// A person starts on the agent once it's up in their terminal. Kairos sees the new program a moment later (up to 15 s
/// when the window stays in front), and keys pressed before that count for whatever ran there first, the lab itself
/// here. Gives when Kairos saw it, or nothing after 30 s: the checks say what Kairos saw.
fn wait_focus(kd: &Path, class: &str, project: &str, since: f64) -> Result<Option<f64>> {
    let until = Instant::now() + Duration::from_secs(30);
    while Instant::now() < until {
        let seen = events(kd)?.iter().find_map(|e| {
            let ts = e["ts"].as_f64().filter(|&t| t >= since)?;
            (e["kind"] == "focus" && e["class"] == class && e["project"] == project).then_some(ts)
        });
        if seen.is_some() {
            return Ok(seen);
        }
        sleep(Duration::from_millis(300));
    }
    Ok(None)
}

fn run() -> Result<bool> {
    if std::env::args().nth(1).as_deref() == Some("pane") {
        let mut words = std::env::args().skip(2);
        let (Some(port), Some(away)) = (words.next(), words.next()) else { bail!("usage: lab pane <port> <folder> [editor]") };
        return pane::serve(&port, &away, words.next().as_deref() == Some("editor"));
    }
    if std::env::args().nth(1).as_deref() == Some("model") {
        return serve(&std::env::args().nth(2).context("usage: lab model <project folder>")?);
    }
    if std::env::args().nth(1).as_deref() == Some("credit") {
        credit::show(Path::new(&std::env::args().nth(2).context("usage: lab credit <kairosd folder>")?))?;
        return Ok(true);
    }
    let args = args()?;
    fs::create_dir_all(&args.work)?;
    // A run takes a few minutes. One that hangs fails by itself instead of taking the machine's whole report with it.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(15 * 60));
        eprintln!("lab: still running after 15 minutes, gave up");
        std::process::exit(2);
    });
    if let Some(app) = app::find(&args.harness) {
        return app::run(app, &args);
    }
    let h = harness::find(&args.harness)?;
    if args.scenario == "two" {
        return two::run(h, &args);
    }
    // Runs the agent in a pane of tmux or screen, whose server runs it away from the window.
    let mux = ["tmux", "screen"].into_iter().find(|m| args.scenario == *m);
    // Runs it in VS Code's own terminal, in VS Code's window.
    let in_editor = args.scenario == "vscode";
    if args.scenario != "basic" && mux.is_none() && !in_editor {
        bail!("no scenario {:?}, the lab knows basic, two, tmux, screen and vscode", args.scenario);
    }
    if in_editor && !args.window {
        bail!("the vscode scenario runs the agent in VS Code's window, so it needs --window");
    }
    if let Some(m) = mux {
        if cfg!(windows) || Command::new("sh").args(["-c", &format!("command -v {m}")]).output().map_or(true, |o| !o.status.success()) {
            println!("SKIP {} {m}: no {m} here", h.id);
            return Ok(true);
        }
    }
    let root = std::path::absolute(&args.work)?;
    let (bin, version) = h.install(&args.version, &root.join("tools"))?;
    let p = place(&args, h.id, &version, h.state)?;
    let Place { run, home, project, kd } = &p;

    let mut scenario = scenario::basic();
    if let Some(m) = mux {
        scenario.name = m;
    }
    if in_editor {
        scenario.name = "vscode";
    }
    let scenario = Arc::new(scenario);
    let model = Model::start(scenario.clone(), project.clone())?.reads_first(h.reads_first);
    let url = format!("http://127.0.0.1:{}", model.port);
    eprintln!("lab: {} {version}, scenario {}, run folder {}", h.id, scenario.name, run.display());

    let code = if in_editor { Some(app::Editor::install(&root.join("tools"))?) } else { None };
    let person = Person::new(args.window)?;
    let start = || daemon(&args, &p);
    let mut daemon = start()?;
    sleep(Duration::from_secs(2));

    let launch = (h.prepare)(home, project, &url, &version)?;
    let panes = pane::Panes::listen()?;
    let mut editor = match &code {
        Some(exe) => Some(app::Editor::open(exe, &args, &p, &url, panes.port)?),
        None => None,
    };
    // The run's own server.
    let sock = format!("lab-{}", std::process::id());
    let conf = run.join("mux.conf");
    match mux {
        Some("tmux") => fs::write(&conf, "set -g status off\nset -sg escape-time 0\n")?,
        Some(_) => fs::write(&conf, "startup_message off\nmaptimeout 0\n")?,
        None => {}
    }
    let mut open = |args: &[String], raw: &str| -> Result<Term> {
        if let Some(editor) = editor.as_mut() {
            // A new terminal in the editor, and the agent typed into it.
            let mut pane = editor.terminal(&panes)?;
            return pane.start(&command(&bin, args, project, home, &launch.env), &run.join(raw));
        }
        let Some(m) = mux else {
            return Term::spawn(command(&bin, args, project, home, &launch.env), &run.join(raw));
        };
        let mut all: Vec<String> = if m == "tmux" {
            vec!["-L".into(), sock.clone(), "-f".into(), conf.to_string_lossy().into(), "new-session".into(), "--".into()]
        } else {
            vec!["-S".into(), sock.clone(), "-c".into(), conf.to_string_lossy().into()]
        };
        all.push(bin.to_string_lossy().into());
        all.extend(args.iter().cloned());
        let mut cmd = command(Path::new(m), &all, project, home, &launch.env);
        cmd.env_remove("TMUX");
        cmd.env_remove("STY");
        Term::spawn(cmd, &run.join(raw))
    };
    let window = if in_editor { "editor" } else { "agent_terminal" };
    let mut term = open(&launch.args, "pty.raw")?;
    if args.window {
        wait_focus(kd, window, "proj", 0.0)?;
    }
    let mut reopen = || open(&launch.resume, "pty2.raw");
    let mut restart = || -> Result<()> {
        let _ = daemon.kill();
        let _ = daemon.wait();
        daemon = start()?;
        Ok(())
    };
    let drove = drive(h, &mut term, &model, &scenario, &person, Stage { restart: &mut restart, reopen: &mut reopen });
    // Readers look every 3 s and saves are batched for 2 s.
    sleep(Duration::from_secs(8));
    if drove.is_ok() && args.window {
        // Still reading the last reply until kairosd writes down the minute, which it does once a minute.
        let (from, until) = (now(), Instant::now() + Duration::from_secs(65));
        while Instant::now() < until && !events(kd)?.iter().any(|e| e["kind"] == "present" && e["ts"].as_f64().is_some_and(|t| t > from)) {
            sleep(Duration::from_secs(1));
        }
    }
    person.busy(false);
    fs::write(run.join("screen.txt"), term.screen())?;
    term.kill();
    if drove.is_err() {
        if let Some(editor) = editor.as_mut() {
            let _ = fs::write(run.join("page.txt"), editor.text());
        }
    }
    if let Some(m) = mux {
        let stop: &[&str] = if m == "tmux" { &["-L", &sock, "kill-server"] } else { &["-S", &sock, "-X", "quit"] };
        let _ = Command::new(m).args(stop).status();
    }
    if !launch.after.is_empty() {
        let mut stop = Command::new(&bin);
        stop.args(&launch.after).current_dir(project).env("HOME", home).env("USERPROFILE", home).stdin(Stdio::null());
        for k in STRAY_ENV {
            stop.env_remove(k);
        }
        let _ = stop.output();
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
    // Once kairosd is gone: with VS Code closed the lab's own window is in front, in the lab's checkout.
    drop(editor.take());
    // Codex leaves a server running from the run's own home.
    #[cfg(unix)]
    let _ = Command::new("pkill").arg("-f").arg(run).status();
    let requests: String = model.requests().iter().map(|r| format!("{r}\n")).collect();
    fs::write(run.join("requests.jsonl"), requests)?;
    keep(&p, h.stores)?;
    let done = drove?;

    let all = events(kd)?;
    let mine: Vec<Value> = all.iter().filter(|e| e["harness"] == h.id).cloned().collect();
    let mut problems = model.problems();
    problems.extend(check(h, h.writes_version.then_some(version.as_str()), &scenario, &done.esc, &all, project)?);
    let mut summary = None;
    if args.window {
        problems.extend(front(&all, h.id, window, &done.closed));
        let (wrong, said) = credit::check(kd, h.id, "proj", &person, &done.closed)?;
        problems.extend(wrong);
        summary = Some(said);
    }
    let passed = report(h.id, &version, &scenario, &kairosd_version(&args.kairosd)?, &mine, &problems);
    if let Some(said) = summary {
        println!("  {said}");
    }
    if let (Some(save), true) = (&args.save, problems.is_empty()) {
        let dir = save.join(h.id).join(&version).join(scenario.name);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;
        // OpenCode's database is 14 MB of mostly cache, so the store is kept packed, byte for byte as the harness left it.
        let packed = Command::new("tar").arg("-czf").arg(dir.join("store.tar.gz")).arg("-C").arg(run).arg("store").status()?;
        if !packed.success() {
            bail!("tar could not pack the sample's store");
        }
        let lines: String = mine.iter().map(|e| format!("{e}\n")).collect();
        fs::write(dir.join("events.jsonl"), lines)?;
        let about = json!({ "harness": h.id, "version": version, "scenario": scenario.name, "os": std::env::consts::OS, "home": home, "project": project });
        fs::write(dir.join("run.json"), serde_json::to_string_pretty(&about)?)?;
        println!("saved the sample to {}", dir.display());
    }
    Ok(passed)
}
