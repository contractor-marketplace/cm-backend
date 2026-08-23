//! The schema gate on `serve`.
//!
//! These drive the real binary rather than a library function: the guarantee
//! being tested is "this process does not accept traffic against a schema it
//! was not written for", and only the process can demonstrate that.

use sqlx::PgPool;
use std::process::{Command, Stdio};
use std::time::Duration;

/// The URL of the throwaway database `#[sqlx::test]` created, for the child.
///
/// Built by swapping the database name into the suite's own `DATABASE_URL`
/// rather than reassembling one from the connect options, so whatever
/// credentials the environment uses are carried through unchanged.
fn child_database_url(pool: &PgPool) -> String {
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run these tests");
    let database = pool
        .connect_options()
        .get_database()
        .expect("the test pool always names a database")
        .to_owned();

    let (head, query) = match base.split_once('?') {
        Some((head, query)) => (head, Some(query)),
        None => (base.as_str(), None),
    };
    let (authority, _) = head
        .rsplit_once('/')
        .expect("a database URL names a database");

    match query {
        Some(query) => format!("{authority}/{database}?{query}"),
        None => format!("{authority}/{database}"),
    }
}

fn serve(url: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cm-server"));
    command
        .arg("serve")
        .env("DATABASE_URL", url)
        // Port 0 lets the OS pick, so concurrent tests cannot collide.
        .env("CM_BIND_ADDR", "127.0.0.1:0")
        // Every required variable, so a missing one cannot make the refusal
        // test pass for the wrong reason.
        .env("CM_SITE_ORIGIN", "https://app.example.test")
        .env(
            "CM_HASH_PEPPER",
            "test-pepper-that-is-at-least-32-characters",
        )
        .env("CM_ARGON2_MAX_CONCURRENCY", "1")
        .env("CM_ENV", "production")
        .env("CM_LOG_FORMAT", "json")
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

#[sqlx::test(migrations = false)]
async fn serve_refuses_to_start_when_the_schema_is_behind(pool: PgPool) {
    let url = child_database_url(&pool);

    let output = serve(&url).output().expect("run the server");

    // Exit 1 is a runtime failure; exit 2 is a configuration failure. Asserting
    // the specific code stops a future missing environment variable from making
    // this test pass without the schema gate ever running.
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected a runtime failure, got {:?}: {}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let logged = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        logged.contains("refusing to serve"),
        "the operator needs to be told why; output was:\n{logged}"
    );
    assert!(
        logged.contains("cm-server migrate"),
        "the message must name the fix; output was:\n{logged}"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn serve_starts_once_the_schema_matches(pool: PgPool) {
    let url = child_database_url(&pool);

    let mut child = serve(&url).spawn().expect("spawn the server");

    // Long enough for the schema check, which happens before the listener is
    // bound. A process still alive here got past the gate.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let exited = child.try_wait().expect("poll the child");
    assert!(
        exited.is_none(),
        "serve exited instead of serving: {exited:?}"
    );

    child.kill().expect("stop the server");
    child.wait().expect("reap the server");
}

/// A database ahead of the binary is the middle of a rolling deploy, not a
/// fault: migrations are additive, so the older binary keeps working.
#[sqlx::test(migrations = "../../migrations")]
async fn serve_starts_when_the_database_is_ahead_of_the_binary(pool: PgPool) {
    // Forge a ledger entry for a migration this binary does not carry.
    sqlx::query(
        "INSERT INTO _sqlx_migrations \
           (version, description, installed_on, success, checksum, execution_time) \
         VALUES (9999, 'from a newer binary', now(), true, '\\x00'::bytea, 0)",
    )
    .execute(&pool)
    .await
    .expect("insert a future migration");

    let url = child_database_url(&pool);
    let mut child = serve(&url).spawn().expect("spawn the server");

    tokio::time::sleep(Duration::from_secs(3)).await;

    let exited = child.try_wait().expect("poll the child");
    assert!(
        exited.is_none(),
        "a database ahead of the binary must still serve, but it exited: {exited:?}"
    );

    child.kill().expect("stop the server");
    child.wait().expect("reap the server");
}
