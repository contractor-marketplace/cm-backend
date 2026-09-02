//! Verification of Firebase-issued identity tokens.
//!
//! Firebase does one job here and no other: it runs the provider's sign-in
//! dance in the browser and hands back an ID token. This module checks that
//! token and then forgets it. Users, roles, sessions and every application
//! record live in our own database — Firebase is not a directory, not a session
//! store, and not consulted again after this call returns.
//!
//! Only public keys are needed to verify, so no service-account credential is
//! handled anywhere in this service, and there is no Admin SDK.
//!
//! Every check here is made against **one** provider, named by the caller and
//! never read out of the token. A token says which provider signed the user in;
//! letting it also choose which provider it will be checked as is how one
//! provider's token gets accepted as another's.

use cm_core::AppError;
use cm_db::repo::oauth::Provider;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

/// Where Google publishes the public keys for Firebase ID tokens.
pub const GOOGLE_JWK_URL: &str =
    "https://www.googleapis.com/service_accounts/v1/jwk/securetoken@system.gserviceaccount.com";

/// Tolerance for clock skew between us and Google.
const LEEWAY_SECS: u64 = 60;
/// Never refetch keys more often than this, even on repeated unknown key ids —
/// otherwise a flood of tokens carrying invented `kid`s becomes a way to make
/// this service hammer Google.
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(300);
/// How long a cached key set may be served after its stated lifetime before
/// verification starts failing. Failing closed is the point: serving stale keys
/// indefinitely would keep accepting tokens signed by a key Google has retired.
const MAX_STALENESS: Duration = Duration::from_secs(24 * 60 * 60);

/// A set of verification keys and how long they may be used.
#[derive(Clone)]
pub struct KeySet {
    keys: HashMap<String, Arc<DecodingKey>>,
    fetched_at: Instant,
    max_age: Duration,
}

impl std::fmt::Debug for KeySet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeySet")
            .field("key_ids", &self.keys.keys().collect::<Vec<_>>())
            .field("max_age", &self.max_age)
            .finish()
    }
}

impl KeySet {
    pub fn new(keys: HashMap<String, DecodingKey>, max_age: Duration) -> Self {
        Self {
            keys: keys
                .into_iter()
                .map(|(id, key)| (id, Arc::new(key)))
                .collect(),
            fetched_at: Instant::now(),
            max_age,
        }
    }

    fn get(&self, key_id: &str) -> Option<Arc<DecodingKey>> {
        self.keys.get(key_id).cloned()
    }

    fn is_fresh(&self) -> bool {
        self.fetched_at.elapsed() < self.max_age
    }

    fn is_usable(&self) -> bool {
        self.fetched_at.elapsed() < self.max_age + MAX_STALENESS
    }
}

/// A source of verification keys. Injected so tests can supply a local key pair
/// without a network, and so the production fetcher stays a small, replaceable
/// piece.
pub type KeyFetcher =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<KeySet, AppError>> + Send>> + Send + Sync>;

/// How tokens are checked.
#[derive(Clone)]
pub enum Mode {
    /// Real Firebase: RS256 signatures against Google's published keys.
    Signed(KeyFetcher),
    /// The Firebase Auth emulator, which issues **unsigned** tokens. Every
    /// claim is still checked; only the signature is not, because there is
    /// none. Refused outright in production by the configuration layer.
    Emulator,
}

/// The claims we require and read.
#[derive(Debug, Deserialize)]
struct FirebaseClaims {
    sub: String,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    email_verified: Option<bool>,
    auth_time: i64,
    exp: i64,
    iat: i64,
    iss: String,
    aud: String,
    firebase: FirebaseSection,
}

#[derive(Debug, Deserialize)]
struct FirebaseSection {
    sign_in_provider: String,
    #[serde(default)]
    identities: HashMap<String, Vec<String>>,
}

/// Firebase's own name for a provider. It appears twice in a token — as
/// `sign_in_provider` and as a key of the `identities` map — and both must be
/// checked against the provider the caller asked for.
fn firebase_provider_id(provider: Provider) -> &'static str {
    match provider {
        Provider::Google => "google.com",
        Provider::Facebook => "facebook.com",
    }
}

