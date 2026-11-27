use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use kairos_core::event::{App, Class};
use kairos_core::exclude::Exclusions;
use kairos_core::project::{own_folder, project_of, Project};
use kairos_core::sources::{opencode, t3code};

const TERMINALS: &[&str] = &[
    "kitty", "alacritty", "foot", "footclient", "wezterm", "wezterm-gui", "gnome-terminal",
    "gnome-terminal-server", "konsole", "xterm", "urxvt", "st", "tilix", "terminator", "ghostty",
    "xfce4-terminal", "lxterminal", "sakura", "rio", "contour", "com.mitchellh.ghostty",
    "org.wezfurlong.wezterm", "windowsterminal", "openconsole", "conhost", "mintty", "tabby", "hyper",
    "warp", "iterm2", "com.apple.terminal", "com.googlecode.iterm2", "dev.warp.warp", "net.kovidgoyal.kitty",
    "org.alacritty", "com.github.wez.wezterm", "co.zeit.hyper", "ptyxis", "org.gnome.ptyxis", "kgx",
    "org.gnome.console", "cosmic-term", "com.system76.cosmicterm", "mate-terminal", "qterminal", "guake", "yakuake",
];
const CONSOLE_HOSTS: &[&str] = &["conhost", "openconsole"];
pub const CONSOLE_WINDOW: &str = "ConsoleWindowClass";
const SHELLS: &[&str] = &[
    "bash", "zsh", "fish", "sh", "dash", "nu", "xonsh", "tcsh", "ksh", "elvish", "pwsh", "powershell", "cmd",
    "wsl",
];
const BROWSERS: &[&str] = &[
    "firefox", "chromium", "chrome", "google-chrome", "brave", "zen", "librewolf", "vivaldi",
    "msedge", "microsoft-edge", "opera", "arc", "safari",
];
const AGENT_APPS: &[&str] = &[
    "antigravity", "com.anthropic.claude", "claude-desktop", "com.t3tools.t3code", "t3code", "chatgpt",
    "com.anthropic.claudefordesktop", "com.openai.chat", "hermes", "codex", "com.openai.codex", "t3-code",
    // OpenCode's desktop app: `ai.opencode` on Linux (window class `ai.opencode.desktop`, the Mac bundle id too),
    // OpenCode.exe on Windows. Its CLI never owns a window, the terminal does.
    "opencode", "ai.opencode",
    // On a Mac the Antigravity IDE's program is `Electron`, so only its bundle id `com.google.antigravity-ide` names it.
    "com.google.antigravity",
];
const AGENT_CLIS: &[&str] = &["claude", "codex", "opencode", "agy", "gemini", "qwen", "kilo", "copilot", "pi", "hermes", "amp"];
const EDITORS: &[&str] = &[
    "code", "code-oss", "vscode", "codium", "vscodium", "cursor", "windsurf", "zed", "zeditor",
    "zed-editor", "dev.zed.zed", "nvim", "neovide", "vim", "gvim", "nano", "helix", "hx", "emacs",
    "kak", "micro", "lapce", "kate", "gedit", "sublime_text", "idea", "pycharm", "webstorm", "clion",
    "goland", "rustrover", "phpstorm", "rider", "datagrip", "godot", "godot4", "idea64", "pycharm64",
    "webstorm64", "rustrover64", "clion64", "goland64", "phpstorm64", "rider64", "datagrip64",
    "notepad++", "devenv", "xcode", "com.apple.dt.xcode", "com.microsoft.vscode", "com.microsoft.vscodeinsiders",
    "com.vscodium", "com.exafunction.windsurf", "com.google.android.studio",
];

/// Lowercase, without `.exe`, spaces as dashes the way the name lists write them (`T3 Code (Beta).exe`).
pub fn normalize(name: &str) -> String {
    let lower = name.to_lowercase().replace(' ', "-");
    match lower.strip_suffix(".exe") {
        Some(stem) => stem.to_string(),
        None => lower,
    }
}

pub(crate) fn is_one_of(set: &[&str], name: &str) -> bool {
    !name.is_empty()
        && set.iter().any(|b| {
            name == *b
                || name
                    .strip_prefix(b)
                    .is_some_and(|rest| rest.starts_with(['-', '.', '_']))
        })
}

pub fn is_console_host(name: &str) -> bool {
    is_one_of(CONSOLE_HOSTS, name)
}

pub fn is_terminal(name: &str, wclass: &str) -> bool {
    is_one_of(TERMINALS, name) || is_one_of(TERMINALS, &wclass.to_lowercase())
}

pub fn is_browser(name: &str, wclass: &str) -> bool {
    is_one_of(BROWSERS, name) || is_one_of(BROWSERS, wclass)
}

const T3_CODE: &[&str] = &["com.t3tools.t3code", "t3code", "t3-code"];
const T3_PROJECTS_FOR: Duration = Duration::from_secs(20);

// T3 Code shows every project in one window and starts no process in a project until a thread runs. Its title names the
// open thread's project and its own store says which folder that is.
fn t3_projects() -> Vec<(String, PathBuf)> {
    static READ: Mutex<Option<(Instant, Vec<(String, PathBuf)>)>> = Mutex::new(None);
    let mut read = READ.lock().unwrap_or_else(|e| e.into_inner());
    match &*read {
        Some((at, projects)) if at.elapsed() < T3_PROJECTS_FOR => projects.clone(),
        _ => {
            let projects = t3code::projects(&t3code::default_dbs());
            *read = Some((Instant::now(), projects.clone()));
            projects
        }
    }
}

const OPENCODE_DESKTOP: &[&str] = &["opencode", "ai.opencode"];

pub fn is_opencode_desktop(name: &str, wclass: &str) -> bool {
    is_one_of(OPENCODE_DESKTOP, name) || is_one_of(OPENCODE_DESKTOP, &wclass.to_lowercase())
}

const CLAUDE_DESKTOP: &[&str] = &["claude-desktop", "com.anthropic.claude", "com.anthropic.claudefordesktop"];
// On Windows the app is ChatGPT.exe in the package OpenAI.Codex.
const CODEX_APP: &[&str] = &["codex", "com.openai.codex", "openai.codex"];

