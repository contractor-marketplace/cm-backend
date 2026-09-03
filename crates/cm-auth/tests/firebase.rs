//! Firebase ID token verification.
//!
//! Signed against a local key pair rather than a live Firebase project: the
//! logic under test is the claim checking and the key handling, and neither
//! needs Google to be reachable. The one thing this cannot prove is that
//! Google's real key document parses — that is exercised only against the live
//! endpoint, and is called out in the handover notes.

use cm_auth::firebase::{FirebaseVerifier, KeySet, Mode, VerifiedIdentity};
use cm_db::repo::oauth::Provider;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const PROJECT: &str = "cm-test-project";
const KID: &str = "test-key-1";

/// The signing key these tests use.
///
/// Generated per process rather than committed. A private key in a repository
/// is a bad pattern even when it is a throwaway: it trips secret scanners, and
/// the next person to see one has to work out whether it matters.
struct TestKeyPair {
    private_pem: String,
    public_pem: String,
}

fn key_pair() -> &'static TestKeyPair {
    static KEYS: std::sync::OnceLock<TestKeyPair> = std::sync::OnceLock::new();

    KEYS.get_or_init(|| {
        let private_pem = generate_rsa_key();
        let public = std::process::Command::new("openssl")
            .args(["rsa", "-pubout"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child
                    .stdin
                    .as_mut()
                    .expect("stdin")
                    .write_all(private_pem.as_bytes())?;
                child.wait_with_output()
            })
            .expect("openssl rsa -pubout");

        TestKeyPair {
            private_pem,
            public_pem: String::from_utf8(public.stdout).expect("public pem"),
        }
    })
}

/// A fresh 2048-bit RSA key, via the openssl binary. Kept out of the
/// dependency tree: nothing in the service generates RSA keys, and adding a
/// crate that can would be a dependency carried for tests alone.
fn generate_rsa_key() -> String {
    let output = std::process::Command::new("openssl")
        .args([
            "genpkey",
            "-algorithm",
            "RSA",
            "-pkeyopt",
            "rsa_keygen_bits:2048",
        ])
        .output()
        .expect("openssl is required to run these tests");
    assert!(output.status.success(), "openssl genpkey failed");
    String::from_utf8(output.stdout).expect("private pem")
}

fn key_set(kid: &str, max_age: Duration) -> KeySet {
    let mut keys = HashMap::new();
    keys.insert(
        kid.to_owned(),
        DecodingKey::from_rsa_pem(key_pair().public_pem.as_bytes()).expect("public key"),
    );
    KeySet::new(keys, max_age)
}

/// A verifier backed by the local key, plus a counter of how many times the key
/// source was consulted.
fn verifier(max_age: Duration) -> (FirebaseVerifier, Arc<AtomicUsize>) {
    let fetches = Arc::new(AtomicUsize::new(0));
    let counter = fetches.clone();
    let fetcher: cm_auth::firebase::KeyFetcher = Arc::new(move || {
        let counter = counter.clone();
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(key_set(KID, max_age))
        })
    });

    (
        FirebaseVerifier::new(PROJECT, Mode::Signed(fetcher)),
        fetches,
    )
}

struct TokenBuilder {
    claims: serde_json::Value,
    kid: String,
    algorithm: Algorithm,
}

impl TokenBuilder {
    fn valid() -> Self {
        let now = chrono::Utc::now().timestamp();
        Self {
            claims: json!({
                "sub": "firebase-uid-123",
                "user_id": "firebase-uid-123",
                "email": "marisol@example.test",
                "email_verified": true,
                "name": "Marisol Vega",
                "auth_time": now - 30,
                "iat": now - 30,
                "exp": now + 3600,
                "iss": format!("https://securetoken.google.com/{PROJECT}"),
                "aud": PROJECT,
                "firebase": {
                    "sign_in_provider": "google.com",
                    "identities": { "google.com": ["100000000000000000001"] }
                }
            }),
            kid: KID.to_owned(),
            algorithm: Algorithm::RS256,
        }
    }

    /// A token as Firebase mints it for a Facebook sign-in. The subject is
    /// Meta's app-scoped user id, which is what `oauth_identities` stores.
    fn facebook() -> Self {
        Self::valid()
            .claim("firebase.sign_in_provider", json!("facebook.com"))
            .claim(
                "firebase.identities",
                json!({ "facebook.com": ["fb-app-scoped-id-1"] }),
            )
    }

