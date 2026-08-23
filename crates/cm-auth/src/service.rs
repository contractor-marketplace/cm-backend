//! Registration, login, logout, session resolution and password change.
//!
//! Everything that decides whether someone is who they say they are lives in
//! this file. Handlers translate HTTP to these calls and back; they contain no
//! rules of their own.

use crate::cookie;
use crate::csrf;
use crate::firebase::{FirebaseVerifier, Mode as FirebaseMode};
use crate::hash;
use crate::password::PasswordHasherService;
use crate::ratelimit;
use crate::token;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use cm_core::{new_id, AppError, AuthConfig, Origin, Secret};
use cm_db::repo::audit::{ActorKind, AuditEvent};
use cm_db::repo::oauth::{self, Provider};
use cm_db::repo::sessions::RevocationReason;
use cm_db::repo::{audit, passwords, sessions, users};
use cm_db::PgPool;
use sqlx::PgConnection;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Only extend the idle window once a minute. Without this, every authenticated
/// request is a write.
const TOUCH_INTERVAL_SECS: i64 = 60;

/// What the transport knows about a request, for audit and session records.
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    /// Already resolved to a single address by the caller, which is also where
    /// the decision to believe `X-Forwarded-For` is made.
    pub client_ip: Option<String>,
    pub user_agent: Option<String>,
    pub request_id: Option<String>,
}

/// A newly created session. The raw token exists only here and in the
/// `Set-Cookie` the handler builds from it.
#[derive(Debug, Clone)]
pub struct IssuedSession {
    pub session_id: Uuid,
    pub token: String,
    pub csrf_token: String,
    pub absolute_expires_at: DateTime<Utc>,
    pub max_age: Duration,
}

/// A resolved, live session and the account behind it.
#[derive(Debug, Clone)]
pub struct Authenticated {
    pub session_id: Uuid,
    pub user: users::User,
    pub roles: Vec<users::Role>,
    pub csrf_token: String,
    pub session_expires_at: DateTime<Utc>,
}

impl Authenticated {
    pub fn has_role(&self, role: users::Role) -> bool {
        self.roles.contains(&role)
    }
}

/// What phase 1 of a login found, so the connection can be released before any
/// hashing happens.
enum Precheck {
    NoAccount,
    Inactive(Uuid),
    NoPassword(Uuid),
    Locked(Uuid),
    Proceed {
        user_id: Uuid,
        password_hash: String,
    },
}

/// Outcome of a successful password login.
#[derive(Debug, Clone)]
pub struct LoginOutcome {
    pub user: users::User,
    pub session: IssuedSession,
}

#[derive(Clone)]
pub struct AuthService {
    pepper: Secret<String>,
    hasher: PasswordHasherService,
    site_origin: Origin,
    session_idle: ChronoDuration,
    session_absolute: ChronoDuration,
    absolute_max_age: Duration,
    firebase: Option<Arc<FirebaseVerifier>>,
}

impl std::fmt::Debug for AuthService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthService")
            .field("site_origin", &self.site_origin)
            .finish_non_exhaustive()
    }
}

impl AuthService {
    pub fn new(config: &AuthConfig, site_origin: Origin) -> Result<Self, AppError> {
        let firebase = config
            .firebase
            .as_ref()
            .map(|firebase| {
                let mode = if firebase.emulator_host.is_some() {
                    tracing::warn!(
                        project_id = %firebase.project_id,
                        "Firebase emulator mode: sign-in tokens are NOT signature-checked"
                    );
                    FirebaseMode::Emulator
                } else {
                    let client = reqwest::Client::builder()
                        .timeout(Duration::from_secs(10))
                        .build()
                        .map_err(AppError::internal)?;
                    FirebaseMode::Signed(FirebaseVerifier::google_key_fetcher(client))
                };
                Ok::<_, AppError>(Arc::new(FirebaseVerifier::new(
                    firebase.project_id.clone(),
                    mode,
                )))
            })
            .transpose()?;

        Ok(Self {
            firebase,
            pepper: config.hash_pepper.clone(),
            hasher: PasswordHasherService::new(config.argon2_max_concurrency)?,
            site_origin,
            session_idle: ChronoDuration::from_std(config.session_idle)
                .map_err(AppError::internal)?,
            session_absolute: ChronoDuration::from_std(config.session_absolute)
                .map_err(AppError::internal)?,
            absolute_max_age: config.session_absolute,
        })
    }

