use kairos_core::credit::{
    agent_runtime, agent_turns, credit_v1, credit_v3, credit_v4, credit_v5, credit_v6, credit_v6_with, credit_v7, credit_v7_with, credit_v8, credit_v8_with,
    credit_v9, credit_v9_with, credit_v10,
    total_credit_secs,
    unattributed_ai_secs, Credit, CreditClass, Rule, V6Params, PAUSE_MAX_SECS, REVISION, TURN_MAX_SECS,
};
use kairos_core::event::{input_bits, verify_chain, App, Ask, Class, Event, Kind, GENESIS};
use kairos_core::project::{normalize_remote, origin_url};
use kairos_core::{human_credits, last_turn, prompt_stats, total_secs, Turn};
use serde_json::json;

fn ev(ts: f64, project: Option<&str>, kind: Kind) -> Event {
    Event::new(ts, project.map(str::to_string), kind)
}

fn focus(ts: f64, class: Class, project: Option<&str>) -> Event {
    ev(ts, project, Kind::Focus { class, app: None })
}

fn present(ts: f64) -> Event {
    ev(ts, None, Kind::Present { input: None, keys: None, injected: None })
}

fn prompt(ts: f64, words: usize, pasted: bool) -> Event {
    ev(ts, Some("o/r"), Kind::Prompt { words, pasted, model: None, session: "s".into(), shape: None, version: None, via: None, permissions: None })
}

fn reply(ts: f64, reply_words: usize) -> Event {
    ev(
        ts,
        Some("o/r"),
        Kind::ReplyDone { session: "s".into(), reply_words, tools: 3, files_changed: 1, model: None },
    )
}

fn agent_start(ts: f64) -> Event {
    ev(ts, Some("o/r"), Kind::AgentStart { session: "s".into() })
}

#[test]
fn watching_the_agent_earns_nothing() {
    let mut events = vec![
        prompt(0.0, 12, false),
        agent_start(5.0),
        focus(0.0, Class::AgentTerminal, Some("o/r")),
    ];
    events.extend((1..=10).map(|m| present(m as f64 * 60.0)));
    events.push(reply(600.0, 200));
    assert!(credit_v1(&events).is_empty());
    assert_eq!(total_secs(&agent_runtime(&events)), 595.0);
}

#[test]
fn an_open_turn_runs_to_the_last_event_the_device_did_not_write_about_itself() {
    let mut events = vec![prompt(0.0, 12, false), agent_start(5.0), present(60.0)];
    events.push(ev(200.0, None, Kind::Receipt { token: "200".into() }));
    events.push(ev(300.0, None, Kind::Slept { secs: 100.0 }));
    events.push(ev(400.0, None, Kind::Screen { locked: true }));
    events.push(ev(500.0, None, Kind::Machine { vm: Some("none".into()), remote: None }));
    assert_eq!(total_secs(&agent_runtime(&events)), 55.0);
}

#[test]
fn browser_and_other_earn_nothing() {
    let events = vec![
        focus(0.0, Class::BrowserUnknown, None),
        present(60.0),
        focus(60.0, Class::Other, None),
        present(120.0),
    ];
    assert!(credit_v1(&events).is_empty());
}

#[test]
fn editor_with_presence_earns() {
    let events = vec![
        focus(0.0, Class::Editor, Some("o/r")),
        present(60.0),
        present(120.0),
        present(180.0),
    ];
    let c = credit_v1(&events);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].class, CreditClass::Editing);
    assert_eq!(c[0].project.as_deref(), Some("o/r"));
    assert_eq!(total_credit_secs(&c), 180.0);
}

#[test]
fn testing_focus_is_credited_as_testing() {
    let events = vec![focus(0.0, Class::Testing, Some("o/r")), present(60.0)];
    let c = credit_v1(&events);
    assert_eq!(c[0].class, CreditClass::Testing);
    assert_eq!(total_credit_secs(&c), 60.0);
}

#[test]
fn focus_change_inside_a_minute_splits_it() {
    let events = vec![
        focus(0.0, Class::AgentTerminal, Some("o/r")),
        focus(40.0, Class::Editor, Some("o/r")),
        present(60.0),
    ];
    assert_eq!(total_credit_secs(&credit_v1(&events)), 20.0);
}

#[test]
fn idle_minutes_earn_nothing() {
    let events = vec![
        focus(0.0, Class::Editor, Some("o/r")),
        present(60.0),
        ev(100.0, None, Kind::Idle {}),
    ];
    assert_eq!(total_credit_secs(&credit_v1(&events)), 60.0);

    let mut late = events.clone();
    late.push(present(130.0));
    assert_eq!(total_credit_secs(&credit_v1(&late)), 90.0);
}

fn turn(start: f64, end: f64, words: usize, paste: bool, reply_words: usize) -> Turn {
    Turn { start, end, words, paste, reply_words, tools: 0, model: None }
}

#[test]
fn prompt_reply_rule_matches_the_analyzer() {
    let cases: [(usize, bool, usize, f64); 6] = [
        (10, false, 230, 1000.0),
        (10, false, 230, 50.0),
        (400, true, 700, 5000.0),
        (400, true, 700, 150.0),
        (2, false, 300, 500.0),
        (3, false, 0, 500.0),
    ];
    for (words, paste, reply_words, gap) in cases {
        let end = 100.0;
        let turns = [turn(0.0, end, 5, false, reply_words), turn(end + gap, end + gap + 10.0, words, paste, 0)];
        let expected = total_secs(&human_credits(&turns, 120.0));

        let events = vec![
            prompt(0.0, 5, false),
            reply(end, reply_words),
            prompt(end + gap, words, paste),
        ];
        let got = total_credit_secs(&credit_v1(&events));
        assert!((got - expected).abs() < 1e-9, "{words} {paste} {reply_words} {gap}: {got} vs {expected}");
    }
}

#[test]
fn short_prompts_earn_nothing() {
    let events = vec![reply(0.0, 300), prompt(60.0, 2, false)];
    assert!(credit_v1(&events).is_empty());
}

#[test]
fn overlapping_credit_is_not_double_counted() {
    let events = vec![
        focus(0.0, Class::Editor, Some("o/r")),
        present(60.0),
        present(120.0),
        reply(0.0, 230),
        prompt(100.0, 10, false),
    ];
    let c = credit_v1(&events);
    assert_eq!(total_credit_secs(&c), 120.0);
    for w in c.windows(2) {
        assert!(w[0].end <= w[1].start);
    }
}

#[test]
fn hash_chain_detects_a_modified_event() {
    let mut prev = GENESIS.to_string();
    let mut lines = Vec::new();
    for e in [prompt(1.0, 8, false), agent_start(2.0), reply(3.0, 40), present(60.0)] {
        let mut e = e;
        e.seal(&prev);
        prev = e.hash.clone();
        lines.push(serde_json::to_string(&e).unwrap());
    }
    let ok = verify_chain(lines.iter().map(String::as_str), Some(GENESIS)).unwrap();
    assert_eq!(ok.events, 4);
    assert_eq!(ok.last_hash, prev);

    let mut tampered = lines.clone();
    tampered[2] = tampered[2].replace("\"reply_words\":40", "\"reply_words\":400");
    let broken = verify_chain(tampered.iter().map(String::as_str), Some(GENESIS)).unwrap_err();
    assert_eq!(broken.line, 3);

    let mut extra = lines.clone();
    extra[1] = extra[1].replace("}", ",\"note\":\"x\"}");
    assert_eq!(verify_chain(extra.iter().map(String::as_str), None).unwrap_err().line, 2);

    let mut removed = lines.clone();
    removed.remove(1);
    assert_eq!(verify_chain(removed.iter().map(String::as_str), None).unwrap_err().line, 2);
}

#[test]
fn events_round_trip_through_json() {
    let mut e = focus(12.5, Class::AgentTerminal, Some("o/r"));
    e.seal(GENESIS);
    let text = serde_json::to_string(&e).unwrap();
    let back: Event = serde_json::from_str(&text).unwrap();
    assert_eq!(back, e);
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["kind"], "focus");
    assert_eq!(v["class"], "agent_terminal");
    assert_eq!(v["v"], 1);
}

#[test]
fn remote_urls_normalize_to_owner_repo() {
    for url in [
        "https://github.com/hackclub/hackatime.git",
        "https://github.com/hackclub/hackatime",
        "https://user:token@github.com/hackclub/hackatime.git",
        "git@github.com:hackclub/hackatime.git",
        "ssh://git@github.com:22/hackclub/hackatime.git",
        "ssh://git@github.com/hackclub/hackatime.git",
    ] {
        assert_eq!(normalize_remote(url).as_deref(), Some("hackclub/hackatime"), "{url}");
    }
    assert_eq!(normalize_remote("git@gitlab.com:group/sub/repo.git").as_deref(), Some("sub/repo"));
}

#[test]
fn origin_is_read_from_git_config() {
    let cfg = "[core]\n\tbare = false\n[remote \"upstream\"]\n\turl = https://x.test/a/b.git\n[remote \"origin\"]\n\turl = git@github.com:me/proj.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n";
    assert_eq!(origin_url(cfg).as_deref(), Some("git@github.com:me/proj.git"));
    assert_eq!(origin_url("[core]\n\tbare = false\n"), None);
}

#[test]
fn prompt_stats_counts_and_flags_pastes() {
    assert_eq!(prompt_stats("fix the login bug please"), (5, false));
    let log = (0..30).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    assert!(prompt_stats(&log).1);
}

#[test]
fn last_turn_reads_only_the_latest_turn() {
    let lines = [
        json!({"type": "user", "message": {"role": "user", "content": "first prompt here"}}),
        json!({"type": "assistant", "message": {"model": "m1", "content": [{"type": "text", "text": "old reply words"}]}}),
        json!({"type": "user", "message": {"role": "user", "content": "second prompt here"}}),
        json!({"type": "assistant", "message": {"model": "m2", "content": [
            {"type": "text", "text": "one two three"},
            {"type": "tool_use", "name": "Edit", "input": {"file_path": "/x/a.rs"}},
            {"type": "tool_use", "name": "Write", "input": {"file_path": "/x/b.rs"}}]}}),
        json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "content": "ok"}]}}),
        json!({"type": "assistant", "message": {"model": "m2", "content": [
            {"type": "tool_use", "name": "Edit", "input": {"file_path": "/x/a.rs"}},
            {"type": "text", "text": "four five"}]}}),
    ];
    let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    let t = last_turn(&text.join("\n"));
    assert_eq!((t.reply_words, t.tools, t.files_changed), (5, 3, 2));
    assert_eq!(t.model.as_deref(), Some("m2"));
}

fn class_secs(credits: &[Credit], class: CreditClass) -> f64 {
    credits.iter().filter(|c| c.class == class).map(|c| c.end - c.start).sum()
}

