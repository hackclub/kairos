use std::env::consts::{ARCH, OS};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::term::{DOWN, ENTER};

/// When every text is on screen, press the keys.
pub type Rule = (&'static [&'static str], &'static [&'static str]);

pub struct Launch {
    pub args: Vec<String>,
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
    pub first_run: &'static [Rule],
    /// Any of these on a still screen means the prompt box is up.
    pub ready: &'static [&'static str],
    pub during: &'static [Rule],
    pub prepare: fn(home: &Path, project: &Path, model_url: &str, version: &str) -> Result<Launch>,
}

pub const ALL: &[Harness] = &[
    Harness {
        id: "claude-code",
        source: Source::Npm("@anthropic-ai/claude-code"),
        bin: "claude",
        stores: &[".claude/projects"],
        first_run: &[
            (&["Choose the text style"], &[ENTER]),
            (&["Press Enter to continue"], &[ENTER]),
            (&["❯ No, exit", "Yes, I trust this folder"], &[DOWN, ENTER]),
            (&["❯ Yes, I trust this folder"], &[ENTER]),
            (&["Do you trust the files in this folder?", "❯ 1. Yes, proceed"], &[ENTER]),
        ],
        ready: &["shift+tab to cycle", "? for shortcuts"],
        during: &[(&["Do you want to proceed?", "❯ No"], &[DOWN, ENTER]), (&["Do you want to proceed?", "❯ Yes"], &[ENTER])],
        prepare: claude,
    },
    Harness {
        id: "codex",
        source: Source::Npm("@openai/codex"),
        bin: "codex",
        stores: &[".codex/sessions"],
        first_run: &[(&["Trust and continue"], &[ENTER])],
        ready: &["? for shortcuts"],
        during: &[],
        prepare: codex,
    },
    Harness {
        id: "opencode",
        source: Source::Github("sst/opencode"),
        bin: "opencode",
        stores: &[".local/share/opencode/opencode.db", ".local/share/opencode/opencode.db-wal", ".local/share/opencode/opencode.db-shm"],
        first_run: &[],
        ready: &["ctrl+p commands"],
        during: &[],
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
    Ok(Launch { args: Vec::new(), env: vars(&env), after: Vec::new() })
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
    Ok(Launch { args: Vec::new(), env: vars(&[("LAB_KEY", "lab")]), after: Vec::new() })
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
    Ok(Launch { args, env: vars(&[("LAB_KEY", "lab")]), after })
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
    fn newest(&self) -> Result<String> {
        let version = match self.source {
            Source::Npm(package) => output(npm().args(["view", package, "version"]))?,
            Source::Github(repo) => {
                let latest = output(Command::new("curl").args(["-fsSL", &format!("https://api.github.com/repos/{repo}/releases/latest")]))?;
                let latest: Value = serde_json::from_str(&latest).context("GitHub's latest release")?;
                latest["tag_name"].as_str().unwrap_or_default().trim_start_matches('v').to_string()
            }
        };
        if version.is_empty() {
            bail!("could not find the newest {}", self.id);
        }
        Ok(version)
    }

    /// Installs one exact version into its own folder and gives back the program and the version.
    pub fn install(&self, version: &str, tools: &Path) -> Result<(PathBuf, String)> {
        let version = if version == "latest" { self.newest()? } else { version.to_string() };
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
