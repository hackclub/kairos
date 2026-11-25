use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use kairos_core::event::{Event, Kind, MAX_PATHS, MAX_PATH_LEN};
use kairos_core::exclude::Exclusions;
use kairos_core::project::{project_of, Project};
use notify::event::{EventKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use walkdir::WalkDir;

use crate::diag::diag;
use crate::exclusions::Excluded;
use crate::git;
use crate::project_ids::ProjectIds;
use crate::{now, Msg};

const IGNORED_DIRS: &[&str] = &[".git", "target", "node_modules", ".venv", "venv", "site-packages", "__pycache__", ".next"];
const QUIET: Duration = Duration::from_secs(2);
const MAX_WAIT: Duration = Duration::from_secs(30);
const MAX_DIRS: usize = 8192;

pub enum Watch {
    Root(PathBuf, String),
    Fs(notify::Event),
}

fn ignored(path: &Path) -> bool {
    path.components().any(|c| IGNORED_DIRS.iter().any(|d| c.as_os_str() == *d))
        || path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
            n.ends_with('~') || n.ends_with(".swp") || n.ends_with(".swx") || n.ends_with(".tmp") || n == "4913" || atomic_write(n)
        })
}

// Claude Code writes `a.rs.tmp.<pid>.<hex>` and renames it over `a.rs`.
fn atomic_write(name: &str) -> bool {
    name.rsplit_once(".tmp.").and_then(|(_, rest)| rest.split_once('.')).is_some_and(|(pid, id)| {
        !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()) && !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

struct State {
    watcher: RecommendedWatcher,
    roots: Vec<(PathBuf, String)>,
    dirs: HashSet<PathBuf>,
    warned: bool,
}

impl State {
    fn add_tree(&mut self, top: &Path, ex: &Exclusions) {
        let walk = WalkDir::new(top)
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || !(IGNORED_DIRS.iter().any(|d| e.file_name() == *d) || ex.covers(e.path())));
        for entry in walk.flatten().filter(|e| e.file_type().is_dir()) {
            if self.dirs.len() >= MAX_DIRS {
                if !self.warned {
                    diag!("kairosd: warning: watch limit reached, some project folders are not watched");
                    self.warned = true;
                }
                return;
            }
            if self.dirs.insert(entry.path().to_path_buf()) {
                let _ = self.watcher.watch(entry.path(), RecursiveMode::NonRecursive);
            }
        }
    }

    fn forget(&mut self, ex: &Exclusions) -> Vec<String> {
        let covered: Vec<PathBuf> = self.dirs.iter().filter(|d| ex.covers(d)).cloned().collect();
        for dir in covered {
            self.dirs.remove(&dir);
            let _ = self.watcher.unwatch(&dir);
        }
        let mut ids = Vec::new();
        self.roots.retain(|(root, id)| {
            let keep = !ex.covers(root);
            if !keep {
                ids.push(id.clone());
            }
            keep
        });
        ids
    }

    fn add_root(&mut self, root: PathBuf, id: String, ex: &Exclusions, ids: &mut ProjectIds, out: &Sender<Msg>, git: &Sender<git::Cmd>) {
        if let Some(from) = ids.observe(&root, &id, now()) {
            diag!("kairosd: project {from} is now {id}");
            let _ = out.send(Msg::Event(Event::new(now(), Some(id.clone()), Kind::ProjectAlias { from })));
        }
        let _ = git.send(git::Cmd::Root(root.clone(), id.clone()));
        match self.roots.iter_mut().find(|(r, _)| *r == root) {
            Some((_, known)) => *known = id,
            None => {
                self.add_tree(&root, ex);
                self.roots.push((root, id));
            }
        }
    }

    fn project_of(&self, path: &Path) -> Option<&(PathBuf, String)> {
        self.roots.iter().filter(|(r, _)| path.starts_with(r)).max_by_key(|(r, _)| r.as_os_str().len())
    }
}

// A repo cloned inside a watched folder is its own project, as it is for agent edits.
fn nested_repo(file: &Path, root: &Path) -> Option<Project> {
    let repo = file.parent()?.ancestors().take_while(|a| *a != root).find(|a| a.join(".git").exists())?;
    project_of(repo)
}

#[derive(Default)]
struct Saves {
    files: HashSet<PathBuf>,
}

impl Saves {
    fn add(&mut self, path: PathBuf) {
        self.files.insert(path);
    }

    /// Without what git ignores (build output, say): `None` when nothing else is left.
    fn kind(mut self, root: &Path) -> Option<Kind> {
        let ignored = git::ignored(root, &self.files);
        self.files.retain(|f| !ignored.contains(f));
        if self.files.is_empty() {
            return None;
        }
        let mut paths = BTreeSet::new();
        for f in &self.files {
            if let Some(rel) = relative(root, f) {
                paths.insert(rel);
                if paths.len() > MAX_PATHS {
                    paths.pop_last();
                }
            }
        }
        Some(Kind::FileSave { count: self.files.len(), paths: paths.into_iter().collect() })
    }
}

fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<_> = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect();
    let joined = parts.join("/");
    (!joined.is_empty() && joined.len() <= MAX_PATH_LEN).then_some(joined)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn saved_paths_are_relative_sorted_deduped_and_capped() {
        let root = Path::new("proj");
        let mut saves = Saves::default();
        for i in (0..30).rev() {
            saves.add(root.join("src").join(format!("f{i:02}.rs")));
        }
        saves.add(root.join("src").join("f05.rs"));
        saves.add(root.join("a b").join("é.txt"));
        saves.add(root.join("x".repeat(MAX_PATH_LEN)));
        let Some(Kind::FileSave { count, paths }) = saves.kind(root) else { panic!("not a file save") };
        assert_eq!(count, 32);
        assert_eq!(paths.len(), MAX_PATHS);
        assert_eq!(paths[0], "a b/é.txt");
        assert_eq!(paths[1], "src/f00.rs");
        assert!(paths.windows(2).all(|w| w[0] < w[1]));
        assert!(paths.iter().all(|p| !p.contains('\\') && !p.starts_with("proj")));
    }

    #[test]
    fn long_and_root_paths_are_counted_but_not_listed() {
        let root = Path::new("proj");
        let mut saves = Saves::default();
        saves.add(root.join("y".repeat(MAX_PATH_LEN + 1)));
        saves.add(root.join("z".repeat(MAX_PATH_LEN)));
        assert_eq!(saves.kind(root), Some(Kind::FileSave { count: 2, paths: vec!["z".repeat(MAX_PATH_LEN)] }));
        assert_eq!(relative(root, root), None);
        assert_eq!(relative(root, Path::new("other/f")), None);
    }

    #[test]
    fn temporary_files_of_an_atomic_write_are_not_saves() {
        for name in ["hello.py.tmp.244168.6a3edd58acb0", "hello.py.tmp.255391.1795562851350", "hello.py.tmp.48213.9f3ac1d2", "a.rs.tmp.7.0b", "notes.tmp", "a.rs~", ".a.rs.swp"] {
            assert!(ignored(Path::new("/w/p/src").join(name).as_path()), "{name}");
        }
        for name in ["hello.py", "a.tmp.rs", "a.tmp.c", "a.tmp.db", "a.tmp.12", "a.tmp.", "a.tmp.1..2", "a.tmp.x.1f", "tmp.123", "template.html"] {
            assert!(!ignored(Path::new("/w/p/src").join(name).as_path()), "{name}");
        }
    }

    #[test]
    fn files_git_ignores_are_not_saves() {
        let root = std::env::temp_dir().join(format!("kairos-watch-gitignore-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub")).unwrap();
        let git = |args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(&root).args(args).output().unwrap().status.success(), "git {args:?}");
        git(&["init", "-q"]);
        fs::write(root.join(".gitignore"), "dist/\n*.log\n").unwrap();
        fs::write(root.join("sub/.gitignore"), "gen.txt\n").unwrap();
        fs::write(root.join(".git/info/exclude"), "local.cfg\n").unwrap();
        fs::create_dir_all(root.join("dist")).unwrap();
        fs::write(root.join("dist/keep.js"), "x").unwrap();
        git(&["add", "-f", "dist/keep.js"]);
        let mut saves = Saves::default();
        for f in ["src/a.rs", "dist/keep.js", "sub/ok.txt", "a b/é.txt", "dist/out.js", "build.log", "sub/gen.txt", "local.cfg"] {
            saves.add(root.join(f));
        }
        for i in 0..5000 {
            saves.add(root.join(format!("dist/chunk-{i}.js")));
        }
        let at = Instant::now();
        let kind = saves.kind(&root);
        println!("5008 files sorted in {:?}", at.elapsed());
        assert_eq!(kind, Some(Kind::FileSave { count: 4, paths: vec!["a b/é.txt".into(), "dist/keep.js".into(), "src/a.rs".into(), "sub/ok.txt".into()] }));

        let mut build = Saves::default();
        build.add(root.join("dist/out.js"));
        build.add(root.join("debug.log"));
        assert_eq!(build.kind(&root), None, "nothing is left when the batch was all build output");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn saves_are_counted_and_ignored_dirs_skipped() {
        let root = std::env::temp_dir().join(format!("kairos-watch-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        #[cfg(not(windows))]
        let root = root.canonicalize().unwrap();
        let (wtx, wrx) = mpsc::channel();
        let (otx, orx) = mpsc::channel();
        let own = root.with_extension("data");
        spawn(wrx, wtx.clone(), otx, own.clone(), mpsc::channel().0, Arc::new(Excluded::new(own.clone())));
        wtx.send(Watch::Root(root.clone(), "o/r".into())).unwrap();
        std::thread::sleep(Duration::from_millis(700));
        fs::write(root.join("src/a.rs"), "x").unwrap();
        fs::write(root.join("src/b.rs"), "x").unwrap();
        fs::write(root.join("target/junk"), "x").unwrap();
        let Msg::Event(e) = orx.recv_timeout(Duration::from_secs(8)).unwrap() else {
            panic!("expected an event");
        };
        assert_eq!(e.project.as_deref(), Some("o/r"));
        assert_eq!(e.kind, Kind::FileSave { count: 2, paths: vec!["src/a.rs".into(), "src/b.rs".into()] });
        assert!(orx.recv_timeout(Duration::from_secs(3)).is_err());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&own);
    }

    #[test]
    fn an_excluded_root_is_never_watched_and_one_that_becomes_excluded_stops() {
        let base = std::env::temp_dir().join(format!("kairos-watch-excluded-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for name in ["a", "ab"] {
            fs::create_dir_all(base.join(name).join("src")).unwrap();
        }
        fs::create_dir_all(base.join("data")).unwrap();
        let base = base.canonicalize().unwrap();
        let (a, ab, data) = (base.join("a"), base.join("ab"), base.join("data"));
        kairos_core::exclude::add(&data, &a).unwrap();
        let (wtx, wrx) = mpsc::channel();
        let (otx, orx) = mpsc::channel();
        let (gtx, grx) = mpsc::channel();
        spawn(wrx, wtx.clone(), otx, data.clone(), gtx, Arc::new(Excluded::new(data.clone())));
        wtx.send(Watch::Root(a.clone(), "o/a".into())).unwrap();
        wtx.send(Watch::Root(ab.clone(), "o/ab".into())).unwrap();
        std::thread::sleep(Duration::from_millis(700));
        fs::write(a.join("src/x.rs"), "x").unwrap();
        fs::write(ab.join("src/y.rs"), "x").unwrap();
        let Msg::Event(e) = orx.recv_timeout(Duration::from_secs(8)).unwrap() else {
            panic!("expected an event");
        };
        assert_eq!((e.project.as_deref(), e.kind), (Some("o/ab"), Kind::FileSave { count: 1, paths: vec!["src/y.rs".into()] }));
        let roots: Vec<PathBuf> = grx.try_iter().filter_map(|c| match c {
            git::Cmd::Root(root, _) => Some(root),
            _ => None,
        }).collect();
        assert_eq!(roots, [ab.clone()], "the git watcher hears of the sibling only");

        kairos_core::exclude::add(&data, &ab).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        fs::write(a.join("src/x.rs"), "y").unwrap();
        fs::write(ab.join("src/y.rs"), "y").unwrap();
        assert!(orx.recv_timeout(Duration::from_secs(4)).is_err(), "no saves once both are excluded");
        assert_eq!(fs::read_to_string(data.join("project-ids.json")).unwrap(), "{}");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_root_whose_id_changes_is_reported_once_and_saves_follow_the_new_id() {
        let base = std::env::temp_dir().join(format!("kairos-watch-alias-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("game/src")).unwrap();
        fs::create_dir_all(base.join("data")).unwrap();
        #[cfg(not(windows))]
        let base = base.canonicalize().unwrap();
        let root = base.join("game");
        let (wtx, wrx) = mpsc::channel();
        let (otx, orx) = mpsc::channel();
        spawn(wrx, wtx.clone(), otx, base.join("data"), mpsc::channel().0, Arc::new(Excluded::new(base.join("data"))));
        wtx.send(Watch::Root(root.clone(), "game".into())).unwrap();
        wtx.send(Watch::Root(root.clone(), "ada/game".into())).unwrap();
        wtx.send(Watch::Root(root.clone(), "ada/game".into())).unwrap();
        let Msg::Event(e) = orx.recv_timeout(Duration::from_secs(3)).unwrap() else {
            panic!("expected an event");
        };
        assert_eq!(e.project.as_deref(), Some("ada/game"));
        assert_eq!(e.kind, Kind::ProjectAlias { from: "game".into() });
        std::thread::sleep(Duration::from_millis(700));
        fs::write(root.join("src/a.rs"), "x").unwrap();
        let Msg::Event(e) = orx.recv_timeout(Duration::from_secs(8)).unwrap() else {
            panic!("expected an event");
        };
        assert_eq!((e.project.as_deref(), e.kind), (Some("ada/game"), Kind::FileSave { count: 1, paths: vec!["src/a.rs".into()] }));
        assert!(orx.recv_timeout(Duration::from_secs(3)).is_err());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_save_in_a_repo_inside_the_watched_folder_goes_to_that_repo() {
        let base = std::env::temp_dir().join(format!("kairos-watch-nested-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("oasis/phone/src")).unwrap();
        fs::create_dir_all(base.join("data")).unwrap();
        let base = base.canonicalize().unwrap();
        let (outer, inner) = (base.join("oasis"), base.join("oasis/phone"));
        let run = |args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(&inner).args(args).output().unwrap().status.success(), "git {args:?}");
        run(&["init", "-q"]);
        run(&["remote", "add", "origin", "https://github.com/ada/phone.git"]);
        let (wtx, wrx) = mpsc::channel();
        let (otx, orx) = mpsc::channel();
        let (gtx, grx) = mpsc::channel();
        spawn(wrx, wtx.clone(), otx, base.join("data"), gtx, Arc::new(Excluded::new(base.join("data"))));
        wtx.send(Watch::Root(outer.clone(), "oasis".into())).unwrap();
        std::thread::sleep(Duration::from_millis(700));
        fs::write(inner.join("src/a.rs"), "x").unwrap();
        fs::write(outer.join("notes.txt"), "x").unwrap();
        let mut got: Vec<(Option<String>, Kind)> = (0..2)
            .map(|_| match orx.recv_timeout(Duration::from_secs(8)).unwrap() {
                Msg::Event(e) => (e.project, e.kind),
                _ => panic!("expected an event"),
            })
            .collect();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(got, [
            (Some("ada/phone".into()), Kind::FileSave { count: 1, paths: vec!["src/a.rs".into()] }),
            (Some("oasis".into()), Kind::FileSave { count: 1, paths: vec!["notes.txt".into()] }),
        ]);
        let roots: Vec<PathBuf> = grx.try_iter().filter_map(|c| match c {
            git::Cmd::Root(root, _) => Some(root),
            _ => None,
        }).collect();
        assert_eq!(roots, [outer, inner], "the git watcher hears of the inner repo too");
        let _ = fs::remove_dir_all(&base);
    }
}

pub fn spawn(
    rx: Receiver<Watch>,
    tx_self: Sender<Watch>,
    out: Sender<Msg>,
    own_dir: PathBuf,
    git: Sender<git::Cmd>,
    excluded: Arc<Excluded>,
) {
    thread::spawn(move || {
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                let _ = tx_self.send(Watch::Fs(ev));
            }
        });
        let Ok(watcher) = watcher else {
            diag!("kairosd: warning: file watcher unavailable, emitting no file_save events");
            return;
        };
        let mut st = State { watcher, roots: Vec::new(), dirs: HashSet::new(), warned: false };
        let mut ex = excluded.current();
        let mut ids = ProjectIds::load(&own_dir);
        ids.forget(&ex);
        let mut pending: HashMap<PathBuf, (String, Saves)> = HashMap::new();
        let mut first: Option<Instant> = None;
        let mut last = Instant::now();
        loop {
            let msg = rx.recv_timeout(Duration::from_millis(500));
            let latest = excluded.current();
            if !Arc::ptr_eq(&latest, &ex) {
                ex = latest;
                for id in st.forget(&ex) {
                    pending.retain(|_, (pid, _)| *pid != id);
                }
                ids.forget(&ex);
            }
            match msg {
                Ok(Watch::Root(root, id)) => {
                    let root = root.canonicalize().unwrap_or(root);
                    if !ex.covers(&root) {
                        st.add_root(root, id, &ex, &mut ids, &out, &git);
                    }
                }
                Ok(Watch::Fs(ev)) => {
                    let counts = matches!(
                        ev.kind,
                        EventKind::Create(_)
                            | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_) | ModifyKind::Any)
                    );
                    if !counts {
                        continue;
                    }
                    for path in ev.paths {
                        if ignored(&path) || path.starts_with(&own_dir) || ex.covers(&path) || !path.exists() {
                            continue;
                        }
                        if path.is_dir() {
                            st.add_tree(&path, &ex);
                            continue;
                        }
                        if let Some(p) = st.project_of(&path).and_then(|(root, _)| nested_repo(&path, root)) {
                            if !ex.covers(&p.root) {
                                st.add_root(p.root, p.id, &ex, &mut ids, &out, &git);
                            }
                        }
                        if let Some((root, id)) = st.project_of(&path) {
                            pending.entry(root.clone()).or_insert_with(|| (id.clone(), Saves::default())).1.add(path);
                            first.get_or_insert_with(Instant::now);
                            last = Instant::now();
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let due = first.is_some_and(|f| last.elapsed() >= QUIET || f.elapsed() >= MAX_WAIT);
            if due {
                for (root, (id, saves)) in pending.drain() {
                    if let Some(kind) = saves.kind(&root) {
                        let _ = out.send(Msg::Event(Event::new(now(), Some(id), kind)));
                        let _ = git.send(git::Cmd::Activity(root));
                    }
                }
                first = None;
            }
        }
    });
}