/// Which agent app an `agent_app` window belongs to, for the ones whose logs say a prompt was typed in them.
pub fn agent_app(name: &str, wclass: &str) -> Option<App> {
    let wclass = wclass.to_lowercase();
    if is_one_of(CLAUDE_DESKTOP, name) || is_one_of(CLAUDE_DESKTOP, &wclass) || name == "claude" || wclass == "claude" {
        Some(App::ClaudeDesktop)
    } else if is_one_of(CODEX_APP, name) || is_one_of(CODEX_APP, &wclass) {
        Some(App::CodexApp)
    } else if is_opencode_desktop(name, &wclass) {
        Some(App::OpencodeDesktop)
    } else {
        None
    }
}

fn is_agent_app(name: &str, wclass: &str) -> bool {
    is_one_of(AGENT_APPS, name) || is_one_of(AGENT_APPS, wclass) || wclass == "claude"
}

pub fn is_agent_title(title: &str) -> bool {
    let lower = title.to_lowercase();
    // Pi titles its terminal "π - <folder>"; "pi" and "amp" are inside too many words to look for.
    title.starts_with(|c: char| c == '✳' || ('\u{2800}'..='\u{28ff}').contains(&c))
        || title.starts_with("π - ")
        || AGENT_CLIS.iter().filter(|n| !matches!(**n, "pi" | "amp")).any(|n| lower.contains(n))
}

/// An editor by its executable or window class alone (a JetBrains IDE started through java needs its command line too).
pub fn is_editor_app(name: &str, wclass: &str) -> bool {
    is_one_of(EDITORS, name) || is_one_of(EDITORS, wclass) || wclass.starts_with("jetbrains-")
}

/// Whether a window's title can change how it's classified: a browser tab titled as an agent session, or the folder an
/// editor or agent app window shows (one process can show several, a window each).
pub fn titles_matter(name: &str, wclass: &str) -> bool {
    is_browser(name, wclass) || is_editor_app(name, wclass) || is_agent_app(name, wclass)
}

fn is_editor<P: Procs>(p: &P, name: &str, wclass: &str, pid: u32) -> bool {
    is_editor_app(name, wclass)
        || (name == "java"
            && p.cmdline(pid).iter().any(|a| {
                let a = a.to_lowercase();
                a.contains("jetbrains") || a.contains("intellij")
            }))
}

/// Terminals and editors are classed by the processes they run, which can change while the window stays in front.
pub fn runs_children(name: &str, wclass: &str) -> bool {
    let lower = wclass.to_lowercase();
    wclass == CONSOLE_WINDOW
        || [name, lower.as_str()].iter().any(|n| is_one_of(TERMINALS, n) || is_one_of(EDITORS, n))
        || lower.starts_with("jetbrains-")
}

#[derive(Default)]
pub struct Tree {
    ppid: HashMap<u32, u32>,
    children: HashMap<u32, Vec<u32>>,
}

impl Tree {
    #[cfg(windows)]
    pub fn clear(&mut self) {
        self.ppid.clear();
        self.children.clear();
    }

    pub fn insert(&mut self, pid: u32, parent: u32) {
        self.ppid.insert(pid, parent);
        self.children.entry(parent).or_default().push(pid);
    }

    pub fn show(&mut self, owner: u32, pids: &[u32]) {
        self.children.insert(owner, pids.iter().copied().filter(|p| *p != owner).collect());
    }

    #[cfg(unix)]
    pub fn pids(&self) -> impl Iterator<Item = u32> + '_ {
        self.ppid.keys().copied()
    }

    pub fn parent(&self, pid: u32) -> Option<u32> {
        self.ppid.get(&pid).copied()
    }

    pub fn descendants(&self, pid: u32) -> Vec<(u32, usize)> {
        let mut out = Vec::new();
        let mut seen = HashSet::from([pid]);
        let mut frontier = vec![pid];
        let mut depth = 0;
        while !frontier.is_empty() && depth < 32 {
            depth += 1;
            let mut next = Vec::new();
            for p in &frontier {
                for &c in self.children.get(p).into_iter().flatten() {
                    if self.ppid.contains_key(&c) && seen.insert(c) {
                        out.push((c, depth));
                        next.push(c);
                    }
                }
            }
            frontier = next;
        }
        out
    }
}

pub trait Procs {
    const TITLE_HINTS: bool;

    fn tree(&self) -> &Tree;
    fn name(&self, pid: u32) -> String;
    fn comm(&self, pid: u32) -> String {
        self.name(pid)
    }
    fn exe(&self, pid: u32) -> Option<PathBuf>;
    fn cwd(&self, pid: u32) -> Option<PathBuf>;
    fn cmdline(&self, pid: u32) -> Vec<String>;
    /// The terminal device the process runs on, where the system can tell.
    #[cfg(unix)]
    fn tty(&self, _pid: u32) -> Option<u64> {
        None
    }
}

fn is_agent_cli<P: Procs>(p: &P, pid: u32) -> bool {
    let (name, comm) = (p.name(pid), p.comm(pid));
    if name.starts_with("python") || name == "hermes" || comm == "hermes" {
        let args = p.cmdline(pid);
        // The Hermes CLI starts its gateway as its own child, running in the Hermes install folder.
        if args.windows(2).any(|w| w[0] == "gateway" && w[1] == "run") {
            return false;
        }
        // Hermes's launcher runs its own Python with an inline script that imports hermes_cli.
        if name.starts_with("python") && args.iter().take(4).any(|a| a.contains("hermes_cli")) {
            return true;
        }
    }
    // Amp runs each plugin as its own child, in the folder the plugin is in.
    if (name == "amp" || comm == "amp") && p.cmdline(pid).iter().take(3).any(|a| a.ends_with("plugin-runtime.ts")) {
        return false;
    }
    // OpenCode 2 starts its background server as its own child, running in the home folder.
    if (name == "opencode" || comm == "opencode") && p.cmdline(pid).iter().skip(1).take(3).any(|a| a == "--service") {
        return false;
    }
    if AGENT_CLIS.contains(&comm.as_str()) || AGENT_CLIS.contains(&name.as_str()) {
        return true;
    }
    matches!(name.as_str(), "node" | "bun")
        && p.cmdline(pid).iter().take(3).any(|a| {
            let a = a.to_lowercase().replace('\\', "/");
            a.contains("claude-code")
                || a.contains("/@openai/codex")
                || a.contains("/opencode-ai/")
                || a.contains("/@earendil-works/pi-coding-agent/")
                || a.ends_with("/ui-tui/dist/entry.js")
                || a.ends_with("/hermes_cli/tui_dist/entry.js")
                || Path::new(&a)
                    .file_stem()
                    .is_some_and(|n| AGENT_CLIS.iter().any(|c| n == *c))
        })
}

