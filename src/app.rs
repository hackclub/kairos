//! Desktop apps. The lab clicks and types in the app's own window through the Chrome DevTools protocol, which hands the
//! page the same mouse and key events a person's hands do, or with the system's own key events when the app keeps its
//! debugging port closed.

use std::env::consts::{ARCH, OS};
use std::fs::{self, OpenOptions};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::harness::{self, newest_release, output};
use crate::model::Model;
use crate::person::Person;
use crate::scenario::{self, Scenario};
use crate::{credit, Args, STRAY_ENV};

const PORT: u16 = 9333;

pub struct App {
    pub id: &'static str,
    source: Source,
    /// The harness it runs, which Kairos names in its events.
    harness: &'static str,
    /// What it keeps in the home folder, removed before a run in a real home.
    state: &'static [&'static str],
    stores: &'static [&'static str],
    /// The program's file name, where it isn't the one inside a macOS .app.
    exe: fn(&str) -> bool,
    drive: Drive,
    /// The agent to pick in an app that runs several, where it isn't the app's first.
    pick: Option<Pick>,
}

struct Pick {
    /// The name the app shows for the agent.
    name: &'static str,
    /// The app's id for it.
    id: &'static str,
    /// The lab's model by name, where the agent also offers models of its own.
    model: Option<&'static str>,
    /// Off until turned on, on the welcome page.
    off: bool,
}

enum Source {
    /// A repo's GitHub releases, with the file for this system from the version.
    GitHub(&'static str, fn(&str) -> String),
    /// Google's updater, which hands out only the newest build.
    Antigravity,
    /// OpenAI's feed for the Mac and the Windows package, newest only.
    Codex,
    /// Anthropic's apt repo, the Mac's feed and the Windows package list.
    Claude,
    /// Microsoft's update service, newest only.
    VsCode,
}

enum Drive {
    /// Over the app's debugging port.
    Page,
    /// Over the debugging port, in the agent's panel beside the editor.
    Panel,
    /// The app opens the project from the link this makes and its message box takes the keys.
    Keys(fn(&Path) -> String),
    /// Over the debugging port, in VS Code's own chat beside the editor.
    Chat,
}

fn t3_asset(v: &str) -> String {
    match (OS, ARCH) {
        ("linux", _) => format!("T3-Code-{v}-amd64.deb"),
        ("macos", "aarch64") => format!("T3-Code-{v}-arm64.zip"),
        ("macos", _) => format!("T3-Code-{v}-x64.zip"),
        _ => format!("T3-Code-{v}-x64.exe"),
    }
}

fn t3_exe(name: &str) -> bool {
    if OS == "linux" {
        name == "t3code"
    } else {
        name.starts_with("T3 Code") && name.ends_with(".exe")
    }
}

pub const ALL: &[App] = &[
    App {
        id: "t3code",
        source: Source::GitHub("pingdotgg/t3code", t3_asset),
        harness: "claude-code",
        state: &[".t3", ".claude", ".claude.json"],
        stores: &[".claude/projects", ".t3/userdata/statev2.sqlite", ".t3/userdata/statev2.sqlite-wal", ".t3/userdata/logs"],
        exe: t3_exe,
        drive: Drive::Page,
        pick: None,
    },
    // T3 starts `codex app-server` itself, on the same ~/.codex.
    App {
        id: "t3code-codex",
        source: Source::GitHub("pingdotgg/t3code", t3_asset),
        harness: "codex",
        state: &[".t3", ".codex"],
        stores: &[".codex/sessions", ".t3/userdata/statev2.sqlite", ".t3/userdata/statev2.sqlite-wal", ".t3/userdata/logs"],
        exe: t3_exe,
        drive: Drive::Page,
        pick: Some(Pick { name: "Codex", id: "codex", model: None, off: false }),
    },
    // T3 starts `opencode serve` itself, on the same config and database.
    App {
        id: "t3code-opencode",
        source: Source::GitHub("pingdotgg/t3code", t3_asset),
        harness: "opencode",
        state: &[".t3", ".config/opencode", ".local/share/opencode", ".local/state/opencode"],
        stores: &[".local/share/opencode/opencode.db", ".local/share/opencode/opencode.db-wal", ".t3/userdata/statev2.sqlite", ".t3/userdata/statev2.sqlite-wal", ".t3/userdata/logs"],
        exe: t3_exe,
        drive: Drive::Page,
        pick: Some(Pick { name: "OpenCode", id: "opencode", model: Some("test-model"), off: true }),
    },
    // Stable builds drop --remote-debugging-port. Its own state is in the system's app data folder, which on Windows
    // isn't under the run's home.
    App {
        id: "opencode-desktop",
        source: Source::GitHub("anomalyco/opencode", |_| match (OS, ARCH) {
            ("linux", _) => "opencode-desktop-linux-amd64.deb".to_string(),
            ("macos", "aarch64") => "opencode-desktop-mac-arm64.zip".to_string(),
            ("macos", _) => "opencode-desktop-mac-x64.zip".to_string(),
            _ => "opencode-desktop-win-x64.exe".to_string(),
        }),
        harness: "opencode",
        state: &[".config/opencode", ".config/ai.opencode.desktop", ".local/share/opencode", ".local/state/opencode"],
        stores: &[".local/share/opencode/opencode.db", ".local/share/opencode/opencode.db-wal", ".config/ai.opencode.desktop/drafts.sqlite", ".config/ai.opencode.desktop/opencode.settings"],
        exe: |name| if OS == "linux" { name == "ai.opencode" } else { name == "OpenCode.exe" },
        drive: Drive::Keys(|p| format!("opencode://open-project?directory={}", p.display())),
        pick: None,
    },
    // OpenAI's Codex app, now called ChatGPT. It runs the codex inside it, on the same ~/.codex.
    App {
        id: "codex-app",
        source: Source::Codex,
        harness: "codex",
        state: &[".codex"],
        stores: &[".codex/sessions", ".codex/log"],
        exe: |name| name == "ChatGPT.exe",
        drive: Drive::Page,
        pick: None,
    },
    // Claude desktop's Code tab, which runs a Claude Code of its own. With no sign-in it takes the lab model as an
    // organization's gateway from machine policy.
    App {
        id: "claude-desktop",
        source: Source::Claude,
        harness: "claude-code",
        state: &[".claude", ".claude.json", ".config/Claude-3p"],
        stores: &[".claude/projects"],
        exe: |name| name == "claude-desktop" || name.eq_ignore_ascii_case("claude.exe"),
        drive: Drive::Keys(|p| format!("claude://code/new?folder={}", p.display())),
        pick: None,
    },
    // The editor with Google's agent in a side panel, which runs agy from ~/.gemini/bin.
    App {
        id: "antigravity-ide",
        source: Source::Antigravity,
        harness: "antigravity",
        state: &[".gemini", ".antigravity-ide", ".config/Antigravity IDE"],
        stores: &[".gemini/antigravity/brain", ".gemini/antigravity/conversation_summaries.db", ".gemini/antigravity/cli.log", ".gemini/antigravity/log", ".gemini/config"],
        exe: |name| name == "antigravity-ide" || name == "Antigravity IDE.exe",
        drive: Drive::Panel,
        pick: None,
    },
    // VS Code's own chat, with a model of the person's own (no GitHub sign-in since 1.122). It runs the chat on the Copilot
    // CLI's runtime and keeps it where the CLI does.
    App {
        id: "vscode",
        source: Source::VsCode,
        harness: "copilot",
        state: &[".copilot", ".vscode", ".config/Code"],
        stores: &[".copilot/session-state", ".config/Code/User/workspaceStorage", ".config/Code/agentSessionData"],
        exe: |name| name == "code" || name == "Code.exe",
        drive: Drive::Chat,
        pick: None,
    },
];

pub fn find(id: &str) -> Option<&'static App> {
    ALL.iter().find(|a| a.id == id)
}

