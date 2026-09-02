//! The mail worker and the outbox it drains.

use cm_core::{new_id, MailConfig, Secret};
use cm_db::repo::email_outbox::{self, Kind, NewEmail};
use cm_db::repo::users::{self, AccountType};
use cm_db::PgPool;
use cm_domain::mail_worker::{self, WorkerConfig};
use cm_domain::mailer::{Mailer, MemoryMailer, ResendMailer};
use uuid::Uuid;

async fn a_user(pool: &PgPool, email: &str) -> Uuid {
    let mut conn = pool.acquire().await.expect("connection");
    users::insert(
        &mut conn,
        new_id(),
        Some(email),
        "Test User",
        AccountType::Homeowner,
    )
    .await
    .expect("insert user")
    .id
}

async fn queue_one(pool: &PgPool, user_id: Uuid, subject: &str) -> Uuid {
    let mut conn = pool.acquire().await.expect("connection");
    email_outbox::enqueue(
        &mut conn,
        &NewEmail {
            user_id,
            recipient: "someone@example.test".to_owned(),
            kind: Kind::LoginCode,
            subject: subject.to_owned(),
            body_text: "Your code is 123456.".to_owned(),
            body_html: None,
            unsubscribe_url: None,
        },
    )
    .await
    .expect("enqueue")
}

fn worker() -> WorkerConfig {
    WorkerConfig {
        // No pacing sleep worth waiting through in a test.
        rate_per_second: 1_000.0,
        ..WorkerConfig::default()
    }
}

/// The happy path: a queued row is delivered and marked, and the message the
/// provider saw is the message that was enqueued.
#[sqlx::test(migrations = "../../migrations")]
async fn run_once_sends_queued_mail_and_marks_it_sent(pool: PgPool) {
    let user = a_user(&pool, "sender@example.test").await;
    let id = queue_one(&pool, user, "A subject").await;

    let memory = MemoryMailer::default();
    let mailer = Mailer::Memory(memory.clone());
    let stats = mail_worker::run_once(&pool, &mailer, &worker())
        .await
        .expect("pass");

    assert_eq!(stats.claimed, 1);
    assert_eq!(stats.sent, 1);
    assert_eq!(stats.failed, 0);

    let sent = memory.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "someone@example.test");
    assert_eq!(sent[0].subject, "A subject");
    assert_eq!(
        sent[0].id, id,
        "the outbox id rides along as the idempotency key"
    );

    let mut conn = pool.acquire().await.expect("connection");
    let (status, attempts, error) = email_outbox::status_of(&mut conn, id)
        .await
        .expect("status")
        .expect("row");
    assert_eq!(status, "sent");
    assert_eq!(attempts, 0);
    assert_eq!(error, None);
}

/// A provider outage delays mail; it never loses it. The row goes back to
/// queued with a future attempt, not to a terminal state.
#[sqlx::test(migrations = "../../migrations")]
async fn a_provider_error_leaves_the_row_queued_for_retry(pool: PgPool) {
    let user = a_user(&pool, "sender@example.test").await;
    let id = queue_one(&pool, user, "Undeliverable").await;

    // A Resend client pointed at a port nothing listens on: the real error
    // path, without the real provider.
    let config = MailConfig {
        resend_api_key: Secret::new("re_test_key".to_owned()),
        from: "Test <no-reply@example.test>".to_owned(),
    };
    let resend = ResendMailer::new(&config, Some("http://127.0.0.1:9".to_owned())).expect("client");
    let mailer = Mailer::Resend(resend);

    let stats = mail_worker::run_once(&pool, &mailer, &worker())
        .await
        .expect("pass");
    assert_eq!(stats.claimed, 1);
    assert_eq!(stats.failed, 1);

    let mut conn = pool.acquire().await.expect("connection");
    let (status, attempts, error) = email_outbox::status_of(&mut conn, id)
        .await
        .expect("status")
        .expect("row");
    assert_eq!(status, "queued", "one failure is a retry, not a verdict");
    assert_eq!(attempts, 1);
    assert!(error.is_some(), "the operator can read what went wrong");

    // Backed off: an immediate second pass must not pick it up again.
    let stats = mail_worker::run_once(&pool, &mailer, &worker())
        .await
        .expect("pass");
    assert_eq!(stats.claimed, 0, "the retry waits out its backoff");
}

/// Retries are capped. Without the cap, a permanently bad recipient would be
/// posted to the provider forever.
#[sqlx::test(migrations = "../../migrations")]
async fn a_failed_send_backs_off_and_gives_up_at_max_attempts(pool: PgPool) {
    let user = a_user(&pool, "sender@example.test").await;
    let id = queue_one(&pool, user, "Doomed").await;

    let mut conn = pool.acquire().await.expect("connection");
    let first = email_outbox::mark_failed(&mut conn, id, "boom", 0, 2)
        .await
        .expect("mark");
    assert_eq!(first.as_str(), "queued");

    let second = email_outbox::mark_failed(&mut conn, id, "boom again", 0, 2)
        .await
        .expect("mark");
    assert_eq!(
        second.as_str(),
        "failed",
        "the second of two attempts is the last"
    );
}

/// `SKIP LOCKED` and the `in_progress` mark together mean one message, one
/// worker: a second claimer sees nothing.
#[sqlx::test(migrations = "../../migrations")]
async fn a_claimed_row_is_invisible_to_a_second_claimer(pool: PgPool) {
    let user = a_user(&pool, "sender@example.test").await;
    queue_one(&pool, user, "Claim me once").await;

    let mut conn = pool.acquire().await.expect("connection");
    let first = email_outbox::claim(&mut conn, "worker-a", 10)
        .await
        .expect("claim");
    assert_eq!(first.len(), 1);

    let second = email_outbox::claim(&mut conn, "worker-b", 10)
        .await
        .expect("claim");
    assert!(
        second.is_empty(),
        "an in-progress row must not be re-claimed"
    );
}

/// A worker that dies mid-flight must not strand its claims forever.
#[sqlx::test(migrations = "../../migrations")]
async fn a_dead_workers_claims_are_requeued_after_the_stale_window(pool: PgPool) {
    let user = a_user(&pool, "sender@example.test").await;
    queue_one(&pool, user, "Abandoned").await;

    let mut conn = pool.acquire().await.expect("connection");
    let claimed = email_outbox::claim(&mut conn, "worker-that-dies", 10)
        .await
        .expect("claim");
    assert_eq!(claimed.len(), 1);

    // Backdate the claim past the stale window, standing in for a crash.
    sqlx::query("UPDATE email_outbox SET locked_at = locked_at - interval '1 hour'")
        .execute(&mut *conn)
        .await
        .expect("backdate");

    let requeued = email_outbox::requeue_stalled(&mut conn, 600)
        .await
        .expect("requeue");
    assert_eq!(requeued, 1);

    let reclaimed = email_outbox::claim(&mut conn, "worker-b", 10)
        .await
        .expect("claim");
    assert_eq!(reclaimed.len(), 1, "the recovered row is claimable again");
}
