use std::collections::HashMap;
use std::ffi::{c_void, OsString};
use std::mem::{size_of, size_of_val};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use kairos_core::exclude::Exclusions;
use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation, ProcessHandleInformation};
use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, ERROR_SUCCESS, HANDLE, HWND, INVALID_HANDLE_VALUE, STATUS_INFO_LENGTH_MISMATCH,
};
use windows_sys::Win32::Storage::Packaging::Appx::GetPackageFamilyName;
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessId, GetProcessIoCounters, OpenProcess, QueryFullProcessImageNameW, IO_COUNTERS,
    PROCESS_DUP_HANDLE, PROCESS_NAME_WIN32, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetClassNameW, GetForegroundWindow, GetWindow, GetWindowTextW, GetWindowThreadProcessId, GW_OWNER,
};

use crate::classify::{classify_with, is_agent_title, is_console_host, normalize, Classified, Procs, Tree, CONSOLE_WINDOW};
use crate::diag::diag;
use crate::platform::tabs::{ActiveTabs, Tab};
use crate::platform::{Backends, Foreground, InputWatch, Sinks};
use crate::{now, Msg};

const POLL: Duration = Duration::from_secs(1);
const IDLE_MS: u32 = 60_000;
const PSEUDO_CONSOLE: &str = "PseudoConsoleWindow";
const MAX_SESSIONS: usize = 512;
// ProcessConsoleHostProcess: the pid of the process's console host, with flags in the low two bits.
const CONSOLE_HOST_PROCESS: i32 = 49;

// 64-bit layout of PEB.ProcessParameters, RTL_USER_PROCESS_PARAMETERS.CurrentDirectory.DosPath and .CommandLine.
#[cfg(target_pointer_width = "64")]
const PEB_PARAMS: usize = 0x20;
#[cfg(target_pointer_width = "64")]
const CURRENT_DIR: usize = 0x38;
#[cfg(target_pointer_width = "64")]
const COMMAND_LINE: usize = 0x70;
#[cfg(target_pointer_width = "32")]
const PEB_PARAMS: usize = 0x10;
#[cfg(target_pointer_width = "32")]
const CURRENT_DIR: usize = 0x24;
#[cfg(target_pointer_width = "32")]
const COMMAND_LINE: usize = 0x40;
// UNICODE_STRING is { u16 Length, u16 MaximumLength, PWSTR Buffer } and the pointer is aligned.
const USTR_BUFFER: usize = size_of::<usize>();

struct Handle(HANDLE);

impl Handle {
    fn open(pid: u32, access: u32) -> Option<Handle> {
        let h = unsafe { OpenProcess(access, 0, pid) };
        (!h.is_null()).then_some(Handle(h))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn read_bytes(h: &Handle, addr: usize, out: &mut [u8]) -> bool {
    let mut read = 0usize;
    let ok = unsafe { ReadProcessMemory(h.0, addr as *const c_void, out.as_mut_ptr().cast(), out.len(), &mut read) };
    ok != 0 && read == out.len()
}

fn read_usize(h: &Handle, addr: usize) -> Option<usize> {
    let mut b = [0u8; size_of::<usize>()];
    read_bytes(h, addr, &mut b).then(|| usize::from_ne_bytes(b))
}

fn process_string(pid: u32, field: usize) -> Option<String> {
    let h = Handle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ)?;
    // PROCESS_BASIC_INFORMATION is six pointer-sized slots, PebBaseAddress is the second.
    let mut info = [0usize; 6];
    let status = unsafe {
        NtQueryInformationProcess(h.0, ProcessBasicInformation, info.as_mut_ptr().cast(), size_of_val(&info) as u32, null_mut())
    };
    if status < 0 || info[1] == 0 {
        return None;
    }
    let params = read_usize(&h, info[1] + PEB_PARAMS).filter(|p| *p != 0)?;
    let mut head = [0u8; 2 * size_of::<usize>()];
    if !read_bytes(&h, params + field, &mut head) {
        return None;
    }
    let len = u16::from_ne_bytes([head[0], head[1]]) as usize;
    let buf = usize::from_ne_bytes(head[USTR_BUFFER..].try_into().ok()?);
    if len == 0 || len % 2 != 0 || buf == 0 {
        return None;
    }
    let mut bytes = vec![0u8; len];
    if !read_bytes(&h, buf, &mut bytes) {
        return None;
    }
    let wide: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_ne_bytes([c[0], c[1]])).collect();
    Some(String::from_utf16_lossy(&wide))
}

