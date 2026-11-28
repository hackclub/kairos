//! Where a device's uploads come from: the provider (ASN) and country from iptoasn.com's table, looked up on this
//! server, a keyed hash of the address, a fingerprint of the request's headers and the device's clock. The address
//! itself is never stored. A device gets a new row only when one of these changes.

use std::collections::HashMap;
use std::io::BufRead;
use std::net::IpAddr;
use std::path::Path;

use anyhow::{Context, Result};
use axum::http::HeaderMap;
use deadpool_postgres::GenericClient;

use crate::util::now;

/// A clock this much off from the last row's starts a new one.
const SKEW_STEP_SECS: f64 = 120.0;

type Range<T> = (T, T, u32, [u8; 2]);

#[derive(Default)]
pub struct Table {
    v4: Vec<Range<u32>>,
    v6: Vec<Range<u128>>,
    names: HashMap<u32, String>,
}

pub struct Place<'a> {
    pub asn: u32,
    pub country: Option<String>,
    pub provider: &'a str,
}

impl Table {
    pub fn load(path: &Path) -> Result<Table> {
        let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut t = Table::default();
        for line in std::io::BufReader::new(file).lines() {
            t.add(&line?);
        }
        t.sort();
        Ok(t)
    }

    pub fn parse(text: &str) -> Table {
        let mut t = Table::default();
        text.lines().for_each(|l| t.add(l));
        t.sort();
        t
    }

    /// One of iptoasn's tab-separated lines: first address, last address, ASN, country, provider. ASN 0 is unrouted.
    fn add(&mut self, line: &str) {
        let mut f = line.split('\t');
        let (Some(a), Some(b), Some(asn), Some(cc), Some(name)) = (f.next(), f.next(), f.next(), f.next(), f.next()) else {
            return;
        };
        let Ok(asn) = asn.parse::<u32>() else { return };
        if asn == 0 {
            return;
        }
        let cc = match cc.as_bytes() {
            &[x, y] if x.is_ascii_uppercase() && y.is_ascii_uppercase() => [x, y],
            _ => *b"??",
        };
        match (a.parse::<IpAddr>(), b.parse::<IpAddr>()) {
            (Ok(IpAddr::V4(a)), Ok(IpAddr::V4(b))) => self.v4.push((a.into(), b.into(), asn, cc)),
            (Ok(IpAddr::V6(a)), Ok(IpAddr::V6(b))) => self.v6.push((a.into(), b.into(), asn, cc)),
            _ => return,
        }
        self.names.entry(asn).or_insert_with(|| name.trim().chars().take(100).collect());
    }

    fn sort(&mut self) {
        self.v4.sort_unstable_by_key(|r| r.0);
        self.v6.sort_unstable_by_key(|r| r.0);
    }

    pub fn len(&self) -> usize {
        self.v4.len() + self.v6.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn find(&self, ip: IpAddr) -> Option<Place<'_>> {
        let (asn, cc) = match ip.to_canonical() {
            IpAddr::V4(a) => hit(&self.v4, u32::from(a))?,
            IpAddr::V6(a) => hit(&self.v6, u128::from(a))?,
        };
        let country = (cc != *b"??").then(|| String::from_utf8_lossy(&cc).into_owned());
        Some(Place { asn, country, provider: self.names.get(&asn).map_or("", String::as_str) })
    }
}

fn hit<T: Ord + Copy>(rows: &[Range<T>], ip: T) -> Option<(u32, [u8; 2])> {
    let r = rows[..rows.partition_point(|r| r.0 <= ip)].last()?;
    (ip <= r.1).then_some((r.2, r.3))
}

pub fn ip_key(secret: &str) -> [u8; 32] {
    blake3::derive_key("kairos 2026-11-28 upload address hash", secret.as_bytes())
}

