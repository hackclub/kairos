use std::env::consts::{ARCH, OS};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::term::{DOWN, ENTER, ESC};

/// When every text is on screen, press the keys.
pub type Rule = (&'static [&'static str], &'static [&'static str]);

pub struct Launch {
    pub args: Vec<String>,
    /// The arguments that open its last session in this folder again.
    pub resume: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Arguments for one more call of the program once the run is over, to stop what it left running.
    pub after: Vec<String>,
}

pub enum Source {
    Npm(&'static str),
    /// A GitHub repo whose releases carry `<bin>-<os>-<arch>` archives.
    Github(&'static str),
}

pub struct Harness {
    /// The name Kairos gives it in events.
    pub id: &'static str,
    pub source: Source,
    pub bin: &'static str,
    /// What it writes that Kairos reads, under the home folder.
    pub stores: &'static [&'static str],
    /// Everything it keeps under the home folder, settings and caches too.
    pub state: &'static [&'static str],
    pub first_run: &'static [Rule],
    /// Any of these on a still screen means the prompt box is up.
    pub ready: &'static [&'static str],
    pub during: &'static [Rule],
    /// The first version that writes down the permission prompts it showed and the pastes it put a placeholder for.
    /// Only Claude Code asks before a command outside its allow list, the others run with approvals off.
    pub marks_since: Option<&'static str>,
    pub prepare: fn(home: &Path, project: &Path, model_url: &str, version: &str) -> Result<Launch>,
}

pub const ALL: &[Harness] = &[
    Harness {
        id: "claude-code",
        source: Source::Npm("@anthropic-ai/claude-code"),
        bin: "claude",
        stores: &[".claude/projects", ".claude/history.jsonl"],
        state: &[".claude", ".claude.json"],
        first_run: &[
            (&["Choose the text style"], &[ENTER]),
            (&["Press Enter to continue"], &[ENTER]),
            (&["❯ No, exit", "Yes, I trust this folder"], &[DOWN]),
            (&["❯ Yes, I trust this folder"], &[ENTER]),
            (&["Do you trust the files in this folder?", "❯ 1. Yes, proceed"], &[ENTER]),
            // Asked when a session is opened again. Auto mode needs Anthropic's own servers.
            (&["❯ No, keep accept edits"], &[ENTER]),
            (&["Make auto mode your default permission mode?", "❯ Yes, set auto mode"], &[DOWN]),
            // Asked in Apple's Terminal, where it would change the terminal's own settings.
            (&["❯ 2. No, maybe later with /terminal-setup"], &[ENTER]),
            (&["No, maybe later with /terminal-setup"], &[DOWN]),
        ],
        ready: &["shift+tab to cycle", "? for shortcuts"],
        during: &[
            (&["Do you want to proceed?", "❯ No"], &[DOWN]),
            (&["Do you want to proceed?", "❯ Yes"], &[ENTER]),
            (&["Do you want to proceed?", "❯ 1. Yes"], &[ENTER]),
        ],
        // 2.0 keeps no record of a permission prompt, and puts a 20 line paste in the prompt as it is.
        marks_since: Some("2.1.0"),
        prepare: claude,
    },
    Harness {
        id: "codex",
        source: Source::Npm("@openai/codex"),
        bin: "codex",
        stores: &[".codex/sessions"],
        state: &[".codex"],
        // It looks for a newer version in the background and offers it the next time it opens.
        first_run: &[(&["Trust and continue"], &[ENTER]), (&["Update available", "esc skip"], &[ESC])],
        ready: &["? for shortcuts"],
        during: &[],
        marks_since: None,
        prepare: codex,
    },
    Harness {
        id: "opencode",
        source: Source::Github("sst/opencode"),
        bin: "opencode",
        stores: &[".local/share/opencode/opencode.db", ".local/share/opencode/opencode.db-wal", ".local/share/opencode/opencode.db-shm"],
        state: &[".config/opencode", ".local/share/opencode", ".local/state/opencode", ".cache/opencode"],
        // After an update it opens with its release notes.
        first_run: &[(&["What's new in"], &[ENTER])],
        ready: &["ctrl+p commands"],
        during: &[],
        marks_since: None,
        prepare: opencode,
    },
];