fn split_command_line(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut started = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    args.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        args.push(cur);
    }
    args
}

#[derive(Default)]
struct Snapshot {
    tree: Tree,
    names: HashMap<u32, String>,
}

impl Snapshot {
    fn refresh(&mut self) {
        self.tree.clear();
        self.names.clear();
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snap == INVALID_HANDLE_VALUE {
            return;
        }
        let snap = Handle(snap);
        let mut entry = PROCESSENTRY32W::default();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = unsafe { Process32FirstW(snap.0, &mut entry) };
        while ok != 0 {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            self.names.insert(entry.th32ProcessID, normalize(&String::from_utf16_lossy(&entry.szExeFile[..len])));
            self.tree.insert(entry.th32ProcessID, entry.th32ParentProcessID);
            ok = unsafe { Process32NextW(snap.0, &mut entry) };
        }
    }

    fn session(&self, root: u32) -> Vec<u32> {
        if is_console_host(&self.name(root)) {
            console_clients(root)
        } else {
            vec![root]
        }
    }
}

/// A ConPTY session (a terminal tab or pane). Its console host keeps a hidden PseudoConsoleWindow
/// that the terminal makes the owner of its own window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pseudo {
    hwnd: usize,
    window: usize,
    /// The process Windows names for the session's window: the session's first shell when the
    /// terminal started it, or the console host for a console handed to the terminal.
    root: u32,
}

fn pseudo_consoles() -> Vec<Pseudo> {
    let class: Vec<u16> = PSEUDO_CONSOLE.encode_utf16().chain([0]).collect();
    let mut out = Vec::new();
    let mut hwnd = null_mut();
    for _ in 0..MAX_SESSIONS {
        hwnd = unsafe { FindWindowExW(null_mut(), hwnd, class.as_ptr(), null()) };
        if hwnd.is_null() {
            break;
        }
        let owner = unsafe { GetWindow(hwnd, GW_OWNER) };
        if let (false, Some(root)) = (owner.is_null(), window_pid(hwnd)) {
            out.push(Pseudo { hwnd: hwnd as usize, window: owner as usize, root });
        }
    }
    out
}

fn console_host(root: u32) -> Option<Handle> {
    let h = Handle::open(root, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut info = 0usize;
    let status = unsafe {
        NtQueryInformationProcess(h.0, CONSOLE_HOST_PROCESS, (&mut info as *mut usize).cast(), size_of::<usize>() as u32, null_mut())
    };
    if status < 0 {
        return None;
    }
    match (info & !3) as u32 {
        0 => image_path(&h)
            .and_then(|p| p.file_name().map(|n| normalize(&n.to_string_lossy())))
            .is_some_and(|n| is_console_host(&n))
            .then_some(h),
        host if host == root => Some(h),
        host => Handle::open(host, PROCESS_QUERY_LIMITED_INFORMATION),
    }
}

fn image_path(h: &Handle) -> Option<PathBuf> {
    let mut buf = [0u16; 2048];
    let mut len = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) };
    (ok != 0).then(|| PathBuf::from(OsString::from_wide(&buf[..len as usize])))
}

fn reads(host: &Handle) -> Option<u64> {
    let mut io = IO_COUNTERS::default();
    (unsafe { GetProcessIoCounters(host.0, &mut io) } != 0).then_some(io.ReadOperationCount)
}

#[derive(Default)]
struct Sessions {
    hosts: HashMap<usize, (u32, Option<Handle>)>,
    active: ActiveTabs,
}