    pub fn site_origin(&self) -> &Origin {
        &self.site_origin
    }

    /// The pepper, for the rate-limit policies that live outside this crate.
    /// Exposed deliberately narrowly: it keys digests, never encrypts anything.
    pub fn pepper(&self) -> &Secret<String> {
        &self.pepper
    }

    fn ip_hash(&self, context: &RequestContext) -> Option<Vec<u8>> {
        context
            .client_ip
            .as_deref()
            .map(|address| hash::ip(&self.pepper, address))
    }

    /// Create a session row and mint its token.
    async fn issue_session(
        &self,
        conn: &mut PgConnection,
        user_id: Uuid,
        context: &RequestContext,
        now: DateTime<Utc>,
    ) -> Result<IssuedSession, AppError> {
        let raw = token::generate()?;
        let token_hash = hash::digest_token(&raw);
        let session_id = new_id();

        let absolute_expires_at = now + self.session_absolute;
        // Never past the absolute ceiling, even if the idle window is longer.
        let idle_expires_at = (now + self.session_idle).min(absolute_expires_at);

        sessions::insert(
            conn,
            session_id,
            user_id,
            &token_hash,
            idle_expires_at,
            absolute_expires_at,
            self.ip_hash(context).as_deref(),
            context.user_agent.as_deref(),
        )
        .await?;

        Ok(IssuedSession {
            session_id,
            token: raw,
            csrf_token: csrf::token_for_session(&self.pepper, session_id),
            absolute_expires_at,
            max_age: self.absolute_max_age,
        })
    }

    fn event(&self, action: &'static str, context: &RequestContext) -> AuditEvent {
        AuditEvent::new(action, "users")
            .request_id(context.request_id.clone())
            .ip_hash(self.ip_hash(context))
    }

    /// Register a new account and sign it in.
    pub async fn register(
        &self,
        pool: &PgPool,
        email: &str,
        display_name: &str,
        password: &str,
        context: &RequestContext,
    ) -> Result<LoginOutcome, AppError> {
        let now = Utc::now();
        ratelimit::enforce(
            pool,
            &self.pepper,
            ratelimit::register_per_ip(),
            context.client_ip.as_deref().unwrap_or("unknown"),
            now,
        )
        .await?;

        let email = email.trim();
        let display_name = display_name.trim();
        if display_name.is_empty() {
            return Err(AppError::invalid("A display name is required."));
        }
        if !looks_like_email(email) {
            return Err(AppError::invalid("Enter a valid email address."));
        }
        PasswordHasherService::check_policy(password, email)?;

        // Hashed before the transaction opens: Argon2 takes tens of
        // milliseconds, and holding a transaction across it would pin a
        // connection for no reason.
        let password_hash = self.hasher.hash(password).await?;

        let mut tx = pool.begin().await.map_err(AppError::internal)?;
        let user = users::insert(&mut tx, new_id(), email, display_name).await?;
        passwords::insert(&mut tx, user.id, &password_hash).await?;
        let session = self.issue_session(&mut tx, user.id, context, now).await?;
        audit::record(
            &mut tx,
            self.event("auth.registered", context)
                .actor(ActorKind::User, Some(user.id))
                .subject(user.id)
                .data(serde_json::json!({ "session_id": session.session_id })),
        )
        .await?;
        tx.commit().await.map_err(AppError::internal)?;

        tracing::info!(user_id = %user.id, "account registered");
        Ok(LoginOutcome { user, session })
    }

