//! Configuration, loaded from the environment and validated before anything
//! else starts.
//!
//! Two properties matter here. The process refuses to boot on a bad value
//! rather than panicking at the first request that touches it, and a bad
//! environment reports *every* problem at once — discovering four missing
//! variables one deploy at a time is its own kind of outage.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;

/// Which deployment this process believes it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    Development,
    Staging,
    Production,
}

impl Environment {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::Staging => "staging",
            Self::Production => "production",
        }
    }

    pub fn is_production(self) -> bool {
        matches!(self, Self::Production)
    }
}

impl fmt::Display for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Environment {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "development" | "dev" | "local" => Ok(Self::Development),
            "staging" | "stage" => Ok(Self::Staging),
            "production" | "prod" => Ok(Self::Production),
            _ => Err(()),
        }
    }
}

/// How log lines are rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// One JSON object per line, for journald and log shipping.
    Json,
    /// Human-readable, for a terminal.
    Pretty,
}

impl FromStr for LogFormat {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "json" => Ok(Self::Json),
            "pretty" | "text" => Ok(Self::Pretty),
            _ => Err(()),
        }
    }
}

/// A value that must never reach a log line or an error message.
///
/// `Debug` is the leak that actually happens in practice — a struct derives it,
/// something logs the struct, and a password ends up in journald. Redacting at
/// the type level means no call site has to remember.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// Deliberately verbose: every read of a secret should be greppable.
    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// The site this API belongs to, e.g. `https://app.example.com`.
///
/// Parsed strictly rather than with a general URL parser: the only shape that
/// is ever valid here is exactly what a browser puts in an `Origin` header, and
/// CSRF checking compares the two as normalised strings. Anything with a path,
/// a query, credentials or a trailing slash is a misconfiguration, not
/// something to silently accept and then fail to match at request time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    normalised: String,
    secure: bool,
}

impl Origin {
    /// The normalised `scheme://host[:port]` form. Default ports are dropped so
    /// `https://x:443` and `https://x` compare equal.
    pub fn as_str(&self) -> &str {
        &self.normalised
    }

    pub fn is_secure(&self) -> bool {
        self.secure
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        let (scheme, rest) = raw
            .split_once("://")
            .ok_or_else(|| "expected scheme://host[:port]".to_owned())?;

        let scheme = scheme.to_ascii_lowercase();
        let secure = match scheme.as_str() {
            "https" => true,
            "http" => false,
            other => return Err(format!("unsupported scheme \"{other}\"; use http or https")),
        };

        if rest.contains('/') || rest.contains('?') || rest.contains('#') {
            return Err(
                "must not contain a path, query or fragment (and no trailing slash)".to_owned(),
            );
        }
        if rest.contains('@') {
            return Err("must not contain credentials".to_owned());
        }
        if rest.is_empty() {
            return Err("expected a host".to_owned());
        }

        // An IPv6 literal must be bracketed, exactly as a browser sends it.
        // Splitting on the last colon without checking would read the tail of
        // `https://::1:8080` as a port and accept a host that is not one.
        let (host, port_text) = if let Some(rest) = rest.strip_prefix('[') {
            let (host, after) = rest
                .split_once(']')
                .ok_or_else(|| "unclosed '[' in an IPv6 host".to_owned())?;
            match after {
                "" => (format!("[{host}]"), None),
                after => match after.strip_prefix(':') {
                    Some(port) => (format!("[{host}]"), Some(port)),
                    None => return Err("expected ':' or nothing after ']'".to_owned()),
                },
            }
        } else {
            match rest.split_once(':') {
                Some((host, port)) if !host.contains(':') && !port.contains(':') => {
                    (host.to_owned(), Some(port))
                }
                Some(_) => return Err("bracket an IPv6 host, e.g. https://[::1]:8080".to_owned()),
                None => (rest.to_owned(), None),
            }
        };

        let port = match port_text {
            Some(text) => Some(
                text.parse::<u16>()
                    .map_err(|_| format!("\"{text}\" is not a valid port"))?,
            ),
            None => None,
        };

        let host = host.to_ascii_lowercase();
        if host.is_empty() || host == "[]" {
            return Err("expected a host".to_owned());
        }

        let default_port = if secure { 443 } else { 80 };
        let normalised = match port {
            Some(port) if port != default_port => format!("{scheme}://{host}:{port}"),
            _ => format!("{scheme}://{host}"),
        };

        Ok(Self { normalised, secure })
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.normalised)
    }
}

