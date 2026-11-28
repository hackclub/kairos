//! Simulated people for load and credit tests, against a server you started yourself. Never point it at
//! kairos.hackclub.com or a real account.

mod check;
mod client;
mod expected;
mod load;
mod metrics;
mod persona;
mod procstat;
mod rng;
mod scrape;
mod tape;
mod work;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

const USAGE: &str = "usage:
  kairos-sim check --server URL [--personas all|name,name] [--concurrency N] [--seed N] [--run ID] [--no-protocol] [--keep]
  kairos-sim load  --server URL --users N --duration 5m --concurrency C [--setup-concurrency 6] [--history 120m] [--active 0.6]
                   [--reads-per-min 1] [--verify 25] [--pid PID] [--seed N] [--run ID] [--keep]
  kairos-sim personas
  kairos-sim db create|drop --url POSTGRES_URL --schema NAME

The server must be one you started yourself (KAIROS_AUTH=mock, PUBLIC_URL equal to --server, and
KAIROS_TRUSTED_PROXY=loopback so each simulated person gets their own rate limit key).";

struct Args {
    flags: HashMap<String, String>,
}

impl Args {
    fn parse(args: &[String]) -> Result<Args> {
        let mut flags = HashMap::new();
        let mut i = 0;
        while i < args.len() {
            let Some(name) = args[i].strip_prefix("--") else { bail!("unexpected argument {:?}\n{USAGE}", args[i]) };
            if matches!(name, "no-protocol" | "keep") {
                flags.insert(name.to_string(), "1".to_string());
                i += 1;
            } else {
                let v = args.get(i + 1).with_context(|| format!("--{name} needs a value"))?;
                flags.insert(name.to_string(), v.clone());
                i += 2;
            }
        }
        Ok(Args { flags })
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.flags.get(k).map(String::as_str)
    }

    fn need(&self, k: &str) -> Result<&str> {
        self.get(k).with_context(|| format!("--{k} is required\n{USAGE}"))
    }

    fn num<T: std::str::FromStr>(&self, k: &str, default: T) -> Result<T> {
        match self.get(k) {
            None => Ok(default),
            Some(v) => v.parse().map_err(|_| anyhow::anyhow!("--{k}: cannot read {v:?} as a number")),
        }
    }

    fn duration(&self, k: &str, default: Duration) -> Result<Duration> {
        let Some(v) = self.get(k) else { return Ok(default) };
        let (n, unit) = match v.strip_suffix('s') {
            Some(n) => (n, 1),
            None => match v.strip_suffix('m') {
                Some(n) => (n, 60),
                None => match v.strip_suffix('h') {
                    Some(n) => (n, 3600),
                    None => (v, 1),
                },
            },
        };
        Ok(Duration::from_secs(n.parse::<u64>().with_context(|| format!("--{k}: cannot read {v:?} as a duration like 90s, 5m or 1h"))? * unit))
    }
}

fn unix() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
}

fn run_id(a: &Args) -> String {
    a.get("run").map(str::to_string).unwrap_or_else(|| format!("{:x}", (unix() * 1000.0) as u64 & 0xff_ffff_ffff))
}

fn work_dir(a: &Args, run: &str) -> PathBuf {
    a.get("work-dir").map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join(format!("kairos-sim-{run}")))
}

async fn check_cmd(a: &Args) -> Result<bool> {
    let server = a.need("server")?;
    let run = run_id(a);
    let now = unix();
    let ctx = persona::Ctx { now, seed: a.num("seed", 1)? };
    let personas = persona::select(ctx, a.get("personas").unwrap_or("all"))?;
    let dir = work_dir(a, &run);
    let opts = Arc::new(check::Opts { work_dir: dir.clone(), run: run.clone(), now });
    let http = client::Http::new(server, a.num("concurrency", 16)?, Arc::new(metrics::Metrics::default()))?;
    println!("check: {} personas against {server} (run {run})", personas.len());
    for p in &personas {
        println!("  {:<18} {:>5} events on {} device(s): {}", p.name, p.event_count(), p.devices.len(), p.blurb);
    }
    let mut tasks = Vec::new();
    for (i, p) in personas.into_iter().enumerate() {
        let (http, opts) = (http.clone(), opts.clone());
        tasks.push(tokio::spawn(async move { check::run_persona(http, p, i + 1, &opts).await }));
    }
    if a.get("no-protocol").is_none() {
        let (http, opts) = (http.clone(), opts.clone());
        tasks.push(tokio::spawn(async move { check::protocol(http, 900, &opts).await }));
    }
    let mut outcomes = Vec::new();
    for t in tasks {
        outcomes.push(t.await?);
    }
    let ok = check::report(&outcomes);
    if a.get("keep").is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    Ok(ok)
}

async fn load_cmd(a: &Args) -> Result<bool> {
    let run = run_id(a);
    let dir = work_dir(a, &run);
    let opts = load::Opts {
        server: a.need("server")?.to_string(),
        users: a.num("users", 100)?,
        duration: a.duration("duration", Duration::from_secs(300))?,
        concurrency: a.num("concurrency", 64)?,
        setup_concurrency: a.num("setup-concurrency", 6)?,
        history: a.duration("history", Duration::from_secs(7200))?,
        active: a.num("active", 0.6)?,
        reads_per_min: a.num("reads-per-min", 1.0)?,
        verify: a.num("verify", 25)?,
        pid: a.get("pid").map(|p| p.parse()).transpose().context("--pid")?,
        seed: a.num("seed", 1)?,
        work_dir: dir.clone(),
        run,
    };
    let ok = load::run(opts).await;
    if a.get("keep").is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    ok
}

async fn db_cmd(action: &str, a: &Args) -> Result<()> {
    let schema = a.need("schema")?;
    if schema.is_empty() || !schema.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
        bail!("--schema must be lowercase letters, digits and underscores");
    }
    let (client, conn) = tokio_postgres::connect(a.need("url")?, tokio_postgres::NoTls).await.context("connecting to Postgres")?;
    tokio::spawn(conn);
    match action {
        "create" => client.batch_execute(&format!("CREATE SCHEMA {schema}")).await?,
        "drop" => client.batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE")).await?,
        other => bail!("db {other}?\n{USAGE}"),
    }
    println!("schema {schema}: {action} ok");
    Ok(())
}

async fn real() -> Result<bool> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else { bail!("{USAGE}") };
    match cmd.as_str() {
        "check" => check_cmd(&Args::parse(&args[1..])?).await,
        "load" => load_cmd(&Args::parse(&args[1..])?).await,
        "personas" => {
            for p in persona::select(persona::Ctx { now: unix(), seed: 1 }, "all")? {
                let logs: Vec<&[kairos_core::event::Event]> = p.devices.iter().map(|d| d.events.as_slice()).collect();
                let credit = expected::Expected::new(p.tz, &logs).all_total() / 3600.0;
                println!("{:<18} {:>5} events {:>6.2}h credit  {}", p.name, p.event_count(), credit, p.blurb);
            }
            Ok(true)
        }
        "db" => {
            let Some(action) = args.get(1) else { bail!("{USAGE}") };
            db_cmd(action, &Args::parse(&args[2..])?).await?;
            Ok(true)
        }
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(true)
        }
        other => bail!("unknown command {other:?}\n{USAGE}"),
    }
}

#[tokio::main]
async fn main() {
    match real().await {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(e) => {
            eprintln!("kairos-sim: {e:#}");
            std::process::exit(2);
        }
    }
}
