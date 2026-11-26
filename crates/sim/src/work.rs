use chrono::{Duration, NaiveDate, NaiveDateTime, TimeZone};
use chrono_tz::Tz;
use kairos_core::event::{Class, Kind};

use crate::rng::Rng;
use crate::tape::Tape;

pub fn local(tz: Tz, day: NaiveDate, hours: f64) -> f64 {
    let naive: NaiveDateTime = day.and_hms_opt(0, 0, 0).unwrap() + Duration::seconds((hours * 3600.0).round() as i64);
    match tz.from_local_datetime(&naive).earliest().or_else(|| tz.from_local_datetime(&naive).latest()) {
        Some(t) => t.timestamp() as f64,
        None => tz.from_local_datetime(&(naive + Duration::hours(1))).earliest().map_or(0.0, |t| t.timestamp() as f64),
    }
}

pub fn local_day(tz: Tz, ts: f64) -> NaiveDate {
    chrono::DateTime::from_timestamp(ts as i64, 0).unwrap().with_timezone(&tz).date_naive()
}

#[derive(Clone)]
pub struct Chat {
    pub project: String,
    pub harness: &'static str,
    pub session: String,
    pub focus: Class,
    pub run: (f64, f64),
    pub prompt_words: (u64, u64),
    pub reply_words: (u64, u64),
    pub edit_p: f64,
    pub browse_p: f64,
    pub tests_every: u32,
}

impl Chat {
    pub fn new(project: &str, harness: &'static str, session: &str, focus: Class) -> Chat {
        Chat {
            project: project.into(),
            harness,
            session: session.into(),
            focus,
            run: (40.0, 420.0),
            prompt_words: (6, 60),
            reply_words: (80, 700),
            edit_p: 0.35,
            browse_p: 0.15,
            tests_every: 4,
        }
    }
}

fn sha(rng: &mut Rng) -> String {
    format!("{:016x}{:016x}{:08x}", rng.next_u64(), rng.next_u64(), rng.next_u64() as u32)
}

pub fn typing_secs(rng: &mut Rng, words: u64) -> f64 {
    words as f64 / 35.0 * 60.0 * rng.range(1.0, 1.8)
}

pub fn reading_secs(rng: &mut Rng, reply_words: u64) -> f64 {
    reply_words as f64 / 230.0 * 60.0 * rng.range(0.7, 1.4) + rng.range(5.0, 45.0)
}

fn edit_noise(tape: &mut Tape, rng: &mut Rng, project: &str, a: f64, b: f64) {
    let mut t = a + rng.range(10.0, 40.0);
    while t < b {
        tape.at(t, Some(project), Kind::FileSave { count: rng.int(1, 3) as usize, paths: vec![] });
        t += rng.range(25.0, 60.0);
    }
}

pub fn editor_burst(tape: &mut Tape, rng: &mut Rng, project: &str, a: f64, b: f64) {
    tape.focus(a, Class::Editor, Some(project));
    tape.touch(a, b);
    edit_noise(tape, rng, project, a, b);
}

pub fn commit(tape: &mut Tape, rng: &mut Rng, project: &str, ts: f64) {
    let sha = sha(rng);
    tape.at(
        ts,
        Some(project),
        Kind::Commit { sha, files: rng.int(1, 8) as u32, added: rng.int(3, 200) as u32, removed: rng.int(0, 80) as u32, authored: None, committed: None, via: None, changes: vec![], replaces: None, not_before: None },
    );
}

pub fn turn(tape: &mut Tape, rng: &mut Rng, chat: &Chat, t: f64, project_focus: bool) -> (f64, f64, u64) {
    let words = rng.int(chat.prompt_words.0, chat.prompt_words.1);
    let pasted = rng.chance(0.08);
    let typing = typing_secs(rng, words);
    tape.touch(t, t + typing);
    let p = t + typing;
    tape.prompt(p, &chat.project, chat.harness, &chat.session, words as usize, pasted);
    let run = rng.range(chat.run.0, chat.run.1);
    let reply = rng.int(chat.reply_words.0, chat.reply_words.1);
    let r = p + run;
    tape.agent(p + 0.8, r, &chat.project, chat.harness, &chat.session, reply as usize);
    if project_focus {
        tape.focus(t, chat.focus, Some(&chat.project));
    }
    (p, r, reply)
}

