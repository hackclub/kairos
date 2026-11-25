use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use kairos_core::account as saved;
use kairos_core::{paths, update};
use anyhow::{bail, Context, Result};

use crate::account;

const INSTALL_USAGE: &str = "usage: kairos install [--no-login] [--keep-wakatime] [--server URL]";
const UNINSTALL_USAGE: &str = "usage: kairos uninstall";
// Deleting tracked data is done on request for now, so --purge uninstalls and keeps it. Flip this (and the usage) to give it back.
const PURGE: bool = false;

pub fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn command(program: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

fn run(program: &str, args: &[&str]) -> bool {
    command(program, args).stdout(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

pub fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = command(program, args).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

fn place(from: &Path, to: &Path) -> Result<()> {
    if same_file(from, to) {
        return Ok(());
    }
    let _ = fs::remove_file(to.with_extension("old"));
    let tmp = to.with_extension("new");
    fs::copy(from, &tmp).with_context(|| format!("copying {}", from.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    }
    let mut last = None;
    for _ in 0..10 {
        match fs::rename(&tmp, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let old = to.with_extension("old");
                let _ = fs::remove_file(&old);
                let _ = fs::rename(to, &old);
                last = Some(e);
                thread::sleep(Duration::from_millis(200));
            }
        }
    }
    let _ = fs::remove_file(&tmp);
    Err(last.unwrap()).with_context(|| format!("writing {}", to.display()))
}

fn remove(path: &Path) -> bool {
    for _ in 0..10 {
        match fs::remove_file(path) {
            Ok(()) => return true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
            Err(_) => thread::sleep(Duration::from_millis(200)),
        }
    }
    false
}

pub fn install_dir() -> Result<PathBuf> {
    paths::install_dir().context("cannot find your home folder")
}

pub fn daemon_running() -> bool {
    let Ok(file) = fs::OpenOptions::new().write(true).open(paths::data_dir().join("kairosd.lock")) else {
        return false;
    };
    matches!(file.try_lock(), Err(fs::TryLockError::WouldBlock))
}

pub fn daemon_pid() -> Option<u32> {
    if !daemon_running() {
        return None;
    }
    fs::read_to_string(paths::data_dir().join("kairosd.pid")).ok()?.trim().parse().ok()
}

fn terminate(pid: u32) {
    let pid = pid.to_string();
    if cfg!(windows) {
        run("taskkill", &["/PID", &pid, "/F"]);
    } else {
        run("kill", &["-TERM", &pid]);
    }
}

#[cfg_attr(windows, allow(dead_code))]
fn stop_running() {
    if let Some(pid) = daemon_pid() {
        terminate(pid);
        let deadline = Instant::now() + Duration::from_secs(5);
        while daemon_running() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(200));
        }
    }
}

fn start(daemon: &Path, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new(daemon);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let log = fs::OpenOptions::new().create(true).append(true).open(paths::data_dir().join("kairosd.log"));
        cmd.process_group(0).stderr(log.map(Stdio::from).unwrap_or_else(|_| Stdio::null()));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.stderr(Stdio::null()).creation_flags(DETACHED_PROCESS | NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    cmd.spawn().with_context(|| format!("starting {}", daemon.display()))?;
    Ok(())
}

/// The new one waits for the old one's lock before it starts tracking.
fn replace(daemon: &Path, pid: u32) -> Result<&'static str> {
    start(daemon, &["--replace", &pid.to_string()])?;
    terminate(pid);
    Ok("started the new version")
}

#[allow(dead_code)]
mod gen {
    use std::path::{Path, PathBuf};

    use kairos_core::paths;

    pub fn systemd_unit(daemon: &Path) -> String {
        let path = daemon
            .display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$");
        format!(
            "[Unit]\nDescription=Kairos focus time tracker\n\n\
             [Service]\nExecStart=\"{path}\"\nRestart=on-failure\nRestartSec=5\nRestartForceExitStatus=75\n\n\
             [Install]\nWantedBy=default.target\n"
        )
    }

    pub fn desktop_entry(daemon: &Path) -> String {
        let mut quoted = String::new();
        for c in daemon.display().to_string().chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                quoted.push('\\');
            }
            quoted.push(c);
        }
        let exec = quoted.replace('\\', "\\\\").replace('%', "%%");
        format!(
            "[Desktop Entry]\nType=Application\nName=Kairos\nComment=Kairos focus time tracker\n\
             Exec=\"{exec}\"\nTerminal=false\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n"
        )
    }

    fn xml(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }

    pub fn launch_agent(daemon: &Path, log: &Path) -> String {
        let log = xml(&log.display().to_string());
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n\
             \t<key>Label</key><string>{}</string>\n\
             \t<key>ProgramArguments</key><array><string>{}</string></array>\n\
             \t<key>RunAtLoad</key><true/>\n\
             \t<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n\
             \t<key>ProcessType</key><string>Background</string>\n\
             \t<key>StandardOutPath</key><string>{log}</string>\n\
             \t<key>StandardErrorPath</key><string>{log}</string>\n\
             </dict>\n</plist>\n",
            paths::LAUNCHD_LABEL,
            xml(&daemon.display().to_string()),
        )
    }

    #[derive(Debug, PartialEq)]
    pub enum RegValue {
        Str(String),
        Expand(String),
        Dword(u32),
    }

    pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    pub const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Kairos";
    pub const ENV_KEY: &str = "Environment";
    pub const PATH_MARK: &str = "# kairos";

    pub fn run_value(daemon: &Path) -> String {
        format!("\"{}\"", daemon.display())
    }

    pub fn uninstall_values(dir: &Path, version: &str, size_kb: u32) -> Vec<(&'static str, RegValue)> {
        use RegValue::{Dword, Str};
        vec![
            ("DisplayName", Str("Kairos".into())),
            ("DisplayVersion", Str(version.into())),
            ("Publisher", Str("Kairos".into())),
            ("InstallLocation", Str(dir.display().to_string())),
            ("UninstallString", Str(format!("\"{}\" uninstall", dir.join("kairos.exe").display()))),
            ("NoModify", Dword(1)),
            ("NoRepair", Dword(1)),
            ("EstimatedSize", Dword(size_kb)),
        ]
    }

    fn same_entry(entry: &str, dir: &str, expand: &impl Fn(&str) -> String) -> bool {
        let entry = entry.trim();
        if entry.is_empty() {
            return false;
        }
        let norm = |s: &str| s.trim().trim_matches('"').trim_end_matches(['\\', '/']).to_lowercase();
        let entry = if entry.contains('%') { expand(entry) } else { entry.to_string() };
        norm(&entry) == norm(dir)
    }

    /// The Windows PATH value with `dir` added, or None when it is already there.
    pub fn path_with(existing: &str, dir: &str, expand: impl Fn(&str) -> String) -> Option<String> {
        if existing.split(';').any(|e| same_entry(e, dir, &expand)) {
            return None;
        }
        if existing.trim().is_empty() {
            return Some(dir.to_string());
        }
        Some(if existing.ends_with(';') { format!("{existing}{dir};") } else { format!("{existing};{dir}") })
    }

    /// The Windows PATH value without `dir`, or None when it was not there.
    pub fn path_without(existing: &str, dir: &str, expand: impl Fn(&str) -> String) -> Option<String> {
        let parts: Vec<&str> = existing.split(';').collect();
        let kept: Vec<&str> = parts.iter().copied().filter(|e| !same_entry(e, dir, &expand)).collect();
        (kept.len() != parts.len()).then(|| kept.join(";"))
    }

    /// Relative to `$HOME` when `dir` is under it, and quoted because the macOS folder has a space.
    pub fn path_line(dir: &Path, home: &Path, fish: bool) -> String {
        let escape = |s: &str| {
            let mut out = String::new();
            for c in s.chars() {
                if matches!(c, '"' | '\\' | '$') || (c == '`' && !fish) {
                    out.push('\\');
                }
                out.push(c);
            }
            out
        };
        let target = match dir.strip_prefix(home) {
            Ok(rel) => format!("$HOME/{}", escape(&rel.display().to_string())),
            Err(_) => escape(&dir.display().to_string()),
        };
        if fish {
            format!("fish_add_path -g \"{target}\" {PATH_MARK}")
        } else {
            format!("export PATH=\"{target}:$PATH\" {PATH_MARK}")
        }
    }


    // A login bash (every macOS terminal) reads only the first of these that exists, so a new ~/.bash_profile beside
    // someone's ~/.profile would switch that one off.
    const BASH_LOGIN: [&str; 3] = [".bash_profile", ".bash_login", ".profile"];

    /// Every file `startup_files` can name.
    pub fn all_startup_files(home: &Path) -> Vec<(PathBuf, bool)> {
        let mut files: Vec<(PathBuf, bool)> = [".zshrc", ".bashrc"].iter().chain(&BASH_LOGIN).map(|f| (home.join(f), false)).collect();
        files.push((home.join(".config/fish/conf.d/kairos.fish"), true));
        files
    }

    /// The startup files for `shell` (a name or a $SHELL path) and whether each one takes fish syntax.
    /// Empty for a shell Kairos doesn't know.
    pub fn startup_files(shell: &str, home: &Path, macos: bool) -> Vec<(PathBuf, bool)> {
        match Path::new(shell.trim()).file_name().and_then(|n| n.to_str()) {
            Some("zsh") => vec![(home.join(".zshrc"), false)],
            Some("bash") if macos => {
                let login = BASH_LOGIN.iter().map(|f| home.join(f)).find(|f| f.exists());
                vec![(home.join(".bashrc"), false), (login.unwrap_or_else(|| home.join(BASH_LOGIN[0])), false)]
            }
            Some("bash") => vec![(home.join(".bashrc"), false)],
            Some("fish") => vec![(home.join(".config/fish/conf.d/kairos.fish"), true)],
            _ => Vec::new(),
        }
    }

    /// The file with `line` appended, or None when it already has it.
    pub fn rc_add(content: &str, line: &str) -> Option<String> {
        if content.lines().any(|l| l == line) {
            return None;
        }
        let gap = if content.is_empty() || content.ends_with('\n') { "" } else { "\n" };
        Some(format!("{content}{gap}{line}\n"))
    }

    /// The file without the lines equal to `line`, or None when it has none.
    pub fn rc_remove(content: &str, line: &str) -> Option<String> {
        let is_ours = |l: &&str| l.trim_end_matches(['\n', '\r']) == line;
        let lines: Vec<&str> = content.split_inclusive('\n').collect();
        let kept: String = lines.iter().filter(|l| !is_ours(l)).copied().collect();
        (kept.len() != content.len()).then_some(kept)
    }
}

