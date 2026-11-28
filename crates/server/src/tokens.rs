use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::{DateTime, Days, NaiveDate, Utc};
use deadpool_postgres::GenericClient;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::device;
use crate::credit::day_start;
use crate::limits::{self, Bucket};
use crate::prices::{self, Counts, Price};
use crate::util::{err, now, Res};
use crate::web::pages::{tool_name, DayBar};
use crate::St;

const MAX_ROWS: usize = 2000;
// Devices send the last 30 days; anything much older than that is not a resend.
const MAX_AGE_SECS: i64 = 40 * 86400;
const MAX_COUNT: u64 = 1_000_000_000_000;

#[derive(Deserialize)]
pub struct Upload {
    hours: Vec<HourIn>,
}

#[derive(Deserialize)]
struct HourIn {
    hour: i64,
    harness: String,
    model: String,
    project: Option<String>,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

async fn tokens_on(c: &impl GenericClient, user_id: i64) -> Result<bool> {
    Ok(c.query_one("SELECT tokens_on FROM users WHERE id = $1", &[&user_id]).await?.get(0))
}

/// Whether the person wants their devices to read token counts.
pub async fn state(State(st): State<St>, h: HeaderMap) -> Res<Json<Value>> {
    let dev = device::auth(&st, &h).await?;
    let c = st.pool.get().await?;
    Ok(Json(json!({ "on": tokens_on(&c, dev.user_id).await? })))
}

fn clean_harness(s: &str) -> Option<String> {
    (!s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'))
        .then(|| s.to_string())
}

fn clean_text(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect::<String>().trim().to_string()
}

type Key = (i64, String, String, String);

/// Rows outside the window or with names that can't be stored are dropped, not refused: the rest still count.
fn accepted(hours: Vec<HourIn>, now: i64) -> HashMap<Key, Counts> {
    let mut out: HashMap<Key, Counts> = HashMap::new();
    for h in hours {
        let (Some(harness), model) = (clean_harness(&h.harness), clean_text(&h.model, 100)) else { continue };
        if model.is_empty() || h.hour % 3600 != 0 || h.hour < now - MAX_AGE_SECS || h.hour > now + 3600 {
            continue;
        }
        let project = h.project.as_deref().map(|p| clean_text(p, 200)).unwrap_or_default();
        let n = |v: u64| v.min(MAX_COUNT) as i64;
        let c = Counts { input: n(h.input), output: n(h.output), cache_read: n(h.cache_read), cache_write: n(h.cache_write) };
        let e = out.entry((h.hour, harness, model, project)).or_default();
        *e = Counts {
            input: e.input.max(c.input),
            output: e.output.max(c.output),
            cache_read: e.cache_read.max(c.cache_read),
            cache_write: e.cache_write.max(c.cache_write),
        };
    }
    out
}

/// Whole hours per device. A resend keeps the larger count, so a harness that cleaned up its old logs never shrinks them.
pub async fn upload(State(st): State<St>, h: HeaderMap, Json(up): Json<Upload>) -> Res<Json<Value>> {
    let dev = device::auth(&st, &h).await?;
    limits::check(&st, Bucket::Events, &format!("tokens:{}", dev.token_hash))?;
    if up.hours.len() > MAX_ROWS {
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "at most 2000 hours per request"));
    }
    let c = st.pool.get().await?;
    if !tokens_on(&c, dev.user_id).await? {
        return Ok(Json(json!({ "on": false })));
    }
    let rows = accepted(up.hours, now() as i64);
    let mut hour = Vec::with_capacity(rows.len());
    let (mut harness, mut model, mut project) = (Vec::new(), Vec::new(), Vec::new());
    let (mut input, mut output, mut read, mut write) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ((hr, ha, m, p), n) in rows {
        hour.push(hr as f64);
        harness.push(ha);
        model.push(m);
        project.push(p);
        input.push(n.input);
        output.push(n.output);
        read.push(n.cache_read);
        write.push(n.cache_write);
    }
    c.execute(
        "INSERT INTO token_hours (user_id, device_id, hour, harness, model, project, input, output, cache_read, cache_write)
         SELECT $1, $2, to_timestamp(h), ha, m, p, i, o, r, w
         FROM unnest($3::float8[], $4::text[], $5::text[], $6::text[], $7::int8[], $8::int8[], $9::int8[], $10::int8[])
              AS t(h, ha, m, p, i, o, r, w)
         ON CONFLICT (user_id, hour, device_id, harness, model, project) DO UPDATE SET
            input = GREATEST(token_hours.input, EXCLUDED.input), output = GREATEST(token_hours.output, EXCLUDED.output),
            cache_read = GREATEST(token_hours.cache_read, EXCLUDED.cache_read),
            cache_write = GREATEST(token_hours.cache_write, EXCLUDED.cache_write)",
        &[&dev.user_id, &dev.id, &hour, &harness, &model, &project, &input, &output, &read, &write],
    )
    .await?;
    Ok(Json(json!({ "on": true })))
}