/// Two devices on one network get the same hash. IPv6 is hashed by its /64, which a home keeps while the rest changes.
pub fn ip_hash(key: &[u8; 32], ip: IpAddr) -> String {
    let hash = match ip.to_canonical() {
        IpAddr::V4(a) => blake3::keyed_hash(key, &a.octets()),
        IpAddr::V6(a) => blake3::keyed_hash(key, &a.octets()[..8]),
    };
    hash.to_hex()[..16].to_string()
}

// Headers a proxy on the way adds or rewrites, so they would split one client into several fingerprints.
const PROXY_HEADERS: [&str; 14] = [
    "host",
    "connection",
    "keep-alive",
    "content-length",
    "forwarded",
    "via",
    "x-real-ip",
    "true-client-ip",
    "cdn-loop",
    "x-request-id",
    "traceparent",
    "tracestate",
    "authorization",
    "kairos-clock",
];

/// Which headers the client sends and the values that say what sent it. Digits are folded so an update doesn't change it.
pub fn fingerprint(h: &HeaderMap) -> String {
    let mut names: Vec<&str> = h
        .keys()
        .map(|k| k.as_str())
        .filter(|n| !n.starts_with("cf-") && !n.starts_with("x-forwarded-") && !PROXY_HEADERS.contains(n))
        .collect();
    names.sort_unstable();
    names.dedup();
    let mut x = blake3::Hasher::new();
    x.update(names.join("\n").as_bytes());
    for n in ["user-agent", "accept", "accept-encoding", "content-type", "content-encoding"] {
        x.update(b"\0");
        if let Some(v) = h.get(n) {
            let mut last_digit = false;
            for &b in v.as_bytes() {
                let digit = b.is_ascii_digit();
                if !(digit && last_digit) {
                    x.update(&[if digit { b'0' } else { b }]);
                }
                last_digit = digit;
            }
        }
    }
    x.finalize().to_hex()[..16].to_string()
}

/// Seconds the device's clock is behind the server's, from the `kairos-clock` header (Unix seconds) when it sends one.
pub fn clock_skew(h: &HeaderMap) -> Option<f64> {
    let sent: f64 = h.get("kairos-clock")?.to_str().ok()?.trim().parse().ok()?;
    let skew = now() - sent;
    (sent.is_finite() && skew.abs() < 10.0 * 365.0 * 86_400.0).then_some(skew)
}

pub struct Seen<'a> {
    pub place: Option<Place<'a>>,
    pub ip_hash: Option<String>,
    pub fingerprint: String,
    pub clock_skew: Option<f64>,
}