    fn claim(mut self, path: &str, value: serde_json::Value) -> Self {
        let mut cursor = &mut self.claims;
        let parts: Vec<&str> = path.split('.').collect();
        for part in &parts[..parts.len() - 1] {
            cursor = cursor.get_mut(*part).expect("claim path exists");
        }
        cursor[parts[parts.len() - 1]] = value;
        self
    }

    fn remove(mut self, path: &str) -> Self {
        let mut cursor = &mut self.claims;
        let parts: Vec<&str> = path.split('.').collect();
        for part in &parts[..parts.len() - 1] {
            cursor = cursor.get_mut(*part).expect("claim path exists");
        }
        cursor
            .as_object_mut()
            .expect("object")
            .remove(parts[parts.len() - 1]);
        self
    }

    fn kid(mut self, kid: &str) -> Self {
        self.kid = kid.to_owned();
        self
    }

    fn algorithm(mut self, algorithm: Algorithm) -> Self {
        self.algorithm = algorithm;
        self
    }

    fn sign(self) -> String {
        let mut header = Header::new(self.algorithm);
        header.kid = Some(self.kid);
        let key = match self.algorithm {
            Algorithm::HS256 => EncodingKey::from_secret(b"a different signing scheme"),
            _ => EncodingKey::from_rsa_pem(key_pair().private_pem.as_bytes()).expect("private key"),
        };
        jsonwebtoken::encode(&header, &self.claims, &key).expect("sign")
    }

    /// An emulator-style token: header, payload, empty signature.
    fn unsigned(self) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(self.claims.to_string());
        format!("{header}.{payload}.")
    }
}

