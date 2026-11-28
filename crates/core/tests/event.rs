use kairos_core::event::{input_bits, verify_chain, Ask, Event, FileChange, Kind, GENESIS};
use serde_json::{json, Value};

#[test]
fn git_events_from_older_daemons_read_the_same_and_new_fields_stay_off_when_empty() {
    let old = json!({ "kind": "commit", "sha": "a".repeat(40), "files": 2, "added": 5, "removed": 1 });
    let kind: Kind = serde_json::from_value(old.clone()).unwrap();
    assert!(matches!(&kind, Kind::Commit { authored: None, committed: None, changes, .. } if changes.is_empty()));
    assert_eq!(serde_json::to_value(&kind).unwrap(), old);

    let old_save = json!({ "kind": "file_save", "count": 3 });
    let save: Kind = serde_json::from_value(old_save.clone()).unwrap();
    assert_eq!(serde_json::to_value(&save).unwrap(), old_save);

    let diff = Kind::Diff {
        files: 1,
        added: 4,
        removed: 0,
        changes: vec![FileChange { path: "src/main.rs".into(), added: 4, removed: 0 }],
    };
    let v: Value = serde_json::to_value(&diff).unwrap();
    assert_eq!(v["changes"], json!([{ "path": "src/main.rs", "added": 4, "removed": 0 }]));
    assert_eq!(serde_json::from_value::<Kind>(v).unwrap(), diff);
}

#[test]
fn a_nul_in_any_text_is_dropped_before_the_event_is_hashed() {
    let mut e = Event::new(1.0, Some("a\0b".into()), Kind::AgentCommit { session: "s\0".into(), sha: Some("abc1234".into()) });
    e.harness = Some("claude\0-code".into());
    e.seal(GENESIS);
    let line = serde_json::to_string(&e).unwrap();
    assert!(!line.contains("\\u0000"), "{line}");
    assert_eq!((e.project.as_deref(), e.harness.as_deref()), (Some("ab"), Some("claude-code")));
    assert!(verify_chain(std::iter::once(line.as_str()), Some(GENESIS)).is_ok());
}

fn bits(s: &str) -> u64 {
    assert_eq!(s.len(), 15, "{s}");
    assert!(s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "{s}");
    u64::from_str_radix(s, 16).unwrap()
}

#[test]
fn input_bit_i_is_the_second_starting_at_ts_minus_60_plus_i() {
    assert_eq!(input_bits(1000.0, &[]), "000000000000000");
    assert_eq!(bits(&input_bits(1000.0, &[999.5])), 1 << 59);
    assert_eq!(bits(&input_bits(1000.0, &[999.0])), 1 << 59);
    assert_eq!(bits(&input_bits(1000.0, &[998.99])), 1 << 58);
    assert_eq!(bits(&input_bits(1000.25, &[940.25])), 1);
    assert_eq!(bits(&input_bits(1000.0, &[941.0, 941.9, 940.0, 970.5])), 0b11 | 1 << 30);
    assert_eq!(input_bits(1000.0, &[940.0, 999.9]), "800000000000001");
}

#[test]
fn input_outside_the_window_is_dropped() {
    assert_eq!(input_bits(1000.0, &[939.999, 1000.0, 1000.5, 0.0]), "000000000000000");
    assert_eq!(bits(&input_bits(1000.0, &[939.999, 940.0, 1000.0])), 1);
    let every_second: Vec<f64> = (0..60).map(|i| 940.5 + f64::from(i)).collect();
    assert_eq!(input_bits(1000.0, &every_second), "fffffffffffffff");
}

#[test]
fn a_present_with_input_round_trips_and_one_without_leaves_the_field_off() {
    let with = Kind::Present { input: Some("800000000000001".into()), keys: None, injected: None };
    let v = serde_json::to_value(&with).unwrap();
    assert_eq!(v, json!({ "kind": "present", "input": "800000000000001" }));
    assert_eq!(serde_json::from_value::<Kind>(v).unwrap(), with);

    let without = Kind::Present { input: None, keys: None, injected: None };
    let v = serde_json::to_value(&without).unwrap();
    assert_eq!(v, json!({ "kind": "present" }));
    assert_eq!(serde_json::from_value::<Kind>(v).unwrap(), without);
}

#[test]
fn an_old_present_line_still_verifies_and_hashes_the_same() {
    const OLD_HASH: &str = "13f853c183426c8263e0a1677bf434eec73cd7768ec598227d66169949d297ab";
    let old = format!(r#"{{"v":1,"ts":1000.5,"kind":"present","prev":"{GENESIS}","hash":"{OLD_HASH}"}}"#);
    assert!(verify_chain(std::iter::once(old.as_str()), Some(GENESIS)).is_ok());

    let mut e: Event = serde_json::from_str(&old).unwrap();
    assert_eq!(e.kind, Kind::Present { input: None, keys: None, injected: None });
    assert_eq!(serde_json::to_string(&e).unwrap(), old);
    e.seal(GENESIS);
    assert_eq!(e.hash, OLD_HASH);

    let mut new = Event::new(1000.5, None, Kind::Present { input: Some("000000000000001".into()), keys: None, injected: None });
    new.seal(GENESIS);
    assert_ne!(new.hash, OLD_HASH);
    let line = serde_json::to_string(&new).unwrap();
    assert!(verify_chain(std::iter::once(line.as_str()), Some(GENESIS)).is_ok());
}

#[test]
fn answered_events_round_trip_and_older_shapes_still_parse() {
    let kind = Kind::Answered { session: "s".into(), asked: 10.5, why: Ask::Permission, shown_words: 7, answer_words: 0 };
    let v = serde_json::to_value(&kind).unwrap();
    assert_eq!(v, json!({ "kind": "answered", "session": "s", "asked": 10.5, "why": "permission", "shown_words": 7, "answer_words": 0 }));
    assert_eq!(serde_json::from_value::<Kind>(v).unwrap(), kind);
    assert_eq!(kind.name(), "answered");
    assert!(kind.is_activity());

    let bare: Kind = serde_json::from_value(json!({ "kind": "answered", "session": "s", "asked": 1.0, "why": "question" })).unwrap();
    assert_eq!(bare, Kind::Answered { session: "s".into(), asked: 1.0, why: Ask::Question, shown_words: 0, answer_words: 0 });
    let new_why: Kind = serde_json::from_value(json!({ "kind": "answered", "session": "s", "asked": 1.0, "why": "elicitation" })).unwrap();
    assert!(matches!(new_why, Kind::Answered { why: Ask::Unknown, .. }));
}
