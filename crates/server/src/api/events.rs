//! Uploads from paired devices. What lands here becomes the credited hours Hack Club programs read.

use std::collections::HashMap;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use deadpool_postgres::GenericClient;
use kairos_core::event::{verify_chain, MAX_CHANGES, MAX_PATH_LEN};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::aliases;
use crate::api::device;
use crate::credit;
use crate::limits::{self, Bucket};
use crate::live;
use crate::util::{err, hash, now, Res};
use crate::St;

const MAX_BATCH: usize = 5000;
const FUTURE_SECS: f64 = 300.0;
const MAX_EVENT_BYTES: usize = 256 * 1024;
const GIT_DATE_SLACK: f64 = 86_400.0;
const VIA: [&str; 7] = ["commit", "amend", "cherry_pick", "revert", "rebase", "arrived", "unknown"];

#[derive(Deserialize)]
pub struct Upload {
    events: Vec<Value>,
}

struct Row {
    ts: f64,
    kind: String,
    project: Option<String>,
}

fn bad(error: &str, index: usize) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": error, "index": index }))).into_response()
}

fn project_id(s: &str) -> Option<String> {
    Some(s.chars().filter(|c| !c.is_control()).take(200).collect::<String>()).filter(|p| !p.is_empty())
}

fn git_date(v: &Value, key: &str, limit: f64) -> Option<i64> {
    v.get(key).and_then(Value::as_i64).filter(|t| *t > 0 && *t as f64 <= limit)
}

fn clean_changes(v: &Value) -> Option<Value> {
    let list = v.get("changes")?.as_array()?;
    let kept: Vec<Value> = list
        .iter()
        .filter_map(|c| {
            let path: String = c.get("path")?.as_str()?.chars().filter(|ch| !ch.is_control()).collect();
            if path.is_empty() || path.len() > MAX_PATH_LEN {
                return None;
            }
            let lines = |k: &str| c.get(k)?.as_u64().map(|x| x.min(i32::MAX as u64));
            Some(json!({ "path": path, "added": lines("added")?, "removed": lines("removed")? }))
        })
        .take(MAX_CHANGES)
        .collect();
    (!kept.is_empty()).then_some(Value::Array(kept))
}

/// Postgres can't keep a NUL inside JSON text.
fn has_nul(v: &Value) -> bool {
    match v {
        Value::String(s) => s.contains('\0'),
        Value::Array(a) => a.iter().any(has_nul),
        Value::Object(m) => m.iter().any(|(k, v)| k.contains('\0') || has_nul(v)),
        _ => false,
    }
}

pub fn repo_url(project: &str) -> Option<String> {
    let parts: Vec<&str> = project.split('/').collect();
    let plain = |s: &&str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !parts.iter().all(plain) {
        return None;
    }
    match parts.len() {
        2 => Some(format!("https://github.com/{project}")),
        3 if parts[0].contains('.') => Some(format!("https://{project}")),
        _ => None,
    }
}

/// The oldest turn a client sends is 14 days back. An upload dated further behind what its device already sent makes
/// the server work through that much history again, so a device gets to do it only a few times (`Bucket::Rewind`).
const REWIND_SECS: f64 = 16.0 * 86_400.0;