impl Sessions {
    fn poll(&mut self, consoles: &[Pseudo]) {
        self.hosts.retain(|hwnd, (root, _)| consoles.iter().any(|c| c.hwnd == *hwnd && c.root == *root));
        let tabs: Vec<Tab> = consoles
            .iter()
            .map(|c| {
                let (_, host) = self.hosts.entry(c.hwnd).or_insert((c.root, None));
                if host.is_none() {
                    *host = console_host(c.root);
                }
                Tab { id: c.hwnd, window: c.window, reads: host.as_ref().and_then(reads) }
            })
            .collect();
        self.active.update(&tabs);
    }

    fn shown(&self, consoles: &[Pseudo], window: usize) -> Vec<u32> {
        let active = self.active.of(window);
        let mut roots: Vec<u32> = consoles
            .iter()
            .filter(|c| c.window == window && active.is_none_or(|a| a.contains(&c.hwnd)))
            .map(|c| c.root)
            .collect();
        roots.sort_unstable();
        roots.dedup();
        roots
    }
}

fn window_pid(hwnd: HWND) -> Option<u32> {
    if hwnd.is_null() {
        return None;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    (pid != 0).then_some(pid)
}

#[repr(C)]
#[derive(Clone, Copy)]
struct HandleEntry {
    value: HANDLE,
    _handle_count: usize,
    _pointer_count: usize,
    _granted_access: u32,
    object_type: u32,
    _attributes: u32,
    _reserved: u32,
}

fn handle_table(process: HANDLE) -> Vec<HandleEntry> {
    // PROCESS_HANDLE_SNAPSHOT_INFORMATION: a count and a reserved slot, then the entries.
    const HEADER: usize = 2;
    let mut buf = vec![0usize; 4096];
    for _ in 0..4 {
        let len = (buf.len() * size_of::<usize>()) as u32;
        let mut needed = 0u32;
        let status =
            unsafe { NtQueryInformationProcess(process, ProcessHandleInformation, buf.as_mut_ptr().cast(), len, &mut needed) };
        if status == STATUS_INFO_LENGTH_MISMATCH {
            buf = vec![0; (needed as usize / size_of::<usize>()).max(buf.len()) * 2];
            continue;
        }
        if status < 0 {
            break;
        }
        let fits = (buf.len() - HEADER) * size_of::<usize>() / size_of::<HandleEntry>();
        let entries = unsafe { std::slice::from_raw_parts(buf.as_ptr().add(HEADER).cast::<HandleEntry>(), buf[0].min(fits)) };
        return entries.to_vec();
    }
    Vec::new()
}

fn process_object_type() -> Option<u32> {
    static TYPE: OnceLock<Option<u32>> = OnceLock::new();
    *TYPE.get_or_init(|| {
        let me = Handle::open(std::process::id(), PROCESS_QUERY_LIMITED_INFORMATION)?;
        handle_table(unsafe { GetCurrentProcess() }).iter().find(|e| e.value == me.0).map(|e| e.object_type)
    })
}

fn console_clients(host: u32) -> Vec<u32> {
    let (Some(process_type), Some(h)) = (process_object_type(), Handle::open(host, PROCESS_QUERY_INFORMATION | PROCESS_DUP_HANDLE))
    else {
        return Vec::new();
    };
    let me = unsafe { GetCurrentProcess() };
    let mut clients = Vec::new();
    for entry in handle_table(h.0).iter().filter(|e| e.object_type == process_type) {
        let mut dup = null_mut();
        if unsafe { DuplicateHandle(h.0, entry.value, me, &mut dup, PROCESS_QUERY_LIMITED_INFORMATION, 0, 0) } == 0 {
            continue;
        }
        let dup = Handle(dup);
        let pid = unsafe { GetProcessId(dup.0) };
        if pid != 0 && pid != host && !clients.contains(&pid) {
            clients.push(pid);
        }
    }
    clients
}

impl Procs for Snapshot {
    const TITLE_HINTS: bool = true;

    fn tree(&self) -> &Tree {
        &self.tree
    }

    fn name(&self, pid: u32) -> String {
        self.names.get(&pid).cloned().unwrap_or_default()
    }

    fn exe(&self, pid: u32) -> Option<PathBuf> {
        image_path(&Handle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?)
    }

    fn cwd(&self, pid: u32) -> Option<PathBuf> {
        process_string(pid, CURRENT_DIR).map(PathBuf::from)
    }

    fn cmdline(&self, pid: u32) -> Vec<String> {
        process_string(pid, COMMAND_LINE).map(|l| split_command_line(&l)).unwrap_or_default()
    }
}

// The family name of a packaged (Store or MSIX) app, like `OpenAI.Codex_2p2nqsd0c76g0`.
fn package_family(pid: u32) -> Option<String> {
    let h = Handle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut name = [0u16; 128];
    let mut len = name.len() as u32;
    let ok = unsafe { GetPackageFamilyName(h.0, &mut len, name.as_mut_ptr()) } == ERROR_SUCCESS;
    ok.then(|| String::from_utf16_lossy(&name[..(len as usize).saturating_sub(1).min(name.len())]))
}

// Win32 class names don't name apps (every Electron app is Chrome_WidgetWin_1), so only the console
// class is passed on: the owner Windows reports for a console window is the shell inside it. A packaged app
// passes its family name instead, which tells apart apps whose programs share a name (the Codex app and ChatGPT
// both run ChatGPT.exe).
fn foreground(buf: &mut [u16], sessions: &Sessions, consoles: &[Pseudo]) -> Option<(usize, Foreground)> {
    let hwnd = unsafe { GetForegroundWindow() };
    let pid = window_pid(hwnd)?;
    let n = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) }.max(0) as usize;
    let wclass = if String::from_utf16_lossy(&buf[..n]) == CONSOLE_WINDOW { CONSOLE_WINDOW.to_string() } else { package_family(pid).unwrap_or_default() };
    let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) }.max(0) as usize;
    let title = String::from_utf16_lossy(&buf[..n]);
    let sessions = sessions.shown(consoles, hwnd as usize);
    Some((hwnd as usize, Foreground { pid, wclass, title, sessions, window: hwnd as usize as u64 }))
}

pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    process_string(pid, CURRENT_DIR).map(PathBuf::from)
}

pub fn classify(fg: &Foreground, ex: &Exclusions) -> Classified {
    let mut snap = Snapshot::default();
    snap.refresh();
    if !fg.sessions.is_empty() {
        let shown: Vec<u32> = fg.sessions.iter().flat_map(|&root| snap.session(root)).collect();
        snap.tree.show(fg.pid, &shown);
    }
    classify_with(&snap, fg.pid, &fg.wclass, &fg.title, ex)
}

pub fn spawn(sinks: Sinks) -> Backends {
    thread::spawn(move || focus(sinks.foreground));
    let idle = if spawn_idle(sinks.events) { "GetLastInputInfo" } else { "none" };
    Backends { focus: "GetForegroundWindow", idle }
}

fn focus(out: Sender<Option<Foreground>>) {
    let mut buf = [0u16; 512];
    let mut sessions = Sessions::default();
    let mut seen = None;
    loop {
        let consoles = pseudo_consoles();
        sessions.poll(&consoles);
        let fg = foreground(&mut buf, &sessions, &consoles);
        let key = fg.as_ref().map(|(hwnd, f)| (*hwnd, f.pid, is_agent_title(&f.title), f.sessions.clone()));
        if key != seen {
            seen = key;
            if out.send(fg.map(|(_, f)| f)).is_err() {
                return;
            }
        }
        thread::sleep(POLL);
    }
}

