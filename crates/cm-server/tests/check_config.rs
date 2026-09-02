//! The deploy preflight.
//!
//! `check-config` parses the environment without touching the database, so
//! these tests hand the child a well-formed URL nothing ever connects to.

use std::process::{Command, Stdio};

fn check_config(extra: &[(&str, &str)]) -> (Option<i32>, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cm-server"));
    command
        .arg("check-config")
        .env("DATABASE_URL", "postgres://cm@127.0.0.1:5432/cm_unused")
        .env("CM_SITE_ORIGIN", "https://app.example.test")
        .env(
            "CM_HASH_PEPPER",
            "test-pepper-that-is-at-least-32-characters",
        )
        .env("CM_ENV", "production")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra {
        command.env(key, value);
    }

    let output = command.output().expect("run check-config");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.code(), combined)
}

/// The preflight fails loudly on a production environment with gaps, and names
/// them — the same rule `serve` enforces, checked before anything restarts.
#[test]
fn check_config_fails_in_production_when_mail_is_unconfigured() {
    let (code, output) = check_config(&[("CM_JOB_PHOTO_BUCKET", "cm-photos")]);

    assert_eq!(code, Some(2), "a gap is a configuration failure:\n{output}");
    assert!(
        output.contains("CM_RESEND_API_KEY"),
        "the gap must name the fix:\n{output}"
    );
}

#[test]
fn check_config_passes_a_fully_configured_production_environment() {
    let (code, output) = check_config(&[
        ("CM_JOB_PHOTO_BUCKET", "cm-photos"),
        ("CM_RESEND_API_KEY", "re_test_key"),
        ("CM_MAIL_FROM", "Test <no-reply@example.test>"),
    ]);

    assert_eq!(code, Some(0), "{output}");
    assert!(
        output.contains("no-reply@example.test"),
        "the summary shows the From address:\n{output}"
    );
    assert!(
        !output.contains("re_test_key"),
        "the summary must never show the key:\n{output}"
    );
}
