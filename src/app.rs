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
    repo: &'static str,
    /// The harness it runs, which Kairos names in its events.
    harness: &'static str,
    /// What it keeps in the home folder, removed before a run in a real home.
    state: &'static [&'static str],
    stores: &'static [&'static str],
    /// The release file for this system, from the version.
    asset: fn(&str) -> String,
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

enum Drive {
    /// Over the app's debugging port.
    Page,
    /// The app opens the project from a link and its message box takes the keys.
    Keys,
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
        repo: "pingdotgg/t3code",
        harness: "claude-code",
        state: &[".t3", ".claude", ".claude.json"],
        stores: &[".claude/projects", ".t3/userdata/statev2.sqlite", ".t3/userdata/statev2.sqlite-wal", ".t3/userdata/logs"],
        asset: t3_asset,
        exe: t3_exe,
        drive: Drive::Page,
        pick: None,
    },
    // T3 starts `codex app-server` itself, on the same ~/.codex.
    App {
        id: "t3code-codex",
        repo: "pingdotgg/t3code",
        harness: "codex",
        state: &[".t3", ".codex"],
        stores: &[".codex/sessions", ".t3/userdata/statev2.sqlite", ".t3/userdata/statev2.sqlite-wal", ".t3/userdata/logs"],
        asset: t3_asset,
        exe: t3_exe,
        drive: Drive::Page,
        pick: Some(Pick { name: "Codex", id: "codex", model: None, off: false }),
    },
    // T3 starts `opencode serve` itself, on the same config and database.
    App {
        id: "t3code-opencode",
        repo: "pingdotgg/t3code",
        harness: "opencode",
        state: &[".t3", ".config/opencode", ".local/share/opencode", ".local/state/opencode"],
        stores: &[".local/share/opencode/opencode.db", ".local/share/opencode/opencode.db-wal", ".t3/userdata/statev2.sqlite", ".t3/userdata/statev2.sqlite-wal", ".t3/userdata/logs"],
        asset: t3_asset,
        exe: t3_exe,
        drive: Drive::Page,
        pick: Some(Pick { name: "OpenCode", id: "opencode", model: Some("test-model"), off: true }),
    },
    // Stable builds drop --remote-debugging-port. Its own state is in the system's app data folder, which on Windows
    // isn't under the run's home.
    App {
        id: "opencode-desktop",
        repo: "anomalyco/opencode",
        harness: "opencode",
        state: &[".config/opencode", ".config/ai.opencode.desktop", ".local/share/opencode", ".local/state/opencode"],
        stores: &[".local/share/opencode/opencode.db", ".local/share/opencode/opencode.db-wal", ".config/ai.opencode.desktop/drafts.sqlite", ".config/ai.opencode.desktop/opencode.settings"],
        asset: |_| match (OS, ARCH) {
            ("linux", _) => "opencode-desktop-linux-amd64.deb".to_string(),
            ("macos", "aarch64") => "opencode-desktop-mac-arm64.zip".to_string(),
            ("macos", _) => "opencode-desktop-mac-x64.zip".to_string(),
            _ => "opencode-desktop-win-x64.exe".to_string(),
        },
        exe: |name| if OS == "linux" { name == "ai.opencode" } else { name == "OpenCode.exe" },
        drive: Drive::Keys,
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
        let version = if want == "latest" { newest_release(self.repo, None)? } else { want.to_string() };
        let dir = tools.join(format!("{}-{version}", self.id));
        if let Some(exe) = program(&dir, self.exe) {
            return Ok((exe, version));
        }
        fs::create_dir_all(&dir)?;
        let asset = (self.asset)(&version);
        let file = dir.join(&asset);
        let url = format!("https://github.com/{}/releases/download/v{version}/{asset}", self.repo);
        output(Command::new("curl").args(["-fsSL", "--retry", "3", "-o"]).arg(&file).arg(&url)).with_context(|| url.clone())?;
        match OS {
            "linux" => output(Command::new("dpkg-deb").arg("-x").arg(&file).arg(&dir))?,
            // ditto keeps the links inside the app's frameworks.
            "macos" => output(Command::new("ditto").args(["-x", "-k"]).arg(&file).arg(&dir))?,
            // The installer, silent and into the folder named last.
            _ => output(Command::new(&file).arg("/S").arg(format!("/D={}", dir.join("app").display())))?,
        };
        fs::remove_file(&file)?;
        let exe = program(&dir, self.exe).with_context(|| format!("installed {} {version} but found no program in {}", self.id, dir.display()))?;
        Ok((exe, version))
    }
}