#[cfg(windows)]
mod reg {
    use std::io;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteKeyW, RegDeleteTreeW, RegGetValueW, RegSetValueExW,
        HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_DWORD, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_NOEXPAND,
        RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE};

    use super::gen::RegValue;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn check(rc: u32) -> io::Result<()> {
        if rc == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(rc as i32))
        }
    }

    pub fn set(key: &str, values: &[(&str, RegValue)]) -> io::Result<()> {
        let mut h: HKEY = null_mut();
        let key = wide(key);
        check(unsafe {
            RegCreateKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, null(), &mut h, null_mut())
        })?;
        let result = values.iter().try_for_each(|(name, value)| {
            let name = wide(name);
            check(match value {
                RegValue::Str(s) => {
                    let data = wide(s);
                    unsafe { RegSetValueExW(h, name.as_ptr(), 0, REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32) }
                }
                RegValue::Expand(s) => {
                    let data = wide(s);
                    unsafe { RegSetValueExW(h, name.as_ptr(), 0, REG_EXPAND_SZ, data.as_ptr().cast(), (data.len() * 2) as u32) }
                }
                RegValue::Dword(d) => unsafe {
                    RegSetValueExW(h, name.as_ptr(), 0, REG_DWORD, (d as *const u32).cast(), 4)
                },
            })
        });
        unsafe { RegCloseKey(h) };
        result
    }

    pub fn get(key: &str, name: &str) -> Option<String> {
        let (key, name) = (wide(key), wide(name));
        let mut len = 0u32;
        let rc = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), &mut len)
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let mut buf = vec![0u16; (len as usize).div_ceil(2)];
        let mut len = (buf.len() * 2) as u32;
        let rc = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, null_mut(), buf.as_mut_ptr().cast(), &mut len)
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
    }

    /// The value as stored, with `%VARS%` left alone, and whether it is a REG_EXPAND_SZ. None when it isn't set.
    pub fn get_unexpanded(key: &str, name: &str) -> io::Result<Option<(String, bool)>> {
        let (key, name) = (wide(key), wide(name));
        let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
        let (mut len, mut kind) = (0u32, 0u32);
        match unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), flags, &mut kind, null_mut(), &mut len) } {
            ERROR_FILE_NOT_FOUND => return Ok(None),
            rc => check(rc)?,
        }
        let mut buf = vec![0u16; (len as usize).div_ceil(2)];
        let mut len = (buf.len() * 2) as u32;
        check(unsafe {
            RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), flags, &mut kind, buf.as_mut_ptr().cast(), &mut len)
        })?;
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok(Some((String::from_utf16_lossy(&buf[..end]), kind == REG_EXPAND_SZ)))
    }

    pub fn expand(s: &str) -> String {
        let src = wide(s);
        let need = unsafe { ExpandEnvironmentStringsW(src.as_ptr(), null_mut(), 0) };
        if need == 0 {
            return s.to_string();
        }
        let mut buf = vec![0u16; need as usize];
        let got = unsafe { ExpandEnvironmentStringsW(src.as_ptr(), buf.as_mut_ptr(), need) };
        if got == 0 || got > need {
            return s.to_string();
        }
        String::from_utf16_lossy(&buf[..got as usize - 1])
    }

    /// Tells open programs (Explorer, so new terminals) to reread the environment.
    pub fn broadcast_environment() {
        let env = wide("Environment");
        let mut result = 0usize;
        unsafe {
            SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, env.as_ptr() as isize, SMTO_ABORTIFHUNG, 2000, &mut result)
        };
    }

    pub fn delete_value(key: &str, name: &str) -> io::Result<()> {
        match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), wide(name).as_ptr()) } {
            ERROR_FILE_NOT_FOUND => Ok(()),
            rc => check(rc),
        }
    }

    pub fn delete_key(key: &str) -> io::Result<()> {
        let key = wide(key);
        match unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key.as_ptr()) } {
            ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => {}
            rc => check(rc)?,
        }
        match unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, key.as_ptr()) } {
            ERROR_FILE_NOT_FOUND => Ok(()),
            rc => check(rc),
        }
    }
}

