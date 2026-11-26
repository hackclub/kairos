#![allow(dead_code)]

use kairos_core::event::{Ask, Class, Event, Kind, Via};

pub const BASE: f64 = 1_773_100_800.0;

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03);
        r.next();
        r
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }

    pub fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn chance(&mut self, p: f64) -> bool {
        self.f() < p
    }

    pub fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.f()
    }

    pub fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len())]
    }

    pub fn shuffle<T>(&mut self, xs: &mut [T]) {
        for i in (1..xs.len()).rev() {
            xs.swap(i, self.below(i + 1));
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Profile {
    pub events: usize,
    pub gaps_of_days: bool,
    pub crashed_turns: bool,
    pub long_turns: bool,
    pub projectless_replies: bool,
    pub queued_heartbeats: bool,
    pub long_pauses: bool,
    pub hostile: bool,
    pub wild: bool,
}

impl Profile {
    pub const CLEAN: Profile = Profile {
        events: 120,
        gaps_of_days: false,
        crashed_turns: false,
        long_turns: false,
        projectless_replies: false,
        queued_heartbeats: false,
        long_pauses: false,
        hostile: false,
        wild: false,
    };
}

const PROJECTS: [&str; 3] = ["ada/site", "ada/api", "ada/lib"];
const HARNESSES: [Option<&str>; 5] = [Some("claude-code"), Some("codex"), Some("gemini-cli"), Some("cursor"), None];
const CLASSES: [Class; 10] = [
    Class::AgentTerminal,
    Class::Editor,
    Class::Terminal,
    Class::Testing,
    Class::BrowserUnknown,
    Class::Other,
    Class::AgentApp,
    Class::WebAgent,
    Class::Docs,
    Class::Unknown,
];
const WORKISH: [Class; 5] = [Class::AgentTerminal, Class::Editor, Class::Terminal, Class::Testing, Class::AgentApp];
const AGENTISH: [Class; 3] = [Class::AgentTerminal, Class::Editor, Class::AgentApp];

struct Session {
    id: String,
    project: Option<&'static str>,
    harness: Option<&'static str>,
    reply_at: Option<f64>,
    started: f64,
}

struct Gen<'a> {
    rng: &'a mut Rng,
    p: Profile,
    t: f64,
    out: Vec<Event>,
    sessions: Vec<Session>,
}

fn ev(ts: f64, project: Option<&str>, harness: Option<&str>, kind: Kind) -> Event {
    let mut e = Event::new(ts, project.map(str::to_string), kind);
    e.harness = harness.map(str::to_string);
    e
}

impl Gen<'_> {
    fn push(&mut self, ts: f64, project: Option<&str>, harness: Option<&str>, kind: Kind) {
        self.out.push(ev(ts, project, harness, kind));
    }

    fn gap(&mut self) -> f64 {
        let r = self.rng.f();
        match r {
            r if r < 0.50 => self.rng.range(1.0, 30.0),
            r if r < 0.78 => self.rng.range(30.0, 300.0),
            r if r < 0.92 => self.rng.range(300.0, 3600.0),
            r if r < 0.985 || !self.p.gaps_of_days => self.rng.range(3600.0, 9.0 * 3600.0),
            _ => self.rng.range(26.0 * 3600.0, 60.0 * 3600.0),
        }
    }

    fn whole(&mut self) -> f64 {
        if self.rng.chance(0.5) {
            self.t.floor()
        } else {
            self.t
        }
    }

    fn input(&mut self) -> Option<String> {
        match self.rng.below(10) {
            0 => None,
            1 => Some(format!("{:015x}", 0)),
            2 => Some(format!("{:015x}", (1u64 << 60) - 1)),
            _ => {
                let density = self.rng.range(0.05, 0.9);
                let mut bits = 0u64;
                for i in 0..60 {
                    if self.rng.chance(density) {
                        bits |= 1 << i;
                    }
                }
                Some(format!("{bits:015x}"))
            }
        }
    }

    fn present(&mut self) {
        let input = if self.p.hostile && self.rng.chance(0.05) {
            Some(self.rng.pick(&["", "zz", "fffffffffffffffff", "+1f"]).to_string())
        } else {
            self.input()
        };
        let ts = self.whole();
        self.push(ts, None, None, Kind::Present { input });
    }

    fn presents(&mut self, n: usize) {
        let step = if self.rng.chance(0.15) { 30.0 } else { 60.0 };
        for _ in 0..n {
            self.t += step + self.rng.range(0.0, 3.0);
            self.present();
        }
    }

    fn focus(&mut self, class: Class, project: Option<&str>) {
        let ts = self.whole();
        self.push(ts, project, None, Kind::Focus { class, app: None });
    }

    fn random_focus(&mut self) {
        let class = self.rng.pick(&CLASSES);
        let needs_project = matches!(class, Class::AgentTerminal | Class::Editor | Class::Terminal | Class::Testing | Class::AgentApp);
        let project = if (needs_project && !self.rng.chance(0.1)) || self.rng.chance(0.15) { Some(self.rng.pick(&PROJECTS)) } else { None };
        self.focus(class, project);
    }

    fn session(&mut self) -> usize {
        self.rng.below(self.sessions.len())
    }

    fn words(&mut self) -> usize {
        match self.rng.below(8) {
            0 => self.rng.below(3),
            1 => 3,
            2 => self.rng.below(15) + 3,
            3 => self.rng.below(80) + 20,
            4 => self.rng.below(300),
            5 => 60,
            _ => self.rng.below(40) + 3,
        }
    }

    fn prompt(&mut self, i: usize) {
        if !self.p.long_turns {
            if let Some(at) = self.sessions[i].reply_at.filter(|at| *at != f64::MAX) {
                self.sessions[i].reply_at = None;
                let when = at.min(self.t).max(self.sessions[i].started);
                self.reply(i, when);
            }
        }
        let words = self.words();
        let pasted = self.rng.chance(0.25);
        let (id, project, harness) = (self.sessions[i].id.clone(), self.sessions[i].project, self.sessions[i].harness);
        let project = if self.rng.chance(0.05) { None } else { project };
        let ts = self.whole();
        self.push(ts, project, harness, Kind::Prompt { words, pasted, model: None, session: id.clone(), shape: None, version: None, via: None });
        let dur = if self.p.long_turns && self.rng.chance(0.15) {
            self.rng.range(3700.0, 8.0 * 3600.0)
        } else if self.rng.chance(0.3) {
            self.rng.range(300.0, 3000.0)
        } else {
            self.rng.range(5.0, 300.0)
        };
        if self.rng.chance(0.7) {
            let at = ts + self.rng.range(0.5, 30.0f64.min(dur * 0.5));
            self.push(at, project, harness, Kind::AgentStart { session: id });
            self.sessions[i].started = at;
        } else {
            self.sessions[i].started = ts;
        }
        let crash = self.p.crashed_turns && self.rng.chance(0.08);
        self.sessions[i].reply_at = Some(if crash { f64::MAX } else { ts + dur });
    }

    fn reply(&mut self, i: usize, at: f64) {
        let (id, project, harness) = (self.sessions[i].id.clone(), self.sessions[i].project, self.sessions[i].harness);
        let project = match self.p.projectless_replies && self.rng.chance(0.3) {
            true if self.rng.chance(0.5) => Some(self.rng.pick(&PROJECTS)),
            true => None,
            false => project,
        };
        let reply_words = self.words() * 4;
        let tools = self.rng.below(12);
        self.push(at, project, harness, Kind::ReplyDone { session: id, reply_words, tools, files_changed: tools / 3, model: None });
    }

    fn flush_replies(&mut self) {
        for i in 0..self.sessions.len() {
            if let Some(at) = self.sessions[i].reply_at.filter(|at| *at <= self.t) {
                self.sessions[i].reply_at = None;
                self.reply(i, at);
            }
        }
    }

    fn answered(&mut self, i: usize) {
        let asked = (self.t - self.rng.range(3.0, 1500.0)).max(BASE);
        let (id, project, harness) = (self.sessions[i].id.clone(), self.sessions[i].project, self.sessions[i].harness);
        let why = self.rng.pick(&[Ask::Question, Ask::Plan, Ask::Permission]);
        let shown_words = self.rng.below(400);
        let answer_words = if self.rng.chance(0.5) { 0 } else { self.rng.below(60) };
        let ts = self.whole();
        self.push(ts, project, harness, Kind::Answered { session: id, asked, why, shown_words, answer_words });
    }

    fn file_save(&mut self, i: usize) {
        let project = self.sessions[i].project;
        let n = self.rng.below(3) + 1;
        let paths: Vec<String> = (0..n).map(|k| format!("src/f{}.rs", self.rng.below(3) + k)).collect();
        let ts = self.whole();
        self.push(ts, project, None, Kind::FileSave { count: n, paths: paths.clone() });
        if self.rng.chance(0.5) {
            let harness = self.sessions[i].harness;
            let id = self.sessions[i].id.clone();
            for path in paths {
                if self.rng.chance(0.8) {
                    let at = ts + self.rng.range(-30.0, 30.0);
                    let (added, removed) = (self.rng.below(30) as u32, self.rng.below(10) as u32);
                    self.push(at, project, harness, Kind::AgentEdit { session: id.clone(), path, added, removed });
                }
            }
        }
    }

    fn heartbeat(&mut self) {
        let project = if self.rng.chance(0.9) { Some(self.rng.pick(&PROJECTS)) } else { None };
        let at = if self.p.queued_heartbeats && self.rng.chance(0.3) { (self.t - self.rng.range(600.0, 6.0 * 3600.0)).max(BASE) } else { self.t };
        let human_lines = match self.rng.below(3) {
            0 => None,
            1 => Some(0),
            _ => Some(self.rng.below(20) as i64 + 1),
        };
        let write = self.rng.chance(0.7);
        let path = self.rng.chance(0.5).then(|| format!("src/f{}.rs", self.rng.below(4)));
        let ai_lines = self.rng.chance(0.3).then(|| self.rng.below(20) as i64);
        let ts = self.t;
        self.push(ts, project, None, Kind::Heartbeat { at, editor: Some("vscode".into()), language: None, category: None, path, write, ai_lines, human_lines });
    }

    fn pause(&mut self) {
        let span = if self.p.long_pauses && self.rng.chance(0.5) { self.rng.range(3.0 * 3600.0, 12.0 * 3600.0) } else { self.rng.range(60.0, 3600.0) };
        let ts = self.t;
        self.push(ts, None, None, Kind::Pause { until: ts + span });
    }

    fn episode(&mut self) {
        let i = self.session();
        let project = self.sessions[i].project;
        let class = self.rng.pick(&AGENTISH);
        self.focus(class, project);
        let n = self.rng.below(7) + 1;
        self.presents(n);
        self.t += self.rng.range(0.0, 20.0);
        self.prompt(i);
        let n = self.rng.below(5);
        if n > 0 {
            let reading = self.rng.pick(&WORKISH);
            let again = if self.rng.chance(0.6) { project } else { Some(self.rng.pick(&PROJECTS)) };
            self.t += self.rng.range(1.0, 20.0);
            self.focus(reading, again);
            self.presents(n);
        }
    }

    fn queued_edit(&mut self) {
        let i = self.session();
        let project = self.sessions[i].project;
        self.prompt(i);
        self.sessions[i].reply_at = Some(self.t + 3000.0);
        self.t += 2.0;
        self.focus(Class::Editor, project);
        self.presents(6);
        let seen = self.t - 150.0;
        let sent = self.t + self.rng.range(4000.0, 5.0 * 3600.0);
        self.push(
            sent,
            project,
            None,
            Kind::Heartbeat { at: seen, editor: Some("vscode".into()), language: None, category: None, path: None, write: true, ai_lines: None, human_lines: Some(5) },
        );
    }

    fn pause_stack(&mut self) {
        let ts = self.t;
        self.push(ts, None, None, Kind::Pause { until: ts + 10.0 * 3600.0 });
        for k in 1..=2 {
            self.push(ts + 60.0 * k as f64, None, None, Kind::Pause { until: ts + 60.0 * k as f64 + 300.0 });
        }
        self.t += self.rng.range(3.5 * 3600.0, 6.0 * 3600.0);
        let class = self.rng.pick(&[Class::Terminal, Class::Testing, Class::Editor]);
        let project = Some(self.rng.pick(&PROJECTS));
        self.focus(class, project);
        let n = self.rng.below(5) + 2;
        self.presents(n);
    }

    fn abandoned_turn(&mut self) {
        let i = self.session();
        let project = self.sessions[i].project;
        self.prompt(i);
        self.sessions[i].reply_at = Some(f64::MAX);
        self.t += self.rng.range(30.0 * 3600.0, 50.0 * 3600.0);
        self.focus(Class::Editor, project);
        let n = self.rng.below(5) + 2;
        self.presents(n);
    }

    fn step(&mut self) {
        self.t += self.gap();
        self.flush_replies();
        if self.p.queued_heartbeats && self.rng.chance(0.05) {
            return self.queued_edit();
        }
        if self.p.long_pauses && self.rng.chance(0.04) {
            return self.pause_stack();
        }
        if self.p.crashed_turns && self.p.gaps_of_days && self.rng.chance(0.04) {
            return self.abandoned_turn();
        }
        let r = self.rng.f();
        let i = self.session();
        match r {
            r if r < 0.22 => self.episode(),
            r if r < 0.40 => self.random_focus(),
            r if r < 0.70 => {
                let n = self.rng.below(6) + 1;
                self.presents(n);
            }
            r if r < 0.75 => {
                let ts = self.whole();
                self.push(ts, None, None, Kind::Idle {});
            }
            r if r < 0.80 => self.prompt(i),
            r if r < 0.85 => self.answered(i),
            r if r < 0.89 => self.file_save(i),
            r if r < 0.93 => self.heartbeat(),
            r if r < 0.955 => self.pause(),
            r if r < 0.97 => {
                let ts = self.t;
                self.push(ts, None, None, Kind::Resume {});
            }
            r if r < 0.98 => {
                let ts = self.t;
                let project = self.rng.pick(&PROJECTS);
                let kind = match self.rng.below(5) {
                    0 => Kind::Commit { sha: "abcdef1".into(), files: 1, added: 3, removed: 1, authored: None, committed: None, via: Some(Via::Commit), changes: vec![], replaces: None, not_before: None },
                    1 => Kind::Diff { files: 2, added: 4, removed: 4, changes: vec![] },
                    2 => Kind::AgentCommit { session: "s1".into(), sha: None },
                    3 => Kind::ProjectAlias { from: "old/name".into() },
                    _ => Kind::Backends { focus: "x".into(), idle: "y".into() },
                };
                self.push(ts, Some(project), None, kind);
            }
            _ => self.episode(),
        }
    }
}