fn reply_in(ts: f64, project: Option<&str>, reply_words: usize) -> Event {
    ev(
        ts,
        project,
        Kind::ReplyDone { session: "n".into(), reply_words, tools: 0, files_changed: 0, model: None },
    )
}

#[test]
fn typing_is_credited_while_the_agent_runs() {
    let mut events = vec![
        prompt(0.0, 2, false),
        agent_start(5.0),
        focus(0.0, Class::AgentTerminal, Some("o/r")),
        prompt(300.0, 35, false),
        prompt(900.0, 400, true),
        reply(1000.0, 0),
    ];
    events.extend((1..=15).map(|m| present(m as f64 * 60.0)));
    let c = credit_v3(&events);
    assert!((class_secs(&c, CreditClass::Typing) - (60.0 + 60.0 * 60.0 / 35.0)).abs() < 1e-9);
    assert!((total_credit_secs(&c) - class_secs(&c, CreditClass::Typing)).abs() < 1e-9);
    assert_eq!(c[0].project.as_deref(), Some("o/r"));
}

#[test]
fn reading_is_clipped_by_idle() {
    let events = vec![
        reply(0.0, 230),
        present(60.0),
        ev(90.0, None, Kind::Idle {}),
        present(120.0),
        prompt(1000.0, 2, false),
    ];
    let c = credit_v3(&events);
    assert_eq!(class_secs(&c, CreditClass::Reading), 60.0 + 30.0 + 0.0);
    assert_eq!(total_credit_secs(&c), 90.0);
}

#[test]
fn reading_without_a_project_is_not_credited() {
    let events = vec![reply_in(0.0, None, 230), present(60.0), present(120.0), present(180.0)];
    assert!(credit_v3(&events).is_empty());
    assert_eq!(unattributed_ai_secs(&events), 180.0);
    assert_eq!(unattributed_ai_secs(&[reply(0.0, 230), present(60.0)]), 0.0);
}

#[test]
fn terminal_present_is_credited() {
    let events = vec![focus(0.0, Class::Terminal, Some("o/r")), present(60.0), present(120.0)];
    let c = credit_v3(&events);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].class, CreditClass::Terminal);
    assert_eq!(c[0].project.as_deref(), Some("o/r"));
    assert_eq!(total_credit_secs(&c), 120.0);
    assert!(credit_v1(&events).is_empty());
}

#[test]
fn watching_agent_terminal_and_app_earn_nothing_in_v3() {
    let events = vec![
        focus(0.0, Class::AgentTerminal, Some("o/r")),
        present(60.0),
        focus(60.0, Class::AgentApp, Some("o/r")),
        present(120.0),
        focus(120.0, Class::BrowserUnknown, None),
        present(180.0),
        focus(180.0, Class::Other, None),
        present(240.0),
    ];
    assert!(credit_v3(&events).is_empty());
}

#[test]
fn web_agent_present_is_credited() {
    let events = vec![focus(0.0, Class::WebAgent, None), present(60.0), present(120.0)];
    let c = credit_v3(&events);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].class, CreditClass::WebAgent);
    assert_eq!(c[0].project, None);
    assert_eq!(total_credit_secs(&c), 120.0);
}

#[test]
fn v3_does_not_double_count_overlaps() {
    let events = vec![
        focus(0.0, Class::Editor, Some("o/r")),
        present(60.0),
        present(120.0),
        reply(0.0, 230),
        prompt(100.0, 10, false),
    ];
    let c = credit_v3(&events);
    assert!((total_credit_secs(&c) - 120.0).abs() < 1e-9);
    for w in c.windows(2) {
        assert!(w[0].end <= w[1].start);
    }
    let typing = 60.0 * 10.0 / 35.0;
    assert!((class_secs(&c, CreditClass::Typing) - typing).abs() < 1e-9);
    assert!((class_secs(&c, CreditClass::Editing) - (120.0 - typing)).abs() < 1e-9);
    assert_eq!(class_secs(&c, CreditClass::Reading), 0.0);
}

#[test]
fn new_focus_classes_round_trip_and_old_logs_parse() {
    for (class, name) in [
        (Class::Terminal, "terminal"),
        (Class::AgentApp, "agent_app"),
        (Class::WebAgent, "web_agent"),
        (Class::Editor, "editor"),
    ] {
        let text = serde_json::to_string(&focus(1.0, class, None)).unwrap();
        assert!(text.contains(&format!("\"class\":\"{name}\"")), "{text}");
        assert_eq!(serde_json::from_str::<Event>(&text).unwrap().kind, Kind::Focus { class, app: None });
    }
    let old = r#"{"v":1,"ts":1.0,"kind":"focus","class":"browser_unknown","prev":"","hash":""}"#;
    assert!(serde_json::from_str::<Event>(old).is_ok());
}

fn presents(from: f64, to: f64) -> Vec<Event> {
    (0..)
        .map(|i| from + 60.0 * i as f64)
        .take_while(|t| *t <= to)
        .map(present)
        .collect()
}

fn secs_by(credits: &[Credit], class: CreditClass, project: Option<&str>) -> f64 {
    credits
        .iter()
        .filter(|c| c.class == class && c.project.as_deref() == project)
        .map(|c| c.end - c.start)
        .sum()
}

#[test]
fn v4_docs_borrow_the_project_worked_on_just_before() {
    let mut events = vec![focus(0.0, Class::Editor, Some("o/r")), focus(120.0, Class::Docs, None)];
    events.extend(presents(60.0, 600.0));
    let c = credit_v4(&events);
    assert_eq!(secs_by(&c, CreditClass::Editing, Some("o/r")), 120.0);
    assert_eq!(secs_by(&c, CreditClass::Docs, Some("o/r")), 480.0);
    assert!(credit_v3(&events).iter().all(|c| c.class != CreditClass::Docs));
}

#[test]
fn v4_docs_long_after_project_work_count_as_other() {
    let mut events = vec![focus(0.0, Class::Editor, Some("o/r")), focus(60.0, Class::Docs, None)];
    events.extend(presents(60.0, 1200.0));
    let c = credit_v4(&events);
    let borrowed = secs_by(&c, CreditClass::Docs, Some("o/r"));
    let other = secs_by(&c, CreditClass::Docs, None);
    assert_eq!(borrowed + other, 1140.0);
    assert!(borrowed <= 900.0 + 60.0, "borrowed {borrowed}");
    assert!(other > 0.0);
}

#[test]
fn v4_chats_outside_projects_count_as_other() {
    let mut events = vec![
        ev(0.0, None, Kind::ReplyDone { session: "h".into(), reply_words: 230, tools: 0, files_changed: 0, model: None }),
        ev(200.0, None, Kind::Prompt { words: 35, pasted: false, model: None, session: "h".into(), shape: None, version: None, via: None, permissions: None }),
    ];
    events.extend(presents(60.0, 240.0));
    let c = credit_v4(&events);
    assert!(secs_by(&c, CreditClass::Typing, None) > 0.0);
    assert!(secs_by(&c, CreditClass::Reading, None) > 0.0);
    assert!(c.iter().all(|c| c.project.is_none()));
    assert!(credit_v3(&events).is_empty());
}

#[test]
fn v4_pause_removes_credit_until_it_ends() {
    let mut events = vec![
        focus(0.0, Class::Editor, Some("o/r")),
        ev(120.0, None, Kind::Pause { until: 400.0 }),
    ];
    events.extend(presents(60.0, 600.0));
    let c = credit_v4(&events);
    assert_eq!(total_credit_secs(&c), 600.0 - 280.0);
    assert!(c.iter().all(|c| c.end <= 120.0 || c.start >= 400.0));
}

#[test]
fn v4_resume_ends_a_pause_early() {
    let mut events = vec![
        focus(0.0, Class::Editor, Some("o/r")),
        ev(120.0, None, Kind::Pause { until: 3600.0 }),
        ev(300.0, None, Kind::Resume {}),
    ];
    events.extend(presents(60.0, 600.0));
    assert_eq!(total_credit_secs(&credit_v4(&events)), 600.0 - 180.0);
}

#[test]
fn v5_credits_no_docs_web_agent_or_borrowed_time() {
    let mut events = vec![
        focus(0.0, Class::Editor, Some("o/r")),
        focus(120.0, Class::Docs, None),
        focus(300.0, Class::WebAgent, None),
        focus(480.0, Class::Other, None),
    ];
    events.extend(presents(60.0, 600.0));
    let c = credit_v5(&events);
    assert_eq!(secs_by(&c, CreditClass::Editing, Some("o/r")), 120.0);
    assert_eq!(total_credit_secs(&c), 120.0);
    assert!(credit_v4(&events).iter().any(|c| c.class == CreditClass::Docs));
    assert!(credit_v4(&events).iter().any(|c| c.class == CreditClass::WebAgent));
}

#[test]
fn v5_still_credits_testing_tied_to_a_project() {
    let mut events = vec![focus(0.0, Class::Testing, Some("o/app"))];
    events.extend(presents(60.0, 360.0));
    let c = credit_v5(&events);
    assert_eq!(secs_by(&c, CreditClass::Testing, Some("o/app")), 360.0);
    assert_eq!(total_credit_secs(&c), 360.0);
}

#[test]
fn v5_matches_v4_on_typing_reading_editing_and_terminal() {
    let mut events = vec![
        focus(0.0, Class::Editor, Some("o/r")),
        focus(300.0, Class::Terminal, Some("o/r")),
        focus(600.0, Class::AgentTerminal, Some("o/r")),
        reply(700.0, 230),
        prompt(900.0, 40, false),
        ev(1000.0, None, Kind::ReplyDone { session: "h".into(), reply_words: 230, tools: 0, files_changed: 0, model: None }),
        ev(1200.0, None, Kind::Prompt { words: 35, pasted: false, model: None, session: "h".into(), shape: None, version: None, via: None, permissions: None }),
    ];
    events.extend(presents(60.0, 1300.0));
    let c = credit_v5(&events);
    assert_eq!(c, credit_v4(&events));
    for class in [CreditClass::Typing, CreditClass::Reading, CreditClass::Editing, CreditClass::Terminal] {
        assert!(c.iter().any(|c| c.class == class), "{class:?}");
    }
    assert!(secs_by(&c, CreditClass::Typing, None) > 0.0, "credit with no project is kept as Other");
}

#[test]
fn v5_pause_removes_credit_until_it_ends() {
    let mut events = vec![
        focus(0.0, Class::Editor, Some("o/r")),
        ev(120.0, None, Kind::Pause { until: 400.0 }),
    ];
    events.extend(presents(60.0, 600.0));
    let c = credit_v5(&events);
    assert_eq!(total_credit_secs(&c), 600.0 - 280.0);
    assert!(c.iter().all(|c| c.end <= 120.0 || c.start >= 400.0));
}