/// The app's program in what its package unpacked to.
fn program(dir: &Path, fits: fn(&str) -> bool) -> Option<PathBuf> {
    if OS == "macos" {
        let app = fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "app"))?;
        return fs::read_dir(app.join("Contents").join("MacOS")).ok()?.flatten().map(|e| e.path()).next();
    }
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        let Ok(entries) = fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let Ok(kind) = e.file_type() else { continue };
            if kind.is_dir() {
                todo.push(e.path());
            } else if kind.is_file() && e.file_name().to_str().is_some_and(fits) {
                return Some(e.path());
            }
        }
    }
    None
}

impl App {
    fn install(&self, want: &str, tools: &Path) -> Result<(PathBuf, String)> {
        let (version, url) = match self.source {
            Source::GitHub(repo, asset) => {
                let version = if want == "latest" { newest_release(repo, None)? } else { want.to_string() };
                let url = format!("https://github.com/{repo}/releases/download/v{version}/{}", asset(&version));
                (version, url)
            }
            Source::Antigravity => antigravity_ide(want)?,
            Source::Codex => codex_app(want)?,
            Source::Claude => claude_desktop(want)?,
            Source::VsCode => vs_code(want)?,
        };
        let dir = tools.join(format!("{}-{version}", self.id));
        if let Some(exe) = program(&dir, self.exe) {
            return Ok((exe, version));
        }
        fs::create_dir_all(&dir)?;
        let file = dir.join(url.rsplit('/').next().unwrap_or_default().replace("%20", "-"));
        output(Command::new("curl").args(["-fsSL", "--retry", "3", "-o"]).arg(&file).arg(&url)).with_context(|| url.clone())?;
        let name = file.to_string_lossy().to_string();
        let into = dir.join("app");
        match OS {
            _ if name.ends_with(".tar.gz") => output(Command::new("tar").arg("-xzf").arg(&file).arg("-C").arg(&dir))?,
            // Installed the way the Store does it, so the app runs with its package's name like on a person's PC.
            _ if name.ends_with(".msix") => {
                let add = format!("Add-AppxPackage -Path '{}'; (Get-AppxPackage -Name OpenAI.Codex).InstallLocation", file.display());
                let at = output(Command::new("powershell").args(["-NoProfile", "-Command", &add]))?;
                let look = "$p = Get-AppxPackage -Name OpenAI.Codex; $p.PackageFamilyName; (Get-AppxPackageManifest $p).OuterXml";
                if let Ok(manifest) = output(Command::new("powershell").args(["-NoProfile", "-Command", look])) {
                    let _ = fs::write(tools.parent().unwrap_or(tools).join("codex-manifest.txt"), manifest);
                }
                fs::remove_file(&file)?;
                let exe = PathBuf::from(at.trim()).join("app").join("ChatGPT.exe");
                if !exe.is_file() {
                    bail!("installed the Codex app but there is no {}", exe.display());
                }
                return Ok((exe, version));
            }
            // A zip, or one by another name. Windows' own tar reads zips, Git's doesn't.
            _ if cfg!(windows) && (name.ends_with(".nupkg") || name.ends_with(".zip")) => {
                let tar = PathBuf::from(std::env::var_os("SystemRoot").context("no SystemRoot")?).join("System32").join("tar.exe");
                output(Command::new(tar).arg("-xf").arg(&file).arg("-C").arg(&dir))?
            }
            "linux" => output(Command::new("dpkg-deb").arg("-x").arg(&file).arg(&dir))?,
            // ditto keeps the links inside the app's frameworks.
            "macos" => output(Command::new("ditto").args(["-x", "-k"]).arg(&file).arg(&dir))?,
            // VS Code's Inno Setup installer, which would start the editor when done unless told not to.
            _ if matches!(self.source, Source::Antigravity) => output(
                Command::new(&file).args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/MERGETASKS=!runcode"]).arg(format!("/DIR={}", into.display())),
            )?,
            // The installer, silent and into the folder named last.
            _ => output(Command::new(&file).arg("/S").arg(format!("/D={}", into.display())))?,
        };
        fs::remove_file(&file)?;
        let exe = program(&dir, self.exe).with_context(|| format!("installed {} {version} but found no program in {}", self.id, dir.display()))?;
        Ok((exe, version))
    }
}

/// The newest Antigravity IDE and where to get it. The updater names no version, its download folder does.
fn antigravity_ide(want: &str) -> Result<(String, String)> {
    let system = match (OS, ARCH) {
        ("linux", _) => "linux-x64",
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", _) => "darwin",
        _ => "win32-x64-user",
    };
    let api = format!("https://antigravity-ide-auto-updater-974169037036.us-central1.run.app/api/update/{system}/stable/latest");
    let latest: Value = serde_json::from_str(&output(Command::new("curl").args(["-fsSL", "--retry", "3", &api]))?).context("the updater's answer")?;
    let url = latest["url"].as_str().context("the updater gave no url")?.replace(' ', "%20");
    let version = url.split("/stable/").nth(1).and_then(|r| r.split(['-', '/']).next()).context("no version in the download's url")?.to_string();
    if want != "latest" && want != version {
        bail!("Google only hands out the newest Antigravity IDE, {version}, not {want}");
    }
    Ok((version, url))
}

/// The newest VS Code and where to get it.
fn vs_code(want: &str) -> Result<(String, String)> {
    let system = match OS {
        "linux" => "linux-x64",
        "macos" => "darwin-universal",
        _ => "win32-x64-archive",
    };
    let api = format!("https://update.code.visualstudio.com/api/update/{system}/stable/latest");
    let latest: Value = serde_json::from_str(&output(Command::new("curl").args(["-fsSL", "--retry", "3", &api]))?).context("VS Code's update service")?;
    let version = latest["productVersion"].as_str().context("VS Code's update service named no version")?.to_string();
    let url = latest["url"].as_str().context("VS Code's update service gave no url")?.to_string();
    if want != "latest" && want != version {
        bail!("the lab only gets the newest VS Code, {version}, not {want}");
    }
    Ok((version, url))
}

/// Where VS Code keeps its settings and chats, and whether the lab has to name it. Windows keeps it in the runner's own
/// AppData, which is where kairosd looks too.
fn vscode_data(home: &Path) -> Result<(PathBuf, bool)> {
    Ok(match OS {
        "linux" => (home.join(".config").join("Code"), false),
        // A macOS app finds Application Support from the account, not $HOME, and the folder's socket can't take a path
        // longer than 103 characters, so a short one, linked to where kairosd looks in the run's home.
        "macos" => {
            let short = PathBuf::from(format!("/tmp/lab-code-{}", std::process::id()));
            let _ = fs::remove_dir_all(&short);
            let support = home.join("Library").join("Application Support");
            fs::create_dir_all(&support)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&short, support.join("Code"))?;
            (short, true)
        }
        _ => (PathBuf::from(std::env::var_os("APPDATA").context("no APPDATA")?).join("Code"), false),
    })
}

