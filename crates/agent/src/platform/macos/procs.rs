use std::ffi::{c_char, c_int, OsStr, OsString};
use std::mem::{size_of, MaybeUninit};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;

use crate::classify::{normalize, Procs, Tree};

pub struct LibProc {
    tree: Tree,
}

impl LibProc {
    pub fn scan() -> LibProc {
        let mut tree = Tree::default();
        for pid in all_pids() {
            if let Some(info) = short_info(pid) {
                tree.insert(info.pbsi_pid, info.pbsi_ppid);
            }
        }
        LibProc { tree }
    }

    pub fn show(&mut self, owner: u32, pids: &[u32]) {
        self.tree.show(owner, pids);
    }
}

/// When the terminal device was last read.
pub fn tty_atime(dev: u64) -> Option<f64> {
    // SAFETY: devname returns null or a NUL-terminated name in a static buffer, copied right away (one caller thread).
    let name = unsafe { libc::devname(libc::dev_t::try_from(dev).ok()?, libc::S_IFCHR) };
    if name.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy().into_owned();
    let meta = std::fs::metadata(Path::new("/dev").join(name)).ok().filter(|m| m.rdev() == dev)?;
    crate::platform::ttys::secs(meta.accessed().ok()?)
}

impl Procs for LibProc {
    const TITLE_HINTS: bool = false;

    fn tree(&self) -> &Tree {
        &self.tree
    }

    fn name(&self, pid: u32) -> String {
        exe(pid).and_then(|p| p.file_name().map(|n| app_name(&n.to_string_lossy()))).unwrap_or_default()
    }

    // argv[0] is the name the process was started as (`claude`, `-zsh`), where the exe can be a
    // versioned file behind a symlink. Processes of other users only show their short kernel name.
    fn comm(&self, pid: u32) -> String {
        if let Some(argv0) = self.cmdline(pid).first() {
            return base_name(argv0.trim_start_matches('-'));
        }
        let info = c_int::try_from(pid).ok().and_then(short_info);
        info.map(|i| app_name(&c_string(&i.pbsi_comm))).unwrap_or_default()
    }

    fn exe(&self, pid: u32) -> Option<PathBuf> {
        exe(pid)
    }

    fn cwd(&self, pid: u32) -> Option<PathBuf> {
        cwd(pid)
    }

    fn cmdline(&self, pid: u32) -> Vec<String> {
        procargs(pid).map(|b| parse_procargs(&b)).unwrap_or_default()
    }

    // Only the user's own processes say: Terminal's `login` is root's, the shell under it is the first that does.
    fn tty(&self, pid: u32) -> Option<u64> {
        let size = size_of::<libc::proc_bsdinfo>() as c_int;
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        // SAFETY: the buffer is a zeroed proc_bsdinfo of `size` bytes, the flavor's exact struct.
        let n = unsafe {
            libc::proc_pidinfo(c_int::try_from(pid).ok()?, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size)
        };
        if n != size {
            return None;
        }
        // SAFETY: plain data that the kernel filled.
        let dev = unsafe { info.assume_init() }.e_tdev;
        (dev != 0 && dev != u32::MAX).then_some(u64::from(dev))
    }
}

/// Bundle executables have spaces (`Google Chrome`, `Microsoft Edge`); the name lists use dashes.
pub fn app_name(file_name: &str) -> String {
    normalize(&file_name.replace(' ', "-"))
}

fn base_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| app_name(&n.to_string_lossy())).unwrap_or_default()
}

fn c_string(chars: &[c_char]) -> String {
    let bytes: Vec<u8> = chars.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn all_pids() -> Vec<c_int> {
    // A null buffer returns the number of processes plus some slack.
    let n = unsafe { libc::proc_listallpids(null_mut(), 0) };
    let Ok(n) = usize::try_from(n) else { return Vec::new() };
    let mut pids: Vec<c_int> = vec![0; n];
    let bytes = c_int::try_from(pids.len() * size_of::<c_int>()).unwrap_or(c_int::MAX);
    // SAFETY: the buffer holds `bytes` bytes of c_int slots and the kernel writes at most that many.
    let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(usize::try_from(got).unwrap_or(0));
    pids.retain(|&p| p > 0);
    pids
}

// PROC_PIDT_SHORTBSDINFO works on processes of every user (PROC_PIDTBSDINFO doesn't), so the tree
// stays connected through root-owned links such as Terminal -> login -> zsh.
fn short_info(pid: c_int) -> Option<libc::proc_bsdshortinfo> {
    let size = size_of::<libc::proc_bsdshortinfo>() as c_int;
    let mut info = MaybeUninit::<libc::proc_bsdshortinfo>::zeroed();
    // SAFETY: the buffer is a zeroed proc_bsdshortinfo of `size` bytes, the flavor's exact struct.
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDT_SHORTBSDINFO, 0, info.as_mut_ptr().cast(), size) };
    // SAFETY: the struct is plain integers and chars, valid when zeroed and fully written on success.
    (n == size).then(|| unsafe { info.assume_init() })
}

