use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use deadpool_postgres::GenericClient;
use serde_json::Value;

use crate::St;

const URL: &str = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const EVERY: Duration = Duration::from_secs(24 * 3600);
const MAX_JUMP: f64 = 5.0;
// The real list has thousands of models; a short one is a broken download, not the new prices.
const MIN_MODELS: usize = 100;

/// Dollars per token. A missing cache price means the model bills those tokens as plain input.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Counts {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

impl Counts {
    pub fn total(&self) -> i64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    pub fn add(&mut self, o: &Counts) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
    }
}

impl Price {
    pub fn cost(&self, n: &Counts) -> f64 {
        n.input as f64 * self.input
            + n.output as f64 * self.output
            + n.cache_read as f64 * self.cache_read.unwrap_or(self.input)
            + n.cache_write as f64 * self.cache_write.unwrap_or(self.input)
    }
}

fn price_of(v: &Value) -> Option<Price> {
    let num = |k: &str| v.get(k).and_then(Value::as_f64).filter(|x| x.is_finite() && *x >= 0.0);
    Some(Price {
        input: num("input_cost_per_token")?,
        output: num("output_cost_per_token")?,
        cache_read: num("cache_read_input_token_cost"),
        cache_write: num("cache_creation_input_token_cost"),
    })
}

pub fn parse(text: &str) -> Result<HashMap<String, Price>> {
    let v: Value = serde_json::from_str(text).context("the price list is not JSON")?;
    let list: HashMap<String, Price> = v
        .as_object()
        .context("the price list is not an object")?
        .iter()
        .filter(|(k, _)| k.as_str() != "sample_spec" && k.len() <= 200)
        .filter_map(|(k, v)| Some((k.to_lowercase(), price_of(v)?)))
        .collect();
    if list.len() < MIN_MODELS {
        bail!("the price list has only {} priced models", list.len());
    }
    Ok(list)
}

fn jumped(old: f64, new: f64) -> bool {
    if old == new {
        return false;
    }
    if old <= 0.0 || new <= 0.0 {
        return true;
    }
    (new / old).max(old / new) > MAX_JUMP
}

fn moved_too_far(old: &Price, new: &Price) -> bool {
    let opt = |a: Option<f64>, b: Option<f64>| matches!((a, b), (Some(a), Some(b)) if jumped(a, b));
    jumped(old.input, new.input)
        || jumped(old.output, new.output)
        || opt(old.cache_read, new.cache_read)
        || opt(old.cache_write, new.cache_write)
}

/// The prices to write: new models and moves within 5x. A known model whose price moved further keeps its last good price.
pub fn merge(old: &HashMap<String, Price>, new: HashMap<String, Price>) -> (Vec<(String, Price)>, Vec<String>) {
    let mut write = Vec::new();
    let mut kept = Vec::new();
    for (model, p) in new {
        match old.get(&model) {
            Some(o) if *o == p => {}
            Some(o) if moved_too_far(o, &p) => kept.push(model),
            _ => write.push((model, p)),
        }
    }
    kept.sort();
    (write, kept)
}

async fn stored(c: &impl GenericClient) -> Result<HashMap<String, Price>> {
    Ok(c.query("SELECT model, input, output, cache_read, cache_write FROM model_prices", &[])
        .await?
        .iter()
        .map(|r| (r.get(0), Price { input: r.get(1), output: r.get(2), cache_read: r.get(3), cache_write: r.get(4) }))
        .collect())
}

pub async fn store(c: &impl GenericClient, rows: &[(String, Price)]) -> Result<()> {
    for chunk in rows.chunks(500) {
        let models: Vec<&str> = chunk.iter().map(|r| r.0.as_str()).collect();
        let input: Vec<f64> = chunk.iter().map(|r| r.1.input).collect();
        let output: Vec<f64> = chunk.iter().map(|r| r.1.output).collect();
        let read: Vec<Option<f64>> = chunk.iter().map(|r| r.1.cache_read).collect();
        let write: Vec<Option<f64>> = chunk.iter().map(|r| r.1.cache_write).collect();
        c.execute(
            "INSERT INTO model_prices (model, input, output, cache_read, cache_write)
             SELECT * FROM unnest($1::text[], $2::float8[], $3::float8[], $4::float8[], $5::float8[])
             ON CONFLICT (model) DO UPDATE SET input = EXCLUDED.input, output = EXCLUDED.output,
                cache_read = EXCLUDED.cache_read, cache_write = EXCLUDED.cache_write, updated_at = now()",
            &[&models, &input, &output, &read, &write],
        )
        .await?;
    }
    Ok(())
}

pub async fn refresh(st: &St) -> Result<()> {
    let text = st.http.get(URL).send().await?.error_for_status()?.text().await?;
    let new = parse(&text)?;
    let c = st.pool.get().await?;
    let old = stored(&c).await?;
    let (write, kept) = merge(&old, new);
    if !kept.is_empty() {
        tracing::warn!(models = kept.join(", "), "prices moved more than 5x, kept the last good ones");
    }
    store(&c, &write).await?;
    tracing::info!(updated = write.len(), "model prices refreshed");
    Ok(())
}

pub fn spawn(st: St) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await;
        loop {
            if let Err(e) = refresh(&st).await {
                tracing::warn!(error = format!("{e:#}"), "model prices not refreshed, keeping the last good ones");
            }
            tokio::time::sleep(EVERY).await;
        }
    });
}