/// What a verified token tells us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedIdentity {
    /// The provider's own account id — Google's subject, or Facebook's
    /// app-scoped user id. This is what keys `oauth_identities`, because a
    /// direct OIDC integration with the same provider returns the same value:
    /// dropping Firebase later re-links nobody. For Facebook that holds only
    /// while the Meta App ID stays the same, since the id is scoped to it.
    pub provider_subject: String,
    /// Firebase's own uid. Recorded for support, never matched on.
    pub firebase_uid: String,
    pub email: Option<String>,
    pub email_verified: bool,
}

pub struct FirebaseVerifier {
    project_id: String,
    mode: Mode,
    cache: RwLock<Option<KeySet>>,
    last_forced_refresh: Mutex<Option<Instant>>,
}

impl std::fmt::Debug for FirebaseVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FirebaseVerifier")
            .field("project_id", &self.project_id)
            .field(
                "mode",
                &match self.mode {
                    Mode::Signed(_) => "signed",
                    Mode::Emulator => "emulator",
                },
            )
            .finish()
    }
}

impl FirebaseVerifier {
    pub fn new(project_id: impl Into<String>, mode: Mode) -> Self {
        Self {
            project_id: project_id.into(),
            mode,
            cache: RwLock::new(None),
            last_forced_refresh: Mutex::new(None),
        }
    }

    /// The production key source: Google's JWK endpoint, honouring the
    /// `Cache-Control: max-age` it returns.
    pub fn google_key_fetcher(client: reqwest::Client) -> KeyFetcher {
        Arc::new(move || {
            let client = client.clone();
            Box::pin(async move {
                let response = client.get(GOOGLE_JWK_URL).send().await.map_err(|e| {
                    AppError::unavailable(format!("could not fetch Google's keys: {e}"))
                })?;

                if !response.status().is_success() {
                    return Err(AppError::unavailable(format!(
                        "Google's key endpoint answered {}",
                        response.status()
                    )));
                }

                let max_age = response
                    .headers()
                    .get(reqwest::header::CACHE_CONTROL)
                    .and_then(|value| value.to_str().ok())
                    .and_then(parse_max_age)
                    .unwrap_or(Duration::from_secs(3600));

                let document: JwkDocument = response.json().await.map_err(|e| {
                    AppError::unavailable(format!("Google's keys were unreadable: {e}"))
                })?;

                let mut keys = HashMap::new();
                for key in document.keys {
                    if key.kty != "RSA" || key.alg.as_deref() != Some("RS256") {
                        continue;
                    }
                    match DecodingKey::from_rsa_components(&key.n, &key.e) {
                        Ok(decoding) => {
                            keys.insert(key.kid, decoding);
                        }
                        Err(error) => {
                            tracing::warn!(kid = %key.kid, %error, "skipping an unusable Google key");
                        }
                    }
                }

                if keys.is_empty() {
                    return Err(AppError::unavailable("Google returned no usable keys"));
                }

                Ok(KeySet::new(keys, max_age))
            })
        })
    }

