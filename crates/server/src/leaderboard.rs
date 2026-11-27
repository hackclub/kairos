//! Each user's days are their own local days, so "today" means today where that person is.
//! Only public, non-banned profiles are listed.

use anyhow::Result;
use deadpool_postgres::GenericClient;

use crate::credit::todays;
use crate::util::now;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Today,
    Week,
}

impl Period {
    pub fn parse(s: &str) -> Option<Period> {
        match s {
            "today" | "daily" => Some(Period::Today),
            "week" | "weekly" | "last_7_days" => Some(Period::Week),
            _ => None,
        }
    }

    fn days(self) -> i32 {
        match self {
            Period::Today => 1,
            Period::Week => 7,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub rank: u32,
    pub user_id: i64,
    pub username: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub seconds: f64,
}

pub async fn top(c: &impl GenericClient, period: Period, limit: i64) -> Result<Vec<Entry>> {
    let (zones, days, utc) = todays(now());
    let rows = c
        .query(
            "SELECT u.id, u.username, u.display_name, u.avatar_url, sum(d.seconds) AS secs
             FROM user_days d JOIN users u ON u.id = d.user_id
             LEFT JOIN unnest($3::text[], $4::date[]) AS z(name, today) ON z.name = u.timezone
             WHERE u.is_public AND u.trust <> 'banned' AND d.class <> 'agent'
               AND d.day > coalesce(z.today, $5) - $1::int AND d.day <= coalesce(z.today, $5)
             GROUP BY u.id HAVING sum(d.seconds) >= 60
             ORDER BY secs DESC, u.id LIMIT $2",
            &[&period.days(), &limit, &zones, &days, &utc],
        )
        .await?;
    Ok(rows
        .iter()
        .enumerate()
        .map(|(i, r)| Entry {
            rank: i as u32 + 1,
            user_id: r.get(0),
            username: r.get(1),
            display_name: r.get(2),
            avatar_url: r.get(3),
            seconds: r.get(4),
        })
        .collect())
}