/// Install gives the person Hackatime's plugin and points WakaTime at kairosd, so the editor's own heartbeats are in the
/// run like on a real machine.
fn hackatime(exe: &Path, kairosd: &Path, p: &crate::Place) -> Result<()> {
    let home = |cmd: &mut Command| {
        cmd.env("HOME", &p.home).env("USERPROFILE", &p.home).env("KAIROS_DIR", &p.kd);
        for k in crate::STRAY_ENV {
            cmd.env_remove(k);
        }
    };
    let mut on = Command::new(kairosd.with_file_name(if cfg!(windows) { "kairos.exe" } else { "kairos" }));
    home(on.args(["wakatime", "on"]));
    output(&mut on)?;
    let dir = exe.parent().context("VS Code has no folder")?;
    let cli = match OS {
        "macos" => dir.parent().context("VS Code is in no bundle")?.join("Resources").join("app").join("bin").join("code"),
        "windows" => dir.join("bin").join("code.cmd"),
        _ => dir.join("bin").join("code"),
    };
    let mut add = Command::new(cli);
    home(add.args(["--install-extension", "WakaTime.vscode-wakatime"]));
    output(&mut add)?;
    Ok(())
}

/// A person who brings their own model adds it in the Language Models editor, which writes chatLanguageModels.json,
/// and lets the agent run its tools without asking each time.
fn vscode_setup(data: &Path, model_url: &str) -> Result<()> {
    let user = data.join("User");
    fs::create_dir_all(&user)?;
    let models = json!([{
        "name": "Lab", "vendor": "customendpoint", "apiKey": "lab", "apiType": "chat-completions",
        "models": [{ "id": "test-model", "name": "Lab Model", "url": format!("{model_url}/v1/chat/completions"),
                     "toolCalling": true, "vision": false, "maxInputTokens": 100000, "maxOutputTokens": 8000 }],
    }]);
    fs::write(user.join("chatLanguageModels.json"), serde_json::to_string_pretty(&models)?)?;
    let settings = json!({
        "telemetry.telemetryLevel": "off",
        "update.mode": "none",
        "extensions.autoUpdate": false,
        "workbench.startupEditor": "none",
        "security.workspace.trust.enabled": false,
        "chat.agentHost.byokModels.enabled": true,
        "chat.tools.global.autoApprove": true,
        "chat.tools.global.autoApprove.optIn": true,
    });
    fs::write(user.join("settings.json"), serde_json::to_string_pretty(&settings)?)?;
    Ok(())
}

/// VS Code's first run: its welcome asks for a sign-in, then a theme, and the chat beside the editor picks up the model.
fn vscode_start(port: u16) -> Result<Page> {
    let mut page = Page::open(port, Duration::from_secs(90))?;
    // The chat beside the editor draws before the welcome does.
    page.wait_for(&["Continue without Signing In"], Duration::from_secs(90))?;
    page.press("Continue without Signing In", "Get Started")?;
    page.click("Get Started")?;
    let picked = |page: &mut Page| -> Result<bool> { Ok(page.ax()?.iter().any(|(_, n, _)| n == "Models, Lab Model")) };
    let until = Instant::now() + Duration::from_secs(60);
    let mut looked = false;
    while !picked(&mut page)? {
        if Instant::now() > until {
            bail!("the chat never offered the lab's model. Page:\n{}", page.text()?);
        }
        // Until the person opens the list of models once, the chat shows a note about adding one.
        if !looked && page.text()?.contains("Add Models") {
            page.click("Add Models")?;
            sleep(Duration::from_secs(2));
            page.key("Escape")?;
            looked = true;
        }
        sleep(Duration::from_millis(500));
    }
    Ok(page)
}

/// The newest Codex app. Only the Mac's feed names versions.
fn codex_app(want: &str) -> Result<(String, String)> {
    let (version, url) = if OS == "macos" {
        let feed = output(Command::new("curl").args(["-fsSL", "--retry", "3", "https://persistent.oaistatic.com/codex-app-prod/appcast.xml"]))?;
        let url = feed.split("<enclosure url=\"").nth(1).and_then(|r| r.split('"').next()).context("no download in the Codex app's feed")?;
        let version = url.rsplit('-').next().and_then(|v| v.strip_suffix(".zip")).context("no version in the Codex app's download")?;
        (version.to_string(), url.to_string())
    } else {
        ("latest".to_string(), format!("https://persistent.oaistatic.com/codex-app-prod/ChatGPT-{}.msix", if ARCH == "aarch64" { "arm64" } else { "x64" }))
    };
    if want != "latest" && want != version {
        bail!("OpenAI only hands out the newest Codex app, {version}, not {want}");
    }
    Ok((version, url))
}

/// The newest Claude desktop. Linux builds are numbered apart from the Mac's and Windows'.
fn claude_desktop(want: &str) -> Result<(String, String)> {
    let get = |url: &str| output(Command::new("curl").args(["-fsSL", "--retry", "3", url]));
    let (version, url) = match OS {
        "linux" => {
            let apt = "https://downloads.claude.ai/claude-desktop/apt/stable";
            let packages = get(&format!("{apt}/dists/stable/main/binary-amd64/Packages"))?;
            let mut builds: Vec<(Vec<u64>, String, String)> = packages
                .split("\n\n")
                .filter(|p| p.lines().any(|l| l == "Package: claude-desktop"))
                .filter_map(|p| {
                    let field = |k: &str| p.lines().find_map(|l| l.strip_prefix(k)).map(str::trim);
                    let v = field("Version:")?;
                    Some((v.split('.').map(|n| n.parse().unwrap_or(0)).collect(), v.to_string(), format!("{apt}/{}", field("Filename:")?)))
                })
                .collect();
            builds.sort();
            let (_, version, url) = builds.pop().context("no claude-desktop in Anthropic's apt repo")?;
            (version, url)
        }
        "macos" => {
            let feed: Value = serde_json::from_str(&get("https://downloads.claude.ai/releases/darwin/universal/RELEASES.json")?).context("the Mac feed")?;
            let version = feed["currentRelease"].as_str().context("the Mac feed names no release")?;
            let url = feed["releases"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|r| r["version"] == version)
                .and_then(|r| r["updateTo"]["url"].as_str())
                .context("no download for the Mac's current release")?;
            (version.to_string(), url.to_string())
        }
        _ => {
            let base = "https://downloads.claude.ai/releases/win32/x64";
            let list = get(&format!("{base}/RELEASES"))?;
            let name = list.lines().filter_map(|l| l.split_whitespace().nth(1)).filter(|n| n.ends_with("-full.nupkg")).last().context("no full package in the Windows list")?;
            let version = name.strip_prefix("AnthropicClaude-").and_then(|n| n.strip_suffix("-full.nupkg")).context("no version in the Windows package's name")?;
            (version.to_string(), format!("{base}/{name}"))
        }
    };
    if want != "latest" && want != version {
        bail!("the lab only finds the newest Claude desktop, {version}, not {want}");
    }
    Ok((version, url))
}

const CLAUDE_FILE: &str = "/etc/claude-desktop/managed-settings.json";
const CLAUDE_PLIST: &str = "/Library/Managed Preferences/com.anthropic.claudefordesktop.plist";
const CLAUDE_KEY: &str = r"HKLM:\SOFTWARE\Policies\Claude";

