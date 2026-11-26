//! Commit messages (the reflog's included) and diff contents never leave: output is parsed as a stream
//! and only shas, dates, paths and line counts are kept.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use kairos_core::event::{Event, FileChange, Kind, Via, MAX_CHANGES, MAX_PATH_LEN};
use notify::{EventKind, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};

use crate::diag::diag;
use crate::exclusions::Excluded;
use crate::{now, Msg};

const STATE_FILE: &str = "git-state.json";
const TICK: Duration = Duration::from_secs(1);
const GIT_TIMEOUT: Duration = Duration::from_secs(20);
const DEBOUNCE: Duration = Duration::from_millis(1500);
const DIFF_EVERY: Duration = Duration::from_secs(300);
const SWEEP: Duration = Duration::from_secs(600);
const RETRY: Duration = Duration::from_secs(600);
const MAX_REPOS: usize = 32;
const RESTORE: usize = 16;
const MAX_SAVED: usize = 128;
const MAX_SEEN: usize = 2000;
const MAX_COMMITS: usize = 100;
const MAX_OUT: usize = 1 << 20;
const MAX_REFLOG: usize = 300;
const UNTRACKED_FILE_MAX: u64 = 512 << 10;
const UNTRACKED_BUDGET: u64 = 4 << 20;
const BINARY_PROBE: usize = 8000;
// Older commits predate the daemon. Skipping them also keeps their timestamps from dragging the server's recompute far back.
const MAX_AGE: f64 = 48.0 * 3600.0;

pub enum Cmd {
    Root(PathBuf, String),
    Activity(PathBuf),
    Fs(Vec<PathBuf>),
}

#[derive(Debug)]
enum Fail {
    Gone,
    NoGit,
    Exit,
    Timeout,
    TooBig,
}

trait Sink: Send + 'static {
    fn feed(&mut self, chunk: &[u8]);
}

#[derive(Default)]
struct Buf {
    data: Vec<u8>,
    over: bool,
}

impl Sink for Buf {
    fn feed(&mut self, chunk: &[u8]) {
        let room = MAX_OUT - self.data.len();
        self.data.extend_from_slice(&chunk[..chunk.len().min(room)]);
        self.over |= chunk.len() > room;
    }
}

#[derive(Default, Debug, Clone, PartialEq)]
struct Stat {
    files: u32,
    added: u32,
    removed: u32,
    changes: Vec<FileChange>,
}

/// Bounded memory: pruned to the largest entries at twice the final size, which never drops one that belongs in the final list.
#[derive(Default)]
struct Tally {
    files: u32,
    added: u32,
    removed: u32,
    top: Vec<FileChange>,
}

impl Tally {
    fn count(&mut self, added: u32, removed: u32) {
        self.files = self.files.saturating_add(1);
        self.added = self.added.saturating_add(added);
        self.removed = self.removed.saturating_add(removed);
    }

    fn list(&mut self, path: String, added: u32, removed: u32) {
        self.top.push(FileChange { path, added, removed });
        if self.top.len() >= 2 * MAX_CHANGES {
            self.prune();
        }
    }

    fn prune(&mut self) {
        let size = |c: &FileChange| c.added as u64 + c.removed as u64;
        self.top.sort_by(|a, b| Reverse(size(a)).cmp(&Reverse(size(b))).then_with(|| a.path.cmp(&b.path)));
        self.top.truncate(MAX_CHANGES);
    }

    fn finish(mut self) -> Stat {
        self.prune();
        self.top.sort_by(|a, b| a.path.cmp(&b.path));
        Stat { files: self.files, added: self.added, removed: self.removed, changes: self.top }
    }
}

const REC_CAP: usize = MAX_PATH_LEN + 64;

/// `git ... --numstat -z` records: `added TAB removed TAB path NUL`, and for a rename or copy
/// `added TAB removed TAB NUL old NUL new NUL` (the new path is the one reported).
#[derive(Default)]
struct Numstat {
    tally: Tally,
    rec: Vec<u8>,
    len: usize,
    rename: Option<(u32, u32, u8)>,
}

impl Numstat {
    fn list(&mut self, added: u32, removed: u32, path: &[u8], len: usize) {
        if len == 0 || len > MAX_PATH_LEN {
            return;
        }
        let path = String::from_utf8_lossy(path);
        if path.len() <= MAX_PATH_LEN {
            self.tally.list(path.into_owned(), added, removed);
        }
    }

    fn end(&mut self) {
        let rec = std::mem::take(&mut self.rec);
        let len = std::mem::take(&mut self.len);
        match self.rename.take() {
            Some((a, r, 2)) => {
                self.rename = Some((a, r, 1));
                return;
            }
            Some((a, r, _)) => return self.list(a, r, &rec, len),
            None => {}
        }
        let mut parts = rec.splitn(3, |b| *b == b'\t');
        let (Some(added), Some(removed)) = (parts.next(), parts.next()) else { return };
        let num = |b: &[u8]| std::str::from_utf8(b).ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
        let (a, r) = (num(added), num(removed));
        self.tally.count(a, r);
        match len.saturating_sub(added.len() + removed.len() + 2) {
            0 => self.rename = Some((a, r, 2)),
            plen => self.list(a, r, parts.next().unwrap_or_default(), plen),
        }
    }
}

impl Sink for Numstat {
    fn feed(&mut self, chunk: &[u8]) {
        for &b in chunk {
            if b == 0 {
                self.end();
            } else {
                if self.rec.len() < REC_CAP {
                    self.rec.push(b);
                }
                self.len += 1;
            }
        }
    }
}

/// The newest reflog entry comes first, so a later record for the same sha is an earlier event and wins. The entry after an
/// amend is HEAD before it: the commit the amend replaced.
#[derive(Default)]
struct Reflog {
    via: HashMap<String, Via>,
    replaces: HashMap<String, String>,
    amend: Option<String>,
    sha: Vec<u8>,
    action: Vec<u8>,
    in_action: bool,
    action_done: bool,
}

fn via_of(action: &str) -> Via {
    let action = action.trim();
    let first = action.split_whitespace().next().unwrap_or("");
    if first == "rebase" || action.contains("--rebase") || ["(pick)", "(reword)", "(squash)", "(fixup)", "(edit)"].iter().any(|m| action.contains(m)) {
        return Via::Rebase;
    }
    match action {
        "commit" | "commit (initial)" => Via::Commit,
        "commit (amend)" => Via::Amend,
        _ => match first {
            "cherry-pick" => Via::CherryPick,
            "revert" => Via::Revert,
            "pull" | "merge" | "reset" | "checkout" | "clone" | "branch" | "fetch" => Via::Arrived,
            _ => Via::Unknown,
        },
    }
}

impl Reflog {
    fn end(&mut self) {
        let sha = String::from_utf8_lossy(&std::mem::take(&mut self.sha)).into_owned();
        let action = std::mem::take(&mut self.action);
        self.in_action = false;
        self.action_done = false;
        let amend = self.amend.take();
        if !is_sha(&sha) || self.via.len() >= 4 * MAX_REFLOG {
            return;
        }
        if let Some(amend) = amend {
            self.replaces.insert(amend, sha.clone());
        }
        let via = via_of(&String::from_utf8_lossy(&action));
        if via == Via::Amend {
            self.amend = Some(sha.clone());
        }
        self.via.insert(sha, via);
    }
}

impl Sink for Reflog {
    fn feed(&mut self, chunk: &[u8]) {
        for &b in chunk {
            match b {
                0 => self.end(),
                0x1f if !self.in_action => self.in_action = true,
                _ if !self.in_action => {
                    if self.sha.len() < 65 {
                        self.sha.push(b);
                    }
                }
                b':' => self.action_done = true,
                _ if !self.action_done && self.action.len() < 96 => self.action.push(b),
                _ => {}
            }
        }
    }
}

fn scrub(cmd: &mut Command, vars: impl Iterator<Item = OsString>) {
    for k in vars {
        if k.to_string_lossy().to_ascii_uppercase().starts_with("GIT_") {
            cmd.env_remove(k);
        }
    }
}

fn is_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

struct Dirs {
    gitdir: PathBuf,
    common: PathBuf,
    submodule: bool,
}

struct Rec {
    sha: String,
    email: String,
    time: i64,
    authored: i64,
}

// kairosd keeps the PATH it started with, so a Git for Windows installed since then is looked up where its installer puts it.
#[cfg(windows)]
fn add_installed_git_to_path() -> bool {
    for (var, sub) in [("ProgramFiles", "Git\\cmd"), ("ProgramW6432", "Git\\cmd"), ("LOCALAPPDATA", "Programs\\Git\\cmd")] {
        let Some(base) = std::env::var_os(var) else { continue };
        let dir = PathBuf::from(base).join(sub);
        if dir.join("git.exe").is_file() {
            let mut path = std::env::var_os("PATH").unwrap_or_default();
            path.push(";");
            path.push(&dir);
            std::env::set_var("PATH", path);
            return true;
        }
    }
    false
}

#[cfg(not(windows))]
fn add_installed_git_to_path() -> bool {
    false
}

#[derive(Clone)]
struct Git {
    program: OsString,
}

impl Default for Git {
    fn default() -> Git {
        Git { program: "git".into() }
    }
}

