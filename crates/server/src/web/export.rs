//! Streamed, so millions of events never sit in memory. Rows come from one repeatable-read snapshot.
//! Token hashes, sessions and keys are never selected.

use std::convert::Infallible;

use anyhow::Result;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use futures_util::{pin_mut, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_postgres::types::ToSql;
use tokio_postgres::IsolationLevel;

use crate::auth::SignedIn;
use crate::limits::{self, Bucket};
use crate::util::{now, Res};
use crate::St;

const CHUNK: usize = 64 * 1024;

struct Out {
    buf: String,
    tx: mpsc::Sender<Bytes>,
}

impl Out {
    async fn push(&mut self, s: &str) -> Result<()> {
        self.buf.push_str(s);
        if self.buf.len() >= CHUNK {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        if !self.buf.is_empty() {
            let b = Bytes::from(std::mem::take(&mut self.buf));
            self.tx.send(b).await.map_err(|_| anyhow::anyhow!("client went away"))?;
        }
        Ok(())
    }
}

/// Each section: (key, query returning one json column, ordered).
const SECTIONS: &[(&str, &str)] = &[
    (
        "devices",
        "SELECT json_build_object('id', id, 'name', name, 'os', os, 'client_version', client_version, 'backends', backends,
            'anchor', anchor, 'head', head, 'event_count', event_count, 'created_at', created_at, 'last_seen_at', last_seen_at,
            'revoked_at', revoked_at) FROM devices WHERE user_id = $1 ORDER BY id",
    ),
    (
        "device_networks",
        "SELECT json_build_object('device_id', n.device_id, 'since', n.since, 'last_seen', n.last_seen, 'asn', n.asn, 'provider', n.provider,
            'country', n.country, 'ip_hash', n.ip_hash, 'fingerprint', n.fingerprint, 'clock_skew', n.clock_skew)
         FROM device_networks n JOIN devices d ON d.id = n.device_id WHERE d.user_id = $1 ORDER BY n.device_id, n.since",
    ),
    (
        "receipts",
        "SELECT json_build_object('device_id', r.device_id, 'seq', r.seq, 'ts', r.ts, 'server_ts', r.server_ts, 'valid', r.valid)
         FROM receipts r JOIN devices d ON d.id = r.device_id WHERE d.user_id = $1 ORDER BY r.device_id, r.seq",
    ),
    (
        "projects",
        "SELECT json_build_object('id', id, 'repo_url', repo_url, 'hidden', hidden, 'first_seen', first_seen, 'last_seen', last_seen)
         FROM projects WHERE user_id = $1 ORDER BY id",
    ),
    (
        "project_links",
        "SELECT json_build_object('from', from_id, 'to', to_id, 'source', source, 'at', at)
         FROM project_links WHERE user_id = $1 ORDER BY at, from_id, to_id",
    ),
    (
        "commits",
        "SELECT json_build_object('project', project, 'sha', sha, 'ts', ts, 'files', files, 'added', added, 'removed', removed, 'verified', verified)
         FROM commits WHERE user_id = $1 ORDER BY ts",
    ),
    (
        "days",
        "SELECT json_build_object('day', day, 'project', project, 'class', class, 'seconds', seconds)
         FROM user_days WHERE user_id = $1 ORDER BY day, project, class",
    ),
    (
        "spans",
        "SELECT json_build_object('device_id', device_id, 'start', start_ts, 'end', end_ts, 'project', project, 'class', class, 'rule', rule)
         FROM spans WHERE user_id = $1 ORDER BY start_ts",
    ),
    (
        "agent_runtime",
        "SELECT json_build_object('device_id', device_id, 'start', start_ts, 'end', end_ts, 'project', project)
         FROM agent_spans WHERE user_id = $1 ORDER BY start_ts",
    ),
    (
        "heartbeats",
        "SELECT json_build_object('time', time, 'project', project, 'language', language, 'editor', editor, 'branch', branch,
            'category', category, 'is_write', is_write) FROM heartbeats WHERE user_id = $1 ORDER BY time",
    ),
    (
        "token_hours",
        "SELECT json_build_object('device_id', device_id, 'hour', hour, 'harness', harness, 'model', model, 'project', project,
            'input', input, 'output', output, 'cache_read', cache_read, 'cache_write', cache_write)
         FROM token_hours WHERE user_id = $1 ORDER BY hour, device_id, harness, model, project",
    ),
    (
        "events",
        "SELECT json_build_object('device_id', e.device_id, 'seq', e.seq, 'received_at', e.received_at, 'event', e.body)
         FROM events e JOIN devices d ON d.id = e.device_id WHERE d.user_id = $1 ORDER BY e.device_id, e.seq",
    ),
];

async fn write(st: &St, user_id: i64, out: &mut Out) -> Result<()> {
    let mut c = st.pool.get().await?;
    let tx = c.build_transaction().isolation_level(IsolationLevel::RepeatableRead).read_only(true).start().await?;
    let profile: Value = tx
        .query_one(
            "SELECT json_build_object('id', id, 'username', username, 'display_name', display_name, 'email', email,
                'slack_id', slack_id, 'hca_id', hca_sub, 'verification_status', verification_status, 'ysws_eligible', ysws_eligible,
                'role', role, 'is_public', is_public, 'show_ai_tools', show_ai_tools, 'hide_projects', hide_projects, 'tokens_on', tokens_on, 'tokens_public', tokens_public, 'timezone', timezone, 'created_at', created_at)
             FROM users WHERE id = $1",
            &[&user_id],
        )
        .await?
        .get(0);
    out.push(&format!(
        "{{\"format\":\"kairos-export\",\"version\":1,\"exported_at\":{},\"profile\":{}",
        json!(now()),
        profile
    ))
    .await?;
    let params: [&(dyn ToSql + Sync); 1] = [&user_id];
    for (key, sql) in SECTIONS {
        out.push(&format!(",\"{key}\":[")).await?;
        let rows = tx.query_raw(*sql, params.iter().map(|p| *p as &dyn ToSql)).await?;
        pin_mut!(rows);
        let mut first = true;
        while let Some(row) = rows.next().await {
            let v: Value = row?.get(0);
            if !first {
                out.push(",").await?;
            }
            first = false;
            out.push(&v.to_string()).await?;
        }
        out.push("]").await?;
    }
    out.push("}\n").await?;
    out.flush().await?;
    tx.commit().await?;
    Ok(())
}

pub async fn export(State(st): State<St>, SignedIn(s): SignedIn) -> Res<Response> {
    limits::check(&st, Bucket::Export, &s.user.id.to_string())?;
    let (tx, rx) = mpsc::channel::<Bytes>(8);
    let user_id = s.user.id;
    let task_st = st.clone();
    tokio::spawn(async move {
        let mut out = Out { buf: String::new(), tx };
        if let Err(e) = write(&task_st, user_id, &mut out).await {
            tracing::warn!(error = format!("{e:#}"), "export stopped");
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|b| (Ok::<_, Infallible>(b), rx)) });
    let filename = format!("attachment; filename=\"kairos-{}.json\"", s.user.username);
    Ok((
        [
            (header::CONTENT_TYPE, "application/json".to_string()),
            (header::CONTENT_DISPOSITION, filename),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}
