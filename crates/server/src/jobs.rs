use std::time::Duration;

use crate::St;

const EVERY: Duration = Duration::from_secs(300);

pub async fn cleanup(st: &St) -> anyhow::Result<()> {
    let c = st.pool.get().await?;
    c.execute("DELETE FROM web_sessions WHERE expires_at < now()", &[]).await?;
    c.execute("DELETE FROM auth_flows WHERE expires_at < now()", &[]).await?;
    c.execute("DELETE FROM device_codes WHERE expires_at < now() - interval '1 hour'", &[]).await?;
    c.execute("DELETE FROM device_grants WHERE expires_at < now()", &[]).await?;
    st.limits.cleanup();
    if let Some(m) = &st.mock {
        m.cleanup();
    }
    Ok(())
}

pub fn spawn(st: St) {
    crate::prices::spawn(st.clone());
    let pool = st.pool.clone();
    tokio::spawn(async move {
        match crate::credit::rebuild_stale(&pool).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(users = n, revision = kairos_core::credit::REVISION, "rebuilt stored credit"),
            Err(e) => tracing::warn!(error = format!("{e:#}"), "credit rebuild stopped"),
        }
    });
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(EVERY);
        loop {
            tick.tick().await;
            if let Err(e) = cleanup(&st).await {
                tracing::warn!(error = format!("{e:#}"), "cleanup failed");
            }
        }
    });
}
