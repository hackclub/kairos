use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{Local, NaiveDate, TimeZone};
use kairos_core::credit::{total_credit_secs, Rule};
use kairos_core::event::{input_bits, load_events, log_files, Event, Kind, INPUT_WINDOW_SECS};
use kairos_core::paths;

use crate::diag::diag;
use crate::log::Log;
use crate::platform::Backends;
use crate::status::{self, Shared};
use crate::{now, Msg};

const PRESENT_EVERY: Duration = Duration::from_secs(60);
const STATUS_EVERY: Duration = Duration::from_secs(10);

pub struct Recorder {
    log: Log,
    status: Shared,
    hooks: bool,
    active: bool,
    next_present: Option<Instant>,
    input: Seen,
    keys: Seen,
    injected: Seen,
    paused_until: Option<f64>,
    // The newest focus seen while paused, written when tracking resumes so credit starts from the
    // window that is actually in front.
    held_focus: Option<Event>,
    today: Today,
    next_status: Instant,
}

/// The seconds of one kind of input. Only kept while the presence backend reports that kind; without it a present
/// carries no bits for it.
#[derive(Default)]
struct Seen {
    tracked: bool,
    at: Vec<f64>,
}

impl Seen {
    fn push(&mut self, t: f64) {
        if self.tracked {
            self.at.retain(|&s| s >= t - INPUT_WINDOW_SECS);
            self.at.push(t);
        }
    }

    fn bits(&self, ts: f64) -> Option<String> {
        self.tracked.then(|| input_bits(ts, &self.at))
    }
}

struct Today {
    date: NaiveDate,
    events: Vec<Event>,
}

impl Today {
    fn load(date: NaiveDate) -> Today {
        let path = paths::log_path(&date.to_string());
        let events = if path.exists() { load_events(&path).unwrap_or_default() } else { Vec::new() };
        Today { date, events }
    }

    fn push(&mut self, e: Event) {
        let date = Local::now().date_naive();
        if date != self.date {
            *self = Today { date, events: Vec::new() };
        }
        self.events.push(e);
    }

    fn credited_secs(&self) -> f64 {
        let start = Local
            .from_local_datetime(&self.date.and_hms_opt(0, 0, 0).unwrap_or_default())
            .earliest()
            .map_or(0.0, |t| t.timestamp() as f64);
        let credits: Vec<_> = Rule::CURRENT
            .apply(&self.events)
            .into_iter()
            .filter(|c| c.end > start)
            .map(|mut c| {
                c.start = c.start.max(start);
                c
            })
            .collect();
        total_credit_secs(&credits)
    }
}

/// A pause that was still on when kairosd last stopped. It holds across a restart: the server leaves out everything
/// until it ends, so recording again would only send what the person asked not to.
fn pending_pause(dir: &Path) -> Option<f64> {
    let files = log_files(dir);
    let mut until = None;
    for path in &files[files.len().saturating_sub(2)..] {
        for e in load_events(path).unwrap_or_default() {
            match e.kind {
                Kind::Pause { until: u } => until = Some(u),
                Kind::Resume {} => until = None,
                _ => {}
            }
        }
    }
    until.filter(|u| *u > now())
}

fn describe(e: &Event) -> String {
    let mut s = e.kind.name().to_string();
    match &e.kind {
        Kind::Focus { class, .. } => s.push_str(&format!(" class={}", class.as_str())),
        Kind::FileSave { count, .. } => s.push_str(&format!(" count={count}")),
        Kind::Backends { focus, idle } => s.push_str(&format!(" focus={focus} idle={idle}")),
        Kind::Commit { files, added, removed, .. } | Kind::Diff { files, added, removed, .. } => {
            s.push_str(&format!(" files={files} added={added} removed={removed}"))
        }
        Kind::AgentEdit { added, removed, .. } => s.push_str(&format!(" added={added} removed={removed}")),
        Kind::ProjectAlias { from } => s.push_str(&format!(" from={from}")),
        _ => {}
    }
    if let Some(p) = &e.project {
        s.push_str(&format!(" project={p}"));
    }
    if let Some(h) = &e.harness {
        s.push_str(&format!(" harness={h}"));
    }
    s
}