    /// Authenticate with a password.
    ///
    /// Every failure returns the same error. The only way to tell the cases
    /// apart from outside is timing, and the decoy verification below removes
    /// the difference between "no such account" and "wrong password" — the two
    /// that would otherwise turn this endpoint into an account enumerator.
    ///
    /// Three phases, and the split is load-bearing:
    ///
    /// 1. Read the credential, then **release the connection**. Argon2 holds
    ///    ~19 MiB for tens of milliseconds and queues behind a semaphore; a
    ///    pooled connection held across it means a burst of logins exhausts the
    ///    pool and starves every other query, readiness checks included.
    /// 2. Verify, holding nothing.
    /// 3. Re-read under a row lock and revalidate before acting. Between
    ///    phases 1 and 3 the password can have been changed, the account
    ///    suspended, or the credential locked; acting on the phase-1 snapshot
    ///    would mint a session for a password the account no longer has.
    pub async fn login(
        &self,
        pool: &PgPool,
        email: &str,
        password: &str,
        context: &RequestContext,
    ) -> Result<LoginOutcome, AppError> {
        let now = Utc::now();
        ratelimit::enforce(
            pool,
            &self.pepper,
            ratelimit::login_per_ip(),
            context.client_ip.as_deref().unwrap_or("unknown"),
            now,
        )
        .await?;

        let precheck = self.login_precheck(pool, email, now).await?;

        let (user_id, verified_hash) = match precheck {
            Precheck::NoAccount => {
                self.hasher.verify_decoy(password).await?;
                self.audit_failed_login(pool, None, "unknown_account", context)
                    .await?;
                return Err(AppError::Unauthenticated);
            }
            Precheck::Inactive(user_id) => {
                self.hasher.verify_decoy(password).await?;
                self.audit_failed_login(pool, Some(user_id), "account_not_active", context)
                    .await?;
                return Err(AppError::Unauthenticated);
            }
            Precheck::NoPassword(user_id) => {
                self.hasher.verify_decoy(password).await?;
                self.audit_failed_login(pool, Some(user_id), "no_password_set", context)
                    .await?;
                return Err(AppError::Unauthenticated);
            }
            Precheck::Locked(user_id) => {
                // The hash is deliberately not verified while locked. Doing the
                // work anyway would let an attacker keep a locked account
                // burning 19 MiB and a core per guess — turning a lockout into
                // an amplification vector on a single shared box. The cost is
                // that a locked account answers faster than a wrong password
                // does; that only tells an attacker something about an account
                // they have already locked themselves.
                self.audit_failed_login(pool, Some(user_id), "account_locked", context)
                    .await?;
                return Err(AppError::Unauthenticated);
            }
            Precheck::Proceed {
                user_id,
                password_hash,
            } => (user_id, password_hash),
        };

        // Phase 2: nothing from the pool is held here.
        let verified = self.hasher.verify(password, &verified_hash).await?;

        if !verified {
            let mut conn = pool.acquire().await.map_err(AppError::internal)?;
            let after = passwords::record_failure(&mut conn, user_id).await?;
            self.record_failed_login(&mut conn, Some(user_id), "bad_password", context)
                .await?;

            if after.is_locked_at(Utc::now()) {
                audit::record(
                    &mut conn,
                    self.event("auth.account_locked", context)
                        .actor(ActorKind::System, Some(user_id))
                        .subject(user_id)
                        .data(serde_json::json!({
                            "failed_attempts": after.failed_attempts,
                            "locked_until": after.locked_until,
                        })),
                )
                .await?;
                tracing::warn!(user_id = %user_id, "account locked after repeated failures");
            }

            return Err(AppError::Unauthenticated);
        }

        // Phase 3: revalidate under the row lock, then act.
        let mut tx = pool.begin().await.map_err(AppError::internal)?;
        let user = self
            .revalidate(&mut tx, user_id, &verified_hash, Utc::now())
            .await?;

        passwords::clear_failures(&mut tx, user_id).await?;
        let session = self.issue_session(&mut tx, user_id, context, now).await?;
        audit::record(
            &mut tx,
            self.event("auth.login_succeeded", context)
                .actor(ActorKind::User, Some(user_id))
                .subject(user_id)
                .data(serde_json::json!({ "session_id": session.session_id })),
        )
        .await?;
        tx.commit().await.map_err(AppError::internal)?;

        // After the session exists, and outside any transaction: cost
        // parameters move over time, and a correct password is the only chance
        // to upgrade a stored hash without asking the person for it again.
        self.upgrade_hash_if_needed(pool, user_id, &verified_hash, password)
            .await?;

        Ok(LoginOutcome { user, session })
    }