#[cfg(windows)]
mod os {
    use super::gen::{self, RegValue};
    use super::*;

    pub fn stop() {
        run("taskkill", &["/IM", "kairosd.exe", "/F"]);
        thread::sleep(Duration::from_millis(500));
    }

    pub fn enable(daemon: &Path) -> Result<()> {
        reg::set(gen::RUN_KEY, &[("Kairos", RegValue::Str(gen::run_value(daemon)))])
            .context("could not register Kairos to start when you log in")?;
        write_entry(daemon.parent().context("bad install folder")?, env!("CARGO_PKG_VERSION"))?;
        start(daemon, &[])
    }

    fn write_entry(dir: &Path, version: &str) -> Result<()> {
        let size: u64 = ["kairos.exe", "kairosd.exe"].iter().filter_map(|n| fs::metadata(dir.join(n)).ok()).map(|m| m.len()).sum();
        let size_kb = u32::try_from(size / 1024).unwrap_or(u32::MAX);
        reg::set(gen::UNINSTALL_KEY, &gen::uninstall_values(dir, version, size_kb))
            .context("could not add Kairos to Apps & features")
    }

    pub fn refresh_entry(version: &str) -> Result<()> {
        if reg::get(gen::UNINSTALL_KEY, "DisplayName").is_some() {
            write_entry(&install_dir()?, version)?;
        }
        Ok(())
    }

    pub fn disable() -> Result<()> {
        reg::delete_value(gen::RUN_KEY, "Kairos").context("could not remove Kairos from the apps that start at login")?;
        reg::delete_key(gen::UNINSTALL_KEY).context("could not remove Kairos from Apps & features")?;
        Ok(())
    }

    pub fn autostart() -> String {
        let expected = install_dir().map(|d| gen::run_value(&d.join("kairosd.exe"))).unwrap_or_default();
        let run = match reg::get(gen::RUN_KEY, "Kairos") {
            Some(v) if v == expected => "registered (HKCU Run value Kairos)".to_string(),
            Some(v) => format!("HKCU Run value Kairos points elsewhere: {v}"),
            None => "not registered".to_string(),
        };
        let entry = match reg::get(gen::UNINSTALL_KEY, "DisplayVersion") {
            Some(v) => format!("Apps & features entry {v}"),
            None => "no Apps & features entry".to_string(),
        };
        format!("{run}, {entry}")
    }