/// Claude desktop on the lab model the way an organization sets it up, the only way it runs with no sign-in. Auto mode
/// stays off: it asks the model about each command, which the lab model doesn't answer.
fn claude_policy(url: &str, run: &Path) -> Result<()> {
    let policy = json!({
        "inferenceProvider": "gateway",
        "inferenceGatewayBaseUrl": url,
        "inferenceGatewayApiKey": "lab",
        "inferenceModels": ["claude-sonnet-5"],
        "deploymentMode": "3p",
        "disableAutoUpdates": true,
        "autoModeEnabled": false,
    });
    match OS {
        "linux" => {
            let file = run.join("managed-settings.json");
            fs::write(&file, policy.to_string())?;
            // Root's file in root's folder, or none of it applies.
            output(Command::new("sudo").args(["install", "-D", "-m", "644", "-o", "root", "-g", "root"]).arg(&file).arg(CLAUDE_FILE))?;
        }
        "macos" => {
            let (from, plist) = (run.join("policy.json"), run.join("policy.plist"));
            fs::write(&from, policy.to_string())?;
            output(Command::new("plutil").args(["-convert", "xml1", "-o"]).arg(&plist).arg(&from))?;
            output(Command::new("sudo").args(["mkdir", "-p", "/Library/Managed Preferences"]))?;
            output(Command::new("sudo").args(["install", "-m", "644", "-o", "root", "-g", "wheel"]).arg(&plist).arg(CLAUDE_PLIST))?;
        }
        _ => {
            // It reads text and numbers only: a list as its JSON text, a switch as 0 or 1.
            let mut ps = format!("New-Item -Path '{CLAUDE_KEY}' -Force | Out-Null; ");
            for (k, v) in policy.as_object().into_iter().flatten() {
                let (value, kind) = match v {
                    Value::Bool(b) => (u8::from(*b).to_string(), "DWord"),
                    Value::String(s) => (format!("'{s}'"), "String"),
                    other => (format!("'{other}'"), "String"),
                };
                ps += &format!("New-ItemProperty -Path '{CLAUDE_KEY}' -Name {k} -Value {value} -PropertyType {kind} -Force | Out-Null; ");
            }
            output(Command::new("powershell").args(["-NoProfile", "-Command", &ps]))?;
        }
    }
    Ok(())
}

fn claude_policy_off() {
    let _ = match OS {
        "linux" => Command::new("sudo").args(["rm", "-f", CLAUDE_FILE]).output(),
        "macos" => Command::new("sudo").args(["rm", "-f", CLAUDE_PLIST]).output(),
        _ => Command::new("powershell").args(["-NoProfile", "-Command", &format!("Remove-Item -Path '{CLAUDE_KEY}' -Recurse -Force")]).output(),
    };
}

/// Claude desktop's first run: a welcome saying the organization set it up, then the link again (the first one came
/// before it was ready) and its question whether to trust the folder. Each time the button wanted is the second one.
fn claude_start(person: &Person, exe: &Path, link: &str, home: &Path, run: &Path) -> Result<()> {
    // The keys go in blind, so what each one landed on is kept.
    screenshot(&run.join("start0.png"));
    person.tab()?;
    person.enter()?;
    sleep(Duration::from_secs(5));
    screenshot(&run.join("start1.png"));
    if OS == "macos" {
        // A Mac app takes links from the system, not from a second copy of itself.
        let bundle = exe.ancestors().nth(3).context("the app is in no bundle")?;
        output(Command::new("open").arg("-a").arg(bundle).arg(link))?;
    } else {
        // A second copy hands its link to the first and quits.
        let log = OpenOptions::new().create(true).append(true).open(run.join("app.log"))?;
        let mut again = Command::new(exe);
        again.arg(link).env("HOME", home).env("USERPROFILE", home).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
        // With the runner's XDG_CONFIG_HOME it would look for the first copy in another settings folder and start anew.
        for k in crate::STRAY_ENV {
            again.env_remove(k);
        }
        if OS == "linux" {
            again.args(["--no-sandbox", "--disable-gpu"]);
        }
        again.spawn()?;
    }
    sleep(Duration::from_secs(6));
    screenshot(&run.join("start2.png"));
    person.tab()?;
    person.enter()?;
    sleep(Duration::from_secs(3));
    screenshot(&run.join("start3.png"));
    Ok(())
}

/// The whole screen, where the system lets the lab take it (not on Windows, and macOS needs screen recording allowed).
fn screenshot(to: &Path) {
    let _ = match OS {
        "macos" => Command::new("screencapture").arg("-x").arg(to).status(),
        "linux" => Command::new("import").args(["-window", "root"]).arg(to).status(),
        _ => return,
    };
}

/// The Codex app's first run: what kind of work and a setup to skip. `--open-project` lists the project, and a new chat
/// starts in it from its button there. Each screen it shows and what the lab clicked there are kept in the run folder.
fn codex_start(page: &mut Page, run: &Path, project: &str) -> Result<()> {
    sleep(Duration::from_secs(30));
    let mut log = String::new();
    for step in 0..10 {
        if let Ok(shot) = page.call("Page.captureScreenshot", json!({ "format": "png" })) {
            fs::write(run.join(format!("shot{step}.b64")), shot["data"].as_str().unwrap_or_default())?;
        }
        let ax = page.ax()?;
        let lines: Vec<String> = ax.iter().map(|(r, n, _)| format!("{r}: {}", n.chars().take(120).collect::<String>())).collect();
        fs::write(run.join(format!("ax{step}.txt")), lines.join("\n"))?;
        let has = |roles: &[&str], name: &str| ax.iter().any(|(r, n, _)| roles.contains(&r.as_str()) && n == name);
        let mut did = Vec::new();
        // Its setup asks what kind of work, then thinks, and lets you skip to the app.
        if has(&["radio"], "Engineering") && has(&["button"], "Continue") && page.ax_click(&["radio"], "Engineering")? {
            did.push("Engineering");
            sleep(Duration::from_secs(1));
        }
        let ways = ["Go to ChatGPT", "Skip", "Continue", "Continue without administrator access"];
        let roles = ["button", "link", "tab", "menuitem", "menuitemradio", "option", "radio"];
        if let Some(name) = ways.into_iter().find(|n| has(&roles, n)) {
            page.ax_click(&roles, name)?;
            did.push(name);
        }
        log += &format!("step {step}: clicked {did:?}\n");
        if did.is_empty() {
            fs::write(run.join("steps.txt"), &log)?;
            if !ax.iter().any(|(r, _, _)| r == "textbox") {
                bail!("the Codex app shows no message box:\n{log}");
            }
            let new = format!("Start new chat in {project}");
            if !page.ax_click(&["button"], &new)? {
                bail!("the Codex app lists no {project}:\n{log}");
            }
            sleep(Duration::from_secs(3));
            let unset = |page: &mut Page| -> Result<bool> { Ok(page.ax()?.iter().any(|(r, n, _)| r == "button" && n == "Choose project")) };
            // The sidebar button can leave the message box without a project; its own picker sets one.
            if unset(&mut *page)? && page.ax_click(&["button"], "Choose project")? {
                sleep(Duration::from_secs(1));
                let picked = page.ax_click(&["menuitem", "menuitemradio", "option", "listitem", "radio"], project)?;
                log += &format!("picked {project} in the message box: {picked}\n");
                sleep(Duration::from_secs(2));
            }
            // On Windows the app ignores config.toml's full access and runs commands in its own sandbox, where .git is
            // read only, so the script's commits fail.
            let full = |page: &mut Page| -> Result<bool> { Ok(page.ax()?.iter().any(|(r, n, _)| r == "StaticText" && n == "Full access")) };
            if !full(&mut *page)? && page.ax_click(&["button"], "Change permissions")? {
                sleep(Duration::from_secs(1));
                let ax = page.ax()?;
                let lines: Vec<String> = ax.iter().map(|(r, n, _)| format!("{r}: {}", n.chars().take(120).collect::<String>())).collect();
                fs::write(run.join("ax-perms.txt"), lines.join("\n"))?;
                let items = ["menuitem", "menuitemradio", "menuitemcheckbox", "option", "radio", "button"];
                let name = ax.iter().find(|(r, n, _)| items.contains(&r.as_str()) && n.starts_with("Full access")).map(|x| x.1.clone());
                let picked = match name {
                    Some(name) => page.ax_click(&items, &name)?,
                    None => false,
                };
                sleep(Duration::from_secs(2));
                let ax = page.ax()?;
                let lines: Vec<String> = ax.iter().map(|(r, n, _)| format!("{r}: {}", n.chars().take(120).collect::<String>())).collect();
                fs::write(run.join("ax-perms2.txt"), lines.join("\n"))?;
                if ax.iter().any(|(r, _, _)| r == "dialog" || r == "alertdialog") {
                    for ok in ["Continue", "Enable", "Turn on", "Confirm", "Allow", "Yes"] {
                        if page.ax_click(&["button"], ok)? {
                            log += &format!("confirmed full access with {ok:?}\n");
                            sleep(Duration::from_secs(2));
                            break;
                        }
                    }
                }
                log += &format!("picked full access: {picked}, now full: {}\n", full(&mut *page)?);
            }
            let ax = page.ax()?;
            let lines: Vec<String> = ax.iter().map(|(r, n, _)| format!("{r}: {}", n.chars().take(120).collect::<String>())).collect();
            fs::write(run.join("ax-chat.txt"), lines.join("\n"))?;
            fs::write(run.join("steps.txt"), &log)?;
            if unset(&mut *page)? {
                bail!("the new chat in the Codex app is in no project");
            }
            return Ok(());
        }
        sleep(Duration::from_secs(5));
    }
    fs::write(run.join("steps.txt"), &log)?;
    bail!("the Codex app kept asking:\n{log}")
}