fn vars(env: &[(&str, &str)]) -> Vec<(String, String)> {
    env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn claude(home: &Path, _project: &Path, model_url: &str, _version: &str) -> Result<Launch> {
    let dir = home.join(".claude");
    fs::create_dir_all(&dir)?;
    // Auto mode needs Anthropic's own servers, so the lab runs in accept edits and allows the scripted git commands.
    let settings = json!({ "permissions": { "defaultMode": "acceptEdits", "allow": ["Bash(git add:*)", "Bash(git commit:*)"] } });
    fs::write(dir.join("settings.json"), serde_json::to_string_pretty(&settings)?)?;
    let env = [
        ("ANTHROPIC_BASE_URL", model_url),
        ("ANTHROPIC_AUTH_TOKEN", "lab"),
        ("DISABLE_AUTOUPDATER", "1"),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ];
    Ok(Launch { args: Vec::new(), resume: vec!["--continue".to_string()], env: vars(&env), after: Vec::new() })
}

fn codex(home: &Path, project: &Path, model_url: &str, _version: &str) -> Result<Launch> {
    let dir = home.join(".codex");
    fs::create_dir_all(&dir)?;
    // The folder is trusted up front, the way Codex itself saves it: its trust question comes up seconds after the
    // prompt on a slow machine, and typing would land in it.
    let config = format!(
        "model = \"gpt-5.1-codex\"\nmodel_provider = \"lab\"\napproval_policy = \"never\"\nsandbox_mode = \"danger-full-access\"\n\n\
         [model_providers.lab]\nname = \"lab\"\nbase_url = \"{model_url}/v1\"\nwire_api = \"responses\"\nenv_key = \"LAB_KEY\"\n\n\
         [projects.'{}']\ntrust_level = \"trusted\"\n",
        project.display()
    );
    fs::write(dir.join("config.toml"), config)?;
    Ok(Launch { args: Vec::new(), resume: vec!["resume".to_string(), "--last".to_string()], env: vars(&[("LAB_KEY", "lab")]), after: Vec::new() })
}

fn opencode(home: &Path, _project: &Path, model_url: &str, version: &str) -> Result<Launch> {
    let dir = home.join(".config").join("opencode");
    fs::create_dir_all(&dir)?;
    let base = format!("{model_url}/v1");
    let models = json!({ "test-model": {} });
    // 2.x renamed the provider keys and runs a background server that outlives the window.
    let (config, args, after) = if version.starts_with("1.") {
        let lab = json!({ "npm": "@ai-sdk/openai-compatible", "options": { "baseURL": base, "apiKey": "lab" }, "models": models });
        (json!({ "model": "lab/test-model", "provider": { "lab": lab } }), vec![], vec![])
    } else {
        let lab = json!({ "package": "@opencode/ai/providers/openai-compatible", "settings": { "baseURL": base }, "env": ["LAB_KEY"], "models": models });
        (json!({ "model": "lab/test-model", "providers": { "lab": lab } }), vec!["--auto".to_string()], vec!["service".to_string(), "stop".to_string()])
    };
    fs::write(dir.join("opencode.json"), serde_json::to_string_pretty(&config)?)?;
    let resume = args.iter().cloned().chain(["--continue".to_string()]).collect();
    Ok(Launch { args, resume, env: vars(&[("LAB_KEY", "lab")]), after })
}

pub fn find(id: &str) -> Result<&'static Harness> {
    match ALL.iter().find(|h| h.id == id) {
        Some(h) => Ok(h),
        None => bail!("no harness {id:?}, the lab knows: {}", ALL.iter().map(|h| h.id).collect::<Vec<_>>().join(", ")),
    }
}