#[tokio::test]
async fn a_valid_token_yields_the_google_subject_not_the_firebase_uid() {
    let (verifier, fetches) = verifier(Duration::from_secs(3600));

    let identity = verifier
        .verify(&TokenBuilder::valid().sign(), Provider::Google)
        .await
        .expect("should verify");

    assert_eq!(
        identity,
        VerifiedIdentity {
            provider_subject: "100000000000000000001".to_owned(),
            firebase_uid: "firebase-uid-123".to_owned(),
            email: Some("marisol@example.test".to_owned()),
            email_from_identities: false,
            email_verified: true,
            name: Some("Marisol Vega".to_owned()),
        },
        "the identity key must be Google's subject, so dropping Firebase later re-links nobody"
    );
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_facebook_token_yields_the_app_scoped_id() {
    let (verifier, _) = verifier(Duration::from_secs(3600));

    let identity = verifier
        .verify(&TokenBuilder::facebook().sign(), Provider::Facebook)
        .await
        .expect("should verify");

    assert_eq!(
        identity.provider_subject, "fb-app-scoped-id-1",
        "Meta's app-scoped id is the key, so a later direct Facebook Login \
         against the same Meta app re-links nobody"
    );
}

/// The takeover that email-based account linking creates, refused in both
/// directions.
///
/// With linking left on in the Firebase console, one Firebase user accumulates
/// several identities and every token it mints carries all of them at once. A
/// token obtained by signing in with Facebook then also contains the Google
/// subject of whoever first signed in under that address — so a verifier
/// willing to read any slot other than the one that signed in would let control
/// of the Facebook side become control of the Google account.
///
/// The console setting is required to be off. This is the check that does not
/// depend on somebody having remembered to set it.
#[tokio::test]
async fn a_token_from_one_provider_is_never_accepted_as_another() {
    let (verifier, _) = verifier(Duration::from_secs(3600));

    let both = json!({
        "google.com": ["100000000000000000001"],
        "facebook.com": ["fb-app-scoped-id-1"]
    });

    let signed_in_with_facebook = TokenBuilder::valid()
        .claim("firebase.sign_in_provider", json!("facebook.com"))
        .claim("firebase.identities", both.clone())
        .sign();

    assert!(
        verifier
            .verify(&signed_in_with_facebook, Provider::Google)
            .await
            .is_err(),
        "a Facebook sign-in must never resolve to the Google identity riding \
         along in the same token"
    );

    let signed_in_with_google = TokenBuilder::valid()
        .claim("firebase.sign_in_provider", json!("google.com"))
        .claim("firebase.identities", both)
        .sign();

    assert!(
        verifier
            .verify(&signed_in_with_google, Provider::Facebook)
            .await
            .is_err(),
        "and the mirror image, so neither direction is safe only by accident"
    );

    // What it must still do: read its own slot and ignore the other.
    let identity = verifier
        .verify(&signed_in_with_google, Provider::Google)
        .await
        .expect("the provider that actually signed in still verifies");
    assert_eq!(identity.provider_subject, "100000000000000000001");
}

#[tokio::test]
async fn a_fresh_key_set_is_not_refetched() {
    let (verifier, fetches) = verifier(Duration::from_secs(3600));

    for _ in 0..5 {
        verifier
            .verify(&TokenBuilder::valid().sign(), Provider::Google)
            .await
            .expect("verify");
    }

    assert_eq!(
        fetches.load(Ordering::SeqCst),
        1,
        "a cached, fresh key set must not be refetched per request"
    );
}

#[tokio::test]
async fn a_stale_key_set_is_refetched() {
    let (verifier, fetches) = verifier(Duration::from_secs(0));

    verifier
        .verify(&TokenBuilder::valid().sign(), Provider::Google)
        .await
        .expect("verify");
    verifier
        .verify(&TokenBuilder::valid().sign(), Provider::Google)
        .await
        .expect("verify");

    assert!(
        fetches.load(Ordering::SeqCst) >= 2,
        "an expired key set must be refreshed"
    );
}

/// Every rejection path, as a table. A new claim check without a row here is a
/// check nobody is proving.
#[tokio::test]
async fn every_invalid_token_is_rejected() {
    let now = chrono::Utc::now().timestamp();

    let cases: Vec<(&str, String)> = vec![
        (
            "wrong issuer",
            TokenBuilder::valid()
                .claim("iss", json!("https://securetoken.google.com/someone-else"))
                .sign(),
        ),
        (
            "wrong audience",
            TokenBuilder::valid()
                .claim("aud", json!("another-project"))
                .sign(),
        ),
        (
            "expired",
            TokenBuilder::valid()
                .claim("exp", json!(now - 3600))
                .claim("iat", json!(now - 7200))
                .sign(),
        ),
        (
            "issued in the future",
            TokenBuilder::valid().claim("iat", json!(now + 3600)).sign(),
        ),
        (
            "authenticated in the future",
            TokenBuilder::valid()
                .claim("auth_time", json!(now + 3600))
                .sign(),
        ),
        (
            "empty subject",
            TokenBuilder::valid().claim("sub", json!("")).sign(),
        ),
        (
            "subject disagrees with user_id",
            TokenBuilder::valid()
                .claim("user_id", json!("someone-else"))
                .sign(),
        ),
        (
            "another sign-in provider",
            TokenBuilder::valid()
                .claim("firebase.sign_in_provider", json!("password"))
                .sign(),
        ),
        (
            "no google identity",
            TokenBuilder::valid()
                .claim("firebase.identities", json!({}))
                .sign(),
        ),
        (
            "two google identities",
            TokenBuilder::valid()
                .claim("firebase.identities", json!({"google.com": ["a", "b"]}))
                .sign(),
        ),
        (
            "unknown signing key",
            TokenBuilder::valid().kid("some-other-key").sign(),
        ),
        (
            "symmetric algorithm",
            TokenBuilder::valid().algorithm(Algorithm::HS256).sign(),
        ),
        ("unsigned", TokenBuilder::valid().unsigned()),
        ("not a token", "not-a-token".to_owned()),
        ("empty", String::new()),
        (
            "missing required claim",
            TokenBuilder::valid().remove("auth_time").sign(),
        ),
    ];

    let (verifier, _) = verifier(Duration::from_secs(3600));
    for (label, token) in cases {
        let result = verifier.verify(&token, Provider::Google).await;
        assert!(result.is_err(), "{label} should be rejected");
    }
}

/// A signature from a different key must not verify, even with a known `kid`.
#[tokio::test]
async fn a_token_signed_by_another_key_is_rejected() {
    let other_key = {
        let output = std::process::Command::new("openssl")
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
            ])
            .output()
            .expect("openssl");
        String::from_utf8(output.stdout).expect("pem")
    };

    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(KID.to_owned());
    let token = jsonwebtoken::encode(
        &header,
        &TokenBuilder::valid().claims,
        &EncodingKey::from_rsa_pem(other_key.as_bytes()).expect("other key"),
    )
    .expect("sign");

    let (verifier, _) = verifier(Duration::from_secs(3600));
    assert!(
        verifier.verify(&token, Provider::Google).await.is_err(),
        "a valid-looking token signed by the wrong key must be refused"
    );
}

