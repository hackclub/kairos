mod harness;
mod model;
mod scenario;
mod term;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use portable_pty::CommandBuilder;
use serde_json::{json, Value};

use harness::{Harness, Rule};
use model::Model;
use scenario::Scenario;
use term::{Term, ENTER};

const USAGE: &str = "usage: lab run <harness> [version|latest|N.x] --kairosd PATH [--work DIR] [--save DIR] [--real-home] [--window]";
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
}

fn args() -> Result<Args> {
    let mut words = Vec::new();
    let (mut kairosd, mut work, mut save, mut real_home, mut window) = (None, PathBuf::from("work"), None, false, false);
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--kairosd" => kairosd = it.next().map(PathBuf::from),
            "--work" => work = it.next().map(PathBuf::from).context(USAGE)?,
            "--save" => save = it.next().map(PathBuf::from),
            "--real-home" => real_home = true,
            "--window" => window = true,
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
    })
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().context("git")?;
    if !out.status.success() {
        bail!("git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Presses keys the way a person does: only at a screen that has held still for a second, then looks again.
struct Hands {
    last: String,
    since: Instant,
}

impl Hands {
    fn new() -> Hands {
        Hands { last: String::new(), since: Instant::now() }
    }

    /// The screen and how long it has looked like this, after pressing the keys of the first rule that fits it.
    fn look(&mut self, term: &mut Term, rules: &[Rule]) -> Result<(String, Duration)> {
        let screen = term.screen();
        if screen != self.last {
            (self.last, self.since) = (screen.clone(), Instant::now());
        } else if self.since.elapsed() > Duration::from_secs(1) {
            if let Some((_, keys)) = rules.iter().find(|(texts, _)| texts.iter().all(|t| screen.contains(t))) {
                for k in *keys {
                    term.send(k)?;
                }
                self.since = Instant::now();
            }
        }
        Ok((screen, self.since.elapsed()))
    }
}

fn drive(h: &Harness, term: &mut Term, model: &Model, scenario: &Scenario) -> Result<()> {
    // Codex shows its prompt for a moment before the trust question, so the prompt has to be up and holding still.
    let until = Instant::now() + Duration::from_secs(90);
    let mut hands = Hands::new();
    loop {
        if term.exited() {
            bail!("{} quit before its prompt came up. Screen:\n{}", h.id, term.screen());
        }
        if Instant::now() > until {
            bail!("{} never showed its prompt ({:?}). Screen:\n{}", h.id, h.ready, term.screen());
        }
        let (screen, still) = hands.look(term, h.first_run)?;
        if h.ready.iter().any(|r| screen.contains(r)) && still > Duration::from_secs(2) {
            break;
        }
        sleep(Duration::from_millis(200));
    }
    for (i, turn) in scenario.turns.iter().enumerate() {
        model.arm(i);
        term.type_text(turn.prompt)?;
        sleep(Duration::from_millis(400));
        term.send(ENTER)?;
        let until = Instant::now() + Duration::from_secs(90);
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
        sleep(Duration::from_secs(4));
    }
    Ok(())
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
fn front(all: &[Value], harness: &str) -> Vec<String> {
    let of = |kind: &str| -> Vec<f64> { all.iter().filter(|e| e["harness"] == harness && e["kind"] == kind).filter_map(|e| e["ts"].as_f64()).collect() };
    let (Some(&from), Some(&to)) = (of("prompt").first(), of("reply_done").last()) else {
        return Vec::new();
    };
    let want = (json!("agent_terminal"), json!("proj"));
    let mut problems = Vec::new();
    let mut before = None;
    for e in all.iter().filter(|e| e["kind"] == "focus") {
        let (ts, seen) = (e["ts"].as_f64().unwrap_or(0.0), (e["class"].clone(), e["project"].clone()));
        if ts <= from {
            before = Some(seen);
        } else if ts <= to && seen != want {
            problems.push(format!("{:.1} s after the first prompt the window in front became {} in project {}", ts - from, seen.0, seen.1));
        }
    }
    match before {
        Some(seen) if seen == want => {}
        Some(seen) => problems.push(format!("at the first prompt the window in front was {} in project {}, not agent_terminal in proj", seen.0, seen.1)),
        None => problems.push("Kairos reported no window before the first prompt".to_string()),
    }
    problems
}

fn check(h: &Harness, version: &str, scenario: &Scenario, all: &[Value], project: &Path) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let mine: Vec<&Value> = all.iter().filter(|e| e["harness"] == h.id).collect();
    let files = scenario.files();
    for save in all.iter().filter(|e| e["kind"] == "file_save") {
        for path in save["paths"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if !files.contains_key(path) {
                problems.push(format!("file_save names {path}, a file the script never wrote"));
            }
        }
    }
    for (path, content) in files {
        if fs::read_to_string(project.join(&path)).unwrap_or_default() != content {
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
        differ(&mut problems, n, "prompt.pasted", &prompt["pasted"], json!(false));
        differ(&mut problems, n, "prompt.version", &prompt["version"], json!(version));
        differ(&mut problems, n, "the project", &prompt["project"], json!("proj"));
        differ(&mut problems, n, "agent_start count", &json!(of("agent_start").len()), json!(1));
        let edits: Vec<Value> = of("agent_edit").iter().map(|e| json!([e["path"], e["added"], e["removed"]])).collect();
        differ(&mut problems, n, "agent edits [path, added, removed]", &json!(edits), json!(want.edits));
        let commits = of("agent_commit");
        differ(&mut problems, n, "agent_commit count", &json!(commits.len()), json!(want.commits));
        for c in commits {
            match c["sha"].as_str() {
                Some(sha) if log.lines().any(|l| l.starts_with(sha)) => {}
                sha => problems.push(format!("turn {n}: agent_commit sha {sha:?} is not a commit in the project")),
            }
        }
        let done = of("reply_done");
        differ(&mut problems, n, "reply_done count", &json!(done.len()), json!(1));
        if let Some(d) = done.first() {
            differ(&mut problems, n, "reply_done.reply_words", &d["reply_words"], json!(want.reply_words));
            differ(&mut problems, n, "reply_done.tools", &d["tools"], json!(want.tools));
            differ(&mut problems, n, "reply_done.files_changed", &d["files_changed"], json!(want.files));
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

fn run() -> Result<bool> {
    let args = args()?;
    let h = harness::find(&args.harness)?;
    fs::create_dir_all(&args.work)?;
    let root = std::path::absolute(&args.work)?;
    let (bin, version) = h.install(&args.version, &root.join("tools"))?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let run = root.join(format!("{}-{version}-{stamp}", h.id));
    // kairosd on Windows asks the system for the home folder, no variable moves it, so there the run uses the real one.
    let home = if args.real_home {
        if std::env::var_os("CI").is_none() {
            bail!("--real-home rewrites harness settings and sessions in this account's home, so it only runs in CI");
        }
        let home = PathBuf::from(std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).context("no home folder")?);
        // Every run starts as a first install, with nothing an earlier version left behind.
        for kept in h.state {
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
    for d in [&home, &project, &kd] {
        fs::create_dir_all(d)?;
    }
    git(&project, &["init", "-q", "-b", "main"])?;
    git(&project, &["config", "user.name", "lab"])?;
    git(&project, &["config", "user.email", "lab@example.invalid"])?;
    fs::write(project.join("README.md"), "# demo\n")?;
    git(&project, &["add", "-A"])?;
    git(&project, &["commit", "-q", "-m", "init"])?;

    let scenario = Arc::new(scenario::basic());
    let model = Model::start(scenario.clone(), project.clone())?;
    let url = format!("http://127.0.0.1:{}", model.port);
    eprintln!("lab: {} {version}, scenario {}, run folder {}", h.id, scenario.name, run.display());

    let mut daemon = Command::new(&args.kairosd);
    daemon.env("HOME", &home).env("USERPROFILE", &home).env("KAIROS_DIR", &kd);
    daemon.stdin(Stdio::null()).stdout(File::create(run.join("kairosd.out"))?).stderr(File::create(run.join("kairosd.err"))?);
    for k in STRAY_ENV {
        daemon.env_remove(k);
    }
    let mut daemon = daemon.spawn().context("starting kairosd")?;
    sleep(Duration::from_secs(2));

    let launch = (h.prepare)(&home, &project, &url, &version)?;
    // npm's launchers on Windows are batch files, which only cmd can start.
    let mut cmd = if bin.extension().is_some_and(|e| e == "cmd") {
        let mut cmd = CommandBuilder::new("cmd.exe");
        cmd.arg("/c");
        cmd.arg(&bin);
        cmd
    } else {
        CommandBuilder::new(&bin)
    };
    cmd.args(&launch.args);
    cmd.cwd(&project);
    cmd.env("HOME", &home);
    cmd.env("USERPROFILE", &home);
    cmd.env("TERM", "xterm-256color");
    for k in STRAY_ENV {
        cmd.env_remove(k);
    }
    for (k, v) in &launch.env {
        cmd.env(k, v);
    }
    let mut term = Term::spawn(cmd, &run.join("pty.raw"))?;
    let drove = drive(h, &mut term, &model, &scenario);
    // Readers look every 3 s and saves are batched for 2 s.
    sleep(Duration::from_secs(8));
    fs::write(run.join("screen.txt"), term.screen())?;
    term.kill();
    if !launch.after.is_empty() {
        let mut stop = Command::new(&bin);
        stop.args(&launch.after).current_dir(&project).env("HOME", &home).env("USERPROFILE", &home).stdin(Stdio::null());
        for k in STRAY_ENV {
            stop.env_remove(k);
        }
        let _ = stop.output();
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
    // Codex leaves a server running from the run's own home.
    #[cfg(unix)]
    let _ = Command::new("pkill").arg("-f").arg(&run).status();
    let requests: String = model.requests().iter().map(|r| format!("{r}\n")).collect();
    fs::write(run.join("requests.jsonl"), requests)?;
    // What Kairos read, kept next to the run so a wrong event can be traced to the line it came from.
    for store in h.stores {
        let (from, to) = (home.join(store), run.join("store").join(store));
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else if from.is_file() {
            fs::create_dir_all(to.parent().context("a store path with no folder")?)?;
            fs::copy(&from, &to)?;
        }
    }
    drove?;

    let all = events(&kd)?;
    let mine: Vec<Value> = all.iter().filter(|e| e["harness"] == h.id).cloned().collect();
    let mut problems = model.problems();
    problems.extend(check(h, &version, &scenario, &all, &project)?);
    if args.window {
        problems.extend(front(&all, h.id));
    }
    for e in &mine {
        println!("{e}");
    }
    if problems.is_empty() {
        println!("PASS {} {version} {}: {} events match the script", h.id, scenario.name, mine.len());
    } else {
        println!("FAIL {} {version} {}:", h.id, scenario.name);
        for p in &problems {
            println!("  {p}");
        }
    }
    if let (Some(save), true) = (&args.save, problems.is_empty()) {
        let dir = save.join(h.id).join(&version).join(scenario.name);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;
        // OpenCode's database is 14 MB of mostly cache, so the store is kept packed, byte for byte as the harness left it.
        let packed = Command::new("tar").arg("-czf").arg(dir.join("store.tar.gz")).arg("-C").arg(&run).arg("store").status()?;
        if !packed.success() {
            bail!("tar could not pack the sample's store");
        }
        let lines: String = mine.iter().map(|e| format!("{e}\n")).collect();
        fs::write(dir.join("events.jsonl"), lines)?;
        let about = json!({ "harness": h.id, "version": version, "scenario": scenario.name, "os": std::env::consts::OS, "home": home, "project": project });
        fs::write(dir.join("run.json"), serde_json::to_string_pretty(&about)?)?;
        println!("saved the sample to {}", dir.display());
    }
    Ok(problems.is_empty())
}