fn output(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().with_context(|| format!("running {:?}", cmd.get_program()))?;
    if !out.status.success() {
        bail!("{:?} failed: {}", cmd.get_program(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn npm() -> Command {
    Command::new(if cfg!(windows) { "npm.cmd" } else { "npm" })
}

impl Harness {
    pub fn marks(&self, version: &str) -> bool {
        let n = |v: &str| v.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
        self.marks_since.is_some_and(|since| n(version) >= n(since))
    }

    /// `latest` is what the harness hands out as its newest, `2.x` the highest 2 it has released.
    fn newest(&self, want: &str) -> Result<String> {
        let major = want.strip_suffix(".x");
        let version = match (&self.source, major) {
            (Source::Npm(package), None) => output(npm().args(["view", package, "version"]))?,
            (Source::Npm(package), Some(m)) => {
                let all = output(npm().args(["view", &format!("{package}@{m}"), "version", "--json"]))?;
                match serde_json::from_str(&all).context("npm's version list")? {
                    Value::Array(a) => a.last().and_then(Value::as_str).unwrap_or_default().to_string(),
                    one => one.as_str().unwrap_or_default().to_string(),
                }
            }
            (Source::Github(repo), _) => {
                let mut curl = Command::new("curl");
                curl.arg("-fsSL");
                // Runners share addresses, and GitHub turns away their unsigned requests.
                if let Ok(token) = std::env::var("GITHUB_TOKEN") {
                    curl.args(["-H", &format!("Authorization: Bearer {token}")]);
                }
                let all = output(curl.arg(format!("https://api.github.com/repos/{repo}/releases?per_page=100")))?;
                let all: Value = serde_json::from_str(&all).context("GitHub's releases")?;
                let numbers = |r: &Value| -> Option<Vec<u64>> {
                    if r["prerelease"] == true || r["draft"] == true {
                        return None;
                    }
                    let n: Vec<u64> = r["tag_name"].as_str()?.strip_prefix('v')?.split('.').map(str::parse).collect::<Result<_, _>>().ok()?;
                    (n.len() == 3 && major.is_none_or(|m| n[0].to_string() == m)).then_some(n)
                };
                let top = all.as_array().into_iter().flatten().filter_map(numbers).max().unwrap_or_default();
                top.iter().map(u64::to_string).collect::<Vec<_>>().join(".")
            }
        };
        if version.is_empty() {
            bail!("could not find the newest {} for {want:?}", self.id);
        }
        Ok(version)
    }

    /// Installs one exact version into its own folder and gives back the program and the version.
    pub fn install(&self, version: &str, tools: &Path) -> Result<(PathBuf, String)> {
        let version = if version == "latest" || version.ends_with(".x") { self.newest(version)? } else { version.to_string() };
        let dir = tools.join(format!("{}-{version}", self.id));
        let bin = match self.source {
            Source::Npm(_) => dir.join("node_modules").join(".bin").join(if cfg!(windows) { format!("{}.cmd", self.bin) } else { self.bin.to_string() }),
            Source::Github(_) => dir.join(if cfg!(windows) { format!("{}.exe", self.bin) } else { self.bin.to_string() }),
        };
        if bin.exists() {
            return Ok((bin, version));
        }
        fs::create_dir_all(&dir)?;
        match self.source {
            Source::Npm(package) => {
                let spec = format!("{package}@{version}");
                output(npm().args(["install", "--no-audit", "--no-fund", "--loglevel", "error", "--prefix"]).arg(&dir).arg(&spec))?;
            }
            Source::Github(repo) => {
                let (os, ext) = match OS {
                    "linux" => ("linux", "tar.gz"),
                    "macos" => ("darwin", "zip"),
                    _ => ("windows", "zip"),
                };
                let asset = format!("{}-{os}-{}.{ext}", self.bin, if ARCH == "aarch64" { "arm64" } else { "x64" });
                let url = format!("https://github.com/{repo}/releases/download/v{version}/{asset}");
                output(Command::new("curl").args(["-fsSL", "-o"]).arg(dir.join(&asset)).arg(&url)).with_context(|| url.clone())?;
                output(Command::new("tar").arg("-xf").arg(dir.join(&asset)).arg("-C").arg(&dir))?;
                fs::remove_file(dir.join(&asset))?;
            }
        }
        if !bin.exists() {
            bail!("installed {} {version} but there is no {}", self.id, bin.display());
        }
        Ok((bin, version))
    }
}