struct Page {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    next: u64,
}

impl Page {
    fn open(port: u16, within: Duration) -> Result<Page> {
        Page::open_where(port, within, |t| t["type"] == "page")
    }

    /// The first of the app's pages and frames that `fits`.
    fn open_where(port: u16, within: Duration, fits: impl Fn(&Value) -> bool) -> Result<Page> {
        let until = Instant::now() + within;
        loop {
            let list = Command::new("curl").args(["-fsS", "-m", "5", &format!("http://127.0.0.1:{port}/json")]).output();
            let page = list.ok().and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok()).and_then(|v| {
                v.as_array()?.iter().find(|t| fits(t))?["webSocketDebuggerUrl"].as_str().map(str::to_string)
            });
            if let Some(url) = page {
                let (ws, _) = tungstenite::connect(url.as_str()).context("the app's debugging port")?;
                // A page that stops answering fails the run instead of holding the machine.
                if let MaybeTlsStream::Plain(s) = ws.get_ref() {
                    s.set_read_timeout(Some(Duration::from_secs(60)))?;
                }
                return Ok(Page { ws, next: 0 });
            }
            if Instant::now() > until {
                bail!("the app never opened a page on port {port}");
            }
            sleep(Duration::from_millis(500));
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next += 1;
        let id = self.next;
        self.ws.send(Message::Text(json!({ "id": id, "method": method, "params": params }).to_string()))?;
        loop {
            if let Message::Text(t) = self.ws.read().with_context(|| format!("the app never answered {method}"))? {
                let v: Value = serde_json::from_str(&t)?;
                if v["id"] == id {
                    if let Some(e) = v.get("error") {
                        bail!("{method}: {e}");
                    }
                    return Ok(v["result"].clone());
                }
            }
        }
    }

    fn eval(&mut self, expr: &str) -> Result<Value> {
        let r = self.call("Runtime.evaluate", json!({ "expression": expr, "returnByValue": true, "awaitPromise": true }))?;
        Ok(r["result"]["value"].clone())
    }

    fn text(&mut self) -> Result<String> {
        Ok(self.eval("document.body ? document.body.innerText : ''")?.as_str().unwrap_or_default().to_string())
    }

    /// Waits for any of the texts on the page and gives back the page.
    fn wait_for(&mut self, any: &[&str], within: Duration) -> Result<String> {
        let until = Instant::now() + within;
        loop {
            let text = self.text()?;
            if any.iter().any(|w| text.contains(w)) {
                return Ok(text);
            }
            if Instant::now() > until {
                bail!("the app never showed {any:?}. Page:\n{text}");
            }
            sleep(Duration::from_millis(300));
        }
    }