pub fn exe(pid: u32) -> Option<PathBuf> {
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is PROC_PIDPATHINFO_MAXSIZE bytes, the size libproc requires.
    let n = unsafe { libc::proc_pidpath(c_int::try_from(pid).ok()?, buf.as_mut_ptr().cast(), buf.len() as u32) };
    let n = usize::try_from(n).ok().filter(|&n| n > 0 && n <= buf.len())?;
    Some(PathBuf::from(OsString::from_vec(buf[..n].to_vec())))
}

pub fn cwd(pid: u32) -> Option<PathBuf> {
    let size = size_of::<libc::proc_vnodepathinfo>() as c_int;
    let mut info = MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    // SAFETY: the buffer is a zeroed proc_vnodepathinfo of `size` bytes, the flavor's exact struct.
    let n = unsafe {
        libc::proc_pidinfo(c_int::try_from(pid).ok()?, libc::PROC_PIDVNODEPATHINFO, 0, info.as_mut_ptr().cast(), size)
    };
    if n != size {
        return None;
    }
    // SAFETY: as above, plain data that the kernel filled.
    let info = unsafe { info.assume_init() };
    let path: Vec<u8> =
        info.pvi_cdir.vip_path.as_flattened().iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    (!path.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(&path)))
}

fn procargs(pid: u32) -> Option<Vec<u8>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, c_int::try_from(pid).ok()?];
    let mut len: libc::size_t = 0;
    // SAFETY: a null buffer only asks for the size, written to `len`.
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, null_mut(), &mut len, null_mut(), 0) } != 0 || len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len];
    // SAFETY: the buffer is `len` bytes and the kernel writes at most `len`, updating it.
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut len, null_mut(), 0) } != 0 {
        return None;
    }
    buf.truncate(len);
    Some(buf)
}

// KERN_PROCARGS2 is argc (a native int), the exec path, NUL padding, then argc NUL-terminated
// arguments followed by the environment.
fn parse_procargs(buf: &[u8]) -> Vec<String> {
    let Some((argc, rest)) = buf.split_first_chunk::<4>() else { return Vec::new() };
    let argc = usize::try_from(i32::from_ne_bytes(*argc)).unwrap_or(0);
    let Some(path_end) = rest.iter().position(|&b| b == 0) else { return Vec::new() };
    let rest = &rest[path_end..];
    let Some(start) = rest.iter().position(|&b| b != 0) else { return Vec::new() };
    rest[start..]
        .split(|&b| b == 0)
        .take(argc)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn procargs_buf(argc: i32, exec: &str, pad: usize, rest: &[&str]) -> Vec<u8> {
        let mut b = argc.to_ne_bytes().to_vec();
        b.extend_from_slice(exec.as_bytes());
        b.extend(std::iter::repeat_n(0, pad));
        for s in rest {
            b.extend_from_slice(s.as_bytes());
            b.push(0);
        }
        b
    }

    #[test]
    fn procargs_split_argv_from_path_padding_and_env() {
        let argv = ["node", "/x/claude-code/cli.js", "--resume", "HOME=/u", "TERM=x"];
        let b = procargs_buf(3, "/usr/local/bin/node", 5, &argv);
        assert_eq!(parse_procargs(&b), ["node", "/x/claude-code/cli.js", "--resume"]);
        let b = procargs_buf(1, "/bin/zsh", 1, &["-zsh", "PATH=/bin"]);
        assert_eq!(parse_procargs(&b), ["-zsh"]);
        let b = procargs_buf(2, "/bin/echo", 3, &["echo", ""]);
        assert_eq!(parse_procargs(&b), ["echo", ""]);
    }

    #[test]
    fn procargs_tolerate_short_or_odd_buffers() {
        assert!(parse_procargs(&[]).is_empty());
        assert!(parse_procargs(&[1, 0]).is_empty());
        assert!(parse_procargs(&procargs_buf(2, "/bin/sh", 0, &[])).is_empty());
        assert!(parse_procargs(&procargs_buf(-1, "/bin/sh", 1, &["sh"])).is_empty());
    }

    #[test]
    fn a_tty_read_is_seen_and_its_process_named() {
        crate::platform::ttys::check_a_real_terminal(tty_atime, |pid| LibProc::scan().tty(pid));
    }

    #[test]
    fn bundle_executables_match_the_name_lists() {
        assert_eq!(app_name("Google Chrome"), "google-chrome");
        assert_eq!(app_name("Microsoft Edge"), "microsoft-edge");
        assert_eq!(app_name("Brave Browser"), "brave-browser");
        assert_eq!(app_name("iTerm2"), "iterm2");
        assert_eq!(app_name("Xcode"), "xcode");
        assert_eq!(base_name("-zsh".trim_start_matches('-')), "zsh");
        assert_eq!(base_name("/Users/a/.local/bin/claude"), "claude");
    }
}
