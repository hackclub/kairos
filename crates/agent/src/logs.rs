use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use kairos_core::event::{Event, Kind, MAX_PATH_LEN};
use kairos_core::exclude::Exclusions;
use kairos_core::project::{project_of, Project};
use kairos_core::sources::{self, Found};
use serde_json::{Map, Value};

use crate::exclusions::Excluded;
use crate::sessions::Sessions;
use crate::shadow::Shadow;
use crate::watch::Watch;
use crate::Msg;

const EVERY: Duration = Duration::from_secs(3);
const SAVE_EVERY: Duration = Duration::from_secs(60);
// A folder's project id changes when a remote is added or the repo renamed, so lookups expire.
const PROJECT_TTL: Duration = Duration::from_secs(60);
const MAX_CACHED: usize = 1024;

pub(crate) type Lookups = HashMap<PathBuf, (Instant, Option<Project>)>;

fn save(path: &PathBuf, text: &str) {
    let tmp = path.with_extension("json.tmp");
    if fs::write(&tmp, text).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

pub(crate) fn cached_project(cache: &mut Lookups, cwd: &PathBuf) -> Option<Project> {
    if let Some((_, p)) = cache.get(cwd).filter(|(at, _)| at.elapsed() < PROJECT_TTL) {
        return p.clone();
    }
    if cache.len() >= MAX_CACHED {
        cache.retain(|_, (at, _)| at.elapsed() < PROJECT_TTL);
        if cache.len() >= MAX_CACHED {
            cache.clear();
        }
    }
    let p = project_of(cwd);
    cache.insert(cwd.clone(), (Instant::now(), p.clone()));
    p
}

// A file path as parts, from text so a Windows path reads the same on every OS: `\` separates
// parts only in drive (`C:\`) and UNC (`\\host\`, `\\?\`) paths, `.` and `..` are resolved.
struct Abs {
    windows: bool,
    parts: Vec<String>,
}

fn drive(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

fn windows_style(s: &str) -> bool {
    drive(s) || s.starts_with("\\\\")
}

fn split(path: &str, windows: bool) -> Vec<String> {
    let path = if windows { path.replace('\\', "/") } else { path.to_string() };
    let path = path.strip_prefix("//?/").unwrap_or(&path);
    let mut parts: Vec<String> = Vec::new();
    for p in path.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| !drive(l)) {
                    parts.pop();
                }
            }
            p => parts.push(p.to_string()),
        }
    }
    parts
}

impl Abs {
    fn of(path: &str, cwd: Option<&str>) -> Option<Abs> {
        let rooted = |s: &str| s.starts_with('/') || windows_style(s);
        let full = if rooted(path) {
            path.to_string()
        } else {
            let cwd = cwd.filter(|c| rooted(c))?;
            format!("{}/{path}", cwd.trim_end_matches(['/', '\\']))
        };
        let windows = windows_style(&full);
        let parts = split(&full, windows);
        (!parts.is_empty()).then_some(Abs { windows, parts })
    }

    fn parent(&self) -> Option<PathBuf> {
        let up = &self.parts[..self.parts.len() - 1];
        let head = up.first()?;
        Some(PathBuf::from(if !self.windows {
            format!("/{}", up.join("/"))
        } else if !drive(head) {
            format!("//{}", up.join("/"))
        } else if up.len() == 1 {
            format!("{head}/")
        } else {
            up.join("/")
        }))
    }

    fn under(&self, root: &str) -> Option<String> {
        let root = Abs::of(root, None)?;
        let same = |a: &String, b: &String| if self.windows { a.eq_ignore_ascii_case(b) } else { a == b };
        let inside = root.parts.len() < self.parts.len() && root.parts.iter().zip(&self.parts).all(|(a, b)| same(a, b));
        inside.then(|| self.parts[root.parts.len()..].join("/"))
    }
}