/// Names a harness writes for a model, most exact first, in the forms LiteLLM keys use: a provider in front
/// (`anthropic/…`), a release date (`-20251101`), dots for dashes (`4.5`), a reasoning effort (`-high`).
pub fn candidates(model: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: String| {
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    };
    let m = model.trim().to_lowercase();
    let mut base = vec![m.clone()];
    if let Some((_, tail)) = m.rsplit_once('/') {
        base.push(tail.to_string());
    }
    for b in base {
        let undated = strip_date(&b);
        let plain = strip_effort(&undated);
        for s in [b.clone(), undated.clone(), plain.clone()] {
            push(s.clone());
            if s.contains('.') {
                push(s.replace('.', "-"));
            }
        }
    }
    out
}

fn strip_date(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() > 9 && matches!(b[b.len() - 9], b'-' | b'@') && b[b.len() - 8..].iter().all(u8::is_ascii_digit) {
        s[..s.len() - 9].to_string()
    } else {
        s.to_string()
    }
}

fn strip_effort(s: &str) -> String {
    ["-minimal", "-low", "-medium", "-high", "-xhigh", "-thinking"]
        .iter()
        .find_map(|e| s.strip_suffix(e))
        .unwrap_or(s)
        .to_string()
}

/// The price of each model that has one.
pub async fn lookup(c: &impl GenericClient, models: &[String]) -> Result<HashMap<String, Price>> {
    let cands: Vec<(String, Vec<String>)> = models.iter().map(|m| (m.clone(), candidates(m))).collect();
    let all: Vec<&str> = cands.iter().flat_map(|c| c.1.iter().map(String::as_str)).collect();
    let found: HashMap<String, Price> = c
        .query("SELECT model, input, output, cache_read, cache_write FROM model_prices WHERE model = ANY($1)", &[&all])
        .await?
        .iter()
        .map(|r| (r.get(0), Price { input: r.get(1), output: r.get(2), cache_read: r.get(3), cache_write: r.get(4) }))
        .collect();
    Ok(cands.into_iter().filter_map(|(m, list)| list.iter().find_map(|k| found.get(k)).map(|p| (m, *p))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(input: f64, output: f64) -> Price {
        Price { input, output, cache_read: None, cache_write: None }
    }

    #[test]
    fn names_try_the_exact_one_first_then_plainer_forms() {
        assert_eq!(candidates("claude-opus-5-5"), vec!["claude-opus-5-5"]);
        assert_eq!(candidates("claude-sonnet-4-5-20250929"), vec!["claude-sonnet-4-5-20250929", "claude-sonnet-4-5"]);
        assert_eq!(candidates("gemini-3.8-flash-high"), vec!["gemini-3.8-flash-high", "gemini-3-8-flash-high", "gemini-3.8-flash", "gemini-3-8-flash"]);
        assert_eq!(candidates("Z-AI/GLM-5.3"), vec!["z-ai/glm-5.3", "z-ai/glm-5-3", "glm-5.3", "glm-5-3"]);
    }

    #[test]
    fn the_list_needs_both_prices_and_enough_models() {
        let mut m = serde_json::Map::new();
        m.insert("sample_spec".into(), serde_json::json!({ "input_cost_per_token": 1.0, "output_cost_per_token": 1.0 }));
        m.insert("embed-only".into(), serde_json::json!({ "input_cost_per_token": 1e-7 }));
        for i in 0..MIN_MODELS {
            m.insert(format!("Model-{i}"), serde_json::json!({ "input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6, "cache_read_input_token_cost": 1e-7 }));
        }
        let list = parse(&Value::Object(m.clone()).to_string()).unwrap();
        assert_eq!(list.len(), MIN_MODELS);
        assert_eq!(list["model-0"], Price { input: 1e-6, output: 2e-6, cache_read: Some(1e-7), cache_write: None });
        m.remove("Model-0");
        assert!(parse(&Value::Object(m).to_string()).is_err());
        assert!(parse("<html>").is_err());
    }

    #[test]
    fn a_price_that_moves_more_than_five_times_keeps_the_old_one() {
        let old = HashMap::from([("a".to_string(), p(1.0, 2.0)), ("b".to_string(), p(1.0, 2.0)), ("c".to_string(), p(1.0, 2.0))]);
        let new = HashMap::from([
            ("a".to_string(), p(1.0, 2.0)),
            ("b".to_string(), p(4.0, 2.0)),
            ("c".to_string(), p(1.0, 20.0)),
            ("d".to_string(), p(9.0, 9.0)),
        ]);
        let (mut write, kept) = merge(&old, new);
        write.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(write, vec![("b".to_string(), p(4.0, 2.0)), ("d".to_string(), p(9.0, 9.0))]);
        assert_eq!(kept, vec!["c"]);
        assert!(jumped(0.0, 1.0) && jumped(1.0, 0.0) && !jumped(0.0, 0.0));
    }

    #[test]
    fn cache_tokens_cost_their_own_price_or_plain_input() {
        let n = Counts { input: 1000, output: 100, cache_read: 10_000, cache_write: 500 };
        let full = Price { input: 3e-6, output: 15e-6, cache_read: Some(0.3e-6), cache_write: Some(3.75e-6) };
        assert!((full.cost(&n) - (0.003 + 0.0015 + 0.003 + 0.001875)).abs() < 1e-12);
        assert!((p(1e-6, 2e-6).cost(&n) - (0.001 + 0.0002 + 0.01 + 0.0005)).abs() < 1e-12);
    }
}