    pub fn restart(daemon: &Path, pid: u32) -> Result<&'static str> {
        replace(daemon, pid)
    }

    pub fn remove_later(files: &[PathBuf], dir: &Path) -> Result<()> {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut names = String::new();
        for f in files {
            let old = f.with_extension("old");
            let left = if fs::rename(f, &old).is_ok() { old } else { f.clone() };
            names.push_str(&format!(" \"{}\"", left.display()));
        }
        let script = format!(
            "for /l %i in (1,1,10) do @(ping -n 2 127.0.0.1 >nul & del /f /q{names} >nul 2>&1) & rmdir \"{}\"",
            dir.display()
        );
        Command::new("cmd")
            .arg("/c")
            .raw_arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
            .spawn()?;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod os {
    use super::*;

    fn plist_path() -> Result<PathBuf> {
        Ok(dirs::home_dir()
            .context("cannot find your home folder")?
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", paths::LAUNCHD_LABEL)))
    }

    fn service() -> String {
        let uid = output("id", &["-u"]).filter(|u| !u.is_empty()).unwrap_or_else(|| "501".into());
        format!("gui/{uid}/{}", paths::LAUNCHD_LABEL)
    }

    pub fn stop() {
        run("launchctl", &["bootout", &service()]);
        thread::sleep(Duration::from_millis(500));
        stop_running();
    }

    pub fn enable(daemon: &Path) -> Result<()> {
        let path = plist_path()?;
        fs::create_dir_all(path.parent().unwrap())?;
        fs::create_dir_all(paths::data_dir())?;
        fs::write(&path, gen::launch_agent(daemon, &paths::data_dir().join("kairosd.log")))
            .with_context(|| format!("writing {}", path.display()))?;
        let file = path.display().to_string();
        let service = service();
        let domain = service.rsplit_once('/').map_or("", |(d, _)| d);
        for _ in 0..3 {
            if run("launchctl", &["bootstrap", domain, &file]) {
                return Ok(());
            }
            thread::sleep(Duration::from_secs(1));
        }
        println!("Could not register Kairos to start at login, starting it for now only.");
        start(daemon, &[])
    }

    pub fn refresh_entry(_: &str) -> Result<()> {
        Ok(())
    }

    pub fn disable() -> Result<()> {
        stop();
        match fs::remove_file(plist_path()?) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).context("removing the login item"),
            _ => Ok(()),
        }
    }

    pub fn autostart() -> String {
        let Ok(path) = plist_path() else { return "unknown".into() };
        match (path.is_file(), run("launchctl", &["print", &service()])) {
            (true, true) => format!("LaunchAgent {} (loaded)", paths::LAUNCHD_LABEL),
            (true, false) => format!("LaunchAgent {} (file present, not loaded)", paths::LAUNCHD_LABEL),
            (false, _) => "not registered".into(),
        }
    }

    pub fn restart(daemon: &Path, pid: u32) -> Result<&'static str> {
        if run("launchctl", &["print", &service()]) && run("launchctl", &["kickstart", "-k", &service()]) {
            return Ok("launchd restarted it");
        }
        replace(daemon, pid)
    }

    pub fn remove_later(_: &[PathBuf], _: &Path) -> Result<()> {
        bail!("could not remove the installed files")
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod os {
    use super::*;

    const UNIT: &str = paths::SYSTEMD_UNIT;

    fn unit_path() -> Result<PathBuf> {
        Ok(dirs::config_dir().context("cannot find your config folder")?.join("systemd/user").join(UNIT))
    }

    fn desktop_path() -> Result<PathBuf> {
        Ok(dirs::config_dir().context("cannot find your config folder")?.join("autostart/kairos.desktop"))
    }

    fn systemd_user() -> bool {
        run("systemctl", &["--user", "show-environment"])
    }

    pub fn stop() {
        if systemd_user() {
            run("systemctl", &["--user", "stop", UNIT]);
        }
        stop_running();
    }

    fn remove_unit() -> Result<()> {
        let path = unit_path()?;
        if path.exists() {
            run("systemctl", &["--user", "disable", "--now", UNIT]);
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            run("systemctl", &["--user", "daemon-reload"]);
        }
        Ok(())
    }

    fn write(path: &Path, text: String) -> Result<()> {
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }

    pub fn enable(daemon: &Path) -> Result<()> {
        let desktop = desktop_path()?;
        if systemd_user() {
            write(&unit_path()?, gen::systemd_unit(daemon))?;
            if run("systemctl", &["--user", "daemon-reload"]) && run("systemctl", &["--user", "enable", "--now", UNIT]) {
                let _ = fs::remove_file(&desktop);
                return Ok(());
            }
            remove_unit()?;
            println!("Could not register Kairos with systemd, using an autostart entry instead.");
        } else {
            let _ = fs::remove_file(unit_path()?);
        }
        write(&desktop, gen::desktop_entry(daemon))?;
        start(daemon, &[])
    }

    pub fn refresh_entry(_: &str) -> Result<()> {
        Ok(())
    }

    pub fn disable() -> Result<()> {
        remove_unit()?;
        match fs::remove_file(desktop_path()?) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).context("removing the autostart entry"),
            _ => {}
        }
        stop_running();
        Ok(())
    }

    pub fn autostart() -> String {
        if unit_path().is_ok_and(|p| p.is_file()) {
            let state = output("systemctl", &["--user", "is-enabled", UNIT]).filter(|s| !s.is_empty());
            return format!("systemd user unit {UNIT} ({})", state.as_deref().unwrap_or("systemd not reachable"));
        }
        match desktop_path() {
            Ok(p) if p.is_file() => format!("XDG autostart entry {}", p.display()),
            _ => "not registered".into(),
        }
    }

    pub fn restart(daemon: &Path, pid: u32) -> Result<&'static str> {
        if run("systemctl", &["--user", "is-active", "--quiet", UNIT]) && run("systemctl", &["--user", "restart", UNIT]) {
            return Ok("systemd restarted it");
        }
        replace(daemon, pid)
    }

    pub fn remove_later(_: &[PathBuf], _: &Path) -> Result<()> {
        bail!("could not remove the installed files")
    }
}

#[cfg(windows)]
mod onpath {
    use super::gen::{self, RegValue};
    use super::*;

    fn edit(change: impl Fn(&str) -> Option<String>) -> Result<bool> {
        let (current, expandable) =
            reg::get_unexpanded(gen::ENV_KEY, "Path").context("reading your PATH")?.unwrap_or((String::new(), true));
        let Some(new) = change(&current) else { return Ok(false) };
        let value = if expandable { RegValue::Expand(new) } else { RegValue::Str(new) };
        reg::set(gen::ENV_KEY, &[("Path", value)]).context("writing your PATH")?;
        reg::broadcast_environment();
        Ok(true)
    }

