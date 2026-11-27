mod account;
mod daemon;
mod doctor;
mod editors;
mod exclude;
mod hackatime;
#[cfg(unix)]
mod hook;
mod install;
mod pwsh;
mod report;
mod update;
mod wakatime;

use std::path::PathBuf;
use std::process::ExitCode;

use kairos_core::{analyze, find_session_files, parse_file, Options, Report};
use anyhow::{bail, Context, Result};
use chrono::Local;

const USAGE: &str = "usage: kairos analyze [--days N] [--json] [--think-secs S] [DIR...]\n       kairos report [--date YYYY-MM-DD] [--rule v1|v3|v4|v5|v6|v7|v8|v9] [--json [--spans [--tune KEY=VALUE]...]]\n       kairos verify [--date YYYY-MM-DD]\n       kairos install [--no-login] [--keep-wakatime] [--server URL]\n       kairos uninstall\n       kairos wakatime [on|off]\n       kairos hackatime\n       kairos exclude [PATH | --list | --remove PATH]\n       kairos login [--server URL]\n       kairos logout\n       kairos status\n       kairos update [--check]\n       kairos doctor\n       kairos hook\n       kairos hooks print";

struct Args {
    opts: Options,
    json: bool,
    dirs: Vec<PathBuf>,
}

fn parse_args(args: Vec<String>) -> Result<Args> {
    let mut it = args.into_iter();
    match it.next().as_deref() {
        Some("analyze") => {}
        Some("-h" | "--help") | None => bail!("{USAGE}"),
        Some(other) => bail!("unknown command {other:?}\n{USAGE}"),
    }
    let mut opts = Options::default();
    let mut json = false;
    let mut dirs = Vec::new();
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (a.clone(), None),
        };
        let mut value = |name: &str| -> Result<String> {
            inline
                .clone()
                .or_else(|| it.next())
                .with_context(|| format!("{name} needs a value"))
        };
        match flag.as_str() {
            "--json" => json = true,
            "--days" => opts.days = value("--days")?.parse().context("--days must be a whole number")?,
            "--think-secs" => {
                opts.think_secs = value("--think-secs")?.parse().context("--think-secs must be a number")?
            }
            "-h" | "--help" => bail!("{USAGE}"),
            f if f.starts_with("--") => bail!("unknown option {f:?}\n{USAGE}"),
            _ => dirs.push(PathBuf::from(a)),
        }
    }
    if dirs.is_empty() {
        if let Some(home) = dirs::home_dir() {
            dirs.extend(
                [".claude/projects", ".claude-work/projects"]
                    .iter()
                    .map(|d| home.join(d))
                    .filter(|d| d.is_dir()),
            );
        }
    }
    Ok(Args { opts, json, dirs })
}

fn print_text(r: &Report, days: usize) {
    let t = &r.totals;
    println!("Human active time  {:>8.1} h", t.human_hours);
    println!("Agent runtime      {:>8.1} h", t.agent_hours);
    println!("Agent:human ratio  {:>8.1}x", t.agent_to_human_ratio);
    println!("Prompts            {:>8}", t.prompts);
    println!("Sessions           {:>8}", t.sessions);
    println!("Interrupts         {:>8}", t.interrupts);
    println!("\nLast {days} days");
    println!("{:<12} {:>8} {:>8} {:>8}", "date", "human h", "agent h", "prompts");
    for d in &r.days {
        println!("{:<12} {:>8.2} {:>8.2} {:>8}", d.date, d.human_hours, d.agent_hours, d.prompts);
    }
    println!("\nTop projects by human hours");
    for p in &r.projects {
        println!("{:>8.2}  {}", p.human_hours, p.project);
    }
    println!("\nModels (turns)");
    for m in &r.models {
        println!("{:>8}  {}", m.turns, m.model);
    }
}

fn run() -> Result<ExitCode> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    match raw.first().map(String::as_str) {
        Some("amp-event") => {
            let mut message = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::Read::take(std::io::stdin(), 16 * 1024 * 1024), &mut message);
            kairos_core::sources::amp::record(&message);
            return Ok(ExitCode::SUCCESS);
        }
        #[cfg(unix)]
        Some("hook") => {
            hook::run();
            return Ok(ExitCode::SUCCESS);
        }
        #[cfg(unix)]
        Some("hooks") if raw.get(1).map(String::as_str) == Some("print") => {
            hook::print_snippet();
            return Ok(ExitCode::SUCCESS);
        }
        Some("report") => {
            report::report(&raw[1..])?;
            return Ok(ExitCode::SUCCESS);
        }
        Some("sources") => {
            let since: f64 = raw.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            for mut s in kairos_core::sources::all() {
                s.restore(&serde_json::json!({ "since": since }));
                for f in s.poll().into_iter().filter(|f| f.ts >= since) {
                    let project = f.cwd.as_deref().and_then(kairos_core::project::project_of).map(|p| p.id);
                    let harness = s.harness_of(&f).to_string();
                    let mut e = kairos_core::event::Event::new(f.ts, project, f.kind);
                    e.harness = Some(harness);
                    println!("{}", serde_json::to_string(&e)?);
                }
            }
            return Ok(ExitCode::SUCCESS);
        }
        Some("install") => {
            install::install(&raw[1..])?;
            return Ok(ExitCode::SUCCESS);
        }
        Some("uninstall") => {
            install::uninstall(&raw[1..])?;
            return Ok(ExitCode::SUCCESS);
        }
        Some("login") => {
            account::login(&raw[1..])?;
            return Ok(ExitCode::SUCCESS);
        }
        Some("logout") => {
            account::logout()?;
            return Ok(ExitCode::SUCCESS);
        }
        Some("status") => {
            daemon::print_status();
            account::status()?;
            return Ok(ExitCode::SUCCESS);
        }
        Some("pause") => {
            daemon::pause(&raw[1..])?;
            return Ok(ExitCode::SUCCESS);
        }
        Some("resume") => {
            daemon::resume()?;
            return Ok(ExitCode::SUCCESS);
        }
        Some("--version") => {
            println!("kairos {}", env!("CARGO_PKG_VERSION"));
            return Ok(ExitCode::SUCCESS);
        }
        Some("update") => {
            update::update(&raw[1..])?;
            return Ok(ExitCode::SUCCESS);
        }
        Some("doctor") => {
            doctor::doctor()?;
            return Ok(ExitCode::SUCCESS);
        }
        Some("wakatime") => {
            wakatime::command(&raw[1..])?;
            return Ok(ExitCode::SUCCESS);
        }
        Some("hackatime") => {
            hackatime::command(&raw[1..])?;
            return Ok(ExitCode::SUCCESS);
        }
        Some("exclude") => {
            exclude::command(&raw[1..])?;
            return Ok(ExitCode::SUCCESS);
        }
        Some("uninstall-entry") => {
            install::refresh_entry(env!("CARGO_PKG_VERSION"))?;
            return Ok(ExitCode::SUCCESS);
        }
        Some("verify") => {
            return Ok(if report::verify(&raw[1..])? { ExitCode::SUCCESS } else { ExitCode::from(1) });
        }
        _ => {}
    }
    let args = parse_args(raw)?;
    let mut sessions = Vec::new();
    for f in find_session_files(&args.dirs) {
        match parse_file(&f) {
            Ok(s) => sessions.push(s),
            Err(e) => eprintln!("skipping {}: {e}", f.display()),
        }
    }
    let report = analyze(&sessions, &args.opts, Local::now().date_naive());
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_text(&report, args.opts.days);
    }
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    // Rust ignores SIGPIPE, so `kairos status | head` would panic in println! once head exits.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
    }
}