fn locate(
    cwd: Option<&str>,
    path: &str,
    covered: impl Fn(&Path) -> bool,
    mut project: impl FnMut(&Path) -> Option<Project>,
) -> Option<(Project, String)> {
    let file = Abs::of(path, cwd)?;
    let dir = file.parent()?;
    if covered(&dir.join(file.parts.last()?)) {
        return None;
    }
    let project = project(&dir)?;
    let rel = file.under(&project.root.to_string_lossy())?;
    (rel.len() <= MAX_PATH_LEN).then_some((project, rel))
}

// What a harness fact becomes. Nothing from a session in an excluded folder, and no edit of an
// excluded file, even when the session itself is somewhere else.
fn event_of(f: Found, harness: &str, ex: &Exclusions, projects: &mut Lookups, watch: &Sender<Watch>) -> Option<Event> {
    if f.cwd.as_deref().is_some_and(|c| ex.covers(c)) {
        return None;
    }
    let (proj, kind) = match f.kind {
        Kind::AgentEdit { session, path, added, removed } => {
            let cwd = f.cwd.as_deref().and_then(Path::to_str);
            let found = locate(
                cwd,
                &path,
                |file| ex.covers(file),
                |dir| cached_project(projects, &dir.to_path_buf()).filter(|p| !ex.covers(&p.root)),
            );
            let (p, path) = found?;
            (Some(p), Kind::AgentEdit { session, path, added, removed })
        }
        kind => {
            let proj = f.cwd.as_ref().and_then(|c| cached_project(projects, c));
            if let Some(p) = &proj {
                let _ = watch.send(Watch::Root(p.root.clone(), p.id.clone()));
            }
            (proj, kind)
        }
    };
    let mut e = Event::new(f.ts, proj.map(|p| p.id), kind);
    e.harness = Some(harness.to_string());
    Some(e)
}

/// The config folder a Claude Code turn's hooks run with, when it isn't the one kairosd's own wakatime-cli would read.
fn claude_dir(harness: &str, kind: &Kind) -> Option<PathBuf> {
    (harness == "claude-code").then(|| kind.session().and_then(sources::claude::other_config_dir)).flatten()
}