    pub fn add(dir: &Path) -> Result<Option<String>> {
        let dir = dir.display().to_string();
        let changed = edit(|cur| gen::path_with(cur, &dir, reg::expand))?;
        Ok(changed.then(|| "Added kairos to your PATH. Open a new terminal to use it.".to_string()))
    }

    pub fn remove(dir: &Path) -> Result<()> {
        let dir = dir.display().to_string();
        edit(|cur| gen::path_without(cur, &dir, reg::expand))?;
        Ok(())
    }
}

#[cfg(not(windows))]
mod onpath {
    use std::io::Write;

    use super::gen;
    use super::*;

    fn read(file: &Path) -> Result<String> {
        match fs::read_to_string(file) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            r => r.with_context(|| format!("reading {}", file.display())),
        }
    }

    pub(super) fn add_in(dir: &Path, home: &Path, shell: &str, macos: bool, path: Option<std::ffi::OsString>) -> Result<Option<String>> {
        if path.is_some_and(|p| std::env::split_paths(&p).any(|d| d == dir)) {
            return Ok(None);
        }
        let files = gen::startup_files(shell, home, macos);
        if files.is_empty() {
            let line = gen::path_line(dir, home, false);
            return Ok(Some(format!("Add this line to your shell's startup file to use the kairos command:\n{line}")));
        }
        let mut changed = Vec::new();
        for (file, fish) in files {
            let content = read(&file)?;
            let Some(text) = gen::rc_add(&content, &gen::path_line(dir, home, fish)) else { continue };
            fs::create_dir_all(file.parent().context("bad startup file")?)?;
            // Appended, so nothing that goes wrong here can cost the file what it already had.
            let mut out = fs::OpenOptions::new().create(true).append(true).open(&file).with_context(|| format!("opening {}", file.display()))?;
            out.write_all(text[content.len()..].as_bytes()).with_context(|| format!("writing {}", file.display()))?;
            changed.push(file);
        }
        if changed.is_empty() {
            return Ok(None);
        }
        let names: Vec<String> = changed
            .iter()
            .map(|f| f.strip_prefix(home).map_or(f.display().to_string(), |r| format!("~/{}", r.display())))
            .collect();
        Ok(Some(format!("Added kairos to your PATH in {}. Open a new terminal to use it.", names.join(" and "))))
    }

    // Written beside the real file (a dotfile is often a link into a dotfiles folder) and renamed over it, so the
    // file is never left half written.
    fn replace(file: &Path, text: &str) -> std::io::Result<()> {
        let real = fs::canonicalize(file)?;
        let name = real.file_name().unwrap_or_default().to_string_lossy();
        let tmp = real.with_file_name(format!(".{}.kairos-tmp", name.trim_start_matches('.')));
        let done = fs::write(&tmp, text)
            .and_then(|()| fs::set_permissions(&tmp, fs::metadata(&real)?.permissions()))
            .and_then(|()| fs::rename(&tmp, &real));
        if done.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        done
    }

    pub(super) fn remove_in(dir: &Path, home: &Path) -> Result<()> {
        for (file, fish) in gen::all_startup_files(home) {
            let Some(text) = gen::rc_remove(&read(&file)?, &gen::path_line(dir, home, fish)) else { continue };
            let done = if text.is_empty() { fs::remove_file(&file) } else { replace(&file, &text) };
            done.with_context(|| format!("updating {}", file.display()))?;
        }
        Ok(())
    }

    pub fn add(dir: &Path) -> Result<Option<String>> {
        let home = dirs::home_dir().context("cannot find your home folder")?;
        let shell = std::env::var("SHELL").unwrap_or_default();
        add_in(dir, &home, &shell, cfg!(target_os = "macos"), std::env::var_os("PATH"))
    }

    pub fn remove(dir: &Path) -> Result<()> {
        let home = dirs::home_dir().context("cannot find your home folder")?;
        remove_in(dir, &home)
    }
}

pub use os::{autostart, restart};

pub fn refresh_entry(version: &str) -> Result<()> {
    os::refresh_entry(version)
}

/// GNOME Shell gives other programs no way to see the focused window on Wayland, so a small extension reports it.
#[cfg(target_os = "linux")]
fn desktop_integration() {
    use kairos_core::gnome_extension::{install_gnome_extension, is_gnome_session, InstallOutcome};

    if !is_gnome_session() {
        return;
    }
    match install_gnome_extension() {
        Ok(InstallOutcome::Active) => println!("Turned on the Kairos GNOME extension."),
        Ok(InstallOutcome::ActiveAfterLogin) => {
            println!("Added the Kairos GNOME extension. Log out and back in once so GNOME loads it.")
        }
        Err(e) => println!("Could not add the Kairos GNOME extension ({e:#}), so app focus won't be tracked."),
    }
}

#[cfg(not(target_os = "linux"))]
fn desktop_integration() {}

#[cfg(target_os = "linux")]
fn remove_desktop_integration() {
    if let Err(e) = kairos_core::gnome_extension::uninstall_gnome_extension() {
        println!("Could not remove the Kairos GNOME extension: {e:#}");
    }
}

#[cfg(windows)]
fn remove_desktop_integration() {
    crate::pwsh::remove_all();
}

#[cfg(target_os = "macos")]
fn remove_desktop_integration() {}

