//! The operator role-grant CLI.
//!
//! There is deliberately no HTTP endpoint that can create an admin. The first
//! one has to come from somewhere, and shell access to the box is a stronger
//! prerequisite than any check an endpoint could make — so this surface is
//! tested by driving the real binary.

use sqlx::PgPool;
use std::process::{Command, Stdio};

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

struct Cli {
    url: String,
}

impl Cli {
    fn new(pool: &PgPool) -> Self {
        Self {
            url: child_database_url(pool),
        }
    }

    fn run(&self, args: &[&str]) -> (bool, String) {
        let output = Command::new(env!("CARGO_BIN_EXE_cm-server"))
            .args(args)
            .env("DATABASE_URL", &self.url)
            .env("CM_BIND_ADDR", "127.0.0.1:0")
            .env("CM_SITE_ORIGIN", "https://app.example.test")
            .env(
                "CM_HASH_PEPPER",
                "test-pepper-that-is-at-least-32-characters",
            )
            .env("CM_ARGON2_MAX_CONCURRENCY", "1")
            .env("CM_ENV", "production")
            .env("CM_LOG_FORMAT", "json")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run the CLI");

        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        (output.status.success(), combined)
    }
}

async fn an_account(pool: &PgPool, email: &str) -> uuid::Uuid {
    let mut conn = pool.acquire().await.expect("connection");
    cm_db::repo::users::insert(
        &mut conn,
        cm_core::new_id(),
        Some(email),
        "Operator Target",
        cm_db::repo::users::AccountType::Homeowner,
    )
    .await
    .expect("insert user")
    .id
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_role_can_be_granted_shown_and_revoked(pool: PgPool) {
    let email = "operator@example.test";
    let user_id = an_account(&pool, email).await;
    let cli = Cli::new(&pool);

    let (ok, output) = cli.run(&["admin", "show-roles", "--email", email]);
    assert!(ok, "{output}");
    assert!(output.contains("no roles"), "{output}");

    let (ok, output) = cli.run(&["admin", "grant-role", "--email", email, "--role", "admin"]);
    assert!(ok, "{output}");
    assert!(output.contains("granted admin"), "{output}");

    let mut conn = pool.acquire().await.expect("connection");
    assert_eq!(
        cm_db::repo::users::roles(&mut conn, user_id)
            .await
            .expect("roles"),
        vec![cm_db::repo::users::Role::Admin]
    );

    // The grant is auditable, and records that it came from the operator CLI
    // rather than from an account.
    let row: (String, Option<uuid::Uuid>, serde_json::Value) = sqlx::query_as(
        "SELECT actor_kind, actor_user_id, data FROM audit_log WHERE action = 'auth.role_granted'",
    )
    .fetch_one(&pool)
    .await
    .expect("an audit row");
    assert_eq!(row.0, "admin");
    assert_eq!(row.1, None, "there is no acting account at a shell");
    assert_eq!(row.2["role"], "admin");
    assert_eq!(row.2["via"], "cli");

    // Repeating a grant changes nothing and writes no second audit row.
    let (ok, output) = cli.run(&["admin", "grant-role", "--email", email, "--role", "admin"]);
    assert!(ok, "{output}");
    assert!(output.contains("no change"), "{output}");
    let grants: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'auth.role_granted'")
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(grants, 1);

    let (ok, output) = cli.run(&["admin", "revoke-role", "--email", email, "--role", "admin"]);
    assert!(ok, "{output}");
    assert!(output.contains("revoked admin"), "{output}");
    assert!(cm_db::repo::users::roles(&mut conn, user_id)
        .await
        .expect("roles")
        .is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_cli_refuses_an_unknown_role_or_account(pool: PgPool) {
    let email = "operator@example.test";
    an_account(&pool, email).await;
    let cli = Cli::new(&pool);

    let (ok, output) = cli.run(&["admin", "grant-role", "--email", email, "--role", "wizard"]);
    assert!(!ok, "an unknown role must fail: {output}");
    assert!(output.contains("unknown role"), "{output}");
    // The error names the valid options rather than leaving the operator to guess.
    assert!(output.contains("moderator"), "{output}");

    let (ok, output) = cli.run(&[
        "admin",
        "grant-role",
        "--email",
        "nobody@example.test",
        "--role",
        "admin",
    ]);
    assert!(!ok, "an unknown account must fail: {output}");
    assert!(output.contains("no account for"), "{output}");
}