impl Git {
    fn command(&self, root: Option<&Path>) -> Command {
        let mut cmd = Command::new(&self.program);
        scrub(&mut cmd, std::env::vars_os().map(|(k, _)| k));
        cmd.env("GIT_TERMINAL_PROMPT", "0").env("GIT_OPTIONAL_LOCKS", "0");
        // fsmonitor is a repo-config command git would run on our behalf.
        cmd.args(["--no-pager", "--no-optional-locks", "-c", "core.fsmonitor=false"]);
        if let Some(root) = root {
            cmd.current_dir(root);
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
        cmd
    }

    fn run<S: Sink>(&self, root: Option<&Path>, args: &[&str], sink: S) -> Result<S, Fail> {
        self.run_with(root, args, None, sink)
    }

    fn run_with<S: Sink>(&self, root: Option<&Path>, args: &[&str], input: Option<Vec<u8>>, sink: S) -> Result<S, Fail> {
        if root.is_some_and(|r| !r.is_dir()) {
            return Err(Fail::Gone);
        }
        let mut cmd = self.command(root);
        if input.is_some() {
            cmd.stdin(Stdio::piped());
        }
        let mut child = cmd
            .args(args)
            .spawn()
            .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { Fail::NoGit } else { Fail::Exit })?;
        if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
            thread::spawn(move || {
                let _ = stdin.write_all(&data);
            });
        }
        let Some(mut stdout) = child.stdout.take() else {
            let _ = child.kill();
            return Err(Fail::Exit);
        };
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut sink = sink;
            let mut chunk = [0u8; 16384];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.feed(&chunk[..n]),
                }
            }
            let _ = tx.send(sink);
        });
        let deadline = Instant::now() + GIT_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Fail::Timeout);
                }
            }
        };
        // A grandchild holding the pipe open must not hang us, so the reader gets a short grace.
        let sink = rx.recv_timeout(Duration::from_secs(2)).map_err(|_| Fail::Timeout)?;
        if status.success() {
            Ok(sink)
        } else {
            Err(Fail::Exit)
        }
    }

    fn available(&self) -> bool {
        let runs = || self.run(None, &["--version"], Buf::default()).is_ok();
        runs() || (add_installed_git_to_path() && runs())
    }

    fn text(&self, root: &Path, args: &[&str]) -> Result<String, Fail> {
        let buf = self.run(Some(root), args, Buf::default())?;
        if buf.over {
            return Err(Fail::TooBig);
        }
        Ok(String::from_utf8_lossy(&buf.data).into_owned())
    }

    fn dirs(&self, root: &Path) -> Result<Dirs, Fail> {
        let text =
            self.text(root, &["rev-parse", "--absolute-git-dir", "--git-common-dir", "--show-superproject-working-tree"])?;
        let mut lines = text.lines();
        let gitdir = PathBuf::from(lines.next().ok_or(Fail::Exit)?);
        let common = PathBuf::from(lines.next().ok_or(Fail::Exit)?);
        let common = if common.is_absolute() { common } else { root.join(common) };
        Ok(Dirs { gitdir, common, submodule: lines.next().is_some_and(|l| !l.is_empty()) })
    }

    fn head(&self, root: &Path) -> Result<Option<String>, Fail> {
        match self.text(root, &["rev-parse", "--verify", "-q", "HEAD^{commit}"]) {
            Ok(t) => Ok(Some(t.trim().to_string()).filter(|s| is_sha(s))),
            Err(Fail::Exit) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn email(&self, root: &Path) -> Option<String> {
        let t = self.text(root, &["config", "--get", "user.email"]).ok()?;
        Some(t.trim().to_lowercase()).filter(|e| !e.is_empty())
    }

    fn commits(&self, root: &Path, from: Option<&str>, to: &str) -> Result<Vec<Rec>, Fail> {
        let range = match from {
            Some(f) => format!("{f}..{to}"),
            None => to.to_string(),
        };
        let max = format!("--max-count={MAX_COMMITS}");
        let text = self.text(
            root,
            &["log", "--no-merges", "--no-show-signature", "-z", "--format=%H%x1f%ae%x1f%ct%x1f%at", &max, &range, "--"],
        )?;
        Ok(text
            .split('\0')
            .filter_map(|r| {
                let mut f = r.trim().split('\u{1f}');
                let (sha, email, time, authored) = (f.next()?, f.next()?, f.next()?, f.next()?);
                Some(Rec {
                    sha: sha.to_string(),
                    email: email.to_lowercase(),
                    time: time.parse().ok()?,
                    authored: authored.parse().ok()?,
                })
            })
            .collect())
    }

    fn numstat(&self, root: &Path, args: &[&str]) -> Result<Tally, Fail> {
        Ok(self.run(Some(root), args, Numstat::default())?.tally)
    }

    fn commit_size(&self, root: &Path, sha: &str) -> Result<Stat, Fail> {
        Ok(self
            .numstat(
                root,
                &[
                    "diff-tree", "--no-commit-id", "--root", "-r", "--numstat", "-z", "-M", "--ignore-submodules=all",
                    "--no-ext-diff", sha,
                ],
            )?
            .finish())
    }

    fn uncommitted(&self, root: &Path) -> Result<Stat, Fail> {
        let mut tally = self.numstat(
            root,
            &[
                "diff", "HEAD", "--numstat", "-z", "-M", "--ignore-submodules=all", "--no-ext-diff", "--no-textconv",
                "--no-color", "--",
            ],
        )?;
        self.untracked(root, &mut tally, UNTRACKED_BUDGET);
        Ok(tally.finish())
    }

    fn untracked(&self, root: &Path, tally: &mut Tally, mut budget: u64) {
        let Ok(list) = self.run(Some(root), &["ls-files", "-z", "--others", "--exclude-standard"], Buf::default()) else {
            return;
        };
        if list.over {
            return;
        }
        for raw in list.data.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            untracked_file(root, raw, &mut budget, tally);
        }
    }

    fn reflog(&self, root: &Path) -> Option<Reflog> {
        let n = format!("--max-count={MAX_REFLOG}");
        let sink = self
            .run(Some(root), &["log", "-g", "--no-show-signature", "-z", "--format=%H%x1f%gs", &n, "HEAD", "--"], Reflog::default())
            .ok()?;
        Some(sink).filter(|r| !r.via.is_empty())
    }
}

fn reflog_len(gitdir: &Path) -> Option<u64> {
    fs::metadata(gitdir.join("logs").join("HEAD")).ok().map(|m| m.len())
}

/// Shas HEAD's reflog records as made here (committed, amended, cherry-picked or reverted, not checked out or pulled) past
/// its first `from` bytes. The reflog only grows until git rewrites it, which the caller rules out by its length.
fn made_after(gitdir: &Path, from: u64) -> HashSet<String> {
    let mut made = HashSet::new();
    let mut buf = Vec::new();
    let read = File::open(gitdir.join("logs").join("HEAD"))
        .and_then(|mut f| f.seek(SeekFrom::Start(from)).map(|_| f))
        .and_then(|f| f.take(MAX_OUT as u64).read_to_end(&mut buf));
    if read.is_err() {
        return made;
    }
    for line in buf.split(|&b| b == b'\n') {
        let Some(tab) = line.iter().position(|&b| b == b'\t') else { continue };
        let action = line[tab + 1..].split(|&b| b == b':').next().unwrap_or_default();
        if !matches!(via_of(&String::from_utf8_lossy(action)), Via::Commit | Via::Amend | Via::CherryPick | Via::Revert) {
            continue;
        }
        if let Some(sha) = line[..tab].split(|&b| b == b' ').nth(1).map(|s| String::from_utf8_lossy(s).into_owned()) {
            if is_sha(&sha) {
                made.insert(sha);
            }
        }
    }
    made
}

fn untracked_file(root: &Path, raw: &[u8], budget: &mut u64, tally: &mut Tally) {
    let Some(rel) = relative(raw) else { return };
    let full = root.join(&rel);
    let Ok(meta) = fs::symlink_metadata(&full) else { return };
    if !meta.is_file() {
        return;
    }
    let path = String::from_utf8_lossy(raw).into_owned();
    let listed = (path.len() <= MAX_PATH_LEN).then_some(path);
    if meta.len() > UNTRACKED_FILE_MAX || meta.len() > *budget {
        tally.count(0, 0);
        return;
    }
    let limit = UNTRACKED_FILE_MAX.min(*budget);
    let mut data = Vec::new();
    let Ok(n) = File::open(&full).and_then(|f| f.take(limit + 1).read_to_end(&mut data)) else { return };
    *budget = budget.saturating_sub(n as u64);
    if n as u64 > limit {
        return;
    }
    let lines = if data[..data.len().min(BINARY_PROBE)].contains(&0) {
        0
    } else {
        let newlines = data.iter().filter(|b| **b == b'\n').count();
        (newlines + usize::from(data.last().is_some_and(|b| *b != b'\n'))) as u32
    };
    tally.count(lines, 0);
    if let Some(path) = listed {
        tally.list(path, lines, 0);
    }
}

fn relative(raw: &[u8]) -> Option<PathBuf> {
    #[cfg(unix)]
    let rel = {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(raw))
    };
    #[cfg(not(unix))]
    let rel = PathBuf::from(String::from_utf8_lossy(raw).into_owned());
    let inside = rel.components().all(|c| matches!(c, Component::Normal(_)));
    (inside && !rel.as_os_str().is_empty()).then_some(rel)
}

#[derive(Clone, Serialize, Deserialize)]
struct Mark {
    head: Option<String>,
    since: f64,
    touched: f64,
    /// When HEAD was last read and how long its reflog was then: a commit the reflog records past that length was made after.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checked: Option<(f64, u64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    repos: HashMap<String, Mark>,
    #[serde(default)]
    seen: Vec<String>,
}

struct Repo {
    id: String,
    gitdir: PathBuf,
    common: PathBuf,
    check_at: Option<Instant>,
    diff_pending: bool,
    next_diff: Instant,
    last_diff: Option<Stat>,
}

#[derive(PartialEq, Clone, Copy)]
enum Avail {
    Unknown,
    Yes,
    No,
}

struct Engine {
    git: Git,
    state_path: PathBuf,
    excluded: Arc<Excluded>,
    saved: Saved,
    repos: HashMap<PathBuf, Repo>,
    rejected: HashMap<PathBuf, Instant>,
    avail: Avail,
    git_retry: Option<Instant>,
    next_sweep: Instant,
    unwatch: Vec<PathBuf>,
    warned_cap: bool,
    dirty: bool,
}