/// Also notes the folder each session runs in, and sets off the shadow when an agent finishes a turn, the moment an agent's
/// WakaTime hook runs wakatime-cli.
pub fn spawn(tx: Sender<Msg>, watch: Sender<Watch>, state_path: PathBuf, excluded: Arc<Excluded>, sessions: Arc<Sessions>, shadow: Arc<Shadow>) {
    thread::spawn(move || {
        let mut srcs = sources::all();
        let saved: Value = fs::read_to_string(&state_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        for s in &mut srcs {
            if let Some(v) = saved.get(s.name()) {
                s.restore(v);
            }
        }
        let mut projects = Lookups::new();
        let mut saved_text = String::new();
        let mut saved_at = Instant::now() - SAVE_EVERY;
        let mut sessions_saved = Instant::now();
        loop {
            let mut state = Map::new();
            let mut reported = false;
            for s in &mut srcs {
                let found = s.poll();
                reported |= !found.is_empty();
                // Read after the poll: anything it found was written before this, so a folder
                // excluded in the meantime is already on the list.
                let ex = excluded.current();
                for f in found {
                    if let (Some(session), Some(cwd)) = (f.kind.session(), f.cwd.as_deref()) {
                        sessions.note(session, cwd);
                    }
                    let turn_ended = matches!(f.kind, Kind::ReplyDone { .. }).then(|| claude_dir(s.harness(), &f.kind));
                    if let Some(e) = event_of(f, s.harness(), &ex, &mut projects, &watch) {
                        let _ = tx.send(Msg::Event(e));
                    }
                    if let Some(claude) = turn_ended {
                        shadow.nudge(claude);
                    }
                }
                state.insert(s.name().to_string(), s.state());
            }
            let text = Value::Object(state).to_string();
            // What was just reported is saved as read right away, or a restart within the minute would report it again.
            if text != saved_text && (reported || saved_at.elapsed() >= SAVE_EVERY) {
                save(&state_path, &text);
                saved_text = text;
                saved_at = Instant::now();
            }
            if sessions_saved.elapsed() >= SAVE_EVERY {
                sessions.save();
                sessions_saved = Instant::now();
            }
            thread::sleep(EVERY);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: &str, root: &str) -> Project {
        Project { id: id.into(), root: PathBuf::from(root) }
    }

    fn lookup(dir: &Path) -> Option<Project> {
        let d = dir.to_string_lossy().replace('\\', "/");
        let roots = [("p", "/w/p"), ("win", "C:\\Users\\u\\proj"), ("unc", "\\\\host\\share\\repo")];
        roots.iter().find_map(|(id, root)| {
            let r = root.replace('\\', "/");
            (d == r || d.starts_with(&format!("{r}/")) || d.eq_ignore_ascii_case(&r) || d.to_ascii_lowercase().starts_with(&format!("{}/", r.to_ascii_lowercase())))
                .then(|| project(id, root))
        })
    }

    fn at(cwd: Option<&str>, path: &str) -> Option<(String, String)> {
        locate(cwd, path, |_| false, lookup).map(|(p, rel)| (p.id, rel))
    }

    #[test]
    fn an_edit_belongs_to_the_project_of_the_file_with_a_path_from_its_root() {
        assert_eq!(at(Some("/w/p"), "/w/p/src/a.rs"), Some(("p".into(), "src/a.rs".into())));
        assert_eq!(at(Some("/w/p/src"), "../b/c.rs"), Some(("p".into(), "b/c.rs".into())));
        assert_eq!(at(Some("/w/p"), "./x/./y/../z.rs"), Some(("p".into(), "x/z.rs".into())));
        assert_eq!(at(Some("/w/p"), "top.rs"), Some(("p".into(), "top.rs".into())));
        assert_eq!(at(None, "/w/p/a.rs"), Some(("p".into(), "a.rs".into())));
        assert_eq!(at(Some("/w/elsewhere"), "/w/p/a.rs"), Some(("p".into(), "a.rs".into())));
    }

    #[test]
    fn files_outside_any_project_or_without_a_folder_are_dropped() {
        assert_eq!(at(Some("/w/p"), "/tmp/scratch.rs"), None);
        assert_eq!(at(Some("/w/p"), "../outside.rs"), None);
        assert_eq!(at(None, "relative.rs"), None);
        assert_eq!(at(Some("relative/dir"), "a.rs"), None);
        assert_eq!(at(Some("/w/p"), "/"), None);
        let long = format!("/w/p/{}.rs", "d/".repeat(MAX_PATH_LEN));
        assert_eq!(at(Some("/w/p"), &long), None);
    }

    #[test]
    fn windows_paths_work_on_every_os() {
        let expected = Some(("win".to_string(), "src/a.rs".to_string()));
        assert_eq!(at(Some("C:\\Users\\u\\proj"), "C:\\Users\\u\\proj\\src\\a.rs"), expected);
        assert_eq!(at(Some("C:\\Users\\u\\proj"), "src\\a.rs"), expected);
        assert_eq!(at(Some("C:\\Users\\u\\proj"), "src/a.rs"), expected);
        assert_eq!(at(Some("C:\\Users\\u\\proj\\src"), "..\\src\\a.rs"), expected);
        assert_eq!(at(None, "c:/users/U/Proj/src/a.rs"), expected);
        assert_eq!(at(None, "\\\\?\\C:\\Users\\u\\proj\\src\\a.rs"), expected);
        assert_eq!(at(None, "\\\\host\\share\\repo\\src\\a.rs"), Some(("unc".into(), "src/a.rs".into())));
        assert_eq!(at(None, "D:\\other\\a.rs"), None);
        assert_eq!(at(Some("C:\\Users\\u\\proj"), "..\\..\\..\\..\\..\\a.rs"), None);
    }

    #[test]
    fn unix_paths_keep_backslashes_and_case() {
        assert_eq!(at(Some("/w/p"), "/w/p/we\\ird.rs"), Some(("p".into(), "we\\ird.rs".into())));
        assert_eq!(at(None, "/W/P/a.rs"), None);
    }

    #[test]
    fn parents_are_the_folder_a_project_is_looked_up_from() {
        let parent = |p: &str| Abs::of(p, None).and_then(|a| a.parent());
        assert_eq!(parent("/w/p/a.rs"), Some(PathBuf::from("/w/p")));
        assert_eq!(parent("C:\\a.rs"), Some(PathBuf::from("C:/")));
        assert_eq!(parent("C:\\x\\a.rs"), Some(PathBuf::from("C:/x")));
        assert_eq!(parent("\\\\host\\share\\a.rs"), Some(PathBuf::from("//host/share")));
        assert_eq!(parent("/a.rs"), None);
    }

    fn repos(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("kairos-logs-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for name in ["a", "ab"] {
            fs::create_dir_all(base.join("w").join(name).join(".git")).unwrap();
        }
        let base = base.canonicalize().unwrap();
        (base.clone(), base.join("w").join("a"), base.join("w").join("ab"))
    }

    #[test]
    fn a_session_in_an_excluded_folder_records_nothing() {
        let (base, a, ab) = repos("excluded");
        kairos_core::exclude::add(&base, &a).unwrap();
        let ex = kairos_core::exclude::load(&base).unwrap();
        let (watch, roots) = std::sync::mpsc::channel();
        let mut cache = Lookups::new();
        let session = || "s1".to_string();
        let kinds = |file: &Path| {
            vec![
                Kind::Prompt { words: 12, pasted: false, model: None, session: session(), shape: None, version: None, via: None },
                Kind::AgentStart { session: session() },
                Kind::ReplyDone { session: session(), reply_words: 40, tools: 2, files_changed: 1, model: None },
                Kind::AgentEdit { session: session(), path: file.to_string_lossy().into_owned(), added: 3, removed: 1 },
                Kind::AgentCommit { session: session(), sha: Some("abc1234".into()) },
            ]
        };
        let mut run = |cwd: &Path, file: &Path| {
            let found = |kind| Found { ts: 1.0, cwd: Some(cwd.to_path_buf()), kind };
            kinds(file).into_iter().filter_map(|k| event_of(found(k), "claude-code", &ex, &mut cache, &watch)).collect::<Vec<_>>()
        };

        assert!(run(&a, &a.join("src/x.rs")).is_empty(), "every kind of event from the session is dropped");
        assert!(run(&a.join("sub"), &a.join("sub/x.rs")).is_empty(), "also from a folder inside it");
        assert!(roots.try_recv().is_err(), "and its project is never passed on to be watched");

        let sibling = run(&ab, &ab.join("src/x.rs"));
        assert_eq!(sibling.len(), 5);
        assert!(sibling.iter().all(|e| e.project.as_deref() == Some("ab") && e.harness.as_deref() == Some("claude-code")));
        assert!(matches!(&sibling[3].kind, Kind::AgentEdit { path, .. } if path == "src/x.rs"));
        assert!(roots.try_recv().is_ok());

        let elsewhere = run(&ab, &a.join("src/x.rs"));
        assert_eq!(elsewhere.len(), 4, "an excluded file is not recorded even when the session is somewhere else");
        assert!(elsewhere.iter().all(|e| !matches!(e.kind, Kind::AgentEdit { .. })));
        let inward = run(&a, &ab.join("src/x.rs"));
        assert!(inward.is_empty());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_single_excluded_file_is_not_an_edit() {
        let (base, _, ab) = repos("file");
        let secret = ab.join("secret.txt");
        fs::write(&secret, "x").unwrap();
        kairos_core::exclude::add(&base, &secret).unwrap();
        let ex = kairos_core::exclude::load(&base).unwrap();
        let (watch, _roots) = std::sync::mpsc::channel();
        let mut cache = Lookups::new();
        let mut edit = |file: &str| {
            let kind = Kind::AgentEdit { session: "s".into(), path: ab.join(file).to_string_lossy().into_owned(), added: 1, removed: 0 };
            event_of(Found { ts: 1.0, cwd: Some(ab.clone()), kind }, "codex", &ex, &mut cache, &watch)
        };
        assert!(edit("secret.txt").is_none());
        assert!(edit("secret.txt.bak").is_some());
        let _ = fs::remove_dir_all(&base);
    }
}