/// Fail closed: if the keys cannot be fetched at all, nothing verifies.
#[tokio::test]
async fn verification_fails_closed_when_keys_are_unavailable() {
    let fetcher: cm_auth::firebase::KeyFetcher = Arc::new(|| {
        Box::pin(async { Err(cm_core::AppError::unavailable("Google is unreachable")) })
    });
    let verifier = FirebaseVerifier::new(PROJECT, Mode::Signed(fetcher));

    assert!(
        verifier
            .verify(&TokenBuilder::valid().sign(), Provider::Google)
            .await
            .is_err(),
        "an unavailable key source must never mean 'accept'"
    );
}

/// Emulator mode skips only the signature. Every claim check still applies.
#[tokio::test]
async fn emulator_mode_accepts_unsigned_tokens_but_still_checks_claims() {
    let verifier = FirebaseVerifier::new(PROJECT, Mode::Emulator);

    let identity = verifier
        .verify(&TokenBuilder::valid().unsigned(), Provider::Google)
        .await
        .expect("the emulator issues unsigned tokens");
    assert_eq!(identity.provider_subject, "100000000000000000001");

    for token in [
        TokenBuilder::valid()
            .claim("aud", json!("another-project"))
            .unsigned(),
        TokenBuilder::valid()
            .claim("firebase.sign_in_provider", json!("password"))
            .unsigned(),
        TokenBuilder::valid()
            .claim("exp", json!(chrono::Utc::now().timestamp() - 10_000))
            .unsigned(),
    ] {
        assert!(
            verifier.verify(&token, Provider::Google).await.is_err(),
            "emulator mode must still enforce the claims"
        );
    }
}

/// The token Firebase actually mints with account linking off.
///
/// The console mode this product requires — "create multiple accounts for
/// each identity provider" — has a documented side effect: the top-level
/// `email` claim is omitted for OAuth users. The address still travels in the
/// token, in the `identities` map beside the provider subject. Reading only
/// the top level turned every federated sign-up away with "that account has
/// no email address", which on a Gmail account is absurd — there was no way
/// into the product through the Google or Facebook buttons at all.
#[tokio::test]
async fn a_multiple_accounts_mode_token_still_yields_its_email() {
    let (verifier, _) = verifier(Duration::from_secs(3600));

    let token = TokenBuilder::valid()
        .remove("email")
        .remove("email_verified")
        .claim(
            "firebase.identities",
            json!({
                "google.com": ["100000000000000000001"],
                "email": ["marisol@example.test"]
            }),
        )
        .sign();

    let identity = verifier
        .verify(&token, Provider::Google)
        .await
        .expect("should verify");

    assert_eq!(identity.email.as_deref(), Some("marisol@example.test"));
    // No top-level claim means no verified claim either; the account is
    // created unverified and proves its address by the emailed code like
    // everyone else.
    assert!(!identity.email_verified);
}

/// Two addresses in the slot is not a shape to guess about.
///
/// With linking off it cannot normally happen; if it does, picking one would
/// attach an address nobody proved. No email means the sign-up is refused
/// with the message that says to use the email path — safe, and honest.
#[tokio::test]
async fn a_plural_identities_email_is_not_guessed_at() {
    let (verifier, _) = verifier(Duration::from_secs(3600));

    let token = TokenBuilder::valid()
        .remove("email")
        .remove("email_verified")
        .claim(
            "firebase.identities",
            json!({
                "google.com": ["100000000000000000001"],
                "email": ["one@example.test", "two@example.test"]
            }),
        )
        .sign();

    let identity = verifier
        .verify(&token, Provider::Google)
        .await
        .expect("the token itself is valid");

    assert_eq!(identity.email, None, "ambiguity must not become an address");
}