pub fn generate(rng: &mut Rng, p: Profile) -> Vec<Event> {
    let sessions = (0..rng.below(3) + 1)
        .map(|k| Session {
            id: format!("s{}", k + 1),
            project: if rng.chance(0.9) { Some(rng.pick(&PROJECTS)) } else { None },
            harness: rng.pick(&HARNESSES),
            reply_at: None,
            started: 0.0,
        })
        .collect();
    let mut g = Gen { rng, p, t: BASE + 3600.0, out: Vec::new(), sessions };
    let first = g.rng.pick(&WORKISH);
    let project = Some(g.rng.pick(&PROJECTS));
    g.focus(first, project);
    while g.out.len() < p.events {
        g.step();
    }
    g.t += 1e9;
    g.flush_replies();
    let mut out = g.out;
    out.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    if p.hostile {
        hostile(rng, &mut out);
    }
    if p.wild {
        wild(rng, &mut out);
    }
    out
}

fn hostile(rng: &mut Rng, ev: &mut Vec<Event>) {
    for _ in 0..rng.below(4) {
        if ev.is_empty() {
            return;
        }
        let at = rng.below(ev.len());
        let copy = ev[at].clone();
        let to = if rng.chance(0.5) { at } else { rng.below(ev.len()) };
        ev.insert(to, copy);
    }
    for _ in 0..rng.below(6) {
        let at = rng.below(ev.len());
        let reach = if rng.chance(0.3) { 72.0 * 3600.0 } else { 7200.0 };
        let shift = rng.range(1.0, reach) * if rng.chance(0.5) { 1.0 } else { -1.0 };
        ev[at].ts = (ev[at].ts + shift).max(BASE);
    }
    for _ in 0..rng.below(5) {
        if ev.len() > 2 {
            let at = rng.below(ev.len() - 1);
            ev[at].ts = ev[at + 1].ts;
        }
    }
    for _ in 0..rng.below(6) {
        if ev.len() > 2 {
            ev.remove(rng.below(ev.len()));
        }
    }
    for _ in 0..rng.below(4) {
        if ev.len() > 2 {
            let at = rng.below(ev.len() - 1);
            ev.swap(at, at + 1);
        }
    }
}

