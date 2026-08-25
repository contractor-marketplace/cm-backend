//! Shared test scaffolding: a configured router and a cookie-aware client.

#![allow(dead_code)]

use axum::body::Body;
use axum::Router;
use cm_api::AppState;
use cm_core::Config;
use http::{HeaderMap, Request, StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tower::ServiceExt;

pub const PEPPER: &str = "test-pepper-that-is-at-least-32-characters";
pub const SITE_ORIGIN: &str = "https://app.example.test";
pub const PASSWORD: &str = "a sufficiently long password";

/// Build a real `Config` from an environment map, so the tests exercise the
/// same validation the binary does rather than a hand-made struct.
pub fn config(overrides: &[(&str, &str)]) -> Config {
    let mut vars: Vec<(String, String)> = vec![
        (
            "DATABASE_URL".into(),
            "postgres://cmdev@127.0.0.1:5432/cm_test".into(),
        ),
        ("CM_SITE_ORIGIN".into(), SITE_ORIGIN.into()),
        ("CM_HASH_PEPPER".into(), PEPPER.into()),
        // One permit: enough to hash, small enough that the tests stay quick.
        ("CM_ARGON2_MAX_CONCURRENCY".into(), "1".into()),
    ];
    for (key, value) in overrides {
        vars.retain(|(existing, _)| existing != key);
        vars.push(((*key).to_owned(), (*value).to_owned()));
    }

    let map: HashMap<String, String> = vars.into_iter().collect();
    Config::load(move |key: &str| map.get(key).cloned()).expect("test configuration must load")
}

pub fn router(pool: PgPool) -> Router {
    router_with(pool, &[])
}

pub fn router_with(pool: PgPool, overrides: &[(&str, &str)]) -> Router {
    let state = AppState::new(pool, &config(overrides)).expect("build state");
    cm_api::build(state)
}

/// A router whose pool points at a port nothing is listening on, built lazily
/// so no connection is attempted until a handler asks for one.
pub fn unreachable_database_router() -> Router {
    let config = config(&[("DATABASE_URL", "postgres://nobody@127.0.0.1:1/nothing")]);
    let pool = cm_db::connect_lazy(&config.database).expect("a lazy pool needs no server");
    cm_api::build(AppState::new(pool, &config).expect("build state"))
}

#[derive(Debug)]
pub struct TestResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub json: Value,
}

impl TestResponse {
    /// Every `Set-Cookie` on the response, in order.
    pub fn set_cookies(&self) -> Vec<String> {
        self.headers
            .get_all(http::header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_owned)
            .collect()
    }

    pub fn cookie(&self, name: &str) -> Option<String> {
        self.set_cookies()
            .into_iter()
            .find(|cookie| cookie.starts_with(&format!("{name}=")))
    }
}

/// A client that behaves enough like a browser to make the auth flows testable:
/// it keeps a cookie jar, honours `Max-Age=0`, and echoes the CSRF cookie in
/// the header on state-changing requests.
pub struct Client {
    router: Router,
    jar: HashMap<String, String>,
    peer: SocketAddr,
    origin: Option<String>,
    send_csrf: bool,
}

impl Client {
    pub fn new(router: Router) -> Self {
        Self {
            router,
            jar: HashMap::new(),
            peer: "203.0.113.10:44444".parse().expect("addr"),
            origin: Some(SITE_ORIGIN.to_owned()),
            send_csrf: true,
        }
    }

    /// Change the apparent client address, for per-IP rate-limit tests.
    pub fn with_peer(mut self, peer: &str) -> Self {
        self.peer = peer.parse().expect("addr");
        self
    }

    /// Drop the CSRF header from state-changing requests.
    pub fn without_csrf(mut self) -> Self {
        self.send_csrf = false;
        self
    }

    pub fn with_origin(mut self, origin: Option<&str>) -> Self {
        self.origin = origin.map(str::to_owned);
        self
    }

    /// Replace the CSRF token with something else, for forgery tests.
    pub fn set_csrf(&mut self, token: &str) {
        self.jar
            .insert("__Host-cm_csrf".to_owned(), token.to_owned());
    }

    pub fn csrf_token(&self) -> Option<&str> {
        self.jar.get("__Host-cm_csrf").map(String::as_str)
    }

    pub fn session_cookie(&self) -> Option<&str> {
        self.jar.get("__Host-cm_session").map(String::as_str)
    }

    pub fn clear_jar(&mut self) {
        self.jar.clear();
    }

    pub fn set_session(&mut self, token: &str) {
        self.jar
            .insert("__Host-cm_session".to_owned(), token.to_owned());
    }

    pub async fn get(&mut self, path: &str) -> TestResponse {
        self.send(http::Method::GET, path, None).await
    }

    pub async fn post(&mut self, path: &str, body: Value) -> TestResponse {
        self.send(http::Method::POST, path, Some(body)).await
    }

