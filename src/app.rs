//! Desktop apps. The lab clicks and types in the app's own window through the Chrome DevTools protocol, which hands the
//! page the same mouse and key events a person's hands do.

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
use crate::scenario::{self, Scenario};
use crate::{Args, STRAY_ENV};

const PORT: u16 = 9333;

pub struct App {
    pub id: &'static str,
    repo: &'static str,
    /// The harness it runs, which Kairos names in its events.
    harness: &'static str,
    /// What it keeps in the home folder, removed before a run in a real home.
    state: &'static [&'static str],
    stores: &'static [&'static str],
}

pub const ALL: &[App] = &[App {
    id: "t3code",
    repo: "pingdotgg/t3code",
    harness: "claude-code",
    state: &[".t3", ".claude", ".claude.json"],
    stores: &[".claude/projects", ".t3/userdata"],
}];

pub fn find(id: &str) -> Option<&'static App> {
    ALL.iter().find(|a| a.id == id)
}

/// The app's program in what its package unpacked to.
fn program(dir: &Path) -> Option<PathBuf> {
    if OS == "macos" {
        let app = fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "app"))?;
        return fs::read_dir(app.join("Contents").join("MacOS")).ok()?.flatten().map(|e| e.path()).next();
    }
    let fits = |name: &str| if OS == "linux" { name == "t3code" } else { name.starts_with("T3 Code") && name.ends_with(".exe") };
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
        if let Some(exe) = program(&dir) {
            return Ok((exe, version));
        }
        fs::create_dir_all(&dir)?;
        let asset = match (OS, ARCH) {
            ("linux", _) => format!("T3-Code-{version}-amd64.deb"),
            ("macos", "aarch64") => format!("T3-Code-{version}-arm64.zip"),
            ("macos", _) => format!("T3-Code-{version}-x64.zip"),
            _ => format!("T3-Code-{version}-x64.exe"),
        };
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
        let exe = program(&dir).with_context(|| format!("installed {} {version} but found no program in {}", self.id, dir.display()))?;
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
            let list = Command::new("curl").args(["-fsS", &format!("http://127.0.0.1:{port}/json")]).output();
            let page = list.ok().and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok()).and_then(|v| {
                v.as_array()?.iter().find(|t| t["type"] == "page")?["webSocketDebuggerUrl"].as_str().map(str::to_string)
            });
            if let Some(url) = page {
                let (ws, _) = tungstenite::connect(url.as_str()).context("the app's debugging port")?;
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
            if let Message::Text(t) = self.ws.read()? {
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
        let at = self.eval(&format!("(() => {{ const e = {find}; if (!e) return null; const r = e.getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height / 2]; }})()"))?;
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
fn t3_start(page: &mut Page, project: &Path) -> Result<()> {
    if page.wait_for(&["Add project", "Welcome to T3 Code"], Duration::from_secs(90))?.contains("Welcome to T3 Code") {
        page.press("Start", "Add project")?;
    }
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
    Ok(())
}

fn drive(page: &mut Page, model: &Model, scenario: &Scenario, project: &Path, restart: &mut dyn FnMut() -> Result<()>) -> Result<()> {
    t3_start(page, project)?;
    let composer = "[...document.querySelectorAll('[contenteditable=true]')].find(e => e.getBoundingClientRect().width > 0)";
    for (i, turn) in scenario.turns.iter().enumerate() {
        if turn.restart {
            sleep(Duration::from_secs(6));
            restart()?;
            sleep(Duration::from_secs(4));
        }
        page.click_at(composer, "the message box")?;
        model.arm(i);
        page.type_text(turn.prompt)?;
        sleep(Duration::from_millis(400));
        page.key("Enter")?;
        let until = Instant::now() + Duration::from_secs(90);
        while !model.said(i) {
            if Instant::now() > until {
                bail!("turn {} never finished. Page:\n{}", i + 1, page.text()?);
            }
            sleep(Duration::from_millis(200));
        }
        sleep(Duration::from_secs(4));
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

    let mut daemon = crate::daemon(args, &p)?;
    sleep(Duration::from_secs(2));
    let launch = (h.prepare)(&p.home, &p.project, &url, "latest")?;
    let log = || OpenOptions::new().create(true).append(true).open(p.run.join("app.log"));
    let mut cmd = Command::new(&exe);
    cmd.arg(format!("--remote-debugging-port={PORT}"));
    if OS == "linux" {
        // An unpacked .deb has no setuid sandbox helper, and Xvfb has no GPU.
        cmd.args(["--no-sandbox", "--disable-gpu"]);
    }
    cmd.current_dir(&p.project).env("HOME", &p.home).env("USERPROFILE", &p.home).stdin(Stdio::null()).stdout(log()?).stderr(log()?);
    for k in STRAY_ENV {
        cmd.env_remove(k);
    }
    for (k, v) in &launch.env {
        cmd.env(k, v);
    }
    let bin = cli.parent().context("the harness has no folder")?.to_path_buf();
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())))?;
    cmd.env("PATH", path);
    let mut child = cmd.spawn().with_context(|| format!("starting {}", exe.display()))?;

    let mut restart = || -> Result<()> {
        let _ = daemon.kill();
        let _ = daemon.wait();
        daemon = crate::daemon(args, &p)?;
        Ok(())
    };
    let drove = Page::open(PORT, Duration::from_secs(90)).and_then(|mut page| {
        let r = drive(&mut page, &model, &scenario, &p.project, &mut restart);
        if r.is_err() {
            let _ = fs::write(p.run.join("page.txt"), page.text().unwrap_or_default());
        }
        r
    });
    sleep(Duration::from_secs(8));
    // Electron's helpers and the harness it started all run from the app's own folder.
    let folder = exe.parent().context("the app has no folder")?;
    if cfg!(windows) {
        let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &child.id().to_string()]).output();
    } else {
        let _ = Command::new("pkill").arg("-f").arg(folder).status();
    }
    let _ = child.kill();
    let _ = child.wait();
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
    if args.window {
        problems.extend(crate::front(&all, h.id, "agent_app", &[]));
    }
    crate::report(app.id, &version, &scenario, &mine, &problems);
    Ok(problems.is_empty())
}