pub async fn upload(State(st): State<St>, h: HeaderMap, Json(up): Json<Upload>) -> Res<Response> {
    let dev = device::auth(&st, &h).await?;
    limits::check(&st, Bucket::Events, &dev.token_hash)?;
    if up.events.len() > MAX_BATCH {
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "at most 5000 events per request"));
    }
    let lines: Vec<String> = up.events.iter().map(Value::to_string).collect();
    if let Some(i) = lines.iter().position(|l| l.len() > MAX_EVENT_BYTES) {
        return Ok(bad("too_big", i));
    }
    if let Some(i) = up.events.iter().position(has_nul) {
        return Ok(bad("bad_text", i));
    }
    if let Err(b) = verify_chain(lines.iter().map(String::as_str), None) {
        return Ok((StatusCode::BAD_REQUEST, Json(json!({ "error": "bad_hash", "index": b.line - 1, "reason": b.reason })))
            .into_response());
    }
    let limit = now() + FUTURE_SECS;
    let mut rows = Vec::with_capacity(up.events.len());
    for (i, v) in up.events.iter().enumerate() {
        let Some(ts) = v.get("ts").and_then(Value::as_f64).filter(|t| t.is_finite() && *t > 0.0 && *t <= limit) else {
            return Ok(bad("bad_ts", i));
        };
        let kind: String = v.get("kind").and_then(Value::as_str).unwrap_or("").chars().take(32).collect();
        rows.push(Row { ts, kind, project: v.get("project").and_then(Value::as_str).and_then(project_id) });
    }
    let version = device::client_version(&h);
    let mut c = st.pool.get().await?;
    let tx = c.transaction().await?;
    let r = tx
        .query_opt("SELECT head, event_count, last_event_ts FROM devices WHERE id = $1 AND revoked_at IS NULL FOR UPDATE", &[&dev.id])
        .await?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "unknown or revoked device token"))?;
    let (head, count, last_ts): (Option<String>, i64, Option<f64>) = (r.get(0), r.get(1), r.get(2));
    if up.events.is_empty() {
        return Ok(Json(json!({ "accepted": 0, "last_hash": head })).into_response());
    }
    let first_prev = up.events[0].get("prev").and_then(Value::as_str).unwrap_or("").to_string();
    if count > 0 && head.as_deref() != Some(first_prev.as_str()) {
        return Ok((StatusCode::CONFLICT, Json(json!({ "error": "chain_mismatch", "last_hash": head }))).into_response());
    }
    let (lo, hi) = rows.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), r| (lo.min(r.ts), hi.max(r.ts)));
    if lo < last_ts.unwrap_or(hi) - REWIND_SECS {
        limits::check(&st, Bucket::Rewind, &dev.id.to_string())?;
    }

    let n = rows.len() as i64;
    let seqs: Vec<i64> = (count + 1..=count + n).collect();
    let ts: Vec<f64> = rows.iter().map(|r| r.ts).collect();
    let kinds: Vec<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
    let projects: Vec<Option<&str>> = rows.iter().map(|r| r.project.as_deref()).collect();
    tx.execute(
        "INSERT INTO events (device_id, seq, ts, kind, project, body)
         SELECT $1, * FROM UNNEST($2::bigint[], $3::float8[], $4::text[], $5::text[], $6::jsonb[])",
        &[&dev.id, &seqs, &ts, &kinds, &projects, &up.events],
    )
    .await?;

    let new_head = up.events.last().and_then(|v| v.get("hash")).and_then(Value::as_str).map(str::to_string);
    let backends = up.events.iter().rev().find(|v| v.get("kind").and_then(Value::as_str) == Some("backends")).map(|v| {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(|x| x.chars().take(32).collect::<String>());
        json!({ "focus": s("focus"), "idle": s("idle") })
    });
    tx.execute(
        "UPDATE devices SET head = $2, event_count = event_count + $3, anchor = COALESCE(anchor, $4), last_seen_at = now(),
            client_version = COALESCE($5, client_version), backends = COALESCE($6, backends),
            first_event_ts = LEAST(first_event_ts, $7), last_event_ts = GREATEST(last_event_ts, $8)
         WHERE id = $1",
        &[&dev.id, &new_head, &n, &first_prev, &version, &backends, &lo, &hi],
    )
    .await?;

    let mut seen: HashMap<&str, (f64, f64)> = HashMap::new();
    for r in &rows {
        if let Some(p) = r.project.as_deref() {
            let e = seen.entry(p).or_insert((r.ts, r.ts));
            e.0 = e.0.min(r.ts);
            e.1 = e.1.max(r.ts);
        }
    }
    for (p, (first, last)) in &seen {
        tx.execute(
            "INSERT INTO projects (user_id, id, repo_url, first_seen, last_seen) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (user_id, id) DO UPDATE SET first_seen = LEAST(projects.first_seen, EXCLUDED.first_seen),
                last_seen = GREATEST(projects.last_seen, EXCLUDED.last_seen)",
            &[&dev.user_id, p, &repo_url(p), first, last],
        )
        .await?;
    }
    // First seen wins, except the device that made a commit replaces a sighting pulled onto another device that uploaded first.
    let date_limit = now() + GIT_DATE_SLACK;
    for (v, r) in up.events.iter().zip(&rows) {
        if r.kind != "commit" {
            continue;
        }
        let (Some(project), Some(sha)) = (r.project.as_deref(), v.get("sha").and_then(Value::as_str)) else { continue };
        let sha = sha.to_ascii_lowercase();
        if !(7..=64).contains(&sha.len()) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let num = |k: &str| v.get(k).and_then(Value::as_u64).map_or(0, |x| x.min(i32::MAX as u64) as i32);
        let via = v.get("via").and_then(Value::as_str).filter(|x| VIA.contains(x));
        tx.execute(
            "INSERT INTO commits (user_id, project, sha, ts, files, added, removed, device_id, authored_at, committed_at, via, changes, not_before)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
             ON CONFLICT (user_id, project, sha) DO UPDATE SET ts = EXCLUDED.ts, files = EXCLUDED.files, added = EXCLUDED.added,
                removed = EXCLUDED.removed, device_id = EXCLUDED.device_id, authored_at = EXCLUDED.authored_at,
                committed_at = EXCLUDED.committed_at, via = EXCLUDED.via, changes = EXCLUDED.changes, not_before = EXCLUDED.not_before,
                received_at = now()
             WHERE commits.via = 'arrived' AND EXCLUDED.via IS NOT NULL AND EXCLUDED.via <> 'arrived'",
            &[
                &dev.user_id,
                &project,
                &sha,
                &r.ts,
                &num("files"),
                &num("added"),
                &num("removed"),
                &dev.id,
                &git_date(v, "authored", date_limit),
                &git_date(v, "committed", date_limit),
                &via,
                &clean_changes(v),
                &v.get("not_before").and_then(Value::as_f64).filter(|t| *t > 0.0 && *t <= r.ts),
            ],
        )
        .await?;
    }
    store_heartbeats(&tx, dev.user_id, &up.events, &rows, limit).await?;

    credit::recompute_device(&tx, dev.id, dev.user_id, lo, hi).await?;
    let moves: Vec<(String, String, f64)> = up
        .events
        .iter()
        .zip(&rows)
        .filter(|(_, r)| r.kind == "project_alias")
        .filter_map(|(v, r)| Some((v.get("from")?.as_str().and_then(project_id)?, r.project.clone()?, r.ts)))
        .filter(|(from, to, _)| from != to)
        .collect();
    let have: i64 = if moves.is_empty() {
        0
    } else {
        tx.query_one("SELECT count(*) FROM project_links WHERE user_id = $1", &[&dev.user_id]).await?.get(0)
    };
    // A device sending thousands of moves is broken or hostile, and every refresh reads all of a user's links.
    let moves = &moves[..moves.len().min((aliases::MAX_LINKS - have).max(0) as usize)];
    if !moves.is_empty() {
        aliases::link(&tx, dev.user_id, moves, "device").await?;
        if aliases::refresh(&tx, dev.user_id).await? {
            let tz = credit::lock_user(&tx, dev.user_id).await?;
            credit::rebuild_all(&tx, dev.user_id, tz).await?;
        }
    }
    tx.commit().await?;
    live::publish(&st, dev.user_id);
    Ok(Json(json!({ "accepted": n, "last_hash": new_head })).into_response())
}