pub fn chat_loop(tape: &mut Tape, rng: &mut Rng, chat: &Chat, from: f64, to: f64) -> f64 {
    let mut t = from;
    let mut n = 0u32;
    tape.focus(t, chat.focus, Some(&chat.project));
    while t < to {
        let (p, r, reply) = turn(tape, rng, chat, t, false);
        if r - p > 45.0 {
            let x = rng.unit();
            if x < chat.edit_p {
                editor_burst(tape, rng, &chat.project, p + 8.0, r - 5.0);
                tape.focus(r - 3.0, chat.focus, Some(&chat.project));
            } else if x < chat.edit_p + chat.browse_p {
                tape.focus(p + 5.0, Class::BrowserUnknown, None);
                tape.touch(p + 5.0, r - 3.0);
                tape.focus(r - 2.0, chat.focus, Some(&chat.project));
            }
        }
        let read = reading_secs(rng, reply);
        tape.touch(r, r + read);
        t = r + read;
        n += 1;
        if chat.tests_every > 0 && n % chat.tests_every == 0 {
            let d = rng.range(20.0, 70.0);
            tape.focus(t, Class::Testing, Some(&chat.project));
            tape.touch(t, t + d);
            t += d;
            tape.focus(t, chat.focus, Some(&chat.project));
            if rng.chance(0.5) {
                commit(tape, rng, &chat.project, t + 5.0);
            }
        }
        if n % 3 == 0 {
            tape.at(t, Some(&chat.project), Kind::Diff { files: rng.int(1, 6) as u32, added: rng.int(5, 120) as u32, removed: rng.int(0, 40) as u32, changes: vec![] });
        }
    }
    t
}

pub fn multi_agent(tape: &mut Tape, rng: &mut Rng, threads: &[Chat], from: f64, to: f64, editor_project: &str) -> f64 {
    struct Th {
        chat: Chat,
        ready_at: f64,
        reply: u64,
    }
    let mut ths: Vec<Th> = threads.iter().map(|c| Th { chat: c.clone(), ready_at: 0.0, reply: 0 }).collect();
    let mut t = from;
    while t < to {
        let i = (0..ths.len()).min_by(|a, b| ths[*a].ready_at.total_cmp(&ths[*b].ready_at)).unwrap();
        if ths[i].ready_at > t {
            let gap = ths[i].ready_at - t;
            if gap > 40.0 && rng.chance(0.5) {
                editor_burst(tape, rng, editor_project, t + 3.0, ths[i].ready_at - 3.0);
            } else if gap > 40.0 {
                tape.focus(t + 3.0, Class::Terminal, Some(editor_project));
                tape.touch(t + 3.0, ths[i].ready_at - 3.0);
            }
            t = ths[i].ready_at;
        }
        let chat = ths[i].chat.clone();
        tape.focus(t, chat.focus, Some(&chat.project));
        if ths[i].reply > 0 {
            let read = reading_secs(rng, ths[i].reply);
            tape.touch(t, t + read);
            t += read;
        }
        let (p, r, reply) = turn(tape, rng, &chat, t, false);
        ths[i].ready_at = r;
        ths[i].reply = reply;
        t = p + rng.range(1.0, 4.0);
        if rng.chance(0.08) {
            commit(tape, rng, &chat.project, t + 2.0);
        }
    }
    t
}

pub fn editor_and_docs(tape: &mut Tape, rng: &mut Rng, chat: &Chat, from: f64, to: f64) -> f64 {
    let mut t = from;
    while t < to {
        let x = rng.unit();
        if x < 0.30 {
            tape.focus(t, Class::Editor, Some(&chat.project));
            let (_, r, reply) = turn(tape, rng, chat, t, false);
            let read = reading_secs(rng, reply);
            tape.touch(r, r + read);
            t = r + read;
        } else if x < 0.62 {
            let d = rng.range(120.0, 420.0);
            editor_burst(tape, rng, &chat.project, t, t + d);
            t += d;
        } else if x < 0.78 {
            let d = rng.range(60.0, 240.0);
            tape.focus(t, Class::Docs, None);
            tape.touch(t, t + d);
            t += d;
        } else if x < 0.85 {
            let d = rng.range(180.0, 480.0);
            tape.focus(t, Class::WebAgent, None);
            tape.touch(t, t + d);
            t += d;
        } else if x < 0.92 {
            let d = rng.range(30.0, 120.0);
            tape.focus(t, if rng.chance(0.5) { Class::Testing } else { Class::Terminal }, Some(&chat.project));
            tape.touch(t, t + d);
            t += d;
        } else {
            let d = rng.range(180.0, 720.0);
            tape.focus(t, Class::BrowserUnknown, None);
            tape.touch(t, t + d);
            t += d;
        }
        if rng.chance(0.06) {
            commit(tape, rng, &chat.project, t);
        }
        t += rng.range(2.0, 15.0);
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_handles_midnight_and_gaps() {
        let tz: Tz = "America/New_York".parse().unwrap();
        let d = NaiveDate::from_ymd_opt(2026, 3, 7).unwrap();
        let a = local(tz, d, 22.0);
        let b = local(tz, d, 28.0);
        assert_eq!(b - a, 5.0 * 3600.0, "2am to 3am does not exist on 2026-03-08");
        assert_eq!(local_day(tz, a), d);
    }
}