#[test]
fn unknown_kinds_and_classes_still_parse() {
    let e: Event = serde_json::from_str(r#"{"v":1,"ts":5.0,"kind":"hologram","depth":3,"prev":"","hash":""}"#).unwrap();
    assert_eq!(e.kind, Kind::Unknown);
    let f: Event = serde_json::from_str(r#"{"v":1,"ts":5.0,"kind":"focus","class":"vr_headset","prev":"","hash":""}"#).unwrap();
    assert_eq!(f.kind, Kind::Focus { class: Class::Unknown, app: None });
}

#[test]
fn v4_two_agents_in_two_projects_at_once_are_credited_once_each() {
    let in_project = |ts: f64, project: &str, kind: Kind| {
        let mut e = ev(ts, Some(project), kind);
        e.harness = Some(if project == "a/a" { "claude-code" } else { "codex" }.into());
        e
    };
    let prompt =
        |session: &str| Kind::Prompt { words: 35, pasted: false, model: None, session: session.into(), shape: None, version: None, via: None, permissions: None };
    let start = |session: &str| Kind::AgentStart { session: session.into() };
    let done = |session: &str| Kind::ReplyDone {
        session: session.into(),
        reply_words: 230,
        tools: 2,
        files_changed: 1,
        model: None,
    };
    let mut events = vec![
        in_project(100.0, "a/a", prompt("sa")),
        in_project(105.0, "a/a", start("sa")),
        in_project(200.0, "b/b", prompt("sb")),
        in_project(205.0, "b/b", start("sb")),
        in_project(300.0, "a/a", done("sa")),
        in_project(320.0, "b/b", done("sb")),
    ];
    events.extend(presents(60.0, 600.0));
    let c = credit_v4(&events);

    assert_eq!(secs_by(&c, CreditClass::Typing, Some("a/a")), 60.0);
    assert_eq!(secs_by(&c, CreditClass::Typing, Some("b/b")), 60.0);
    assert_eq!(secs_by(&c, CreditClass::Reading, Some("a/a")), 180.0);
    assert_eq!(secs_by(&c, CreditClass::Reading, Some("b/b")), 20.0);
    assert!(c.iter().all(|c| matches!(c.project.as_deref(), Some("a/a" | "b/b"))));
    let mut spans: Vec<(f64, f64)> = c.iter().map(|c| (c.start, c.end)).collect();
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    assert!(spans.windows(2).all(|w| w[0].1 <= w[1].0), "a second of real time is credited once");
    assert_eq!(total_credit_secs(&c), 60.0 + 60.0 + 200.0);
    assert_eq!(total_secs(&agent_runtime(&events)), 320.0 - 105.0);
}

#[test]
fn harness_is_part_of_the_hash() {
    let mut e = ev(1.0, Some("o/r"), Kind::AgentStart { session: "s".into() });
    e.harness = Some("cursor".into());
    e.seal(GENESIS);
    let line = serde_json::to_string(&e).unwrap();
    assert!(line.contains(r#""harness":"cursor""#));
    assert!(verify_chain(std::iter::once(line.as_str()), Some(GENESIS)).is_ok());
    let tampered = line.replace("cursor", "codex");
    assert!(verify_chain(std::iter::once(tampered.as_str()), Some(GENESIS)).is_err());
}

#[test]
fn project_facts_change_no_credit_and_anchor_no_project() {
    let mut events = vec![focus(0.0, Class::Docs, None)];
    events.extend(presents(60.0, 300.0));
    let before = credit_v4(&events);
    let mut with_git = events.clone();
    with_git.push(ev(10.0, Some("o/r"), Kind::Commit { sha: "a".repeat(40), files: 3, added: 40, removed: 2, authored: None, committed: None, via: None, changes: vec![], replaces: None, not_before: None }));
    with_git.push(ev(20.0, Some("o/r"), Kind::Diff { files: 1, added: 5, removed: 0, changes: vec![] }));
    with_git.push(ev(30.0, Some("o/r"), Kind::ProjectAlias { from: "r".into() }));
    with_git.push(ev(40.0, Some("o/r"), Kind::Unknown));
    with_git.push(ev(50.0, Some("o/r"), Kind::AgentEdit { session: "s".into(), path: "src/a.rs".into(), added: 9, removed: 1 }));
    with_git.push(ev(55.0, Some("o/r"), Kind::AgentCommit { session: "s".into(), sha: Some("abc1234".into()) }));
    with_git.push(ev(58.0, Some("o/r"), Kind::Heartbeat {
        at: 57.0,
        editor: Some("vscode".into()),
        language: Some("Rust".into()),
        category: Some("coding".into()),
        path: Some("src/a.rs".into()),
        write: true,
        ai_lines: None,
        human_lines: Some(3),
    }));
    let after = credit_v4(&with_git);
    assert_eq!(before, after);
    assert!(after.iter().all(|c| c.project.is_none()));
}

#[test]
fn unread_replies_in_parallel_sessions_split_the_same_way_every_time() {
    let mut events: Vec<Event> = [("a", "proj/a", 100.0), ("b", "proj/b", 110.0), ("c", "proj/c", 120.0)]
        .into_iter()
        .map(|(s, p, t)| ev(t, Some(p), Kind::ReplyDone { session: s.into(), reply_words: 500, tools: 1, files_changed: 0, model: None }))
        .collect();
    events.push(focus(130.0, Class::AgentTerminal, Some("proj/a")));
    events.extend(presents(180.0, 300.0));
    let first = credit_v4(&events);
    let reading: Vec<&Credit> = first.iter().filter(|c| c.class == CreditClass::Reading).collect();
    assert!(!reading.is_empty());
    assert!(reading.iter().all(|c| c.project.as_deref() == Some("proj/a")), "{reading:?}");
    for _ in 0..40 {
        assert_eq!(credit_v4(&events), first);
    }
}

fn pinned_events() -> Vec<Event> {
    let mut events = vec![focus(0.0, Class::Editor, Some("o/r"))];
    events.extend(presents(60.0, 900.0));
    events.push(ev(300.0, Some("o/r"), Kind::FileSave { count: 2, paths: vec![] }));
    events.extend([prompt(920.0, 40, false), agent_start(925.0), focus(930.0, Class::AgentTerminal, Some("o/r"))]);
    events.extend(presents(990.0, 1500.0));
    events.push(reply(1400.0, 600));
    events.push(focus(1500.0, Class::Docs, None));
    events.extend(presents(1560.0, 1800.0));
    events.push(focus(1800.0, Class::WebAgent, None));
    events.extend(presents(1860.0, 2100.0));
    events.push(ev(2100.0, None, Kind::Idle {}));
    events.extend([prompt(2400.0, 900, true), agent_start(2410.0), reply(2700.0, 50), focus(2705.0, Class::Terminal, Some("x/y"))]);
    events.extend(presents(2760.0, 3000.0));
    events.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    events
}

#[test]
fn credit_v5_output_is_unchanged() {
    let events = pinned_events();
    let out = format!("{:?}{:?}", credit_v5(&events), agent_runtime(&events));
    assert_eq!(blake3::hash(out.as_bytes()).to_hex().to_string(), "5c85a17f4b95eee3112e5d1629b03e4f606153cbc05a7f703cc37925395f7707", "{out}");
}

/// Fails whenever credit for the same events changes. If that's intended, bump `credit::REVISION`
/// and paste the new digest, so servers rebuild what they stored under the old code.
// pinned_events plus what only v8 reads: an editor heartbeat naming another project, and a short gap with a trip in it.
fn pinned_v8_events() -> Vec<Event> {
    let mut events = pinned_events();
    events.push(ev(200.0, Some("a/b"), Kind::Heartbeat { at: 200.0, editor: Some("vscode".into()), language: None, category: None, path: None, write: false, ai_lines: None, human_lines: Some(1) }));
    events.extend([focus(3010.0, Class::BrowserUnknown, None), focus(3025.0, Class::Terminal, Some("x/y")), present(3120.0), present(3180.0)]);
    // An agent turn watched with no input after the prompt, then a terminal gap with one key press before it.
    events.extend([
        focus(4000.0, Class::AgentTerminal, Some("o/r")),
        present_with(4060.0, &input_bits(4060.0, &active(4000.0, 30))),
        prompt(4030.0, 20, false),
        reply(4200.0, 50),
        present_with(4240.0, &input_bits(4240.0, &active(4210.0, 6))),
        focus(5000.0, Class::Terminal, Some("x/y")),
        present_with(5060.0, &input_bits(5060.0, &[5030.5])),
        present_with(5240.0, &input_bits(5240.0, &active(5200.0, 5))),
    ]);
    events.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    events
}

// pinned_v8_events plus what only v9 reads: a prompt typed in an agent app whose window shows no project.
fn pinned_v9_events() -> Vec<Event> {
    let mut events = pinned_v8_events();
    events.extend(in_the_codex_app(6000.0, App::CodexApp, None, 100));
    events.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    events
}

// pinned_v9_events plus what only v10 reads: a terminal of the turn's project in front while the agent works, the person
// present but not touching anything after the prompt.
fn pinned_v10_events() -> Vec<Event> {
    let mut events = pinned_v9_events();
    let quiet = "0".repeat(15);
    events.extend([
        focus(7000.0, Class::Terminal, Some("o/r")),
        present_with(7060.0, &input_bits(7060.0, &active(7000.0, 30))),
        prompt(7030.0, 20, false),
        present_with(7120.0, &quiet),
        present_with(7180.0, &quiet),
        reply(7600.0, 0),
    ]);
    events.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    events
}

#[test]
fn stored_credit_is_pinned_to_the_revision() {
    let events = pinned_v10_events();
    let out = format!("{:?}{:?}", Rule::CURRENT.apply(&events), agent_turns(&events));
    assert_eq!((REVISION, blake3::hash(out.as_bytes()).to_hex().to_string()), (10, "a9682483b71c6ca5c1553b32ff5fa33ad5033a9d5aa2e54e2e98f73e08377088".to_string()), "{out}");
}

fn secs_in(credits: &[Credit], class: CreditClass, lo: f64, hi: f64) -> f64 {
    credits.iter().filter(|c| c.class == class).map(|c| (c.end.min(hi) - c.start.max(lo)).max(0.0)).sum()
}

fn present_with(ts: f64, bits: &str) -> Event {
    ev(ts, None, Kind::Present { input: Some(bits.into()), keys: None, injected: None })
}

fn reply_of(ts: f64, session: &str, project: Option<&str>, reply_words: usize) -> Event {
    ev(ts, project, Kind::ReplyDone { session: session.into(), reply_words, tools: 0, files_changed: 0, model: None })
}

#[test]
fn v9_is_the_live_rule() {
    assert_eq!(Rule::parse("v6"), Some(Rule::V6));
    assert_eq!(Rule::parse("credit_v6"), Some(Rule::V6));
    assert_eq!(Rule::V6.name(), "credit_v6");
    assert_eq!(Rule::parse("v7"), Some(Rule::V7));
    assert_eq!(Rule::V7.name(), "credit_v7");
    assert_eq!(Rule::parse("v8"), Some(Rule::V8));
    assert_eq!(Rule::V8.name(), "credit_v8");
    assert_eq!(Rule::parse("v9"), Some(Rule::V9));
    assert_eq!(Rule::V9.name(), "credit_v9");
    assert_eq!(Rule::CURRENT, Rule::V10);
    let events = vec![focus(0.0, Class::Editor, Some("o/r")), present(60.0), present(120.0)];
    assert_eq!(Rule::V6.apply(&events), credit_v6(&events));
    assert_eq!(credit_v6_with(&events, &V6Params::default()), credit_v6(&events));
    assert_eq!(Rule::V7.apply(&events), credit_v7(&events));
    assert_eq!(credit_v7_with(&events, &V6Params::default()), credit_v7(&events));
    assert_eq!(Rule::V8.apply(&events), credit_v8(&events));
    assert_eq!(credit_v8_with(&events, &V6Params::default()), credit_v8(&events));
    assert_eq!(Rule::V9.apply(&events), credit_v9(&events));
    assert_eq!(credit_v9_with(&events, &V6Params::default()), credit_v9(&events));
}

// An agent app window in front from `t0` showing `shows`, the person's input every second for `typing` seconds, then a
// 35 word prompt for o/r its log says was typed in the Codex app, and a 230 word reply 100 s later read in the window.
fn in_the_codex_app(t0: f64, front: App, shows: Option<&str>, typing: u32) -> Vec<Event> {
    let mut events = vec![ev(t0, shows, Kind::Focus { class: Class::AgentApp, app: Some(front) })];
    let inputs = active(t0, typing);
    events.extend((1..=10).map(|m| t0 + 60.0 * f64::from(m)).map(|t| present_with(t, &input_bits(t, &inputs))));
    events.push(ev(t0 + 100.0, Some("o/r"), Kind::Prompt { words: 35, pasted: false, model: None, session: "app".into(), shape: None, version: None, via: Some(App::CodexApp), permissions: None }));
    events.push(reply_of(t0 + 200.0, "app", Some("o/r"), 230));
    events
}

#[test]
fn v9_a_prompt_typed_in_an_agent_app_fits_its_window_whatever_project_it_shows() {
    for shows in [None, Some("x/y")] {
        let events = in_the_codex_app(0.0, App::CodexApp, shows, 100);
        let (v8, v9) = (credit_v8(&events), credit_v9(&events));
        assert_eq!((secs_by(&v8, CreditClass::Typing, Some("o/r")), secs_by(&v8, CreditClass::Reading, Some("o/r"))), (0.0, 0.0), "{shows:?}");
        assert_eq!(secs_by(&v9, CreditClass::Typing, Some("o/r")), 60.0, "{shows:?}: 35 words at 35 wpm");
        assert_eq!(secs_by(&v9, CreditClass::Reading, Some("o/r")), 180.0, "{shows:?}: 230 words at 230 wpm and 120 s to think");
    }
}

#[test]
fn v9_needs_the_app_the_log_names_in_front_with_input_just_before_the_prompt() {
    let nothing = |events: &[Event]| {
        let c = credit_v9(events);
        (secs_by(&c, CreditClass::Typing, Some("o/r")), secs_by(&c, CreditClass::Reading, Some("o/r")))
    };
    assert_eq!(nothing(&in_the_codex_app(0.0, App::ClaudeDesktop, None, 100)), (0.0, 0.0), "another app in front");
    assert_eq!(nothing(&in_the_codex_app(0.0, App::CodexApp, None, 90)), (0.0, 0.0), "no input in the 5 s before: sent from elsewhere");
    let mut terminal = in_the_codex_app(0.0, App::CodexApp, None, 100);
    terminal[0] = focus(0.0, Class::Terminal, None);
    assert_eq!(nothing(&terminal), (0.0, 0.0), "not an agent app window");
}

#[test]
fn v9_is_v8_when_no_prompt_says_where_it_was_typed() {
    let mut events = pinned_v8_events();
    let mut app = in_the_codex_app(6000.0, App::CodexApp, None, 100);
    if let Kind::Prompt { via, .. } = &mut app[11].kind {
        *via = None;
    }
    events.extend(app);
    events.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    assert_eq!(credit_v9(&events), credit_v8(&events));
}

#[test]
fn v9_an_answer_in_a_session_typed_in_the_app_fits_its_window() {
    let mut events = in_the_codex_app(0.0, App::CodexApp, None, 100);
    events.push(ev(530.0, Some("o/r"), Kind::Answered { session: "app".into(), asked: 500.0, why: Ask::Question, shown_words: 46, answer_words: 0 }));
    let answer = |c: &[Credit]| secs_in(c, CreditClass::Reading, 500.0, 530.0);
    assert_eq!(answer(&credit_v8(&events)), 0.0);
    assert_eq!(answer(&credit_v9(&events)), 30.0);
}

#[test]
fn v6_reading_pauses_outside_work_windows_and_resumes_before_the_submit() {
    // 230 words and 120 s of thought: a 180 s budget.
    let mut events = vec![focus(0.0, Class::AgentTerminal, Some("o/r")), reply(0.0, 230)];
    events.extend(presents(60.0, 600.0));
    events.extend([focus(60.0, Class::Other, None), focus(90.0, Class::BrowserUnknown, None), focus(120.0, Class::AgentTerminal, Some("o/r"))]);
    let open = credit_v6(&events);
    assert_eq!(class_secs(&open, CreditClass::Reading), 180.0);
    assert_eq!(secs_in(&open, CreditClass::Reading, 60.0, 120.0), 0.0);
    assert_eq!(secs_in(&open, CreditClass::Reading, 0.0, 60.0), 60.0);
    assert_eq!(open.iter().map(|c| c.end).fold(0.0, f64::max), 240.0);

    let mut submitted = events.clone();
    submitted.push(prompt(200.0, 2, false));
    let c = credit_v6(&submitted);
    assert_eq!(class_secs(&c, CreditClass::Reading), 60.0 + 80.0);
    assert_eq!(secs_in(&c, CreditClass::Reading, 200.0, f64::MAX), 0.0);

    let v5 = credit_v5(&submitted);
    assert!(class_secs(&v5, CreditClass::Reading) > 140.0, "v5 reads on while the other window is in front");
}

#[test]
fn v6_reading_with_two_pending_replies_goes_to_the_project_in_front() {
    let in_project = |ts: f64, session: &str, project: &str| reply_of(ts, session, Some(project), 0);
    let mut events = vec![in_project(0.0, "a", "p/a"), in_project(0.0, "b", "p/b"), focus(0.0, Class::AgentTerminal, Some("p/b"))];
    events.extend(presents(60.0, 600.0));
    events.push(focus(100.0, Class::AgentTerminal, Some("p/a")));
    let c = credit_v6(&events);
    assert_eq!(secs_by(&c, CreditClass::Reading, Some("p/b")), 100.0);
    assert_eq!(secs_by(&c, CreditClass::Reading, Some("p/a")), 120.0);
    assert_eq!(secs_in(&c, CreditClass::Reading, 0.0, 100.0), 100.0);

    // A reply without a project takes any window, but the agent window's own project comes first.
    let mut events = vec![reply_of(0.0, "a", None, 0), reply_of(10.0, "b", Some("p/b"), 0), focus(0.0, Class::AgentTerminal, Some("p/b"))];
    events.extend(presents(60.0, 600.0));
    let c = credit_v6(&events);
    assert_eq!(secs_by(&c, CreditClass::Reading, Some("p/b")), 120.0);
    assert_eq!(secs_by(&c, CreditClass::Reading, None), 120.0);
    assert_eq!(secs_in(&c, CreditClass::Reading, 10.0, 130.0), 120.0);
    assert!(c.iter().filter(|c| c.project.as_deref() == Some("p/b")).all(|c| c.start >= 10.0 && c.end <= 130.0));

    // Not an agent window: the oldest claims it.
    let mut events = vec![in_project(0.0, "a", "p/a"), in_project(10.0, "b", "p/b"), focus(0.0, Class::Docs, None)];
    events.extend(presents(60.0, 600.0));
    let c = credit_v6(&events);
    assert_eq!(secs_in(&c.iter().filter(|c| c.project.as_deref() == Some("p/a")).cloned().collect::<Vec<_>>(), CreditClass::Reading, 0.0, 120.0), 120.0);
    assert_eq!(secs_by(&c, CreditClass::Reading, Some("p/b")), 120.0);
}

fn agent_window(project: &str) -> Vec<Event> {
    vec![focus(0.0, Class::AgentTerminal, Some(project))]
}

#[test]
fn v6_typing_is_placed_in_agent_window_time_only() {
    // 70 words is 120 s of typing; the last 60 s before the submit were spent in another window.
    let mut events = agent_window("o/r");
    events.extend(presents(60.0, 660.0));
    events.push(focus(540.0, Class::Other, None));
    events.push(prompt(600.0, 70, false));
    let c = credit_v6(&events);
    assert_eq!(class_secs(&c, CreditClass::Typing), 120.0);
    assert_eq!(secs_in(&c, CreditClass::Typing, 420.0, 540.0), 120.0);

    // No agent window at all in the lookback: nothing, pasted or not.
    for pasted in [false, true] {
        let mut events = vec![focus(0.0, Class::Other, None)];
        events.extend(presents(60.0, 660.0));
        events.push(prompt(600.0, 70, pasted));
        assert_eq!(class_secs(&credit_v6(&events), CreditClass::Typing), 0.0);
    }

    // A browser tab the extension never classified is not an agent window either.
    let mut events = vec![focus(0.0, Class::BrowserUnknown, None)];
    events.extend(presents(60.0, 660.0));
    events.push(prompt(600.0, 70, false));
    assert_eq!(class_secs(&credit_v6(&events), CreditClass::Typing), 0.0);

    // Another project's agent window does not carry this prompt.
    let mut events = agent_window("x/y");
    events.extend(presents(60.0, 660.0));
    events.push(prompt(600.0, 70, false));
    assert_eq!(class_secs(&credit_v6(&events), CreditClass::Typing), 0.0);
}

#[test]
fn v6_typing_stops_at_the_lookback_and_at_the_sessions_previous_prompt() {
    let mut events = agent_window("o/r");
    events.extend(presents(60.0, 3000.0));
    events.push(prompt(2000.0, 900, false));
    let c = credit_v6(&events);
    assert_eq!(class_secs(&c, CreditClass::Typing), 600.0);
    assert_eq!(secs_in(&c, CreditClass::Typing, 1400.0, 2000.0), 600.0);
    let tuned = credit_v6_with(&events, &V6Params { lookback_secs: 200.0, ..V6Params::default() });
    assert_eq!(class_secs(&tuned, CreditClass::Typing), 200.0);

    let mut events = agent_window("o/r");
    events.extend(presents(60.0, 420.0));
    events.extend([prompt(300.0, 3, false), prompt(400.0, 70, false)]);
    let c = credit_v6(&events);
    assert_eq!(secs_in(&c, CreditClass::Typing, 300.0, 400.0), 100.0);
    assert!((class_secs(&c, CreditClass::Typing) - (100.0 + 60.0 * 3.0 / 35.0)).abs() < 1e-6);
}

#[test]
fn v6_input_bits_limit_typing_and_old_presents_fall_back_to_presence() {
    let bits = |seconds: std::ops::Range<u32>| {
        let inputs: Vec<f64> = seconds.map(|i| 540.0 + f64::from(i) + 0.5).collect();
        input_bits(600.0, &inputs)
    };
    // 35 words is 60 s of typing, but only the first 30 s of the minute had input.
    let mut events = agent_window("o/r");
    events.extend((1..10).map(|m| present_with(m as f64 * 60.0, &input_bits(m as f64 * 60.0, &[]))));
    events.push(present_with(600.0, &bits(0..30)));
    events.push(prompt(600.0, 35, false));
    let c = credit_v6(&events);
    assert_eq!(class_secs(&c, CreditClass::Typing), 30.0);
    assert_eq!(secs_in(&c, CreditClass::Typing, 540.0, 570.0), 30.0);

    // Seconds with input elsewhere in the lookback make up the rest, never seconds without.
    let mut events = agent_window("o/r");
    events.extend((1..10).map(|m| present_with(m as f64 * 60.0, &input_bits(m as f64 * 60.0, &[m as f64 * 60.0 - 10.5]))));
    events.push(present_with(600.0, &bits(0..30)));
    events.push(prompt(600.0, 35, false));
    let c = credit_v6(&events);
    assert_eq!(class_secs(&c, CreditClass::Typing), 30.0 + 9.0);
    assert_eq!(secs_in(&c, CreditClass::Typing, 540.0, 570.0), 30.0);

    // Older events carry no bits: presence alone counts.
    let mut events = agent_window("o/r");
    events.extend(presents(60.0, 600.0));
    events.push(prompt(600.0, 35, false));
    assert_eq!(class_secs(&credit_v6(&events), CreditClass::Typing), 60.0);

    // A present with bits and one without covering other time: bits only bind where they exist.
    let mut events = agent_window("o/r");
    events.extend(presents(60.0, 540.0));
    events.push(present_with(600.0, &bits(0..10)));
    events.push(prompt(600.0, 35, false));
    let c = credit_v6(&events);
    assert_eq!(class_secs(&c, CreditClass::Typing), 60.0);
    assert_eq!(secs_in(&c, CreditClass::Typing, 540.0, 550.0), 10.0);
    assert_eq!(secs_in(&c, CreditClass::Typing, 480.0, 540.0), 50.0);
}

fn answered(ts: f64, asked: f64, why: Ask, shown_words: usize, answer_words: usize) -> Event {
    ev(ts, Some("o/r"), Kind::Answered { session: "s".into(), asked, why, shown_words, answer_words })
}

fn waiting_at_the_agent(until: f64) -> Vec<Event> {
    let mut events = agent_window("o/r");
    events.extend(presents(60.0, until));
    events.push(present(until));
    events
}

#[test]
fn v6_an_answer_earns_a_budget_from_what_was_shown_and_typed() {
    let credit = |shown: usize, typed: usize, waited: f64| {
        let mut events = waiting_at_the_agent(waited);
        events.push(answered(waited, 0.0, Ask::Question, shown, typed));
        let c = credit_v6(&events);
        assert!(c.iter().all(|c| c.class == CreditClass::Reading && c.project.as_deref() == Some("o/r")));
        assert_eq!(secs_in(&c, CreditClass::Reading, waited - class_secs(&c, CreditClass::Reading), waited), class_secs(&c, CreditClass::Reading), "the latest seconds");
        class_secs(&c, CreditClass::Reading)
    };
    let near = |a: f64, b: f64| assert!((a - b).abs() < 1e-6, "{a} vs {b}");
    near(credit(0, 0, 300.0), 30.0);
    near(credit(12, 0, 300.0), 30.0 + 60.0 * 12.0 / 230.0);
    near(credit(230, 0, 300.0), 90.0);
    near(credit(230, 35, 300.0), 150.0);
    near(credit(2300, 0, 1000.0), 600.0);
    near(credit(100_000, 5000, 1000.0), 600.0);
    near(credit(2300, 0, 400.0), 400.0);
}

#[test]
fn v6_an_answer_needs_presence_an_agent_window_and_a_wait() {
    let one = |events: Vec<Event>| class_secs(&credit_v6(&events), CreditClass::Reading);
    let mut ok = waiting_at_the_agent(300.0);
    ok.push(answered(300.0, 0.0, Ask::Permission, 5, 0));
    assert!(one(ok.clone()) > 0.0);

    let mut asleep = agent_window("o/r");
    asleep.extend(presents(60.0, 120.0));
    asleep.push(answered(300.0, 0.0, Ask::Question, 5, 0));
    assert!(one(asleep.clone()) > 0.0);
    assert_eq!(secs_in(&credit_v6(&asleep), CreditClass::Reading, 120.0, 300.0), 0.0);

    for class in [Class::Other, Class::BrowserUnknown, Class::Docs, Class::Testing, Class::Terminal] {
        let mut events = vec![focus(0.0, class, Some("o/r"))];
        events.extend(presents(60.0, 300.0));
        events.push(answered(300.0, 0.0, Ask::Question, 5, 0));
        assert_eq!(one(events), 0.0, "{class:?}");
    }
    let mut elsewhere = agent_window("x/y");
    elsewhere.extend(presents(60.0, 300.0));
    elsewhere.push(answered(300.0, 0.0, Ask::Question, 5, 0));
    assert_eq!(one(elsewhere), 0.0);

    for (ts, asked) in [(300.0, 300.0), (300.0, 301.0), (300.0, f64::MAX)] {
        let mut events = waiting_at_the_agent(300.0);
        events.push(answered(ts, asked, Ask::Question, 5, 0));
        assert_eq!(one(events), 0.0, "asked {asked}");
    }

    let mut left = agent_window("o/r");
    left.extend(presents(60.0, 300.0));
    left.push(focus(250.0, Class::Other, None));
    left.push(answered(300.0, 0.0, Ask::Question, 5, 0));
    let c = credit_v6(&left);
    assert_eq!(secs_in(&c, CreditClass::Reading, 250.0, 300.0), 0.0);
    assert!((class_secs(&c, CreditClass::Reading) - (30.0 + 60.0 * 5.0 / 230.0)).abs() < 1e-6);
}

#[test]
fn v6_a_reply_can_be_read_until_it_expires_or_the_next_prompt() {
    // 2300 words and 120 s of thought: a 720 s budget, far more than the 260 s left before the reply expires at 8200.
    let at_the_agent = |reply_at: f64| {
        let mut events = agent_window("o/r");
        events.push(reply(reply_at, 2300));
        events.extend(presents(8000.0, 9000.0));
        events
    };
    let reading = |events: &[Event]| class_secs(&credit_v6(events), CreditClass::Reading);
    assert_eq!(reading(&at_the_agent(1000.0)), 260.0);
    assert_eq!(secs_in(&credit_v6(&at_the_agent(1000.0)), CreditClass::Reading, 8200.0, 9000.0), 0.0);
    assert_eq!(reading(&at_the_agent(0.0)), 0.0);
    let longer = V6Params { read_expiry_secs: 100_000.0, ..V6Params::default() };
    assert_eq!(class_secs(&credit_v6_with(&at_the_agent(0.0), &longer), CreditClass::Reading), 720.0);

    let mut closed = at_the_agent(1000.0);
    closed.push(prompt(8100.0, 1, false));
    assert_eq!(reading(&closed), 160.0, "the next prompt of the session still closes it first");
}

#[test]
fn v6_an_answer_only_takes_the_seconds_just_before_it() {
    let mut events = agent_window("o/r");
    events.extend(presents(60.0, 4000.0));
    events.push(present(4000.0));
    events.push(answered(4000.0, 0.0, Ask::Question, 2300, 0));
    let c = credit_v6(&events);
    assert_eq!(class_secs(&c, CreditClass::Reading), 600.0);
    assert_eq!(secs_in(&c, CreditClass::Reading, 2200.0, 4000.0), 600.0, "seconds from the asked time are out of reach");

    let mut thin = agent_window("o/r");
    thin.extend(presents(60.0, 300.0));
    thin.extend(presents(3900.0, 4000.0));
    thin.push(answered(4000.0, 0.0, Ask::Question, 2300, 0));
    assert_eq!(class_secs(&credit_v6(&thin), CreditClass::Reading), 120.0, "only the last 1800 s count");

    let wide = V6Params { answer_window_secs: 100_000.0, answer_max_secs: 10_000.0, ..V6Params::default() };
    assert_eq!(class_secs(&credit_v6_with(&thin, &wide), CreditClass::Reading), 420.0);
    let narrow = V6Params { answer_window_secs: 50.0, ..V6Params::default() };
    assert_eq!(class_secs(&credit_v6_with(&events, &narrow), CreditClass::Reading), 50.0);
}

#[test]
fn v6_typing_outranks_an_answer_and_a_pause_removes_credit() {
    let mut events = waiting_at_the_agent(300.0);
    events.push(answered(300.0, 0.0, Ask::Question, 2300, 0));
    events.push(prompt(300.0, 70, false));
    let c = credit_v6(&events);
    assert_eq!(class_secs(&c, CreditClass::Typing), 120.0);
    assert_eq!(secs_in(&c, CreditClass::Reading, 180.0, 300.0), 0.0);
    assert_eq!(secs_in(&c, CreditClass::Reading, 0.0, 180.0), 180.0);
    assert_eq!(total_credit_secs(&c), 300.0);

    let mut events = vec![focus(0.0, Class::Editor, Some("o/r"))];
    events.extend(presents(60.0, 300.0));
    events.push(ev(100.0, None, Kind::Pause { until: 200.0 }));
    assert_eq!(total_credit_secs(&credit_v6(&events)), 200.0);
}

fn by(mut e: Event, harness: &str) -> Event {
    e.harness = Some(harness.into());
    e
}

fn agent_turn(harness: &str, from: f64, to: Option<f64>) -> Vec<Event> {
    let mut out = vec![by(prompt(from, 2, false), harness), by(agent_start(from + 5.0), harness)];
    out.extend(to.map(|t| by(reply(t, 10), harness)));
    out
}

fn editor_day(turn: Vec<Event>) -> Vec<Event> {
    let mut events = vec![focus(0.0, Class::Editor, Some("o/r"))];
    events.extend(presents(60.0, 600.0));
    events.extend(turn);
    events
}

fn human_save(ts: f64, path: &str) -> Event {
    ev(ts, Some("o/r"), Kind::FileSave { count: 1, paths: vec![path.into()] })
}

fn agent_edit(ts: f64, path: &str) -> Event {
    ev(ts, Some("o/r"), Kind::AgentEdit { session: "s".into(), path: path.into(), added: 1, removed: 0 })
}

fn human_heartbeat(at: f64) -> Event {
    ev(
        at,
        Some("o/r"),
        Kind::Heartbeat { at, editor: None, language: None, category: None, path: None, write: true, ai_lines: None, human_lines: Some(4) },
    )
}

fn editing(events: &[Event]) -> f64 {
    class_secs(&credit_v6(events), CreditClass::Editing)
}

#[test]
fn v6_watching_an_agent_run_in_the_editor_earns_nothing() {
    let events = editor_day(agent_turn("claude-code", 100.0, Some(500.0)));
    let c = credit_v6(&events);
    assert_eq!(secs_in(&c, CreditClass::Editing, 0.0, 100.0), 100.0);
    assert_eq!(secs_in(&c, CreditClass::Editing, 100.0, 500.0), 0.0);
    assert_eq!(secs_in(&c, CreditClass::Editing, 500.0, 600.0), 100.0);
    assert_eq!(total_credit_secs(&c), 200.0);
    assert_eq!(class_secs(&credit_v5(&events), CreditClass::Editing), 600.0);

    let unfinished = editor_day(agent_turn("claude-code", 100.0, None));
    assert_eq!(editing(&unfinished), 100.0);

    let mut other_project = editor_day(agent_turn("claude-code", 100.0, Some(500.0)));
    for e in other_project.iter_mut().filter(|e| e.ts == 100.0 || e.ts == 105.0 || e.ts == 500.0) {
        e.project = Some("x/y".into());
    }
    assert_eq!(editing(&other_project), 600.0);

    let no_prompt = editor_day(vec![by(agent_start(100.0), "qwen-code"), by(reply(500.0, 10), "qwen-code")]);
    assert_eq!(editing(&no_prompt), 200.0);
}

#[test]
fn v6_a_save_of_a_file_the_person_touched_credits_around_it() {
    let mut events = editor_day(agent_turn("claude-code", 100.0, Some(500.0)));
    events.push(human_save(300.0, "src/a.rs"));
    let c = credit_v6(&events);
    assert_eq!(secs_in(&c, CreditClass::Editing, 100.0, 500.0), 240.0);
    assert_eq!(secs_in(&c, CreditClass::Editing, 180.0, 420.0), 240.0);
    assert_eq!(class_secs(&c, CreditClass::Editing), 440.0);

    let mut agent_wrote_it = events.clone();
    agent_wrote_it.push(agent_edit(295.0, "src/a.rs"));
    assert_eq!(editing(&agent_wrote_it), 200.0);

    let mut agent_wrote_it_later = events.clone();
    agent_wrote_it_later.push(agent_edit(415.0, "src/a.rs"));
    assert_eq!(editing(&agent_wrote_it_later), 200.0);

    let mut long_before = events.clone();
    long_before.push(agent_edit(100.0, "src/a.rs"));
    assert_eq!(editing(&long_before), 440.0);

    let mut other_file = events.clone();
    other_file.push(agent_edit(295.0, "src/b.rs"));
    assert_eq!(editing(&other_file), 440.0);

    let mut one_of_two = events.clone();
    one_of_two.retain(|e| !matches!(e.kind, Kind::FileSave { .. }));
    one_of_two.push(ev(300.0, Some("o/r"), Kind::FileSave { count: 2, paths: vec!["src/a.rs".into(), "src/b.rs".into()] }));
    one_of_two.push(agent_edit(295.0, "src/a.rs"));
    assert_eq!(editing(&one_of_two), 440.0);

    let mut other_projects_save = events.clone();
    other_projects_save.retain(|e| !matches!(e.kind, Kind::FileSave { .. }));
    other_projects_save.push(ev(300.0, Some("x/y"), Kind::FileSave { count: 1, paths: vec!["src/a.rs".into()] }));
    assert_eq!(editing(&other_projects_save), 200.0);

    let mut unlisted = events.clone();
    unlisted.retain(|e| !matches!(e.kind, Kind::FileSave { .. }));
    unlisted.push(ev(300.0, Some("o/r"), Kind::FileSave { count: 3, paths: vec![] }));
    assert_eq!(editing(&unlisted), 200.0);

    let tuned = credit_v6_with(&events, &V6Params { edit_evidence_secs: 30.0, ..V6Params::default() });
    assert_eq!(class_secs(&tuned, CreditClass::Editing), 200.0 + 60.0);
}

#[test]
fn v6_saves_during_turns_of_harnesses_without_agent_edits_are_not_evidence_but_heartbeats_are() {
    for harness in ["cursor", "copilot", "cline"] {
        let mut events = editor_day(agent_turn(harness, 100.0, Some(500.0)));
        events.push(human_save(300.0, "src/a.rs"));
        assert_eq!(editing(&events), 200.0, "{harness}");
        events.push(human_heartbeat(300.0));
        assert_eq!(editing(&events), 440.0, "{harness}");
    }
    let mut hook_only = editor_day(vec![prompt(100.0, 2, false), reply(500.0, 10)]);
    hook_only.push(human_save(300.0, "src/a.rs"));
    assert_eq!(editing(&hook_only), 200.0);

    let mut one_turn_untraced = editor_day(agent_turn("claude-code", 100.0, Some(500.0)));
    one_turn_untraced.extend([
        by(
            ev(
                150.0,
                Some("o/r"),
                Kind::Prompt { words: 2, pasted: false, model: None, session: "c".into(), shape: None, version: None, via: None, permissions: None },
            ),
            "cursor",
        ),
        by(reply_of(450.0, "c", Some("o/r"), 10), "cursor"),
        human_save(300.0, "src/a.rs"),
    ]);
    assert_eq!(editing(&one_turn_untraced), 200.0);

    let mut ai_only = editor_day(agent_turn("claude-code", 100.0, Some(500.0)));
    ai_only.push(ev(
        300.0,
        Some("o/r"),
        Kind::Heartbeat { at: 300.0, editor: None, language: None, category: None, path: None, write: true, ai_lines: Some(9), human_lines: Some(0) },
    ));
    ai_only.push(ev(
        300.0,
        Some("o/r"),
        Kind::Heartbeat { at: 300.0, editor: None, language: None, category: None, path: None, write: false, ai_lines: None, human_lines: Some(5) },
    ));
    assert_eq!(editing(&ai_only), 200.0);
}

fn editor_save(at: f64, path: Option<&str>, human_lines: Option<i64>, ai_lines: Option<i64>) -> Event {
    let path = path.map(str::to_string);
    ev(at, Some("o/r"), Kind::Heartbeat { at, editor: Some("vscode".into()), language: None, category: None, path, write: true, ai_lines, human_lines })
}

fn editing_v8(events: &[Event]) -> f64 {
    class_secs(&credit_v8(events), CreditClass::Editing)
}

#[test]
fn v8_a_file_changing_on_disk_is_not_hand_editing_but_the_editor_saving_one_is() {
    let mut shell_write = editor_day(agent_turn("codex", 100.0, Some(500.0)));
    shell_write.push(human_save(300.0, "shapes.py"));
    assert_eq!(editing(&shell_write), 440.0, "v6 takes the agent's heredoc for the person's save");
    assert_eq!(editing_v8(&shell_write), 200.0);

    let saved = |human_lines: Option<i64>| {
        let mut events = editor_day(agent_turn("codex", 100.0, Some(500.0)));
        events.push(editor_save(300.0, Some("src/a.rs"), human_lines, None));
        events
    };
    for lines in [None, Some(0), Some(-2), Some(3)] {
        let c = credit_v8(&saved(lines));
        assert_eq!(secs_in(&c, CreditClass::Editing, 180.0, 420.0), 240.0, "{lines:?}: an autosave of a fix inside a line counts too");
        assert_eq!(class_secs(&c, CreditClass::Editing), 440.0, "{lines:?}");
    }
    assert_eq!(editing(&saved(Some(0))), 200.0, "v6 needs new lines from the plugin");

    let mut no_path = editor_day(agent_turn("codex", 100.0, Some(500.0)));
    no_path.push(editor_save(300.0, None, None, None));
    assert_eq!(editing_v8(&no_path), 440.0);

    let with = |extra: Event| {
        let mut events = saved(Some(0));
        events.push(extra);
        editing_v8(&events)
    };
    assert_eq!(with(agent_edit(250.0, "src/a.rs")), 200.0, "the editor saving the agent's change to that file");
    assert_eq!(with(agent_edit(410.0, "src/a.rs")), 200.0);
    assert_eq!(with(agent_edit(250.0, "src/b.rs")), 440.0);
    assert_eq!(with(agent_edit(100.0, "src/a.rs")), 440.0);
    assert_eq!(with(ev(250.0, Some("x/y"), Kind::AgentEdit { session: "s".into(), path: "src/a.rs".into(), added: 1, removed: 0 })), 440.0);

    let mut by_ai = editor_day(agent_turn("codex", 100.0, Some(500.0)));
    by_ai.push(editor_save(300.0, Some("src/a.rs"), Some(0), Some(9)));
    assert_eq!(editing_v8(&by_ai), 200.0, "the plugin put the lines down to AI");
    by_ai.push(editor_save(300.0, Some("src/a.rs"), Some(2), Some(9)));
    assert_eq!(editing_v8(&by_ai), 440.0);

    let mut not_a_save = saved(Some(4));
    for e in &mut not_a_save {
        if let Kind::Heartbeat { write, .. } = &mut e.kind {
            *write = false;
        }
    }
    assert_eq!(editing_v8(&not_a_save), 200.0);

    let mut replayed = saved(Some(4));
    for e in replayed.iter_mut().filter(|e| matches!(e.kind, Kind::Heartbeat { .. })) {
        e.ts += 700.0;
    }
    assert_eq!(editing_v8(&replayed), 200.0, "a heartbeat sent long after its time");
}

#[test]
fn v8_while_an_agent_that_saves_through_the_editor_runs_its_saves_need_typed_lines() {
    for harness in ["cursor", "copilot", "cline", "roo-code", "kilo"] {
        let mut events = editor_day(agent_turn(harness, 100.0, Some(500.0)));
        events.push(editor_save(300.0, Some("src/a.rs"), Some(0), None));
        events.push(editor_save(305.0, Some("src/a.rs"), None, None));
        assert_eq!(editing_v8(&events), 200.0, "{harness}");
        events.push(editor_save(310.0, Some("src/a.rs"), Some(2), None));
        assert_eq!(editing_v8(&events), 440.0, "{harness}");
    }
    let mut elsewhere = editor_day(agent_turn("claude-code", 100.0, Some(500.0)));
    elsewhere.extend([
        by(ev(100.0, Some("x/y"), Kind::Prompt { words: 2, pasted: false, model: None, session: "c".into(), shape: None, version: None, via: None, permissions: None }), "cursor"),
        by(reply_of(500.0, "c", Some("x/y"), 10), "cursor"),
        editor_save(300.0, Some("src/a.rs"), Some(0), None),
    ]);
    assert_eq!(editing_v8(&elsewhere), 440.0, "the editor agent works in another project");
}

#[test]
fn v6_editing_after_the_reply_counts_as_v5() {
    let events = editor_day(agent_turn("cursor", 100.0, Some(300.0)));
    let c = credit_v6(&events);
    assert_eq!(secs_in(&c, CreditClass::Editing, 300.0, 600.0), 300.0);
    assert_eq!(secs_in(&c, CreditClass::Editing, 100.0, 300.0), 0.0);
}

fn editing_v7(events: &[Event]) -> f64 {
    class_secs(&credit_v7(events), CreditClass::Editing)
}

fn in_editor(to: f64) -> Vec<Event> {
    let mut events = vec![focus(0.0, Class::Editor, Some("o/r"))];
    events.extend(presents(60.0, to));
    events
}

fn projectless(mut e: Event) -> Event {
    e.project = None;
    e
}

#[test]
fn v7_a_turn_whose_reply_never_came_ends_turn_max_after_its_last_start() {
    let tm = TURN_MAX_SECS;
    let mut events = in_editor(3.0 * tm);
    events.extend(agent_turn("claude-code", 100.0, None));
    assert_eq!(editing(&events), 100.0);
    let c = credit_v7(&events);
    assert_eq!(secs_in(&c, CreditClass::Editing, 100.0, 105.0 + tm), 0.0);
    assert_eq!(class_secs(&c, CreditClass::Editing), 3.0 * tm - tm - 5.0);

    let mut fresh = events.clone();
    fresh.push(by(prompt(705.0 + tm, 2, false), "claude-code"));
    let c = credit_v7(&fresh);
    assert_eq!(secs_in(&c, CreditClass::Editing, 105.0 + tm, 705.0 + tm), 600.0);
    assert_eq!(secs_in(&c, CreditClass::Editing, 705.0 + tm, 705.0 + 2.0 * tm), 0.0);

    let mut kept = events.clone();
    kept.push(by(prompt(45.0 + tm, 2, false), "claude-code"));
    let c = credit_v7(&kept);
    assert_eq!(secs_in(&c, CreditClass::Editing, 100.0, 45.0 + 2.0 * tm), 0.0);
    assert_eq!(class_secs(&c, CreditClass::Editing), tm + 55.0);
}

#[test]
fn v7_a_turn_with_no_project_gates_only_editor_time_with_none() {
    let turn: Vec<Event> = agent_turn("claude-code", 100.0, Some(500.0)).into_iter().map(projectless).collect();
    let events = editor_day(turn.clone());
    assert_eq!(editing(&events), 200.0);
    assert_eq!(editing_v7(&events), 600.0);

    let mut no_project = editor_day(turn);
    no_project[0] = focus(0.0, Class::Editor, None);
    assert_eq!(editing_v7(&no_project), 200.0);

    let mut in_project = editor_day(agent_turn("claude-code", 100.0, Some(500.0)));
    in_project[0] = focus(0.0, Class::Editor, None);
    assert_eq!(editing_v7(&in_project), 200.0, "a turn in a project still gates an editor with none");

    // eval/replay/repro/open-turn-forever/no-reply: an unanswered prompt with no project a month before.
    let mut month = vec![by(projectless(prompt(-30.0 * 86400.0, 12, false)), "claude-code")];
    month.extend(in_editor(600.0));
    assert_eq!(editing(&month), 0.0);
    assert_eq!(editing_v7(&month), 600.0);
}

#[test]
fn v7_a_window_with_no_project_spends_no_projects_budget() {
    // eval/replay/repro/projectless-window: a reply in o/p and then the Claude app, which has no project, in front.
    let ask =
        ev(-5.0, Some("o/p"), Kind::Prompt { words: 20, pasted: false, model: None, session: "s1".into(), shape: None, version: None, via: None, permissions: None });
    let mut events = vec![ask, reply_of(0.0, "s1", Some("o/p"), 460), focus(1.0, Class::AgentApp, None)];
    events.extend(presents(60.0, 300.0));
    assert_eq!(secs_by(&credit_v6(&events), CreditClass::Reading, Some("o/p")), 240.0);
    assert_eq!(class_secs(&credit_v7(&events), CreditClass::Reading), 0.0);

    let mut no_project = events.clone();
    no_project[1] = reply_of(0.0, "s1", None, 460);
    assert_eq!(secs_by(&credit_v7(&no_project), CreditClass::Reading, None), 240.0, "a reply with no project is read in any work window");

    let mut typed = vec![focus(0.0, Class::AgentApp, None)];
    typed.extend(presents(60.0, 300.0));
    typed.push(prompt(300.0, 35, false));
    assert_eq!(class_secs(&credit_v6(&typed), CreditClass::Typing), 60.0);
    assert_eq!(class_secs(&credit_v7(&typed), CreditClass::Typing), 0.0);
    typed[0] = focus(0.0, Class::AgentApp, Some("o/r"));
    assert_eq!(class_secs(&credit_v7(&typed), CreditClass::Typing), 60.0);
}

#[test]
fn v7_a_project_learned_partway_counts_from_then_on() {
    let late = editor_day(vec![
        by(projectless(prompt(100.0, 2, false)), "claude-code"),
        by(projectless(agent_start(105.0)), "claude-code"),
        by(prompt(300.0, 2, false), "claude-code"),
        by(reply(500.0, 10), "claude-code"),
    ]);
    assert_eq!(editing(&late), 200.0);
    let c = credit_v7(&late);
    assert_eq!(secs_in(&c, CreditClass::Editing, 100.0, 300.0), 200.0);
    assert_eq!(secs_in(&c, CreditClass::Editing, 300.0, 500.0), 0.0);

    let kept = editor_day(vec![prompt(100.0, 2, false), projectless(agent_start(105.0)), reply(500.0, 10)]);
    assert_eq!(editing_v7(&kept), 200.0, "a start without a project keeps the one named just before");

    let mut read = vec![focus(0.0, Class::AgentTerminal, Some("o/r")), reply_of(0.0, "s", None, 230)];
    read.extend(presents(60.0, 600.0));
    read.push(prompt(400.0, 2, false));
    assert_eq!(secs_by(&credit_v6(&read), CreditClass::Reading, Some("o/r")), 180.0);
    assert_eq!(secs_by(&credit_v7(&read), CreditClass::Reading, None), 180.0, "the prompt that closes a reply doesn't name its project");
}

#[test]
fn v7_a_heartbeat_sent_long_after_its_time_is_not_evidence() {
    let sent = |at: f64, lag: f64| {
        let mut e = human_heartbeat(at);
        e.ts = at + lag;
        e
    };
    let mut events = editor_day(agent_turn("cursor", 100.0, Some(500.0)));
    events.push(sent(300.0, 900.0));
    assert_eq!(editing(&events), 440.0);
    assert_eq!(editing_v7(&events), 200.0);
    *events.last_mut().unwrap() = sent(300.0, 90.0);
    assert_eq!(editing_v7(&events), 440.0);
}

#[test]
fn v7_a_pause_ends_pause_max_after_it_began() {
    let mut events = vec![focus(0.0, Class::Editor, Some("o/r")), ev(0.0, None, Kind::Pause { until: 3.0 * 86400.0 })];
    events.extend(presents(PAUSE_MAX_SECS - 600.0, PAUSE_MAX_SECS + 600.0));
    assert_eq!(editing(&events), 0.0);
    assert_eq!(editing_v7(&events), 600.0);
}

#[test]
fn agent_time_of_an_unfinished_turn_reaches_its_last_event_within_turn_max() {
    let tm = TURN_MAX_SECS;
    let mut events = vec![agent_start(0.0)];
    events.extend(presents(60.0, 600.0));
    assert_eq!(agent_runtime(&events), vec![(0.0, 600.0)]);
    events.push(reply(900.0, 10));
    assert_eq!(agent_runtime(&events), vec![(0.0, 900.0)], "a reply after the last event ends it there");

    let mut events = vec![agent_start(0.0)];
    events.extend(presents(60.0, 3.0 * tm));
    events.push(agent_start(60.0));
    events.push(agent_start(2.0 * tm + 60.0));
    let o_r = Some("o/r");
    assert_eq!(agent_turns(&events), vec![((0.0, 60.0), o_r), ((60.0, 60.0 + tm), o_r), ((2.0 * tm + 60.0, 3.0 * tm), o_r)]);
    assert_eq!(agent_runtime(&events), vec![(0.0, 60.0 + tm), (2.0 * tm + 60.0, 3.0 * tm)]);
}

#[test]
fn agent_time_stays_with_the_project_its_turn_started_in() {
    let at = |ts: f64, project: Option<&str>, kind: Kind| ev(ts, project, kind);
    let start = |ts: f64, project: Option<&str>| at(ts, project, Kind::AgentStart { session: "s".into() });
    let done = |ts: f64, project: Option<&str>| at(ts, project, Kind::ReplyDone { session: "s".into(), reply_words: 10, tools: 1, files_changed: 0, model: None });
    let mut events = vec![start(0.0, Some("o/a")), done(600.0, Some("o/b")), start(900.0, Some("o/b")), start(1200.0, None), done(1500.0, None)];
    events.extend(presents(60.0, 3600.0));
    assert_eq!(agent_turns(&events), vec![((0.0, 600.0), Some("o/a")), ((900.0, 1200.0), Some("o/b")), ((1200.0, 1500.0), Some("o/b"))]);
    assert_eq!(agent_runtime(&events), vec![(0.0, 600.0), (900.0, 1500.0)]);
}

// A terminal of o/r is in front from 0. Presence credits [0, 120) and [240, 300), leaving a two minute gap.
fn terminal_gap(mut more: Vec<Event>) -> Vec<Credit> {
    let mut events = vec![focus(0.0, Class::Terminal, Some("o/r")), present(60.0), present(120.0), present(300.0)];
    events.append(&mut more);
    events.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    credit_v8(&events)
}

#[test]
fn v8_a_short_pause_in_the_projects_window_is_bridging_time() {
    let c = terminal_gap(vec![]);
    assert_eq!(secs_by(&c, CreditClass::Bridge, Some("o/r")), 120.0);
    assert_eq!(secs_by(&c, CreditClass::Terminal, Some("o/r")), 180.0);
    let events = vec![focus(0.0, Class::Terminal, Some("o/r")), present(60.0), present(120.0), present(300.0)];
    assert!(credit_v7(&events).iter().all(|c| c.class != CreditClass::Bridge), "v7 bridges nothing");

    let long = vec![focus(0.0, Class::Terminal, Some("o/r")), present(60.0), present(120.0), present(420.0)];
    assert_eq!(secs_by(&credit_v8(&long), CreditClass::Bridge, Some("o/r")), 0.0, "a four minute gap is not bridged");
}

#[test]
fn v8_a_quick_look_elsewhere_inside_a_gap_is_a_trip() {
    let c = terminal_gap(vec![focus(150.0, Class::BrowserUnknown, None), focus(170.0, Class::Terminal, Some("o/r"))]);
    assert_eq!(secs_by(&c, CreditClass::Bridge, Some("o/r")), 100.0);
    assert_eq!(secs_by(&c, CreditClass::Trip, Some("o/r")), 20.0);

    let c = terminal_gap(vec![focus(140.0, Class::BrowserUnknown, None), focus(180.0, Class::Terminal, Some("o/r"))]);
    assert_eq!(secs_by(&c, CreditClass::Bridge, Some("o/r")), 80.0);
    assert_eq!(secs_by(&c, CreditClass::Trip, Some("o/r")), 0.0, "40 s away is not a trip");

    let c = terminal_gap(vec![focus(130.0, Class::Terminal, Some("x/y")), focus(230.0, Class::Terminal, Some("o/r"))]);
    assert_eq!(secs_by(&c, CreditClass::Bridge, Some("o/r")), 20.0, "only seconds the project's own windows were in front");
    assert_eq!(secs_by(&c, CreditClass::Trip, Some("o/r")) + secs_by(&c, CreditClass::Bridge, Some("x/y")), 0.0);
}

#[test]
fn v8_bridges_no_gap_with_a_pause_in_it() {
    let c = terminal_gap(vec![ev(150.0, None, Kind::Pause { until: 200.0 })]);
    assert!(c.iter().all(|c| !matches!(c.class, CreditClass::Bridge | CreditClass::Trip)), "{c:?}");
}

fn beat(at: f64, sent: f64, project: &str, ai_lines: Option<i64>, human_lines: Option<i64>) -> Event {
    ev(sent, Some(project), Kind::Heartbeat { at, editor: Some("vscode".into()), language: None, category: None, path: None, write: false, ai_lines, human_lines })
}

#[test]
fn v8_an_editor_window_takes_its_project_from_the_editors_heartbeats() {
    let mut events = vec![focus(0.0, Class::Editor, Some("o/launched-in"))];
    events.extend(presents(60.0, 600.0));
    events.extend([beat(100.0, 100.0, "o/shown", None, Some(2)), beat(200.0, 201.0, "o/shown", None, None)]);
    events.push(beat(400.0, 400.0, "o/other", Some(5), Some(0)));
    events.push(beat(500.0, 1500.0, "o/other", None, Some(3)));
    events.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    let c = credit_v8(&events);
    assert_eq!(secs_by(&c, CreditClass::Editing, Some("o/shown")), 320.0, "up to two minutes past the last heartbeat");
    assert_eq!(secs_by(&c, CreditClass::Editing, Some("o/launched-in")), 280.0);
    assert_eq!(secs_by(&c, CreditClass::Editing, Some("o/other")), 0.0, "AI lines and a heartbeat replayed long after say nothing");
    assert_eq!(secs_by(&credit_v7(&events), CreditClass::Editing, Some("o/launched-in")), 600.0);
}

#[test]
fn v8_the_nearest_heartbeat_decides_between_two_projects() {
    let mut events = vec![focus(0.0, Class::Editor, None)];
    events.extend(presents(60.0, 300.0));
    events.extend([beat(60.0, 60.0, "o/a", None, None), beat(160.0, 160.0, "o/b", None, None)]);
    events.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    let c = credit_v8(&events);
    assert_eq!(secs_by(&c, CreditClass::Editing, Some("o/a")), 110.0);
    assert_eq!(secs_by(&c, CreditClass::Editing, Some("o/b")), 170.0);
    assert_eq!(secs_by(&c, CreditClass::Editing, None), 20.0, "nothing within two minutes after 280");
}

#[test]
fn v8_only_heartbeats_sent_while_the_editor_was_in_front_move_its_project() {
    let editor = |beat_at: f64| {
        let mut events = vec![focus(0.0, Class::Terminal, Some("o/t")), focus(100.0, Class::Editor, Some("o/launched")), beat(beat_at, beat_at, "o/shown", None, None)];
        events.extend(presents(60.0, 400.0));
        events.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        let c = credit_v8(&events);
        (secs_by(&c, CreditClass::Editing, Some("o/shown")), secs_by(&c, CreditClass::Editing, Some("o/launched")))
    };
    assert_eq!(editor(90.0), (0.0, 260.0), "sent before the editor came to the front");
    assert_eq!(editor(150.0), (170.0, 90.0));
}

fn active(from: f64, n: u32) -> Vec<f64> {
    (0..n).map(|i| from + f64::from(i) + 0.5).collect()
}

// A terminal of o/r, credited [0, 60) and [180, 240) by two presents, each minute with a few seconds of input.
#[test]
fn v8_a_gap_is_bridged_only_when_the_person_was_active_on_both_sides() {
    let bridged = |before: &[f64], after: &[f64]| {
        let events = vec![
            focus(0.0, Class::Terminal, Some("o/r")),
            present_with(60.0, &input_bits(60.0, before)),
            present_with(240.0, &input_bits(240.0, after)),
        ];
        secs_by(&credit_v8(&events), CreditClass::Bridge, Some("o/r"))
    };
    assert_eq!(bridged(&active(30.0, 5), &active(200.0, 5)), 120.0);
    assert_eq!(bridged(&[30.5], &active(200.0, 5)), 0.0, "one key press before the gap");
    assert_eq!(bridged(&active(30.0, 5), &active(200.0, 4)), 0.0, "four seconds of input after it");
}

// Half a minute of typing in o/r's agent terminal ends in a prompt at 30, the agent works until 200, and the person reads
// its reply after a few key presses. While the turn runs, bridging time lasts only half a minute past their last input.
#[test]
fn v8_watching_the_agent_work_is_not_bridging_time() {
    let bridged = |during: &[f64]| {
        let mut events = vec![
            focus(0.0, Class::AgentTerminal, Some("o/r")),
            present_with(60.0, &input_bits(60.0, &active(0.0, 30))),
            prompt(30.0, 20, false),
            reply(200.0, 50),
            present_with(240.0, &input_bits(240.0, &active(210.0, 6))),
        ];
        if !during.is_empty() {
            events.push(present_with(120.0, &input_bits(120.0, during)));
        }
        events.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        let c = credit_v8(&events);
        assert_eq!(secs_by(&c, CreditClass::Reading, Some("o/r")), 40.0, "{c:?}");
        secs_by(&c, CreditClass::Bridge, Some("o/r"))
    };
    assert_eq!(bridged(&[]), 30.0);
    assert_eq!(bridged(&active(100.0, 3)), 63.0, "and half a minute after each key press during the turn");
}

// An editor of o/r between two terminal stretches while an agent works in o/r: with no hand edit near it the editing gate
// refuses the editor's time, and bridging doesn't give it back.
#[test]
fn v8_editor_time_the_gate_refused_is_not_bridged() {
    let credit = |hand_edit: bool| {
        let mut events = vec![
            focus(0.0, Class::Terminal, Some("o/r")),
            focus(120.0, Class::Editor, Some("o/r")),
            focus(240.0, Class::Terminal, Some("o/r")),
            agent_start(100.0),
            reply(400.0, 0),
        ];
        events.extend((1..=5).map(|m| present_with(60.0 * f64::from(m), &"f".repeat(15))));
        if hand_edit {
            let save = Kind::Heartbeat { at: 180.0, editor: Some("vscode".into()), language: None, category: None, path: None, write: true, ai_lines: None, human_lines: Some(3) };
            events.push(ev(180.0, Some("o/r"), save));
        }
        events.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        credit_v8(&events)
    };
    let c = credit(false);
    assert_eq!(secs_by(&c, CreditClass::Editing, Some("o/r")), 0.0);
    assert_eq!(secs_by(&c, CreditClass::Bridge, Some("o/r")) + secs_by(&c, CreditClass::Trip, Some("o/r")), 0.0, "{c:?}");
    assert_eq!(secs_by(&credit(true), CreditClass::Editing, Some("o/r")), 120.0, "a hand edit vouches for it");
}

// A plain terminal of o/r in front, half a minute of typing, then the agent works from 30 to 600 while the person stays
// at the keyboard without touching it (kairosd still marks them present). credit_v9 credits the terminal while they're
// present; credit_v10 stops half a minute after their last input, as bridging does.
#[test]
fn v10_watching_the_agent_from_a_terminal_earns_only_the_wait() {
    let quiet = "0".repeat(15);
    let events = vec![
        focus(0.0, Class::Terminal, Some("o/r")),
        present_with(60.0, &input_bits(60.0, &active(0.0, 30))),
        prompt(30.0, 20, false),
        present_with(120.0, &quiet),
        present_with(180.0, &quiet),
        reply(600.0, 0),
    ];
    assert_eq!(secs_by(&credit_v9(&events), CreditClass::Terminal, Some("o/r")), 180.0);
    assert_eq!(secs_by(&credit_v10(&events), CreditClass::Terminal, Some("o/r")), 60.0);
    let mut later = events.clone();
    later.push(present_with(240.0, &input_bits(240.0, &active(200.0, 2))));
    later.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    assert_eq!(secs_by(&credit_v10(&later), CreditClass::Terminal, Some("o/r")), 92.0, "and half a minute after a key press during the turn");
}

// A 10,000-word reply read with the agent terminal in front and input every second for 50 minutes: credit_v9 gives it
// the whole reading budget (about 45 minutes), credit_v10 at most 20.
#[test]
fn v10_one_reply_is_read_for_at_most_twenty_minutes() {
    let mut events = vec![focus(0.0, Class::AgentTerminal, Some("o/r")), prompt(50.0, 20, false), reply(60.0, 10_000)];
    events.extend((1..=50).map(|m| present_with(60.0 * f64::from(m), &"f".repeat(15))));
    events.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    let v9 = secs_by(&credit_v9(&events), CreditClass::Reading, Some("o/r"));
    assert!(v9 > 2600.0, "{v9}");
    assert_eq!(secs_by(&credit_v10(&events), CreditClass::Reading, Some("o/r")), 1200.0);
    assert_eq!(Rule::parse("v10"), Some(Rule::V10));
    assert_eq!(Rule::V10.name(), "credit_v10");
    assert_eq!(Rule::V10.apply(&events), credit_v10(&events));
}