async fn store_heartbeats(tx: &impl GenericClient, user_id: i64, events: &[Value], rows: &[Row], limit: f64) -> Res<()> {
    let text = |v: &Value, k: &str| {
        let s: String = v.get(k)?.as_str()?.chars().filter(|c| !c.is_control()).take(200).collect();
        (!s.is_empty()).then_some(s)
    };
    let (mut time, mut entity, mut project, mut language, mut editor, mut category, mut write) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (v, r) in events.iter().zip(rows).filter(|(_, r)| r.kind == "heartbeat") {
        let Some(at) = v.get("at").and_then(Value::as_f64).filter(|t| t.is_finite() && *t > 0.0 && *t <= limit) else {
            continue;
        };
        time.push(at);
        entity.push(hash(&format!("{}\0{}", r.project.as_deref().unwrap_or(""), text(v, "path").unwrap_or_default())));
        project.push(r.project.clone());
        language.push(text(v, "language"));
        editor.push(text(v, "editor"));
        category.push(text(v, "category"));
        write.push(v.get("write").and_then(Value::as_bool).unwrap_or(false));
    }
    if !time.is_empty() {
        tx.execute(
            "INSERT INTO heartbeats (user_id, time, entity_hash, project, language, editor, category, is_write)
             SELECT $1, * FROM UNNEST($2::float8[], $3::text[], $4::text[], $5::text[], $6::text[], $7::text[], $8::bool[])
             ON CONFLICT DO NOTHING",
            &[&user_id, &time, &entity, &project, &language, &editor, &category, &write],
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::repo_url;

    #[test]
    fn repo_urls() {
        assert_eq!(repo_url("obaym/kairos").as_deref(), Some("https://github.com/obaym/kairos"));
        assert_eq!(repo_url("gitlab.com/a/b").as_deref(), Some("https://gitlab.com/a/b"));
        assert_eq!(repo_url("my-folder"), None);
        assert_eq!(repo_url("a/b c"), None);
    }
}
