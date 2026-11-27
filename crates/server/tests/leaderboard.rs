mod common;

use chrono::{Days, Utc};
use common::TestApp;
use kairos_server::leaderboard::{self, Period};
use kairos_server::streak;

async fn user(app: &TestApp, name: &str, public: bool, trust: &str, tz: &str) -> i64 {
    let c = app.st.pool.get().await.unwrap();
    c.query_one(
        "INSERT INTO users (hca_sub, username, is_public, trust, timezone) VALUES ($1, $1, $2, $3, $4) RETURNING id",
        &[&name, &public, &trust, &tz],
    )
    .await
    .unwrap()
    .get(0)
}

async fn day(app: &TestApp, user_id: i64, days_ago: u64, class: &str, seconds: f64) {
    let today = Utc::now().date_naive();
    let d = today.checked_sub_days(Days::new(days_ago)).unwrap();
    let c = app.st.pool.get().await.unwrap();
    c.execute(
        "INSERT INTO user_days (user_id, day, project, class, seconds) VALUES ($1, $2, 'o/p', $3, $4)",
        &[&user_id, &d, &class, &seconds],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn leaderboards_rank_public_focus_time_only() {
    let app = TestApp::new().await;
    let ada = user(&app, "ada", true, "unscored", "UTC").await;
    let bob = user(&app, "bob", true, "trusted", "UTC").await;
    let eve = user(&app, "eve", false, "unscored", "UTC").await;
    let mal = user(&app, "mal", true, "banned", "UTC").await;
    let odd = user(&app, "odd", true, "unscored", "Not/AZone").await;
    day(&app, ada, 0, "typing", 1200.0).await;
    day(&app, ada, 3, "editing", 3000.0).await;
    day(&app, bob, 0, "editing", 1800.0).await;
    day(&app, bob, 0, "agent", 99999.0).await;
    day(&app, bob, 9, "editing", 99999.0).await;
    day(&app, eve, 0, "typing", 5000.0).await;
    day(&app, mal, 0, "typing", 5000.0).await;
    day(&app, odd, 0, "typing", 600.0).await;

    let c = app.st.pool.get().await.unwrap();
    let today = leaderboard::top(&c, Period::Today, 10).await.unwrap();
    let names: Vec<_> = today.iter().map(|e| (e.rank, e.username.as_str(), e.seconds)).collect();
    assert_eq!(names, vec![(1, "bob", 1800.0), (2, "ada", 1200.0), (3, "odd", 600.0)]);

    let week = leaderboard::top(&c, Period::Week, 10).await.unwrap();
    let names: Vec<_> = week.iter().map(|e| (e.username.as_str(), e.seconds)).collect();
    assert_eq!(names, vec![("ada", 4200.0), ("bob", 1800.0), ("odd", 600.0)]);
    drop(c);
    app.finish().await;
}

#[tokio::test]
async fn streak_counts_days_with_fifteen_minutes() {
    let app = TestApp::new().await;
    let ada = user(&app, "ada", true, "unscored", "UTC").await;
    day(&app, ada, 1, "typing", 600.0).await;
    day(&app, ada, 1, "reading", 300.0).await;
    day(&app, ada, 2, "editing", 900.0).await;
    day(&app, ada, 3, "agent", 5000.0).await;
    let c = app.st.pool.get().await.unwrap();
    let today = Utc::now().date_naive();
    assert_eq!(streak::current(&c, ada, today).await.unwrap(), 2);
    drop(c);
    app.finish().await;
}

#[tokio::test]
async fn a_zone_name_postgres_may_not_know_still_gets_its_own_today() {
    let app = TestApp::new().await;
    let ind = user(&app, "ind", true, "unscored", "Asia/Calcutta").await;
    let today = Utc::now().with_timezone(&chrono_tz::Asia::Calcutta).date_naive();
    let c = app.st.pool.get().await.unwrap();
    c.execute("INSERT INTO user_days (user_id, day, project, class, seconds) VALUES ($1, $2, 'o/p', 'typing', 900)", &[&ind, &today])
        .await
        .unwrap();
    let top = leaderboard::top(&c, Period::Today, 10).await.unwrap();
    assert_eq!(top.iter().map(|e| (e.username.as_str(), e.seconds)).collect::<Vec<_>>(), [("ind", 900.0)]);
    drop(c);
    app.finish().await;
}