fn idle_ms() -> Option<u32> {
    let mut info = LASTINPUTINFO { cbSize: size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    (unsafe { GetLastInputInfo(&mut info) } != 0).then(|| unsafe { GetTickCount() }.wrapping_sub(info.dwTime))
}

fn spawn_idle(tx: Sender<Msg>) -> bool {
    if idle_ms().is_none() {
        diag!("kairosd: warning: presence unknown, emitting no present events (GetLastInputInfo failed)");
        return false;
    }
    thread::spawn(move || {
        let _ = tx.send(Msg::PresenceReady);
        let _ = tx.send(Msg::InputReady);
        let mut idle = false;
        let mut input = InputWatch::default();
        loop {
            thread::sleep(InputWatch::poll_every(idle));
            let Some(ms) = idle_ms() else { continue };
            if input.observe(now(), f64::from(ms) / 1000.0).is_some_and(|t| tx.send(Msg::Input(t)).is_err()) {
                return;
            }
            let now_idle = ms >= IDLE_MS;
            if now_idle != idle {
                idle = now_idle;
                if tx.send(if idle { Msg::Idled } else { Msg::Resumed }).is_err() {
                    return;
                }
            }
        }
    });
    true
}

#[cfg(test)]
mod tests {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::ptr::null;
    use std::sync::mpsc::{self, Receiver};
    use std::time::Instant;

    use kairos_core::event::Class;
    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::System::SystemInformation::GetTickCount64;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, TerminateProcess, CREATE_NEW_CONSOLE, PROCESS_INFORMATION, PROCESS_TERMINATE, STARTUPINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, IsWindowVisible, SetForegroundWindow};

    use super::*;

    fn classify(fg: &Foreground) -> Classified {
        super::classify(fg, &Exclusions::default())
    }

    const WAIT: Duration = Duration::from_secs(30);
    const RAISE_AFTER: Duration = Duration::from_secs(8);

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kairos-win-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn same_dir(a: &Path, b: &Path) -> bool {
        matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
    }

    fn snapshot() -> Snapshot {
        let mut snap = Snapshot::default();
        snap.refresh();
        snap
    }

    #[test]
    fn reads_own_and_child_processes() {
        let me = std::process::id();
        let snap = snapshot();
        let exe = std::env::current_exe().unwrap();
        assert_eq!(snap.exe(me).unwrap().canonicalize().unwrap(), exe.canonicalize().unwrap());
        assert_eq!(snap.name(me), normalize(&exe.file_name().unwrap().to_string_lossy()));
        assert!(same_dir(&process_cwd(me).unwrap(), &std::env::current_dir().unwrap()));
        assert_eq!(snap.cmdline(me), std::env::args().collect::<Vec<_>>());

        let dir = temp("child");
        let mut child = Command::new("cmd.exe")
            .args(["/d", "/c", "pause"])
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let snap = snapshot();
        let parent = snap.tree().parent(pid);
        let under_me = snap.tree().descendants(me).iter().any(|&(c, _)| c == pid);
        let (name, exe) = (snap.name(pid), snap.exe(pid));
        let in_dir = snap.cwd(pid).is_some_and(|c| same_dir(&c, &dir));
        let args = snap.cmdline(pid);
        let held = console_clients(me);
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(parent, Some(me));
        assert!(under_me);
        assert!(held.contains(&pid), "the child is not among the processes this one holds handles to: {held:?}");
        assert_eq!(name, "cmd");
        assert!(exe.unwrap().file_name().unwrap().eq_ignore_ascii_case("cmd.exe"));
        assert!(in_dir, "the child's working folder read from its PEB");
        assert_eq!(args[1..], ["/d", "/c", "pause"]);
    }

    /// A shell in a console of its own, as a user starts one. `Command` would hand it this
    /// process's pipes, and cmd quits at the end of its input.
    struct Console {
        pid: u32,
        process: Handle,
    }

    impl Console {
        fn open(dir: &Path) -> Console {
            let mut line: Vec<u16> = "cmd.exe /d /k\0".encode_utf16().collect();
            let dir: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
            let startup = STARTUPINFOW { cb: size_of::<STARTUPINFOW>() as u32, ..Default::default() };
            let mut info = PROCESS_INFORMATION::default();
            let ok = unsafe {
                CreateProcessW(
                    null(),
                    line.as_mut_ptr(),
                    null(),
                    null(),
                    0,
                    CREATE_NEW_CONSOLE,
                    null(),
                    dir.as_ptr(),
                    &startup,
                    &mut info,
                )
            };
            assert!(ok != 0, "CreateProcessW: {}", std::io::Error::last_os_error());
            drop(Handle(info.hThread));
            Console { pid: info.dwProcessId, process: Handle(info.hProcess) }
        }
    }

    impl Drop for Console {
        fn drop(&mut self) {
            unsafe { TerminateProcess(self.process.0, 0) };
        }
    }

    struct App(Child);

    impl Drop for App {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn visible_windows() -> Vec<(HWND, u32)> {
        unsafe extern "system" fn add(hwnd: HWND, out: LPARAM) -> BOOL {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if IsWindowVisible(hwnd) != 0 {
                (*(out as *mut Vec<(HWND, u32)>)).push((hwnd, pid));
            }
            1
        }
        let mut out: Vec<(HWND, u32)> = Vec::new();
        unsafe { EnumWindows(Some(add), &mut out as *mut Vec<(HWND, u32)> as LPARAM) };
        out
    }

    fn window_of(is_target: impl Fn(u32) -> bool) -> Option<HWND> {
        visible_windows().into_iter().find(|&(_, pid)| is_target(pid)).map(|(hwnd, _)| hwnd)
    }

    /// A terminal can take the front before it shows the target's session, so each report is checked.
    fn front(
        rx: &Receiver<Option<Foreground>>,
        latest: &mut Option<Foreground>,
        what: &str,
        is_target: impl Fn(&Foreground) -> bool,
        window: impl Fn() -> Option<HWND>,
    ) -> Foreground {
        let start = Instant::now();
        let mut raised = false;
        let mut reports = Vec::new();
        while start.elapsed() < WAIT {
            if let Ok(report) = rx.recv_timeout(POLL) {
                reports.push(report.as_ref().map(|f| (f.pid, snapshot().name(f.pid), f.sessions.clone())));
                *latest = report;
            }
            if let Some(fg) = latest.as_ref().filter(|f| is_target(f)) {
                let (name, after) = (snapshot().name(fg.pid), start.elapsed().as_secs_f32());
                println!("front: {what} as pid {} ({name}), class {:?}, sessions {:?}, after {after:.1} s", fg.pid, fg.wclass, fg.sessions);
                return fg.clone();
            }
            if !raised && start.elapsed() >= RAISE_AFTER {
                raised = true;
                let ok = window().is_some_and(|hwnd| unsafe { SetForegroundWindow(hwnd) } != 0);
                println!("{what} is not in front after {RAISE_AFTER:?}, SetForegroundWindow: {ok}");
            }
        }
        panic!("{what} never came to the front, reports were {reports:?}, ConPTY sessions {:?}", sessions());
    }

    fn sessions() -> Vec<String> {
        let snap = snapshot();
        let named = |pid: u32| format!("{pid} {}", snap.name(pid));
        let session = |c: Pseudo| {
            let runs: Vec<_> = snap.session(c.root).into_iter().map(named).collect();
            let terminal = window_pid(c.window as HWND).map(named);
            let parent = snap.tree().parent(c.root).map(named);
            format!("{terminal:?} window {:#x} shows {} (child of {parent:?}, running {runs:?})", c.window, named(c.root))
        };
        pseudo_consoles().into_iter().map(session).collect()
    }

    fn runs(root: u32, pid: u32) -> bool {
        let snap = snapshot();
        snap.session(root).into_iter().any(|p| p == pid || snap.tree().descendants(p).iter().any(|&(c, _)| c == pid))
    }

    fn idle_backend_is_sane() {
        let ms = idle_ms().expect("GetLastInputInfo answers");
        let up = unsafe { GetTickCount64() };
        println!("idle: {:.1} s since the last input, up {:.0} s", f64::from(ms) / 1000.0, up as f64 / 1000.0);
        assert!(u64::from(ms) <= up, "idle for longer than the machine is up");
        let (tx, rx) = mpsc::channel();
        assert!(spawn_idle(tx));
        assert!(matches!(rx.recv_timeout(Duration::from_secs(2)), Ok(Msg::PresenceReady)));
        if ms >= IDLE_MS {
            let next = std::iter::from_fn(|| rx.recv_timeout(POLL * 3).ok()).find(|m| !matches!(m, Msg::InputReady | Msg::Input(_)));
            assert!(matches!(next, Some(Msg::Idled)), "idle for {ms} ms but no Idled");
        }
    }

    // Needs an interactive desktop; .github/workflows/windows-check.yml runs it on a GitHub runner.
    #[test]
    #[ignore = "launches apps, needs an interactive Windows desktop"]
    fn windows_backends_on_real_apps() {
        idle_backend_is_sane();

        let tmp = temp("apps");
        let repo = tmp.join(format!("kairos-win-proj-{}", std::process::id()));
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || focus(tx));
        let mut latest = None;

        let mut notepad = App(Command::new("notepad.exe").current_dir(&tmp).spawn().expect("notepad"));
        let is_notepad = |pid| snapshot().name(pid) == "notepad";
        let fg = front(&rx, &mut latest, "notepad", |f| is_notepad(f.pid), || window_of(is_notepad));
        let exe = Snapshot::default().exe(fg.pid).expect("notepad's exe");
        println!("notepad exe: {}", exe.display());
        assert!(exe.file_name().unwrap().eq_ignore_ascii_case("notepad.exe"));
        if notepad.0.try_wait().unwrap().is_none() {
            assert_eq!(fg.pid, notepad.0.id());
        }
        let c = classify(&fg);
        assert_eq!((c.class, c.app.as_str()), (Class::Other, "notepad"));

        // Which host draws it depends on the default terminal: a classic console window owned by
        // the shell (or its conhost), or a ConPTY terminal window that shows only the shell's session.
        let console = Console::open(&repo);
        let classic = |pid| pid == console.pid || snapshot().tree().parent(pid) == Some(console.pid);
        let conpty = |f: &Foreground| !f.sessions.is_empty() && f.sessions.iter().all(|&root| runs(root, console.pid));
        let window = || {
            window_of(classic)
                .or_else(|| pseudo_consoles().into_iter().find(|c| runs(c.root, console.pid)).map(|c| c.window as HWND))
        };
        let what = format!("the console (cmd {})", console.pid);
        let fg = front(&rx, &mut latest, &what, |f| classic(f.pid) || conpty(f), window);
        if fg.wclass == CONSOLE_WINDOW {
            assert!(classic(fg.pid));
            println!("console host: classic conhost");
        } else {
            assert!(conpty(&fg), "{:?} in front is neither a console window nor its terminal", fg.wclass);
            println!("console host: ConPTY in {}, sessions {:?}", snapshot().name(fg.pid), sessions());
        }
        let cwd = process_cwd(console.pid).expect("the console's working folder");
        assert!(same_dir(&cwd, &repo), "cwd {} is not {}", cwd.display(), repo.display());
        let c = classify(&fg);
        println!("classified the console: {c:?}");
        assert_eq!(c.class, Class::Terminal);
        let project = c.project.expect("terminal project");
        assert_eq!(project.id, repo.file_name().unwrap().to_string_lossy());
        assert!(same_dir(&project.root, &repo));

        drop((console, notepad));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn wt(args: &[&str]) -> bool {
        match Command::new("wt.exe").args(args).status() {
            Ok(status) => {
                assert!(status.success(), "wt {args:?}: {status}");
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => panic!("wt {args:?}: {e}"),
        }
    }

    fn sessions_in(dirs: &[PathBuf]) -> Vec<Pseudo> {
        let start = Instant::now();
        while start.elapsed() < WAIT {
            let open = pseudo_consoles();
            let found: Vec<Pseudo> = dirs
                .iter()
                .filter_map(|d| open.iter().copied().find(|c| process_cwd(c.root).is_some_and(|w| same_dir(&w, d))))
                .collect();
            if found.len() == dirs.len() {
                return found;
            }
            thread::sleep(POLL / 4);
        }
        panic!("no sessions in {dirs:?}, ConPTY sessions {:?}", sessions());
    }

    struct Shells(Vec<u32>);

    impl Drop for Shells {
        fn drop(&mut self) {
            for &pid in &self.0 {
                if let Some(h) = Handle::open(pid, PROCESS_TERMINATE) {
                    unsafe { TerminateProcess(h.0, 0) };
                }
            }
        }
    }

    // Needs an interactive desktop with Windows Terminal; .github/workflows/windows-check.yml runs it.
    #[test]
    #[ignore = "opens Windows Terminal, needs an interactive Windows desktop"]
    fn windows_terminal_follows_the_tab_in_use() {
        let tmp = temp("tabs");
        let repos: Vec<PathBuf> = ["a", "b", "c"]
            .iter()
            .map(|t| {
                let repo = tmp.join(format!("kairos-tab-{t}-{}", std::process::id()));
                std::fs::create_dir_all(repo.join(".git")).unwrap();
                repo
            })
            .collect();
        let dirs: Vec<String> = repos.iter().map(|r| r.to_string_lossy().into_owned()).collect();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || focus(tx));
        let mut latest = rx.recv_timeout(WAIT).expect("a first report");

        // Two tabs at once, as when Windows Terminal restores a session: either may be the one in use.
        let name = format!("kairos-tabs-{}", std::process::id());
        let cmd = ["cmd", "/d", "/k"];
        let mut args = vec!["-w", name.as_str(), "new-tab", "-d", dirs[0].as_str()];
        args.extend(cmd);
        args.extend([";", "new-tab", "-d", dirs[1].as_str()]);
        args.extend(cmd);
        if !wt(&args) {
            println!("no Windows Terminal here, skipped");
            return;
        }
        let mut tabs = sessions_in(&repos[..2]);
        let mut shells = Shells(tabs.iter().map(|t| t.root).collect());
        let window = tabs[0].window;
        assert_eq!(tabs[1].window, window, "both tabs in one window: {:?}", sessions());
        let raise = || Some(window as HWND);
        let mut both = vec![tabs[0].root, tabs[1].root];
        both.sort_unstable();
        let is_ours = |f: &Foreground| !f.sessions.is_empty() && f.sessions.iter().all(|r| both.contains(r));
        front(&rx, &mut latest, "the new window", is_ours, raise);

        let show = |latest: &mut Option<Foreground>, tabs: &[Pseudo], tab: usize, how: &str| {
            let fg = front(&rx, latest, &format!("tab {tab} after {how}"), |f| f.sessions == [tabs[tab].root], raise);
            let c = classify(&fg);
            println!("classified tab {tab}: {c:?}");
            assert_eq!(c.class, Class::Terminal);
            assert!(same_dir(&c.project.expect("a project").root, &repos[tab]));
        };
        let notepad = App(Command::new("notepad.exe").current_dir(&tmp).spawn().expect("notepad"));
        let is_notepad = |pid| snapshot().name(pid) == "notepad";
        let fg = front(&rx, &mut latest, "notepad", |f| is_notepad(f.pid), || window_of(is_notepad));
        assert!(fg.sessions.is_empty(), "notepad shows no terminal sessions: {:?}", fg.sessions);
        println!("back to the terminal: {}", unsafe { SetForegroundWindow(window as HWND) });
        show(&mut latest, &tabs, 1, "coming back, the tab that was in use");

        assert!(wt(&["-w", &name, "focus-tab", "-t", "0"]));
        show(&mut latest, &tabs, 0, "focus-tab 0");
        assert!(wt(&["-w", &name, "focus-tab", "-t", "1"]));
        show(&mut latest, &tabs, 1, "focus-tab 1");

        let mut args = vec!["-w", name.as_str(), "new-tab", "-d", dirs[2].as_str()];
        args.extend(cmd);
        assert!(wt(&args));
        tabs.push(sessions_in(&repos[2..])[0]);
        shells.0.push(tabs[2].root);
        show(&mut latest, &tabs, 2, "a new tab");
        assert!(wt(&["-w", &name, "focus-tab", "-t", "0"]));
        show(&mut latest, &tabs, 0, "focus-tab 0 again");

        drop((notepad, shells));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