    /// A left click in the middle of an element, found by a script that gives back its box.
    fn click_at(&mut self, find: &str, what: &str) -> Result<()> {
        let at = self.eval(&format!("(() => {{ const e = {find}; if (!e) return null; e.scrollIntoView({{ block: 'center' }}); const r = e.getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height / 2]; }})()"))?;
        let (Some(x), Some(y)) = (at[0].as_f64(), at[1].as_f64()) else { bail!("nothing to click for {what:?}. Page:\n{}", self.text()?) };
        self.call("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y }))?;
        self.call("Input.dispatchMouseEvent", json!({ "type": "mousePressed", "x": x, "y": y, "button": "left", "clickCount": 1 }))?;
        sleep(Duration::from_millis(60));
        self.call("Input.dispatchMouseEvent", json!({ "type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": 1 }))?;
        Ok(())
    }

    /// Clicks the smallest visible element whose own text is `label`.
    fn click(&mut self, label: &str) -> Result<()> {
        let find = format!(
            "[...document.querySelectorAll('button, a, [role=button], [role=menuitem], [role=option], div, span')]
               .filter(e => {{ const r = e.getBoundingClientRect(); return r.width > 0 && r.height > 0 && (e.innerText || '').trim() === {label:?}; }})
               .sort((a, b) => a.getBoundingClientRect().width * a.getBoundingClientRect().height - b.getBoundingClientRect().width * b.getBoundingClientRect().height)[0]"
        );
        self.click_at(&find, label)
    }

    /// Clicks `label` until `next` shows, the way a person clicks again when the app was still busy.
    fn press(&mut self, label: &str, next: &str) -> Result<()> {
        for _ in 0..6 {
            self.click(label)?;
            if self.wait_for(&[next], Duration::from_secs(10)).is_ok() {
                return Ok(());
            }
        }
        bail!("clicking {label:?} never brought up {next:?}. Page:\n{}", self.text()?)
    }

    /// What the page shows a screen reader: role and name of each named node, with its node for clicking.
    fn ax(&mut self) -> Result<Vec<(String, String, Option<u64>)>> {
        let tree = self.call("Accessibility.getFullAXTree", json!({}))?;
        let nodes = tree["nodes"].as_array().cloned().unwrap_or_default();
        Ok(nodes
            .iter()
            .filter(|n| n["ignored"] != json!(true))
            .filter_map(|n| {
                let name = n["name"]["value"].as_str().unwrap_or_default().trim().to_string();
                (!name.is_empty()).then(|| (n["role"]["value"].as_str().unwrap_or_default().to_string(), name, n["backendDOMNodeId"].as_u64()))
            })
            .collect())
    }

    /// A left click in the middle of the first node a screen reader names `name`, of one of `roles`.
    fn ax_click(&mut self, roles: &[&str], name: &str) -> Result<bool> {
        let Some(id) = self.ax()?.into_iter().find(|(r, n, id)| roles.contains(&r.as_str()) && n == name && id.is_some()).and_then(|x| x.2) else {
            return Ok(false);
        };
        let quad = self.call("DOM.getBoxModel", json!({ "backendNodeId": id }))?["model"]["content"].clone();
        let q: Vec<f64> = quad.as_array().context("no box")?.iter().filter_map(Value::as_f64).collect();
        let (x, y) = ((q[0] + q[2] + q[4] + q[6]) / 4.0, (q[1] + q[3] + q[5] + q[7]) / 4.0);
        self.call("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y }))?;
        self.call("Input.dispatchMouseEvent", json!({ "type": "mousePressed", "x": x, "y": y, "button": "left", "clickCount": 1 }))?;
        sleep(Duration::from_millis(60));
        self.call("Input.dispatchMouseEvent", json!({ "type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": 1 }))?;
        Ok(true)
    }

    fn type_text(&mut self, text: &str) -> Result<()> {
        for c in text.chars() {
            let s = c.to_string();
            self.call("Input.dispatchKeyEvent", json!({ "type": "keyDown", "key": s, "text": s, "unmodifiedText": s }))?;
            self.call("Input.dispatchKeyEvent", json!({ "type": "keyUp", "key": s }))?;
            sleep(Duration::from_millis(25));
        }
        Ok(())
    }

    fn key(&mut self, key: &str) -> Result<()> {
        let code = match key {
            "Enter" => 13,
            "Backspace" => 8,
            "Escape" => 27,
            _ => bail!("the lab has no key {key}"),
        };
        let mut down = json!({ "type": "keyDown", "key": key, "code": key, "windowsVirtualKeyCode": code });
        if key == "Enter" {
            down["text"] = json!("\r");
        }
        self.call("Input.dispatchKeyEvent", down)?;
        self.call("Input.dispatchKeyEvent", json!({ "type": "keyUp", "key": key, "code": key, "windowsVirtualKeyCode": code }))?;
        Ok(())
    }
}

/// T3 Code's first run: a welcome page, then a project from a folder on disk, then a thread's message box.
fn t3_start(page: &mut Page, project: &Path, pick: Option<&Pick>) -> Result<()> {
    // Each run starts from a fresh home, so the welcome page always comes, sometimes a moment after the app's frame.
    page.wait_for(&["Welcome to T3 Code"], Duration::from_secs(90))?;
    if let Some(p) = pick.filter(|p| p.off) {
        // The list of agents comes once the app has looked for them, the ones it didn't find folded away below.
        let find = format!("document.querySelector('[aria-label=\"Turn on: {}\"]')", p.name);
        let until = Instant::now() + Duration::from_secs(60);
        let mut unfolded = false;
        while page.eval(&format!("!!{find}"))? != json!(true) && Instant::now() < until {
            let text = page.text()?;
            if !unfolded && !text.contains("Checking") {
                if let Some(more) = ["Add more agents", "Set up an agent"].into_iter().find(|m| text.contains(m)) {
                    page.click(more)?;
                    unfolded = true;
                }
            }
            sleep(Duration::from_millis(500));
        }
        // Clicked again while the list is still unfolding, until the button gives way to the agent's state.
        for _ in 0..6 {
            page.click_at(&find, &format!("Turn on {}", p.name))?;
            let until = Instant::now() + Duration::from_secs(10);
            while page.eval(&format!("!!{find}"))? == json!(true) && Instant::now() < until {
                sleep(Duration::from_millis(500));
            }
            if page.eval(&format!("!!{find}"))? != json!(true) {
                break;
            }
        }
    }
    // Start stays greyed out until an agent is ready, which takes the agent's own server a moment.
    page.press("Start", "Add project")?;
    page.press("Add project", "Local folder")?;
    page.press("Local folder", "Directories")?;
    sleep(Duration::from_secs(1));
    // The folder box starts at ~/.
    page.key("Backspace")?;
    page.key("Backspace")?;
    page.type_text(&project.to_string_lossy())?;
    sleep(Duration::from_secs(1));
    page.key("Enter")?;
    page.wait_for(&["Ask anything"], Duration::from_secs(30))?;
    sleep(Duration::from_secs(2));
    if let Some(p) = pick {
        // The model button under the message box, the agent on the picker's side, then the model by name.
        page.click_at("document.querySelector('[data-chat-provider-model-picker=\"true\"]')", "the model picker")?;
        sleep(Duration::from_secs(1));
        page.click_at(&format!("document.querySelector('[data-model-picker-provider=\"{}\"]')", p.id), p.name)?;
        sleep(Duration::from_secs(1));
        // Codex sends every model it lists to the lab's provider. OpenCode also offers its own hosted models, which
        // must never get the lab's prompts.
        if let Some(model) = p.model {
            page.type_text(model)?;
            sleep(Duration::from_secs(1));
        }
        page.click_at("document.querySelector('[data-model-picker-content] [role=option]:not([aria-expanded])')", &format!("a {} model", p.name))?;
        sleep(Duration::from_secs(1));
        let shown = page.eval("(document.querySelector('[data-chat-provider-model-picker=\"true\"]') || {}).innerText || ''")?;
        if let Some(model) = p.model.filter(|m| !shown.as_str().is_some_and(|s| s.contains(m))) {
            bail!("picked {model} but the model button says {shown}. Page:\n{}", page.text()?);
        }
        // Picking a model moves on to its reasoning levels, the default is fine.
        page.key("Escape")?;
        sleep(Duration::from_secs(1));
    }
    Ok(())
}

/// One turn as a person takes it: reading what came before and typing, hands off while the agent works.
struct Turns<'a> {
    model: &'a Model,
    person: &'a Person,
    restart: &'a mut dyn FnMut() -> Result<()>,
}

impl Turns<'_> {
    fn take(&mut self, i: usize, turn: &scenario::Turn, mut send: impl FnMut(&str) -> Result<()>, page: impl Fn() -> String) -> Result<()> {
        if turn.restart {
            self.person.busy(false);
            sleep(Duration::from_secs(6));
            (self.restart)()?;
            sleep(Duration::from_secs(4));
        }
        self.person.busy(true);
        self.model.arm(i);
        send(turn.prompt)?;
        self.person.busy(false);
        let until = Instant::now() + Duration::from_secs(90);
        while !self.model.said(i) {
            if Instant::now() > until {
                bail!("turn {} never finished. Page:\n{}", i + 1, page());
            }
            sleep(Duration::from_millis(200));
        }
        self.person.busy(true);
        sleep(Duration::from_secs(4));
        Ok(())
    }
}

/// The Antigravity IDE's first run: a welcome over the editor and the agent's terms in its panel, then its message box.
fn antigravity_start(port: u16) -> Result<Page> {
    let mut editor = Page::open(port, Duration::from_secs(90))?;
    let welcome = "document.querySelector('.antigravity-ide-welcome-button')";
    let until = Instant::now() + Duration::from_secs(60);
    while editor.eval(&format!("!!{welcome}"))? != json!(true) {
        if Instant::now() > until {
            bail!("the editor never welcomed. Page:\n{}", editor.text()?);
        }
        sleep(Duration::from_millis(500));
    }
    editor.click_at(welcome, "Get Started")?;
    // The panel is a page of agy's own, framed in the editor. Its settings come up in a frame like it.
    let mut panel = Page::open_where(port, Duration::from_secs(90), |t| {
        t["url"].as_str().is_some_and(|u| u.contains("/?extensionView=true"))
    })?;
    if panel.wait_for(&["Get started", "Ask anything"], Duration::from_secs(90))?.contains("Get started") {
        panel.press("Get started", "Ask anything")?;
    }
    sleep(Duration::from_secs(2));
    Ok(panel)
}