/// Looks projects up for one classification. Meeting an excluded folder leaves no project behind
/// and makes the whole window `other`, the way a private browser window is recorded.
struct Guard<'a> {
    ex: &'a Exclusions,
    hit: Cell<bool>,
}

impl Guard<'_> {
    fn project(&self, path: &Path) -> Option<Project> {
        if self.ex.covers(path) {
            self.hit.set(true);
            return None;
        }
        let project = project_of(path)?;
        if self.ex.covers(&project.root) {
            self.hit.set(true);
            return None;
        }
        Some(project)
    }
}

fn editor_project<P: Procs>(p: &P, pid: u32, g: &Guard) -> Option<Project> {
    let base = p.cwd(pid);
    for arg in p.cmdline(pid).iter().skip(1).filter(|a| !a.starts_with('-')) {
        let path = match &base {
            Some(b) => b.join(arg),
            None => PathBuf::from(arg),
        };
        if path.exists() {
            if let Some(proj) = g.project(&path) {
                return Some(proj);
            }
        }
    }
    base.as_deref()
        .and_then(|b| g.project(b))
        .or_else(|| p.tree().descendants(pid).into_iter().find_map(|(d, _)| p.cwd(d).and_then(|c| g.project(&c))))
}

// One editor process can show several folders, a window each, and the process only says where it started. A window's title
// names its folder, so of the folders the editor, its arguments and the processes it started (terminals, language servers)
// are in, the project the title names, if exactly one, is the window's. A title naming an excluded folder makes the
// window excluded, even with nothing running in that folder.
fn titled_project<P: Procs>(p: &P, pid: u32, title: &str, g: &Guard) -> Option<Project> {
    let names = title_names(title);
    let named = |path: &Path| path.file_name().is_some_and(|n| names.iter().any(|t| t.eq_ignore_ascii_case(&n.to_string_lossy())));
    if g.ex.roots.iter().any(|r| named(r)) {
        g.hit.set(true);
        return None;
    }
    let base = p.cwd(pid);
    let args = p
        .cmdline(pid)
        .into_iter()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .map(|a| match &base {
            Some(b) => b.join(a),
            None => PathBuf::from(a),
        })
        .filter(|d| d.exists());
    let dirs = args.chain(base.clone()).chain(p.tree().descendants(pid).into_iter().filter_map(|(d, _)| p.cwd(d)));
    let mut roots: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let Some(proj) = project_of(&dir) else { continue };
        if (named(&dir) || named(&proj.root)) && !roots.contains(&proj.root) {
            roots.push(proj.root);
        }
    }
    match roots.as_slice() {
        [one] => g.project(one),
        _ => None,
    }
}

/// Names a window title may give its folder: each part between the separators editors use ("● main.rs - kairos - Visual
/// Studio Code", "kairos – main.rs"), and of a part ending in brackets ("app (Workspace)", "kairos [~/src/kairos]") what
/// comes before them, what is in them and that path's last folder.
fn title_names(title: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |s: &str| {
        let s = s.trim();
        if !s.is_empty() && !out.iter().any(|o| o == s) {
            out.push(s.to_string());
        }
    };
    for part in title.replace(" \u{2014} ", " - ").replace(" \u{2013} ", " - ").split(" - ") {
        let part = part.trim().trim_start_matches(['\u{25cf}', '\u{2022}']).trim();
        add(part);
        let open = match part.chars().last() {
            Some(')') => '(',
            Some(']') => '[',
            _ => continue,
        };
        if let Some(i) = part.rfind(open) {
            let inner = &part[i + 1..part.len() - 1];
            add(&part[..i]);
            add(inner);
            if let Some(last) = Path::new(inner).file_name() {
                add(&last.to_string_lossy());
            }
        }
    }
    out
}

fn process_project<P: Procs>(p: &P, pid: u32, g: &Guard) -> Option<Project> {
    p.cwd(pid)
        .and_then(|c| g.project(&c))
        .or_else(|| p.exe(pid).and_then(|e| e.parent().and_then(|d| g.project(d))).filter(|proj| own_folder(&proj.root)))
}

// A console window is drawn by conhost, a child of the console's first client, and Windows names
// that client (or conhost) as the window's owner: the client and everything under it is the session.
fn terminal_tree<P: Procs>(p: &P, pid: u32, console: bool) -> Vec<(u32, usize)> {
    let client = if is_console_host(&p.name(pid)) { p.tree().parent(pid) } else { console.then_some(pid) };
    let Some(client) = client else {
        return p.tree().descendants(pid);
    };
    let mut all = vec![(client, 1)];
    all.extend(p.tree().descendants(client).into_iter().map(|(c, d)| (c, d + 1)));
    all
}