pub async fn record(c: &impl GenericClient, device_id: i64, s: &Seen<'_>) -> Result<()> {
    let asn = s.place.as_ref().map(|p| p.asn as i64);
    let country = s.place.as_ref().and_then(|p| p.country.clone());
    let provider = s.place.as_ref().map(|p| p.provider);
    let last = c
        .query_opt(
            "SELECT since, asn, country, ip_hash, fingerprint, clock_skew FROM device_networks WHERE device_id = $1 ORDER BY since DESC LIMIT 1",
            &[&device_id],
        )
        .await?;
    if let Some(r) = &last {
        let skew: Option<f64> = r.get(5);
        let same_clock = match (skew, s.clock_skew) {
            (Some(a), Some(b)) => (a - b).abs() <= SKEW_STEP_SECS,
            (a, b) => a.is_none() && b.is_none(),
        };
        if r.get::<_, Option<i64>>(1) == asn
            && r.get::<_, Option<String>>(2) == country
            && r.get::<_, Option<String>>(3) == s.ip_hash
            && r.get::<_, String>(4) == s.fingerprint
            && same_clock
        {
            let since: f64 = r.get(0);
            c.execute("UPDATE device_networks SET last_seen = $3 WHERE device_id = $1 AND since = $2", &[&device_id, &since, &now()]).await?;
            return Ok(());
        }
    }
    let t = now();
    c.execute(
        "INSERT INTO device_networks (device_id, since, last_seen, asn, provider, country, ip_hash, fingerprint, clock_skew)
         VALUES ($1, $2, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT DO NOTHING",
        &[&device_id, &t, &asn, &provider, &country, &s.ip_hash, &s.fingerprint, &s.clock_skew],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TSV: &str = "1.0.0.0\t1.0.0.255\t13335\tUS\tCLOUDFLARENET\n\
        5.9.0.0\t5.9.255.255\t24940\tDE\tHETZNER-AS\n\
        10.0.0.0\t10.255.255.255\t0\tNone\tNot routed\n\
        41.32.0.0\t41.47.255.255\t8452\tEG\tTE-AS\n\
        2a01:4f8::\t2a01:4f8:ffff:ffff:ffff:ffff:ffff:ffff\t24940\tDE\tHETZNER-AS\n\
        77.0.0.0\t77.0.0.255\t64500\tNone\tSomewhere\n\
        broken line\n";

    #[test]
    fn finds_the_range_an_address_falls_in() {
        let t = Table::parse(TSV);
        assert_eq!(t.len(), 5);
        let p = t.find("5.9.10.20".parse().unwrap()).unwrap();
        assert_eq!((p.asn, p.country.as_deref(), p.provider), (24940, Some("DE"), "HETZNER-AS"));
        assert_eq!(t.find("41.47.255.255".parse().unwrap()).unwrap().asn, 8452);
        assert_eq!(t.find("2a01:4f8:c0c:1::2".parse().unwrap()).unwrap().provider, "HETZNER-AS");
        assert_eq!(t.find("::ffff:1.0.0.7".parse().unwrap()).unwrap().asn, 13335, "a mapped v4 address is looked up as v4");
        assert!(t.find("10.1.2.3".parse().unwrap()).is_none(), "unrouted space has no provider");
        assert!(t.find("1.0.1.0".parse().unwrap()).is_none());
        assert!(t.find("0.0.0.1".parse().unwrap()).is_none());
        assert_eq!(t.find("77.0.0.1".parse().unwrap()).unwrap().country, None);
    }

    #[test]
    fn the_hash_matches_one_network_and_needs_the_key() {
        let (k, other) = (ip_key("one"), ip_key("two"));
        let h = |key: &[u8; 32], ip: &str| ip_hash(key, ip.parse().unwrap());
        assert_eq!(h(&k, "41.33.1.2"), h(&k, "::ffff:41.33.1.2"));
        assert_ne!(h(&k, "41.33.1.2"), h(&k, "41.33.1.3"));
        assert_ne!(h(&k, "41.33.1.2"), h(&other, "41.33.1.2"));
        assert_eq!(h(&k, "2a01:4f8:c0c:1::2"), h(&k, "2a01:4f8:c0c:1:aaaa::9"), "same /64");
        assert_ne!(h(&k, "2a01:4f8:c0c:1::2"), h(&k, "2a01:4f8:c0c:2::2"));
        assert_eq!(h(&k, "41.33.1.2").len(), 16);
    }

    #[test]
    fn the_fingerprint_ignores_proxies_and_version_numbers() {
        let hm = |pairs: &[(&'static str, &'static str)]| {
            let mut h = HeaderMap::new();
            for (k, v) in pairs {
                h.append(*k, v.parse().unwrap());
            }
            h
        };
        let base = [("user-agent", "kairosd/0.1.27"), ("content-type", "application/json"), ("accept", "*/*")];
        let a = fingerprint(&hm(&base));
        let mut proxied = base.to_vec();
        proxied.extend([("x-forwarded-for", "1.2.3.4"), ("cf-ray", "abc"), ("host", "kairos.hackclub.com"), ("authorization", "Bearer x")]);
        assert_eq!(fingerprint(&hm(&proxied)), a);
        assert_eq!(fingerprint(&hm(&[("user-agent", "kairosd/0.2.3"), ("content-type", "application/json"), ("accept", "*/*")])), a);
        assert_ne!(fingerprint(&hm(&[("user-agent", "python-requests/2.32.3"), ("content-type", "application/json"), ("accept", "*/*")])), a);
        assert_ne!(fingerprint(&hm(&[("user-agent", "kairosd/0.1.27"), ("content-type", "application/json")])), a);
    }
}