    pub async fn send(
        &mut self,
        method: http::Method,
        path: &str,
        body: Option<Value>,
    ) -> TestResponse {
        let mut builder = Request::builder().method(method.clone()).uri(path);

        if !self.jar.is_empty() {
            let cookie = self
                .jar
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(http::header::COOKIE, cookie);
        }
        if self.send_csrf {
            if let Some(token) = self.jar.get("__Host-cm_csrf") {
                builder = builder.header("x-cm-csrf", token.clone());
            }
        }
        if let Some(origin) = &self.origin {
            builder = builder.header(http::header::ORIGIN, origin.clone());
        }

        let request = match body {
            Some(value) => builder
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(value.to_string()))
                .expect("build request"),
            None => builder.body(Body::empty()).expect("build request"),
        };

        let mut request = request;
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(self.peer));

        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("router should not fail");

        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .expect("read body");
        let json = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                panic!(
                    "body was not JSON ({error}): {}",
                    String::from_utf8_lossy(&bytes)
                )
            })
        };

        self.absorb_cookies(&headers);

        TestResponse {
            status,
            headers,
            json,
        }
    }

    fn absorb_cookies(&mut self, headers: &HeaderMap) {
        for raw in headers.get_all(http::header::SET_COOKIE).iter() {
            let Ok(raw) = raw.to_str() else { continue };
            let Some((pair, attributes)) = raw.split_once(';') else {
                continue;
            };
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };

            if attributes.to_lowercase().contains("max-age=0") {
                self.jar.remove(name.trim());
            } else {
                self.jar
                    .insert(name.trim().to_owned(), value.trim().to_owned());
            }
        }
    }

    /// Register an account and end up signed in, as a browser would.
    /// Registers a homeowner, which is the default side of the marketplace.
    pub async fn register(&mut self, email: &str) -> TestResponse {
        self.register_as(email, "homeowner").await
    }

    /// Registers a contractor. Claiming a listing requires one, and the
    /// database enforces that as well as the handler.
    pub async fn register_contractor(&mut self, email: &str) -> TestResponse {
        self.register_as(email, "contractor").await
    }

    pub async fn register_as(&mut self, email: &str, account_type: &str) -> TestResponse {
        self.post(
            "/v1/auth/register",
            serde_json::json!({
                "email": email,
                "display_name": "Test Person",
                "password": PASSWORD,
                "account_type": account_type,
            }),
        )
        .await
    }

    pub async fn login(&mut self, email: &str, password: &str) -> TestResponse {
        self.post(
            "/v1/auth/login",
            serde_json::json!({ "email": email, "password": password }),
        )
        .await
    }
}

/// Wall-clock helper for the few assertions that need one.
pub fn seconds(n: u64) -> Duration {
    Duration::from_secs(n)
}

// ── directory fixtures ──────────────────────────────────────────────────────

/// A contractor as the fixtures describe it.
pub struct SeedContractor {
    pub license_no: &'static str,
    pub name: &'static str,
    pub status: &'static str,
    pub postal_code: &'static str,
    pub classification: &'static str,
}

pub const LA_ZIPS: &[(&str, &str, f64, f64)] = &[
    ("90026", "Silver Lake", 34.0781, -118.2606),
    ("90042", "Highland Park", 34.1156, -118.1926),
    ("90232", "Culver City", 34.0211, -118.3965),
    ("90401", "Santa Monica", 34.0195, -118.4912),
];

pub const SEED_CONTRACTORS: &[SeedContractor] = &[
    SeedContractor {
        license_no: "1047382",
        name: "Ibarra & Daughters Construction",
        status: "active",
        postal_code: "90042",
        classification: "B",
    },
    SeedContractor {
        license_no: "983311",
        name: "Meridian Electric Co",
        status: "active",
        postal_code: "90232",
        classification: "C-10",
    },
    SeedContractor {
        license_no: "771204",
        name: "Stillwater Plumbing",
        status: "active",
        postal_code: "90401",
        classification: "C-36",
    },
    SeedContractor {
        license_no: "445190",
        name: "Reinholt Roofing",
        status: "expired",
        postal_code: "90026",
        classification: "C-39",
    },
];

