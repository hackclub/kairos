use chrono::{Datelike, Local, TimeZone};

use super::pause::Pause;
use crate::status::{Signin, Status};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    OpenDashboard,
    SignIn,
    Pause(Pause),
    Resume,
}

#[cfg_attr(target_os = "linux", allow(dead_code))]
impl Action {
    pub fn id(self) -> &'static str {
        match self {
            Action::OpenDashboard => "dashboard",
            Action::SignIn => "signin",
            Action::Pause(Pause::HalfHour) => "pause-30m",
            Action::Pause(Pause::Hour) => "pause-1h",
            Action::Pause(Pause::Tomorrow) => "pause-tomorrow",
            Action::Resume => "resume",
        }
    }

    pub fn from_id(id: &str) -> Option<Action> {
        Some(match id {
            "dashboard" => Action::OpenDashboard,
            "signin" => Action::SignIn,
            "pause-30m" => Action::Pause(Pause::HalfHour),
            "pause-1h" => Action::Pause(Pause::Hour),
            "pause-tomorrow" => Action::Pause(Pause::Tomorrow),
            "resume" => Action::Resume,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Label(String),
    Separator,
    Entry(Action, String),
}

pub fn menu_model(s: &Status, now: f64) -> Vec<Item> {
    model_in(s, now, &Local)
}

pub fn tooltip(s: &Status) -> String {
    format!("Kairos: {} today", duration(s.today_secs))
}

pub fn is_paused(s: &Status, now: f64) -> bool {
    s.paused_until.is_some_and(|u| u > now)
}

fn model_in<Tz: TimeZone>(s: &Status, now: f64, tz: &Tz) -> Vec<Item> {
    let paused = is_paused(s, now);
    let mut items = vec![
        Item::Label(format!("Today: {}", duration(s.today_secs))),
        Item::Label(status_line(s, now, tz)),
        Item::Separator,
        Item::Entry(Action::OpenDashboard, "Open dashboard".into()),
    ];
    if s.username.is_none() || s.token_rejected {
        items.push(Item::Entry(Action::SignIn, "Sign in…".into()));
    }
    items.push(Item::Separator);
    items.push(Item::Entry(Action::Pause(Pause::HalfHour), "Pause for 30 minutes".into()));
    items.push(Item::Entry(Action::Pause(Pause::Hour), "Pause for 1 hour".into()));
    if paused {
        items.push(Item::Entry(Action::Resume, "Resume tracking".into()));
    }
    items
}

fn status_line<Tz: TimeZone>(s: &Status, now: f64, tz: &Tz) -> String {
    if let Signin::Waiting { code } = &s.signin {
        return format!("Signing in… enter code {code}");
    }
    if let Some(until) = s.paused_until.filter(|u| *u > now) {
        return format!("Paused until {}", clock(until, now, tz));
    }
    if s.username.is_none() || s.token_rejected {
        return match &s.signin {
            Signin::Failed { reason } => format!("Can't sign in: {reason}"),
            _ if s.token_rejected => "Signed out, sign in to keep syncing".into(),
            _ => "Not signed in".into(),
        };
    }
    match &s.upload_error {
        Some(e) => format!("Can't upload: {}", short_reason(e)),
        None => "Tracking".into(),
    }
}

fn duration(secs: f64) -> String {
    let minutes = (secs.max(0.0) / 60.0).round() as u64;
    match minutes / 60 {
        0 => format!("{minutes} m"),
        h => format!("{h} h {} m", minutes % 60),
    }
}

fn clock<Tz: TimeZone>(at: f64, now: f64, tz: &Tz) -> String {
    let (Some(at), Some(now)) = (tz.timestamp_opt(at as i64, 0).single(), tz.timestamp_opt(now as i64, 0).single())
    else {
        return "later".into();
    };
    let (at, now) = (at.naive_local(), now.naive_local());
    let time = at.format("%H:%M");
    match (at.date() - now.date()).num_days() {
        0 => time.to_string(),
        1 => format!("tomorrow {time}"),
        _ => format!("{} {time}", at.weekday()),
    }
}

fn short_reason(e: &str) -> String {
    let lower = e.to_ascii_lowercase();
    if lower.contains("rejected the token") {
        "sign in again".into()
    } else if lower.starts_with("server answered") {
        e.split(':').next().unwrap_or(e).trim().to_string()
    } else if ["sending request", "connect", "timed out", "timeout", "dns"].iter().any(|w| lower.contains(w)) {
        "can't reach the server".into()
    } else {
        "unexpected error".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, Utc};

    const NOW: f64 = 1_748_782_800.0;

    fn signed_in() -> Status {
        Status { username: Some("ada".into()), today_secs: 2.0 * 3600.0 + 14.0 * 60.0, ..Status::default() }
    }

    fn model(s: &Status) -> Vec<Item> {
        model_in(s, NOW, &Utc)
    }

    fn labels(items: &[Item]) -> Vec<&str> {
        items
            .iter()
            .filter_map(|i| match i {
                Item::Label(t) | Item::Entry(_, t) => Some(t.as_str()),
                Item::Separator => None,
            })
            .collect()
    }

    fn has(items: &[Item], a: Action) -> bool {
        items.iter().any(|i| matches!(i, Item::Entry(x, _) if *x == a))
    }

    #[test]
    fn tracking() {
        let items = model(&signed_in());
        assert_eq!(
            labels(&items),
            [
                "Today: 2 h 14 m",
                "Tracking",
                "Open dashboard",
                "Pause for 30 minutes",
                "Pause for 1 hour",
            ]
        );
        assert!(matches!(items[0], Item::Label(_)));
        assert!(matches!(items[1], Item::Label(_)));
    }

    #[test]
    fn not_signed_in_offers_sign_in() {
        let items = model(&Status::default());
        assert_eq!(labels(&items)[..2], ["Today: 0 m", "Not signed in"]);
        assert!(has(&items, Action::SignIn));
        assert!(!has(&items, Action::Resume));
        assert!(!has(&model(&signed_in()), Action::SignIn));
    }

    #[test]
    fn a_rejected_token_offers_sign_in_again() {
        let s = Status { token_rejected: true, upload_error: Some("rejected".into()), ..signed_in() };
        let items = model(&s);
        assert_eq!(labels(&items)[1], "Signed out, sign in to keep syncing");
        assert!(has(&items, Action::SignIn));
        let failed = Status { signin: Signin::Failed { reason: "code expired".into() }, ..s };
        assert_eq!(labels(&model(&failed))[1], "Can't sign in: code expired");
    }

    #[test]
    fn signing_in_shows_the_code() {
        let s = Status { signin: Signin::Waiting { code: "ABCD-EFGH".into() }, ..Status::default() };
        assert_eq!(labels(&model(&s))[1], "Signing in… enter code ABCD-EFGH");
    }

    #[test]
    fn failed_sign_in_is_explained_until_signed_in() {
        let failed = Signin::Failed { reason: "code expired".into() };
        let s = Status { signin: failed.clone(), ..Status::default() };
        assert_eq!(labels(&model(&s))[1], "Can't sign in: code expired");
        let s = Status { signin: failed, ..signed_in() };
        assert_eq!(labels(&model(&s))[1], "Tracking");
    }

    #[test]
    fn paused_shows_the_end_and_offers_resume() {
        let s = Status { paused_until: Some(NOW + 2.0 * 3600.0 + 40.0 * 60.0), ..signed_in() };
        let items = model(&s);
        assert_eq!(labels(&items)[1], "Paused until 15:40");
        assert!(has(&items, Action::Resume));
        assert!(has(&items, Action::Pause(Pause::Hour)));
    }

    #[test]
    fn a_pause_that_has_ended_is_not_shown() {
        let s = Status { paused_until: Some(NOW - 1.0), ..signed_in() };
        let items = model(&s);
        assert_eq!(labels(&items)[1], "Tracking");
        assert!(!has(&items, Action::Resume));
    }

    #[test]
    fn pause_end_is_in_local_time_and_names_the_day_when_not_today() {
        let tz = FixedOffset::east_opt(2 * 3600).unwrap();
        let tomorrow = Status { paused_until: Some(NOW + 9.0 * 3600.0), ..signed_in() };
        assert_eq!(labels(&model_in(&tomorrow, NOW, &tz))[1], "Paused until tomorrow 00:00");
        let later = Status { paused_until: Some(NOW + 3.0 * 86400.0), ..signed_in() };
        assert_eq!(labels(&model_in(&later, NOW, &tz))[1], "Paused until Wed 15:00");
    }

    #[test]
    fn upload_problems_are_short() {
        let say = |e: &str| {
            let s = Status { upload_error: Some(e.into()), ..signed_in() };
            labels(&model(&s))[1].to_string()
        };
        assert_eq!(say("the server rejected the token, run kairos login again"), "Can't upload: sign in again");
        assert_eq!(say("server answered 503 Service Unavailable"), "Can't upload: server answered 503 Service Unavailable");
        assert_eq!(say("server answered 400 Bad Request: {\"error\":\"x\"}"), "Can't upload: server answered 400 Bad Request");
        assert_eq!(
            say("error sending request for url (https://kairos.hackclub.com/api/v1/events)"),
            "Can't upload: can't reach the server"
        );
        assert_eq!(say("saving upload.json: disk full"), "Can't upload: unexpected error");
    }

    #[test]
    fn paused_wins_over_upload_problems() {
        let s = Status { paused_until: Some(NOW + 60.0), upload_error: Some("x".into()), ..signed_in() };
        assert!(labels(&model(&s))[1].starts_with("Paused"));
    }

    #[test]
    fn durations_and_tooltip() {
        assert_eq!(duration(0.0), "0 m");
        assert_eq!(duration(29.0), "0 m");
        assert_eq!(duration(31.0), "1 m");
        assert_eq!(duration(59.0 * 60.0), "59 m");
        assert_eq!(duration(3600.0), "1 h 0 m");
        assert_eq!(duration(-5.0), "0 m");
        assert_eq!(tooltip(&signed_in()), "Kairos: 2 h 14 m today");
    }

    #[test]
    fn action_ids_round_trip() {
        let mut items = model(&Status { paused_until: Some(NOW + 60.0), ..Status::default() });
        items.extend(model(&signed_in()));
        let actions: Vec<_> = items.iter().filter_map(|i| if let Item::Entry(a, _) = i { Some(*a) } else { None }).collect();
        assert_eq!(actions.len(), 8);
        for a in actions {
            assert_eq!(Action::from_id(a.id()), Some(a));
        }
        assert_eq!(Action::from_id("nope"), None);
    }
}