    /// Check a Firebase ID token and extract the identity behind it.
    ///
    /// `provider` is the provider the *caller* is asking about — the endpoint
    /// that was hit — never whatever the token happens to name. The two are
    /// then required to agree.
    pub async fn verify(
        &self,
        token: &str,
        provider: Provider,
    ) -> Result<VerifiedIdentity, AppError> {
        let claims = match &self.mode {
            Mode::Signed(fetcher) => self.verify_signed(token, fetcher).await?,
            Mode::Emulator => self.decode_unsigned(token)?,
        };

        self.check_claims(&claims, provider)?;

        // The provider's own subject, not Firebase's uid, and read from this
        // provider's slot only.
        //
        // With email-based account linking left on in the Firebase console, one
        // Firebase user accumulates several identities and this map carries all
        // of them at once — so reading any slot other than the one that matches
        // `sign_in_provider` would resolve a session to an account the person
        // signing in never proved control of. Linking is required to be off;
        // this reads the right slot regardless.
        //
        // Exactly one entry is required: a list that is empty or plural is not
        // a shape we understand, and guessing which entry to trust is how an
        // account gets linked to the wrong person.
        let identities = claims
            .firebase
            .identities
            .get(firebase_provider_id(provider))
            .map(Vec::as_slice)
            .unwrap_or_default();

        let provider_subject = match identities {
            [only] if !only.trim().is_empty() => only.clone(),
            [] => {
                return Err(AppError::invalid(format!(
                    "The sign-in token carries no {} identity.",
                    provider.display_name()
                )))
            }
            _ => {
                return Err(AppError::invalid(format!(
                    "The sign-in token carries more than one {} identity.",
                    provider.display_name()
                )))
            }
        };

        // The address, from wherever this console mode put it.
        //
        // With account linking off — the setting this product requires — the
        // documented behaviour is that OAuth tokens carry no top-level `email`
        // claim; the address travels only in the `identities` map, in the
        // `"email"` slot beside the provider subject read above. Reading only
        // the top level meant every federated sign-up was refused with "that
        // account has no email address", which on a Gmail account is absurd:
        // there was no way into the product through the provider buttons.
        //
        // The top level wins when present; the slot is a fallback, and only
        // when it holds exactly one address — picking one of several would
        // attach an address nobody proved, so ambiguity stays None and the
        // caller's refusal handles it. The verified flag is NOT inferred: an
        // address recovered from the fallback proves itself by the emailed
        // code like any other unverified address.
        let email = claims.email.clone().or_else(|| {
            match claims.firebase.identities.get("email").map(Vec::as_slice) {
                Some([only]) if !only.trim().is_empty() => Some(only.clone()),
                _ => None,
            }
        });

        Ok(VerifiedIdentity {
            provider_subject,
            firebase_uid: claims.sub.clone(),
            email,
            email_verified: claims.email_verified.unwrap_or(false),
        })
    }

