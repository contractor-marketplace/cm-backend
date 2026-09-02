//! Registration, login, logout, session resolution and password change.
//!
//! Everything that decides whether someone is who they say they are lives in
//! this file. Handlers translate HTTP to these calls and back; they contain no
//! rules of their own.

use crate::cookie;
use crate::csrf;
use crate::firebase::{FirebaseVerifier, Mode as FirebaseMode};
use crate::hash;
use crate::login_code;
use crate::mail;
use crate::password::PasswordHasherService;
use crate::ratelimit;
use crate::token;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use cm_core::{new_id, AppError, AuthConfig, Origin, Secret};
use cm_db::repo::audit::{ActorKind, AuditEvent};
use cm_db::repo::auth_tokens::{self, CodeOutcome, Purpose};
use cm_db::repo::email_outbox::{self, Kind as MailKind, NewEmail};
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

/// A sign-in that is waiting on the emailed code.
#[derive(Debug, Clone)]
pub struct Challenge {
    pub challenge_id: Uuid,
    /// Echoed so the UI can say where the code went.
    pub email: String,
}

/// What presenting a correct password produced: a session outright, when the
/// browser is remembered, or a code challenge when it is not.
#[derive(Debug, Clone)]
pub enum LoginResult {
    Session(LoginOutcome),
    Challenged(Challenge),
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

    /// Register a new account and email it a sign-in code.
    ///
    /// No session is created here. The account exists after this returns, but
    /// signing in — this first time and every time from an unremembered
    /// browser — goes through `verify_login_code`, which is also what marks
    /// the address verified: proving the inbox is not an extra step after
    /// registration, it *is* the last step of registration.
    pub async fn register(
        &self,
        pool: &PgPool,
        email: &str,
        display_name: &str,
        password: &str,
        account_type: users::AccountType,
        context: &RequestContext,
    ) -> Result<Challenge, AppError> {
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

        // Account, credential, code and email in one transaction: a crash
        // anywhere leaves either a complete challenge or no account, never an
        // account whose code was lost.
        let mut tx = pool.begin().await.map_err(AppError::internal)?;
        let user = users::insert(&mut tx, new_id(), email, display_name, account_type).await?;
        passwords::insert(&mut tx, user.id, &password_hash).await?;
        let challenge = self.issue_challenge(&mut tx, user.id, email).await?;
        audit::record(
            &mut tx,
            self.event("auth.registered", context)
                .actor(ActorKind::User, Some(user.id))
                .subject(user.id)
                .data(serde_json::json!({ "challenge_id": challenge.challenge_id })),
        )
        .await?;
        tx.commit().await.map_err(AppError::internal)?;

        tracing::info!(user_id = %user.id, "account registered, sign-in code queued");
        Ok(challenge)
    }