fn key(root: &Path) -> String {
    root.to_string_lossy().into_owned()
}

impl Engine {
    fn new(git: Git, state_path: PathBuf, excluded: Arc<Excluded>) -> Engine {
        let saved = fs::read_to_string(&state_path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Engine {
            git,
            state_path,
            excluded,
            saved,
            repos: HashMap::new(),
            rejected: HashMap::new(),
            avail: Avail::Unknown,
            git_retry: None,
            next_sweep: Instant::now() + SWEEP,
            unwatch: Vec::new(),
            warned_cap: false,
            dirty: false,
        }
    }

    fn save(&self) {
        let Ok(text) = serde_json::to_string(&self.saved) else { return };
        let tmp = self.state_path.with_extension("json.tmp");
        if fs::write(&tmp, text).is_ok() {
            let _ = fs::rename(&tmp, &self.state_path);
        }
    }

    fn reject(&mut self, root: PathBuf, now: Instant) {
        if self.rejected.len() > 512 {
            self.rejected.clear();
        }
        self.rejected.insert(root, now + RETRY);
    }

    fn register(&mut self, root: PathBuf, id: String, now: Instant, now_secs: f64) -> Vec<(PathBuf, RecursiveMode)> {
        if self.excluded.covers(&root) {
            return Vec::new();
        }
        if let Some(repo) = self.repos.get_mut(&root) {
            repo.id = id.clone();
            self.remember(&root, id);
            return Vec::new();
        }
        // Git may get installed while kairosd runs. Not on macOS, where each try can bring the install dialog back.
        if self.avail == Avail::No && !cfg!(target_os = "macos") && self.git_retry.is_some_and(|t| now >= t) {
            self.avail = Avail::Unknown;
        }
        if self.avail == Avail::No || self.rejected.get(&root).is_some_and(|t| now < *t) {
            return Vec::new();
        }
        if !root.join(".git").exists() {
            self.reject(root, now);
            return Vec::new();
        }
        if self.repos.len() >= MAX_REPOS {
            if !self.warned_cap {
                diag!("kairosd: warning: git watch limit reached, some repos are not followed");
                self.warned_cap = true;
            }
            return Vec::new();
        }
        // Probed on the first repo only: a machine without repos never runs git (on macOS the
        // /usr/bin/git stub can pop up an install dialog).
        if self.avail == Avail::Unknown {
            if self.git.available() {
                self.avail = Avail::Yes;
            } else {
                if self.git_retry.is_none() {
                    diag!("kairosd: git not found, commit and diff evidence is off");
                }
                self.git_retry = Some(now + RETRY);
                self.avail = Avail::No;
                return Vec::new();
            }
        }
        let dirs = match self.git.dirs(&root) {
            Ok(d) if !d.submodule => d,
            _ => {
                self.reject(root, now);
                return Vec::new();
            }
        };
        let fresh = !self.saved.repos.contains_key(&key(&root));
        if fresh {
            let checked = reflog_len(&dirs.gitdir).map(|n| (now_secs, n));
            let head = self.git.head(&root).ok().flatten();
            self.saved.repos.insert(key(&root), Mark { head, since: now_secs, touched: now_secs, checked, id: Some(id.clone()) });
            while self.saved.repos.len() > MAX_SAVED {
                let oldest = self.saved.repos.iter().min_by(|a, b| a.1.touched.total_cmp(&b.1.touched)).map(|(k, _)| k.clone());
                match oldest {
                    Some(k) => self.saved.repos.remove(&k),
                    None => break,
                };
            }
            self.save();
        } else {
            self.remember(&root, id.clone());
        }
        let watch = vec![
            (dirs.gitdir.clone(), RecursiveMode::NonRecursive),
            (dirs.gitdir.join("logs"), RecursiveMode::NonRecursive),
            (dirs.common.clone(), RecursiveMode::NonRecursive),
            (dirs.common.join("refs").join("heads"), RecursiveMode::Recursive),
        ];
        self.repos.insert(
            root,
            Repo {
                id,
                gitdir: dirs.gitdir,
                common: dirs.common,
                check_at: (!fresh).then_some(now),
                diff_pending: false,
                next_diff: now,
                last_diff: None,
            },
        );
        watch
    }

    fn remember(&mut self, root: &Path, id: String) {
        if let Some(m) = self.saved.repos.get_mut(&key(root)).filter(|m| m.id.as_deref() != Some(id.as_str())) {
            m.id = Some(id);
            self.dirty = true;
        }
    }

    /// Each update restarts kairosd. Repos followed in the last two days are watched again right away, not once their project
    /// shows activity, so a commit is seen when it lands.
    fn restore(&mut self, now: Instant, now_secs: f64) -> Vec<(PathBuf, RecursiveMode)> {
        let mut recent: Vec<(f64, String, String)> = self
            .saved
            .repos
            .iter()
            .filter_map(|(root, m)| Some((m.checked.map_or(m.touched, |c| c.0), root.clone(), m.id.clone()?)))
            .filter(|(last, ..)| now_secs - last <= MAX_AGE)
            .collect();
        recent.sort_by(|a, b| b.0.total_cmp(&a.0));
        recent.into_iter().take(RESTORE).flat_map(|(_, root, id)| self.register(PathBuf::from(root), id, now, now_secs)).collect()
    }

    fn activity(&mut self, root: &Path) {
        if let Some(repo) = self.repos.get_mut(root) {
            repo.diff_pending = true;
        }
    }

    fn touched(&mut self, paths: &[PathBuf], now: Instant) {
        for p in paths {
            let name = p.file_name().and_then(OsStr::to_str).unwrap_or("");
            for repo in self.repos.values_mut() {
                if !(p.starts_with(&repo.gitdir) || p.starts_with(&repo.common)) {
                    continue;
                }
                let branch = p.starts_with(repo.common.join("refs").join("heads"));
                if (branch || name == "HEAD" || name == "packed-refs") && !name.ends_with(".lock") {
                    repo.check_at = Some(now + DEBOUNCE);
                }
            }
        }
    }

    fn drop_repo(&mut self, root: &Path) {
        let Some(repo) = self.repos.remove(root) else { return };
        self.unwatch.extend([repo.gitdir.clone(), repo.gitdir.join("logs")]);
        if !self.repos.values().any(|r| r.common == repo.common) {
            self.unwatch.extend([repo.common.join("refs").join("heads"), repo.common]);
        }
    }

    fn alive(&mut self, root: &Path) -> bool {
        let alive = root.join(".git").exists();
        if !alive {
            self.drop_repo(root);
        }
        alive
    }

    /// Forgets the watermark too, so taking the folder back later starts from its HEAD then and never imports what happened in between.
    fn drop_excluded(&mut self) {
        let ex = self.excluded.current();
        let covered: Vec<PathBuf> = self.repos.keys().filter(|r| ex.covers(r)).cloned().collect();
        for root in covered {
            self.drop_repo(&root);
        }
        let marks = self.saved.repos.len();
        self.saved.repos.retain(|root, _| !ex.covers(Path::new(root)));
        if self.saved.repos.len() != marks {
            self.save();
        }
    }

    fn tick(&mut self, now: Instant, now_secs: f64) -> Vec<Event> {
        let mut out = Vec::new();
        self.drop_excluded();
        if now >= self.next_sweep {
            self.next_sweep = now + SWEEP;
            for repo in self.repos.values_mut() {
                repo.check_at.get_or_insert(now);
            }
        }
        let roots: Vec<PathBuf> = self.repos.keys().cloned().collect();
        for root in roots {
            if self.repos.get(&root).and_then(|r| r.check_at).is_some_and(|t| now >= t) {
                out.extend(self.check(&root, now_secs));
            }
            if self.repos.get(&root).is_some_and(|r| r.diff_pending && now >= r.next_diff) {
                out.extend(self.sample(&root, now, now_secs));
            }
        }
        if std::mem::take(&mut self.dirty) {
            self.save();
        }
        out
    }

    fn check(&mut self, root: &Path, now_secs: f64) -> Vec<Event> {
        let mut out = Vec::new();
        if !self.alive(root) {
            return out;
        }
        let Some(repo) = self.repos.get_mut(root) else { return out };
        repo.check_at = None;
        let id = repo.id.clone();
        let gitdir = repo.gitdir.clone();
        // Read before HEAD: a commit landing in between is then past this length and is placed after this look.
        let checked = reflog_len(&gitdir).map(|n| (now_secs, n));
        let head = match self.git.head(root) {
            Ok(Some(h)) => h,
            Err(Fail::Gone) => {
                self.drop_repo(root);
                return out;
            }
            _ => return out,
        };
        let Some(mark) = self.saved.repos.get(&key(root)).cloned() else { return out };
        if mark.head.as_deref() == Some(head.as_str()) {
            if let Some(m) = self.saved.repos.get_mut(&key(root)) {
                m.checked = checked;
                self.dirty = true;
            }
            return out;
        }
        let made = match (mark.checked, checked) {
            (Some((_, from)), Some((_, len))) if len >= from => made_after(&gitdir, from),
            _ => HashSet::new(),
        };
        let from = mark.head.as_deref().filter(|h| is_sha(h));
        // The old watermark can vanish (reset, gc, a rewritten history): fall back to HEAD's recent commits.
        let listed = match self.git.commits(root, from, &head) {
            Err(Fail::Exit) if from.is_some() => self.git.commits(root, None, &head),
            other => other,
        };
        let Ok(mut listed) = listed else { return out };
        let mine = self.git.email(root);
        let since = mark.since.floor() as i64;
        let mut reflog = None;
        listed.reverse();
        for rec in listed {
            let wanted = mine.as_deref() == Some(rec.email.as_str())
                && rec.time >= since
                && now_secs - rec.time as f64 <= MAX_AGE
                && is_sha(&rec.sha)
                && !self.saved.seen.contains(&rec.sha);
            if !wanted {
                continue;
            }
            self.saved.seen.push(rec.sha.clone());
            if self.saved.seen.len() > MAX_SEEN {
                self.saved.seen.remove(0);
            }
            let reflog = reflog.get_or_insert_with(|| self.git.reflog(root));
            let via = reflog.as_ref().map(|r| r.via.get(&rec.sha).copied().unwrap_or(Via::Arrived));
            let replaces = reflog.as_ref().filter(|_| via == Some(Via::Amend)).and_then(|r| r.replaces.get(&rec.sha).cloned());
            let not_before = mark.checked.filter(|_| made.contains(&rec.sha)).map(|(t, _)| t);
            match self.git.commit_size(root, &rec.sha) {
                Ok(stat) => {
                    let kind = Kind::Commit {
                        sha: rec.sha,
                        files: stat.files,
                        added: stat.added,
                        removed: stat.removed,
                        authored: Some(rec.authored),
                        committed: Some(rec.time),
                        via,
                        changes: stat.changes,
                        replaces,
                        not_before,
                    };
                    out.push(Event::new(now_secs, Some(id.clone()), kind));
                }
                Err(Fail::Gone) => break,
                Err(_) => diag!("kairosd: warning: could not size a commit, skipping it"),
            }
        }
        if let Some(m) = self.saved.repos.get_mut(&key(root)) {
            m.head = Some(head);
            m.touched = now_secs;
            m.checked = checked;
        }
        self.save();
        if let Some(repo) = self.repos.get_mut(root) {
            repo.diff_pending = true;
        }
        out
    }

    fn sample(&mut self, root: &Path, now: Instant, now_secs: f64) -> Option<Event> {
        if !self.alive(root) {
            return None;
        }
        let repo = self.repos.get_mut(root)?;
        repo.diff_pending = false;
        repo.next_diff = now + DIFF_EVERY;
        let id = repo.id.clone();
        match self.git.uncommitted(root) {
            Ok(stat) => {
                let repo = self.repos.get_mut(root)?;
                let last = repo.last_diff.replace(stat.clone()).unwrap_or_default();
                let changed = stat != last;
                let kind = Kind::Diff { files: stat.files, added: stat.added, removed: stat.removed, changes: stat.changes };
                changed.then(|| Event::new(now_secs, Some(id), kind))
            }
            Err(Fail::Gone) => {
                self.drop_repo(root);
                None
            }
            Err(_) => None,
        }
    }
}

#[derive(Default)]
struct Entries {
    part: Vec<u8>,
    all: Vec<Vec<u8>>,
}

impl Sink for Entries {
    fn feed(&mut self, chunk: &[u8]) {
        for &b in chunk {
            if b == 0 {
                self.all.push(std::mem::take(&mut self.part));
            } else {
                self.part.push(b);
            }
        }
    }
}

/// Which of `files` (all under `root`) git ignores, by its own rules: every .gitignore, info/exclude and the global
/// excludes file. Tracked files never are. All of them are asked about in one run, so a build writing thousands of files
/// costs one git process. Nothing is ignored outside a repository or when git can't tell.
pub fn ignored(root: &Path, files: &HashSet<PathBuf>) -> HashSet<PathBuf> {
    let mut asked: HashMap<Vec<u8>, &PathBuf> = HashMap::new();
    let mut input = Vec::new();
    for f in files {
        let Ok(rel) = f.strip_prefix(root) else { continue };
        let rel: Vec<_> = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect();
        let rel = rel.join("/").into_bytes();
        input.extend_from_slice(&rel);
        input.push(0);
        asked.insert(rel, f);
    }
    if asked.is_empty() {
        return HashSet::new();
    }
    let root = plain(root.to_path_buf());
    match Git::default().run_with(Some(&root), &["check-ignore", "--stdin", "-z"], Some(input), Entries::default()) {
        Ok(out) => out.all.iter().filter_map(|rel| asked.get(rel).map(|f| (*f).clone())).collect(),
        Err(_) => HashSet::new(),
    }
}

/// `\\?\C:\x` becomes `C:\x`: not every tool git spawns copes with verbatim paths.
fn plain(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy().into_owned();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            if rest.as_bytes().get(1) == Some(&b':') {
                return PathBuf::from(rest);
            }
        }
    }
    p
}