fn wild(rng: &mut Rng, ev: &mut [Event]) {
    let odd = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e300, -1e300, f64::MAX, f64::MIN, 0.0, -0.0, 1e-300];
    for _ in 0..rng.below(6) + 1 {
        if ev.is_empty() {
            return;
        }
        let at = rng.below(ev.len());
        let v = rng.pick(&odd);
        match &mut ev[at].kind {
            Kind::Answered { asked, shown_words, answer_words, .. } => match rng.below(3) {
                0 => *asked = v,
                1 => *shown_words = usize::MAX,
                _ => *answer_words = usize::MAX,
            },
            Kind::Pause { until } => *until = v,
            Kind::Heartbeat { at, .. } => *at = v,
            Kind::Prompt { words, .. } if rng.chance(0.5) => *words = usize::MAX,
            Kind::ReplyDone { reply_words, .. } if rng.chance(0.5) => *reply_words = usize::MAX,
            _ => ev[at].ts = v,
        }
    }
}

pub fn distinct_ts(ev: &mut [Event]) {
    let mut order: Vec<usize> = (0..ev.len()).collect();
    order.sort_by(|&a, &b| ev[a].ts.total_cmp(&ev[b].ts));
    let mut last = f64::MIN;
    for i in order {
        if ev[i].ts <= last {
            ev[i].ts = last + 1e-3;
        }
        last = ev[i].ts;
    }
}

