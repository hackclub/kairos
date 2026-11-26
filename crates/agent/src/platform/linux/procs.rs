use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use crate::classify::{normalize, Procs, Tree};

pub struct ProcFs {
    tree: Tree,
}

impl ProcFs {
    pub fn scan() -> ProcFs {
        let mut tree = Tree::default();
        for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
            let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else { continue };
            let Some(pp) = rest.split_whitespace().nth(1).and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            tree.insert(pid, pp);
        }
        ProcFs { tree }
    }

    pub fn show(&mut self, owner: u32, pids: &[u32]) {
        self.tree.show(owner, pids);
    }
}

/// When the pty was last read, from `/dev/pts`.
pub fn tty_atime(dev: u64) -> Option<f64> {
    let matching = |p: PathBuf| fs::metadata(&p).ok().filter(|m| m.rdev() == dev);
    let minor = (dev & 0xff) | ((dev >> 12) & 0xfff00);
    let meta = matching(PathBuf::from(format!("/dev/pts/{minor}")))
        .or_else(|| fs::read_dir("/dev/pts").ok()?.flatten().find_map(|e| matching(e.path())))?;
    crate::platform::ttys::secs(meta.accessed().ok()?)
}

impl Procs for ProcFs {
    const TITLE_HINTS: bool = false;

    fn tree(&self) -> &Tree {
        &self.tree
    }

    fn name(&self, pid: u32) -> String {
        self.exe(pid)
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .map(|n| normalize(n.trim_end_matches(" (deleted)")))
            .unwrap_or_default()
    }

    fn comm(&self, pid: u32) -> String {
        fs::read_to_string(format!("/proc/{pid}/comm")).map(|s| normalize(s.trim())).unwrap_or_default()
    }

    fn exe(&self, pid: u32) -> Option<PathBuf> {
        fs::read_link(format!("/proc/{pid}/exe")).ok()
    }

    fn cwd(&self, pid: u32) -> Option<PathBuf> {
        fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    fn cmdline(&self, pid: u32) -> Vec<String> {
        fs::read(format!("/proc/{pid}/cmdline"))
            .map(|b| {
                b.split(|&c| c == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn tty(&self, pid: u32) -> Option<u64> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = &stat[stat.rfind(')')? + 1..];
        let tty = rest.split_whitespace().nth(4)?.parse::<i64>().ok()?;
        u64::try_from(tty).ok().filter(|t| *t != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::Procs;

    #[test]
    fn a_pty_read_is_seen_and_its_process_named() {
        crate::platform::ttys::check_a_real_terminal(tty_atime, |pid| ProcFs::scan().tty(pid));
    }
}