    async fn verify_signed(
        &self,
        token: &str,
        fetcher: &KeyFetcher,
    ) -> Result<FirebaseClaims, AppError> {
        let header = jsonwebtoken::decode_header(token)
            .map_err(|_| AppError::invalid("The sign-in token is malformed."))?;

        // The header's own `alg` is never used to choose a verifier: that is
        // how algorithm-confusion attacks work. RS256 or nothing.
        if header.alg != Algorithm::RS256 {
            return Err(AppError::invalid("The sign-in token is not RS256-signed."));
        }
        let Some(key_id) = header.kid else {
            return Err(AppError::invalid("The sign-in token names no signing key."));
        };

        let key = match self.key(&key_id).await? {
            Some(key) => key,
            None => {
                // An unknown key id usually means rotation, so one forced
                // refresh is worth it — rate limited so it cannot be used as a
                // lever against Google.
                self.refresh_if_allowed(fetcher).await?;
                self.key(&key_id).await?.ok_or_else(|| {
                    AppError::invalid("The sign-in token was signed by an unknown key.")
                })?
            }
        };

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[&self.project_id]);
        validation.set_issuer(&[self.issuer()]);
        validation.leeway = LEEWAY_SECS;
        validation.validate_exp = true;
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);

        jsonwebtoken::decode::<FirebaseClaims>(token, &key, &validation)
            .map(|data| data.claims)
            .map_err(|error| {
                tracing::debug!(%error, "rejected a Firebase token");
                AppError::invalid("The sign-in token is not valid.")
            })
    }

    /// Emulator tokens are unsigned, so the payload is read directly. Every
    /// claim check still applies.
    fn decode_unsigned(&self, token: &str) -> Result<FirebaseClaims, AppError> {
        let mut parts = token.split('.');
        let (_header, payload) = match (parts.next(), parts.next(), parts.next()) {
            (Some(header), Some(payload), Some(_)) => (header, payload),
            _ => return Err(AppError::invalid("The sign-in token is malformed.")),
        };

        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        let decoded = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| AppError::invalid("The sign-in token is malformed."))?;

        serde_json::from_slice(&decoded)
            .map_err(|_| AppError::invalid("The sign-in token is missing required claims."))
    }

    /// The claim checks that apply in both modes.
    fn check_claims(&self, claims: &FirebaseClaims, provider: Provider) -> Result<(), AppError> {
        let now = chrono::Utc::now().timestamp();
        let leeway = LEEWAY_SECS as i64;

        if claims.iss != self.issuer() {
            return Err(AppError::invalid("The sign-in token has the wrong issuer."));
        }
        if claims.aud != self.project_id {
            return Err(AppError::invalid(
                "The sign-in token is for another project.",
            ));
        }
        if claims.exp <= now - leeway {
            return Err(AppError::invalid("The sign-in token has expired."));
        }
        if claims.iat > now + leeway {
            return Err(AppError::invalid(
                "The sign-in token was issued in the future.",
            ));
        }
        if claims.auth_time > now + leeway {
            return Err(AppError::invalid(
                "The sign-in token was authenticated in the future.",
            ));
        }
        if claims.sub.trim().is_empty() {
            return Err(AppError::invalid("The sign-in token names no subject."));
        }
        if let Some(user_id) = &claims.user_id {
            if user_id != &claims.sub {
                return Err(AppError::invalid("The sign-in token is inconsistent."));
            }
        }
        // The load-bearing line. Without it a token minted by any other
        // provider enabled on the Firebase project is accepted here — and once
        // a second provider exists that is not hypothetical, because a linked
        // Firebase user's `identities` map carries every provider's subject
        // including the one this endpoint is about to trust.
        //
        // Note what this is NOT: a check that the token names *a* provider we
        // support. It is a check that the token names *this* provider, the one
        // the caller asked for. Relaxing it to a set membership test reopens
        // exactly the hole it closes.
        if claims.firebase.sign_in_provider != firebase_provider_id(provider) {
            return Err(AppError::invalid("That sign-in method is not supported."));
        }

        Ok(())
    }

    fn issuer(&self) -> String {
        format!("https://securetoken.google.com/{}", self.project_id)
    }

    /// A key by id, refreshing first if the cache is absent or stale.
    async fn key(&self, key_id: &str) -> Result<Option<Arc<DecodingKey>>, AppError> {
        let Mode::Signed(fetcher) = &self.mode else {
            return Ok(None);
        };

        {
            let cache = self.cache.read().await;
            if let Some(keys) = cache.as_ref() {
                if keys.is_fresh() {
                    return Ok(keys.get(key_id));
                }
            }
        }

        // Stale or empty: try to refresh, but keep serving the last good set if
        // the refresh fails and the set is still within its staleness budget.
        match self.fetch(fetcher).await {
            Ok(()) => {}
            Err(error) => {
                let cache = self.cache.read().await;
                match cache.as_ref() {
                    Some(keys) if keys.is_usable() => {
                        tracing::warn!(%error, "serving stale Google keys; refresh failed");
                        return Ok(keys.get(key_id));
                    }
                    // Fail closed rather than accept anything.
                    _ => return Err(error),
                }
            }
        }

        let cache = self.cache.read().await;
        Ok(cache.as_ref().and_then(|keys| keys.get(key_id)))
    }

    async fn refresh_if_allowed(&self, fetcher: &KeyFetcher) -> Result<(), AppError> {
        let mut last = self.last_forced_refresh.lock().await;
        if let Some(at) = *last {
            if at.elapsed() < MIN_REFRESH_INTERVAL {
                return Ok(());
            }
        }
        *last = Some(Instant::now());
        drop(last);

        self.fetch(fetcher).await
    }

    async fn fetch(&self, fetcher: &KeyFetcher) -> Result<(), AppError> {
        let keys = fetcher().await?;
        tracing::debug!(?keys, "refreshed Google signing keys");
        *self.cache.write().await = Some(keys);
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct JwkDocument {
    keys: Vec<Jwk>,
}

#[derive(Debug, Deserialize)]
struct Jwk {
    kty: String,
    kid: String,
    n: String,
    e: String,
    #[serde(default)]
    alg: Option<String>,
}

fn parse_max_age(cache_control: &str) -> Option<Duration> {
    cache_control
        .split(',')
        .map(str::trim)
        .find_map(|directive| directive.strip_prefix("max-age="))
        .and_then(|seconds| seconds.parse::<u64>().ok())
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_age_is_read_out_of_a_realistic_header() {
        assert_eq!(
            parse_max_age("public, max-age=19962, must-revalidate, no-transform"),
            Some(Duration::from_secs(19962))
        );
        assert_eq!(parse_max_age("no-store"), None);
        assert_eq!(parse_max_age("max-age=banana"), None);
    }

    #[test]
    fn a_key_set_reports_freshness_and_usability() {
        let keys = KeySet::new(HashMap::new(), Duration::from_secs(0));
        assert!(!keys.is_fresh(), "a zero lifetime is immediately stale");
        assert!(keys.is_usable(), "but still inside the staleness budget");
    }
}
