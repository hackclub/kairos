use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kairos_core::event::{Event, Kind};
use kairos_core::exclude;
use kairos_core::{last_turn, paths, project::project_of, prompt_shape, prompt_stats};
use serde_json::Value;

const MAX_INPUT: u64 = 16 * 1024 * 1024;

pub fn run() {
    std::panic::set_hook(Box::new(|_| {}));
    let _ = std::panic::catch_unwind(|| {
        let _ = handle();
    });
}

fn armed_marker(session: &str) -> PathBuf {
    let name: String = session
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    dirs::runtime_dir()
        .unwrap_or_else(paths::data_dir)
        .join("kairos-hook")
        .join(name)
}

fn arm(session: &str) {
    let m = armed_marker(session);
    if let Some(parent) = m.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(m, b"");
}

fn disarm(session: &str) -> bool {
    fs::remove_file(armed_marker(session)).is_ok()
}

fn handle() -> Option<()> {
    let mut input = String::new();
    std::io::stdin().take(MAX_INPUT).read_to_string(&mut input).ok()?;
    let v: Value = serde_json::from_str(&input).ok()?;
    let str_of = |k: &str| v.get(k).and_then(Value::as_str);
    let session = str_of("session_id")?.to_string();
    let cwd = str_of("cwd").map(Path::new);
    if cwd.is_some_and(|c| exclude::load(&paths::data_dir()).unwrap_or_default().covers(c)) {
        return None;
    }
    let project = cwd.and_then(project_of).map(|p| p.id);

    let kind = match str_of("hook_event_name")? {
        "UserPromptSubmit" => {
            let prompt = str_of("prompt").unwrap_or("");
            let (words, pasted) = prompt_stats(prompt);
            arm(&session);
            Kind::Prompt { words, pasted, model: None, session, shape: Some(prompt_shape(prompt)), version: None, via: None, permissions: None }
        }
        "PreToolUse" => {
            if !disarm(&session) {
                return None;
            }
            Kind::AgentStart { session }
        }
        "Stop" => {
            disarm(&session);
            let content = fs::read(str_of("transcript_path")?).ok()?;
            let t = last_turn(&String::from_utf8_lossy(&content));
            Kind::ReplyDone {
                session,
                reply_words: t.reply_words,
                tools: t.tools,
                files_changed: t.files_changed,
                model: t.model,
            }
        }
        _ => return None,
    };
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs_f64();
    let event = Event::new((ts * 1000.0).round() / 1000.0, project, kind);
    send(&event);
    Some(())
}

fn send(event: &Event) {
    let Ok(mut line) = serde_json::to_string(event) else { return };
    line.push('\n');
    if send_socket(&line).is_none() {
        spool(&line);
    }
}

fn send_socket(line: &str) -> Option<()> {
    let mut s = UnixStream::connect(paths::socket_path()).ok()?;
    s.set_write_timeout(Some(Duration::from_millis(100))).ok()?;
    s.write_all(line.as_bytes()).ok()
}

fn spool(line: &str) {
    let path = paths::spool_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

pub fn print_snippet() {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "kairos".to_string());
    let exe = if exe.contains(char::is_whitespace) { format!("'{exe}'") } else { exe };
    let entry = |matcher: Option<&str>| {
        let mut e = serde_json::json!({
            "hooks": [{"type": "command", "command": format!("{exe} hook"), "timeout": 5}]
        });
        if let Some(m) = matcher {
            e["matcher"] = Value::String(m.to_string());
        }
        serde_json::json!([e])
    };
    let snippet = serde_json::json!({
        "hooks": {
            "UserPromptSubmit": entry(None),
            "PreToolUse": entry(Some("*")),
            "Stop": entry(None),
        }
    });
    println!("{}", serde_json::to_string_pretty(&snippet).unwrap_or_default());
}
