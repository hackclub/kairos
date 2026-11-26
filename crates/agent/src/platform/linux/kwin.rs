//! KWin has no focus query or event over D-Bus, so a small script is loaded into it that calls `Activated` on a
//! service owned here whenever the active window or its title changes.

use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use std::thread;

use anyhow::{anyhow, Context, Result};
use kairos_core::paths;
use zbus::blocking::{Connection, Proxy};
use zbus::interface;

use super::backend::{running, Reporter, Running};
use super::dbus::{self, Notice, Watch};
use super::session::SessionEnv;
use crate::platform::Foreground;

pub const NAME: &str = "kwin";

const KWIN: &str = "org.kde.KWin";
const SERVICE: &str = "dev.obayy.Kairos";
const PATH: &str = "/Focus";
const INTERFACE: &str = "dev.obayy.Kairos.Focus";
const PLUGIN: &str = "kairos-focus";

/// The pid goes over as a string: how KWin's script engine types JS numbers on the wire differs
/// between Plasma versions, strings do not.
fn script_source() -> String {
    format!(
        r#"var current = null;

function send(w) {{
    callDBus("{SERVICE}", "{PATH}", "{INTERFACE}", "Activated",
        w ? String(w.pid) : "0", w ? String(w.resourceClass) : "", w ? String(w.caption) : "", w ? String(w.internalId) : "");
}}

function onCaption() {{
    send(current);
}}

function onActivated(w) {{
    if (current && current.captionChanged) {{
        try {{ current.captionChanged.disconnect(onCaption); }} catch (e) {{}}
    }}
    current = w;
    if (current && current.captionChanged) {{
        current.captionChanged.connect(onCaption);
    }}
    send(current);
}}

var activated = workspace.windowActivated || workspace.clientActivated;
activated.connect(onActivated);
onActivated(workspace.activeWindow || workspace.activeClient);
"#
    )
}

fn foreground(pid: &str, class: &str, caption: &str, window: &str) -> Option<Foreground> {
    let pid = pid.parse::<u32>().ok().filter(|p| *p > 0)?;
    let window = (!window.is_empty()).then(|| {
        let mut h = DefaultHasher::new();
        window.hash(&mut h);
        h.finish() | 1
    });
    let window = window.unwrap_or(0);
    Some(Foreground { pid, wclass: class.to_owned(), title: caption.to_owned(), sessions: Vec::new(), window })
}

struct Focus {
    reporter: Mutex<Reporter>,
}

#[interface(name = "dev.obayy.Kairos.Focus")]
impl Focus {
    fn activated(&self, pid: &str, resource_class: &str, caption: &str, window: &str) {
        if let Ok(mut r) = self.reporter.lock() {
            r.report(foreground(pid, resource_class, caption, window));
        }
    }
}

fn kwin_proxy<'a>(conn: &'a Connection, path: &'a str, interface: &'a str) -> Result<Proxy<'a>> {
    Ok(Proxy::new(conn, KWIN, path, interface)?)
}

fn load_script(conn: &Connection, file: &Path) -> Result<()> {
    let scripting = kwin_proxy(conn, "/Scripting", "org.kde.kwin.Scripting")?;
    let _: bool = scripting.call("unloadScript", &PLUGIN)?;
    let id: i32 = scripting.call("loadScript", &(file.to_string_lossy().as_ref(), PLUGIN))?;
    if id < 0 {
        return Err(anyhow!("KWin refused the script"));
    }
    // Plasma 6 exposes the script at /Scripting/Script<id>, Plasma 5 at /<id>.
    let mut last = None;
    for path in [format!("/Scripting/Script{id}"), format!("/{id}")] {
        match kwin_proxy(conn, &path, "org.kde.kwin.Script")?.call::<_, _, ()>("run", &()) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    Err(anyhow!("cannot run the KWin script: {}", last.map_or_else(String::new, |e| e.to_string())))
}

pub fn start(env: &SessionEnv, fg: Sender<Option<Foreground>>) -> Result<Running> {
    // The service must be up before KWin runs the script, or its first report is lost.
    let conn = dbus::builder(env)?
        .name(SERVICE)?
        .serve_at(PATH, Focus { reporter: Mutex::new(Reporter::new(fg)) })?
        .build()
        .context("cannot own the Kairos D-Bus name")?;
    let mut watch = Watch::new(&conn, KWIN, None)?;
    let dir = paths::data_dir();
    fs::create_dir_all(&dir)?;
    let file = dir.join("kwin-focus.js");
    fs::write(&file, script_source())?;
    load_script(&conn, &file)?;
    let (handle, life) = running(NAME);
    thread::spawn(move || {
        let _life = life;
        let _conn = conn;
        while watch.next() != Notice::Gone {}
    });
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_calls_the_service_this_module_owns() {
        let js = script_source();
        assert!(js.contains(r#"callDBus("dev.obayy.Kairos", "/Focus", "dev.obayy.Kairos.Focus", "Activated""#));
    }

    #[test]
    fn the_script_supports_plasma_5_and_6() {
        let js = script_source();
        assert!(js.contains("workspace.windowActivated || workspace.clientActivated"));
        assert!(js.contains("workspace.activeWindow || workspace.activeClient"));
    }

    #[test]
    fn the_script_reports_once_at_load_and_follows_title_changes() {
        let js = script_source();
        assert!(js.contains("onActivated(workspace.activeWindow"));
        assert!(js.contains("captionChanged.connect(onCaption)"));
        assert!(js.contains("captionChanged.disconnect(onCaption)"));
    }

    #[test]
    fn the_script_has_balanced_braces_and_no_leftover_format_markers() {
        let js = script_source();
        assert_eq!(js.matches('{').count(), js.matches('}').count());
        assert!(!js.contains("{{") && !js.contains("}}"));
    }

    #[test]
    fn reports_parse_into_foregrounds() {
        assert_eq!(
            foreground("4242", "kitty", "~", ""),
            Some(Foreground { pid: 4242, wclass: "kitty".into(), title: "~".into(), sessions: vec![], window: 0 })
        );
        let a = foreground("4242", "kitty", "~", "{a1}").unwrap().window;
        assert!(a != 0 && a == foreground("4242", "kitty", "~", "{a1}").unwrap().window, "the same window, the same id");
        assert_ne!(a, foreground("4242", "kitty", "~", "{b2}").unwrap().window);
        assert_eq!(foreground("0", "", "", ""), None);
        assert_eq!(foreground("junk", "kitty", "~", ""), None);
    }
}