/// A person who lets the agent run commands on its own picks Always Proceed. Its default runs them in a sandbox,
/// which needs a kernel the lab's machines don't all have. agy itself comes with the editor's first run.
fn antigravity_setup(home: &Path, agy: &Path) -> Result<()> {
    let bin = home.join(".gemini").join("bin");
    fs::create_dir_all(&bin)?;
    fs::copy(agy, bin.join(agy.file_name().context("agy has no name")?))?;
    let config = home.join(".gemini").join("config");
    fs::create_dir_all(&config)?;
    let settings = json!({ "userSettings": {
        "enableTerminalSandbox": false,
        "permissionPreset": "AGENT_PERMISSION_PRESET_TURBO",
        "autoExecutionPolicy": "CASCADE_COMMANDS_AUTO_EXECUTION_EAGER",
    } });
    fs::write(config.join("config.json"), serde_json::to_string_pretty(&settings)?)?;
    Ok(())
}

const COMPOSER: &str = "(() => { const shown = q => [...document.querySelectorAll(q)].find(e => e.getBoundingClientRect().width > 0); \
                        return shown('[contenteditable=true]') ?? shown('textarea'); })()";

/// VS Code's message box is an editor whose own text area is a sliver at the cursor.
const VSCODE_COMPOSER: &str = "[...document.querySelectorAll('.interactive-input-editor')].find(e => e.getBoundingClientRect().width > 0)";

fn drive(page: &mut Page, composer: &str, scenario: &Scenario, turns: &mut Turns) -> Result<()> {
    let page = std::cell::RefCell::new(page);
    for (i, turn) in scenario.turns.iter().enumerate() {
        let send = |prompt: &str| -> Result<()> {
            let mut page = page.borrow_mut();
            page.click_at(composer, "the message box")?;
            page.type_text(prompt)?;
            sleep(Duration::from_millis(400));
            page.key("Enter")
        };
        turns.take(i, turn, send, || page.borrow_mut().text().unwrap_or_default())?;
    }
    Ok(())
}

/// The app is in front once Kairos says so, and after `start` the project the link opened has its message box in focus.
fn type_in(scenario: &Scenario, kd: &Path, turns: &mut Turns, start: impl FnOnce() -> Result<()>) -> Result<()> {
    let until = Instant::now() + Duration::from_secs(90);
    while !crate::events(kd)?.iter().any(|e| e["kind"] == "focus" && e["class"] == "agent_app") {
        if Instant::now() > until {
            bail!("the app's window never came to the front");
        }
        sleep(Duration::from_millis(500));
    }
    sleep(Duration::from_secs(8));
    start()?;
    let person = turns.person.clone();
    for (i, turn) in scenario.turns.iter().enumerate() {
        let send = |prompt: &str| -> Result<()> {
            person.type_text(prompt)?;
            sleep(Duration::from_millis(400));
            person.enter()
        };
        turns.take(i, turn, send, String::new)?;
    }
    Ok(())
}

/// The app's own logs for a run that failed: every small .log file under `dir`, kept in the run folder (whose upload
/// leaves out the home).
fn keep_logs(dir: &Path, run: &Path) {
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => todo.push(path),
                Ok(t) if t.is_file() && path.extension().is_some_and(|x| x == "log") && e.metadata().is_ok_and(|m| m.len() < 2_000_000) => {
                    let name = path.strip_prefix(dir).unwrap_or(&path).to_string_lossy().replace(['/', '\\'], "_");
                    let _ = fs::create_dir_all(run.join("logs"));
                    let _ = fs::copy(&path, run.join("logs").join(name));
                }
                _ => {}
            }
        }
    }
}

/// A prompt the app's log says was typed in it only counts as typed there when Kairos saw that app's window in front.
fn typed_in(all: &[Value], harness: &str) -> Vec<String> {
    let focus: Vec<(f64, &Value)> = all.iter().filter(|e| e["kind"] == "focus").map(|e| (e["ts"].as_f64().unwrap_or(0.0), &e["app"])).collect();
    all.iter()
        .filter(|e| e["harness"] == harness && e["kind"] == "prompt" && !e["via"].is_null())
        .filter_map(|e| {
            let ts = e["ts"].as_f64()?;
            let front = focus.iter().rev().find(|(t, _)| *t <= ts).map_or(&Value::Null, |(_, app)| *app);
            (*front != e["via"]).then(|| format!("a prompt typed in {} came with app {front} in front", e["via"]))
        })
        .collect()
}

