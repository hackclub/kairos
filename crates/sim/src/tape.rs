use kairos_core::event::{Class, Event, Kind, GENESIS};

const PRESENT_EVERY: f64 = 60.0;
const IDLE_AFTER: f64 = 60.0;

struct Raw {
    ts: f64,
    project: Option<String>,
    harness: Option<String>,
    kind: Kind,
}

struct Pause {
    from: f64,
    until: f64,
    resume: Option<f64>,
}

impl Pause {
    fn end(&self) -> f64 {
        self.resume.map_or(self.until, |r| r.min(self.until))
    }
}

pub fn ms(t: f64) -> f64 {
    (t * 1000.0).round() / 1000.0
}

#[derive(Default)]
pub struct Tape {
    raw: Vec<Raw>,
    activity: Vec<(f64, f64)>,
    pauses: Vec<Pause>,
}

impl Tape {
    pub fn new() -> Tape {
        Tape::default()
    }

    pub fn start(&mut self, ts: f64, os: &str) {
        self.at(ts, None, Kind::Start { version: "0.1.0".into(), os: os.into() });
        self.at(ts + 0.2, None, Kind::Backends { focus: "x11".into(), idle: "x11".into() });
    }

    pub fn at(&mut self, ts: f64, project: Option<&str>, kind: Kind) {
        self.raw.push(Raw { ts: ms(ts), project: project.map(str::to_string), harness: None, kind });
    }

    pub fn focus(&mut self, ts: f64, class: Class, project: Option<&str>) {
        self.at(ts, project, Kind::Focus { class, app: None });
    }

    pub fn touch(&mut self, a: f64, b: f64) {
        if b >= a {
            self.activity.push((a, b));
        }
    }

    pub fn prompt(&mut self, ts: f64, project: &str, harness: &str, session: &str, words: usize, pasted: bool) {
        self.raw.push(Raw {
            ts: ms(ts),
            project: Some(project.into()),
            harness: Some(harness.into()),
            kind: Kind::Prompt { words, pasted, model: Some("sim-model".into()), session: session.into(), shape: None, version: None, via: None, permissions: None },
        });
    }

    pub fn agent(&mut self, start: f64, end: f64, project: &str, harness: &str, session: &str, reply_words: usize) {
        self.raw.push(Raw {
            ts: ms(start),
            project: Some(project.into()),
            harness: Some(harness.into()),
            kind: Kind::AgentStart { session: session.into() },
        });
        self.raw.push(Raw {
            ts: ms(end),
            project: Some(project.into()),
            harness: Some(harness.into()),
            kind: Kind::ReplyDone {
                session: session.into(),
                reply_words,
                tools: 3,
                files_changed: 1,
                model: Some("sim-model".into()),
            },
        });
    }

    pub fn pause(&mut self, from: f64, until: f64, resume: Option<f64>) {
        self.pauses.push(Pause { from: ms(from), until: ms(until), resume: resume.map(ms) });
    }

    fn presence(&mut self) {
        let mut iv = std::mem::take(&mut self.activity);
        iv.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged: Vec<(f64, f64)> = Vec::new();
        for (a, b) in iv {
            match merged.last_mut() {
                Some(l) if a - l.1 < IDLE_AFTER => l.1 = l.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        for (a, b) in merged {
            let mut t = a + PRESENT_EVERY;
            while t < b + IDLE_AFTER {
                self.at(t, None, Kind::Present { input: None, keys: None, injected: None });
                t += PRESENT_EVERY;
            }
            self.at(b + IDLE_AFTER, None, Kind::Idle {});
        }
    }

    /// Drops events inside pauses like the recorder and writes the last focus change at the pause's end.
    pub fn finish(self) -> Vec<Event> {
        self.finish_from(GENESIS)
    }

    pub fn finish_from(mut self, anchor: &str) -> Vec<Event> {
        self.presence();
        let pauses = std::mem::take(&mut self.pauses);
        let mut raw: Vec<Raw> = Vec::with_capacity(self.raw.len());
        let mut held: Vec<Option<Raw>> = pauses.iter().map(|_| None).collect();
        'events: for r in self.raw {
            for (i, p) in pauses.iter().enumerate() {
                if r.ts > p.from && r.ts < p.end() {
                    if matches!(r.kind, Kind::Focus { .. }) {
                        held[i] = Some(r);
                    }
                    continue 'events;
                }
            }
            raw.push(r);
        }
        for (p, held) in pauses.iter().zip(held) {
            raw.push(Raw { ts: p.from, project: None, harness: None, kind: Kind::Pause { until: p.until } });
            if let Some(r) = p.resume.filter(|r| *r < p.until) {
                raw.push(Raw { ts: r, project: None, harness: None, kind: Kind::Resume {} });
            }
            if let Some(mut f) = held {
                f.ts = p.end();
                raw.push(f);
            }
        }
        raw.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        let mut prev = anchor.to_string();
        raw.into_iter()
            .map(|r| {
                let mut e = Event::new(r.ts, r.project, r.kind);
                e.harness = r.harness;
                e.seal(&prev);
                prev = e.hash.clone();
                e
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kairos_core::event::verify_chain;

    fn lines(events: &[Event]) -> Vec<String> {
        events.iter().map(|e| serde_json::to_string(e).unwrap()).collect()
    }

    #[test]
    fn presence_ticks_then_idle() {
        let mut t = Tape::new();
        t.focus(1000.0, Class::Editor, Some("a/b"));
        t.touch(1000.0, 1200.0);
        let ev = t.finish();
        let times: Vec<(f64, &str)> = ev.iter().map(|e| (e.ts, e.kind.name())).collect();
        assert_eq!(
            times,
            vec![
                (1000.0, "focus"),
                (1060.0, "present"),
                (1120.0, "present"),
                (1180.0, "present"),
                (1240.0, "present"),
                (1260.0, "idle")
            ]
        );
    }

    #[test]
    fn short_gaps_do_not_idle() {
        let mut t = Tape::new();
        t.touch(0.0, 100.0);
        t.touch(130.0, 200.0);
        let n = t.finish().iter().filter(|e| matches!(e.kind, Kind::Idle {})).count();
        assert_eq!(n, 1);
    }

    #[test]
    fn pause_drops_events_and_holds_focus() {
        let mut t = Tape::new();
        t.focus(100.0, Class::Editor, Some("p"));
        t.touch(100.0, 1000.0);
        t.pause(300.0, 600.0, None);
        t.focus(400.0, Class::Terminal, Some("p"));
        let ev = t.finish();
        assert!(ev.iter().all(|e| !(e.ts > 300.0 && e.ts < 600.0)), "{:?}", ev.iter().map(|e| e.ts).collect::<Vec<_>>());
        let at_end = ev.iter().find(|e| e.ts == 600.0).expect("held focus written at the end of the pause");
        assert!(matches!(at_end.kind, Kind::Focus { class: Class::Terminal, .. }));
        assert!(ev.iter().any(|e| e.ts == 300.0 && matches!(e.kind, Kind::Pause { until } if until == 600.0)));
    }

    #[test]
    fn chain_verifies_and_is_stable() {
        let build = || {
            let mut t = Tape::new();
            t.start(10.0, "linux");
            t.prompt(20.0, "p", "claude-code", "s1", 12, false);
            t.touch(10.0, 400.0);
            t.finish()
        };
        let a = build();
        let l = lines(&a);
        let ok = verify_chain(l.iter().map(String::as_str), Some(GENESIS)).unwrap();
        assert_eq!(ok.events, a.len());
        assert_eq!(a, build());
    }
}