/// Populate reference data and a small directory, without going through the
/// importer — these tests are about the HTTP surface, not the file format.
pub async fn seed_directory(pool: &PgPool) {
    let mut conn = pool.acquire().await.expect("connection");
    cm_db::repo::reference::seed_trades(&mut conn)
        .await
        .expect("trades");

    for (code, name, lat, lon) in LA_ZIPS {
        cm_db::repo::reference::upsert_zcta(&mut conn, code, name, *lat, *lon, "test")
            .await
            .expect("zcta");
    }

    let run_id = cm_db::repo::licenses::begin_run(
        &mut conn,
        cm_db::repo::licenses::Source::CslbMasterList,
        "fixture.csv",
        &[7u8; 32],
        None,
    )
    .await
    .expect("run");

    for seed in SEED_CONTRACTORS {
        let record = cm_db::repo::licenses::LicenseRecord {
            license_no: seed.license_no.to_owned(),
            business_name: seed.name.to_owned(),
            business_type: Some("Corporation".to_owned()),
            status: cm_db::repo::licenses::LicenseStatus::parse(seed.status).expect("status"),
            status_raw: seed.status.to_uppercase(),
            issue_date: None,
            expiration_date: None,
            classifications: vec![seed.classification.to_owned()],
            bond_amount_cents: Some(2_500_000),
            workers_comp_status: Some("Covered".to_owned()),
            address_line1: Some("100 Main St".to_owned()),
            city: Some("Los Angeles".to_owned()),
            state: Some("CA".to_owned()),
            postal_code: Some(seed.postal_code.to_owned()),
            county: Some("LOS ANGELES".to_owned()),
            phone: None,
            raw: serde_json::json!({ "LicenseNo": seed.license_no }),
            content_hash: vec![0u8; 32],
        };

        let stored = cm_db::repo::licenses::upsert(&mut conn, run_id, &record)
            .await
            .expect("licence");

        let region = cm_db::repo::reference::find_zcta(&mut conn, seed.postal_code)
            .await
            .expect("zcta")
            .expect("known zip");

        let upserted = cm_db::repo::contractors::upsert_from_license(
            &mut conn,
            &cm_db::repo::contractors::SourceFacts {
                license_record_id: stored.id,
                display_name: seed.name.to_owned(),
                slug: format!("{}-{}", cm_domain::slugify(seed.name), seed.license_no),
                postal_code: Some(seed.postal_code.to_owned()),
                region_id: Some(region.id),
            },
        )
        .await
        .expect("contractor");

        let trade_ids = cm_db::repo::reference::all_trades(&mut conn)
            .await
            .expect("trades")
            .into_iter()
            .filter(|t| t.cslb_classification.as_deref() == Some(seed.classification))
            .map(|t| t.id)
            .collect::<Vec<_>>();
        cm_db::repo::contractors::replace_cslb_trades(&mut conn, upserted.id, &trade_ids)
            .await
            .expect("trades");

        cm_domain::location::apply_zip_centroid(&mut conn, upserted.id)
            .await
            .expect("locate");
        cm_domain::verification::recompute(&mut conn, upserted.id, Some(run_id))
            .await
            .expect("verify");
    }
}

/// The contractor id behind a licence number.
pub async fn contractor_id(pool: &PgPool, license_no: &str) -> uuid::Uuid {
    sqlx::query_scalar(
        "SELECT c.id FROM contractors c JOIN license_records l ON l.id = c.license_record_id \
          WHERE l.license_no = $1",
    )
    .bind(license_no)
    .fetch_one(pool)
    .await
    .expect("contractor")
}

/// Claim a listing on someone's behalf, bypassing the approval workflow, for
/// tests whose subject is something else.
pub async fn force_claim(pool: &PgPool, contractor_id: uuid::Uuid, user_id: uuid::Uuid) {
    let mut conn = pool.acquire().await.expect("connection");
    cm_db::repo::contractors::attach_claimant(&mut conn, contractor_id, user_id)
        .await
        .expect("claim");
    cm_domain::verification::recompute(&mut conn, contractor_id, None)
        .await
        .expect("verify");
}

/// The account id behind an email address.
pub async fn user_id(pool: &PgPool, email: &str) -> uuid::Uuid {
    sqlx::query_scalar("SELECT id FROM users WHERE email_norm = lower($1)")
        .bind(email)
        .fetch_one(pool)
        .await
        .expect("user")
}

/// Insert jobs straight through the repository, bypassing the HTTP path.
///
/// Posting is rate-limited to ten a day per account, which is right for a real
/// homeowner and wrong for a test whose subject is pagination. Same reasoning as
/// `force_claim`: a test about one thing should not have to satisfy the rules of
/// another.
///
/// Returns the ids in insertion order.
pub async fn seed_jobs(
    pool: &PgPool,
    poster: uuid::Uuid,
    count: usize,
    postal_code: &str,
) -> Vec<uuid::Uuid> {
    let mut conn = pool.acquire().await.expect("connection");
    let region = cm_db::repo::reference::find_zcta(&mut conn, postal_code)
        .await
        .expect("zcta");

    let mut ids = Vec::with_capacity(count);
    for n in 0..count {
        let id = cm_core::new_id();
        cm_db::repo::jobs::insert(
            &mut conn,
            cm_db::repo::jobs::NewJob {
                id,
                posted_by_user_id: poster,
                title: &format!("Job number {n}"),
                description: "Seeded for a test that is not about posting.",
                trade_id: None,
                budget_min_cents: None,
                budget_max_cents: None,
                timeline: None,
                postal_code: Some(postal_code),
                region_id: region.as_ref().map(|r| r.id),
                centroid: region.as_ref().map(|r| (r.lon, r.lat)),
            },
        )
        .await
        .expect("seed a job");
        ids.push(id);
    }
    ids
}