impl Recorder {
    pub fn open(dir: &Path, status: Shared, hooks: bool) -> Result<Recorder> {
        let log = Log::open(&dir.to_path_buf())?;
        Ok(Recorder {
            log,
            status,
            hooks,
            active: false,
            next_present: None,
            input: Seen::default(),
            keys: Seen::default(),
            injected: Seen::default(),
            paused_until: pending_pause(dir),
            held_focus: None,
            today: Today::load(Local::now().date_naive()),
            next_status: Instant::now(),
        })
    }

    fn write(&mut self, e: Event) {
        match self.log.append(e) {
            Ok(written) => {
                diag!("kairosd: {}", describe(&written));
                self.today.push(written);
            }
            Err(err) => diag!("kairosd: write failed: {err}"),
        }
    }

    fn publish(&mut self) {
        let secs = self.today.credited_secs();
        let paused_until = self.paused_until;
        status::set(&self.status, |s| {
            s.today_secs = secs;
            s.paused_until = paused_until;
        });
        self.next_status = Instant::now() + STATUS_EVERY;
    }

    fn paused(&self) -> bool {
        self.paused_until.is_some()
    }

    fn set_backends(&mut self, b: &Backends) {
        status::set(&self.status, |s| {
            s.focus = b.focus;
            s.idle = b.idle;
        });
        self.write(Event::new(now(), None, Kind::Backends { focus: b.focus.into(), idle: b.idle.into() }));
    }

    fn start_presence(&mut self) {
        self.next_present = (self.active && !self.paused()).then(|| Instant::now() + PRESENT_EVERY);
    }

    fn pause(&mut self, until: f64) {
        if until <= now() {
            return;
        }
        self.paused_until = Some(until);
        self.next_present = None;
        self.write(Event::new(now(), None, Kind::Pause { until }));
        self.publish();
    }

    fn resume(&mut self, explicit: bool) {
        if !self.paused() {
            return;
        }
        self.paused_until = None;
        if explicit {
            self.write(Event::new(now(), None, Kind::Resume {}));
        }
        if let Some(mut f) = self.held_focus.take() {
            f.ts = now();
            self.write(f);
        }
        self.start_presence();
        self.publish();
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Event(e) | Msg::Hook(e) if self.paused() => {
                if matches!(e.kind, Kind::Focus { .. }) {
                    self.held_focus = Some(e);
                }
            }
            Msg::Event(e) => self.write(e),
            Msg::Hook(e) => {
                if self.hooks {
                    self.write(e)
                }
            }
            Msg::PresenceReady => {
                diag!("kairosd: presence detection active");
                self.active = true;
                self.stop_input();
                self.start_presence();
            }
            Msg::InputReady => self.input.tracked = true,
            Msg::InputKinds { keys, injected } => (self.keys.tracked, self.injected.tracked) = (keys, injected),
            Msg::Input(t) if !self.paused() => self.input.push(t),
            Msg::Keys(t) if !self.paused() => self.keys.push(t),
            Msg::Injected(t) if !self.paused() => self.injected.push(t),
            Msg::Input(_) | Msg::Keys(_) | Msg::Injected(_) => {}
            Msg::PresenceLost => {
                self.active = false;
                self.stop_input();
                self.next_present = None;
            }
            Msg::Resumed => {
                self.active = true;
                self.start_presence();
            }
            Msg::Idled => {
                self.next_present = None;
                if self.active {
                    self.active = false;
                    if !self.paused() {
                        self.write(Event::new(now(), None, Kind::Idle {}));
                    }
                }
            }
            Msg::Pause(until) => self.pause(until),
            Msg::Resume => self.resume(true),
            Msg::Backends(b) => self.set_backends(&b),
        }
    }

    fn stop_input(&mut self) {
        for seen in [&mut self.input, &mut self.keys, &mut self.injected] {
            seen.tracked = false;
        }
    }

    fn write_present(&mut self, ts: f64) {
        let (input, keys, injected) = (self.input.bits(ts), self.keys.bits(ts), self.injected.bits(ts));
        self.write(Event::new(ts, None, Kind::Present { input, keys, injected }));
    }

    fn tick(&mut self) {
        if self.paused_until.is_some_and(|u| now() >= u) {
            self.resume(false);
        }
        if let Some(t) = self.next_present {
            if Instant::now() >= t {
                self.write_present(now());
                self.next_present = Some((t + PRESENT_EVERY).max(Instant::now()));
            }
        }
        if Instant::now() >= self.next_status {
            self.publish();
        }
    }

    fn next_wake(&self) -> Duration {
        let mut wake = self.next_status;
        if let Some(t) = self.next_present {
            wake = wake.min(t);
        }
        let mut d = wake.saturating_duration_since(Instant::now());
        if let Some(u) = self.paused_until {
            d = d.min(Duration::from_secs_f64((u - now()).max(0.0)));
        }
        d
    }

    pub fn run(mut self, backends: Backends, rx: Receiver<Msg>) -> Result<()> {
        self.write(Event::new(
            now(),
            None,
            Kind::Start { version: env!("CARGO_PKG_VERSION").into(), os: std::env::consts::OS.into() },
        ));
        self.set_backends(&backends);
        loop {
            match rx.recv_timeout(self.next_wake()) {
                Ok(msg) => self.handle(msg),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            self.tick();
        }
    }
}