pub fn spawn(rx: Receiver<Cmd>, tx_self: Sender<Cmd>, out: Sender<Msg>, dir: PathBuf, excluded: Arc<Excluded>) {
    let _ = thread::Builder::new().name("git".into()).spawn(move || {
        let mut engine = Engine::new(Git::default(), dir.join(STATE_FILE), excluded);
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                if !matches!(ev.kind, EventKind::Access(_)) {
                    let _ = tx_self.send(Cmd::Fs(ev.paths));
                }
            }
        });
        let mut watcher = watcher.ok();
        if watcher.is_none() {
            diag!("kairosd: warning: git watcher unavailable, commits are noticed by a 10 minute sweep");
        }
        let watch = |paths: Vec<(PathBuf, RecursiveMode)>, watcher: &mut Option<notify::RecommendedWatcher>| {
            for (path, mode) in paths {
                if let Some(w) = watcher.as_mut() {
                    let _ = w.watch(&path, mode);
                }
            }
        };
        watch(engine.restore(Instant::now(), now()), &mut watcher);
        loop {
            match rx.recv_timeout(TICK) {
                Ok(Cmd::Root(root, id)) => watch(engine.register(plain(root), id, Instant::now(), now()), &mut watcher),
                Ok(Cmd::Activity(root)) => engine.activity(&plain(root)),
                Ok(Cmd::Fs(paths)) => engine.touched(&paths, Instant::now()),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            for e in engine.tick(Instant::now(), now()) {
                let _ = out.send(Msg::Event(e));
            }
            for path in std::mem::take(&mut engine.unwatch) {
                if let Some(w) = watcher.as_mut() {
                    let _ = w.unwatch(&path);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn fixture(name: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("kairos-git-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Fixture { dir: plain(dir.canonicalize().unwrap()) }
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        git_env(dir, args, &[])
    }

    fn git_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
        let mut cmd = Command::new("git");
        scrub(&mut cmd, std::env::vars_os().map(|(k, _)| k));
        let out = cmd
            .current_dir(dir)
            .args(args)
            .envs(env.iter().copied())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn configure(root: &Path) {
        git(root, &["config", "user.email", "Me@Example.com"]);
        git(root, &["config", "user.name", "Me"]);
        git(root, &["config", "commit.gpgsign", "false"]);
    }

    fn repo(dir: &Path, name: &str) -> PathBuf {
        let root = dir.join(name);
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        configure(&root);
        root
    }

    fn commit(root: &Path, file: &str, text: &str) -> String {
        fs::write(root.join(file), text).unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", "work"]);
        git(root, &["rev-parse", "HEAD"])
    }

    fn engine(dir: &Path) -> Engine {
        Engine::new(Git::default(), dir.join("state.json"), Arc::new(Excluded::new(dir.to_path_buf())))
    }

    fn add(e: &mut Engine, root: &Path) -> Instant {
        let t = Instant::now();
        e.register(root.to_path_buf(), "o/r".into(), t, now());
        t
    }

    fn commits(events: &[Event]) -> Vec<(String, u32, u32, u32)> {
        events
            .iter()
            .filter_map(|e| match &e.kind {
                Kind::Commit { sha, files, added, removed, .. } => Some((sha.clone(), *files, *added, *removed)),
                _ => None,
            })
            .collect()
    }

    fn diffs(events: &[Event]) -> Vec<(u32, u32, u32)> {
        events
            .iter()
            .filter_map(|e| match &e.kind {
                Kind::Diff { files, added, removed, .. } => Some((*files, *added, *removed)),
                _ => None,
            })
            .collect()
    }

    fn check(e: &mut Engine, root: &Path) -> Vec<Event> {
        e.check(root, now())
    }

    fn totals(s: &Stat) -> (u32, u32, u32) {
        (s.files, s.added, s.removed)
    }

    fn parse(data: &[u8], step: usize) -> Stat {
        let mut n = Numstat::default();
        for c in data.chunks(step) {
            n.feed(c);
            assert!(n.tally.top.len() < 2 * MAX_CHANGES);
        }
        n.tally.finish()
    }

    fn change(path: &str, added: u32, removed: u32) -> FileChange {
        FileChange { path: path.into(), added, removed }
    }

    #[test]
    fn numstat_stream_lists_files_in_path_order_and_reports_the_new_name_of_a_rename() {
        let data = b"3\t1\tsrc/a.rs\0-\t-\tlogo.png\x000\t0\t\0old name\0new\tname\x002\t5\ttab\tin\tname\x004\t0\t\0from\0copy\0";
        for step in [1, 2, 3, 7, data.len()] {
            let s = parse(data, step);
            assert_eq!(totals(&s), (5, 9, 6), "chunks of {step}");
            assert_eq!(
                s.changes,
                [change("copy", 4, 0), change("logo.png", 0, 0), change("new\tname", 0, 0), change("src/a.rs", 3, 1), change("tab\tin\tname", 2, 5)],
                "chunks of {step}"
            );
        }
    }

    #[test]
    fn numstat_paths_are_lossy_utf8_and_overlong_ones_are_counted_but_not_listed() {
        let exact = "e".repeat(MAX_PATH_LEN);
        let mut data = format!("1\t1\t{exact}\0").into_bytes();
        data.extend(format!("2\t0\t{}\0", "o".repeat(MAX_PATH_LEN + 1)).into_bytes());
        data.extend(format!("3\t0\t{}\0", "h".repeat(5000)).into_bytes());
        data.extend(format!("4\t0\t\0old\0{}\0", "n".repeat(MAX_PATH_LEN + 1)).into_bytes());
        data.extend(b"5\t0\tbad\xff\xfename\0");
        data.extend(b"6\t0\tafter\0");
        for step in [1, 5, 4096, data.len()] {
            let s = parse(&data, step);
            assert_eq!(totals(&s), (6, 21, 1), "chunks of {step}");
            assert_eq!(s.changes, [change("after", 6, 0), change("bad\u{fffd}\u{fffd}name", 5, 0), change(&exact, 1, 1)]);
        }
        let wide = "é".repeat(MAX_PATH_LEN / 2 + 1);
        let s = parse(format!("1\t0\t{wide}\0").as_bytes(), 3);
        assert_eq!((s.files, s.changes.len()), (1, 0), "the byte length counts, not the characters");
    }

    #[test]
    fn numstat_keeps_the_largest_changes_in_bounded_memory_whatever_the_order() {
        let mut forward = String::new();
        for i in 0..1000 {
            forward.push_str(&format!("{i}\t0\tf{i:04}\0"));
        }
        let mut backward = String::new();
        for i in (0..1000).rev() {
            backward.push_str(&format!("{i}\t0\tf{i:04}\0"));
        }
        let want: Vec<FileChange> = (700..1000).map(|i| change(&format!("f{i:04}"), i, 0)).collect();
        for (data, step) in [(&forward, 1), (&forward, 4096), (&backward, 13)] {
            let s = parse(data.as_bytes(), step);
            assert_eq!(totals(&s), (1000, 499500, 0));
            assert_eq!(s.changes, want);
        }
        let mut ties = String::new();
        for i in (0..800).rev() {
            ties.push_str(&format!("1\t1\tt{i:04}\0"));
        }
        let s = parse(ties.as_bytes(), 9);
        assert_eq!(s.files, 800);
        let want: Vec<FileChange> = (0..MAX_CHANGES).map(|i| change(&format!("t{i:04}"), 1, 1)).collect();
        assert_eq!(s.changes, want, "ties go to the smaller path");
    }

    #[test]
    fn reflog_actions_map_to_how_a_commit_arrived() {
        let cases = [
            ("commit", Via::Commit),
            ("commit (initial)", Via::Commit),
            ("commit (amend)", Via::Amend),
            ("cherry-pick", Via::CherryPick),
            ("revert", Via::Revert),
            ("rebase (pick)", Via::Rebase),
            ("rebase (continue)", Via::Rebase),
            ("pull --rebase (pick)", Via::Rebase),
            ("pull --rebase origin main (finish)", Via::Rebase),
            ("pull", Via::Arrived),
            ("pull -q", Via::Arrived),
            ("merge origin/main", Via::Arrived),
            ("reset", Via::Arrived),
            ("checkout", Via::Arrived),
            ("clone", Via::Arrived),
            ("branch", Via::Arrived),
            ("fetch", Via::Arrived),
            ("am", Via::Unknown),
            ("", Via::Unknown),
        ];
        for (action, via) in cases {
            assert_eq!(via_of(action), via, "{action:?}");
        }
    }

    #[test]
    fn the_reflog_sink_keeps_actions_only_and_the_earliest_entry_of_a_sha_wins() {
        let (a, b) = ("a".repeat(40), "b".repeat(40));
        let data = format!("{a}\x1fcheckout: moving from x to y\0{b}\x1fcommit: secret: message\0{a}\x1fcommit (amend): {}\0{a}\x1fcommit: first\0", "m".repeat(3000));
        for step in [1, 7, data.len()] {
            let mut r = Reflog::default();
            for c in data.as_bytes().chunks(step) {
                r.feed(c);
            }
            assert_eq!(r.via.get(&a), Some(&Via::Commit), "chunks of {step}");
            assert_eq!(r.via.get(&b), Some(&Via::Commit));
            assert!(r.action.is_empty() && r.sha.is_empty());
        }
    }

    #[test]
    fn the_entry_after_an_amend_is_the_commit_it_replaced() {
        let (a, b, c) = ("a".repeat(40), "b".repeat(40), "c".repeat(40));
        let data = format!("{c}\x1fcommit (amend): fix\0{b}\x1fcommit (amend): typo\0{a}\x1fcommit: first\0");
        let mut r = Reflog::default();
        r.feed(data.as_bytes());
        assert_eq!((r.replaces.get(&c), r.replaces.get(&b), r.replaces.get(&a)), (Some(&b), Some(&a), None));
    }

    #[test]
    fn scrub_removes_git_variables_only() {
        let mut cmd = Command::new("git");
        let vars = ["GIT_DIR", "git_work_tree", "PATH", "GITHUB_TOKEN"].map(OsString::from);
        scrub(&mut cmd, vars.into_iter());
        let removed: Vec<String> =
            cmd.get_envs().filter(|(_, v)| v.is_none()).map(|(k, _)| k.to_string_lossy().into_owned()).collect();
        assert_eq!(removed, ["GIT_DIR", "git_work_tree"]);
    }

    #[test]
    fn first_sight_sets_a_watermark_and_a_new_commit_emits_once() {
        let f = fixture("basic");
        let root = repo(&f.dir, "r");
        commit(&root, "a.txt", "one\ntwo\n");
        let mut e = engine(&f.dir);
        add(&mut e, &root);
        assert!(check(&mut e, &root).is_empty());
        fs::write(root.join("b.txt"), "x\ny\nz\n").unwrap();
        let sha = commit(&root, "a.txt", "one\nthree\nfour\n");
        let events = check(&mut e, &root);
        assert_eq!(commits(&events), [(sha.clone(), 2, 5, 1)]);
        assert_eq!(events[0].project.as_deref(), Some("o/r"));
        assert!(check(&mut e, &root).is_empty());
        assert!(e.saved.seen.contains(&sha));
    }

    #[test]
    fn other_authors_are_ignored_and_amends_are_new_commits() {
        let f = fixture("authors");
        let root = repo(&f.dir, "r");
        commit(&root, "a.txt", "1\n");
        let mut e = engine(&f.dir);
        add(&mut e, &root);
        fs::write(root.join("b.txt"), "x\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["-c", "user.email=other@example.com", "commit", "-q", "-m", "theirs"]);
        assert!(check(&mut e, &root).is_empty());
        let first = commit(&root, "c.txt", "1\n2\n");
        assert_eq!(commits(&check(&mut e, &root)).len(), 1);
        fs::write(root.join("c.txt"), "1\n2\n3\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "--amend", "--no-edit"]);
        let amended = git(&root, &["rev-parse", "HEAD"]);
        assert_ne!(first, amended);
        assert_eq!(commits(&check(&mut e, &root)), [(amended, 1, 3, 0)]);
    }

    #[test]
    fn restart_does_not_re_emit_but_catches_up() {
        let f = fixture("restart");
        let root = repo(&f.dir, "r");
        commit(&root, "a.txt", "1\n");
        let mut e = engine(&f.dir);
        add(&mut e, &root);
        commit(&root, "a.txt", "1\n2\n");
        assert_eq!(commits(&check(&mut e, &root)).len(), 1);
        drop(e);
        let mut e = engine(&f.dir);
        let t = add(&mut e, &root);
        assert!(commits(&e.tick(t, now())).is_empty());
        let sha = commit(&root, "a.txt", "1\n2\n3\n");
        e.touched(&[root.join(".git/refs/heads/main")], t);
        assert_eq!(commits(&e.tick(t + DEBOUNCE, now())), [(sha, 1, 1, 0)]);
        drop(e);
        let mut e = engine(&f.dir);
        let t = add(&mut e, &root);
        assert!(commits(&e.tick(t, now())).is_empty());
    }

    #[test]
    fn commits_made_while_the_daemon_was_off_are_caught_up_after_a_restart() {
        let f = fixture("catchup");
        let root = repo(&f.dir, "r");
        commit(&root, "a.txt", "1\n");
        let mut e = engine(&f.dir);
        add(&mut e, &root);
        drop(e);
        let sha = commit(&root, "a.txt", "1\n2\n");
        let mut e = engine(&f.dir);
        let t = add(&mut e, &root);
        assert_eq!(commits(&e.tick(t, now())), [(sha, 1, 1, 0)]);
    }

    fn spans(events: &[Event]) -> Vec<(String, Option<f64>)> {
        events
            .iter()
            .filter_map(|e| match &e.kind {
                Kind::Commit { sha, not_before, .. } => Some((sha.clone(), *not_before)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_commit_made_here_carries_when_head_was_last_read_before_it() {
        let f = fixture("span");
        let root = repo(&f.dir, "r");
        commit(&root, "a.txt", "1\n");
        let mut e = engine(&f.dir);
        add(&mut e, &root);
        let looked = now();
        assert!(e.check(&root, looked).is_empty());
        let sha = commit(&root, "a.txt", "1\n2\n");
        assert_eq!(spans(&e.check(&root, looked + 5.0)), [(sha, Some(looked))]);
        let sha = commit(&root, "a.txt", "1\n2\n3\n");
        assert_eq!(spans(&e.check(&root, looked + 9.0)), [(sha, Some(looked + 5.0))]);
    }

    #[test]
    fn a_commit_made_before_the_last_look_and_checked_out_after_has_no_span() {
        let f = fixture("branch");
        let root = repo(&f.dir, "r");
        commit(&root, "a.txt", "1\n");
        let mut e = engine(&f.dir);
        add(&mut e, &root);
        git(&root, &["checkout", "-q", "-b", "feature"]);
        let made = commit(&root, "f.txt", "f\n");
        git(&root, &["checkout", "-q", "main"]);
        assert!(check(&mut e, &root).is_empty());
        git(&root, &["checkout", "-q", "feature"]);
        assert_eq!(spans(&check(&mut e, &root)), [(made, None)]);
    }

    #[test]
    fn a_restart_watches_recent_repos_again_and_keeps_their_span() {
        let f = fixture("restore");
        let root = repo(&f.dir, "r");
        commit(&root, "a.txt", "1\n");
        let mut e = engine(&f.dir);
        let t = add(&mut e, &root);
        let looked = now();
        e.touched(&[root.join(".git/HEAD")], t);
        assert!(e.tick(t + DEBOUNCE, looked).is_empty());
        drop(e);
        let sha = commit(&root, "a.txt", "1\n2\n");
        let mut e = engine(&f.dir);
        let t = Instant::now();
        assert!(!e.restore(t, now()).is_empty(), "watched without waiting for activity");
        let events = e.tick(t, now());
        assert_eq!(spans(&events), [(sha, Some(looked))]);
        assert_eq!(events[0].project.as_deref(), Some("o/r"));

        let mut e = engine(&f.dir);
        assert!(e.restore(Instant::now(), now() + MAX_AGE + 60.0).is_empty(), "a repo idle for two days waits for activity");
    }

    #[test]
    fn state_from_an_older_kairosd_or_a_rewritten_reflog_gives_no_span() {
        let f = fixture("legacy");
        let root = repo(&f.dir, "r");
        let head = commit(&root, "a.txt", "1\n");
        let t = now();
        let mut repos = serde_json::Map::new();
        repos.insert(key(&root), serde_json::json!({ "head": head, "since": t, "touched": t }));
        let old = serde_json::json!({ "repos": repos, "seen": [] });
        fs::write(f.dir.join("state.json"), old.to_string()).unwrap();
        let sha = commit(&root, "a.txt", "1\n2\n");
        let mut e = engine(&f.dir);
        assert!(e.restore(Instant::now(), now()).is_empty(), "no project id saved");
        add(&mut e, &root);
        assert_eq!(spans(&check(&mut e, &root)), [(sha, None)]);

        assert!(check(&mut e, &root).is_empty());
        fs::write(root.join(".git/logs/HEAD"), "").unwrap();
        let sha = commit(&root, "a.txt", "1\n2\n3\n");
        assert_eq!(spans(&check(&mut e, &root)), [(sha, None)]);
    }

    #[test]
    fn old_commits_arriving_by_branch_switch_or_age_are_skipped() {
        let f = fixture("old");
        let root = repo(&f.dir, "r");
        commit(&root, "a.txt", "1\n");
        let old = [("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z"), ("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")];
        git(&root, &["checkout", "-q", "-b", "feature"]);
        fs::write(root.join("f.txt"), "f\n").unwrap();
        git(&root, &["add", "-A"]);
        git_env(&root, &["commit", "-q", "-m", "old"], &old);
        git(&root, &["checkout", "-q", "main"]);
        let mut e = engine(&f.dir);
        add(&mut e, &root);
        git(&root, &["checkout", "-q", "feature"]);
        assert!(check(&mut e, &root).is_empty());
        git(&root, &["checkout", "-q", "main"]);
        commit(&root, "m.txt", "m\n");
        let three_days_later = now() + 3.0 * 24.0 * 3600.0;
        assert!(e.check(&root, three_days_later).is_empty());
    }

    #[test]
    fn merges_are_skipped_detached_head_and_binary_files_count() {
        let f = fixture("merge");
        let root = repo(&f.dir, "r");
        commit(&root, "a.txt", "1\n");
        let mut e = engine(&f.dir);
        add(&mut e, &root);
        git(&root, &["checkout", "-q", "-b", "feature"]);
        let feature = commit(&root, "f.txt", "f\n");
        assert_eq!(commits(&check(&mut e, &root)).len(), 1);
        git(&root, &["checkout", "-q", "main"]);
        commit(&root, "m.txt", "m\n");
        assert_eq!(commits(&check(&mut e, &root)).len(), 1);
        git(&root, &["merge", "-q", "--no-ff", "-m", "merge", "feature"]);
        assert!(check(&mut e, &root).is_empty());
        assert!(e.saved.seen.contains(&feature));
        git(&root, &["checkout", "-q", "--detach"]);
        fs::write(root.join("logo.bin"), [0u8, 159, 146, 150, 0, 1]).unwrap();
        let sha = commit(&root, "t.txt", "a\nb\n");
        assert_eq!(commits(&check(&mut e, &root)), [(sha, 2, 2, 0)]);
    }

    #[test]
    fn renames_count_once_and_the_first_commit_of_a_new_repo_emits() {
        let f = fixture("rename");
        let root = repo(&f.dir, "r");
        let mut e = engine(&f.dir);
        add(&mut e, &root);
        assert!(check(&mut e, &root).is_empty());
        let first = commit(&root, "big.txt", "1\n2\n3\n4\n5\n6\n7\n8\n");
        assert_eq!(commits(&check(&mut e, &root)), [(first, 1, 8, 0)]);
        git(&root, &["mv", "big.txt", "renamed.txt"]);
        git(&root, &["commit", "-q", "-m", "mv"]);
        let sha = git(&root, &["rev-parse", "HEAD"]);
        assert_eq!(commits(&check(&mut e, &root)), [(sha, 1, 0, 0)]);
    }

    #[test]
    fn linked_worktrees_resolve_their_own_git_dir() {
        let f = fixture("worktree");
        let root = repo(&f.dir, "r");
        commit(&root, "a.txt", "1\n");
        let wt = f.dir.join("wt");
        git(&root, &["worktree", "add", "-q", "-b", "wtb", wt.to_str().unwrap()]);
        let mut e = engine(&f.dir);
        let t = Instant::now();
        let watch = e.register(wt.clone(), "o/r".into(), t, now());
        assert!(watch.iter().any(|(p, _)| p.ends_with("worktrees/wt")));
        let sha = commit(&wt, "w.txt", "w\n");
        e.touched(&[root.join(".git/refs/heads/wtb")], t);
        assert_eq!(commits(&e.tick(t + DEBOUNCE, now())), [(sha, 1, 1, 0)]);
        e.touched(&[root.join(".git/index"), root.join(".git/HEAD.lock")], t + Duration::from_secs(60));
        assert!(e.repos[&wt].check_at.is_none());
    }

    #[test]
    fn submodules_are_ignored_and_stay_out_of_the_diff() {
        let f = fixture("sub");
        let inner = repo(&f.dir, "inner");
        commit(&inner, "i.txt", "i\n");
        let root = repo(&f.dir, "outer");
        commit(&root, "o.txt", "o\n");
        git(&root, &["-c", "protocol.file.allow=always", "submodule", "add", "-q", inner.to_str().unwrap(), "sub"]);
        git(&root, &["commit", "-q", "-m", "sub"]);
        let mut e = engine(&f.dir);
        let t = Instant::now();
        assert!(e.register(root.join("sub"), "o/sub".into(), t, now()).is_empty());
        assert!(e.repos.is_empty());
        configure(&root.join("sub"));
        commit(&root.join("sub"), "i2.txt", "i2\n");
        assert_eq!(totals(&e.git.uncommitted(&root).unwrap()), (0, 0, 0));
        assert!(!e.register(root.clone(), "o/r".into(), t, now()).is_empty());
    }

    #[test]
    fn diff_emits_on_change_only_and_respects_the_rate_limit() {
        let f = fixture("diff");
        let root = repo(&f.dir, "r");
        commit(&root, "a.txt", "1\n2\n3\n");
        let mut e = engine(&f.dir);
        let t = add(&mut e, &root);
        e.activity(&root);
        assert!(e.tick(t, now()).is_empty(), "clean tree emits nothing");
        fs::write(root.join("a.txt"), "1\nx\n3\n4\n").unwrap();
        fs::write(root.join("untracked.txt"), "counted too\n").unwrap();
        e.activity(&root);
        assert!(e.tick(t + Duration::from_secs(10), now()).is_empty(), "inside the rate limit");
        let at = t + DIFF_EVERY + Duration::from_secs(1);
        assert_eq!(diffs(&e.tick(at, now())), [(2, 3, 1)]);
        e.activity(&root);
        assert!(e.tick(at + DIFF_EVERY + Duration::from_secs(1), now()).is_empty(), "unchanged");
        fs::write(root.join("a.txt"), "1\nx\n3\n4\n5\n").unwrap();
        e.activity(&root);
        let at2 = at + 2 * DIFF_EVERY + Duration::from_secs(2);
        assert_eq!(diffs(&e.tick(at2, now())), [(2, 4, 1)]);
        commit(&root, "a.txt", "1\nx\n3\n4\n5\n");
        e.touched(&[root.join(".git/HEAD")], at2);
        let events = e.tick(at2 + 3 * DIFF_EVERY, now());
        assert_eq!(commits(&events).len(), 1);
        assert_eq!(diffs(&events), [(0, 0, 0)]);
    }

    fn untracked(root: &Path, budget: u64) -> Stat {
        let mut tally = Tally::default();
        Git::default().untracked(root, &mut tally, budget);
        tally.finish()
    }

    fn changed(events: &[Event]) -> Vec<Vec<FileChange>> {
        events
            .iter()
            .filter_map(|e| match &e.kind {
                Kind::Commit { changes, .. } | Kind::Diff { changes, .. } => Some(changes.clone()),
                _ => None,
            })
            .collect()
    }

    fn vias(events: &[Event]) -> Vec<Option<Via>> {
        events
            .iter()
            .filter_map(|e| match &e.kind {
                Kind::Commit { via, .. } => Some(*via),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn untracked_files_count_as_added_lines_and_ignored_ones_do_not() {
        let f = fixture("untracked");
        let root = repo(&f.dir, "r");
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        commit(&root, "a.txt", "1\n");
        fs::create_dir_all(root.join("dir")).unwrap();
        fs::write(root.join("new.txt"), "a\nb\nc").unwrap();
        fs::write(root.join("full.txt"), "x\n").unwrap();
        fs::write(root.join("empty.txt"), "").unwrap();
        fs::write(root.join("bin.dat"), [1u8, 0, 2, b'\n', b'\n']).unwrap();
        fs::write(root.join("skip.log"), "ignored\n").unwrap();
        fs::write(root.join("dir/inner.txt"), "1\n2\n").unwrap();
        fs::write(root.join("a.txt"), "1\n2\n").unwrap();
        let stat = Git::default().uncommitted(&root).unwrap();
        assert_eq!(totals(&stat), (6, 7, 0));
        assert_eq!(
            stat.changes,
            [change("a.txt", 1, 0), change("bin.dat", 0, 0), change("dir/inner.txt", 2, 0), change("empty.txt", 0, 0), change("full.txt", 1, 0), change("new.txt", 3, 0)]
        );
    }

    #[test]
    fn untracked_files_over_the_size_cap_are_counted_without_lines_or_a_listing() {
        let f = fixture("untracked-size");
        let root = repo(&f.dir, "r");
        commit(&root, "a.txt", "1\n");
        fs::write(root.join("exact.txt"), "a\n".repeat(UNTRACKED_FILE_MAX as usize / 2)).unwrap();
        fs::write(root.join("over.txt"), "a\n".repeat(UNTRACKED_FILE_MAX as usize / 2 + 1)).unwrap();
        let stat = untracked(&root, UNTRACKED_BUDGET);
        assert_eq!(totals(&stat), (2, UNTRACKED_FILE_MAX as u32 / 2, 0));
        assert_eq!(stat.changes, [change("exact.txt", UNTRACKED_FILE_MAX as u32 / 2, 0)]);
    }

    #[test]
    fn untracked_reads_stop_at_the_budget_but_the_files_still_count() {
        let f = fixture("untracked-budget");
        let root = repo(&f.dir, "r");
        commit(&root, "a.txt", "1\n");
        for name in ["b1", "b2", "b3", "b4"] {
            fs::write(root.join(name), "1\n2\n3\n").unwrap();
        }
        let stat = untracked(&root, 13);
        assert_eq!(totals(&stat), (4, 6, 0));
        assert_eq!(stat.changes, [change("b1", 3, 0), change("b2", 3, 0)]);
        let stat = untracked(&root, 0);
        assert_eq!((totals(&stat), stat.changes.len()), ((4, 0, 0), 0));
    }

    #[cfg(unix)]
    #[test]
    fn untracked_symlinks_are_not_followed_and_long_paths_are_only_counted() {
        let f = fixture("untracked-links");
        let root = repo(&f.dir, "r");
        commit(&root, "a.txt", "1\n");
        fs::write(root.join("real.txt"), "1\n2\n").unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/in.txt"), "x\n").unwrap();
        std::os::unix::fs::symlink("real.txt", root.join("link.txt")).unwrap();
        std::os::unix::fs::symlink("sub", root.join("sublink")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("abs.txt")).unwrap();
        std::os::unix::fs::symlink("/nonexistent", root.join("dangling")).unwrap();
        let deep = root.join("d".repeat(150)).join("e".repeat(150)).join("g".repeat(150));
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("f.txt"), "1\n2\n3\n").unwrap();
        let stat = untracked(&root, UNTRACKED_BUDGET);
        assert_eq!(totals(&stat), (3, 6, 0));
        assert_eq!(stat.changes, [change("real.txt", 2, 0), change("sub/in.txt", 1, 0)]);
    }

    #[test]
    fn only_paths_inside_the_repo_are_read() {
        for bad in [&b"/etc/passwd"[..], b"../x", b"a/../b", b"./a", b"", b"a/.."] {
            assert_eq!(relative(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
        assert_eq!(relative(b"a/b c.txt"), Some(PathBuf::from("a").join("b c.txt")));
    }

    #[test]
    fn a_commit_is_stamped_when_it_was_seen_and_carries_git_s_dates_and_its_files() {
        let f = fixture("dates");
        let root = repo(&f.dir, "r");
        commit(&root, "a.txt", "1\n");
        let mut e = engine(&f.dir);
        let t = Instant::now();
        let start = now();
        e.register(root.clone(), "o/r".into(), t, start - 3.0 * 3600.0);
        let (authored, committed) = (start as i64 - 7200, start as i64 - 3600);
        let (au, co) = (format!("{authored} +0000"), format!("{committed} +0000"));
        fs::write(root.join("a.txt"), "1\n2\n").unwrap();
        fs::write(root.join("b.txt"), "x\ny\nz\n").unwrap();
        git(&root, &["add", "-A"]);
        git_env(&root, &["commit", "-q", "-m", "work"], &[("GIT_AUTHOR_DATE", &au), ("GIT_COMMITTER_DATE", &co)]);
        let events = check(&mut e, &root);
        assert_eq!(events.len(), 1);
        assert!((events[0].ts - now()).abs() < 5.0, "ts is when the commit was seen, not git's date");
        let Kind::Commit { files, added, removed, authored: a, committed: c, via, changes, .. } = &events[0].kind else {
            panic!("not a commit");
        };
        assert_eq!((*files, *added, *removed), (2, 4, 0));
        assert_eq!((*a, *c), (Some(authored), Some(committed)));
        assert_eq!(*via, Some(Via::Commit));
        assert_eq!(changes, &[change("a.txt", 1, 0), change("b.txt", 3, 0)]);
    }

    #[test]
    fn a_diff_is_emitted_when_only_the_split_between_files_changes() {
        let f = fixture("split");
        let root = repo(&f.dir, "r");
        fs::write(root.join("b.txt"), "1\n").unwrap();
        commit(&root, "a.txt", "1\n");
        let mut e = engine(&f.dir);
        let t = add(&mut e, &root);
        fs::write(root.join("a.txt"), "1\n2\n").unwrap();
        let first = e.sample(&root, t, now()).unwrap();
        assert_eq!(changed(&[first]), [[change("a.txt", 1, 0)]]);
        assert!(e.sample(&root, t, now()).is_none());
        fs::write(root.join("a.txt"), "1\n").unwrap();
        fs::write(root.join("b.txt"), "1\n2\n").unwrap();
        let second = e.sample(&root, t, now()).unwrap();
        assert_eq!(diffs(&[second.clone()]), [(1, 1, 0)]);
        assert_eq!(changed(&[second]), [[change("b.txt", 1, 0)]]);
        assert!(e.sample(&root, t, now()).is_none());
    }

    #[test]
    fn the_reflog_tells_how_each_commit_reached_head() {
        let f = fixture("via");
        let root = repo(&f.dir, "r");
        commit(&root, "a.txt", "1\n");
        let mut e = engine(&f.dir);
        add(&mut e, &root);
        let mut seen = Vec::new();
        let mut step = |e: &mut Engine, want: &[Option<Via>]| {
            let events = check(e, &root);
            assert_eq!(vias(&events), want);
            seen.extend(events.clone());
            events
        };
        let commit_of = |events: &[Event]| match &events[0].kind {
            Kind::Commit { sha, replaces, .. } => (sha.clone(), replaces.clone()),
            other => panic!("not a commit: {other:?}"),
        };

        fs::write(root.join("b.txt"), "b\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "tell nobody"]);
        let (first, none) = commit_of(&step(&mut e, &[Some(Via::Commit)]));
        assert_eq!(none, None);

        git(&root, &["commit", "-q", "--amend", "-m", "tell nobody again"]);
        let (_, replaced) = commit_of(&step(&mut e, &[Some(Via::Amend)]));
        assert_eq!(replaced, Some(first), "an amend names the commit it replaced");

        git(&root, &["checkout", "-q", "-b", "side"]);
        commit(&root, "s.txt", "s\n");
        git(&root, &["checkout", "-q", "main"]);
        commit(&root, "pre.txt", "p\n");
        git(&root, &["cherry-pick", "side"]);
        step(&mut e, &[Some(Via::Commit), Some(Via::CherryPick)]);

        git(&root, &["revert", "--no-edit", "HEAD"]);
        step(&mut e, &[Some(Via::Revert)]);

        git(&root, &["checkout", "-q", "-b", "topic"]);
        commit(&root, "t1.txt", "1\n");
        commit(&root, "t2.txt", "2\n");
        git(&root, &["checkout", "-q", "main"]);
        commit(&root, "m1.txt", "m\n");
        step(&mut e, &[Some(Via::Commit)]);
        git(&root, &["checkout", "-q", "topic"]);
        git(&root, &["rebase", "-q", "main"]);
        step(&mut e, &[Some(Via::Rebase), Some(Via::Rebase)]);
        git(&root, &["checkout", "-q", "main"]);
        commit(&root, "m2.txt", "m\n");
        step(&mut e, &[Some(Via::Commit)]);
        git(&root, &["checkout", "-q", "topic"]);
        git_env(&root, &["rebase", "-q", "-i", "main"], &[("GIT_SEQUENCE_EDITOR", "true")]);
        step(&mut e, &[Some(Via::Rebase), Some(Via::Rebase)]);
        git(&root, &["checkout", "-q", "main"]);
        step(&mut e, &[]);

        let remote = f.dir.join("remote.git");
        git(&f.dir, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(&root, &["remote", "add", "origin", remote.to_str().unwrap()]);
        git(&root, &["push", "-q", "origin", "main"]);
        let two = f.dir.join("two");
        git(&f.dir, &["clone", "-q", remote.to_str().unwrap(), two.to_str().unwrap()]);
        configure(&two);
        commit(&two, "x1.txt", "1\n");
        commit(&two, "x2.txt", "2\n");
        commit(&two, "x3.txt", "3\n");
        git(&two, &["push", "-q", "origin", "main"]);
        git(&root, &["fetch", "-q", "origin"]);
        git(&root, &["-c", "pull.rebase=false", "pull", "-q", "--ff-only", "origin", "main"]);
        step(&mut e, &[Some(Via::Arrived), Some(Via::Arrived), Some(Via::Arrived)]);

        commit(&two, "y.txt", "y\n");
        git(&two, &["push", "-q", "origin", "main"]);
        git(&root, &["fetch", "-q", "origin"]);
        git(&root, &["reset", "-q", "--hard", "origin/main"]);
        step(&mut e, &[Some(Via::Arrived)]);

        let log = serde_json::to_string(&seen).unwrap();
        assert!(!log.contains("tell nobody"), "a commit message reached an event");
    }

    #[test]
    fn a_repo_without_a_head_reflog_reports_no_via() {
        let f = fixture("noreflog");
        let root = repo(&f.dir, "r");
        git(&root, &["config", "core.logAllRefUpdates", "false"]);
        commit(&root, "a.txt", "1\n");
        let mut e = engine(&f.dir);
        add(&mut e, &root);
        commit(&root, "a.txt", "1\n2\n");
        assert_eq!(vias(&check(&mut e, &root)), [None]);
    }

    #[test]
    fn missing_git_disables_cleanly() {
        let f = fixture("nogit");
        let root = repo(&f.dir, "r");
        commit(&root, "a.txt", "1\n");
        let mut e = Engine::new(Git { program: "/nonexistent/kairos-git".into() }, f.dir.join("state.json"), Arc::new(Excluded::new(f.dir.clone())));
        let t = Instant::now();
        assert!(e.register(root.clone(), "o/r".into(), t, now()).is_empty());
        assert!(e.avail == Avail::No);
        assert!(e.tick(t + Duration::from_secs(5), now()).is_empty());
        assert!(e.repos.is_empty());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn git_installed_later_is_found_on_a_retry() {
        let f = fixture("lategit");
        let root = repo(&f.dir, "r");
        commit(&root, "a.txt", "1\n");
        let mut e = Engine::new(Git { program: "/nonexistent/kairos-git".into() }, f.dir.join("state.json"), Arc::new(Excluded::new(f.dir.clone())));
        let t = Instant::now();
        e.register(root.clone(), "o/r".into(), t, now());
        e.git = Git::default();
        e.register(root.clone(), "o/r".into(), t + RETRY - Duration::from_secs(1), now());
        assert!(e.avail == Avail::No);
        e.register(root.clone(), "o/r".into(), t + RETRY, now());
        assert!(e.avail == Avail::Yes);
        assert!(e.repos.contains_key(&root));
    }

    #[test]
    fn a_repo_deleted_while_followed_is_dropped() {
        let f = fixture("gone");
        let root = repo(&f.dir, "r");
        commit(&root, "a.txt", "1\n");
        let mut e = engine(&f.dir);
        let t = add(&mut e, &root);
        e.activity(&root);
        e.touched(&[root.join(".git/HEAD")], t);
        fs::remove_dir_all(&root).unwrap();
        assert!(e.tick(t + Duration::from_secs(5), now()).is_empty());
        assert!(e.repos.is_empty());
        assert!(!e.unwatch.is_empty());
    }

    #[test]
    fn an_excluded_repo_is_never_followed_and_one_that_becomes_excluded_is_dropped() {
        let f = fixture("excluded");
        let (a, ab) = (repo(&f.dir, "a"), repo(&f.dir, "ab"));
        commit(&a, "a.txt", "1\n");
        commit(&ab, "a.txt", "1\n");
        kairos_core::exclude::add(&f.dir, &a).unwrap();
        let mut e = engine(&f.dir);
        let t = Instant::now();
        assert!(e.register(a.clone(), "o/a".into(), t, now()).is_empty());
        assert!(!e.register(ab.clone(), "o/ab".into(), t, now()).is_empty(), "a sibling whose name starts the same");
        assert_eq!(e.repos.len(), 1);
        assert!(!e.saved.repos.contains_key(&key(&a)));

        commit(&ab, "a.txt", "2\n");
        kairos_core::exclude::add(&f.dir, &ab).unwrap();
        e.touched(&[ab.join(".git/HEAD")], t);
        e.activity(&ab);
        assert!(e.tick(t + Duration::from_secs(10), now()).is_empty(), "no commit and no diff once it is excluded");
        assert!(e.repos.is_empty() && e.saved.repos.is_empty());
        assert!(!e.unwatch.is_empty());
        assert!(e.register(ab, "o/ab".into(), t, now()).is_empty());
    }

    #[test]
    fn a_deleted_dot_git_never_falls_through_to_a_parent_repo() {
        let f = fixture("parent");
        let parent = repo(&f.dir, "parent");
        commit(&parent, "a.txt", "1\n");
        let child = parent.join("child");
        fs::create_dir_all(&child).unwrap();
        git(&child, &["init", "-q"]);
        let mut e = engine(&f.dir);
        let t = add(&mut e, &child);
        assert_eq!(e.repos.len(), 1);
        fs::remove_dir_all(child.join(".git")).unwrap();
        e.activity(&child);
        assert!(e.tick(t, now()).is_empty());
        assert!(e.repos.is_empty());
    }

    // kairosd has no console, so a console program it starts opens a window unless created without one.
    #[cfg(windows)]
    mod console_windows {
        use std::cell::RefCell;
        use std::os::windows::process::CommandExt;
        use std::ptr::null_mut;

        use windows_sys::Win32::Foundation::HWND;
        use windows_sys::Win32::System::Threading::DETACHED_PROCESS;
        use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, GetClassNameW, PeekMessageW, EVENT_OBJECT_SHOW, MSG, OBJID_WINDOW, PM_REMOVE,
            WINEVENT_OUTOFCONTEXT,
        };

        use super::*;
        use crate::classify::CONSOLE_WINDOW;

        const MODE: &str = "KAIROS_GIT_WINDOW_HELPER";
        const REPO: &str = "KAIROS_GIT_WINDOW_REPO";
        // A new console shows as a classic console window, or as a Windows Terminal window and its
        // ConPTY window when Windows Terminal is the default terminal.
        const CONSOLE_CLASSES: &[&str] = &[CONSOLE_WINDOW, "CASCADIA_HOSTING_WINDOW_CLASS", "PseudoConsoleWindow"];

        thread_local! {
            static SHOWN: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
        }

        unsafe extern "system" fn shown(_: HWINEVENTHOOK, _: u32, hwnd: HWND, object: i32, _: i32, _: u32, _: u32) {
            if object != OBJID_WINDOW {
                return;
            }
            let mut buf = [0u16; 128];
            let n = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
            SHOWN.with(|s| s.borrow_mut().push(String::from_utf16_lossy(&buf[..n])));
        }

        fn pump() {
            let mut msg = MSG::default();
            while unsafe { PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) } != 0 {
                unsafe { DispatchMessageW(&msg) };
            }
        }

        fn windows_shown_by(mode: &str, repo: &Path) -> Vec<String> {
            SHOWN.with(|s| s.borrow_mut().clear());
            let hook = unsafe {
                SetWinEventHook(EVENT_OBJECT_SHOW, EVENT_OBJECT_SHOW, null_mut(), Some(shown), 0, 0, WINEVENT_OUTOFCONTEXT)
            };
            assert!(!hook.is_null(), "SetWinEventHook failed");
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "git::tests::console_windows::helper", "--nocapture"])
                .env(MODE, mode)
                .env(REPO, repo)
                .creation_flags(DETACHED_PROCESS)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut until = None;
            loop {
                pump();
                if until.is_some_and(|t| Instant::now() >= t) {
                    break;
                }
                // Hook events arrive through the message queue, so keep pumping a little after the exit.
                if until.is_none() && child.try_wait().unwrap().is_some() {
                    until = Some(Instant::now() + Duration::from_millis(500));
                }
                if Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("the {mode} helper did not finish");
                }
                thread::sleep(Duration::from_millis(10));
            }
            unsafe { UnhookWinEvent(hook) };
            let out = child.wait_with_output().unwrap();
            let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(out.status.success() && text.contains("1 passed"), "the {mode} helper failed: {text}");
            SHOWN.with(RefCell::take)
        }

        #[test]
        fn helper() {
            let Ok(mode) = std::env::var(MODE) else { return };
            let repo = PathBuf::from(std::env::var_os(REPO).unwrap());
            if mode == "plain" {
                assert!(Command::new("git").arg("--version").stdout(Stdio::null()).status().unwrap().success());
                return;
            }
            let git = Git::default();
            assert!(git.available());
            assert!(!git.dirs(&repo).unwrap().submodule);
            let head = git.head(&repo).unwrap().expect("a commit");
            assert_eq!(git.commits(&repo, None, &head).unwrap().len(), 1);
            assert_eq!(git.email(&repo).as_deref(), Some("me@example.com"));
            assert_eq!(totals(&git.commit_size(&repo, &head).unwrap()), (1, 2, 0));
            assert_eq!(totals(&git.uncommitted(&repo).unwrap()), (1, 1, 0));
        }

        #[test]
        #[ignore = "watches the interactive desktop for console windows"]
        fn git_runs_without_a_console_window() {
            let f = fixture("window");
            let root = repo(&f.dir, "r");
            commit(&root, "a.txt", "1\n2\n");
            fs::write(root.join("a.txt"), "1\n2\n3\n").unwrap();
            let control = windows_shown_by("plain", &root);
            println!("plain git from a process without a console showed {control:?}");
            assert!(control.iter().any(|c| CONSOLE_CLASSES.contains(&c.as_str())), "no console seen for plain git, this check is blind here");
            let ours = windows_shown_by("watcher", &root);
            println!("the watcher's git showed {ours:?}");
            assert!(!ours.iter().any(|c| CONSOLE_CLASSES.contains(&c.as_str())));
        }
    }
}