    /// Create a code challenge for an account and queue its email.
    ///
    /// Runs inside the caller's transaction, so the token row and the outbox
    /// row commit or vanish together.
    async fn issue_challenge(
        &self,
        conn: &mut PgConnection,
        user_id: Uuid,
        email: &str,
    ) -> Result<Challenge, AppError> {
        let challenge_id = new_id();
        let code = login_code::generate_code()?;
        auth_tokens::issue(
            conn,
            challenge_id,
            user_id,
            Purpose::LoginCode,
            &login_code::code_hash(&self.pepper, challenge_id, &code),
            login_code::CODE_TTL_SECS,
        )
        .await?;

        let rendered = mail::login_code(&code);
        email_outbox::enqueue(
            conn,
            &NewEmail {
                user_id,
                recipient: email.to_owned(),
                kind: MailKind::LoginCode,
                subject: rendered.subject,
                body_text: rendered.text,
                body_html: Some(rendered.html),
                unsubscribe_url: None,
            },
        )
        .await?;

        Ok(Challenge {
            challenge_id,
            email: email.to_owned(),
        })
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
    ///
    /// `device` is the raw remembered-device cookie, if the browser sent one.
    /// A valid one skips the emailed code; it never skips the password.
    pub async fn login(
        &self,
        pool: &PgPool,
        email: &str,
        password: &str,
        device: Option<&str>,
        context: &RequestContext,
    ) -> Result<LoginResult, AppError> {
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

        // The password is right. Whether it becomes a session now or a code
        // challenge depends on the browser: a valid device cookie for this
        // account says a code was completed here before.
        let remembered = device
            .map(|value| login_code::device_remembers(&self.pepper, value, user_id, now))
            .unwrap_or(false);

        if !remembered {
            // Every challenge is an email; bounded per account, and before the
            // transaction so the counter never rides a rollback.
            ratelimit::enforce(
                pool,
                &self.pepper,
                ratelimit::login_code_issue_per_user(),
                &user_id.to_string(),
                now,
            )
            .await?;
        }

        // Phase 3: revalidate under the row lock, then act.
        let mut tx = pool.begin().await.map_err(AppError::internal)?;
        let user = self
            .revalidate(&mut tx, user_id, &verified_hash, Utc::now())
            .await?;

        passwords::clear_failures(&mut tx, user_id).await?;

        let result = if remembered {
            let session = self.issue_session(&mut tx, user_id, context, now).await?;
            audit::record(
                &mut tx,
                self.event("auth.login_succeeded", context)
                    .actor(ActorKind::User, Some(user_id))
                    .subject(user_id)
                    .data(serde_json::json!({ "session_id": session.session_id })),
            )
            .await?;
            LoginResult::Session(LoginOutcome { user, session })
        } else {
            let challenge = self.issue_challenge(&mut tx, user_id, &user.email).await?;
            audit::record(
                &mut tx,
                self.event("auth.login_challenged", context)
                    .actor(ActorKind::User, Some(user_id))
                    .subject(user_id)
                    .data(serde_json::json!({ "challenge_id": challenge.challenge_id })),
            )
            .await?;
            LoginResult::Challenged(challenge)
        };
        tx.commit().await.map_err(AppError::internal)?;

        // After the outcome exists, and outside any transaction: cost
        // parameters move over time, and a correct password is the only chance
        // to upgrade a stored hash without asking the person for it again.
        self.upgrade_hash_if_needed(pool, user_id, &verified_hash, password)
            .await?;

        Ok(result)
    }

    /// Exchange a challenge and its emailed code for a session.
    ///
    /// The attempt is spent on its own connection, not in the transaction that
    /// acts on success — a failed guess must count even though nothing else
    /// happens, and the atomic UPDATE in the repo is what caps racing guesses.
    ///
    /// Completing a code is also what verifies the address: it is the same
    /// proof of inbox control a verification link would be.
    pub async fn verify_login_code(
        &self,
        pool: &PgPool,
        challenge_id: Uuid,
        code: &str,
        context: &RequestContext,
    ) -> Result<(LoginOutcome, String), AppError> {
        let now = Utc::now();
        ratelimit::enforce(
            pool,
            &self.pepper,
            ratelimit::login_code_verify_per_challenge(),
            &challenge_id.to_string(),
            now,
        )
        .await?;

        let outcome = {
            let mut conn = pool.acquire().await.map_err(AppError::internal)?;
            auth_tokens::verify_code(
                &mut conn,
                challenge_id,
                &login_code::code_hash(&self.pepper, challenge_id, code.trim()),
                login_code::MAX_CODE_ATTEMPTS,
            )
            .await?
        };

        let user_id = match outcome {
            CodeOutcome::Matched { user_id } => user_id,
            CodeOutcome::Wrong | CodeOutcome::Gone => {
                let mut conn = pool.acquire().await.map_err(AppError::internal)?;
                audit::record(
                    &mut conn,
                    self.event("auth.login_code_failed", context)
                        .actor(ActorKind::User, None)
                        .data(serde_json::json!({ "challenge_id": challenge_id })),
                )
                .await?;
                // One message for wrong, expired, consumed and never-existed:
                // anything finer would let a caller probe which challenges are
                // live.
                return Err(AppError::invalid("That code is incorrect or has expired."));
            }
        };

        let mut tx = pool.begin().await.map_err(AppError::internal)?;
        let Some(user) = users::find_by_id(&mut tx, user_id).await? else {
            return Err(AppError::Unauthenticated);
        };
        if !user.status.can_authenticate() {
            return Err(AppError::Unauthenticated);
        }

        // The proof of inbox control this table was waiting for since 0005.
        // The account is re-read afterwards so the outcome carries the
        // verified state it just gained.
        users::mark_email_verified(&mut tx, user_id).await?;
        let Some(user) = users::find_by_id(&mut tx, user_id).await? else {
            return Err(AppError::Unauthenticated);
        };
        let session = self.issue_session(&mut tx, user_id, context, now).await?;
        audit::record(
            &mut tx,
            self.event("auth.login_succeeded", context)
                .actor(ActorKind::User, Some(user_id))
                .subject(user_id)
                .data(serde_json::json!({
                    "session_id": session.session_id,
                    "via": "login_code",
                })),
        )
        .await?;
        tx.commit().await.map_err(AppError::internal)?;

        let device = login_code::device_value(
            &self.pepper,
            user_id,
            now + ChronoDuration::from_std(login_code::DEVICE_TTL).map_err(AppError::internal)?,
        );

        Ok((LoginOutcome { user, session }, device))
    }

    /// Re-send a challenge's code, as a fresh challenge.
    ///
    /// The old challenge is consumed by the new issue, so the response carries
    /// a new id for the client to hold. An unknown or dead challenge gets the
    /// same message as a wrong code, for the same reason.
    pub async fn resend_login_code(
        &self,
        pool: &PgPool,
        challenge_id: Uuid,
        context: &RequestContext,
    ) -> Result<Challenge, AppError> {
        let now = Utc::now();

        let user = {
            let mut conn = pool.acquire().await.map_err(AppError::internal)?;
            let Some(user_id) = auth_tokens::challenge_user(&mut conn, challenge_id).await? else {
                return Err(AppError::invalid("That code is incorrect or has expired."));
            };
            let Some(user) = users::find_by_id(&mut conn, user_id).await? else {
                return Err(AppError::invalid("That code is incorrect or has expired."));
            };
            user
        };

        ratelimit::enforce(
            pool,
            &self.pepper,
            ratelimit::login_code_issue_per_user(),
            &user.id.to_string(),
            now,
        )
        .await?;

        let mut tx = pool.begin().await.map_err(AppError::internal)?;
        let challenge = self.issue_challenge(&mut tx, user.id, &user.email).await?;
        audit::record(
            &mut tx,
            self.event("auth.login_code_resent", context)
                .actor(ActorKind::User, Some(user.id))
                .subject(user.id)
                .data(serde_json::json!({ "challenge_id": challenge.challenge_id })),
        )
        .await?;
        tx.commit().await.map_err(AppError::internal)?;

        Ok(challenge)
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

    /// How long a reset link lives.
    const RESET_TTL_SECS: i64 = 3600;

    /// Ask for a password-reset link.
    ///
    /// Returns `Ok(())` for every well-formed address, found or not — the
    /// endpoint must not be an account enumerator. The found path does no
    /// Argon2 work, only a token issue and two inserts, so the timing skew
    /// between the branches is negligible and is not padded.
    pub async fn request_password_reset(
        &self,
        pool: &PgPool,
        email: &str,
        context: &RequestContext,
    ) -> Result<(), AppError> {
        let now = Utc::now();
        ratelimit::enforce(
            pool,
            &self.pepper,
            ratelimit::password_reset_request_per_ip(),
            context.client_ip.as_deref().unwrap_or("unknown"),
            now,
        )
        .await?;

        let email = email.trim();
        if !looks_like_email(email) {
            return Err(AppError::invalid("Enter a valid email address."));
        }

        // Bounded per target as well as per caller, so many addresses cannot
        // take turns flooding one victim's inbox.
        ratelimit::enforce(
            pool,
            &self.pepper,
            ratelimit::password_reset_request_per_email(),
            &email.to_lowercase(),
            now,
        )
        .await?;

        let user = {
            let mut conn = pool.acquire().await.map_err(AppError::internal)?;
            users::find_by_email(&mut conn, email).await?
        };
        let Some(user) = user else {
            // Indistinguishable from success, deliberately.
            return Ok(());
        };
        if !user.status.can_authenticate() {
            return Ok(());
        }

        // Token and email in one transaction: the link in the mail always
        // matches a digest in the table.
        let raw = token::generate()?;
        let link = format!("{}/reset-password?token={raw}", self.site_origin.as_str());

        let mut tx = pool.begin().await.map_err(AppError::internal)?;
        auth_tokens::issue(
            &mut tx,
            new_id(),
            user.id,
            Purpose::PasswordReset,
            &hash::digest_token(&raw),
            Self::RESET_TTL_SECS,
        )
        .await?;
        let rendered = mail::password_reset(&link);
        email_outbox::enqueue(
            &mut tx,
            &NewEmail {
                user_id: user.id,
                recipient: user.email.clone(),
                kind: MailKind::PasswordReset,
                subject: rendered.subject,
                body_text: rendered.text,
                body_html: Some(rendered.html),
                unsubscribe_url: None,
            },
        )
        .await?;
        audit::record(
            &mut tx,
            self.event("auth.password_reset_requested", context)
                .actor(ActorKind::User, Some(user.id))
                .subject(user.id),
        )
        .await?;
        tx.commit().await.map_err(AppError::internal)?;

        Ok(())
    }

    /// Complete a reset: the emailed link plus a new password.
    ///
    /// The token is peeked before it is consumed, so a weak new password
    /// leaves the link alive; the consume itself is a single atomic UPDATE, so
    /// racing confirmations spend it exactly once. Every session is revoked —
    /// the reason for a reset is usually that the old credential leaked — and
    /// the address is marked verified, because a completed reset is the same
    /// proof of inbox control a code is.
    pub async fn confirm_password_reset(
        &self,
        pool: &PgPool,
        token: &str,
        new_password: &str,
        context: &RequestContext,
    ) -> Result<(), AppError> {
        const BAD_LINK: &str = "That reset link is invalid or has expired.";
        let digest = hash::digest_token(token.trim());

        let user = {
            let mut conn = pool.acquire().await.map_err(AppError::internal)?;
            let Some(user_id) =
                auth_tokens::peek_link(&mut conn, &digest, Purpose::PasswordReset).await?
            else {
                return Err(AppError::invalid(BAD_LINK));
            };
            let Some(user) = users::find_by_id(&mut conn, user_id).await? else {
                return Err(AppError::invalid(BAD_LINK));
            };
            user
        };
        if !user.status.can_authenticate() {
            return Err(AppError::invalid(BAD_LINK));
        }

        PasswordHasherService::check_policy(new_password, &user.email)?;

        // The expensive half, outside any connection.
        let new_hash = self.hasher.hash(new_password).await?;

        let mut tx = pool.begin().await.map_err(AppError::internal)?;
        // Consumed inside the transaction that acts on it: a race spends the
        // token once, and the loser is told the link is gone.
        if auth_tokens::consume_link(&mut tx, &digest, Purpose::PasswordReset)
            .await?
            .is_none()
        {
            return Err(AppError::invalid(BAD_LINK));
        }

        passwords::set_hash(&mut tx, user.id, &new_hash).await?;
        let revoked =
            sessions::revoke_all_for_user(&mut tx, user.id, RevocationReason::PasswordChange, None)
                .await?;
        users::mark_email_verified(&mut tx, user.id).await?;
        audit::record(
            &mut tx,
            self.event("auth.password_reset_completed", context)
                .actor(ActorKind::User, Some(user.id))
                .subject(user.id)
                .data(serde_json::json!({ "sessions_revoked": revoked })),
        )
        .await?;
        tx.commit().await.map_err(AppError::internal)?;

        tracing::info!(user_id = %user.id, sessions_revoked = revoked, "password reset completed");
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
            .ok_or_else(|| AppError::unavailable("Federated sign-in is not configured."))
    }

    /// Sign in with a Firebase-issued token from `provider`.
    ///
    /// The account is resolved by `(provider, subject)` and by nothing else. If
    /// no identity matches, a **new** account is created — the address on the
    /// token is never used to find an existing one, however verified it claims
    /// to be. That rule is the whole defence: an attacker who can obtain a
    /// Google or Facebook account bearing someone's address must not thereby
    /// obtain their account here.
    ///
    /// The consequence is deliberate and visible to users: someone who
    /// registered with a password and then clicks "Continue with Google" gets a
    /// second account. Joining them is `link_provider`, which requires being
    /// signed in to the first one.
    /// Sign in with a federated provider, registering on first arrival.
    ///
    /// `intended_account_type` is consulted ONLY when this call creates an
    /// account. An identity that already resolves to a user ignores it
    /// entirely, so the field cannot be used to flip an existing account's
    /// side of the marketplace — which is not a thing this product allows at
    /// all, by any route.
    pub async fn sign_in_with_provider(
        &self,
        pool: &PgPool,
        provider: Provider,
        id_token: &str,
        intended_account_type: Option<users::AccountType>,
        client_email: Option<&str>,
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

        let identity = self.firebase()?.verify(id_token, provider).await?;

        let mut tx = pool.begin().await.map_err(AppError::internal)?;

        let existing =
            oauth::find_by_subject(&mut tx, provider, &identity.provider_subject).await?;

        let (user, action) = match existing {
            Some(link) => {
                let Some(user) = users::find_by_id(&mut tx, link.user_id).await? else {
                    return Err(AppError::Unauthenticated);
                };
                if !user.status.can_authenticate() {
                    return Err(AppError::Unauthenticated);
                }
                oauth::touch_login(&mut tx, link.id).await?;
                (user, "auth.federated_login_succeeded")
            }
            None => {
                // A fresh account. The address is stored as this account's
                // email; if it collides with an existing account the insert
                // fails, and the caller is told to sign in to that account.
                //
                // Where the address comes from is a chain, most-proved first.
                // The token's own claim wins — but the console mode this
                // product requires (account linking off) strips that claim
                // from OAuth tokens, so the browser also forwards the address
                // the popup showed. That client copy is exactly as trustworthy
                // as one typed into the email form, which is to say: contact
                // information until the emailed code proves it, never before.
                // The verified fast-path below stays keyed to the token's own
                // claim, so a client-sourced address cannot verify itself.
                //
                // Facebook makes the fully-absent case real rather than
                // theoretical: an account created from a phone number, or one
                // that declined the email permission, has no address anywhere.
                // There is nowhere to put such a user yet — `users.email` is
                // NOT NULL — so they are turned away with a message that says
                // what to do.
                let client_email = client_email
                    .map(str::trim)
                    .filter(|value| looks_like_email(value));
                let email_source = if identity.email.is_some() {
                    if identity.email_from_identities {
                        "identities"
                    } else {
                        "token"
                    }
                } else if client_email.is_some() {
                    "client"
                } else {
                    "none"
                };
                let email = identity
                    .email
                    .clone()
                    .or_else(|| client_email.map(str::to_owned))
                    .ok_or_else(|| {
                        AppError::invalid(format!(
                            "That {} account has no email address. Sign up with an email \
                             address instead.",
                            provider.display_name()
                        ))
                    })?;
                // Answers, permanently, what production tokens actually carry
                // — the question that made this outage take two rounds to fix.
                tracing::info!(
                    provider = provider.as_str(),
                    email_source,
                    "federated first arrival"
                );
                let display_name = email.split('@').next().unwrap_or("New user").to_owned();

                // Which side of the marketplace this account is on has to come
                // from the person, not from a token — a token cannot know, and
                // an account can never change sides afterwards. Defaulting to
                // homeowner would silently trap every contractor who signed up
                // with a provider button: wrong capabilities, and no route out
                // except abandoning the account.
                //
                // So the sign-up page sends the choice it already collected,
                // and arriving here without one is refused with something the
                // person can act on. The sign-in page sends nothing, which is
                // correct: someone signing in is expected to have an account
                // already, and if they do not, being sent to sign up and
                // choose is the right outcome rather than being assigned a
                // side at random.
                let account_type = intended_account_type.ok_or_else(|| {
                    AppError::invalid(format!(
                        "No account here yet uses that {} sign-in. Create an account first \
                         and choose whether you are a homeowner or a contractor — it cannot \
                         be changed later.",
                        provider.display_name()
                    ))
                })?;

                let user =
                    users::insert(&mut tx, new_id(), email.trim(), &display_name, account_type)
                        .await
                        .map_err(|error| match error {
                            // Method-agnostic on purpose: the colliding account may
                            // itself be federated (a Google account, hit by a Facebook
                            // sign-up sharing the address), and "sign in with your
                            // password" is wrong advice for an account that has none.
                            // Naming the link feature waits for the account page that
                            // reaches it — Phase D restores the fuller wording.
                            AppError::Conflict { .. } => AppError::conflict(
                                "An account already uses that email address. Sign in to \
                                 that account instead.",
                            ),
                            other => other,
                        })?;

                oauth::insert(
                    &mut tx,
                    new_id(),
                    user.id,
                    provider,
                    &identity.provider_subject,
                    Some(&identity.firebase_uid),
                    identity.email.as_deref(),
                    identity.email_verified,
                )
                .await?;

                (user, "auth.federated_registered")
            }
        };

        // The provider's verified-email claim is honoured only when it is a
        // claim about *this account's* address — a linked identity can carry a
        // different one, and verifying ours on the strength of theirs would
        // verify an address nobody proved.
        if identity.email_verified
            && identity
                .email
                .as_deref()
                .is_some_and(|claimed| claimed.trim().eq_ignore_ascii_case(user.email.trim()))
        {
            users::mark_email_verified(&mut tx, user.id).await?;
        }

        let session = self.issue_session(&mut tx, user.id, context, now).await?;
        audit::record(
            &mut tx,
            self.event(action, context)
                .actor(ActorKind::User, Some(user.id))
                .subject(user.id)
                .data(serde_json::json!({
                    "session_id": session.session_id,
                    "provider": provider.as_str(),
                })),
        )
        .await?;
        tx.commit().await.map_err(AppError::internal)?;

        Ok(LoginOutcome { user, session })
    }

    /// Attach a provider identity to the account that is already signed in.
    ///
    /// Linking is only reachable from an authenticated session, so control of
    /// both identities is proved before they are joined. This is the only way
    /// two identities ever end up on one account: nothing merges them on the
    /// strength of a shared email address.
    pub async fn link_provider(
        &self,
        pool: &PgPool,
        user_id: Uuid,
        provider: Provider,
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

        let identity = self.firebase()?.verify(id_token, provider).await?;

        let mut tx = pool.begin().await.map_err(AppError::internal)?;

        if oauth::exists_for_user(&mut tx, user_id, provider).await? {
            return Err(AppError::conflict(format!(
                "This account already has a {} identity linked.",
                provider.display_name()
            )));
        }
        if oauth::find_by_subject(&mut tx, provider, &identity.provider_subject)
            .await?
            .is_some()
        {
            return Err(AppError::conflict(format!(
                "That {} account is already linked to another account.",
                provider.display_name()
            )));
        }

        oauth::insert(
            &mut tx,
            new_id(),
            user_id,
            provider,
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
                .data(serde_json::json!({ "provider": provider.as_str() })),
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