/// Authentication settings.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// Keys the digests of values that must not be stored in the clear: client
    /// IP addresses, and rate-limit bucket keys. Also keys the CSRF token
    /// derivation. Rotating it invalidates outstanding CSRF tokens and orphans
    /// existing IP digests; it does not log anyone out.
    pub hash_pepper: Secret<String>,
    /// How long a session survives without being used.
    pub session_idle: Duration,
    /// How long a session survives at all, however active.
    pub session_absolute: Duration,
    /// Concurrent Argon2 hashes. Each holds ~19 MiB, so this is a memory
    /// budget as much as a CPU one.
    pub argon2_max_concurrency: usize,
    /// Whether `X-Forwarded-For` may be believed. False unless a reverse proxy
    /// is genuinely the only way in: a client that can reach the port directly
    /// would otherwise choose its own rate-limit bucket.
    pub trust_proxy_headers: bool,
    /// Google sign-in, when configured. Absent means the endpoint answers
    /// "not configured" rather than half-working.
    pub firebase: Option<FirebaseConfig>,
}

/// Firebase Authentication settings.
///
/// Only a project id is needed: verification uses Google's public keys, so no
/// service-account credential is handled by this service at all.
#[derive(Debug, Clone)]
pub struct FirebaseConfig {
    pub project_id: String,
    /// Set only for local development against the Auth emulator, which issues
    /// **unsigned** tokens. Refused in production by the loader below.
    pub emulator_host: Option<String>,
}

/// Database connection settings.
#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    pub url: Secret<String>,
    pub max_connections: u32,
    pub acquire_timeout: Duration,
}

/// The fully validated configuration for this process.
#[derive(Debug, Clone)]
pub struct Config {
    pub environment: Environment,
    pub bind_addr: SocketAddr,
    pub site_origin: Origin,
    pub log_format: LogFormat,
    pub database: DatabaseConfig,
    pub auth: AuthConfig,
    pub shutdown_grace: Duration,
    /// The GCS bucket job photos are stored in.
    ///
    /// Optional so development and tests run with an in-memory store and no
    /// credentials. Production refuses to start without it — see
    /// `Config::production_gaps`, which is what stops an unset variable from
    /// quietly downgrading a live server to storage that vanishes on restart.
    pub job_photo_bucket: Option<String>,
}

/// One thing wrong with the environment.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("{key} is required but was not set")]
    Missing { key: &'static str },
    #[error("{key} is invalid: {reason}")]
    Invalid { key: &'static str, reason: String },
}

impl ConfigError {
    /// The variable at fault, so callers can report it without string matching.
    pub fn key(&self) -> &'static str {
        match self {
            Self::Missing { key } | Self::Invalid { key, .. } => key,
        }
    }
}

/// Everything wrong with the environment, reported together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigErrors(Vec<ConfigError>);

impl ConfigErrors {
    pub fn as_slice(&self) -> &[ConfigError] {
        &self.0
    }

    pub fn keys(&self) -> Vec<&'static str> {
        self.0.iter().map(ConfigError::key).collect()
    }
}