    /// Phase 1: everything that needs the database, and nothing that does not.
    async fn login_precheck(
        &self,
        pool: &PgPool,
        email: &str,
        now: DateTime<Utc>,
    ) -> Result<Precheck, AppError> {
        let mut conn = pool.acquire().await.map_err(AppError::internal)?;

        let Some(user) = users::find_by_email(&mut conn, email.trim()).await? else {
            return Ok(Precheck::NoAccount);
        };
        if !user.status.can_authenticate() {
            return Ok(Precheck::Inactive(user.id));
        }
        let Some(credential) = passwords::find(&mut conn, user.id).await? else {
            return Ok(Precheck::NoPassword(user.id));
        };
        if credential.is_locked_at(now) {
            return Ok(Precheck::Locked(user.id));
        }

        // An elapsed lock is cleared before the attempt, so the counter starts
        // again rather than locking the account on its first new mistake.
        if credential.locked_until.is_some() {
            passwords::clear_failures(&mut conn, user.id).await?;
        }

        Ok(Precheck::Proceed {
            user_id: user.id,
            password_hash: credential.password_hash,
        })
    }

    /// Confirm, under a row lock, that the world still matches what was
    /// verified. Returns the account as it stands now.
    async fn revalidate(
        &self,
        tx: &mut PgConnection,
        user_id: Uuid,
        verified_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<users::User, AppError> {
        let Some(current) = passwords::find_for_update(tx, user_id).await? else {
            return Err(AppError::Unauthenticated);
        };
        let Some(user) = users::find_by_id(tx, user_id).await? else {
            return Err(AppError::Unauthenticated);
        };

        if !user.status.can_authenticate() {
            tracing::warn!(%user_id, "account stopped being active mid-authentication");
            return Err(AppError::Unauthenticated);
        }
        if current.password_hash != verified_hash {
            // The password was changed between the read and the act. The
            // credential presented is no longer this account's password.
            tracing::warn!(%user_id, "password changed mid-authentication; refusing");
            return Err(AppError::Unauthenticated);
        }
        if current.is_locked_at(now) {
            tracing::warn!(%user_id, "account locked mid-authentication; refusing");
            return Err(AppError::Unauthenticated);
        }

        Ok(user)
    }

    /// Replace a stored hash whose parameters are below the current ones.
    ///
    /// Hashed outside any connection, then swapped in with a compare-and-swap,
    /// so an upgrade that races a real password change loses harmlessly.
    async fn upgrade_hash_if_needed(
        &self,
        pool: &PgPool,
        user_id: Uuid,
        verified_hash: &str,
        password: &str,
    ) -> Result<(), AppError> {
        if !PasswordHasherService::needs_rehash(verified_hash) {
            return Ok(());
        }

        let upgraded = self.hasher.hash(password).await?;
        let mut conn = pool.acquire().await.map_err(AppError::internal)?;
        if passwords::upgrade_hash(&mut conn, user_id, verified_hash, &upgraded).await? {
            tracing::info!(%user_id, "password hash upgraded to current parameters");
        }

        Ok(())
    }

    /// Record a failed attempt on a connection of its own.
    async fn audit_failed_login(
        &self,
        pool: &PgPool,
        user_id: Option<Uuid>,
        reason: &'static str,
        context: &RequestContext,
    ) -> Result<(), AppError> {
        let mut conn = pool.acquire().await.map_err(AppError::internal)?;
        self.record_failed_login(&mut conn, user_id, reason, context)
            .await
    }

    /// Write the failure event.
    ///
    /// The attempted address is recorded only when it belongs to a real
    /// account, which `subject_id` already identifies. Logging the raw string
    /// for unknown addresses would fill the audit table with the email
    /// addresses of people who are not users.
    async fn record_failed_login(
        &self,
        conn: &mut PgConnection,
        user_id: Option<Uuid>,
        reason: &'static str,
        context: &RequestContext,
    ) -> Result<(), AppError> {
        let mut event = self
            .event("auth.login_failed", context)
            .actor(ActorKind::User, user_id)
            .data(serde_json::json!({ "reason": reason }));
        if let Some(user_id) = user_id {
            event = event.subject(user_id);
        }

        audit::record(conn, event).await?;
        Ok(())
    }

    /// Resolve a raw session token to a live session.
    pub async fn authenticate(
        &self,
        pool: &PgPool,
        raw_token: &str,
    ) -> Result<Authenticated, AppError> {
        let token_hash = hash::digest_token(raw_token);
        let mut conn = pool.acquire().await.map_err(AppError::internal)?;

        let Some(session) = sessions::find_live(&mut conn, &token_hash).await? else {
            return Err(AppError::Unauthenticated);
        };

        let Some(user) = users::find_by_id(&mut conn, session.user_id).await? else {
            // The foreign key makes this unreachable; treated as a failure
            // rather than an unwrap so it can never become one.
            return Err(AppError::Unauthenticated);
        };

        // A suspended account's existing sessions stop working immediately,
        // without needing the suspension to hunt them down and revoke them.
        if !user.status.can_authenticate() {
            return Err(AppError::Unauthenticated);
        }

        let roles = users::roles(&mut conn, user.id).await?;

        let idle_expires_at = Utc::now() + self.session_idle;
        sessions::touch(&mut conn, session.id, idle_expires_at, TOUCH_INTERVAL_SECS).await?;

        Ok(Authenticated {
            csrf_token: csrf::token_for_session(&self.pepper, session.id),
            session_id: session.id,
            session_expires_at: session.absolute_expires_at,
            user,
            roles,
        })
    }

    /// Verify the CSRF token for a state-changing request.
    pub fn verify_csrf(
        &self,
        session_id: Uuid,
        presented: Option<&str>,
        origin_header: Option<&str>,
    ) -> Result<(), AppError> {
        csrf::verify(
            &self.pepper,
            &self.site_origin,
            session_id,
            presented,
            origin_header,
        )
    }

    /// End the calling session.
    pub async fn logout(
        &self,
        pool: &PgPool,
        session_id: Uuid,
        user_id: Uuid,
        context: &RequestContext,
    ) -> Result<(), AppError> {
        let mut tx = pool.begin().await.map_err(AppError::internal)?;
        sessions::revoke(&mut tx, session_id, RevocationReason::Logout).await?;
        audit::record(
            &mut tx,
            self.event("auth.logged_out", context)
                .actor(ActorKind::User, Some(user_id))
                .subject(user_id)
                .data(serde_json::json!({ "session_id": session_id })),
        )
        .await?;
        tx.commit().await.map_err(AppError::internal)?;

        Ok(())
    }

    /// End every session for the account, including the calling one.
    pub async fn logout_all(
        &self,
        pool: &PgPool,
        user_id: Uuid,
        context: &RequestContext,
    ) -> Result<u64, AppError> {
        let mut tx = pool.begin().await.map_err(AppError::internal)?;
        let revoked =
            sessions::revoke_all_for_user(&mut tx, user_id, RevocationReason::LogoutAll, None)
                .await?;
        audit::record(
            &mut tx,
            self.event("auth.logged_out_all", context)
                .actor(ActorKind::User, Some(user_id))
                .subject(user_id)
                .data(serde_json::json!({ "sessions_revoked": revoked })),
        )
        .await?;
        tx.commit().await.map_err(AppError::internal)?;

        Ok(revoked)
    }

    /// Change a password.
    ///
    /// Every other session is revoked, and the calling one is rotated rather
    /// than kept: if the reason for the change is that the old password leaked,
    /// leaving the attacker's session — or the victim's own session id — alive
    /// defeats the point of changing it.
    ///
    /// Phased like `login`, for the same two reasons: no pooled connection is
    /// held across either the verification or the new hash, and the swap is a
    /// compare-and-swap under a row lock, so two changes that both verified the
    /// same old password cannot both succeed.
    pub async fn change_password(
        &self,
        pool: &PgPool,
        user_id: Uuid,
        current_session_id: Uuid,
        current_password: &str,
        new_password: &str,
        context: &RequestContext,
    ) -> Result<IssuedSession, AppError> {
        let now = Utc::now();
        ratelimit::enforce(
            pool,
            &self.pepper,
            ratelimit::password_change_per_user(),
            &user_id.to_string(),
            now,
        )
        .await?;

        // Phase 1: read, then release.
        let (email, verified_hash) = {
            let mut conn = pool.acquire().await.map_err(AppError::internal)?;
            let Some(user) = users::find_by_id(&mut conn, user_id).await? else {
                return Err(AppError::Unauthenticated);
            };
            let Some(credential) = passwords::find(&mut conn, user_id).await? else {
                return Err(AppError::invalid("This account has no password set."));
            };
            (user.email, credential.password_hash)
        };

        // Phase 2: nothing from the pool is held here.
        if !self.hasher.verify(current_password, &verified_hash).await? {
            let mut conn = pool.acquire().await.map_err(AppError::internal)?;
            audit::record(
                &mut conn,
                self.event("auth.password_change_failed", context)
                    .actor(ActorKind::User, Some(user_id))
                    .subject(user_id)
                    .data(serde_json::json!({ "reason": "bad_current_password" })),
            )
            .await?;
            return Err(AppError::invalid("Your current password is not correct."));
        }

        PasswordHasherService::check_policy(new_password, &email)?;
        if current_password == new_password {
            return Err(AppError::invalid(
                "The new password must be different from the current one.",
            ));
        }

        // Also outside any connection: this is the expensive half.
        let new_hash = self.hasher.hash(new_password).await?;

        // Phase 3: revalidate under the row lock, then swap.
        let mut tx = pool.begin().await.map_err(AppError::internal)?;
        self.revalidate(&mut tx, user_id, &verified_hash, Utc::now())
            .await
            .map_err(|error| match error {
                // The caller is already authenticated, so naming the cause
                // discloses nothing and saves them guessing.
                AppError::Unauthenticated => AppError::conflict(
                    "This account changed while the request was in flight. Try again.",
                ),
                other => other,
            })?;

        if !passwords::replace_hash(&mut tx, user_id, &verified_hash, &new_hash).await? {
            tx.rollback().await.map_err(AppError::internal)?;
            return Err(AppError::conflict(
                "This account changed while the request was in flight. Try again.",
            ));
        }

        let revoked = sessions::revoke_all_for_user(
            &mut tx,
            user_id,
            RevocationReason::PasswordChange,
            Some(current_session_id),
        )
        .await?;
        sessions::revoke(&mut tx, current_session_id, RevocationReason::Rotation).await?;
        let session = self.issue_session(&mut tx, user_id, context, now).await?;
        audit::record(
            &mut tx,
            self.event("auth.password_changed", context)
                .actor(ActorKind::User, Some(user_id))
                .subject(user_id)
                .data(serde_json::json!({
                    "other_sessions_revoked": revoked,
                    "rotated_from": current_session_id,
                    "rotated_to": session.session_id,
                })),
        )
        .await?;
        tx.commit().await.map_err(AppError::internal)?;

        tracing::info!(%user_id, other_sessions_revoked = revoked, "password changed");
        Ok(session)
    }

    /// Replace the Firebase verifier. Tests use this to inject a local key
    /// pair; nothing in the running service calls it.
    pub fn with_firebase(mut self, verifier: Arc<FirebaseVerifier>) -> Self {
        self.firebase = Some(verifier);
        self
    }

    fn firebase(&self) -> Result<&FirebaseVerifier, AppError> {
        self.firebase
            .as_deref()
            .ok_or_else(|| AppError::unavailable("Google sign-in is not configured."))
    }

    /// Sign in with a Firebase-issued Google token.
    ///
    /// The account is resolved by `(provider, subject)` and by nothing else. If
    /// no identity matches, a **new** account is created — the address on the
    /// token is never used to find an existing one, however verified it claims
    /// to be. That rule is the whole defence: an attacker who can obtain a
    /// Google account bearing someone's address must not thereby obtain their
    /// account here.
    ///
    /// The consequence is deliberate and visible to users: someone who
    /// registered with a password and then clicks "Sign in with Google" gets a
    /// second account. Joining them is `link_google`, which requires being
    /// signed in to the first one.
    pub async fn sign_in_with_google(
        &self,
        pool: &PgPool,
        id_token: &str,
        context: &RequestContext,
    ) -> Result<LoginOutcome, AppError> {
        let now = Utc::now();
        ratelimit::enforce(
            pool,
            &self.pepper,
            ratelimit::federated_sign_in_per_ip(),
            context.client_ip.as_deref().unwrap_or("unknown"),
            now,
        )
        .await?;

        let identity = self.firebase()?.verify(id_token).await?;

        let mut tx = pool.begin().await.map_err(AppError::internal)?;

        let existing =
            oauth::find_by_subject(&mut tx, Provider::Google, &identity.provider_subject).await?;

        let (user, action) = match existing {
            Some(link) => {
                let Some(user) = users::find_by_id(&mut tx, link.user_id).await? else {
                    return Err(AppError::Unauthenticated);
                };
                if !user.status.can_authenticate() {
                    return Err(AppError::Unauthenticated);
                }
                oauth::touch_login(&mut tx, link.id).await?;
                (user, "auth.google_login_succeeded")
            }
            None => {
                // A fresh account. The address is stored as this account's
                // email; if it collides with an existing account the insert
                // fails, and the caller is told to sign in and link instead.
                let email = identity.email.clone().ok_or_else(|| {
                    AppError::invalid("That Google account has no email address.")
                })?;
                let display_name = email.split('@').next().unwrap_or("New user").to_owned();

                let user = users::insert(&mut tx, new_id(), email.trim(), &display_name)
                    .await
                    .map_err(|error| match error {
                        AppError::Conflict { .. } => AppError::conflict(
                            "An account already uses that email address. Sign in to it, \
                             then link Google from account settings.",
                        ),
                        other => other,
                    })?;

                oauth::insert(
                    &mut tx,
                    new_id(),
                    user.id,
                    Provider::Google,
                    &identity.provider_subject,
                    Some(&identity.firebase_uid),
                    identity.email.as_deref(),
                    identity.email_verified,
                )
                .await?;

                (user, "auth.google_registered")
            }
        };

        let session = self.issue_session(&mut tx, user.id, context, now).await?;
        audit::record(
            &mut tx,
            self.event(action, context)
                .actor(ActorKind::User, Some(user.id))
                .subject(user.id)
                .data(serde_json::json!({ "session_id": session.session_id })),
        )
        .await?;
        tx.commit().await.map_err(AppError::internal)?;

        Ok(LoginOutcome { user, session })
    }

    /// Attach a Google identity to the account that is already signed in.
    ///
    /// Linking is only reachable from an authenticated session, so control of
    /// both identities is proved before they are joined.
    pub async fn link_google(
        &self,
        pool: &PgPool,
        user_id: Uuid,
        id_token: &str,
        context: &RequestContext,
    ) -> Result<(), AppError> {
        ratelimit::enforce(
            pool,
            &self.pepper,
            ratelimit::link_identity_per_user(),
            &user_id.to_string(),
            Utc::now(),
        )
        .await?;

        let identity = self.firebase()?.verify(id_token).await?;

        let mut tx = pool.begin().await.map_err(AppError::internal)?;

        if oauth::exists_for_user(&mut tx, user_id, Provider::Google).await? {
            return Err(AppError::conflict(
                "This account already has a Google identity linked.",
            ));
        }
        if oauth::find_by_subject(&mut tx, Provider::Google, &identity.provider_subject)
            .await?
            .is_some()
        {
            return Err(AppError::conflict(
                "That Google account is already linked to another account.",
            ));
        }

        oauth::insert(
            &mut tx,
            new_id(),
            user_id,
            Provider::Google,
            &identity.provider_subject,
            Some(&identity.firebase_uid),
            identity.email.as_deref(),
            identity.email_verified,
        )
        .await?;

        audit::record(
            &mut tx,
            self.event("auth.identity_linked", context)
                .actor(ActorKind::User, Some(user_id))
                .subject(user_id)
                .data(serde_json::json!({ "provider": "google" })),
        )
        .await?;
        tx.commit().await.map_err(AppError::internal)?;

        Ok(())
    }

    /// The `Set-Cookie` pair for a newly issued session.
    pub fn session_cookies(&self, session: &IssuedSession) -> [String; 2] {
        [
            cookie::session(&session.token, session.max_age),
            cookie::csrf(&session.csrf_token, session.max_age),
        ]
    }
}

/// A deliberately loose check, matching the database CHECK constraint. Real
/// validation of an address is delivery, which is a later milestone.
fn looks_like_email(value: &str) -> bool {
    if value.len() > 254 || value.chars().any(char::is_whitespace) {
        return false;
    }
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };

    !local.is_empty() && !domain.is_empty() && domain.contains('.') && !domain.contains('@')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_shape_matches_the_database_constraint() {
        for good in ["marisol@example.com", "a.b+tag@sub.example.co.uk", "x@y.z"] {
            assert!(looks_like_email(good), "{good} should be accepted");
        }
        for bad in [
            "",
            "no-at-sign",
            "@example.com",
            "user@",
            "user@nodot",
            "two@at@example.com",
            "spa ce@example.com",
            "trailing@example.com ",
        ] {
            assert!(!looks_like_email(bad), "{bad} should be rejected");
        }
    }
}