struct Page {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    next: u64,
}

impl Page {
    fn open(port: u16, within: Duration) -> Result<Page> {
        let until = Instant::now() + within;
        loop {
            let list = Command::new("curl").args(["-fsS", "-m", "5", &format!("http://127.0.0.1:{port}/json")]).output();
            let page = list.ok().and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok()).and_then(|v| {
                v.as_array()?.iter().find(|t| t["type"] == "page")?["webSocketDebuggerUrl"].as_str().map(str::to_string)
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

fn drive(page: &mut Page, scenario: &Scenario, project: &Path, pick: Option<&Pick>, turns: &mut Turns) -> Result<()> {
    t3_start(page, project, pick)?;
    let composer = "[...document.querySelectorAll('[contenteditable=true]')].find(e => e.getBoundingClientRect().width > 0)";
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

/// The app is in front once Kairos says so, and the project the link opened has its message box in focus.
fn type_in(scenario: &Scenario, kd: &Path, turns: &mut Turns) -> Result<()> {
    let until = Instant::now() + Duration::from_secs(90);
    while !crate::events(kd)?.iter().any(|e| e["kind"] == "focus" && e["class"] == "agent_app") {
        if Instant::now() > until {
            bail!("the app's window never came to the front");
        }
        sleep(Duration::from_millis(500));
    }
    sleep(Duration::from_secs(8));
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

pub fn run(app: &App, args: &Args) -> Result<bool> {
    let tools = std::path::absolute(&args.work)?.join("tools");
    let (exe, version) = app.install(&args.version, &tools)?;
    let h = harness::find(app.harness)?;
    // People who use the app have the harness too, and the app asks it whether it is signed in.
    let (cli, _) = h.install("latest", &tools)?;
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
    let launch = (h.prepare)(&p.home, &p.project, &url, "latest")?;
    let log = || OpenOptions::new().create(true).append(true).open(p.run.join("app.log"));
    let mut cmd = Command::new(&exe);
    match app.drive {
        Drive::Page => cmd.arg(format!("--remote-debugging-port={PORT}")),
        Drive::Keys => cmd.arg(format!("opencode://open-project?directory={}", p.project.display())),
    };
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
    let drove = match app.drive {
        Drive::Page => Page::open(PORT, Duration::from_secs(90)).and_then(|mut page| {
            let r = drive(&mut page, &scenario, &p.project, app.pick.as_ref(), &mut turns);
            if r.is_err() {
                let _ = fs::write(p.run.join("page.txt"), page.text().unwrap_or_default());
            }
            r
        }),
        Drive::Keys => type_in(&scenario, &p.kd, &mut turns),
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
    person.busy(false);
    // Electron's helpers and the harness it started all run from the app's own folder.
    let folder = exe.parent().context("the app has no folder")?;
    if cfg!(windows) {
        let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &child.id().to_string()]).output();
    } else {
        let _ = Command::new("pkill").arg("-f").arg(folder).status();
        // The agent's server the app started from the harness's own folder.
        let _ = Command::new("pkill").arg("-f").arg(&bin_dir).status();
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
    drove?;

    let all = crate::events(&p.kd)?;
    let mine: Vec<Value> = all.iter().filter(|e| e["harness"] == h.id).cloned().collect();
    let mut problems = model.problems();
    problems.extend(crate::check(h, None, &scenario, &[], &all, &p.project)?);
    let mut summary = None;
    if args.window {
        problems.extend(crate::front(&all, h.id, "agent_app", &[]));
        let (wrong, said) = credit::check(&p.kd, h.id, "proj", &person)?;
        problems.extend(wrong);
        summary = Some(said);
    }
    let passed = crate::report(app.id, &version, &scenario, &crate::kairosd_version(&args.kairosd)?, &mine, &problems);
    if let Some(said) = summary {
        println!("  {said}");
    }
    Ok(passed)
}