pub fn install(args: &[String]) -> Result<()> {
    let mut sign_in = true;
    let mut take_wakatime = true;
    let mut server = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--no-login" => sign_in = false,
            "--keep-wakatime" => take_wakatime = false,
            "--server" => server = Some(it.next().context("--server needs a value")?.clone()),
            s if s.starts_with("--server=") => server = Some(s["--server=".len()..].to_string()),
            other => bail!("unknown option {other:?}\n{INSTALL_USAGE}"),
        }
    }
    let me = std::env::current_exe().context("finding the kairos program")?;
    let daemon_src = me.with_file_name(exe("kairosd"));
    if !daemon_src.is_file() {
        bail!("kairosd was not found next to kairos ({}), download both files again", daemon_src.display());
    }
    let dir = install_dir()?;

    println!("Installing Kairos...");
    os::stop();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    place(&me, &dir.join(exe("kairos")))?;
    let daemon = dir.join(exe("kairosd"));
    place(&daemon_src, &daemon)?;
    os::enable(&daemon)?;
    desktop_integration();
    println!("Kairos is running now and will start by itself when you log in.");
    match onpath::add(&dir) {
        Ok(Some(line)) => println!("{line}"),
        Ok(None) => {}
        Err(e) => println!("Could not add Kairos to your PATH ({e:#}), so run it as {}.", dir.join(exe("kairos")).display()),
    }
    #[cfg(windows)]
    crate::pwsh::add_all();

    let data = paths::data_dir();
    let config = kairos_core::wakatime::config_path();
    let browser = account::browser_here();
    let needs_hackatime = config.as_deref().is_some_and(|c| !kairos_core::wakatime::on_hackatime(&data, c));
    // With nobody at a terminal (a script, CI) there is no one to answer the browser, and the wait would only stall.
    let asked = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let oauth = crate::hackatime::Oauth::new().filter(|_| browser && needs_hackatime && asked);
    let pasted = oauth.is_none() && crate::hackatime::offer();
    if take_wakatime {
        match crate::wakatime::take() {
            Ok(_) => crate::editors::add_missing(),
            Err(e) => println!("Could not point WakaTime editor plugins at Kairos ({e:#})."),
        }
    } else if pasted || oauth.is_some() {
        crate::editors::add_missing();
    }
    // No editor plugin may ever bring wakatime-cli, and kairosd tries again by itself if this fails.
    let _ = kairos_core::wakatime_cli::install(&data);

    let kairos = dir.join(exe("kairos"));
    let signed_in = saved::load(&data);
    if let Some(a) = &signed_in {
        println!("Already signed in as {}.", a.username);
    }
    let connect = signed_in.is_none() && sign_in;
    let hackatime = oauth.as_ref().zip(config.as_deref());
    if !(connect || hackatime.is_some()) {
        if signed_in.is_none() {
            println!("Run {} login when you are ready to connect your account.", kairos.display());
        }
        return Ok(());
    }
    let server = server.unwrap_or_else(kairos_core::pairing::default_server);
    let end_on = signed_in.as_ref().map(|a| a.server.as_str()).or(sign_in.then_some(server.as_str()));
    let t = account::Trip { server: end_on, connect, hackatime, data: &data, open: &account::open_browser, wait: account::WAIT };
    let done = if browser {
        match account::trip(&t) {
            Ok(d) => Some(d),
            Err(e) if e.is::<account::NoBrowser>() => None,
            Err(e) => {
                println!("{}", account::retry_hint(&e));
                return Ok(());
            }
        }
    } else {
        None
    };
    let hackatime_later = |why: &str| println!("Hackatime wasn't set up ({why}). Run {} hackatime whenever you want to set it up.", kairos.display());
    match done {
        Some(d) => {
            match d.hackatime {
                Some(Ok(())) => crate::hackatime::set_up(),
                Some(Err(e)) => hackatime_later(&format!("{e:#}")),
                None => {}
            }
            if connect {
                println!("All set, Kairos is tracking in the background.");
            }
        }
        None => {
            if hackatime.is_some() {
                hackatime_later("no browser could be opened");
            }
            if connect {
                match account::with_code(&server, &data) {
                    Ok(_) => println!("All set, Kairos is tracking in the background."),
                    Err(e) => println!("Could not finish signing in ({e:#}).\nRun {} login to try again.", kairos.display()),
                }
            }
        }
    }
    Ok(())
}

