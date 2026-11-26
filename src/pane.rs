//! A second terminal window, the way a person opens one to run a second agent: another lab process runs in it and
//! starts the program in a pty of its own, so the program is in that window's process tree, and passes the program's
//! bytes and the keys over a local socket.

use std::ffi::OsString;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use portable_pty::{native_pty_system, CommandBuilder};
use serde_json::{json, Value};

use crate::term::{self, Term};

/// A terminal window: its X window id, console window handle or (macOS) the title it was given.
#[derive(Clone, Debug)]
pub struct Window(String);

pub struct Pane {
    stream: TcpStream,
    pub window: Window,
}

impl Pane {
    /// Opens the window with nothing running in it yet, in `away`, a folder that is no project.
    pub fn open(run: &Path, away: &Path) -> Result<Pane> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port().to_string();
        let exe = std::env::current_exe()?;
        let away = away.to_string_lossy().into_owned();
        if cfg!(windows) {
            // `start` takes its first quoted word as the window's title.
            #[cfg(windows)]
            std::os::windows::process::CommandExt::raw_arg(&mut Command::new("cmd"), format!("/c start \"lab-pane\" \"{}\" pane {port} \"{away}\"", exe.display()))
                .status()?;
        } else if cfg!(target_os = "macos") {
            let q = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
            let script = run.join("pane.command");
            std::fs::write(&script, format!("#!/bin/bash\nexec {} pane {port} {}\n", q(&exe.to_string_lossy()), q(&away)))?;
            Command::new("chmod").arg("+x").arg(&script).status()?;
            Command::new("open").args(["-a", "Terminal"]).arg(&script).status()?;
        } else {
            // Not a child of the lab: the first window's process tree would hold the second window's agent too.
            let ok = Command::new("setsid").args(["-f", "xterm", "-T", "lab-pane", "-e"]).arg(&exe).args(["pane", &port, &away]).status().context("xterm")?;
            anyhow::ensure!(ok.success(), "could not open a second xterm");
        }
        listener.set_nonblocking(true)?;
        let until = Instant::now() + Duration::from_secs(60);
        let mut stream = loop {
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < until => thread::sleep(Duration::from_millis(200)),
                Err(e) => bail!("the second window never came up: {e}"),
            }
        };
        stream.set_nonblocking(false)?;
        let window = Window(line(&mut stream)?);
        Ok(Pane { stream, window })
    }

    /// Starts the program in the window, as typing its name there would.
    pub fn start(&mut self, cmd: &CommandBuilder, raw_path: &Path) -> Result<Term> {
        let argv: Vec<String> = cmd.get_argv().iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let cwd = cmd.get_cwd().map(|c| c.to_string_lossy().into_owned());
        let env: Vec<(&str, &str)> = cmd.iter_full_env_as_str().collect();
        self.stream.write_all(format!("{}\n", json!({ "argv": argv, "cwd": cwd, "env": env })).as_bytes())?;
        Term::remote(self.stream.try_clone()?, raw_path)
    }
}