pub fn run(app: &App, args: &Args) -> Result<bool> {
    if matches!(app.source, Source::Codex) && OS == "linux" {
        println!("SKIP {}: no Linux build", app.id);
        return Ok(true);
    }
    if app.id == "claude-desktop" && OS == "macos" {
        // It stops right after logging "Starting app" on GitHub's Mac runners, cause not found yet (2026-11-27).
        println!("SKIP {}: hangs at startup on the Mac runner", app.id);
        return Ok(true);
    }
    let tools = std::path::absolute(&args.work)?.join("tools");
    let (exe, version) = app.install(&args.version, &tools)?;
    let h = harness::find(app.harness)?;
    // People who use the app have the harness too, and the app asks it whether it is signed in.
    let (cli, _) = if matches!(app.source, Source::VsCode) { (exe.clone(), String::new()) } else { h.install("latest", &tools)? };
    let p = crate::place(args, app.id, &version, app.state)?;
    let scenario = Arc::new(scenario::app());
    let model = Model::start(scenario.clone(), p.project.clone())?;
    let url = format!("http://127.0.0.1:{}", model.port);
    eprintln!("lab: {} {version}, scenario {}, run folder {}", app.id, scenario.name, p.run.display());

    // A macOS app finds Application Support from the account, not $HOME, so OpenCode desktop writes its open tabs under
    // the runner's own home while kairosd, on the run's home, looks in the run's. On a real Mac both are one folder.
    #[cfg(target_os = "macos")]
    if app.id == "opencode-desktop" {
        let support = |home: &Path| home.join("Library").join("Application Support");
        let own = PathBuf::from(std::env::var_os("HOME").context("no HOME")?);
        let real = support(own.as_path()).join("ai.opencode.desktop");
        std::fs::create_dir_all(&real)?;
        std::fs::create_dir_all(support(p.home.as_path()))?;
        std::os::unix::fs::symlink(&real, support(p.home.as_path()).join("ai.opencode.desktop"))?;
    }
    let person = Person::new(args.window)?;
    let mut daemon = crate::daemon(args, &p)?;
    sleep(Duration::from_secs(2));
    let mut cmd = Command::new(&exe);
    let launch = if matches!(app.source, Source::VsCode) {
        let (data, named) = vscode_data(&p.home)?;
        vscode_setup(&data, &url)?;
        hackatime(&exe, &args.kairosd, &p)?;
        if named {
            cmd.arg(format!("--user-data-dir={}", data.display()));
        }
        harness::Launch::default()
    } else {
        (h.prepare)(&p.home, &p.project, &url, "latest")?
    };
    let log = || OpenOptions::new().create(true).append(true).open(p.run.join("app.log"));
    match app.drive {
        // The Codex app opens a new thread in the folder after --open-project.
        Drive::Page if matches!(app.source, Source::Codex) => cmd.arg(format!("--remote-debugging-port={PORT}")).arg("--open-project").arg(&p.project),
        Drive::Page => cmd.arg(format!("--remote-debugging-port={PORT}")),
        // The project the way `antigravity-ide .` opens it. Basic keeps Linux from asking which keyring to use.
        Drive::Panel | Drive::Chat => cmd.arg(format!("--remote-debugging-port={PORT}")).args(["--disable-workspace-trust", "--password-store=basic"]).arg(&p.project),
        Drive::Keys(link) => cmd.arg(link(&p.project)),
    };
    if app.id == "antigravity-ide" {
        antigravity_setup(&p.home, &cli)?;
        if OS == "macos" {
            // VS Code's socket in its settings folder can't take a path longer than 103 characters, and the run's home
            // is deep in the runner's work folder.
            let short = PathBuf::from(format!("/tmp/lab-ide-{}", std::process::id()));
            let _ = fs::remove_dir_all(&short);
            cmd.arg(format!("--user-data-dir={}", short.display()));
        }
    }
    if OS == "linux" {
        // An unpacked .deb has no setuid sandbox helper, and Xvfb has no GPU.
        cmd.args(["--no-sandbox", "--disable-gpu"]);
    }
    if OS == "macos" {
        // The run's own home has no keychain, and the app's main process waits on a keychain dialog nobody answers.
        cmd.arg("--use-mock-keychain");
    }
    cmd.current_dir(&p.project).env("HOME", &p.home).env("USERPROFILE", &p.home).stdin(Stdio::null()).stdout(log()?).stderr(log()?);
    for k in STRAY_ENV {
        cmd.env_remove(k);
    }
    for (k, v) in &launch.env {
        cmd.env(k, v);
    }
    if app.id == "claude-desktop" {
        claude_policy(&url, &p.run)?;
    }
    let bin = cli.parent().context("the harness has no folder")?.to_path_buf();
    let bin_dir = bin.clone();
    // On Windows an app can't run npm's claude.cmd without a shell. T3 looks for the package next to the shim, the way
    // a global npm install lays it out, so the program itself goes on PATH first.
    let native = bin.parent().map(|m| m.join("@anthropic-ai").join("claude-code").join("bin")).filter(|d| cfg!(windows) && d.join("claude.exe").is_file());
    let path = std::env::join_paths(native.into_iter().chain([bin]).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())))?;
    cmd.env("PATH", path);
    let mut child = cmd.spawn().with_context(|| format!("starting {}", exe.display()))?;

    let mut restart = || -> Result<()> {
        let _ = daemon.kill();
        let _ = daemon.wait();
        daemon = crate::daemon(args, &p)?;
        Ok(())
    };
    let mut turns = Turns { model: &model, person: &person, restart: &mut restart };
    let start = |page: &mut Page| match app.source {
        Source::Codex => codex_start(page, &p.run, &p.project.file_name().unwrap_or_default().to_string_lossy()),
        _ => t3_start(page, &p.project, app.pick.as_ref()),
    };
    let drove = match app.drive {
        Drive::Page => Page::open(PORT, Duration::from_secs(90)).and_then(|mut page| {
            let r = start(&mut page).and_then(|()| drive(&mut page, COMPOSER, &scenario, &mut turns));
            if r.is_err() {
                let _ = fs::write(p.run.join("page.txt"), page.text().unwrap_or_default());
            }
            r
        }),
        Drive::Panel => antigravity_start(PORT).and_then(|mut page| {
            let r = drive(&mut page, COMPOSER, &scenario, &mut turns);
            if r.is_err() {
                let _ = fs::write(p.run.join("page.txt"), page.text().unwrap_or_default());
            }
            r
        }),
        Drive::Chat => vscode_start(PORT).and_then(|mut page| {
            let r = drive(&mut page, VSCODE_COMPOSER, &scenario, &mut turns);
            if r.is_err() {
                let _ = fs::write(p.run.join("page.txt"), page.text().unwrap_or_default());
            }
            r
        }),
        Drive::Keys(link) => type_in(&scenario, &p.kd, &mut turns, || match app.id {
            "claude-desktop" => claude_start(&person, &exe, &link(&p.project), &p.home, &p.run),
            _ => Ok(()),
        }),
    };
    // Readers look every 3 s and saves are batched for 2 s.
    sleep(Duration::from_secs(8));
    if drove.is_ok() && args.window {
        // Still reading the last reply until kairosd writes down the minute.
        let (from, until) = (crate::now(), Instant::now() + Duration::from_secs(65));
        while Instant::now() < until && !crate::events(&p.kd)?.iter().any(|e| e["kind"] == "present" && e["ts"].as_f64().is_some_and(|t| t > from)) {
            sleep(Duration::from_secs(1));
        }
    }
    if drove.is_err() && !cfg!(windows) {
        // What was on screen and what was still running when the lab gave up.
        screenshot(&p.run.join("screen.png"));
        if let Ok(ps) = Command::new("ps").args(["-A", "-o", "pid,stat,etime,args"]).output() {
            let _ = fs::write(p.run.join("ps.txt"), ps.stdout);
        }
    }
    person.busy(false);
    // Electron's helpers and the harness it started all run from the app's own folder.
    let folder = exe.parent().context("the app has no folder")?;
    if cfg!(windows) {
        let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &child.id().to_string()]).output();
    } else {
        let _ = Command::new("pkill").arg("-f").arg(folder).status();
        // The agent's server the app started from the harness's own folder, or agy from the run's home.
        let _ = Command::new("pkill").arg("-f").arg(&bin_dir).status();
        let _ = Command::new("pkill").arg("-f").arg(p.home.join(".gemini").join("bin")).status();
        // The Claude Code Claude desktop downloaded for itself.
        let _ = Command::new("pkill").arg("-f").arg("Claude-3p/claude-code").status();
    }
    if app.id == "claude-desktop" {
        claude_policy_off();
    }
    let _ = child.kill();
    let _ = child.wait();
    if !launch.after.is_empty() {
        // The app's server outlives its window, like the CLI's.
        let _ = Command::new(&cli).args(&launch.after).current_dir(&p.project).env("HOME", &p.home).env("USERPROFILE", &p.home).stdin(Stdio::null()).output();
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
    let requests: String = model.requests().iter().map(|r| format!("{r}\n")).collect();
    fs::write(p.run.join("requests.jsonl"), requests)?;
    crate::keep(&p, app.stores)?;
    if drove.is_err() {
        keep_logs(&p.home, &p.run);
        // A macOS app logs under the account's own Library, not $HOME.
        if let Some(own) = std::env::var_os("HOME").filter(|_| OS == "macos") {
            keep_logs(&PathBuf::from(own).join("Library").join("Logs"), &p.run);
        }
    }
    drove?;

    let all = crate::events(&p.kd)?;
    let mine: Vec<Value> = all.iter().filter(|e| e["harness"] == h.id).cloned().collect();
    let mut problems = model.problems();
    problems.extend(crate::check(h, None, &scenario, &[], &all, &p.project)?);
    let mut summary = None;
    if args.window {
        let window = if matches!(app.drive, Drive::Chat) { "editor" } else { "agent_app" };
        problems.extend(crate::front(&all, h.id, window, &[]));
        problems.extend(typed_in(&all, h.id));
        let (wrong, said) = credit::check(&p.kd, h.id, "proj", &person, &[])?;
        problems.extend(wrong);
        summary = Some(said);
    }
    let passed = crate::report(app.id, &version, &scenario, &crate::kairosd_version(&args.kairosd)?, &mine, &problems);
    if let Some(said) = summary {
        println!("  {said}");
    }
    Ok(passed)
}
