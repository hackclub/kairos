use std::net::IpAddr;

use anyhow::{bail, Context, Result};
use reqwest::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    Hca,
    Mock,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub public_url: String,
    pub bind: String,
    pub auth: AuthMode,
    pub issuer: String,
    pub backchannel: Option<String>,
    pub client_id: String,
    pub client_secret: String,
    pub trusted_proxies: Vec<Cidr>,
    pub downloads: Option<std::path::PathBuf>,
    pub chrome_extension_url: Option<String>,
    pub firefox_extension_url: Option<String>,
    /// Never logged or rendered.
    pub github_token: Option<String>,
    pub github_api: String,
    pub self_delete: bool,
    /// iptoasn.com's combined table, unpacked. Without it uploads get no provider or country.
    pub ip2asn: Option<std::path::PathBuf>,
    /// Keys the upload address hash. Never logged or rendered.
    pub ip_key: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Config> {
        Config::from_vars(|k| std::env::var(k).ok())
    }

    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Config> {
        let var = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        if var("KAIROS_DEV_LOGIN").is_some() {
            bail!("KAIROS_DEV_LOGIN was removed: use KAIROS_AUTH=mock for test sign-ins");
        }
        let database_url = var("DATABASE_URL").context("DATABASE_URL is not set")?;
        let public_url = origin(&var("PUBLIC_URL").unwrap_or_else(|| "http://localhost:8787".into()))
            .context("PUBLIC_URL must be an http(s) origin like https://kairos.example.com")?;
        let bind = var("BIND").unwrap_or_else(|| "127.0.0.1:8787".into());
        let auth = match var("KAIROS_AUTH").as_deref() {
            None | Some("hca") => AuthMode::Hca,
            Some("mock") => AuthMode::Mock,
            Some(other) => bail!("KAIROS_AUTH must be hca or mock, not {other:?}"),
        };
        let (issuer, client_id, client_secret) = match auth {
            AuthMode::Hca => {
                let issuer = origin_or_path(&var("HCA_URL").unwrap_or_else(|| "https://auth.hackclub.com".into()))
                    .context("HCA_URL must be an https URL")?;
                if !issuer.starts_with("https://") && !loopback(&issuer) {
                    bail!("HCA_URL must use https");
                }
                (
                    issuer,
                    var("HCA_CLIENT_ID").context("HCA_CLIENT_ID is not set (or set KAIROS_AUTH=mock for local testing)")?,
                    var("HCA_CLIENT_SECRET").context("HCA_CLIENT_SECRET is not set")?,
                )
            }
            AuthMode::Mock => {
                if !loopback(&public_url) && var("KAIROS_ALLOW_MOCK_AUTH").as_deref() != Some("1") {
                    bail!(
                        "KAIROS_AUTH=mock lets anyone sign in as anyone; it only runs on localhost unless KAIROS_ALLOW_MOCK_AUTH=1"
                    );
                }
                let secret = match var("HCA_CLIENT_SECRET") {
                    Some(s) => s,
                    None => crate::util::random_token("")?,
                };
                (format!("{public_url}/mock-auth"), var("HCA_CLIENT_ID").unwrap_or_else(|| "kairos".into()), secret)
            }
        };
        let backchannel = match auth {
            AuthMode::Mock if !loopback(&public_url) => {
                let port = bind.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok()).context("BIND must end in a port")?;
                Some(format!("http://127.0.0.1:{port}/mock-auth"))
            }
            _ => None,
        };
        let trusted_proxies = match var("KAIROS_TRUSTED_PROXY") {
            None => Vec::new(),
            Some(v) => v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .flat_map(|s| match s {
                    "1" | "loopback" => vec![Cidr::parse("127.0.0.0/8"), Cidr::parse("::1/128")],
                    s => vec![Cidr::parse(s)],
                })
                .collect::<Result<_>>()
                .context("KAIROS_TRUSTED_PROXY must be 1, loopback, or a comma list of IPs/CIDRs")?,
        };
        let downloads = var("KAIROS_DOWNLOADS").filter(|d| !d.is_empty()).map(std::path::PathBuf::from);
        if downloads.as_ref().is_some_and(|d| !d.is_absolute()) {
            bail!("KAIROS_DOWNLOADS must be an absolute path");
        }
        let chrome_extension_url = store_url(var("KAIROS_CHROME_EXTENSION_URL"), "KAIROS_CHROME_EXTENSION_URL")?;
        let firefox_extension_url = store_url(var("KAIROS_FIREFOX_EXTENSION_URL"), "KAIROS_FIREFOX_EXTENSION_URL")?;
        let github_token = var("KAIROS_GITHUB_TOKEN");
        if github_token.as_ref().is_some_and(|t| !t.bytes().all(|b| b.is_ascii_graphic())) {
            bail!("KAIROS_GITHUB_TOKEN must be a single token with no spaces or control characters");
        }
        let github_api = origin_or_path(&var("KAIROS_GITHUB_API").unwrap_or_else(|| "https://api.github.com/graphql".into()))
            .context("KAIROS_GITHUB_API must be an https URL")?;
        if !github_api.starts_with("https://") && !loopback(&github_api) {
            bail!("KAIROS_GITHUB_API must use https");
        }
        let self_delete = match var("KAIROS_SELF_DELETE").as_deref() {
            None | Some("0") => false,
            Some("1") => true,
            Some(other) => bail!("KAIROS_SELF_DELETE must be 0 or 1, not {other:?}"),
        };
        let ip2asn = var("KAIROS_IP2ASN").map(std::path::PathBuf::from);
        let ip_key = var("KAIROS_IP_KEY");
        if ip_key.as_ref().is_some_and(|k| k.len() < 32) {
            bail!("KAIROS_IP_KEY must be at least 32 characters");
        }
        Ok(Config {
            database_url,
            public_url,
            bind,
            auth,
            issuer,
            backchannel,
            client_id,
            client_secret,
            trusted_proxies,
            downloads,
            chrome_extension_url,
            firefox_extension_url,
            github_token,
            github_api,
            self_delete,
            ip2asn,
            ip_key,
        })
    }

    pub fn provider_url(&self, url: &str) -> String {
        match (&self.backchannel, url.strip_prefix(&self.issuer)) {
            (Some(base), Some(rest)) => format!("{base}{rest}"),
            _ => url.to_string(),
        }
    }

    pub fn https(&self) -> bool {
        self.public_url.starts_with("https://")
    }

    pub fn test_mode(&self) -> bool {
        self.auth == AuthMode::Mock
    }

    pub fn host(&self) -> &str {
        self.public_url.split_once("://").map_or(self.public_url.as_str(), |(_, h)| h)
    }
}