/// One line, read a byte at a time so nothing after it is taken from the stream.
fn line(stream: &mut TcpStream) -> Result<String> {
    let mut out = Vec::new();
    let mut b = [0u8; 1];
    while stream.read(&mut b)? == 1 && b[0] != b'\n' {
        out.push(b[0]);
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// The lab process in the second window.
pub fn serve(port: &str, away: &str) -> Result<bool> {
    std::env::set_current_dir(away)?;
    let window = own(&format!("lab-pane-{port}"))?;
    let mut stream = TcpStream::connect(("127.0.0.1", port.parse::<u16>()?))?;
    stream.write_all(format!("{}\n", window.0).as_bytes())?;
    let spec = line(&mut stream)?;
    if spec.is_empty() {
        return Ok(true);
    }
    let spec: Value = serde_json::from_str(&spec)?;
    let argv: Vec<OsString> = spec["argv"].as_array().into_iter().flatten().filter_map(Value::as_str).map(OsString::from).collect();
    println!("lab: running {:?} in {}", argv, spec["cwd"]);
    let mut cmd = CommandBuilder::from_argv(argv);
    cmd.env_clear();
    for kv in spec["env"].as_array().into_iter().flatten() {
        if let (Some(k), Some(v)) = (kv[0].as_str(), kv[1].as_str()) {
            cmd.env(k, v);
        }
    }
    if let Some(cwd) = spec["cwd"].as_str() {
        cmd.cwd(cwd);
    }
    let pair = native_pty_system().openpty(term::size()).map_err(|e| anyhow!("{e}"))?;
    let child = Arc::new(Mutex::new(pair.slave.spawn_command(cmd).map_err(|e| anyhow!("{e}"))?));
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().map_err(|e| anyhow!("{e}"))?;
    let mut writer = pair.master.take_writer().map_err(|e| anyhow!("{e}"))?;
    let mut out = stream.try_clone()?;
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n @ 1..) = reader.read(&mut buf) {
            if out.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        let _ = out.shutdown(Shutdown::Both);
    });
    // A pty on Windows stays open after its program ends.
    let (watched, closer) = (child.clone(), stream.try_clone()?);
    thread::spawn(move || loop {
        if matches!(watched.lock().unwrap().try_wait(), Ok(Some(_))) {
            let _ = closer.shutdown(Shutdown::Both);
            return;
        }
        thread::sleep(Duration::from_millis(500));
    });
    let mut buf = [0u8; 8192];
    while let Ok(n @ 1..) = stream.read(&mut buf) {
        if writer.write_all(&buf[..n]).is_err() || writer.flush().is_err() {
            break;
        }
    }
    let mut child = child.lock().unwrap();
    let _ = child.kill();
    let _ = child.wait();
    drop(pair.master);
    Ok(true)
}

/// The window this process runs in. On macOS it is told apart by a title this sets.
#[cfg(target_os = "linux")]
pub fn own(_title: &str) -> Result<Window> {
    if let Ok(id) = std::env::var("WINDOWID") {
        return Ok(Window(id));
    }
    let id = crate::harness::output(Command::new("xdotool").arg("getactivewindow"))?;
    Ok(Window(id))
}

#[cfg(target_os = "macos")]
pub fn own(title: &str) -> Result<Window> {
    let mut tty = std::fs::OpenOptions::new().write(true).open("/dev/tty").context("this process has no terminal")?;
    write!(tty, "\x1b]0;{title}\x07")?;
    Ok(Window(title.to_string()))
}

#[cfg(windows)]
pub fn own(_title: &str) -> Result<Window> {
    let hwnd = unsafe { windows_sys::Win32::System::Console::GetConsoleWindow() };
    anyhow::ensure!(!hwnd.is_null(), "this process has no console window");
    Ok(Window((hwnd as usize).to_string()))
}

/// Brings the window to the front, the way clicking on it does (the lab presses something along with it).
#[cfg(target_os = "linux")]
pub fn raise(w: &Window) -> Result<()> {
    crate::harness::output(Command::new("xdotool").args(["windowactivate", "--sync", &w.0]))?;
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn raise(w: &Window) -> Result<()> {
    let script = format!(
        "tell application \"System Events\" to tell process \"Terminal\"\nset frontmost to true\nperform action \"AXRaise\" of (first window whose name contains \"{}\")\nend tell",
        w.0
    );
    crate::harness::output(Command::new("osascript").args(["-e", &script]))?;
    Ok(())
}

#[cfg(windows)]
pub fn raise(w: &Window) -> Result<()> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_MENU};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, SetForegroundWindow};
    let hwnd = w.0.parse::<usize>()? as windows_sys::Win32::Foundation::HWND;
    let key = |flags: u32| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_MENU, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    // Windows lets a program bring another window forward right after a key press, like the Alt this taps.
    let alt = [key(0), key(KEYEVENTF_KEYUP)];
    for _ in 0..3 {
        unsafe {
            SendInput(alt.len() as u32, alt.as_ptr(), std::mem::size_of::<INPUT>() as i32);
            SetForegroundWindow(hwnd);
        }
        thread::sleep(Duration::from_millis(300));
        if unsafe { GetForegroundWindow() } == hwnd {
            return Ok(());
        }
    }
    bail!("window {} would not come to the front", w.0)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn own(_title: &str) -> Result<Window> {
    bail!("the lab can't find its window on this system")
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn raise(_w: &Window) -> Result<()> {
    bail!("the lab can't switch windows on this system")
}