fn classify_terminal<P: Procs>(p: &P, pid: u32, console: bool, titled_agent: bool, g: &Guard) -> (Class, Option<Project>) {
    let mut desc = terminal_tree(p, pid, console);
    desc.retain(|(c, _)| !is_console_host(&p.name(*c)));
    let agent = desc.iter().filter(|(c, _)| is_agent_cli(p, *c)).max_by_key(|(c, d)| (*d, *c));
    if let Some(&(a, _)) = agent {
        return (Class::AgentTerminal, p.cwd(a).and_then(|c| g.project(&c)));
    }
    let deepest = desc.iter().max_by_key(|(c, d)| (*d, *c)).map(|&(c, _)| c);
    if titled_agent {
        return (Class::AgentTerminal, deepest.and_then(|d| p.cwd(d)).and_then(|c| g.project(&c)));
    }
    let Some(deep) = deepest else {
        return (Class::Other, None);
    };
    let name = p.name(deep);
    if is_one_of(SHELLS, &name) || is_one_of(SHELLS, &p.comm(deep)) {
        return match p.cwd(deep).and_then(|c| g.project(&c)) {
            Some(proj) => (Class::Terminal, Some(proj)),
            None => (Class::Other, None),
        };
    }
    if is_editor(p, &name, "", deep) {
        return (Class::Editor, editor_project(p, deep, g));
    }
    match process_project(p, deep, g) {
        Some(proj) => (Class::Testing, Some(proj)),
        None => (Class::Other, None),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Classified {
    pub class: Class,
    pub project: Option<Project>,
    pub app: String,
}

pub fn classify_with<P: Procs>(p: &P, pid: u32, wclass: &str, title: &str, ex: &Exclusions) -> Classified {
    let app = p.name(pid);
    let g = Guard { ex, hit: Cell::new(false) };
    let (class, project) = classify_parts(p, pid, &app, wclass, title, &g);
    if g.hit.get() {
        return Classified { class: Class::Other, project: None, app };
    }
    Classified { class, project, app }
}

fn classify_parts<P: Procs>(p: &P, pid: u32, name: &str, wclass: &str, title: &str, g: &Guard) -> (Class, Option<Project>) {
    let console = wclass == CONSOLE_WINDOW;
    let wclass = wclass.to_lowercase();
    if console || is_terminal(name, &wclass) {
        return classify_terminal(p, pid, console, P::TITLE_HINTS && is_agent_title(title), g);
    }
    if is_browser(name, &wclass) {
        return (Class::BrowserUnknown, None);
    }
    if is_agent_app(name, &wclass) || name == "claude" {
        if is_one_of(T3_CODE, name) || is_one_of(T3_CODE, &wclass) {
            if let Some(folder) = t3code::titled_folder(title, &t3_projects()) {
                return (Class::AgentApp, g.project(folder));
            }
        }
        if is_opencode_desktop(name, &wclass) {
            return (Class::AgentApp, opencode::desktop_folder(&opencode::desktop_dirs()).and_then(|f| g.project(&f)));
        }
        return (Class::AgentApp, titled_project(p, pid, title, g).or_else(|| editor_project(p, pid, g)));
    }
    if is_editor(p, name, &wclass, pid) {
        return (Class::Editor, titled_project(p, pid, title, g).or_else(|| editor_project(p, pid, g)));
    }
    match process_project(p, pid, g) {
        Some(proj) => (Class::Testing, Some(proj)),
        None => (Class::Other, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        tree: Tree,
        procs: HashMap<u32, (&'static str, Option<PathBuf>, Vec<String>)>,
        exes: HashMap<u32, PathBuf>,
    }

    impl Fake {
        fn new(list: &[(u32, u32, &'static str)]) -> Fake {
            let mut f = Fake { tree: Tree::default(), procs: HashMap::new(), exes: HashMap::new() };
            for &(pid, ppid, name) in list {
                f.tree.insert(pid, ppid);
                f.procs.insert(pid, (name, None, Vec::new()));
            }
            f
        }

        fn cwd(mut self, pid: u32, dir: &Path) -> Fake {
            self.procs.get_mut(&pid).unwrap().1 = Some(dir.to_path_buf());
            self
        }

        fn exe(mut self, pid: u32, file: &Path) -> Fake {
            self.exes.insert(pid, file.to_path_buf());
            self
        }

        fn args(mut self, pid: u32, args: &[&str]) -> Fake {
            self.procs.get_mut(&pid).unwrap().2 = args.iter().map(|a| a.to_string()).collect();
            self
        }
    }

    impl Procs for Fake {
        const TITLE_HINTS: bool = true;

        fn tree(&self) -> &Tree {
            &self.tree
        }

        fn name(&self, pid: u32) -> String {
            self.procs.get(&pid).map(|p| normalize(p.0)).unwrap_or_default()
        }

        fn exe(&self, pid: u32) -> Option<PathBuf> {
            self.exes.get(&pid).cloned()
        }

        fn cwd(&self, pid: u32) -> Option<PathBuf> {
            self.procs.get(&pid).and_then(|p| p.1.clone())
        }

        fn cmdline(&self, pid: u32) -> Vec<String> {
            self.procs.get(&pid).map(|p| p.2.clone()).unwrap_or_default()
        }
    }

    fn classify_with<P: Procs>(p: &P, pid: u32, wclass: &str, title: &str) -> Classified {
        super::classify_with(p, pid, wclass, title, &Exclusions::default())
    }

    fn class_of(f: &Fake, pid: u32, title: &str) -> Class {
        classify_with(f, pid, "", title).class
    }

    fn repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kairos-classify-{tag}-{}", std::process::id())).join("proj");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        dir
    }

    #[test]
    fn agent_apps_are_matched_by_exe_or_window_class() {
        assert!(is_agent_app("antigravity", ""));
        assert!(is_agent_app("electron", "com.anthropic.claude"));
        assert!(is_agent_app("", "claude"));
        assert!(is_agent_app("claude-desktop", ""));
        assert!(is_agent_app("t3code", ""));
        assert!(is_agent_app("electron", "com.t3tools.t3code"));
        assert!(is_agent_app(&normalize("T3 Code (Beta).exe"), ""));
        assert!(is_agent_app(&normalize("T3 Code.exe"), ""));
        assert!(is_agent_app(&normalize("Codex.exe"), ""));
        assert!(is_agent_app("codex", "com.openai.codex"));
        assert!(is_agent_app("ai.opencode", "ai.opencode.desktop"));
        assert!(is_agent_app("electron", "ai.opencode.desktop"));
        assert!(is_agent_app("electron", "com.google.antigravity-ide"));
        assert!(is_agent_app(&normalize("Antigravity IDE.exe"), ""));
        assert!(is_agent_app(&normalize("OpenCode.exe"), ""));
        assert!(!is_agent_app("claude", "kitty"));
        assert!(!is_agent_app("code", "code"));
    }

    #[test]
    fn terminals_and_editors_are_rechecked_often() {
        assert!(runs_children("windowsterminal", "CASCADIA_HOSTING_WINDOW_CLASS"));
        assert!(runs_children("powershell", CONSOLE_WINDOW));
        assert!(runs_children("", "com.googlecode.iterm2"));
        assert!(runs_children("code", "Chrome_WidgetWin_1"));
        assert!(runs_children("java", "jetbrains-idea"));
        assert!(!runs_children("firefox", "MozillaWindowClass"));
        assert!(!runs_children("robot-app", "WindowsForms10.Window.8.app.0.1"));
    }

    #[test]
    fn names_are_lowercased_and_lose_exe() {
        assert_eq!(normalize("WindowsTerminal.exe"), "windowsterminal");
        assert_eq!(normalize("Code.EXE"), "code");
        assert_eq!(normalize("notepad++.exe"), "notepad++");
        assert_eq!(normalize("kitty"), "kitty");
    }

    #[test]
    fn windows_terminal_running_claude_is_agent_terminal() {
        let dir = repo("agent");
        let f = Fake::new(&[
            (10, 1, "WindowsTerminal.exe"),
            (11, 10, "OpenConsole.exe"),
            (12, 11, "pwsh.exe"),
            (13, 12, "claude.exe"),
            (14, 13, "conhost.exe"),
        ])
        .cwd(12, Path::new("C:\\nowhere"))
        .cwd(13, &dir);
        let Classified { class, project, .. } = classify_with(&f, 10, "", "pwsh");
        assert_eq!(class, Class::AgentTerminal);
        assert_eq!(project.unwrap().id, "proj");
    }

    #[test]
    fn every_agent_cli_with_a_reader_counts() {
        for cli in ["claude", "codex.exe", "opencode", "agy", "agy.exe", "gemini", "qwen", "kilo", "copilot", "pi", "pi.exe", "hermes.exe", "amp", "amp.exe"] {
            let f = Fake::new(&[(10, 1, "kitty"), (12, 10, "bash"), (13, 12, cli)]);
            assert_eq!(class_of(&f, 10, ""), Class::AgentTerminal, "{cli}");
        }
    }

    #[test]
    fn hermes_runs_as_its_own_python() {
        let script = "import os, re, sys\nsys.path.insert(0, '/home/a/.hermes/hermes-agent')\nfrom hermes_cli.main import main\nsys.exit(main())";
        let f = Fake::new(&[(10, 1, "kitty"), (12, 10, "bash"), (13, 12, "python3")])
            .args(13, &["/home/a/.hermes/tools/hermes-agent/bin/python3", "-I", "-c", script, "chat"]);
        assert_eq!(class_of(&f, 10, ""), Class::AgentTerminal);
        let f = Fake::new(&[(10, 1, "kitty"), (12, 10, "bash"), (13, 12, "python3")]).args(13, &["python3", "manage.py", "runserver"]);
        assert_ne!(class_of(&f, 10, ""), Class::AgentTerminal);
        assert!(is_agent_app("hermes", "") && is_agent_app("hermes-canary", ""));
    }

    #[test]
    fn hermes_gateway_under_the_cli_does_not_move_the_project() {
        let dir = repo("hermes");
        let install = repo("hermes-install");
        let f = Fake::new(&[(10, 1, "kitty"), (12, 10, "bash"), (13, 12, "hermes"), (14, 13, "python3.14")])
            .cwd(13, &dir)
            .args(13, &["/home/a/.hermes/tools/python-3.14/bin/python3", "-I", "-c", "from hermes_cli.main import main"])
            .cwd(14, &install)
            .args(14, &["/home/a/.hermes/tools/python-3.14/bin/python3", "-m", "hermes_cli.main", "--profile", "default", "gateway", "run", "--quiet", "--idle-exit"]);
        let Classified { class, project, .. } = classify_with(&f, 10, "", "");
        assert_eq!(class, Class::AgentTerminal);
        assert_eq!(project.unwrap().root, dir);
    }

    #[test]
    fn an_amp_plugin_under_the_cli_does_not_move_the_project() {
        let dir = repo("amp");
        let plugins = repo("amp-plugins");
        let f = Fake::new(&[(10, 1, "kitty"), (12, 10, "bash"), (13, 12, "amp"), (14, 13, "amp")])
            .cwd(13, &dir)
            .args(13, &["amp", "-m", "low"])
            .cwd(14, &plugins)
            .args(14, &["/home/a/.amp/bin/amp", "run", "/tmp/amp-plugin-runtime-2oheEi/plugin-runtime.ts", "/home/a/.config/amp/plugins/kairos.ts"]);
        let Classified { class, project, .. } = classify_with(&f, 10, "", "");
        assert_eq!(class, Class::AgentTerminal);
        assert_eq!(project.unwrap().root, dir);
    }

    #[test]
    fn the_opencode_server_under_the_cli_does_not_move_the_project() {
        let dir = repo("opencode");
        let home = repo("opencode-home");
        let f = Fake::new(&[(10, 1, "kitty"), (12, 10, "bash"), (13, 12, "opencode"), (14, 13, "opencode")])
            .cwd(13, &dir)
            .args(13, &["/home/a/.opencode/bin/opencode"])
            .cwd(14, &home)
            .args(14, &["/home/a/.opencode/bin/opencode", "serve", "--service"]);
        let Classified { class, project, .. } = classify_with(&f, 10, "", "");
        assert_eq!(class, Class::AgentTerminal);
        assert_eq!(project.unwrap().root, dir);
    }

    #[test]
    fn agent_titles() {
        assert!(is_agent_title("π - kairos") && is_agent_title("✳ Fix the parser") && is_agent_title("opencode"));
        assert!(!is_agent_title("pip install requests") && !is_agent_title("api-server: bash") && !is_agent_title("pi@raspberrypi: ~"));
        assert!(!is_agent_title("xampp control panel") && !is_agent_title("vim example.py"));
    }

    #[test]
    fn node_running_an_agent_cli_counts() {
        for script in ["C:\\Users\\a\\AppData\\Roaming\\npm\\node_modules\\@anthropic-ai\\claude-code\\cli.js",
            "C:\\Users\\a\\AppData\\Roaming\\npm\\node_modules\\@openai\\codex\\bin\\codex.js",
            "C:\\Users\\a\\AppData\\Roaming\\npm\\node_modules\\opencode-ai\\bin\\opencode",
            "C:\\Users\\a\\.pi\\agent\\install\\releases\\1.6.0\\node_modules\\@earendil-works\\pi-coding-agent\\dist\\bundle\\cli.js",
            "C:\\Users\\a\\AppData\\Local\\hermes\\hermes-agent\\ui-tui\\dist\\entry.js"]
        {
            let f = Fake::new(&[(10, 1, "WindowsTerminal.exe"), (12, 10, "cmd.exe"), (13, 12, "node.exe")])
                .args(13, &["node.exe", script]);
            assert_eq!(class_of(&f, 10, ""), Class::AgentTerminal, "{script}");
        }
        let f = Fake::new(&[(10, 1, "WindowsTerminal.exe"), (12, 10, "cmd.exe"), (13, 12, "node.exe")])
            .args(13, &["node.exe", "server.js"]);
        assert_eq!(class_of(&f, 10, ""), Class::Other);
    }

    #[test]
    fn plain_shell_in_a_project_is_terminal() {
        let dir = repo("shell");
        let f = Fake::new(&[(10, 1, "WindowsTerminal.exe"), (11, 10, "OpenConsole.exe"), (12, 11, "pwsh.exe")])
            .cwd(12, &dir);
        let Classified { class, project, .. } = classify_with(&f, 10, "", "PowerShell");
        assert_eq!(class, Class::Terminal);
        assert_eq!(project.unwrap().root, dir);
        let f = Fake::new(&[(10, 1, "WindowsTerminal.exe"), (11, 10, "OpenConsole.exe"), (12, 11, "pwsh.exe")]);
        assert_eq!(class_of(&f, 10, "PowerShell"), Class::Other);
    }

    #[test]
    fn conhost_windows_look_at_their_parent_shell() {
        let dir = repo("conhost");
        let f = Fake::new(&[(20, 1, "cmd.exe"), (21, 20, "conhost.exe")]).cwd(20, &dir);
        assert_eq!(class_of(&f, 21, "C:\\Windows\\system32\\cmd.exe"), Class::Terminal);
        let f = Fake::new(&[(20, 1, "cmd.exe"), (21, 20, "conhost.exe"), (22, 20, "claude.exe")]);
        assert_eq!(class_of(&f, 21, "cmd"), Class::AgentTerminal);
    }

    #[test]
    fn console_windows_are_terminals_whichever_process_owns_them() {
        let dir = repo("console");
        let f = Fake::new(&[(20, 1, "cmd.exe"), (21, 20, "conhost.exe")]).cwd(20, &dir);
        for owner in [20, 21] {
            let Classified { class, project, app } = classify_with(&f, owner, CONSOLE_WINDOW, "C:\\Windows\\system32\\cmd.exe");
            assert_eq!(class, Class::Terminal, "{app}");
            assert_eq!(project.unwrap().root, dir);
        }
        assert_eq!(classify_with(&f, 20, "", "").class, Class::Testing, "a bare shell pid is no console window");

        let claude = "C:\\Users\\a\\AppData\\Roaming\\npm\\node_modules\\@anthropic-ai\\claude-code\\cli.js";
        let f = Fake::new(&[(20, 1, "cmd.exe"), (21, 20, "conhost.exe"), (22, 20, "node.exe")])
            .cwd(20, &dir)
            .args(22, &["node.exe", claude]);
        assert_eq!(classify_with(&f, 20, CONSOLE_WINDOW, "cmd").class, Class::AgentTerminal);

        let f = Fake::new(&[(30, 1, "pwsh.exe"), (31, 30, "conhost.exe"), (32, 30, "python.exe")]).cwd(32, &dir);
        assert_eq!(classify_with(&f, 30, CONSOLE_WINDOW, "pwsh").class, Class::Testing);
        let f = Fake::new(&[(40, 1, "claude.exe"), (41, 40, "conhost.exe")]).cwd(40, &dir);
        assert_eq!(classify_with(&f, 40, CONSOLE_WINDOW, "claude").class, Class::AgentTerminal);
        let f = Fake::new(&[(50, 1, "cmd.exe"), (51, 50, "conhost.exe")]);
        assert_eq!(classify_with(&f, 50, CONSOLE_WINDOW, "cmd").class, Class::Other);
    }

    #[cfg(windows)]
    #[test]
    fn a_terminal_window_shows_only_its_own_sessions() {
        let (dir, other) = (repo("handoff"), repo("other-tab"));
        let claude = "C:\\Users\\a\\AppData\\Roaming\\npm\\node_modules\\@anthropic-ai\\claude-code\\cli.js";
        let mut f = Fake::new(&[
            (1, 0, "explorer.exe"),
            (20, 1, "cmd.exe"),
            (21, 20, "conhost.exe"),
            (10, 5, "WindowsTerminal.exe"),
            (11, 10, "OpenConsole.exe"),
            (12, 10, "pwsh.exe"),
            (13, 12, "node.exe"),
        ])
        .cwd(20, &dir)
        .cwd(13, &other)
        .args(13, &["node.exe", claude]);
        assert_eq!(class_of(&f, 10, "cmd"), Class::AgentTerminal, "every tab of every window counts");
        f.tree.show(10, &[20, 10]);
        let Classified { class, project, .. } = classify_with(&f, 10, "", "C:\\Windows\\system32\\cmd.exe");
        assert_eq!((class, project.map(|p| p.root)), (Class::Terminal, Some(dir)), "only the console handed to it");
        assert_eq!(f.tree.parent(20), Some(1));
        f.tree.show(10, &[12]);
        assert_eq!(classify_with(&f, 10, "", "pwsh").project.map(|p| p.root), Some(other), "only its own tab");
        f.tree.show(10, &[12, 20]);
        f.tree.show(21, &[10]);
        let mut all: Vec<u32> = f.tree.descendants(10).into_iter().map(|(c, _)| c).collect();
        all.sort();
        assert_eq!(all, [12, 13, 20, 21], "a cycle lists each process once");
    }

    #[test]
    fn terminal_titles_mark_agents_only_when_the_platform_reads_them() {
        let f = Fake::new(&[(10, 1, "WindowsTerminal.exe"), (12, 10, "pwsh.exe")]);
        for title in ["⠂ Fix the build", "✳ Claude Code", "codex", "opencode - repo", "Claude"] {
            assert_eq!(class_of(&f, 10, title), Class::AgentTerminal, "{title}");
        }
        assert_eq!(class_of(&f, 10, "PowerShell"), Class::Other);
        assert_eq!(class_of(&f, 10, ""), Class::Other);
    }

    #[test]
    fn windows_editors_browsers_and_agent_apps() {
        let f = Fake::new(&[
            (1, 0, "Code.exe"),
            (2, 0, "idea64.exe"),
            (3, 0, "Notepad++.exe"),
            (4, 0, "msedge.exe"),
            (5, 0, "chrome.exe"),
            (6, 0, "Claude.exe"),
            (7, 0, "ChatGPT.exe"),
            (8, 0, "explorer.exe"),
            (9, 0, "godot_v4.3-stable_win64.exe"),
            (10, 0, "Codex.exe"),
            (11, 0, "T3 Code (Beta).exe"),
        ]);
        assert_eq!(class_of(&f, 1, "main.rs - proj - Visual Studio Code"), Class::Editor);
        assert_eq!(class_of(&f, 2, ""), Class::Editor);
        assert_eq!(class_of(&f, 3, ""), Class::Editor);
        assert_eq!(class_of(&f, 9, ""), Class::Editor);
        assert_eq!(class_of(&f, 4, "Fix build - Claude Code - Claude"), Class::BrowserUnknown);
        assert_eq!(class_of(&f, 4, "Inbox"), Class::BrowserUnknown);
        assert_eq!(class_of(&f, 5, "Fix build - Claude Code - Claude"), Class::BrowserUnknown);
        assert_eq!(class_of(&f, 5, "Some other tab"), Class::BrowserUnknown);
        assert_eq!(class_of(&f, 6, ""), Class::AgentApp);
        assert_eq!(class_of(&f, 7, ""), Class::AgentApp);
        assert_eq!(class_of(&f, 10, ""), Class::AgentApp);
        assert_eq!(class_of(&f, 11, ""), Class::AgentApp);
        assert_eq!(class_of(&f, 8, ""), Class::Other);
    }

    #[test]
    fn macos_apps_by_executable_or_bundle_id() {
        let dir = repo("macos");
        let f = Fake::new(&[
            (1, 0, "Electron"),
            (2, 0, "Xcode"),
            (3, 0, "safari"),
            (4, 0, "google-chrome"),
            (5, 0, "Claude"),
            (6, 0, "ChatGPT"),
            (7, 0, "Finder"),
            (10, 0, "Terminal"),
            (11, 10, "login"),
            (12, 11, "zsh"),
            (20, 0, "stable"),
            (21, 20, "zsh"),
            (22, 21, "claude"),
        ])
        .cwd(12, &dir);
        let class = |pid, wclass| classify_with(&f, pid, wclass, "").class;
        assert_eq!(class(1, "com.microsoft.VSCode"), Class::Editor);
        assert_eq!(class(1, ""), Class::Other);
        assert_eq!(class(2, "com.apple.dt.Xcode"), Class::Editor);
        assert_eq!(class(3, "com.apple.Safari"), Class::BrowserUnknown);
        assert_eq!(class(4, "com.google.Chrome"), Class::BrowserUnknown);
        assert_eq!(class(5, "com.anthropic.claudefordesktop"), Class::AgentApp);
        assert_eq!(class(6, "com.openai.chat"), Class::AgentApp);
        assert_eq!(class(7, "com.apple.finder"), Class::Other);
        assert_eq!(class(10, "com.apple.Terminal"), Class::Terminal);
        assert_eq!(class(20, "dev.warp.Warp-Stable"), Class::AgentTerminal);
    }

    #[test]
    fn editor_project_comes_from_the_command_line() {
        let dir = repo("editor");
        let f = Fake::new(&[(1, 0, "Code.exe")]).args(1, &["Code.exe", "--reuse-window", dir.to_str().unwrap()]);
        let Classified { class, project, .. } = classify_with(&f, 1, "", "");
        assert_eq!(class, Class::Editor);
        assert_eq!(project.unwrap().root, dir);
    }

    #[test]
    fn unknown_app_in_a_project_is_testing() {
        let dir = repo("testing");
        let f = Fake::new(&[(1, 0, "game.exe")]).cwd(1, &dir);
        assert_eq!(class_of(&f, 1, ""), Class::Testing);
    }

    #[test]
    fn a_program_from_a_tool_that_installs_as_a_git_clone_is_not_testing() {
        let base = std::env::temp_dir().join(format!("kairos-classify-tool-{}", std::process::id()));
        let nvm = base.join(".nvm");
        std::fs::create_dir_all(nvm.join(".git")).unwrap();
        std::fs::create_dir_all(nvm.join("versions/bin")).unwrap();
        let node = nvm.join("versions/bin/node");

        let f = Fake::new(&[(1, 0, "node")]).cwd(1, &base).exe(1, &node);
        let c = classify_with(&f, 1, "", "");
        assert_eq!((c.class, c.project), (Class::Other, None));

        let term = Fake::new(&[(10, 1, "kitty"), (11, 10, "bash"), (12, 11, "node")]).cwd(11, &base).cwd(12, &base).exe(12, &node);
        let c = classify_with(&term, 10, "", "");
        assert_eq!((c.class, c.project), (Class::Other, None));

        let dir = repo("tool-own");
        let own = Fake::new(&[(1, 0, "node")]).cwd(1, &dir).exe(1, &node);
        assert_eq!(class_of(&own, 1, ""), Class::Testing, "running in a project still counts");
        let _ = std::fs::remove_dir_all(&base);
    }

    fn siblings(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("kairos-classify-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for name in ["a", "ab"] {
            std::fs::create_dir_all(base.join(name).join("secret")).unwrap();
            std::fs::create_dir_all(base.join(name).join(".git")).unwrap();
        }
        let base = base.canonicalize().unwrap();
        (base.clone(), base.join("a"), base.join("ab"))
    }

    fn excluding(base: &Path, folders: &[PathBuf]) -> Exclusions {
        for f in folders {
            kairos_core::exclude::add(base, f).unwrap();
        }
        kairos_core::exclude::load(base).unwrap()
    }

    #[test]
    fn an_editor_window_takes_the_project_its_title_names() {
        let base = std::env::temp_dir().join(format!("kairos-classify-titled-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for name in ["launched", "shown", "twin/web", "other/web"] {
            std::fs::create_dir_all(base.join(name).join(".git")).unwrap();
        }
        let base = base.canonicalize().unwrap();
        let (launched, shown) = (base.join("launched"), base.join("shown"));
        // One VS Code process started in `launched`, with a terminal and a language server in `shown`.
        let f = Fake::new(&[(10, 1, "code"), (11, 10, "code"), (12, 11, "bash"), (13, 10, "code"), (14, 10, "code")])
            .cwd(10, &launched)
            .cwd(11, &shown)
            .cwd(12, &shown.join(".git"))
            .cwd(13, &base.join("twin/web"))
            .cwd(14, &base.join("other/web"));
        let project = |title: &str, ex: &Exclusions| {
            let c = super::classify_with(&f, 10, "", title, ex);
            (c.class, c.project.map(|p| p.root))
        };
        let none = Exclusions::default();
        assert_eq!(project("main.rs - shown - Visual Studio Code", &none), (Class::Editor, Some(shown.clone())));
        assert_eq!(project("\u{25cf} main.rs - launched - Visual Studio Code", &none), (Class::Editor, Some(launched.clone())));
        assert_eq!(project("shown [~/src/shown] \u{2013} main.rs", &none), (Class::Editor, Some(shown.clone())), "a JetBrains title");
        assert_eq!(project("Welcome - Visual Studio Code", &none), (Class::Editor, Some(launched.clone())), "no folder named: where it started");
        assert_eq!(project("main.rs - web - Visual Studio Code", &none), (Class::Editor, Some(launched.clone())), "two folders named web");
        assert_eq!(project("", &none), (Class::Editor, Some(launched.clone())), "no title (macOS)");

        let ex = excluding(&base, &[shown.clone()]);
        assert_eq!(project("main.rs - shown - Visual Studio Code", &ex), (Class::Other, None));
        assert_eq!(project("main.rs - launched - Visual Studio Code", &ex), (Class::Editor, Some(launched.clone())));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn titles_matter_for_browsers_editors_and_agent_apps_only() {
        assert!(titles_matter("firefox", "") && titles_matter("code", "") && titles_matter("", "jetbrains-idea") && titles_matter("antigravity", ""));
        assert!(!titles_matter("foot", "foot") && !titles_matter("bash", ""));
    }

    #[test]
    fn an_excluded_folder_is_other_with_no_project_whatever_is_in_front() {
        let (base, a, ab) = siblings("excluded");
        let ex = excluding(&base, &[a.clone()]);
        let claude = "C:\\Users\\a\\AppData\\Roaming\\npm\\node_modules\\@anthropic-ai\\claude-code\\cli.js";
        let shell = |dir: &Path| Fake::new(&[(10, 1, "WindowsTerminal.exe"), (11, 10, "OpenConsole.exe"), (12, 11, "pwsh.exe")]).cwd(12, dir);
        let agent = |dir: &Path| {
            Fake::new(&[(10, 1, "WindowsTerminal.exe"), (12, 10, "pwsh.exe"), (13, 12, "node.exe")]).cwd(13, dir).args(13, &["node.exe", claude])
        };
        let editor = |dir: &Path| Fake::new(&[(10, 1, "Code.exe")]).args(10, &["Code.exe", dir.to_str().unwrap()]);
        let game = |dir: &Path| Fake::new(&[(10, 1, "game.exe")]).cwd(10, dir);
        let result = |f: &Fake, ex: &Exclusions| {
            let c = super::classify_with(f, 10, "", "", ex);
            (c.class, c.project.map(|p| p.root))
        };
        let none = Exclusions::default();
        let nowhere = (Class::Other, None);

        assert_eq!(result(&shell(&a), &none), (Class::Terminal, Some(a.clone())));
        assert_eq!(result(&shell(&a), &ex), nowhere);
        assert_eq!(result(&shell(&a.join("secret")), &ex), nowhere, "a folder inside it");
        assert_eq!(result(&shell(&ab), &ex), (Class::Terminal, Some(ab.clone())), "a sibling whose name starts the same");
        assert_eq!(result(&agent(&a), &none), (Class::AgentTerminal, Some(a.clone())));
        assert_eq!(result(&agent(&a), &ex), nowhere);
        assert_eq!(result(&agent(&ab), &ex), (Class::AgentTerminal, Some(ab.clone())));
        assert_eq!(result(&editor(&a), &none), (Class::Editor, Some(a.clone())));
        assert_eq!(result(&editor(&a), &ex), nowhere);
        assert_eq!(result(&editor(&ab), &ex), (Class::Editor, Some(ab.clone())));
        assert_eq!(result(&game(&a), &ex), nowhere);
        assert_eq!(result(&game(&ab), &ex), (Class::Testing, Some(ab.clone())));

        let inside = excluding(&base.join("inside"), &[a.join("secret")]);
        assert_eq!(result(&shell(&a.join("secret")), &inside), nowhere, "a folder in a repo");
        assert_eq!(result(&shell(&a), &inside), (Class::Terminal, Some(a.clone())), "the rest of the repo");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn agent_app_windows_say_which_app() {
        let cases = [
            ("claude", "com.anthropic.claudefordesktop", Some(App::ClaudeDesktop)),
            ("claude", "", Some(App::ClaudeDesktop)),
            ("claude-desktop", "Claude", Some(App::ClaudeDesktop)),
            ("chatgpt", "com.openai.codex", Some(App::CodexApp)),
            ("codex", "", Some(App::CodexApp)),
            ("chatgpt", "com.openai.chat", None),
            ("chatgpt", "OpenAI.Codex_2p2nqsd0c76g0", Some(App::CodexApp)),
            ("chatgpt", "OpenAI.ChatGPT-Desktop_2p2nqsd0c76g0", None),
            ("ai.opencode", "ai.opencode.desktop", Some(App::OpencodeDesktop)),
            ("opencode", "", Some(App::OpencodeDesktop)),
            ("t3code", "com.t3tools.t3code", None),
            ("antigravity", "", None),
        ];
        for (name, wclass, app) in cases {
            assert_eq!(agent_app(name, wclass), app, "{name} {wclass}");
        }
    }
}
