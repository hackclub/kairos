use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use kairos_core::sources::claude::Claude;
use kairos_core::sources::codex::Codex;
use kairos_core::sources::opencode::OpenCode;
use kairos_core::sources::Source;
use serde_json::{json, Value};

// The lab branch keeps, for every supported version of a harness, what the harness wrote in a scripted session
// (`store.tar.gz`) and what a released kairosd reported while it ran (`events.jsonl`). The readers here have to get
// the same facts out of the same files.
fn samples() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("KAIROS_LAB_SAMPLES") {
        let dir = PathBuf::from(dir);
        assert!(dir.is_dir(), "KAIROS_LAB_SAMPLES is set but {} is not a folder", dir.display());
        return Some(dir);
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../lab/samples");
    dir.is_dir().then_some(dir)
}

fn folders(dir: &Path) -> Vec<PathBuf> {
    let mut all: Vec<PathBuf> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.is_dir()).collect();
    all.sort();
    all
}

fn source(harness: &str, store: &Path) -> Option<Box<dyn Source>> {
    Some(match harness {
        "claude-code" => Box::new(Claude::new(vec![store.join(".claude/projects")])),
        "codex" => Box::new(Codex::new(vec![store.join(".codex/sessions")])),
        "opencode" => Box::new(OpenCode::new(store.join(".local/share/opencode/opencode.db"))),
        _ => return None,
    })
}

const NEWER: &[&str] = &["permissions"];

fn ms(ts: f64) -> i64 {
    (ts * 1000.0).round() as i64
}

// The daemon adds the project and the harness, and names an edited file from the project's root.
fn recorded(sample: &Path) -> Vec<(i64, Value)> {
    let text = fs::read_to_string(sample.join("events.jsonl")).unwrap();
    let mut events = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut e: Value = serde_json::from_str(line).unwrap();
        let fields = e.as_object_mut().unwrap();
        if fields.remove("harness").is_none_or(|h| h.is_null()) {
            continue;
        }
        let ts = fields.remove("ts").and_then(|t| t.as_f64()).unwrap();
        for added in ["project", "v", "prev", "hash", "seq"] {
            fields.remove(added);
        }
        events.push((ms(ts), e));
    }
    events
}

fn read(src: &mut dyn Source, project: &str) -> Vec<(i64, Value)> {
    // A state with a start time and no files, so everything in the store is new.
    src.restore(&json!({ "since": 0, "cursor": 0 }));
    let root = format!("{}/", project.trim_end_matches('/'));
    let mut found = Vec::new();
    for f in src.poll() {
        let mut kind = serde_json::to_value(&f.kind).unwrap();
        if kind["kind"] == "agent_edit" {
            let path = kind["path"].as_str().unwrap().replace('\\', "/");
            kind["path"] = json!(path.strip_prefix(&root).unwrap_or(&path));
        }
        found.push((ms(f.ts), kind));
    }
    found
}

#[test]
fn readers_report_what_the_released_daemon_did_for_every_saved_session() {
    let Some(dir) = samples() else {
        eprintln!("no lab/samples here, nothing checked");
        return;
    };
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join("lab-samples");
    let _ = fs::remove_dir_all(&scratch);
    let (mut checked, mut wrong) = (0, Vec::new());
    for sample in folders(&dir).iter().flat_map(|h| folders(h)).flat_map(|v| folders(&v)) {
        let run: Value = serde_json::from_str(&fs::read_to_string(sample.join("run.json")).unwrap()).unwrap();
        let name = sample.strip_prefix(&dir).unwrap().display().to_string();
        let out = scratch.join(&name);
        fs::create_dir_all(&out).unwrap();
        let tar = Command::new("tar").arg("-xzf").arg(sample.join("store.tar.gz")).arg("-C").arg(&out).status().unwrap();
        assert!(tar.success(), "could not unpack {name}");
        let Some(mut src) = source(run["harness"].as_str().unwrap(), &out.join("store")) else {
            wrong.push(format!("{name}: no reader for {}", run["harness"]));
            continue;
        };
        let (want, mut got) = (recorded(&sample), read(src.as_mut(), run["project"].as_str().unwrap()));
        // A field newer than the daemon that recorded the sample is checked once a sample is recorded with it.
        for (w, g) in want.iter().zip(got.iter_mut()) {
            for field in NEWER {
                if w.1.get(field).is_none() {
                    g.1.as_object_mut().unwrap().remove(*field);
                }
            }
        }
        checked += 1;
        if want.is_empty() {
            wrong.push(format!("{name}: the sample has no harness events"));
        }
        for i in 0..want.len().max(got.len()) {
            if want.get(i) != got.get(i) {
                let show = |e: Option<&(i64, Value)>| e.map_or("nothing".to_string(), |(ts, kind)| format!("{ts} {kind}"));
                wrong.push(format!("{name}: event {i}\n  recorded {}\n  read     {}", show(want.get(i)), show(got.get(i))));
                break;
            }
        }
    }
    let _ = fs::remove_dir_all(&scratch);
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    assert!(checked > 0, "{} has no samples", dir.display());
}
