//! The server's own time, handed to a device with each accepted upload and sealed into its log by a later one. Every
//! event after a receipt was made after that time, whatever the device's clock said. The mac ties a receipt to its
//! device, so one can't be made up for a time the server never handed out.

use anyhow::Result;
use deadpool_postgres::GenericClient;
use serde_json::Value;

pub const HEADER: &str = "kairos-receipt";

pub fn key(secret: &str) -> [u8; 32] {
    blake3::derive_key("kairos 2026-11-28 upload receipt", secret.as_bytes())
}

fn mac(key: &[u8; 32], device: i64, at: i64) -> blake3::Hash {
    blake3::keyed_hash(key, format!("{device}.{at}").as_bytes())
}

pub fn issue(key: Option<&[u8; 32]>, device: i64, at: i64) -> String {
    match key {
        Some(k) => format!("{at}.{}", mac(k, device, at).to_hex()),
        None => at.to_string(),
    }
}

/// The time a receipt says, and whether its mac checks out (None when the server has no key to check it with).
pub fn check(key: Option<&[u8; 32]>, device: i64, token: &str, now: f64) -> Option<(i64, Option<bool>)> {
    let (at, m) = match token.split_once('.') {
        Some((at, m)) => (at, Some(m)),
        None => (token, None),
    };
    let at: i64 = at.parse().ok().filter(|t| *t > 0 && (*t as f64) <= now + 1.0)?;
    // blake3::Hash compares in constant time.
    let valid = key.map(|k| m.and_then(|m| blake3::Hash::from_hex(m).ok()).is_some_and(|h| h == mac(k, device, at)));
    Some((at, valid))
}

/// `events` and `seqs` are one upload's events and the seq each was stored at.
pub async fn store(c: &impl GenericClient, key: Option<&[u8; 32]>, device: i64, events: &[Value], seqs: &[i64], now: f64) -> Result<()> {
    for (v, seq) in events.iter().zip(seqs) {
        if v.get("kind").and_then(Value::as_str) != Some("receipt") {
            continue;
        }
        let (Some(ts), Some(token)) = (v.get("ts").and_then(Value::as_f64), v.get("token").and_then(Value::as_str)) else { continue };
        let Some((at, valid)) = check(key, device, token, now) else { continue };
        c.execute(
            "INSERT INTO receipts (device_id, seq, ts, server_ts, valid) VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
            &[&device, seq, &ts, &at, &valid],
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_receipt_checks_out_only_for_its_device_and_time() {
        let k = key("a test key that is long enough to use");
        let t = issue(Some(&k), 7, 1_795_886_000);
        assert_eq!(check(Some(&k), 7, &t, 1_795_886_000.0), Some((1_795_886_000, Some(true))));
        assert_eq!(check(Some(&k), 8, &t, 1_795_886_000.0), Some((1_795_886_000, Some(false))), "another device");
        let moved = t.replacen("1795886000", "1795885000", 1);
        assert_eq!(check(Some(&k), 7, &moved, 1_795_886_000.0), Some((1_795_885_000, Some(false))), "another time");
        assert_eq!(check(Some(&key("another key that is long enough to use")), 7, &t, 1_795_886_000.0).unwrap().1, Some(false));
        assert_eq!(check(Some(&k), 7, "1795886000", 1_795_886_000.0), Some((1_795_886_000, Some(false))), "no mac");
        assert_eq!(check(None, 7, &t, 1_795_886_000.0), Some((1_795_886_000, None)), "nothing to check with");
        assert_eq!(check(Some(&k), 7, &t, 1_795_885_000.0), None, "from the future");
        for junk in ["", ".", "abc.def", "-5", "0"] {
            assert_eq!(check(Some(&k), 7, junk, 1_795_886_000.0), None, "{junk:?}");
        }
        assert_eq!(issue(None, 7, 1_795_886_000), "1795886000");
    }
}