pub fn describe(ev: &[Event]) -> String {
    let t0 = ev.iter().map(|e| e.ts).filter(|t| t.is_finite()).fold(f64::INFINITY, f64::min);
    let t0 = if t0.is_finite() { t0.floor() } else { 0.0 };
    let mut s = format!("  (times are seconds after {t0})\n");
    for (i, e) in ev.iter().enumerate() {
        s.push_str(&format!("  {i:3} +{:<10.3} {:?} {:?} {:?}\n", e.ts - t0, e.project.as_deref(), e.harness.as_deref(), e.kind));
    }
    s
}

pub fn cases(default: usize) -> usize {
    std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub fn seed() -> u64 {
    std::env::var("KAIROS_PROP_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(0x5eed_0006)
}

pub fn shrink(mut ev: Vec<Event>, max_evals: usize, fails: &mut dyn FnMut(&[Event]) -> bool) -> Vec<Event> {
    let mut evals = 0;
    let mut chunk = (ev.len() / 2).max(1);
    loop {
        let mut progress = false;
        let mut at = 0;
        while at < ev.len() {
            if evals >= max_evals {
                return ev;
            }
            let end = (at + chunk).min(ev.len());
            let mut trial = ev.clone();
            trial.drain(at..end);
            evals += 1;
            if !trial.is_empty() && fails(&trial) {
                ev = trial;
                progress = true;
            } else {
                at = end;
            }
        }
        if chunk == 1 && !progress {
            break;
        }
        if chunk > 1 {
            chunk = (chunk / 2).max(1);
        }
    }
    for i in 0..ev.len() {
        if evals >= max_evals {
            break;
        }
        let mut trial = ev.clone();
        let rounded = (trial[i].ts - BASE).round() + BASE;
        if rounded == trial[i].ts || !rounded.is_finite() {
            continue;
        }
        trial[i].ts = rounded;
        evals += 1;
        if fails(&trial) {
            ev = trial;
        }
    }
    ev
}

pub fn run_cases(name: &str, default_cases: usize, shrink_evals: usize, p: Profile, prop: impl FnMut(&[Event], u64) -> Result<(), String>) -> Result<(), String> {
    run_cases_from(name, default_cases, shrink_evals, p, generate, prop)
}

pub fn run_cases_from(
    name: &str,
    default_cases: usize,
    shrink_evals: usize,
    p: Profile,
    make: impl Fn(&mut Rng, Profile) -> Vec<Event>,
    mut prop: impl FnMut(&[Event], u64) -> Result<(), String>,
) -> Result<(), String> {
    let shrink_evals = std::env::var("KAIROS_PROP_SHRINK").ok().and_then(|v| v.parse().ok()).unwrap_or(shrink_evals);
    for i in 0..cases(default_cases) {
        let case_seed = seed().wrapping_add(i as u64);
        let mut rng = Rng::new(case_seed);
        let ev = make(&mut rng, p);
        let Err(first) = prop(&ev, case_seed) else { continue };
        let timed_out = first.starts_with("timeout");
        let small = if timed_out { ev.clone() } else { shrink(ev.clone(), shrink_evals, &mut |t| prop(t, case_seed).is_err()) };
        let why = prop(&small, case_seed).err().unwrap_or(first);
        return Err(format!(
            "{name}: case {i} (seed {case_seed}, rerun with KAIROS_PROP_SEED={case_seed} PROPTEST_CASES=1) failed, shrunk from {} to {} events\n{why}\n{}",
            ev.len(),
            small.len(),
            describe(&small)
        ));
    }
    Ok(())
}

pub type Iv = (f64, f64);

pub fn norm(mut v: Vec<Iv>) -> Vec<Iv> {
    v.retain(|iv| iv.1 > iv.0);
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<Iv> = Vec::new();
    for (s, e) in v {
        match out.last_mut() {
            Some(l) if s <= l.1 => l.1 = l.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

pub fn meet(a: &[Iv], b: &[Iv]) -> Vec<Iv> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() && j < b.len() {
        let (s, e) = (a[i].0.max(b[j].0), a[i].1.min(b[j].1));
        if e > s {
            out.push((s, e));
        }
        if a[i].1 < b[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

pub fn minus(a: &[Iv], b: &[Iv]) -> Vec<Iv> {
    let mut out = Vec::new();
    for &(s, e) in a {
        let mut cur = s;
        for &(bs, be) in b {
            if be <= cur {
                continue;
            }
            if bs >= e {
                break;
            }
            if bs > cur {
                out.push((cur, bs));
            }
            cur = cur.max(be);
        }
        if cur < e {
            out.push((cur, e));
        }
    }
    out
}

pub fn measure(v: &[Iv]) -> f64 {
    v.iter().map(|iv| iv.1 - iv.0).sum()
}