pub fn money(dollars: f64) -> String {
    if dollars <= 0.0 {
        "$0".into()
    } else if dollars < 0.01 {
        "<$0.01".into()
    } else if dollars < 1000.0 {
        format!("${dollars:.2}")
    } else {
        let whole = format!("{:.0}", dollars);
        let mut out = String::new();
        for (i, ch) in whole.chars().enumerate() {
            if i > 0 && (whole.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(ch);
        }
        format!("${out}")
    }
}

pub fn count(n: i64) -> String {
    let n = n.max(0) as f64;
    let (v, unit) = if n >= 1e9 {
        (n / 1e9, "B")
    } else if n >= 1e6 {
        (n / 1e6, "M")
    } else if n >= 1e3 {
        (n / 1e3, "K")
    } else {
        return format!("{n}");
    };
    if v >= 100.0 {
        format!("{v:.0}{unit}")
    } else {
        format!("{}{unit}", format!("{v:.1}").trim_end_matches(".0"))
    }
}

fn tokens_text(n: i64) -> String {
    format!("{} {}", count(n), if n == 1 { "token" } else { "tokens" })
}

struct DayRow {
    day: NaiveDate,
    harness: String,
    model: String,
    n: Counts,
}

/// Token counts per local day, AI tool and model, for days `from` through `to`.
async fn load(c: &impl GenericClient, user_id: i64, tz: chrono_tz::Tz, from: NaiveDate, to: NaiveDate, projects: Option<&[String]>) -> Result<Vec<DayRow>> {
    let lo = day_start(tz, from) - 86400.0;
    let hi = day_start(tz, to.succ_opt().unwrap_or(to)) + 86400.0;
    let rows = c
        .query(
            "SELECT hour, harness, model, sum(input)::int8, sum(output)::int8, sum(cache_read)::int8, sum(cache_write)::int8
             FROM token_hours
             WHERE user_id = $1 AND hour >= to_timestamp($2) AND hour < to_timestamp($3) AND ($4::text[] IS NULL OR project = ANY($4))
             GROUP BY 1, 2, 3",
            &[&user_id, &lo, &hi, &projects],
        )
        .await?;
    // The local day is worked out here: Postgres doesn't know every zone name chrono-tz does (Asia/Calcutta).
    let mut days: BTreeMap<(NaiveDate, String, String), Counts> = BTreeMap::new();
    for r in &rows {
        let day = r.get::<_, DateTime<Utc>>(0).with_timezone(&tz).date_naive();
        if day < from || day > to {
            continue;
        }
        let n = Counts { input: r.get(3), output: r.get(4), cache_read: r.get(5), cache_write: r.get(6) };
        days.entry((day, r.get(1), r.get(2))).or_default().add(&n);
    }
    Ok(days.into_iter().map(|((day, harness, model), n)| DayRow { day, harness, model, n }).collect())
}

pub struct Stat {
    pub label: &'static str,
    pub cost: String,
    pub tokens: String,
    pub split: Vec<(&'static str, String)>,
}

/// A row of a token list: the bar is the share of tokens, the value what the list is sorted by.
pub struct TRow {
    pub label: String,
    pub pct: String,
    pub value: String,
    pub hint: String,
}

pub struct View {
    pub stats: Vec<Stat>,
    pub days: Vec<DayBar>,
    pub first_label: String,
    pub tools: Vec<TRow>,
    pub models: Vec<TRow>,
    pub pickable: bool,
    pub dim: bool,
    pub unpriced: bool,
    pub empty: bool,
}

pub struct ProjectView {
    pub cost: String,
    pub tokens: String,
    pub per_hour: Option<String>,
    pub models: Vec<TRow>,
    pub unpriced: bool,
    pub empty: bool,
}

struct Priced {
    prices: HashMap<String, Price>,
}

impl Priced {
    async fn of(c: &impl GenericClient, rows: &[DayRow]) -> Result<Priced> {
        let mut models: Vec<String> = rows.iter().map(|r| r.model.clone()).collect();
        models.sort();
        models.dedup();
        Ok(Priced { prices: prices::lookup(c, &models).await? })
    }

    fn cost(&self, model: &str, n: &Counts) -> Option<f64> {
        self.prices.get(model).map(|p| p.cost(n))
    }
}

struct Sum {
    n: Counts,
    cost: f64,
    unpriced: bool,
}

fn sum<'a>(rows: impl Iterator<Item = &'a DayRow>, priced: &Priced) -> Sum {
    let mut s = Sum { n: Counts::default(), cost: 0.0, unpriced: false };
    for r in rows {
        s.n.add(&r.n);
        match priced.cost(&r.model, &r.n) {
            Some(c) => s.cost += c,
            None => s.unpriced |= r.n.total() > 0,
        }
    }
    s
}

fn tool_rows(rows: &[&DayRow], priced: &Priced) -> Vec<TRow> {
    let mut by: BTreeMap<String, Vec<&DayRow>> = BTreeMap::new();
    for &r in rows {
        by.entry(r.harness.clone()).or_default().push(r);
    }
    let mut list: Vec<(String, Sum)> = by.into_iter().map(|(h, rs)| (h, sum(rs.into_iter(), priced))).filter(|s| s.1.n.total() > 0).collect();
    list.sort_by(|a, b| b.1.n.total().cmp(&a.1.n.total()).then(a.0.cmp(&b.0)));
    let max = list.first().map_or(0, |s| s.1.n.total()) as f64;
    list.into_iter()
        .take(12)
        .map(|(h, s)| TRow {
            label: tool_name(&h),
            pct: format!("{:.1}", if max > 0.0 { s.n.total() as f64 / max * 100.0 } else { 0.0 }),
            value: tokens_text(s.n.total()),
            hint: format!("{} at API prices", money(s.cost)),
        })
        .collect()
}

fn model_rows(rows: &[&DayRow], priced: &Priced) -> Vec<TRow> {
    let mut by: BTreeMap<String, Vec<&DayRow>> = BTreeMap::new();
    for &r in rows {
        by.entry(r.model.clone()).or_default().push(r);
    }
    let mut list: Vec<(String, Sum, bool)> = by
        .into_iter()
        .map(|(m, rs)| {
            let has = priced.prices.contains_key(&m);
            (m, sum(rs.into_iter(), priced), has)
        })
        .filter(|s| s.1.n.total() > 0)
        .collect();
    list.sort_by(|a, b| b.1.cost.total_cmp(&a.1.cost).then(b.1.n.total().cmp(&a.1.n.total())).then(a.0.cmp(&b.0)));
    let max = list.iter().map(|s| s.1.n.total()).max().unwrap_or(0) as f64;
    list.into_iter()
        .take(12)
        .map(|(m, s, has)| TRow {
            label: m,
            pct: format!("{:.1}", if max > 0.0 { s.n.total() as f64 / max * 100.0 } else { 0.0 }),
            value: if has { money(s.cost) } else { "no price".into() },
            hint: tokens_text(s.n.total()),
        })
        .collect()
}

pub struct Settings {
    pub on: bool,
    pub public: bool,
}

pub async fn settings(c: &impl GenericClient, user_id: i64) -> Result<Settings> {
    let r = c.query_one("SELECT tokens_on, tokens_public FROM users WHERE id = $1", &[&user_id]).await?;
    Ok(Settings { on: r.get(0), public: r.get(1) })
}

pub async fn any(c: &impl GenericClient, user_id: i64, projects: Option<&[String]>) -> Result<bool> {
    Ok(c
        .query_opt("SELECT 1 FROM token_hours WHERE user_id = $1 AND ($2::text[] IS NULL OR project = ANY($2)) LIMIT 1", &[&user_id, &projects])
        .await?
        .is_some())
}

/// Today, the last 7 and 30 days, a 30 day chart, and the tools and models of the picked days
/// (`pick`, the dashboard) or of the last 30 days (a profile).
pub async fn view(c: &impl GenericClient, user_id: i64, tz: chrono_tz::Tz, today: NaiveDate, pick: Option<(NaiveDate, NaiveDate)>) -> Result<View> {
    let ago = |n| today.checked_sub_days(Days::new(n)).unwrap_or(today);
    let month = ago(29);
    let from = pick.as_ref().map_or(month, |p| p.0.min(month));
    let rows = load(c, user_id, tz, from, today, None).await?;
    let priced = Priced::of(c, &rows).await?;
    let since = |d: NaiveDate| sum(rows.iter().filter(|r| r.day >= d), &priced);
    let stat = |label, s: Sum| Stat {
        label,
        cost: money(s.cost),
        tokens: tokens_text(s.n.total()),
        split: if s.n.total() > 0 {
            vec![("In", count(s.n.input)), ("Out", count(s.n.output)), ("Cache read", count(s.n.cache_read)), ("Cache write", count(s.n.cache_write))]
        } else {
            Vec::new()
        },
    };
    let (t, w, m) = (since(today), since(ago(6)), since(month));
    let (unpriced_month, month_total) = (m.unpriced, m.n.total());
    let stats = vec![stat("Today", t), stat("This week", w), stat("Last 30 days", m)];

    let mut per_day: BTreeMap<NaiveDate, Vec<&DayRow>> = BTreeMap::new();
    for r in &rows {
        per_day.entry(r.day).or_default().push(r);
    }
    let dates: Vec<NaiveDate> = (0..30).rev().map(ago).collect();
    let day_sums: Vec<(NaiveDate, Sum)> = dates.iter().map(|d| (*d, sum(per_day.get(d).into_iter().flatten().copied(), &priced))).collect();
    let max = day_sums.iter().map(|d| d.1.n.total()).max().unwrap_or(0) as f64;
    let (lo, hi) = pick.as_ref().map_or((month, today), |p| (p.0, p.1));
    let days = day_sums
        .iter()
        .map(|(d, s)| DayBar {
            title: format!("{} · {} · {}", d.format("%a %b %-d"), tokens_text(s.n.total()), money(s.cost)),
            pct: format!("{:.1}", if max > 0.0 { s.n.total() as f64 / max * 100.0 } else { 0.0 }),
            day: d.to_string(),
            on: pick.is_some() && (lo..=hi).contains(d),
        })
        .collect();
    let picked: Vec<&DayRow> = rows.iter().filter(|r| r.day >= lo && r.day <= hi).collect();
    let p = sum(picked.iter().copied(), &priced);
    Ok(View {
        stats,
        days,
        first_label: month.format("%b %-d").to_string(),
        tools: tool_rows(&picked, &priced),
        models: model_rows(&picked, &priced),
        dim: pick.is_some() && (lo, hi) != (month, today),
        pickable: pick.is_some(),
        unpriced: unpriced_month || p.unpriced,
        empty: month_total == 0 && p.n.total() == 0,
    })
}

/// A project's tokens and cost for the picked days, with the cost per hour of focus time in it on those days.
pub async fn project_view(c: &impl GenericClient, user_id: i64, tz: chrono_tz::Tz, projects: &[String], from: NaiveDate, to: NaiveDate, focus_secs: f64) -> Result<ProjectView> {
    let rows = load(c, user_id, tz, from, to, Some(projects)).await?;
    let priced = Priced::of(c, &rows).await?;
    let all: Vec<&DayRow> = rows.iter().collect();
    let s = sum(all.iter().copied(), &priced);
    let priced_any = rows.iter().any(|r| priced.prices.contains_key(&r.model) && r.n.total() > 0);
    Ok(ProjectView {
        cost: money(s.cost),
        tokens: count(s.n.total()),
        per_hour: (focus_secs >= 60.0 && priced_any).then(|| money(s.cost / (focus_secs / 3600.0))),
        models: model_rows(&all, &priced),
        unpriced: s.unpriced,
        empty: s.n.total() == 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(hour: i64, model: &str, input: u64) -> HourIn {
        HourIn { hour, harness: "claude-code".into(), model: model.into(), project: None, input, output: 1, cache_read: 0, cache_write: 0 }
    }

    #[test]
    fn money_and_counts_read_like_a_person_would_say_them() {
        assert_eq!((money(0.0), money(0.004), money(1.5), money(12.346), money(1234.4), money(1_234_567.0)), ("$0".into(), "<$0.01".into(), "$1.50".into(), "$12.35".into(), "$1,234".into(), "$1,234,567".into()));
        assert_eq!((count(0), count(999), count(1000), count(1340), count(123_456), count(4_100_000), count(2_000_000_000)), ("0".into(), "999".into(), "1K".into(), "1.3K".into(), "123K".into(), "4.1M".into(), "2B".into()));
    }

    #[test]
    fn only_whole_recent_hours_with_clean_names_are_kept() {
        let now = 1_800_000_000 - 1_800_000_000 % 3600;
        let mut bad_harness = h(now, "m", 1);
        bad_harness.harness = "Claude Code".into();
        let mut control = h(now - 3600, "a\u{7}b", 5);
        control.project = Some("p\nq".into());
        let rows = accepted(
            vec![h(now, "m", 1), h(now, "m", 9), h(now + 1, "m", 1), h(now - 41 * 86400, "m", 1), h(now + 7200, "m", 1), h(now, " ", 1), bad_harness, control],
            now,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[&(now, "claude-code".into(), "m".into(), String::new())].input, 9);
        assert_eq!(rows[&(now - 3600, "claude-code".into(), "ab".into(), "pq".into())].input, 5);
    }
}
