#![allow(dead_code)]

mod sse;
#[allow(unused_imports)]
pub use sse::Events;

use std::collections::HashMap;
use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{header, HeaderMap, Method, Request, Response, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use kairos_core::event::{Class, Event, Kind, GENESIS};
use kairos_server::config::Config;
use kairos_server::util::urlenc;
use kairos_server::{db, router, App, St};
use serde_json::{json, Value};
use tokio_postgres::NoTls;
use tower::ServiceExt;

pub struct TestApp {
    pub st: St,
    pub router: Router,
    pub url: String,
    schema: String,
    base: String,
    served: tokio::task::AbortHandle,
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

impl Resp {
    pub fn location(&self) -> String {
        self.headers.get(header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
    }

    pub fn cookies(&self) -> Vec<String> {
        self.headers.get_all(header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok()).map(String::from).collect()
    }

    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|_| panic!("not json ({}): {}", self.status, self.body))
    }

    pub fn csrf(&self) -> String {
        let at = self.body.find("name=\"csrf\" value=\"").expect("page has a csrf field") + "name=\"csrf\" value=\"".len();
        self.body[at..].split('"').next().unwrap().to_string()
    }
}

fn random_hex() -> String {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).unwrap();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl TestApp {
    pub async fn new() -> TestApp {
        TestApp::with(&[]).await
    }

    pub async fn with(extra: &[(&str, &str)]) -> TestApp {
        let base = std::env::var("KAIROS_TEST_DATABASE_URL").expect("KAIROS_TEST_DATABASE_URL is set");
        let schema = format!("t_{}", random_hex());
        let (client, conn) = tokio_postgres::connect(&base, NoTls).await.unwrap();
        tokio::spawn(conn);
        client.batch_execute(&format!("CREATE SCHEMA {schema}")).await.unwrap();
        let mut cfg: tokio_postgres::Config = base.parse().unwrap();
        cfg.options(format!("-c search_path={schema}"));
        let pool = db::pool_from(cfg).unwrap();
        db::migrate(&pool).await.unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let mut vars: HashMap<String, String> = [
            ("DATABASE_URL", base.as_str()),
            ("PUBLIC_URL", url.as_str()),
            ("KAIROS_AUTH", "mock"),
            ("HCA_CLIENT_SECRET", "test-secret"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for (k, v) in extra {
            vars.insert(k.to_string(), v.to_string());
        }
        let config = Config::from_vars(|k| vars.get(k).cloned()).unwrap();
        let st = App::new(config, pool).unwrap();
        let app = router(st.clone());
        let served = app.clone().into_make_service_with_connect_info::<SocketAddr>();
        let served = tokio::spawn(async move { axum::serve(listener, served).await }).abort_handle();
        let router = app.layer(MockConnectInfo(SocketAddr::from(([10, 0, 0, 1], 40000))));
        TestApp { st, router, url, schema, base, served }
    }

    pub async fn finish(self) {
        // The router served in the background keeps the app and its listener alive, so both are let go here.
        self.served.abort();
        self.st.pool.close();
        let (client, conn) = tokio_postgres::connect(&self.base, NoTls).await.unwrap();
        tokio::spawn(conn);
        client.batch_execute(&format!("DROP SCHEMA {} CASCADE", self.schema)).await.unwrap();
    }

    pub async fn send(&self, req: Request<Body>) -> Resp {
        let res = self.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        Resp { status, headers, body: String::from_utf8_lossy(&bytes).into_owned() }
    }

    pub async fn api(&self, method: Method, path: &str, body: Option<Value>, token: Option<&str>) -> Resp {
        let mut b = Request::builder().method(method).uri(path).header(header::USER_AGENT, "kairosd/9.9.9");
        if let Some(t) = token {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = match body {
            Some(v) => b.header(header::CONTENT_TYPE, "application/json").body(Body::from(v.to_string())).unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        self.send(req).await
    }

    pub async fn count(&self, sql: &str, user_id: i64) -> i64 {
        let c = self.st.pool.get().await.unwrap();
        c.query_one(sql, &[&user_id]).await.unwrap().get(0)
    }

    pub async fn exec(&self, sql: &str) {
        let c = self.st.pool.get().await.unwrap();
        c.batch_execute(sql).await.unwrap();
    }

    pub fn browser(&self) -> Browser<'_> {
        Browser { app: self, jar: HashMap::new() }
    }
}

pub fn form(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", urlenc(k), urlenc(v))).collect::<Vec<_>>().join("&")
}

pub fn query_of(url: &str) -> HashMap<String, String> {
    let u = reqwest::Url::parse(url).unwrap();
    u.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

pub struct Browser<'a> {
    pub app: &'a TestApp,
    pub jar: HashMap<String, String>,
}

impl Browser<'_> {
    fn path(&self, p: &str) -> String {
        p.strip_prefix(&self.app.url).unwrap_or(p).to_string()
    }

    fn cookie_header(&self) -> String {
        self.jar.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ")
    }

    pub async fn request(&mut self, method: Method, path: &str, body: Option<String>, origin: Option<&str>) -> Resp {
        let mut b = Request::builder().method(method).uri(self.path(path));
        if !self.jar.is_empty() {
            b = b.header(header::COOKIE, self.cookie_header());
        }
        if let Some(o) = origin {
            b = b.header(header::ORIGIN, o);
        }
        let req = match body {
            Some(f) => b.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded").body(Body::from(f)).unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = self.app.send(req).await;
        for c in res.cookies() {
            let first = c.split(';').next().unwrap_or("");
            if let Some((k, v)) = first.split_once('=') {
                if v.is_empty() || c.contains("Max-Age=0") {
                    self.jar.remove(k);
                } else {
                    self.jar.insert(k.to_string(), v.to_string());
                }
            }
        }
        res
    }

    pub async fn open(&mut self, path: &str) -> Response<Body> {
        let mut b = Request::builder().method(Method::GET).uri(self.path(path));
        if !self.jar.is_empty() {
            b = b.header(header::COOKIE, self.cookie_header());
        }
        self.app.router.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap()
    }

    pub async fn get(&mut self, path: &str) -> Resp {
        self.request(Method::GET, path, None, None).await
    }

    pub async fn post(&mut self, path: &str, pairs: &[(&str, &str)]) -> Resp {
        let origin = self.app.url.clone();
        self.request(Method::POST, path, Some(form(pairs)), Some(&origin)).await
    }

    pub async fn authorize(&mut self, name: &str, email: &str) -> String {
        let login = self.get("/auth/login?next=/me").await;
        assert_eq!(login.status, StatusCode::SEE_OTHER, "{}", login.body);
        let auth_url = login.location();
        assert!(auth_url.starts_with(&format!("{}/mock-auth/oauth/authorize?", self.app.url)), "{auth_url}");
        let page = self.get(&auth_url).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.body);
        let q = query_of(&auth_url);
        let mut pairs: Vec<(&str, &str)> = q.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        pairs.extend([
            ("action", "approve"),
            ("name", name),
            ("email", email),
            ("slack_id", "U0TEST"),
            ("verification_status", "verified"),
            ("ysws_eligible", "on"),
        ]);
        let approved = self.post("/mock-auth/oauth/authorize", &pairs).await;
        assert_eq!(approved.status, StatusCode::SEE_OTHER, "{}", approved.body);
        approved.location()
    }

    pub async fn sign_in(&mut self, name: &str, email: &str) {
        let cb = self.authorize(name, email).await;
        let res = self.get(&cb).await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
        assert_eq!(res.location(), "/me");
    }

    pub async fn csrf(&mut self) -> String {
        self.get("/settings").await.csrf()
    }

    pub async fn pair(&mut self, name: &str) -> String {
        let start = self.app.api(Method::POST, "/api/v1/device/start", Some(json!({ "name": name, "os": "linux" })), None).await;
        assert_eq!(start.status, StatusCode::OK, "{}", start.body);
        let s = start.json();
        let code = s["user_code"].as_str().unwrap().to_string();
        let poll = s["poll_token"].as_str().unwrap().to_string();
        let page = self.get(&format!("/device?code={code}")).await;
        assert!(page.body.contains(&code), "{}", page.body);
        let ok = self.post("/device/approve", &[("code", &code), ("csrf", &page.csrf())]).await;
        assert!(ok.body.contains("Computer connected"), "{}", ok.body);
        let p = self.app.api(Method::POST, "/api/v1/device/poll", Some(json!({ "poll_token": poll })), None).await.json();
        assert_eq!(p["status"], "approved");
        p["token"].as_str().unwrap().to_string()
    }
}

pub async fn user_id(app: &TestApp, username: &str) -> i64 {
    let c = app.st.pool.get().await.unwrap();
    c.query_one("SELECT id FROM users WHERE username = $1", &[&username]).await.unwrap().get(0)
}

pub struct Chain {
    prev: String,
    pub events: Vec<Value>,
}

impl Default for Chain {
    fn default() -> Self {
        Chain { prev: GENESIS.to_string(), events: Vec::new() }
    }
}

impl Chain {
    pub fn push(&mut self, ts: f64, project: Option<&str>, kind: Kind) {
        let mut e = Event::new(ts, project.map(String::from), kind);
        e.seal(&self.prev);
        self.prev = e.hash.clone();
        self.events.push(serde_json::to_value(&e).unwrap());
    }

    pub fn editing(&mut self, project: &str, start: f64, end: f64) {
        self.push(start, Some(project), Kind::Focus { class: Class::Editor, app: None });
        let mut t = start + 60.0;
        while t <= end + 1e-9 {
            self.push(t, None, Kind::Present { input: None });
            t += 60.0;
        }
    }

    pub fn prompt(&mut self, ts: f64, project: Option<&str>, harness: &str) {
        self.push_from(ts, project, harness, Kind::Prompt { words: 5, pasted: false, model: None, session: "s1".into(), shape: None, version: None, via: None, permissions: None });
    }

    /// Ends the turn `prompt` started, as the agent's reply does.
    pub fn reply(&mut self, ts: f64, project: Option<&str>, harness: &str) {
        self.push_from(ts, project, harness, Kind::ReplyDone { session: "s1".into(), reply_words: 40, tools: 0, files_changed: 0, model: None });
    }

    pub fn push_from(&mut self, ts: f64, project: Option<&str>, harness: &str, kind: Kind) {
        let mut e = Event::new(ts, project.map(String::from), kind);
        e.harness = Some(harness.to_string());
        e.seal(&self.prev);
        self.prev = e.hash.clone();
        self.events.push(serde_json::to_value(&e).unwrap());
    }

    pub fn take(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.events)
    }
}

pub async fn upload(app: &TestApp, token: &str, events: Vec<Value>) -> Resp {
    app.api(Method::POST, "/api/v1/events", Some(json!({ "events": events })), Some(token)).await
}

pub async fn day_seconds(app: &TestApp, user_id: i64) -> Vec<(String, String, String, f64)> {
    let c = app.st.pool.get().await.unwrap();
    c.query("SELECT day::text, project, class, seconds FROM user_days WHERE user_id = $1 ORDER BY 1, 2, 3", &[&user_id])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
        .collect()
}