pub fn uninstall(args: &[String]) -> Result<()> {
    let mut purge = false;
    for a in args {
        match a.as_str() {
            "--purge" => purge = PURGE,
            other => bail!("unknown option {other:?}\n{UNINSTALL_USAGE}"),
        }
    }
    let dir = install_dir()?;
    os::stop();
    os::disable()?;
    remove_desktop_integration();
    if let Err(e) = onpath::remove(&dir) {
        println!("Could not take Kairos off your PATH ({e:#}).");
    }
    crate::editors::remove_added();
    kairos_core::sources::amp::remove_plugin();
    match crate::wakatime::release() {
        Ok(Some(line)) => println!("{line}"),
        Ok(None) => {}
        Err(e) => println!("Could not put back your WakaTime settings ({e:#})."),
    }
    if let Ok(Some(_lock)) = update::lock(&paths::data_dir()) {
        update::cleanup(&dir);
    }
    let mut busy = Vec::new();
    for name in ["kairos", "kairosd"] {
        let path = dir.join(exe(name));
        if !remove(&path) {
            busy.push(path);
        }
    }
    if busy.is_empty() {
        let _ = fs::remove_dir(&dir);
    } else {
        os::remove_later(&busy, &dir)?;
    }
    println!("Kairos was removed and will no longer start when you log in.");
    let data = paths::data_dir();
    if purge {
        match fs::remove_dir_all(&data) {
            Ok(()) => println!("Deleted your tracked data in {}.", data.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => bail!("could not delete {}: {e}", data.display()),
        }
    } else {
        println!("Your tracked data stays in {}.", data.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::gen::*;
    use std::path::Path;

    #[test]
    fn systemd_unit_restarts_on_failure_and_exit_75() {
        let u = systemd_unit(Path::new("/home/a b/.local/bin/kairosd"));
        assert!(u.contains("ExecStart=\"/home/a b/.local/bin/kairosd\"\n"));
        assert!(u.contains("\nRestart=on-failure\n"));
        assert!(u.contains("\nRestartSec=5\n"));
        assert!(u.contains("\nRestartForceExitStatus=75\n"));
        assert!(u.contains("\nWantedBy=default.target\n"));
        let odd = systemd_unit(Path::new("/home/x\"%$\\/kairosd"));
        assert!(odd.contains(r#"ExecStart="/home/x\"%%$$\\/kairosd""#), "{odd}");
    }

    #[test]
    fn desktop_entry_quotes_the_path() {
        let d = desktop_entry(Path::new("/home/a b/.local/bin/kairosd"));
        assert!(d.starts_with("[Desktop Entry]\nType=Application\n"));
        assert!(d.contains("\nExec=\"/home/a b/.local/bin/kairosd\"\n"));
        let odd = desktop_entry(Path::new("/home/$x%/kairosd"));
        assert!(odd.contains(r#"Exec="/home/\\$x%%/kairosd""#), "{odd}");
    }

    #[test]
    fn launch_agent_stays_quit_but_restarts_after_crashes() {
        let p = launch_agent(Path::new("/Users/a&b/bin/kairosd"), Path::new("/tmp/x.log"));
        assert!(p.contains("<string>/Users/a&amp;b/bin/kairosd</string>"));
        assert!(p.contains("<key>Label</key><string>dev.obayy.kairos</string>"));
        assert!(p.contains("<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>"));
        assert!(p.contains("<key>StandardErrorPath</key><string>/tmp/x.log</string>"));
        assert!(p.contains("<key>StandardOutPath</key><string>/tmp/x.log</string>"));
    }

    #[test]
    fn windows_registry_values() {
        let dir = Path::new(r"C:\Users\a b\AppData\Local\Programs\Kairos");
        assert_eq!(run_value(&dir.join("kairosd.exe")), format!("\"{}\"", dir.join("kairosd.exe").display()));
        let v = uninstall_values(dir, "1.2.3", 9000);
        let get = |k: &str| v.iter().find(|(n, _)| *n == k).map(|(_, v)| v).unwrap();
        assert_eq!(get("DisplayName"), &RegValue::Str("Kairos".into()));
        assert_eq!(get("DisplayVersion"), &RegValue::Str("1.2.3".into()));
        assert_eq!(get("Publisher"), &RegValue::Str("Kairos".into()));
        assert_eq!(get("InstallLocation"), &RegValue::Str(dir.display().to_string()));
        assert_eq!(get("UninstallString"), &RegValue::Str(format!("\"{}\" uninstall", dir.join("kairos.exe").display())));
        assert_eq!(get("NoModify"), &RegValue::Dword(1));
        assert_eq!(get("NoRepair"), &RegValue::Dword(1));
        assert_eq!(get("EstimatedSize"), &RegValue::Dword(9000));
        assert!(UNINSTALL_KEY.ends_with(r"\Uninstall\Kairos") && RUN_KEY.ends_with(r"\Run"));
    }

    fn expand(s: &str) -> String {
        s.replace("%LOCALAPPDATA%", r"C:\Users\a\AppData\Local")
    }

    const DIR: &str = r"C:\Users\a\AppData\Local\Programs\Kairos";

    #[test]
    fn windows_path_gets_the_install_folder_once() {
        assert_eq!(path_with("", DIR, expand), Some(DIR.to_string()));
        assert_eq!(path_with(r"C:\bin;D:\tools", DIR, expand), Some(format!(r"C:\bin;D:\tools;{DIR}")));
        assert_eq!(path_with(r"C:\bin;", DIR, expand), Some(format!(r"C:\bin;{DIR};")));
        assert_eq!(path_with(&format!(r"C:\bin;{DIR}"), DIR, expand), None);
        assert_eq!(path_with(&format!(r"C:\bin;{DIR}\"), DIR, expand), None, "trailing backslash");
        assert_eq!(path_with(r"C:\bin;c:\users\A\appdata\local\programs\kairos", DIR, expand), None, "case");
        assert_eq!(path_with(r"C:\bin;%LOCALAPPDATA%\Programs\Kairos", DIR, expand), None, "variables");
        assert_eq!(path_with(&format!(r#"C:\bin;"{DIR}""#), DIR, expand), None, "quoted");
        assert!(path_with(&format!(r"C:\bin;{DIR}2"), DIR, expand).is_some(), "a longer name is another folder");
    }

    #[test]
    fn windows_path_loses_only_the_install_folder() {
        assert_eq!(path_without(&format!(r"C:\bin;{DIR};D:\tools"), DIR, expand), Some(r"C:\bin;D:\tools".to_string()));
        assert_eq!(path_without(&format!("{DIR};C:\\bin"), DIR, expand), Some(r"C:\bin".to_string()));
        assert_eq!(path_without(&format!(r"C:\bin;{DIR}"), DIR, expand), Some(r"C:\bin".to_string()));
        assert_eq!(path_without(DIR, DIR, expand), Some(String::new()));
        assert_eq!(path_without(r"%LOCALAPPDATA%\Programs\Kairos\;C:\bin;", DIR, expand), Some(r"C:\bin;".to_string()));
        assert_eq!(path_without(r"C:\bin;;D:\tools", DIR, expand), None);
        assert_eq!(path_without("", DIR, expand), None);
        let added = path_with(r"C:\bin;%SystemRoot%", DIR, expand).unwrap();
        assert_eq!(path_without(&added, DIR, expand), Some(r"C:\bin;%SystemRoot%".to_string()));
        let added = path_with(r"C:\bin;%SystemRoot%;", DIR, expand).unwrap();
        assert_eq!(path_without(&added, DIR, expand), Some(r"C:\bin;%SystemRoot%;".to_string()), "a closing semicolon stays");
    }

    #[test]
    fn path_line_is_quoted_and_relative_to_home() {
        let home = Path::new("/Users/a b");
        let mac = Path::new("/Users/a b/Library/Application Support/Kairos/bin");
        assert_eq!(
            path_line(mac, home, false),
            r#"export PATH="$HOME/Library/Application Support/Kairos/bin:$PATH" # kairos"#
        );
        assert_eq!(
            path_line(mac, home, true),
            r#"fish_add_path -g "$HOME/Library/Application Support/Kairos/bin" # kairos"#
        );
        assert_eq!(
            path_line(Path::new("/Users/a b/.local/bin"), home, false),
            r#"export PATH="$HOME/.local/bin:$PATH" # kairos"#
        );
        assert_eq!(path_line(Path::new("/opt/k$x"), home, false), r#"export PATH="/opt/k\$x:$PATH" # kairos"#);
    }

    #[test]
    fn startup_files_follow_the_shell() {
        let home = Path::new("/h");
        let files = |shell, mac| startup_files(shell, home, mac);
        assert_eq!(files("/usr/bin/zsh", false), vec![(home.join(".zshrc"), false)]);
        assert_eq!(files("/bin/bash", false), vec![(home.join(".bashrc"), false)]);
        assert_eq!(files("/bin/bash", true), vec![(home.join(".bashrc"), false), (home.join(".bash_profile"), false)]);
        assert_eq!(files("/opt/homebrew/bin/fish", true), vec![(home.join(".config/fish/conf.d/kairos.fish"), true)]);
        assert!(files("/usr/bin/nu", false).is_empty());
        assert!(files("", false).is_empty());
    }

    #[test]
    fn rc_add_is_idempotent_and_keeps_the_rest() {
        let line = "export PATH=\"$HOME/.local/bin:$PATH\" # kairos";
        assert_eq!(rc_add("", line), Some(format!("{line}\n")));
        let once = rc_add("alias a=b\nexport X=1\n", line).unwrap();
        assert_eq!(once, format!("alias a=b\nexport X=1\n{line}\n"));
        assert_eq!(rc_add(&once, line), None);
        assert_eq!(rc_add("alias a=b", line), Some(format!("alias a=b\n{line}\n")), "no trailing newline");
        assert_eq!(rc_add(&format!("{line}\r\n"), line), None, "a CRLF file");
    }

    #[test]
    fn rc_remove_takes_only_our_line() {
        let line = "export PATH=\"$HOME/.local/bin:$PATH\" # kairos";
        let other = "export PATH=\"$HOME/.local/bin:$PATH\"";
        assert_eq!(rc_remove("", line), None);
        assert_eq!(rc_remove("alias a=b\n", line), None);
        assert_eq!(rc_remove(&format!("a\n{line}\nb\n"), line), Some("a\nb\n".to_string()));
        assert_eq!(rc_remove(&format!("{line}\n"), line), Some(String::new()));
        assert_eq!(rc_remove(&format!("a\n{line}"), line), Some("a\n".to_string()), "last line without newline");
        assert_eq!(rc_remove(&format!("a\r\n{line}\r\nb"), line), Some("a\r\nb".to_string()));
        assert_eq!(rc_remove(&format!("{other}\n{line}\n"), line), Some(format!("{other}\n")), "a user's own line stays");
        let base = "# mine\n\nexport A=1\n";
        assert_eq!(rc_remove(&rc_add(base, line).unwrap(), line), Some(base.to_string()));
    }

    #[cfg(not(windows))]
    #[test]
    fn shell_files_are_edited_on_disk() {
        use super::onpath::{add_in, remove_in};
        use std::ffi::OsString;

        let home = std::env::temp_dir().join(format!("kairos-onpath-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let dir = home.join("Library/Application Support/Kairos/bin");
        let line = r#"export PATH="$HOME/Library/Application Support/Kairos/bin:$PATH" # kairos"#;
        let rc = home.join(".bashrc");
        let profile = home.join(".bash_profile");
        std::fs::write(&rc, "alias a=b").unwrap();

        let elsewhere = Some(OsString::from("/usr/bin:/bin"));
        let msg = add_in(&dir, &home, "/bin/bash", true, elsewhere.clone()).unwrap().unwrap();
        assert!(msg.contains("~/.bashrc and ~/.bash_profile") && msg.contains("new terminal"), "{msg}");
        assert_eq!(std::fs::read_to_string(&rc).unwrap(), format!("alias a=b\n{line}\n"));
        assert_eq!(std::fs::read_to_string(&profile).unwrap(), format!("{line}\n"));
        assert_eq!(add_in(&dir, &home, "/bin/bash", true, elsewhere.clone()).unwrap(), None, "second install");

        let on_path = std::env::join_paths(["/usr/bin", dir.to_str().unwrap()]).unwrap();
        let fish = add_in(&dir, &home, "/usr/bin/fish", true, Some(on_path)).unwrap();
        assert_eq!(fish, None, "already on PATH");
        assert!(!home.join(".config").exists());

        let fish = add_in(&dir, &home, "/usr/bin/fish", true, elsewhere.clone()).unwrap().unwrap();
        assert!(fish.contains("~/.config/fish/conf.d/kairos.fish"), "{fish}");
        let unknown = add_in(&dir, &home, "/usr/bin/nu", true, elsewhere).unwrap().unwrap();
        assert!(unknown.ends_with(line), "{unknown}");

        remove_in(&dir, &home).unwrap();
        assert_eq!(std::fs::read_to_string(&rc).unwrap(), "alias a=b\n");
        assert!(!profile.exists() && !home.join(".config/fish/conf.d/kairos.fish").exists());
        remove_in(&dir, &home).unwrap();
        let _ = std::fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn a_macos_bash_user_keeps_their_profile_and_a_linked_file_stays_a_link() {
        use super::onpath::{add_in, remove_in};
        use std::os::unix::fs::PermissionsExt;

        let home = std::env::temp_dir().join(format!("kairos-onpath-profile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("dotfiles")).unwrap();
        let dir = home.join("bin");
        let line = r#"export PATH="$HOME/bin:$PATH" # kairos"#;
        let real = home.join("dotfiles/profile");
        std::fs::write(&real, "export MINE=1\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&real, home.join(".profile")).unwrap();

        let msg = add_in(&dir, &home, "/bin/bash", true, None).unwrap().unwrap();
        assert!(msg.contains("~/.bashrc and ~/.profile"), "{msg}");
        assert!(!home.join(".bash_profile").exists());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), format!("export MINE=1\n{line}\n"));

        remove_in(&dir, &home).unwrap();
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "export MINE=1\n");
        assert!(std::fs::symlink_metadata(home.join(".profile")).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(!home.join(".bashrc").exists());
        let names: Vec<_> = std::fs::read_dir(home.join("dotfiles")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["profile"]);
        let _ = std::fs::remove_dir_all(&home);
    }
}