fn store_url(value: Option<String>, name: &str) -> Result<Option<String>> {
    let Some(v) = value else { return Ok(None) };
    let u = Url::parse(&v).with_context(|| format!("{name} must be an https URL"))?;
    if u.scheme() != "https" || u.host_str().is_none() || !u.username().is_empty() || u.password().is_some() {
        bail!("{name} must be an https URL without credentials");
    }
    Ok(Some(u.to_string()))
}

fn origin(s: &str) -> Result<String> {
    let u = Url::parse(s)?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() || u.query().is_some() || u.path() != "/" {
        bail!("not an origin: {s}");
    }
    Ok(u.as_str().trim_end_matches('/').to_string())
}

fn origin_or_path(s: &str) -> Result<String> {
    let u = Url::parse(s)?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() || u.query().is_some() {
        bail!("not a base URL: {s}");
    }
    Ok(u.as_str().trim_end_matches('/').to_string())
}

fn loopback(url: &str) -> bool {
    let Ok(u) = Url::parse(url) else { return false };
    let Some(h) = u.host_str() else { return false };
    let h = h.trim_start_matches('[').trim_end_matches(']');
    h == "localhost" || h.ends_with(".localhost") || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    addr: IpAddr,
    bits: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Result<Cidr> {
        let (ip, bits) = match s.split_once('/') {
            Some((ip, b)) => (ip.parse::<IpAddr>()?, Some(b.parse::<u8>()?)),
            None => (s.parse::<IpAddr>()?, None),
        };
        let max = if ip.is_ipv4() { 32 } else { 128 };
        let bits = bits.unwrap_or(max);
        if bits > max {
            bail!("bad prefix length in {s}");
        }
        Ok(Cidr { addr: ip, bits })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        match (self.addr, ip) {
            (IpAddr::V4(a), IpAddr::V4(b)) => {
                let mask = if self.bits == 0 { 0 } else { u32::MAX << (32 - self.bits) };
                u32::from(a) & mask == u32::from(b) & mask
            }
            (IpAddr::V6(a), IpAddr::V6(b)) => {
                let mask = if self.bits == 0 { 0 } else { u128::MAX << (128 - self.bits) };
                u128::from(a) & mask == u128::from(b) & mask
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(vars: &[(&str, &str)]) -> Result<Config> {
        let m: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_vars(|k| m.get(k).cloned())
    }

    #[test]
    fn mock_needs_opt_in_off_localhost() {
        let db = ("DATABASE_URL", "postgres://x@localhost/x");
        assert!(cfg(&[db, ("KAIROS_AUTH", "mock")]).is_ok());
        assert!(cfg(&[db, ("KAIROS_AUTH", "mock"), ("PUBLIC_URL", "https://kairos.example.com")]).is_err());
        let c = cfg(&[db, ("KAIROS_AUTH", "mock"), ("PUBLIC_URL", "https://kairos.example.com"), ("KAIROS_ALLOW_MOCK_AUTH", "1")]).unwrap();
        assert!(c.test_mode());
        assert_eq!(c.issuer, "https://kairos.example.com/mock-auth");
    }

    #[test]
    fn mock_behind_a_proxy_talks_to_itself_over_loopback() {
        let db = ("DATABASE_URL", "postgres://x@localhost/x");
        let public = cfg(&[
            db,
            ("KAIROS_AUTH", "mock"),
            ("PUBLIC_URL", "https://kairos.example.com"),
            ("KAIROS_ALLOW_MOCK_AUTH", "1"),
            ("BIND", "0.0.0.0:8787"),
        ])
        .unwrap();
        assert_eq!(public.provider_url("https://kairos.example.com/mock-auth/oauth/token"), "http://127.0.0.1:8787/mock-auth/oauth/token");
        assert_eq!(public.provider_url("https://elsewhere.example.com/x"), "https://elsewhere.example.com/x");
        let local = cfg(&[db, ("KAIROS_AUTH", "mock"), ("PUBLIC_URL", "http://127.0.0.1:9000")]).unwrap();
        assert_eq!(local.provider_url("http://127.0.0.1:9000/mock-auth/oauth/token"), "http://127.0.0.1:9000/mock-auth/oauth/token");
        let hca = cfg(&[db, ("HCA_CLIENT_ID", "id"), ("HCA_CLIENT_SECRET", "s")]).unwrap();
        assert_eq!(hca.provider_url("https://auth.hackclub.com/oauth/token"), "https://auth.hackclub.com/oauth/token");
    }

    #[test]
    fn hca_needs_credentials_and_rejects_dev_login() {
        let db = ("DATABASE_URL", "postgres://x@localhost/x");
        assert!(cfg(&[db]).is_err());
        let c = cfg(&[db, ("HCA_CLIENT_ID", "id"), ("HCA_CLIENT_SECRET", "s"), ("PUBLIC_URL", "https://k.example.com/")]).unwrap();
        assert_eq!(c.public_url, "https://k.example.com");
        assert_eq!(c.issuer, "https://auth.hackclub.com");
        assert!(cfg(&[db, ("KAIROS_DEV_LOGIN", "1"), ("KAIROS_AUTH", "mock")]).is_err());
        assert!(cfg(&[db, ("KAIROS_AUTH", "mock"), ("PUBLIC_URL", "https://k.example.com/app")]).is_err());
    }

    #[test]
    fn extension_urls_are_optional_and_https() {
        let db = ("DATABASE_URL", "postgres://x@localhost/x");
        let mock = ("KAIROS_AUTH", "mock");
        let c = cfg(&[db, mock]).unwrap();
        assert!(c.chrome_extension_url.is_none() && c.firefox_extension_url.is_none());
        let c = cfg(&[db, mock, ("KAIROS_CHROME_EXTENSION_URL", "https://chromewebstore.google.com/detail/kairos/abc")]).unwrap();
        assert_eq!(c.chrome_extension_url.as_deref(), Some("https://chromewebstore.google.com/detail/kairos/abc"));
        assert!(cfg(&[db, mock, ("KAIROS_FIREFOX_EXTENSION_URL", "http://addons.mozilla.org/x")]).is_err());
        assert!(cfg(&[db, mock, ("KAIROS_FIREFOX_EXTENSION_URL", "javascript:alert(1)")]).is_err());
        assert!(cfg(&[db, mock, ("KAIROS_CHROME_EXTENSION_URL", "https://user:pw@example.com/")]).is_err());
    }

    #[test]
    fn github_check_is_optional_and_keeps_its_token_safe() {
        let db = ("DATABASE_URL", "postgres://x@localhost/x");
        let mock = ("KAIROS_AUTH", "mock");
        let c = cfg(&[db, mock]).unwrap();
        assert!(c.github_token.is_none());
        assert_eq!(c.github_api, "https://api.github.com/graphql");
        let c = cfg(&[db, mock, ("KAIROS_GITHUB_TOKEN", " ghp_abc "), ("KAIROS_GITHUB_API", "http://127.0.0.1:9000/graphql")]).unwrap();
        assert_eq!(c.github_token.as_deref(), Some("ghp_abc"));
        assert_eq!(c.github_api, "http://127.0.0.1:9000/graphql");
        assert!(cfg(&[db, mock, ("KAIROS_GITHUB_TOKEN", "two words")]).is_err());
        assert!(cfg(&[db, mock, ("KAIROS_GITHUB_API", "http://github.example.com/graphql")]).is_err());
    }

    #[test]
    fn cidrs() {
        let c = Cidr::parse("172.16.0.0/12").unwrap();
        assert!(c.contains("172.18.0.1".parse().unwrap()));
        assert!(!c.contains("10.0.0.1".parse().unwrap()));
        assert!(c.contains("::ffff:172.18.0.1".parse().unwrap()));
        assert!(Cidr::parse("::1/128").unwrap().contains("::1".parse().unwrap()));
        assert!(Cidr::parse("1.2.3.4/33").is_err());
    }
}