#[cfg(test)]
mod tests {
    use kairos_core::event::{FileChange, Via};

    use super::*;

    #[test]
    fn a_pause_holds_across_a_restart_until_it_ends_or_is_resumed() {
        let dir = std::env::temp_dir().join(format!("kairos-pause-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let write = |day: &str, kinds: Vec<Kind>| {
            let text: String = kinds.into_iter().map(|k| serde_json::to_string(&Event::new(1.0, None, k)).unwrap() + "\n").collect();
            std::fs::write(dir.join(format!("events-{day}.jsonl")), text).unwrap();
        };
        let later = now() + 3600.0;
        assert_eq!(pending_pause(&dir), None);
        write("2026-11-20", vec![Kind::Pause { until: later }]);
        assert_eq!(pending_pause(&dir), Some(later));
        write("2026-11-21", vec![Kind::Idle {}]);
        assert_eq!(pending_pause(&dir), Some(later), "paused yesterday, until today");
        write("2026-11-21", vec![Kind::Idle {}, Kind::Resume {}]);
        assert_eq!(pending_pause(&dir), None);
        write("2026-11-21", vec![Kind::Resume {}, Kind::Pause { until: now() - 1.0 }]);
        assert_eq!(pending_pause(&dir), None, "it ended while kairosd was down");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_log_line_has_counts_but_no_paths() {
        let changes = vec![FileChange { path: "src/private-name.rs".into(), added: 1, removed: 0 }];
        let commit = Kind::Commit {
            sha: "a".repeat(40),
            files: 1,
            added: 1,
            removed: 0,
            authored: Some(1),
            committed: Some(2),
            via: Some(Via::Commit),
            changes: changes.clone(),
            replaces: None,
            not_before: None,
        };
        let saved = Kind::FileSave { count: 1, paths: vec!["src/private-name.rs".into()] };
        let diff = Kind::Diff { files: 1, added: 1, removed: 0, changes };
        for kind in [commit, saved, diff] {
            let line = describe(&Event::new(1.0, Some("o/r".into()), kind));
            assert!(!line.contains("private-name"), "{line}");
        }
    }

    #[test]
    fn the_journal_line_for_agent_work_has_counts_and_no_paths_or_shas() {
        let edit = Event::new(1.0, Some("o/r".into()), Kind::AgentEdit { session: "s".into(), path: "secret/dir/a.rs".into(), added: 4, removed: 2 });
        assert_eq!(describe(&edit), "agent_edit added=4 removed=2 project=o/r");
        let commit = Event::new(1.0, None, Kind::AgentCommit { session: "s".into(), sha: Some("abc1234".into()) });
        assert_eq!(describe(&commit), "agent_commit");
    }

    struct TestDir(std::path::PathBuf);

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn recorder(name: &str) -> (Recorder, TestDir) {
        let dir = std::env::temp_dir().join(format!("kairos-recorder-{}-{name}", std::process::id()));
        let mut rec = Recorder::open(&dir, status::shared(), false).unwrap();
        rec.today = Today { date: Local::now().date_naive(), events: Vec::new() };
        (rec, TestDir(dir))
    }

    fn last_present_input(rec: &Recorder) -> Option<String> {
        match &rec.today.events.last().expect("an event was written").kind {
            Kind::Present { input, .. } => input.clone(),
            other => panic!("not a present: {other:?}"),
        }
    }

    #[test]
    fn a_present_carries_the_seconds_with_input() {
        let (mut rec, _dir) = recorder("bits");
        rec.handle(Msg::PresenceReady);
        rec.handle(Msg::InputReady);
        for t in [930.0, 940.0, 941.7, 941.9, 999.5, 1000.0] {
            rec.handle(Msg::Input(t));
        }
        rec.write_present(1000.0);
        assert_eq!(last_present_input(&rec).as_deref(), Some("800000000000003"));
        rec.write_present(1001.0);
        assert_eq!(last_present_input(&rec).as_deref(), Some("c00000000000001"));
    }

    #[test]
    fn a_backend_without_input_gets_presents_without_the_field() {
        let (mut rec, _dir) = recorder("none");
        rec.handle(Msg::PresenceReady);
        rec.handle(Msg::Input(999.0));
        rec.write_present(1000.0);
        assert_eq!(last_present_input(&rec), None);

        rec.handle(Msg::InputReady);
        rec.handle(Msg::Input(999.0));
        rec.write_present(1000.0);
        assert_eq!(last_present_input(&rec).as_deref(), Some("800000000000000"));

        rec.handle(Msg::PresenceLost);
        rec.handle(Msg::PresenceReady);
        rec.write_present(1000.0);
        assert_eq!(last_present_input(&rec), None);
    }

    #[test]
    fn a_present_carries_key_and_injected_seconds_only_from_a_backend_that_tells_them_apart() {
        let (mut rec, _dir) = recorder("kinds");
        let kinds = |rec: &Recorder| match &rec.today.events.last().expect("an event was written").kind {
            Kind::Present { keys, injected, .. } => (keys.clone(), injected.clone()),
            other => panic!("not a present: {other:?}"),
        };
        rec.handle(Msg::PresenceReady);
        rec.handle(Msg::InputReady);
        rec.handle(Msg::Keys(999.0));
        rec.write_present(1000.0);
        assert_eq!(kinds(&rec), (None, None));

        rec.handle(Msg::InputKinds { keys: true, injected: true });
        for t in [941.0, 999.5] {
            rec.handle(Msg::Input(t));
            rec.handle(Msg::Keys(t));
        }
        rec.handle(Msg::Injected(942.2));
        rec.write_present(1000.0);
        assert_eq!(last_present_input(&rec).as_deref(), Some("800000000000002"));
        assert_eq!(kinds(&rec), (Some("800000000000002".into()), Some("000000000000004".into())));

        rec.handle(Msg::PresenceReady);
        rec.handle(Msg::InputReady);
        rec.handle(Msg::InputKinds { keys: true, injected: false });
        rec.write_present(1000.0);
        assert_eq!(kinds(&rec), (Some("800000000000002".into()), None));
    }

    #[test]
    fn input_while_paused_is_not_kept() {
        let (mut rec, _dir) = recorder("paused");
        rec.handle(Msg::PresenceReady);
        rec.handle(Msg::InputReady);
        rec.handle(Msg::Pause(now() + 1000.0));
        rec.handle(Msg::Input(990.0));
        rec.handle(Msg::InputKinds { keys: true, injected: true });
        rec.handle(Msg::Keys(990.0));
        rec.handle(Msg::Injected(990.0));
        rec.handle(Msg::Resume);
        rec.write_present(1000.0);
        assert_eq!(last_present_input(&rec).as_deref(), Some("000000000000000"));
        let Kind::Present { keys, injected, .. } = &rec.today.events.last().unwrap().kind else { panic!("not a present") };
        assert_eq!((keys.as_deref(), injected.as_deref()), (Some("000000000000000"), Some("000000000000000")));
    }
}