impl fmt::Display for ConfigErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "configuration is invalid ({} problem(s)):", self.0.len())?;
        for error in &self.0 {
            writeln!(f, "  - {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigErrors {}

const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8080";
const DEFAULT_MAX_CONNECTIONS: u32 = 16;
const DEFAULT_ACQUIRE_TIMEOUT_SECS: u64 = 5;
const DEFAULT_SHUTDOWN_GRACE_SECS: u64 = 15;
const DEFAULT_SESSION_IDLE_DAYS: u64 = 14;
const DEFAULT_SESSION_ABSOLUTE_DAYS: u64 = 90;
const DEFAULT_ARGON2_MAX_CONCURRENCY: usize = 4;
/// 32 bytes of entropy is the floor for a value that keys HMACs and digests.
const MIN_PEPPER_LEN: usize = 32;

impl Config {
    /// Load from the process environment.
    pub fn from_env() -> Result<Self, ConfigErrors> {
        Self::load(|key| std::env::var(key).ok())
    }

    /// Load from an arbitrary source.
    ///
    /// Tests use this rather than mutating the process environment: `set_var`
    /// is global, and a test suite that races on it fails in ways that look
    /// like bugs in the code under test.
    pub fn load(source: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigErrors> {
        let mut errors = Vec::new();

        let environment = required_or_default(
            &source,
            "CM_ENV",
            Environment::Development,
            "one of development, staging, production",
            &mut errors,
        );

        let database_url = match read(&source, "DATABASE_URL") {
            Some(url) if is_postgres_url(&url) => Some(Secret::new(url)),
            Some(_) => {
                errors.push(ConfigError::Invalid {
                    key: "DATABASE_URL",
                    reason: "expected a postgres:// or postgresql:// URL".to_owned(),
                });
                None
            }
            None => {
                errors.push(ConfigError::Missing {
                    key: "DATABASE_URL",
                });
                None
            }
        };

        let bind_addr = parse_or_default::<SocketAddr>(
            &source,
            "CM_BIND_ADDR",
            DEFAULT_BIND_ADDR,
            "expected host:port, for example 127.0.0.1:8080",
            &mut errors,
        );

        // Default depends on the environment: a terminal wants readable logs, a
        // server wants parseable ones.
        let log_format = match read(&source, "CM_LOG_FORMAT") {
            Some(raw) => match raw.parse::<LogFormat>() {
                Ok(value) => Some(value),
                Err(()) => {
                    errors.push(ConfigError::Invalid {
                        key: "CM_LOG_FORMAT",
                        reason: "expected json or pretty".to_owned(),
                    });
                    None
                }
            },
            None => Some(match environment {
                Some(Environment::Development) | None => LogFormat::Pretty,
                Some(_) => LogFormat::Json,
            }),
        };

        let max_connections = bounded_or_default::<u32>(
            &source,
            "CM_DB_MAX_CONNECTIONS",
            DEFAULT_MAX_CONNECTIONS,
            1,
            200,
            &mut errors,
        );
        let acquire_timeout_secs = bounded_or_default::<u64>(
            &source,
            "CM_DB_ACQUIRE_TIMEOUT_SECS",
            DEFAULT_ACQUIRE_TIMEOUT_SECS,
            1,
            60,
            &mut errors,
        );
        let shutdown_grace_secs = bounded_or_default::<u64>(
            &source,
            "CM_SHUTDOWN_GRACE_SECS",
            DEFAULT_SHUTDOWN_GRACE_SECS,
            1,
            300,
            &mut errors,
        );

        let site_origin = match read(&source, "CM_SITE_ORIGIN") {
            Some(raw) => match Origin::parse(&raw) {
                Ok(origin) => {
                    // Session cookies carry the `__Host-` prefix, which browsers
                    // only accept with `Secure`. A production deployment served
                    // over http would set cookies the browser silently drops,
                    // and every request would look like a logged-out one.
                    if environment == Some(Environment::Production) && !origin.is_secure() {
                        errors.push(ConfigError::Invalid {
                            key: "CM_SITE_ORIGIN",
                            reason: "must be https in production: __Host- cookies require Secure"
                                .to_owned(),
                        });
                        None
                    } else {
                        Some(origin)
                    }
                }
                Err(reason) => {
                    errors.push(ConfigError::Invalid {
                        key: "CM_SITE_ORIGIN",
                        reason,
                    });
                    None
                }
            },
            None => {
                errors.push(ConfigError::Missing {
                    key: "CM_SITE_ORIGIN",
                });
                None
            }
        };

        let hash_pepper = match read(&source, "CM_HASH_PEPPER") {
            Some(pepper) if pepper.len() >= MIN_PEPPER_LEN => Some(Secret::new(pepper)),
            Some(pepper) => {
                errors.push(ConfigError::Invalid {
                    key: "CM_HASH_PEPPER",
                    reason: format!(
                        "must be at least {MIN_PEPPER_LEN} characters, got {}",
                        pepper.len()
                    ),
                });
                None
            }
            None => {
                errors.push(ConfigError::Missing {
                    key: "CM_HASH_PEPPER",
                });
                None
            }
        };

        let session_idle_days = bounded_or_default::<u64>(
            &source,
            "CM_SESSION_IDLE_DAYS",
            DEFAULT_SESSION_IDLE_DAYS,
            1,
            365,
            &mut errors,
        );
        let session_absolute_days = bounded_or_default::<u64>(
            &source,
            "CM_SESSION_ABSOLUTE_DAYS",
            DEFAULT_SESSION_ABSOLUTE_DAYS,
            1,
            730,
            &mut errors,
        );
        // An absolute lifetime shorter than the idle one is a configuration
        // that silently ignores the idle setting; say so rather than picking
        // one of the two.
        if let (Some(idle), Some(absolute)) = (session_idle_days, session_absolute_days) {
            if idle > absolute {
                errors.push(ConfigError::Invalid {
                    key: "CM_SESSION_IDLE_DAYS",
                    reason: format!(
                        "{idle} days exceeds CM_SESSION_ABSOLUTE_DAYS ({absolute}); \
                         an idle window longer than the absolute one has no effect"
                    ),
                });
            }
        }

        let argon2_max_concurrency = bounded_or_default::<usize>(
            &source,
            "CM_ARGON2_MAX_CONCURRENCY",
            DEFAULT_ARGON2_MAX_CONCURRENCY,
            1,
            64,
            &mut errors,
        );

        let firebase_project_id = read(&source, "FIREBASE_PROJECT_ID");
        let firebase_emulator_host = read(&source, "FIREBASE_AUTH_EMULATOR_HOST");
        let mut firebase = None;

        // The interlock. Emulator tokens carry no signature at all, so a
        // production process that accepted them would accept anything anyone
        // cared to encode. Refusing to start is the only safe response.
        if firebase_emulator_host.is_some() && environment == Some(Environment::Production) {
            errors.push(ConfigError::Invalid {
                key: "FIREBASE_AUTH_EMULATOR_HOST",
                reason: "must not be set in production: emulator tokens are unsigned".to_owned(),
            });
        } else if let Some(project_id) = firebase_project_id {
            firebase = Some(FirebaseConfig {
                project_id,
                emulator_host: firebase_emulator_host,
            });
        } else if firebase_emulator_host.is_some() {
            errors.push(ConfigError::Invalid {
                key: "FIREBASE_AUTH_EMULATOR_HOST",
                reason: "is set but FIREBASE_PROJECT_ID is not".to_owned(),
            });
        }

        let trust_proxy_headers = match read(&source, "CM_TRUST_PROXY_HEADERS") {
            None => Some(false),
            Some(raw) => match raw.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => Some(true),
                "false" | "0" | "no" => Some(false),
                _ => {
                    errors.push(ConfigError::Invalid {
                        key: "CM_TRUST_PROXY_HEADERS",
                        reason: "expected true or false".to_owned(),
                    });
                    None
                }
            },
        };

        if !errors.is_empty() {
            return Err(ConfigErrors(errors));
        }

        // Every `expect` below is unreachable: a `None` pushed an error, and a
        // non-empty error list returned above.
        Ok(Self {
            environment: environment.expect("environment validated"),
            bind_addr: bind_addr.expect("bind_addr validated"),
            site_origin: site_origin.expect("site_origin validated"),
            log_format: log_format.expect("log_format validated"),
            database: DatabaseConfig {
                url: database_url.expect("database_url validated"),
                max_connections: max_connections.expect("max_connections validated"),
                acquire_timeout: Duration::from_secs(
                    acquire_timeout_secs.expect("acquire_timeout validated"),
                ),
            },
            auth: AuthConfig {
                hash_pepper: hash_pepper.expect("hash_pepper validated"),
                session_idle: Duration::from_secs(
                    session_idle_days.expect("session_idle validated") * 86_400,
                ),
                session_absolute: Duration::from_secs(
                    session_absolute_days.expect("session_absolute validated") * 86_400,
                ),
                argon2_max_concurrency: argon2_max_concurrency
                    .expect("argon2_max_concurrency validated"),
                trust_proxy_headers: trust_proxy_headers.expect("trust_proxy_headers validated"),
                firebase,
            },
            shutdown_grace: Duration::from_secs(
                shutdown_grace_secs.expect("shutdown_grace validated"),
            ),
            job_photo_bucket: read(&source, "CM_JOB_PHOTO_BUCKET"),
        })
    }

    /// Settings that are acceptable in development and not in production.
    ///
    /// Kept apart from `load` because they are not malformed values — they are
    /// absent ones that only matter once real people are using the thing. Each
    /// is returned as a sentence an operator can act on.
    pub fn production_gaps(&self) -> Vec<String> {
        let mut gaps = Vec::new();

        if self.environment == Environment::Production && self.job_photo_bucket.is_none() {
            gaps.push(
                "CM_JOB_PHOTO_BUCKET is not set, so job photos would be held in memory and \
                 lost on restart. Set it to the GCS bucket name."
                    .to_owned(),
            );
        }

        gaps
    }

    /// A redacted view suitable for logging at startup.
    pub fn redacted_summary(&self) -> BTreeMap<&'static str, String> {
        BTreeMap::from([
            ("environment", self.environment.to_string()),
            ("bind_addr", self.bind_addr.to_string()),
            ("site_origin", self.site_origin.to_string()),
            (
                "log_format",
                match self.log_format {
                    LogFormat::Json => "json".to_owned(),
                    LogFormat::Pretty => "pretty".to_owned(),
                },
            ),
            ("database_url", "[redacted]".to_owned()),
            (
                "job_photo_bucket",
                match &self.job_photo_bucket {
                    Some(bucket) => bucket.clone(),
                    None => "(unset — in-memory, NOT durable)".to_owned(),
                },
            ),
            (
                "db_max_connections",
                self.database.max_connections.to_string(),
            ),
            (
                "db_acquire_timeout_secs",
                self.database.acquire_timeout.as_secs().to_string(),
            ),
            (
                "shutdown_grace_secs",
                self.shutdown_grace.as_secs().to_string(),
            ),
            ("hash_pepper", "[redacted]".to_owned()),
            (
                "session_idle_days",
                (self.auth.session_idle.as_secs() / 86_400).to_string(),
            ),
            (
                "session_absolute_days",
                (self.auth.session_absolute.as_secs() / 86_400).to_string(),
            ),
            (
                "argon2_max_concurrency",
                self.auth.argon2_max_concurrency.to_string(),
            ),
            (
                "trust_proxy_headers",
                self.auth.trust_proxy_headers.to_string(),
            ),
            (
                "google_sign_in",
                match &self.auth.firebase {
                    None => "not configured".to_owned(),
                    Some(firebase) if firebase.emulator_host.is_some() => {
                        format!("{} (EMULATOR — tokens are unsigned)", firebase.project_id)
                    }
                    Some(firebase) => firebase.project_id.clone(),
                },
            ),
        ])
    }
}

/// Treats an empty or whitespace-only variable as unset: `FOO=` in a systemd
/// EnvironmentFile is a variable someone meant to fill in, not an empty value.
fn read(source: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    source(key)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn is_postgres_url(url: &str) -> bool {
    url.starts_with("postgres://") || url.starts_with("postgresql://")
}

fn required_or_default<T: FromStr<Err = ()>>(
    source: &impl Fn(&str) -> Option<String>,
    key: &'static str,
    default: T,
    expected: &str,
    errors: &mut Vec<ConfigError>,
) -> Option<T> {
    match read(source, key) {
        None => Some(default),
        Some(raw) => match raw.parse::<T>() {
            Ok(value) => Some(value),
            Err(()) => {
                errors.push(ConfigError::Invalid {
                    key,
                    reason: format!("expected {expected}"),
                });
                None
            }
        },
    }
}

fn parse_or_default<T>(
    source: &impl Fn(&str) -> Option<String>,
    key: &'static str,
    default: &str,
    expected: &str,
    errors: &mut Vec<ConfigError>,
) -> Option<T>
where
    T: FromStr,
{
    let raw = read(source, key).unwrap_or_else(|| default.to_owned());
    match raw.parse::<T>() {
        Ok(value) => Some(value),
        Err(_) => {
            errors.push(ConfigError::Invalid {
                key,
                reason: expected.to_owned(),
            });
            None
        }
    }
}

fn bounded_or_default<T>(
    source: &impl Fn(&str) -> Option<String>,
    key: &'static str,
    default: T,
    min: T,
    max: T,
    errors: &mut Vec<ConfigError>,
) -> Option<T>
where
    T: FromStr + PartialOrd + fmt::Display + Copy,
{
    let Some(raw) = read(source, key) else {
        return Some(default);
    };
    match raw.parse::<T>() {
        Ok(value) if value >= min && value <= max => Some(value),
        Ok(value) => {
            errors.push(ConfigError::Invalid {
                key,
                reason: format!("{value} is outside the allowed range {min}..={max}"),
            });
            None
        }
        Err(_) => {
            errors.push(ConfigError::Invalid {
                key,
                reason: format!("expected a whole number between {min} and {max}"),
            });
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    const PEPPER: &str = "a-pepper-that-is-at-least-32-characters-long";

    fn minimal() -> Vec<(&'static str, &'static str)> {
        vec![
            ("DATABASE_URL", "postgres://cmdev@127.0.0.1:5432/cm_dev"),
            ("CM_SITE_ORIGIN", "http://localhost:3000"),
            ("CM_HASH_PEPPER", PEPPER),
        ]
    }

    #[test]
    fn loads_with_only_the_required_variable_set() {
        let config = Config::load(env(&minimal())).expect("should load");

        assert_eq!(config.environment, Environment::Development);
        assert_eq!(config.bind_addr.to_string(), DEFAULT_BIND_ADDR);
        assert_eq!(config.log_format, LogFormat::Pretty);
        assert_eq!(config.database.max_connections, DEFAULT_MAX_CONNECTIONS);
        assert_eq!(config.shutdown_grace, Duration::from_secs(15));
    }

    #[test]
    fn missing_database_url_is_named_not_panicked() {
        let errors = Config::load(env(&[])).expect_err("should fail");

        assert!(errors.as_slice().contains(&ConfigError::Missing {
            key: "DATABASE_URL"
        }));
        assert!(errors.to_string().contains("DATABASE_URL"));
    }

    #[test]
    fn empty_variable_is_treated_as_unset() {
        let errors = Config::load(env(&[("DATABASE_URL", "   ")])).expect_err("should fail");
        assert!(errors.keys().contains(&"DATABASE_URL"));
    }

    #[test]
    fn a_non_postgres_database_url_is_rejected() {
        let errors = Config::load(env(&[("DATABASE_URL", "mysql://localhost/cm")]))
            .expect_err("should fail");

        assert!(errors.keys().contains(&"DATABASE_URL"));
        assert!(errors.to_string().contains("postgres://"));
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let errors = Config::load(env(&[
            ("CM_ENV", "banana"),
            ("CM_BIND_ADDR", "not-an-address"),
            ("CM_LOG_FORMAT", "yaml"),
            ("CM_DB_MAX_CONNECTIONS", "0"),
        ]))
        .expect_err("should fail");

        let mut keys = errors.keys();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "CM_BIND_ADDR",
                "CM_DB_MAX_CONNECTIONS",
                "CM_ENV",
                "CM_HASH_PEPPER",
                "CM_LOG_FORMAT",
                "CM_SITE_ORIGIN",
                "DATABASE_URL",
            ]
        );
    }

    #[test]
    fn out_of_range_numbers_name_the_range() {
        let mut vars = minimal();
        vars.push(("CM_DB_MAX_CONNECTIONS", "5000"));
        let errors = Config::load(env(&vars)).expect_err("should fail");

        assert_eq!(errors.keys(), vec!["CM_DB_MAX_CONNECTIONS"]);
        assert!(errors.to_string().contains("1..=200"));
    }

    #[test]
    fn production_defaults_to_json_logs() {
        let mut vars = minimal();
        vars.push(("CM_ENV", "production"));
        vars.retain(|(k, _)| *k != "CM_SITE_ORIGIN");
        vars.push(("CM_SITE_ORIGIN", "https://app.example.com"));
        let config = Config::load(env(&vars)).expect("should load");

        assert_eq!(config.environment, Environment::Production);
        assert_eq!(config.log_format, LogFormat::Json);
        assert!(config.environment.is_production());
    }

    #[test]
    fn origins_normalise_scheme_host_and_default_ports() {
        for (raw, expected) in [
            ("https://App.Example.COM", "https://app.example.com"),
            ("https://app.example.com:443", "https://app.example.com"),
            ("http://localhost:80", "http://localhost"),
            ("http://localhost:3000", "http://localhost:3000"),
            ("http://[::1]:8080", "http://[::1]:8080"),
            ("https://[::1]", "https://[::1]"),
            ("  https://app.example.com  ", "https://app.example.com"),
        ] {
            assert_eq!(
                Origin::parse(raw)
                    .unwrap_or_else(|e| panic!("{raw}: {e}"))
                    .as_str(),
                expected
            );
        }
    }

    #[test]
    fn origins_reject_anything_a_browser_would_never_send() {
        for raw in [
            "app.example.com",
            "ftp://app.example.com",
            "https://app.example.com/",
            "https://app.example.com/path",
            "https://app.example.com?q=1",
            "https://user:pw@app.example.com",
            "https://app.example.com:notaport",
            "https://::1:8080",
            "https://[::1",
            "https://[::1]x",
            "https://[]",
            "https://",
        ] {
            assert!(Origin::parse(raw).is_err(), "{raw} should be rejected");
        }
    }

    #[test]
    fn production_requires_an_https_site_origin() {
        let mut vars = minimal();
        vars.push(("CM_ENV", "production"));
        let errors = Config::load(env(&vars)).expect_err("http in production should fail");

        assert_eq!(errors.keys(), vec!["CM_SITE_ORIGIN"]);
        assert!(errors.to_string().contains("__Host-"));
    }

    #[test]
    fn a_short_pepper_is_rejected() {
        let mut vars = minimal();
        vars.retain(|(k, _)| *k != "CM_HASH_PEPPER");
        vars.push(("CM_HASH_PEPPER", "too-short"));
        let errors = Config::load(env(&vars)).expect_err("should fail");

        assert_eq!(errors.keys(), vec!["CM_HASH_PEPPER"]);
        assert!(errors.to_string().contains("32"));
    }

    #[test]
    fn an_idle_window_longer_than_the_absolute_one_is_rejected() {
        let mut vars = minimal();
        vars.push(("CM_SESSION_IDLE_DAYS", "90"));
        vars.push(("CM_SESSION_ABSOLUTE_DAYS", "30"));
        let errors = Config::load(env(&vars)).expect_err("should fail");

        assert_eq!(errors.keys(), vec!["CM_SESSION_IDLE_DAYS"]);
    }

    #[test]
    fn proxy_headers_are_distrusted_unless_asked_for() {
        let config = Config::load(env(&minimal())).expect("should load");
        assert!(!config.auth.trust_proxy_headers);

        let mut vars = minimal();
        vars.push(("CM_TRUST_PROXY_HEADERS", "true"));
        assert!(
            Config::load(env(&vars))
                .expect("should load")
                .auth
                .trust_proxy_headers
        );

        let mut vars = minimal();
        vars.push(("CM_TRUST_PROXY_HEADERS", "maybe"));
        assert_eq!(
            Config::load(env(&vars)).expect_err("should fail").keys(),
            vec!["CM_TRUST_PROXY_HEADERS"]
        );
    }

    #[test]
    fn google_sign_in_is_off_unless_a_project_is_named() {
        let config = Config::load(env(&minimal())).expect("should load");
        assert!(config.auth.firebase.is_none());

        let mut vars = minimal();
        vars.push(("FIREBASE_PROJECT_ID", "cm-demo"));
        let config = Config::load(env(&vars)).expect("should load");
        let firebase = config.auth.firebase.expect("configured");
        assert_eq!(firebase.project_id, "cm-demo");
        assert!(firebase.emulator_host.is_none());
    }

    #[test]
    fn the_emulator_cannot_be_enabled_in_production() {
        let mut vars = minimal();
        vars.retain(|(k, _)| *k != "CM_SITE_ORIGIN");
        vars.push(("CM_SITE_ORIGIN", "https://app.example.com"));
        vars.push(("CM_ENV", "production"));
        vars.push(("FIREBASE_PROJECT_ID", "cm-demo"));
        vars.push(("FIREBASE_AUTH_EMULATOR_HOST", "127.0.0.1:9099"));

        let errors = Config::load(env(&vars)).expect_err("production must refuse the emulator");
        assert_eq!(errors.keys(), vec!["FIREBASE_AUTH_EMULATOR_HOST"]);
        assert!(errors.to_string().contains("unsigned"));
    }

    #[test]
    fn the_emulator_without_a_project_is_a_configuration_error() {
        let mut vars = minimal();
        vars.push(("FIREBASE_AUTH_EMULATOR_HOST", "127.0.0.1:9099"));
        assert_eq!(
            Config::load(env(&vars)).expect_err("should fail").keys(),
            vec!["FIREBASE_AUTH_EMULATOR_HOST"]
        );
    }

    #[test]
    fn auth_defaults_are_the_documented_ones() {
        let config = Config::load(env(&minimal())).expect("should load");
        assert_eq!(config.auth.session_idle, Duration::from_secs(14 * 86_400));
        assert_eq!(
            config.auth.session_absolute,
            Duration::from_secs(90 * 86_400)
        );
        assert_eq!(config.auth.argon2_max_concurrency, 4);
    }

    #[test]
    fn the_database_url_never_appears_in_debug_or_display_output() {
        let mut vars = minimal();
        vars.push(("CM_ENV", "production"));
        vars.retain(|(k, _)| *k != "CM_SITE_ORIGIN");
        vars.push(("CM_SITE_ORIGIN", "https://app.example.com"));
        let config = Config::load(env(&vars)).expect("should load");

        let debug = format!("{config:?}");
        assert!(
            !debug.contains("cm_dev"),
            "debug output leaked the URL: {debug}"
        );
        assert!(debug.contains("redacted"));

        assert!(
            !debug.contains(PEPPER),
            "debug output leaked the pepper: {debug}"
        );

        let summary = config.redacted_summary();
        assert_eq!(summary["database_url"], "[redacted]");
        assert_eq!(summary["hash_pepper"], "[redacted]");
        assert_eq!(
            config.database.url.expose(),
            "postgres://cmdev@127.0.0.1:5432/cm_dev"
        );
    }
}
