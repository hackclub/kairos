use std::time::Duration;

use anyhow::{bail, Context, Result};
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use tokio_postgres::NoTls;

const MIGRATIONS: &[(i32, &str, &str)] = &[
    (1, "init", include_str!("../migrations/0001_init.sql")),
    (2, "tool_days", include_str!("../migrations/0002_tool_days.sql")),
    (3, "review_notes", include_str!("../migrations/0003_review_notes.sql")),
    (4, "commit_evidence", include_str!("../migrations/0004_commit_evidence.sql")),
    (5, "credit_revision", include_str!("../migrations/0005_credit_revision.sql")),
    (6, "timezone_set", include_str!("../migrations/0006_timezone_set.sql")),
    (7, "project_aliases", include_str!("../migrations/0007_project_aliases.sql")),
    (8, "departed", include_str!("../migrations/0008_departed.sql")),
    (9, "device_grants", include_str!("../migrations/0009_device_grants.sql")),
    (10, "sign_in_repeat", include_str!("../migrations/0010_sign_in_repeat.sql")),
    (11, "commit_not_before", include_str!("../migrations/0011_commit_not_before.sql")),
];
const MIGRATION_LOCK: i64 = 0x6b61_6972_6f73;

pub fn pool(url: &str) -> Result<Pool> {
    pool_from(url.parse().context("DATABASE_URL is not a valid Postgres URL")?)
}

pub fn pool_from(cfg: tokio_postgres::Config) -> Result<Pool> {
    let mgr = Manager::from_config(cfg, NoTls, ManagerConfig { recycling_method: RecyclingMethod::Fast });
    // A handler must not hold a connection while something it calls takes another: with every
    // connection held that way the pool can never hand one out. The wait timeout turns such a
    // mistake into failed requests instead of a server that hangs.
    Ok(Pool::builder(mgr).max_size(16).runtime(Runtime::Tokio1).wait_timeout(Some(Duration::from_secs(10))).build()?)
}

/// Each migration runs in its own transaction under an advisory lock, so two servers starting at once never race.
pub async fn migrate(pool: &Pool) -> Result<Vec<i32>> {
    let mut c = pool.get().await?;
    c.execute("SELECT pg_advisory_lock($1)", &[&MIGRATION_LOCK]).await?;
    let res = apply(&mut c).await;
    let unlocked = c.execute("SELECT pg_advisory_unlock($1)", &[&MIGRATION_LOCK]).await;
    let applied = res?;
    unlocked?;
    Ok(applied)
}

async fn apply(c: &mut deadpool_postgres::Client) -> Result<Vec<i32>> {
    c.batch_execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INT PRIMARY KEY,
            name TEXT NOT NULL,
            applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
    )
    .await?;
    let newest: Option<i32> = c.query_one("SELECT max(version) FROM schema_migrations", &[]).await?.get(0);
    let known = MIGRATIONS.last().map_or(0, |m| m.0);
    if newest.unwrap_or(0) > known {
        bail!("the database has migration {} but this build only knows up to {known}", newest.unwrap_or(0));
    }
    let mut applied = Vec::new();
    for &(version, name, sql) in MIGRATIONS {
        let tx = c.transaction().await?;
        if tx.query_opt("SELECT 1 FROM schema_migrations WHERE version = $1", &[&version]).await?.is_some() {
            continue;
        }
        tx.batch_execute(sql).await.with_context(|| format!("migration {version:04}_{name}"))?;
        tx.execute("INSERT INTO schema_migrations (version, name) VALUES ($1, $2)", &[&version, &name]).await?;
        tx.commit().await?;
        tracing::info!(version, name, "applied migration");
        applied.push(version);
    }
    Ok(applied)
}
